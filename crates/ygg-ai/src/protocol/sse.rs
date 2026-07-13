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

    /// Pushes a chunk of bytes into the decoder, returning any fully parsed events.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, DecodeError> {
        self.buf.extend_from_slice(bytes);

        let mut events = Vec::new();
        let mut scan_idx = 0;

        while let Some(lf_pos) = self.buf[scan_idx..].iter().position(|&b| b == b'\n') {
            let actual_lf_pos = scan_idx + lf_pos;
            let line_bytes = &self.buf[scan_idx..actual_lf_pos];
            scan_idx = actual_lf_pos + 1;

            self.current_event_bytes = self
                .current_event_bytes
                .checked_add(line_bytes.len() + 1)
                .ok_or(DecodeError::BodyTooLarge)?;
            if self.current_event_bytes > MAX_SSE_EVENT_BYTES {
                return Err(DecodeError::BodyTooLarge);
            }

            // Strip trailing \r if present
            let line_bytes = if !line_bytes.is_empty() && line_bytes[line_bytes.len() - 1] == b'\r'
            {
                &line_bytes[..line_bytes.len() - 1]
            } else {
                line_bytes
            };

            let line = std::str::from_utf8(line_bytes).map_err(|_| DecodeError::InvalidUtf8)?;

            if line.is_empty() {
                // Empty line denotes end of event
                if let Some(event) = self.dispatch_current() {
                    events.push(event);
                }
            } else if line.starts_with(':') {
                // Comment line, ignore
            } else {
                let (field, mut value) = match line.find(':') {
                    Some(idx) => {
                        let (f, v) = line.split_at(idx);
                        (f, &v[1..])
                    }
                    None => (line, ""),
                };

                // Strip leading space if present
                if value.starts_with(' ') {
                    value = &value[1..];
                }

                match field {
                    "event" => {
                        self.current_event = Some(value.to_string());
                    }
                    "data" => {
                        self.current_data.push(value.to_string());
                    }
                    _ => {
                        // Ignore other fields (id, retry, etc.)
                    }
                }
            }
        }

        // Drain processed bytes
        if scan_idx > 0 {
            self.buf.drain(..scan_idx);
        }
        if self
            .buf
            .len()
            .checked_add(self.current_event_bytes)
            .map_or(true, |size| size > MAX_SSE_EVENT_BYTES)
        {
            return Err(DecodeError::BodyTooLarge);
        }

        Ok(events)
    }

    /// Flushes any remaining data at the end of the stream as a final event.
    pub(crate) fn finish(mut self) -> Result<Option<SseEvent>, DecodeError> {
        if !self.buf.is_empty() {
            self.current_event_bytes = self
                .current_event_bytes
                .checked_add(self.buf.len())
                .ok_or(DecodeError::BodyTooLarge)?;
            if self.current_event_bytes > MAX_SSE_EVENT_BYTES {
                return Err(DecodeError::BodyTooLarge);
            }
            // Process the trailing bytes as a line
            let mut line_bytes = &self.buf[..];
            if !line_bytes.is_empty() && line_bytes[line_bytes.len() - 1] == b'\r' {
                line_bytes = &line_bytes[..line_bytes.len() - 1];
            }
            let line = std::str::from_utf8(line_bytes).map_err(|_| DecodeError::InvalidUtf8)?;

            if !line.is_empty() && !line.starts_with(':') {
                let (field, mut value) = match line.find(':') {
                    Some(idx) => {
                        let (f, v) = line.split_at(idx);
                        (f, &v[1..])
                    }
                    None => (line, ""),
                };

                if value.starts_with(' ') {
                    value = &value[1..];
                }

                match field {
                    "event" => {
                        self.current_event = Some(value.to_string());
                    }
                    "data" => {
                        self.current_data.push(value.to_string());
                    }
                    _ => {}
                }
            }
        }

        Ok(self.dispatch_current())
    }

    fn dispatch_current(&mut self) -> Option<SseEvent> {
        if self.current_event.is_none() && self.current_data.is_empty() {
            return None;
        }

        let event = SseEvent {
            event: self.current_event.take(),
            data: self.current_data.join("\n"),
        };
        self.current_data.clear();
        self.current_event_bytes = 0;
        Some(event)
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
