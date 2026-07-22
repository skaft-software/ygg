/// Word boundary navigation for text editors.
///
/// Pure functions — do not mutate state.
use unicode_segmentation::UnicodeSegmentation;

pub type WordSegmenter = dyn Fn(&str) -> Vec<String>;
pub type AtomicSegmentPredicate = dyn Fn(&str) -> bool;

/// Options for word navigation functions.
pub struct WordNavigationOptions<'a> {
    /// Custom segmenter returning word segments for the given text.
    pub segment: Option<&'a WordSegmenter>,
    /// Predicate identifying atomic segments (e.g. paste markers).
    pub is_atomic_segment: Option<&'a AtomicSegmentPredicate>,
}

/// Characters considered punctuation for word boundary detection.
pub const PUNCTUATION_CHARS: &str = "(){}[]<>.,;:'\"!?+\\-=*/|&%^$#@~`";

fn is_whitespace_char(s: &str) -> bool {
    s.chars().all(|c| c.is_whitespace())
}

fn is_punctuation_char(c: char) -> bool {
    PUNCTUATION_CHARS.contains(c)
}

fn default_segment(text: &str) -> Vec<String> {
    text.split_word_bounds().map(|s| s.to_string()).collect()
}

/// Find cursor position after moving one word backward from `cursor` in `text`.
pub fn find_word_backward(
    text: &str,
    cursor: usize,
    options: Option<&WordNavigationOptions>,
) -> usize {
    if cursor == 0 {
        return 0;
    }

    let text_before = &text[..cursor];
    let segments = match options.and_then(|o| o.segment) {
        Some(seg_fn) => seg_fn(text_before),
        None => default_segment(text_before),
    };
    let is_atomic = options.and_then(|o| o.is_atomic_segment);

    let mut idx = segments.len();
    let mut new_cursor = cursor;

    // Skip trailing whitespace
    while idx > 0 {
        let seg = &segments[idx - 1];
        let is_atomic_seg = is_atomic.is_some_and(|f| f(seg));
        if !is_atomic_seg && is_whitespace_char(seg) {
            new_cursor -= seg.len();
            idx -= 1;
        } else {
            break;
        }
    }

    if idx == 0 {
        return new_cursor;
    }

    let last = &segments[idx - 1];

    if is_atomic.is_some_and(|f| f(last)) {
        // Skip one atomic segment
        new_cursor -= last.len();
    } else if is_word_like(last) {
        // Skip inside one word-like segment, preserving ASCII punctuation boundaries
        let punct_indices: Vec<usize> = last
            .char_indices()
            .filter(|(_, c)| is_punctuation_char(*c))
            .map(|(i, _)| i)
            .collect();

        if punct_indices.is_empty() {
            new_cursor -= last.len();
        } else {
            let last_match = punct_indices[punct_indices.len() - 1];
            let ch = last.chars().nth(last_match).unwrap();
            new_cursor -= last.len() - (last_match + ch.len_utf8());
        }
    } else {
        // Skip non-word non-whitespace run (punctuation)
        while idx > 0 {
            let seg = &segments[idx - 1];
            let is_atomic_seg = is_atomic.is_some_and(|f| f(seg));
            if !is_atomic_seg && !is_word_like(seg) && !is_whitespace_char(seg) {
                new_cursor -= seg.len();
                idx -= 1;
            } else {
                break;
            }
        }
    }

    new_cursor
}

/// Find cursor position after moving one word forward from `cursor` in `text`.
pub fn find_word_forward(
    text: &str,
    cursor: usize,
    options: Option<&WordNavigationOptions>,
) -> usize {
    if cursor >= text.len() {
        return text.len();
    }

    let text_after = &text[cursor..];
    let segments = match options.and_then(|o| o.segment) {
        Some(seg_fn) => seg_fn(text_after),
        None => default_segment(text_after),
    };
    let is_atomic = options.and_then(|o| o.is_atomic_segment);

    let mut seg_idx = 0;
    let mut new_cursor = cursor;

    // Skip leading whitespace
    while seg_idx < segments.len() {
        let seg = &segments[seg_idx];
        let is_atomic_seg = is_atomic.is_some_and(|f| f(seg));
        if !is_atomic_seg && is_whitespace_char(seg) {
            new_cursor += seg.len();
            seg_idx += 1;
        } else {
            break;
        }
    }

    if seg_idx >= segments.len() {
        return new_cursor;
    }

    let current = &segments[seg_idx];

    if is_atomic.is_some_and(|f| f(current)) {
        // Skip one atomic segment
        new_cursor += current.len();
    } else if is_word_like(current) {
        // Skip inside one word-like segment
        let first_punct = current.find(is_punctuation_char);
        new_cursor += first_punct.unwrap_or(current.len());
    } else {
        // Skip non-word non-whitespace run
        while seg_idx < segments.len() {
            let seg = &segments[seg_idx];
            let is_atomic_seg = is_atomic.is_some_and(|f| f(seg));
            if !is_atomic_seg && !is_word_like(seg) && !is_whitespace_char(seg) {
                new_cursor += seg.len();
                seg_idx += 1;
            } else {
                break;
            }
        }
    }

    new_cursor
}

/// Heuristic: does this segment look like a word (contains alphanumeric chars)?
fn is_word_like(segment: &str) -> bool {
    segment.chars().any(|c| c.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backward_basic() {
        let text = "hello world";
        let pos = find_word_backward(text, 11, None); // after "world"
        assert_eq!(pos, 6); // before "world", after "hello "
    }

    #[test]
    fn test_forward_basic() {
        let text = "hello world";
        let pos = find_word_forward(text, 0, None); // at start
        assert_eq!(pos, 5); // after "hello"
    }

    #[test]
    fn test_backward_from_start() {
        let pos = find_word_backward("abc", 0, None);
        assert_eq!(pos, 0);
    }

    #[test]
    fn test_forward_from_end() {
        let pos = find_word_forward("abc", 3, None);
        assert_eq!(pos, 3);
    }
}
