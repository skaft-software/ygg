/// Ring buffer for Emacs-style kill/yank operations.
///
/// Tracks killed (deleted) text entries. Consecutive kills can accumulate
/// into a single entry. Supports yank (paste most recent) and yank-pop
/// (cycle through older entries).
pub struct KillRing {
    ring: Vec<String>,
}

/// Options for pushing to the kill ring.
pub struct PushOptions {
    /// If accumulating, prepend (backward deletion) or append (forward deletion).
    pub prepend: bool,
    /// Merge with the most recent entry instead of creating a new one.
    pub accumulate: bool,
}

impl KillRing {
    pub fn new() -> Self {
        KillRing { ring: Vec::new() }
    }

    /// Add text to the kill ring.
    pub fn push(&mut self, text: &str, opts: &PushOptions) {
        if text.is_empty() {
            return;
        }

        if opts.accumulate && !self.ring.is_empty() {
            let last = self.ring.pop().unwrap();
            if opts.prepend {
                self.ring.push(format!("{}{}", text, last));
            } else {
                self.ring.push(format!("{}{}", last, text));
            }
        } else {
            self.ring.push(text.to_string());
        }
    }

    /// Get most recent entry without modifying the ring.
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(|s| s.as_str())
    }

    /// Move last entry to front (for yank-pop cycling).
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            if let Some(last) = self.ring.pop() {
                self.ring.insert(0, last);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

impl Default for KillRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_peek() {
        let mut kr = KillRing::new();
        kr.push(
            "hello",
            &PushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        assert_eq!(kr.peek(), Some("hello"));
    }

    #[test]
    fn test_accumulate_append() {
        let mut kr = KillRing::new();
        kr.push(
            "hello",
            &PushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        kr.push(
            " world",
            &PushOptions {
                prepend: false,
                accumulate: true,
            },
        );
        assert_eq!(kr.peek(), Some("hello world"));
    }

    #[test]
    fn test_accumulate_prepend() {
        let mut kr = KillRing::new();
        kr.push(
            "world",
            &PushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        kr.push(
            "hello ",
            &PushOptions {
                prepend: true,
                accumulate: true,
            },
        );
        assert_eq!(kr.peek(), Some("hello world"));
    }

    #[test]
    fn test_rotate() {
        let mut kr = KillRing::new();
        kr.push(
            "a",
            &PushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        kr.push(
            "b",
            &PushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        kr.push(
            "c",
            &PushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        assert_eq!(kr.peek(), Some("c"));
        kr.rotate();
        assert_eq!(kr.peek(), Some("b"));
        kr.rotate();
        assert_eq!(kr.peek(), Some("a"));
    }
}
