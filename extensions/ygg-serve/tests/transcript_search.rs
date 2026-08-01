#![allow(missing_docs)]

#[path = "../src/transcript_search.rs"]
mod transcript_search;

use std::collections::BTreeSet;

use serde_json::json;
use transcript_search::*;

fn document(
    session_id: &str,
    item_id: &str,
    kind: SearchDocumentKind,
    title: &str,
    text: &str,
    timestamp_ms: u64,
) -> SearchDocument {
    SearchDocument {
        session_id: session_id.into(),
        item_id: item_id.into(),
        kind,
        session_title: title.into(),
        text: text.into(),
        timestamp_ms,
    }
}

fn small_limits() -> TranscriptSearchLimits {
    TranscriptSearchLimits {
        max_documents: 4,
        max_documents_per_session: 3,
        max_indexed_text_bytes: 256,
        max_unique_terms: 12,
        max_postings: 16,
    }
}

#[test]
fn indexes_every_public_persisted_category_and_filters_them() {
    let documents: Vec<SearchDocument> = serde_json::from_str(include_str!(
        "../fixtures/transcript-search-visible-documents.json"
    ))
    .expect("golden public transcript projections");
    assert_eq!(
        documents
            .iter()
            .map(|document| document.kind)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            SearchDocumentKind::User,
            SearchDocumentKind::Assistant,
            SearchDocumentKind::Tool,
            SearchDocumentKind::Error,
            SearchDocumentKind::Attachment,
        ])
    );

    let mut index = TranscriptSearchIndex::new();
    index.replace_session("session-visible", documents).unwrap();
    for kind in [
        SearchDocumentKind::User,
        SearchDocumentKind::Assistant,
        SearchDocumentKind::Tool,
        SearchDocumentKind::Error,
        SearchDocumentKind::Attachment,
    ] {
        let hits = index
            .search(
                "reconnect",
                &SearchFilter {
                    session_id: Some("session-visible".into()),
                    kinds: BTreeSet::from([kind]),
                },
                10,
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "missing category {kind:?}");
        assert_eq!(hits[0].kind, kind);
    }
}

#[test]
fn public_dtos_are_camel_case_path_free_and_reject_private_fields() {
    let request = TranscriptSearchRequest {
        query: "reconnect".into(),
        filter: SearchFilter {
            session_id: Some("session-visible".into()),
            kinds: BTreeSet::from([SearchDocumentKind::Tool]),
        },
        limit: 20,
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "query": "reconnect",
            "filter": {
                "sessionId": "session-visible",
                "kinds": ["tool"]
            },
            "limit": 20
        })
    );

    let unknown_private_field = json!({
        "sessionId": "session-visible",
        "itemId": "tool-1",
        "kind": "tool",
        "sessionTitle": "Visible title",
        "text": "Visible summary",
        "timestampMs": 1,
        "rawToolArguments": {"token": "must-not-enter-index"}
    });
    assert!(serde_json::from_value::<SearchDocument>(unknown_private_field).is_err());

    let serialized = serde_json::to_string(&TranscriptSearchResult {
        hits: vec![SearchHit {
            session_id: "session-visible".into(),
            item_id: "tool-1".into(),
            kind: SearchDocumentKind::Tool,
            session_title: "Visible title".into(),
            snippet: "Visible summary".into(),
            match_ranges: vec![SearchMatchRange {
                start_char: 0,
                end_char: 7,
            }],
            title_match_ranges: Vec::new(),
            timestamp_ms: 1,
            score: 10,
        }],
        truncated: false,
    })
    .unwrap();
    for forbidden in [
        "hostPath",
        "cwd",
        "rawToolArguments",
        "privateAnswer",
        "hiddenReasoning",
        "secret",
    ] {
        assert!(!serialized.contains(forbidden));
    }
}

#[test]
fn upsert_replaces_duplicate_identity_without_stale_terms() {
    let mut index = TranscriptSearchIndex::new();
    index
        .upsert_document(document(
            "session-a",
            "item-1",
            SearchDocumentKind::Assistant,
            "Session",
            "obsolete canary",
            1,
        ))
        .unwrap();
    index
        .upsert_document(document(
            "session-a",
            "item-1",
            SearchDocumentKind::Assistant,
            "Renamed",
            "current answer",
            2,
        ))
        .unwrap();

    assert_eq!(index.len(), 1);
    assert!(index
        .search("obsolete", &SearchFilter::default(), 10)
        .unwrap()
        .is_empty());
    assert!(index
        .search("session", &SearchFilter::default(), 10)
        .unwrap()
        .is_empty());
    let hit = index
        .search("current", &SearchFilter::default(), 10)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(hit.timestamp_ms, 2);
    assert_eq!(hit.session_title, "Renamed");
}

#[test]
fn session_replacement_is_incremental_atomic_and_last_duplicate_wins() {
    let mut index = TranscriptSearchIndex::with_limits(small_limits()).unwrap();
    index
        .upsert_document(document(
            "other",
            "stable",
            SearchDocumentKind::User,
            "Other",
            "untouched anchor",
            1,
        ))
        .unwrap();
    index
        .replace_session(
            "session-a",
            [
                document(
                    "session-a",
                    "duplicate",
                    SearchDocumentKind::Assistant,
                    "First",
                    "obsolete version",
                    2,
                ),
                document(
                    "session-a",
                    "duplicate",
                    SearchDocumentKind::Assistant,
                    "Second",
                    "winning version",
                    3,
                ),
            ],
        )
        .unwrap();

    assert_eq!(index.len(), 2);
    assert!(index
        .search("obsolete", &SearchFilter::default(), 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        index
            .search("winning", &SearchFilter::default(), 10)
            .unwrap()[0]
            .session_title,
        "Second"
    );
    assert_eq!(
        index
            .search("anchor", &SearchFilter::default(), 10)
            .unwrap()[0]
            .item_id,
        "stable"
    );

    let before = index.stats();
    let result = index.replace_session(
        "session-a",
        [document(
            "wrong-session",
            "bad",
            SearchDocumentKind::User,
            "Bad",
            "must not partially replace",
            4,
        )],
    );
    assert_eq!(result, Err(SearchError::InvalidText));
    assert_eq!(index.stats(), before);
    assert!(!index
        .search("winning", &SearchFilter::default(), 10)
        .unwrap()
        .is_empty());
}

#[test]
fn document_and_session_removal_clear_only_their_postings() {
    let mut index = TranscriptSearchIndex::new();
    for (session_id, item_id, text) in [
        ("session-a", "one", "shared alpha"),
        ("session-a", "two", "shared beta"),
        ("session-b", "one", "shared gamma"),
    ] {
        index
            .upsert_document(document(
                session_id,
                item_id,
                SearchDocumentKind::User,
                "Title",
                text,
                1,
            ))
            .unwrap();
    }

    assert!(index.remove_document("session-a", "one").unwrap());
    assert!(!index.remove_document("session-a", "missing").unwrap());
    assert!(index
        .search("alpha", &SearchFilter::default(), 10)
        .unwrap()
        .is_empty());
    assert_eq!(index.remove_session("session-a").unwrap(), 1);
    assert_eq!(
        index
            .search("shared", &SearchFilter::default(), 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.session_id)
            .collect::<Vec<_>>(),
        ["session-b"]
    );
    assert_eq!(index.remove_session("session-a").unwrap(), 0);
}

#[test]
fn configured_capacity_is_bounded_and_failed_updates_are_atomic() {
    assert_eq!(
        TranscriptSearchIndex::with_limits(TranscriptSearchLimits {
            max_documents: MAX_SEARCH_DOCUMENTS + 1,
            ..small_limits()
        })
        .unwrap_err(),
        SearchError::InvalidLimits
    );
    assert_eq!(
        TranscriptSearchIndex::with_limits(TranscriptSearchLimits {
            max_documents_per_session: 5,
            ..small_limits()
        })
        .unwrap_err(),
        SearchError::InvalidLimits
    );

    let mut index = TranscriptSearchIndex::with_limits(small_limits()).unwrap();
    for item in ["one", "two", "three"] {
        index
            .upsert_document(document(
                "session-a",
                item,
                SearchDocumentKind::User,
                "Title",
                item,
                1,
            ))
            .unwrap();
    }
    let before = index.stats();
    assert_eq!(
        index.upsert_document(document(
            "session-a",
            "four",
            SearchDocumentKind::User,
            "Title",
            "four",
            1,
        )),
        Err(SearchError::Capacity)
    );
    assert_eq!(index.stats(), before);
    index
        .upsert_document(document(
            "session-b",
            "four",
            SearchDocumentKind::User,
            "Title",
            "four",
            1,
        ))
        .unwrap();
    assert_eq!(
        index.upsert_document(document(
            "session-c",
            "five",
            SearchDocumentKind::User,
            "Title",
            "five",
            1,
        )),
        Err(SearchError::Capacity),
        "the global document cap applies across sessions"
    );

    let mut term_limited = TranscriptSearchIndex::with_limits(TranscriptSearchLimits {
        max_unique_terms: 2,
        ..small_limits()
    })
    .unwrap();
    term_limited
        .upsert_document(document(
            "session-a",
            "one",
            SearchDocumentKind::User,
            "same",
            "alpha",
            1,
        ))
        .unwrap();
    assert_eq!(
        term_limited.upsert_document(document(
            "session-a",
            "two",
            SearchDocumentKind::User,
            "same",
            "beta",
            1,
        )),
        Err(SearchError::Capacity)
    );
    assert!(!term_limited
        .search("alpha", &SearchFilter::default(), 10)
        .unwrap()
        .is_empty());

    let mut text_limited = TranscriptSearchIndex::with_limits(TranscriptSearchLimits {
        max_indexed_text_bytes: 10,
        ..small_limits()
    })
    .unwrap();
    text_limited
        .upsert_document(document(
            "session-a",
            "one",
            SearchDocumentKind::User,
            "Title",
            "alpha",
            1,
        ))
        .unwrap();
    assert_eq!(
        text_limited.upsert_document(document(
            "session-b",
            "two",
            SearchDocumentKind::User,
            "T",
            "beta",
            1,
        )),
        Err(SearchError::Capacity)
    );

    let mut posting_limited = TranscriptSearchIndex::with_limits(TranscriptSearchLimits {
        max_postings: 2,
        ..small_limits()
    })
    .unwrap();
    posting_limited
        .upsert_document(document(
            "session-a",
            "one",
            SearchDocumentKind::User,
            "Title",
            "alpha",
            1,
        ))
        .unwrap();
    assert_eq!(
        posting_limited.upsert_document(document(
            "session-b",
            "two",
            SearchDocumentKind::User,
            "Title",
            "beta",
            1,
        )),
        Err(SearchError::Capacity)
    );
    posting_limited
        .upsert_document(document(
            "session-a",
            "one",
            SearchDocumentKind::User,
            "Title",
            "beta",
            2,
        ))
        .expect("replacement reuses the posting budget");
}

#[test]
fn unicode_snippets_and_ranges_use_source_scalar_offsets_after_case_expansion() {
    let prefix = "🙂".repeat(MAX_SEARCH_SNIPPET_CHARS);
    let text = format!("{prefix} İSTANBUL résumé naïve 東京");
    let mut index = TranscriptSearchIndex::new();
    index
        .upsert_document(document(
            "session-a",
            "unicode",
            SearchDocumentKind::Attachment,
            "İstanbul research",
            &text,
            1,
        ))
        .unwrap();

    let hit = index
        .search("İstanbul naïve", &SearchFilter::default(), 1)
        .unwrap()
        .pop()
        .unwrap();
    assert!(hit.snippet.chars().count() <= MAX_SEARCH_SNIPPET_CHARS);
    assert_eq!(hit.match_ranges.len(), 2);
    let highlighted = hit
        .match_ranges
        .iter()
        .map(|range| {
            hit.snippet
                .chars()
                .skip(range.start_char)
                .take(range.end_char - range.start_char)
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert_eq!(highlighted, ["İSTANBUL", "naïve"]);
    let title_highlight = hit
        .session_title
        .chars()
        .skip(hit.title_match_ranges[0].start_char)
        .take(hit.title_match_ranges[0].end_char - hit.title_match_ranges[0].start_char)
        .collect::<String>();
    assert_eq!(title_highlight, "İstanbul");

    let cjk = index
        .search("東京", &SearchFilter::default(), 1)
        .unwrap()
        .pop()
        .unwrap();
    let range = cjk.match_ranges[0];
    assert_eq!(
        cjk.snippet
            .chars()
            .skip(range.start_char)
            .take(range.end_char - range.start_char)
            .collect::<String>(),
        "東京"
    );
}

#[test]
fn title_only_matches_have_title_ranges_without_fake_snippet_ranges() {
    let mut index = TranscriptSearchIndex::new();
    index
        .upsert_document(document(
            "session-a",
            "item",
            SearchDocumentKind::Assistant,
            "Reconnect diagnosis",
            "The visible answer does not repeat the heading.",
            1,
        ))
        .unwrap();
    let hit = index
        .search("reconnect", &SearchFilter::default(), 1)
        .unwrap()
        .pop()
        .unwrap();
    assert!(hit.match_ranges.is_empty());
    assert_eq!(
        hit.title_match_ranges,
        [SearchMatchRange {
            start_char: 0,
            end_char: 9
        }]
    );

    assert_eq!(
        index
            .update_session_title("session-a", "Durable replay diagnosis")
            .unwrap(),
        1
    );
    assert!(index
        .search("reconnect", &SearchFilter::default(), 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        index
            .search("durable", &SearchFilter::default(), 1)
            .unwrap()[0]
            .session_title,
        "Durable replay diagnosis"
    );
    assert_eq!(
        index.update_session_title("session-a", &"x".repeat(MAX_SEARCH_TERM_CHARS + 1)),
        Err(SearchError::TooLarge)
    );
    assert!(!index
        .search("durable", &SearchFilter::default(), 1)
        .unwrap()
        .is_empty());
}

#[test]
fn and_semantics_filters_ranking_ties_and_truncation_are_stable() {
    let mut index = TranscriptSearchIndex::new();
    for document in [
        document(
            "session-b",
            "same",
            SearchDocumentKind::Tool,
            "Run",
            "tests passed",
            8,
        ),
        document(
            "session-a",
            "later",
            SearchDocumentKind::Tool,
            "Run",
            "tests passed",
            9,
        ),
        document(
            "session-a",
            "more",
            SearchDocumentKind::Tool,
            "Run",
            "tests tests passed",
            1,
        ),
        document(
            "session-a",
            "failed",
            SearchDocumentKind::Error,
            "Run",
            "tests failed",
            10,
        ),
    ] {
        index.upsert_document(document).unwrap();
    }

    let result = index
        .search_request(&TranscriptSearchRequest {
            query: "tests passed tests".into(),
            filter: SearchFilter {
                session_id: Some("session-a".into()),
                kinds: BTreeSet::from([SearchDocumentKind::Tool]),
            },
            limit: 1,
        })
        .unwrap();
    assert_eq!(result.hits[0].item_id, "more");
    assert!(result.truncated);
    assert!(index
        .search("tests absent", &SearchFilter::default(), 10)
        .unwrap()
        .is_empty());

    let ties = index
        .search("tests passed", &SearchFilter::default(), 10)
        .unwrap();
    assert_eq!(
        ties.into_iter()
            .map(|hit| (hit.session_id, hit.item_id))
            .collect::<Vec<_>>(),
        [
            ("session-a".into(), "more".into()),
            ("session-a".into(), "later".into()),
            ("session-b".into(), "same".into()),
        ]
    );
}

#[test]
fn rejects_invalid_identity_controls_bounds_and_result_limits() {
    let mut index = TranscriptSearchIndex::new();
    for bad in [
        document(
            "../session",
            "item",
            SearchDocumentKind::User,
            "Title",
            "text",
            1,
        ),
        document(
            "session",
            "item/../../secret",
            SearchDocumentKind::User,
            "Title",
            "text",
            1,
        ),
        document(
            "session",
            "item",
            SearchDocumentKind::User,
            "Title",
            "bad\u{202e}text",
            1,
        ),
        document(
            "session",
            "item",
            SearchDocumentKind::User,
            "Title",
            " \n\t ",
            1,
        ),
    ] {
        assert_eq!(index.upsert_document(bad), Err(SearchError::InvalidText));
    }
    assert_eq!(
        index.upsert_document(document(
            "session",
            "item",
            SearchDocumentKind::User,
            "Title",
            &"x".repeat(MAX_SEARCH_DOCUMENT_TEXT_BYTES + 1),
            1,
        )),
        Err(SearchError::TooLarge)
    );
    assert_eq!(
        index.upsert_document(document(
            "session",
            "item",
            SearchDocumentKind::User,
            "Title",
            &"x".repeat(MAX_SEARCH_TERM_CHARS + 1),
            1,
        )),
        Err(SearchError::TooLarge)
    );
    assert_eq!(
        index.search("", &SearchFilter::default(), 10),
        Err(SearchError::EmptyQuery)
    );
    assert_eq!(
        index.search("ok", &SearchFilter::default(), 0),
        Err(SearchError::InvalidLimit)
    );
    assert_eq!(
        index.search("ok", &SearchFilter::default(), MAX_SEARCH_RESULTS + 1),
        Err(SearchError::InvalidLimit)
    );
    assert_eq!(
        index.search(
            &"a ".repeat(MAX_SEARCH_QUERY_TERMS + 1),
            &SearchFilter::default(),
            10,
        ),
        Ok(Vec::new()),
        "duplicate query terms are intentionally deduplicated"
    );
    assert_eq!(
        index.search(
            &(0..=MAX_SEARCH_QUERY_TERMS)
                .map(|term| format!("term{term}"))
                .collect::<Vec<_>>()
                .join(" "),
            &SearchFilter::default(),
            10,
        ),
        Err(SearchError::TooLarge)
    );
    assert_eq!(
        index.search(
            &"x".repeat(MAX_SEARCH_QUERY_CHARS + 1),
            &SearchFilter::default(),
            10,
        ),
        Err(SearchError::TooLarge)
    );
    assert_eq!(
        index.search(
            &"x".repeat(MAX_SEARCH_TERM_CHARS + 1),
            &SearchFilter::default(),
            10,
        ),
        Err(SearchError::TooLarge)
    );
}

#[test]
fn stats_track_incremental_replacement_without_leaking_content() {
    let mut index = TranscriptSearchIndex::new();
    index
        .upsert_document(document(
            "session",
            "item",
            SearchDocumentKind::User,
            "Title",
            "alpha beta",
            1,
        ))
        .unwrap();
    let first = index.stats();
    assert!(!index.is_empty());
    assert_eq!(first.indexed_documents, 1);
    assert_eq!(first.indexed_sessions, 1);
    assert_eq!(first.postings, 3);
    assert_eq!(first.unique_terms, 3);

    index
        .upsert_document(document(
            "session",
            "item",
            SearchDocumentKind::User,
            "Title",
            "alpha",
            2,
        ))
        .unwrap();
    let replaced = index.stats();
    assert_eq!(replaced.indexed_documents, 1);
    assert_eq!(replaced.indexed_sessions, 1);
    assert_eq!(replaced.postings, 2);
    assert_eq!(replaced.unique_terms, 2);
    assert!(serde_json::to_string(&replaced)
        .unwrap()
        .contains("\"indexedDocuments\":1"));
}
