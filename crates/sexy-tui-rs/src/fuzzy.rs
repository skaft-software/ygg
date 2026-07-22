//! Fuzzy matching utilities.
//! Matches if all query characters appear in order (not necessarily consecutive).
//! Lower score = better match.

/// Result of a fuzzy match operation.
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

/// Check if query matches text using fuzzy matching.
/// All query characters must appear in text in order (not necessarily consecutive).
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_lowercase();
    let text_lower = text.to_lowercase();

    let match_query = |normalized: &str| -> FuzzyMatch {
        if normalized.is_empty() {
            return FuzzyMatch {
                matches: true,
                score: 0.0,
            };
        }

        if normalized.len() > text_lower.len() {
            return FuzzyMatch {
                matches: false,
                score: 0.0,
            };
        }

        let mut query_index = 0;
        let mut score = 0.0;
        let mut last_match_idx: i64 = -1;
        let mut consecutive_matches = 0;
        let query_chars: Vec<char> = normalized.chars().collect();
        let text_chars: Vec<char> = text_lower.chars().collect();

        for (i, &tc) in text_chars.iter().enumerate() {
            if query_index >= query_chars.len() {
                break;
            }
            if tc == query_chars[query_index] {
                let is_word_boundary = i == 0
                    || matches!(text_lower.chars().nth(i - 1), Some(c) if " -_./:".contains(c));

                // Reward consecutive matches
                if last_match_idx == (i as i64) - 1 {
                    consecutive_matches += 1;
                    score -= (consecutive_matches * 5) as f64;
                } else {
                    consecutive_matches = 0;
                    // Penalize gaps
                    if last_match_idx >= 0 {
                        score += ((i as i64) - last_match_idx - 1) as f64 * 2.0;
                    }
                }

                // Reward word boundary matches
                if is_word_boundary {
                    score -= 10.0;
                }

                // Slight penalty for later matches
                score += i as f64 * 0.1;

                last_match_idx = i as i64;
                query_index += 1;
            }
        }

        if query_index < query_chars.len() {
            return FuzzyMatch {
                matches: false,
                score: 0.0,
            };
        }

        if normalized == text_lower {
            score -= 100.0;
        }

        FuzzyMatch {
            matches: true,
            score,
        }
    };

    let primary = match_query(&query_lower);
    if primary.matches {
        return primary;
    }

    // Try swapped alpha-numeric ordering (e.g., "foo123" ↔ "123foo")
    let swapped = try_swap_alpha_numeric(&query_lower);
    if let Some(s) = swapped {
        let swapped_match = match_query(&s);
        if swapped_match.matches {
            return FuzzyMatch {
                matches: true,
                score: swapped_match.score + 5.0,
            };
        }
    }

    primary
}

fn try_swap_alpha_numeric(query: &str) -> Option<String> {
    // Match pattern: letters followed by digits
    if let Some(cap) = regex_alpha_digits(query) {
        return Some(format!("{}{}", cap.digits, cap.letters));
    }
    // Match pattern: digits followed by letters
    if let Some(cap) = regex_digits_alpha(query) {
        return Some(format!("{}{}", cap.letters, cap.digits));
    }
    None
}

struct AlphaDigits {
    letters: String,
    digits: String,
}

fn regex_alpha_digits(s: &str) -> Option<AlphaDigits> {
    let letters_end = s.find(|c: char| c.is_ascii_digit())?;
    let letters = s[..letters_end].to_string();
    let digits = s[letters_end..].to_string();
    if letters.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(AlphaDigits { letters, digits })
}

fn regex_digits_alpha(s: &str) -> Option<AlphaDigits> {
    let digits_end = s.find(|c: char| c.is_ascii_alphabetic())?;
    let digits = s[..digits_end].to_string();
    let letters = s[digits_end..].to_string();
    if digits.is_empty() || letters.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(AlphaDigits { letters, digits })
}

/// Filter and sort items by fuzzy match quality (best matches first).
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter<T: Clone, F: Fn(&T) -> String>(
    items: &[T],
    query: &str,
    get_text: F,
) -> Vec<T> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return items.to_vec();
    }

    let tokens: Vec<&str> = trimmed
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return items.to_vec();
    }

    let mut results: Vec<(T, f64)> = Vec::new();

    for item in items {
        let text = get_text(item);
        let mut total_score = 0.0;
        let mut all_match = true;

        for token in &tokens {
            let m = fuzzy_match(token, &text);
            if m.matches {
                total_score += m.score;
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            results.push((item.clone(), total_score));
        }
    }

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        let m = fuzzy_match("hello", "hello");
        assert!(m.matches);
        assert!(m.score < 0.0); // exact match gets bonus
    }

    #[test]
    fn test_subsequence_match() {
        let m = fuzzy_match("hlo", "hello");
        assert!(m.matches);
    }

    #[test]
    fn test_no_match() {
        let m = fuzzy_match("xyz", "hello");
        assert!(!m.matches);
    }

    #[test]
    fn test_filter_empty_query() {
        let items = vec!["alpha", "beta", "gamma"];
        let result = fuzzy_filter(&items, "", |s: &&str| s.to_string());
        assert_eq!(result, items);
    }

    #[test]
    fn test_filter_substring() {
        let items = vec!["application", "banana", "apple"];
        let result = fuzzy_filter(&items, "app", |s: &&str| s.to_string());
        // "apple" and "application" both match "app" — either could come first
        assert!(!result.is_empty());
        assert!(result.contains(&"apple"));
    }
}
