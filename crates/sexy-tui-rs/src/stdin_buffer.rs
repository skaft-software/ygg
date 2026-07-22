//! Stdin buffer for handling bracketed paste and multi-byte sequences.
//! Port of src/stdin-buffer.ts (434 lines).

const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// Options for StdinBuffer.
pub struct StdinBufferOptions {
    /// Maximum time to wait for more data before flushing (ms).
    pub flush_timeout_ms: u64,
    /// Maximum buffer size before forcing a flush.
    pub max_buffer_size: usize,
}

impl Default for StdinBufferOptions {
    fn default() -> Self {
        StdinBufferOptions {
            flush_timeout_ms: 10,
            max_buffer_size: 65536,
        }
    }
}

/// Buffered stdin reader that handles bracketed paste and escape sequences.
pub struct StdinBuffer {
    buffer: String,
    in_paste: bool,
    paste_line_count: usize,
    paste_counter: usize,
    max_buffer_size: usize,
}

impl StdinBuffer {
    pub fn new(options: StdinBufferOptions) -> Self {
        StdinBuffer {
            buffer: String::new(),
            in_paste: false,
            paste_line_count: 0,
            paste_counter: 0,
            max_buffer_size: options.max_buffer_size,
        }
    }

    /// Feed raw input data into the buffer. Returns completed chunks.
    pub fn feed(&mut self, data: &str) -> Vec<String> {
        let mut results: Vec<String> = Vec::new();

        for ch in data.chars() {
            self.buffer.push(ch);

            // Flush if buffer exceeds max size (prevents unbounded growth)
            if self.buffer.len() >= self.max_buffer_size {
                results.push(std::mem::take(&mut self.buffer));
            }

            // Check for bracketed paste start
            if self.buffer.ends_with(BRACKETED_PASTE_START) {
                self.in_paste = true;
                self.paste_line_count = 0;
                self.paste_counter += 1;
                // Remove the marker from buffer
                let len = self.buffer.len();
                self.buffer.truncate(len - BRACKETED_PASTE_START.len());

                // Flush any pending non-paste content
                if !self.buffer.is_empty() {
                    results.push(std::mem::take(&mut self.buffer));
                }
                continue;
            }

            // Check for bracketed paste end
            if self.buffer.ends_with(BRACKETED_PASTE_END) {
                self.in_paste = false;
                let len = self.buffer.len();
                self.buffer.truncate(len - BRACKETED_PASTE_END.len());

                // If paste was large, wrap it in a marker
                let paste_content = std::mem::take(&mut self.buffer);
                if self.paste_line_count > 10 {
                    results.push(format!(
                        "[paste #{} +{} lines]",
                        self.paste_counter, self.paste_line_count
                    ));
                } else {
                    results.push(paste_content);
                }
                continue;
            }

            // Count lines during paste
            if self.in_paste && ch == '\n' {
                self.paste_line_count += 1;
            }

            // Flush on newline when not in paste
            if !self.in_paste && ch == '\n' {
                results.push(std::mem::take(&mut self.buffer));
            }
        }

        results
    }

    /// Flush any remaining buffered data.
    pub fn flush(&mut self) -> Vec<String> {
        let mut results = Vec::new();
        if !self.buffer.is_empty() {
            results.push(std::mem::take(&mut self.buffer));
        }
        results
    }
}
