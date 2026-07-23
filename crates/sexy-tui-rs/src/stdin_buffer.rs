//! Pi-compatible stdin sequence and bracketed-paste buffering.
//!
//! Input chunks are arbitrary transport fragments.  This parser emits one
//! complete terminal sequence at a time and keeps paste payloads semantic.

use std::time::{Duration, Instant};

const ESC: char = '\x1b';
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

#[derive(Clone, Debug)]
pub struct StdinBufferOptions {
    /// Pi's incomplete-sequence timeout (10ms by default).
    pub flush_timeout_ms: u64,
    /// Compatibility safety limit. `usize::MAX` preserves Pi behavior.
    pub max_buffer_size: usize,
}

impl Default for StdinBufferOptions {
    fn default() -> Self {
        Self {
            flush_timeout_ms: 10,
            max_buffer_size: usize::MAX,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StdinEvent {
    Data(String),
    Paste(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    Complete,
    Incomplete,
    NotEscape,
}

fn complete_sequence(data: &str) -> Completion {
    if !data.starts_with(ESC) {
        return Completion::NotEscape;
    }
    if data.len() == 1 {
        return Completion::Incomplete;
    }
    let after = &data[1..];
    if after.starts_with('[') {
        if after.starts_with("[M") {
            return if data.len() >= 6 {
                Completion::Complete
            } else {
                Completion::Incomplete
            };
        }
        return if complete_csi(data) {
            Completion::Complete
        } else {
            Completion::Incomplete
        };
    }
    if after.starts_with(']') {
        return if complete_string_sequence(data, true) {
            Completion::Complete
        } else {
            Completion::Incomplete
        };
    }
    if after.starts_with('P') || after.starts_with('_') {
        return if complete_string_sequence(data, false) {
            Completion::Complete
        } else {
            Completion::Incomplete
        };
    }
    if after.starts_with('O') {
        return if after.chars().count() >= 2 {
            Completion::Complete
        } else {
            Completion::Incomplete
        };
    }
    Completion::Complete
}

fn complete_string_sequence(data: &str, allow_bell: bool) -> bool {
    data.ends_with("\x1b\\") || (allow_bell && data.ends_with('\x07'))
}

fn complete_csi(data: &str) -> bool {
    let Some(payload) = data.strip_prefix("\x1b[") else {
        return true;
    };
    let Some(last) = payload.as_bytes().last().copied() else {
        return false;
    };
    if !(0x40..=0x7e).contains(&last) {
        return false;
    }
    if !payload.starts_with('<') {
        return true;
    }
    if !matches!(last, b'M' | b'm') {
        return false;
    }
    let parameters = &payload[1..payload.len() - 1];
    let mut parts = parameters.split(';');
    let valid = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    });
    valid && parts.next().is_none()
}

fn kitty_printable_codepoint(sequence: &str) -> Option<u32> {
    let body = sequence.strip_prefix("\x1b[")?.strip_suffix('u')?;
    // Only unmodified CSI-u: codepoint[:shifted][:base]u.
    if body.contains(';') {
        return None;
    }
    let codepoint = body.split(':').next()?.parse::<u32>().ok()?;
    (codepoint >= 32).then_some(codepoint)
}

fn extract_complete_sequences(mut buffer: &str) -> (Vec<String>, String) {
    let mut sequences = Vec::new();
    while !buffer.is_empty() {
        if buffer.starts_with(ESC) {
            let boundaries = buffer
                .char_indices()
                .map(|(offset, _)| offset)
                .skip(1)
                .chain(std::iter::once(buffer.len()));
            let mut consumed = None;
            for end in boundaries {
                let candidate = &buffer[..end];
                match complete_sequence(candidate) {
                    Completion::Complete => {
                        if candidate == "\x1b\x1b"
                            && matches!(
                                buffer.as_bytes().get(end),
                                Some(b'[' | b']' | b'O' | b'P' | b'_')
                            )
                        {
                            sequences.push("\x1b".into());
                            consumed = Some(1);
                            break;
                        }
                        sequences.push(candidate.into());
                        consumed = Some(end);
                        break;
                    }
                    Completion::Incomplete => {}
                    Completion::NotEscape => {
                        sequences.push(candidate.into());
                        consumed = Some(end);
                        break;
                    }
                }
            }
            let Some(end) = consumed else {
                return (sequences, buffer.into());
            };
            buffer = &buffer[end..];
        } else {
            let character = buffer.chars().next().expect("non-empty input");
            sequences.push(character.to_string());
            buffer = &buffer[character.len_utf8()..];
        }
    }
    (sequences, String::new())
}

pub struct StdinBuffer {
    buffer: String,
    timeout_ms: u64,
    timeout_deadline: Option<Instant>,
    paste_mode: bool,
    paste_buffer: String,
    pending_kitty_printable_codepoint: Option<u32>,
    max_buffer_size: usize,
}

impl Default for StdinBuffer {
    fn default() -> Self {
        Self::new(StdinBufferOptions::default())
    }
}

impl StdinBuffer {
    pub fn new(options: StdinBufferOptions) -> Self {
        Self {
            buffer: String::new(),
            timeout_ms: options.flush_timeout_ms,
            timeout_deadline: None,
            paste_mode: false,
            paste_buffer: String::new(),
            pending_kitty_printable_codepoint: None,
            max_buffer_size: options.max_buffer_size,
        }
    }

    /// Process a raw transport chunk. Pi treats a lone high byte as a legacy
    /// Meta-key encoding (`ESC` plus byte minus 128).
    pub fn process_bytes(&mut self, data: &[u8]) -> Vec<StdinEvent> {
        if let [byte] = data {
            if *byte > 127 {
                let value = [0x1b, *byte - 128];
                return self.process(&String::from_utf8_lossy(&value));
            }
        }
        self.process(&String::from_utf8_lossy(data))
    }

    /// Process one UTF-8 transport chunk and return complete semantic events.
    pub fn process(&mut self, data: &str) -> Vec<StdinEvent> {
        self.timeout_deadline = None;
        let mut events = Vec::new();
        if data.is_empty() && self.buffer.is_empty() {
            self.emit_data(String::new(), &mut events);
            return events;
        }
        self.buffer.push_str(data);

        if self.paste_mode {
            self.paste_buffer.push_str(&self.buffer);
            self.buffer.clear();
            self.finish_paste(&mut events);
            return events;
        }

        if let Some(start) = self.buffer.find(BRACKETED_PASTE_START) {
            if start > 0 {
                let before = self.buffer[..start].to_owned();
                let (sequences, _) = extract_complete_sequences(&before);
                for sequence in sequences {
                    self.emit_data(sequence, &mut events);
                }
            }
            self.pending_kitty_printable_codepoint = None;
            let content = self.buffer[start + BRACKETED_PASTE_START.len()..].to_owned();
            self.buffer.clear();
            self.paste_mode = true;
            self.paste_buffer = content;
            self.finish_paste(&mut events);
            return events;
        }

        let (sequences, remainder) = extract_complete_sequences(&self.buffer);
        self.buffer = remainder;
        for sequence in sequences {
            self.emit_data(sequence, &mut events);
        }

        if self.buffer.len() >= self.max_buffer_size {
            if let Some(sequence) = self.flush_one() {
                self.emit_data(sequence, &mut events);
            }
        } else if !self.buffer.is_empty() {
            self.timeout_deadline = Some(Instant::now() + Duration::from_millis(self.timeout_ms));
        }
        events
    }

    fn finish_paste(&mut self, events: &mut Vec<StdinEvent>) {
        let Some(end) = self.paste_buffer.find(BRACKETED_PASTE_END) else {
            return;
        };
        let pasted = self.paste_buffer[..end].to_owned();
        let remaining = self.paste_buffer[end + BRACKETED_PASTE_END.len()..].to_owned();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
        events.push(StdinEvent::Paste(pasted));
        if !remaining.is_empty() {
            events.extend(self.process(&remaining));
        }
    }

    fn emit_data(&mut self, sequence: String, events: &mut Vec<StdinEvent>) {
        let raw = {
            let mut chars = sequence.chars();
            let first = chars.next();
            (chars.next().is_none())
                .then_some(first)
                .flatten()
                .map(|value| value as u32)
        };
        if raw.is_some() && raw == self.pending_kitty_printable_codepoint {
            self.pending_kitty_printable_codepoint = None;
            return;
        }
        self.pending_kitty_printable_codepoint = kitty_printable_codepoint(&sequence);
        events.push(StdinEvent::Data(sequence));
    }

    fn flush_one(&mut self) -> Option<String> {
        self.timeout_deadline = None;
        if self.buffer.is_empty() {
            return None;
        }
        self.pending_kitty_printable_codepoint = None;
        Some(std::mem::take(&mut self.buffer))
    }

    /// Poll the Pi timeout. Call this from the terminal event-loop tick.
    pub fn poll_timeout(&mut self) -> Vec<StdinEvent> {
        if self
            .timeout_deadline
            .is_none_or(|deadline| Instant::now() < deadline)
        {
            return Vec::new();
        }
        self.flush_one()
            .map(|value| vec![StdinEvent::Data(value)])
            .unwrap_or_default()
    }

    pub fn flush(&mut self) -> Vec<String> {
        self.flush_one().into_iter().collect()
    }

    pub fn clear(&mut self) {
        self.timeout_deadline = None;
        self.buffer.clear();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
    }

    pub fn get_buffer(&self) -> &str {
        &self.buffer
    }
    pub fn destroy(&mut self) {
        self.clear();
    }

    /// Compatibility adapter for the old Rust API. New callers should use
    /// [`Self::process`] so paste cannot be confused with key data.
    pub fn feed(&mut self, data: &str) -> Vec<String> {
        self.process(data)
            .into_iter()
            .map(|event| match event {
                StdinEvent::Data(value) | StdinEvent::Paste(value) => value,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(events: Vec<StdinEvent>) -> Vec<String> {
        events
            .into_iter()
            .filter_map(|event| match event {
                StdinEvent::Data(value) => Some(value),
                StdinEvent::Paste(_) => None,
            })
            .collect()
    }

    #[test]
    fn pi_splits_regular_and_complete_sequences() {
        let mut buffer = StdinBuffer::default();
        assert_eq!(data(buffer.process_bytes(&[0xe1])), vec!["\x1ba"]);
        assert_eq!(
            data(buffer.process("abc\x1b[A\x1b[97;1:3u")),
            vec!["a", "b", "c", "\x1b[A", "\x1b[97;1:3u"]
        );
        assert_eq!(buffer.get_buffer(), "");
    }

    #[test]
    fn pi_accumulates_partial_mouse_and_old_mouse_sequences() {
        let mut buffer = StdinBuffer::default();
        assert!(buffer.process("\x1b[<35").is_empty());
        assert_eq!(data(buffer.process(";20;5m")), vec!["\x1b[<35;20;5m"]);
        assert!(buffer.process("\x1b[M a").is_empty());
        assert_eq!(data(buffer.process("b")), vec!["\x1b[M ab"]);
    }

    #[test]
    fn pi_keeps_paste_semantic_and_processes_trailing_input() {
        let mut buffer = StdinBuffer::default();
        assert_eq!(
            buffer.process("a\x1b[200~one\ntwo\x1b[201~b"),
            vec![
                StdinEvent::Data("a".into()),
                StdinEvent::Paste("one\ntwo".into()),
                StdinEvent::Data("b".into())
            ]
        );
    }

    #[test]
    fn pi_handles_wezterm_escape_and_duplicate_kitty_text() {
        let mut buffer = StdinBuffer::default();
        assert_eq!(
            data(buffer.process("\x1b\x1b[27;129:3u")),
            vec!["\x1b", "\x1b[27;129:3u"]
        );
        assert_eq!(data(buffer.process("\x1b[224uà")), vec!["\x1b[224u"]);
    }

    #[test]
    fn explicit_and_timed_flush_match_pi() {
        let mut buffer = StdinBuffer::new(StdinBufferOptions {
            flush_timeout_ms: 0,
            ..Default::default()
        });
        assert!(buffer.process("\x1b[<35").is_empty());
        assert_eq!(
            buffer.poll_timeout(),
            vec![StdinEvent::Data("\x1b[<35".into())]
        );
        assert!(buffer.flush().is_empty());
    }
}
