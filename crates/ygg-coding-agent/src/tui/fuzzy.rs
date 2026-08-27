//! Fuzzy search primitives ported from the pi TUI (`fuzzy.ts` and
//! `session-selector-search.ts`).
//!
//! Matching runs on UTF-16 code units so that scores reproduce the
//! JavaScript reference implementation exactly. Lower scores are better
//! matches. The session-specific application of these primitives (search
//! text assembly, row filtering and ordering) lives in
//! [`crate::tui::view::panel_render`].

/// Result of one fuzzy token match. Lower `score` is a better match.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FuzzyMatch {
    pub(crate) matches: bool,
    pub(crate) score: f64,
}

/// Case-insensitive subsequence match with pi-compatible scoring.
///
/// Rewards consecutive runs and word-boundary hits, penalises gaps and late
/// positions, and gives an exact full-string match a large bonus. When the
/// query does not match directly, one alphanumeric-swap fallback is tried:
/// `abc123` also matches `123abc` (and vice versa) with a small penalty.
pub(crate) fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower: Vec<u16> = to_utf16(&query.to_lowercase());
    let text_lower: Vec<u16> = to_utf16(&text.to_lowercase());
    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }
    let swapped = swap_alphanumeric(&query.to_lowercase());
    if swapped.is_empty() {
        return primary;
    }
    let swapped_query: Vec<u16> = to_utf16(&swapped);
    let swapped_match = match_query(&swapped_query, &text_lower);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

fn to_utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn match_query(query: &[u16], text: &[u16]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }
    let mut query_index = 0;
    let mut score = 0.0f64;
    let mut last_match_index: i64 = -1;
    let mut consecutive_matches = 0i64;
    for i in 0..text.len() {
        if query_index >= query.len() {
            break;
        }
        if text[i] == query[query_index] {
            let is_word_boundary = match i {
                0 => true,
                _ => text.get(i - 1).is_some_and(|unit| is_boundary_unit(*unit)),
            };
            if last_match_index == i as i64 - 1 {
                consecutive_matches += 1;
                score -= consecutive_matches as f64 * 5.0;
            } else {
                consecutive_matches = 0;
                if last_match_index >= 0 {
                    score += (i as i64 - last_match_index - 1) as f64 * 2.0;
                }
            }
            if is_word_boundary {
                score -= 10.0;
            }
            score += i as f64 * 0.1;
            last_match_index = i as i64;
            query_index += 1;
        }
    }
    if query_index < query.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }
    if query == text {
        score -= 100.0;
    }
    FuzzyMatch {
        matches: true,
        score,
    }
}

fn is_boundary_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit))
        .map(|c| c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | ':'))
        .unwrap_or(false)
}

/// Return the swapped alphanumeric form of a lowercased query, or the empty
/// string when the query is not a plain `letters+digits` or `digits+letters`
/// run (matching pi's `swapAlphanumeric`).
fn swap_alphanumeric(query_lower: &str) -> String {
    // `[a-z]+[0-9]+` -> digits first, then letters.
    if let Some(i) = query_lower.bytes().position(|b| b.is_ascii_digit()) {
        if i > 0
            && query_lower[..i].bytes().all(|b| b.is_ascii_lowercase())
            && query_lower[i..].bytes().all(|b| b.is_ascii_digit())
        {
            return format!("{}{}", &query_lower[i..], &query_lower[..i]);
        }
    }
    // `[0-9]+[a-z]+` -> letters first, then digits.
    if let Some(i) = query_lower.bytes().position(|b| b.is_ascii_alphabetic()) {
        if i > 0
            && query_lower[..i].bytes().all(|b| b.is_ascii_digit())
            && query_lower[i..].bytes().all(|b| b.is_ascii_lowercase())
        {
            return format!("{}{}", &query_lower[i..], &query_lower[..i]);
        }
    }
    String::new()
}

/// Kind of a single search token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    /// Substring match with pi's fuzzy scoring.
    Fuzzy,
    /// Exact substring match, whitespace-insensitive, zero score.
    Phrase,
}

/// One token of a parsed search query.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SearchToken {
    pub(crate) kind: TokenKind,
    pub(crate) value: String,
}

/// Mode of a parsed search query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchMode {
    /// Fuzzy tokens and/or quoted exact phrases; every token must match.
    Tokens,
    /// A single case-insensitive regular expression.
    Regex,
}

/// A search query parsed into an executable matching description.
#[derive(Clone, Debug)]
pub(crate) struct ParsedSearchQuery {
    pub(crate) mode: SearchMode,
    pub(crate) tokens: Vec<SearchToken>,
    pub(crate) regex: Option<regex::Regex>,
    pub(crate) error: Option<String>,
}

impl ParsedSearchQuery {
    pub(crate) fn is_empty(&self) -> bool {
        self.mode == SearchMode::Tokens && self.tokens.is_empty()
    }
}

/// Parse a search query exactly like pi's `parseSearchQuery`.
///
/// - The empty string matches everything.
/// - `re:<pattern>` selects regex mode (case-insensitive).
/// - Otherwise the query is split on whitespace; `"quoted text"` wraps exact
///   phrase tokens and everything else becomes a fuzzy token.
/// - An unterminated quote degrades to whitespace-split fuzzy tokens.
pub(crate) fn parse_search_query(query: &str) -> ParsedSearchQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedSearchQuery {
            mode: SearchMode::Tokens,
            tokens: Vec::new(),
            regex: None,
            error: None,
        };
    }
    if let Some(rest) = trimmed.strip_prefix("re:") {
        let pattern = rest.trim();
        if pattern.is_empty() {
            return ParsedSearchQuery {
                mode: SearchMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some("Empty regex".to_owned()),
            };
        }
        return match regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(regex) => ParsedSearchQuery {
                mode: SearchMode::Regex,
                tokens: Vec::new(),
                regex: Some(regex),
                error: None,
            },
            Err(error) => ParsedSearchQuery {
                mode: SearchMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some(format!("Invalid regex: {error}")),
            },
        };
    }

    let mut tokens: Vec<SearchToken> = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    let mut had_unclosed_quote = false;
    for ch in trimmed.chars() {
        if ch == '"' {
            if in_quote {
                flush_token(&mut tokens, &mut buf, TokenKind::Phrase);
                in_quote = false;
            } else {
                flush_token(&mut tokens, &mut buf, TokenKind::Fuzzy);
                in_quote = true;
            }
            continue;
        }
        if !in_quote && ch.is_whitespace() {
            flush_token(&mut tokens, &mut buf, TokenKind::Fuzzy);
            continue;
        }
        buf.push(ch);
    }
    if in_quote {
        had_unclosed_quote = true;
    }
    if had_unclosed_quote {
        // pi falls back to whitespace-split fuzzy tokens without an error.
        let tokens = trimmed
            .split_whitespace()
            .filter(|token| !token.is_empty())
            .map(|token| SearchToken {
                kind: TokenKind::Fuzzy,
                value: token.to_owned(),
            })
            .collect();
        return ParsedSearchQuery {
            mode: SearchMode::Tokens,
            tokens,
            regex: None,
            error: None,
        };
    }
    if !buf.trim().is_empty() {
        tokens.push(SearchToken {
            kind: TokenKind::Fuzzy,
            value: buf.trim().to_owned(),
        });
    }
    ParsedSearchQuery {
        mode: SearchMode::Tokens,
        tokens,
        regex: None,
        error: None,
    }
}

fn flush_token(tokens: &mut Vec<SearchToken>, buf: &mut String, kind: TokenKind) {
    let value = buf.trim().to_owned();
    buf.clear();
    if !value.is_empty() {
        tokens.push(SearchToken { kind, value });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matching_rewards_exact_and_supports_alphanumeric_swaps() {
        assert!(fuzzy_match("alpha", "Alpha release").matches);
        assert!(fuzzy_match("abc123", "123abc").matches);
        assert!(fuzzy_match("界", "世界").matches);
        assert!(!fuzzy_match("missing", "present").matches);
        assert!(fuzzy_match("alpha", "alpha").score < fuzzy_match("alpha", "xalpha").score);
    }

    #[test]
    fn search_parser_supports_phrases_and_regex_errors() {
        let parsed = parse_search_query("one \"two words\"");
        assert_eq!(parsed.mode, SearchMode::Tokens);
        assert_eq!(
            parsed.tokens,
            vec![
                SearchToken {
                    kind: TokenKind::Fuzzy,
                    value: "one".into()
                },
                SearchToken {
                    kind: TokenKind::Phrase,
                    value: "two words".into()
                }
            ]
        );

        let regex = parse_search_query("re:foo.*bar");
        assert_eq!(regex.mode, SearchMode::Regex);
        assert!(regex.regex.is_some());
        assert!(parse_search_query("re:").error.is_some());
        assert!(parse_search_query("re:[").error.is_some());
    }

    #[test]
    fn unterminated_quotes_fall_back_to_fuzzy_tokens() {
        let parsed = parse_search_query("\"quoted text");
        assert_eq!(parsed.mode, SearchMode::Tokens);
        assert!(parsed
            .tokens
            .iter()
            .all(|token| token.kind == TokenKind::Fuzzy));
    }
}
