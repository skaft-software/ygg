//! Shared Server-Sent Events (SSE) decoder.

use crate::error::DecodeError;

const MAX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// Represents a parsed Server-Sent Event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// Optional event type name (e.g. from `event:` field).
    pub event: Option<String>,
    /// Accumulated event payload (from `data:` fields).
    pub data: String,
}

/// A parsed SSE frame.
///
/// Comments normally carry no application semantics and callers using
/// [`SseDecoder::push`] continue to receive only data events. HTTP transports
/// that explicitly negotiate a namespaced extension may use
/// [`SseDecoder::push_frames`] to inspect comments without exposing them to a
/// provider codec as `data:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseFrame {
    /// A complete data event.
    Event(SseEvent),
    /// A comment line without its leading colon.
    Comment(String),
}

/// A stateful push-based decoder for SSE streams.
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
    current_event: Option<String>,
    current_data: Vec<String>,
    current_event_bytes: usize,
}

impl SseDecoder {
    /// Creates a new decoder.
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            current_event: None,
            current_data: Vec::new(),
            current_event_bytes: 0,
        }
    }

    /// Pushes a chunk of bytes into the decoder, returning any fully parsed data events.
    ///
    /// SSE comments remain intentionally invisible to ordinary callers. Use
    /// [`Self::push_frames`] only when an explicitly negotiated extension
    /// needs to inspect them.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, DecodeError> {
        Ok(self
            .push_frames(bytes)?
            .into_iter()
            .filter_map(|frame| match frame {
                SseFrame::Event(event) => Some(event),
                SseFrame::Comment(_) => None,
            })
            .collect())
    }

    /// Pushes a chunk of bytes, retaining data events and comment frames.
    pub(crate) fn push_frames(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, DecodeError> {
        let mut frames = Vec::new();
        let mut start = 0;

        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let incoming = &bytes[start..index];
            self.ensure_pending_size(incoming.len().saturating_add(1))?;

            if self.buf.is_empty() {
                self.process_line(incoming, 1, &mut frames)?;
            } else {
                self.buf.extend_from_slice(incoming);
                let line = std::mem::take(&mut self.buf);
                let result = self.process_line(&line, 1, &mut frames);
                self.buf = line;
                self.buf.clear();
                result?;
            }
            start = index + 1;
        }

        let trailing = &bytes[start..];
        self.ensure_pending_size(trailing.len())?;
        self.buf.extend_from_slice(trailing);
        Ok(frames)
    }

    /// Flushes any remaining data at the end of the stream as a final event.
    pub(crate) fn finish(self) -> Result<Option<SseEvent>, DecodeError> {
        Ok(self
            .finish_frames()?
            .into_iter()
            .rev()
            .find_map(|frame| match frame {
                SseFrame::Event(event) => Some(event),
                SseFrame::Comment(_) => None,
            }))
    }

    /// Flushes all remaining data and comment frames at the end of the stream.
    pub(crate) fn finish_frames(mut self) -> Result<Vec<SseFrame>, DecodeError> {
        let mut frames = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.process_line(&line, 0, &mut frames)?;
        }
        if let Some(event) = self.dispatch_current() {
            frames.push(SseFrame::Event(event));
        }
        Ok(frames)
    }

    fn ensure_pending_size(&self, additional: usize) -> Result<(), DecodeError> {
        if self
            .current_event_bytes
            .checked_add(self.buf.len())
            .and_then(|size| size.checked_add(additional))
            .is_none_or(|size| size > MAX_SSE_EVENT_BYTES)
        {
            return Err(DecodeError::BodyTooLarge);
        }
        Ok(())
    }

    fn process_line(
        &mut self,
        line_bytes: &[u8],
        line_ending_bytes: usize,
        frames: &mut Vec<SseFrame>,
    ) -> Result<(), DecodeError> {
        self.current_event_bytes = self
            .current_event_bytes
            .checked_add(line_bytes.len())
            .and_then(|size| size.checked_add(line_ending_bytes))
            .ok_or(DecodeError::BodyTooLarge)?;
        if self.current_event_bytes > MAX_SSE_EVENT_BYTES {
            return Err(DecodeError::BodyTooLarge);
        }

        let line_bytes = line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes);
        let line = std::str::from_utf8(line_bytes).map_err(|_| DecodeError::InvalidUtf8)?;
        if line.is_empty() {
            if let Some(event) = self.dispatch_current() {
                frames.push(SseFrame::Event(event));
            }
            return Ok(());
        }
        if let Some(comment) = line.strip_prefix(':') {
            frames.push(SseFrame::Comment(comment.trim_start().to_owned()));
            return Ok(());
        }

        let (field, mut value) = match line.find(':') {
            Some(index) => {
                let (field, value) = line.split_at(index);
                (field, &value[1..])
            }
            None => (line, ""),
        };
        if value.starts_with(' ') {
            value = &value[1..];
        }
        match field {
            "event" => self.current_event = Some(value.to_string()),
            "data" => self.current_data.push(value.to_string()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch_current(&mut self) -> Option<SseEvent> {
        let event = if self.current_event.is_none() && self.current_data.is_empty() {
            None
        } else {
            Some(SseEvent {
                event: self.current_event.take(),
                data: self.current_data.join("\n"),
            })
        };
        self.current_event = None;
        self.current_data.clear();
        self.current_event_bytes = 0;
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_decoder_basic() {
        let mut decoder = SseDecoder::new();
        let payload = b"event: message\ndata: hello world\n\n";
        let events = decoder.push(payload).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, Some("message".to_string()));
        assert_eq!(events[0].data, "hello world");
    }

    #[test]
    fn test_sse_decoder_crlf_and_comments() {
        let mut decoder = SseDecoder::new();
        let payload =
            b":keep-alive\r\nevent: message\r\ndata: first line\r\ndata: second line\r\n\r\n";
        let events = decoder.push(payload).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, Some("message".to_string()));
        assert_eq!(events[0].data, "first line\nsecond line");
    }

    #[test]
    fn push_frames_preserves_comments_without_changing_ordinary_callers() {
        let payload = b": ygg-lifecycle: loading; warming\ndata: hello\n\n";

        let mut framed = SseDecoder::new();
        assert_eq!(
            framed.push_frames(payload).unwrap(),
            vec![
                SseFrame::Comment("ygg-lifecycle: loading; warming".into()),
                SseFrame::Event(SseEvent {
                    event: None,
                    data: "hello".into(),
                }),
            ]
        );

        let mut ordinary = SseDecoder::new();
        assert_eq!(
            ordinary.push(payload).unwrap(),
            vec![SseEvent {
                event: None,
                data: "hello".into(),
            }]
        );
    }

    #[test]
    fn test_sse_decoder_utf8_split_chunking() {
        // UTF-8 character '🎉' is [0xF0, 0x9F, 0x8E, 0x89]
        let mut decoder = SseDecoder::new();
        let chunk1 = b"data: \xf0\x9f";
        let chunk2 = b"\x8e\x89\n\n";

        let events1 = decoder.push(chunk1).unwrap();
        assert!(events1.is_empty());

        let events2 = decoder.push(chunk2).unwrap();
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].data, "🎉");
    }

    #[test]
    fn test_sse_decoder_boundary_chunking_property() {
        let payload = b":comment\nevent: ping\ndata: {\"msg\": \"ok\"}\n\ndata: [DONE]\n\n";

        // Feed the payload byte-by-byte to prove it produces identical events
        let mut decoder = SseDecoder::new();
        let mut all_events = Vec::new();
        for &byte in payload.iter() {
            let evs = decoder.push(&[byte]).unwrap();
            all_events.extend(evs);
        }
        let final_ev = decoder.finish().unwrap();
        if let Some(ev) = final_ev {
            all_events.push(ev);
        }

        assert_eq!(all_events.len(), 2);
        assert_eq!(all_events[0].event, Some("ping".to_string()));
        assert_eq!(all_events[0].data, "{\"msg\": \"ok\"}");
        assert_eq!(all_events[1].event, None);
        assert_eq!(all_events[1].data, "[DONE]");
    }

    #[test]
    fn oversized_unterminated_event_is_rejected_before_buffering_it() {
        let mut decoder = SseDecoder::new();
        let payload = vec![b'x'; MAX_SSE_EVENT_BYTES + 1];
        assert!(matches!(
            decoder.push(&payload),
            Err(DecodeError::BodyTooLarge)
        ));
        assert!(decoder.buf.is_empty());
    }

    #[test]
    fn a_large_chunk_of_separate_events_is_not_treated_as_one_event() {
        let data = "x".repeat(MAX_SSE_EVENT_BYTES / 2);
        let event = format!("data: {data}\n\n");
        let payload = event.repeat(3);
        let mut decoder = SseDecoder::new();
        let events = decoder.push(payload.as_bytes()).unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| event.data == data));
    }

    #[test]
    fn test_sse_decoder_finish_trailing_no_newline() {
        let decoder = SseDecoder {
            buf: b"data: unfinished".to_vec(),
            current_event: None,
            current_data: Vec::new(),
            current_event_bytes: 0,
        };
        let ev = decoder.finish().unwrap().unwrap();
        assert_eq!(ev.data, "unfinished");
    }
}
