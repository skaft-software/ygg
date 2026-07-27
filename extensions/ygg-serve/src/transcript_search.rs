//! Bounded incremental search over already-public transcript projections.
//!
//! This module deliberately accepts only a small, path-free projection. The
//! adapter that reads durable history must copy user-visible text into
//! [`SearchDocument`] and must never pass raw tool arguments, private
//! request-user-input answers, host paths, secrets, hidden reasoning, or
//! unprojected model history.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Hard upper bound for documents retained by one in-memory index.
pub const MAX_SEARCH_DOCUMENTS: usize = 50_000;
/// Hard upper bound for documents from one session.
pub const MAX_SEARCH_DOCUMENTS_PER_SESSION: usize = 10_000;
/// Hard upper bound for UTF-8 text in one public projection.
pub const MAX_SEARCH_DOCUMENT_TEXT_BYTES: usize = 64 * 1024;
/// Hard upper bound for searchable UTF-8 text retained by one index.
pub const MAX_SEARCH_INDEXED_TEXT_BYTES: usize = 64 * 1024 * 1024;
/// Hard upper bound for unique terms retained by one index.
pub const MAX_SEARCH_UNIQUE_TERMS: usize = 250_000;
/// Hard upper bound for document-to-term postings retained by one index.
pub const MAX_SEARCH_POSTINGS: usize = 2_000_000;
/// Hard upper bound for unique terms contributed by one document.
pub const MAX_SEARCH_TERMS_PER_DOCUMENT: usize = 2_048;
/// Hard upper bound for Unicode scalar values in one term.
pub const MAX_SEARCH_TERM_CHARS: usize = 128;
/// Hard upper bound for Unicode scalar values in one query.
pub const MAX_SEARCH_QUERY_CHARS: usize = 512;
/// Hard upper bound for distinct query terms.
pub const MAX_SEARCH_QUERY_TERMS: usize = 16;
/// Hard upper bound for returned hits.
pub const MAX_SEARCH_RESULTS: usize = 100;
/// Maximum Unicode scalar values in one result excerpt.
pub const MAX_SEARCH_SNIPPET_CHARS: usize = 240;

const MAX_IDENTITY_BYTES: usize = 256;
const MAX_SESSION_TITLE_BYTES: usize = 512;

/// Searchable, already-public transcript category.
///
/// These are the only persisted categories admitted by the search boundary.
/// In particular, there is no category for hidden reasoning, raw tool input,
/// private answers, credentials, or host filesystem metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchDocumentKind {
    /// Submitted user-visible message text.
    User,
    /// Assistant-visible response text.
    Assistant,
    /// Redacted tool title, target, status, or public result summary.
    Tool,
    /// Public failure or warning summary.
    Error,
    /// Public attachment display name or explicitly user-approved extracted text.
    Attachment,
}

/// One bounded, already-redacted searchable transcript projection.
///
/// This DTO is intentionally path-free and rejects unknown serialized fields.
/// A durable-history adapter should construct one document per visible
/// transcript item and use the durable session/item identities for deep links.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchDocument {
    /// Stable opaque session identifier used for deep links.
    pub session_id: String,
    /// Stable opaque transcript item identifier used for jump-to-item.
    pub item_id: String,
    /// Public category.
    pub kind: SearchDocumentKind,
    /// Public session title at index time.
    pub session_title: String,
    /// Public searchable text, already redacted by the durable-history adapter.
    pub text: String,
    /// Host timestamp used for deterministic result ordering.
    pub timestamp_ms: u64,
}

/// Optional query restrictions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchFilter {
    /// Restrict results to one opaque session identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Restrict results to selected public categories; empty means all.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub kinds: BTreeSet<SearchDocumentKind>,
}

/// One half-open match range expressed in Unicode scalar indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchMatchRange {
    /// Inclusive Unicode scalar offset.
    pub start_char: usize,
    /// Exclusive Unicode scalar offset.
    pub end_char: usize,
}

/// Stable search hit with bounded public text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchHit {
    /// Stable opaque session identifier.
    pub session_id: String,
    /// Stable opaque transcript item identifier.
    pub item_id: String,
    /// Public category.
    pub kind: SearchDocumentKind,
    /// Public session title.
    pub session_title: String,
    /// Bounded excerpt from public item text.
    pub snippet: String,
    /// Exact matching ranges within `snippet`, as Unicode scalar indexes.
    pub match_ranges: Vec<SearchMatchRange>,
    /// Matching ranges within `session_title`, as Unicode scalar indexes.
    pub title_match_ranges: Vec<SearchMatchRange>,
    /// Host timestamp.
    pub timestamp_ms: u64,
    /// Deterministic relevance score.
    pub score: u32,
}

/// Path-free request DTO for an authenticated search route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TranscriptSearchRequest {
    /// Plain-text AND query.
    pub query: String,
    /// Optional session/category restrictions.
    #[serde(default)]
    pub filter: SearchFilter,
    /// Requested result count.
    pub limit: usize,
}

/// Path-free response DTO for an authenticated search route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TranscriptSearchResult {
    /// Deterministically ranked public hits.
    pub hits: Vec<SearchHit>,
    /// Whether additional matches exist beyond `hits`.
    pub truncated: bool,
}

/// Path-free operational statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TranscriptSearchStats {
    /// Number of indexed public documents.
    pub indexed_documents: usize,
    /// Number of sessions represented by those documents.
    pub indexed_sessions: usize,
    /// UTF-8 bytes retained from titles and public item text.
    pub indexed_text_bytes: usize,
    /// Unique normalized terms.
    pub unique_terms: usize,
    /// Document-to-term relationships.
    pub postings: usize,
}

/// Conservative per-index limits.
///
/// [`TranscriptSearchIndex::with_limits`] permits smaller values for a host
/// with a tighter memory budget, but rejects values above the hard bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptSearchLimits {
    /// Maximum retained documents.
    pub max_documents: usize,
    /// Maximum retained documents from one session.
    pub max_documents_per_session: usize,
    /// Maximum aggregate public text bytes.
    pub max_indexed_text_bytes: usize,
    /// Maximum unique normalized terms.
    pub max_unique_terms: usize,
    /// Maximum document-to-term relationships.
    pub max_postings: usize,
}

impl Default for TranscriptSearchLimits {
    fn default() -> Self {
        Self {
            max_documents: MAX_SEARCH_DOCUMENTS,
            max_documents_per_session: MAX_SEARCH_DOCUMENTS_PER_SESSION,
            max_indexed_text_bytes: MAX_SEARCH_INDEXED_TEXT_BYTES,
            max_unique_terms: MAX_SEARCH_UNIQUE_TERMS,
            max_postings: MAX_SEARCH_POSTINGS,
        }
    }
}

/// Search validation or capacity failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    /// Query is empty after tokenization.
    #[error("search query is empty")]
    EmptyQuery,
    /// Query, document, or term set exceeds its public bound.
    #[error("search input exceeds its bound")]
    TooLarge,
    /// Document identity or public text contains forbidden controls.
    #[error("search input contains invalid public text")]
    InvalidText,
    /// Requested result count is outside the public bound.
    #[error("search result limit is invalid")]
    InvalidLimit,
    /// Configured limits are zero, inconsistent, or above hard bounds.
    #[error("search index limits are invalid")]
    InvalidLimits,
    /// Index capacity was reached.
    #[error("search index capacity was reached")]
    Capacity,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DocumentKey {
    session_id: String,
    item_id: String,
}

impl DocumentKey {
    fn new(session_id: &str, item_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            item_id: item_id.to_owned(),
        }
    }

    fn for_document(document: &SearchDocument) -> Self {
        Self::new(&document.session_id, &document.item_id)
    }
}

#[derive(Clone, Debug)]
struct IndexedDocument {
    public: SearchDocument,
    terms: BTreeSet<String>,
    stored_text_bytes: usize,
}

impl IndexedDocument {
    fn validate(document: SearchDocument) -> Result<Self, SearchError> {
        validate_identity(&document.session_id)?;
        validate_identity(&document.item_id)?;
        validate_public_text(&document.session_title, MAX_SESSION_TITLE_BYTES)?;
        validate_public_text(&document.text, MAX_SEARCH_DOCUMENT_TEXT_BYTES)?;
        if document.text.trim().is_empty() {
            return Err(SearchError::InvalidText);
        }

        let terms = terms_for_document(&document)?;
        let stored_text_bytes = document
            .session_title
            .len()
            .checked_add(document.text.len())
            .ok_or(SearchError::TooLarge)?;
        Ok(Self {
            public: document,
            terms,
            stored_text_bytes,
        })
    }
}

/// Rebuildable, bounded, incremental in-memory transcript index.
///
/// The index is intentionally not serialized. The host rebuilds it from
/// durable, already-redacted transcript projections after restart.
#[derive(Clone, Debug)]
pub struct TranscriptSearchIndex {
    documents: BTreeMap<DocumentKey, IndexedDocument>,
    postings: BTreeMap<String, BTreeSet<DocumentKey>>,
    session_counts: BTreeMap<String, usize>,
    indexed_text_bytes: usize,
    posting_count: usize,
    limits: TranscriptSearchLimits,
}

impl Default for TranscriptSearchIndex {
    fn default() -> Self {
        Self::with_limits(TranscriptSearchLimits::default())
            .expect("default transcript-search limits are valid")
    }
}

impl TranscriptSearchIndex {
    /// Creates an empty index with conservative production bounds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty index with limits no larger than the hard bounds.
    pub fn with_limits(limits: TranscriptSearchLimits) -> Result<Self, SearchError> {
        validate_limits(limits)?;
        Ok(Self {
            documents: BTreeMap::new(),
            postings: BTreeMap::new(),
            session_counts: BTreeMap::new(),
            indexed_text_bytes: 0,
            posting_count: 0,
            limits,
        })
    }

    /// Number of indexed public documents.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether no public documents are indexed.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Returns bounded, path-free index statistics.
    pub fn stats(&self) -> TranscriptSearchStats {
        TranscriptSearchStats {
            indexed_documents: self.documents.len(),
            indexed_sessions: self.session_counts.len(),
            indexed_text_bytes: self.indexed_text_bytes,
            unique_terms: self.postings.len(),
            postings: self.posting_count,
        }
    }

    /// Incrementally inserts or replaces one durable transcript item.
    ///
    /// Identity is `(session_id, item_id)`. Reusing that identity replaces the
    /// old public projection and removes its stale terms atomically.
    pub fn upsert_document(&mut self, document: SearchDocument) -> Result<(), SearchError> {
        let incoming = IndexedDocument::validate(document)?;
        let key = DocumentKey::for_document(&incoming.public);
        let removals = self.documents.contains_key(&key).then_some(key.clone());
        self.ensure_capacity(removals.iter(), std::iter::once(&incoming))?;

        if let Some(removal) = removals {
            self.remove_unchecked(&removal);
        }
        self.insert_unchecked(key, incoming);
        Ok(())
    }

    /// Incrementally replaces every public document belonging to one session.
    ///
    /// The operation is atomic with respect to validation and capacity. If the
    /// input repeats an item identity, its last projection wins.
    pub fn replace_session(
        &mut self,
        session_id: &str,
        documents: impl IntoIterator<Item = SearchDocument>,
    ) -> Result<(), SearchError> {
        validate_identity(session_id)?;
        let mut incoming = BTreeMap::<DocumentKey, IndexedDocument>::new();
        for document in documents {
            let indexed = IndexedDocument::validate(document)?;
            if indexed.public.session_id != session_id {
                return Err(SearchError::InvalidText);
            }
            incoming.insert(DocumentKey::for_document(&indexed.public), indexed);
        }
        if incoming.len() > self.limits.max_documents_per_session {
            return Err(SearchError::Capacity);
        }

        let removals = self
            .documents
            .keys()
            .filter(|key| key.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        self.ensure_capacity(removals.iter(), incoming.values())?;

        for key in removals {
            self.remove_unchecked(&key);
        }
        for (key, document) in incoming {
            self.insert_unchecked(key, document);
        }
        Ok(())
    }

    /// Incrementally refreshes title terms for one renamed session.
    ///
    /// Only that session's bounded projections are re-tokenized. On validation
    /// or capacity failure, the existing title terms remain intact.
    pub fn update_session_title(
        &mut self,
        session_id: &str,
        session_title: &str,
    ) -> Result<usize, SearchError> {
        validate_identity(session_id)?;
        validate_public_text(session_title, MAX_SESSION_TITLE_BYTES)?;
        let removals = self
            .documents
            .keys()
            .filter(|key| key.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut additions = Vec::with_capacity(removals.len());
        for key in &removals {
            let mut public = self
                .documents
                .get(key)
                .ok_or(SearchError::Capacity)?
                .public
                .clone();
            public.session_title = session_title.to_owned();
            additions.push((key.clone(), IndexedDocument::validate(public)?));
        }
        self.ensure_capacity(
            removals.iter(),
            additions.iter().map(|(_, document)| document),
        )?;
        for key in &removals {
            self.remove_unchecked(key);
        }
        let updated = additions.len();
        for (key, document) in additions {
            self.insert_unchecked(key, document);
        }
        Ok(updated)
    }

    /// Removes one public transcript item, returning whether it existed.
    pub fn remove_document(
        &mut self,
        session_id: &str,
        item_id: &str,
    ) -> Result<bool, SearchError> {
        validate_identity(session_id)?;
        validate_identity(item_id)?;
        Ok(self
            .remove_unchecked(&DocumentKey::new(session_id, item_id))
            .is_some())
    }

    /// Removes all documents for one deleted session.
    pub fn remove_session(&mut self, session_id: &str) -> Result<usize, SearchError> {
        validate_identity(session_id)?;
        let keys = self
            .documents
            .keys()
            .filter(|key| key.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        let removed = keys.len();
        for key in keys {
            self.remove_unchecked(&key);
        }
        Ok(removed)
    }

    /// Searches all distinct terms with deterministic AND semantics.
    pub fn search(
        &self,
        query: &str,
        filter: &SearchFilter,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchError> {
        self.search_request(&TranscriptSearchRequest {
            query: query.to_owned(),
            filter: filter.clone(),
            limit,
        })
        .map(|result| result.hits)
    }

    /// Searches using the path-free transport request DTO.
    pub fn search_request(
        &self,
        request: &TranscriptSearchRequest,
    ) -> Result<TranscriptSearchResult, SearchError> {
        if !(1..=MAX_SEARCH_RESULTS).contains(&request.limit) {
            return Err(SearchError::InvalidLimit);
        }
        let terms = query_terms(&request.query)?;
        if let Some(session_id) = &request.filter.session_id {
            validate_identity(session_id)?;
        }
        if terms.iter().any(|term| !self.postings.contains_key(term)) {
            return Ok(TranscriptSearchResult {
                hits: Vec::new(),
                truncated: false,
            });
        }

        let Some(mut candidates) = terms
            .iter()
            .filter_map(|term| self.postings.get(term).cloned())
            .reduce(|left, right| left.intersection(&right).cloned().collect())
        else {
            return Ok(TranscriptSearchResult {
                hits: Vec::new(),
                truncated: false,
            });
        };

        if let Some(session_id) = &request.filter.session_id {
            candidates.retain(|key| &key.session_id == session_id);
        }
        if !request.filter.kinds.is_empty() {
            candidates.retain(|key| {
                self.documents
                    .get(key)
                    .is_some_and(|document| request.filter.kinds.contains(&document.public.kind))
            });
        }

        let mut hits = candidates
            .into_iter()
            .filter_map(|key| self.documents.get(&key))
            .map(|document| hit_for(&document.public, &terms))
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.timestamp_ms.cmp(&left.timestamp_ms))
                .then_with(|| left.session_id.cmp(&right.session_id))
                .then_with(|| left.item_id.cmp(&right.item_id))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        let truncated = hits.len() > request.limit;
        hits.truncate(request.limit);
        Ok(TranscriptSearchResult { hits, truncated })
    }

    fn ensure_capacity<'a>(
        &self,
        removals: impl IntoIterator<Item = &'a DocumentKey>,
        additions: impl IntoIterator<Item = &'a IndexedDocument>,
    ) -> Result<(), SearchError> {
        let removals = removals.into_iter().collect::<BTreeSet<_>>();
        let additions = additions.into_iter().collect::<Vec<_>>();

        let removed_documents = removals
            .iter()
            .filter_map(|key| self.documents.get(*key))
            .collect::<Vec<_>>();
        let final_documents = self
            .documents
            .len()
            .checked_sub(removed_documents.len())
            .and_then(|count| count.checked_add(additions.len()))
            .ok_or(SearchError::Capacity)?;
        if final_documents > self.limits.max_documents {
            return Err(SearchError::Capacity);
        }

        let removed_text_bytes = removed_documents
            .iter()
            .try_fold(0usize, |total, document| {
                total.checked_add(document.stored_text_bytes)
            })
            .ok_or(SearchError::Capacity)?;
        let added_text_bytes = additions.iter().try_fold(0usize, |total, document| {
            total.checked_add(document.stored_text_bytes)
        });
        let final_text_bytes = self
            .indexed_text_bytes
            .checked_sub(removed_text_bytes)
            .and_then(|total| total.checked_add(added_text_bytes?))
            .ok_or(SearchError::Capacity)?;
        if final_text_bytes > self.limits.max_indexed_text_bytes {
            return Err(SearchError::Capacity);
        }

        let removed_postings = removed_documents
            .iter()
            .try_fold(0usize, |total, document| {
                total.checked_add(document.terms.len())
            })
            .ok_or(SearchError::Capacity)?;
        let added_postings = additions.iter().try_fold(0usize, |total, document| {
            total.checked_add(document.terms.len())
        });
        let final_postings = self
            .posting_count
            .checked_sub(removed_postings)
            .and_then(|total| total.checked_add(added_postings?))
            .ok_or(SearchError::Capacity)?;
        if final_postings > self.limits.max_postings {
            return Err(SearchError::Capacity);
        }

        let mut term_deltas = BTreeMap::<&str, isize>::new();
        for document in &removed_documents {
            for term in &document.terms {
                *term_deltas.entry(term.as_str()).or_default() -= 1;
            }
        }
        for document in &additions {
            for term in &document.terms {
                *term_deltas.entry(term.as_str()).or_default() += 1;
            }
        }
        let mut final_unique_terms =
            isize::try_from(self.postings.len()).map_err(|_| SearchError::Capacity)?;
        for (term, delta) in term_deltas {
            let current = self.postings.get(term).map_or(0isize, |keys| {
                isize::try_from(keys.len()).unwrap_or(isize::MAX)
            });
            let final_count = current.checked_add(delta).ok_or(SearchError::Capacity)?;
            if final_count < 0 {
                return Err(SearchError::Capacity);
            }
            match (current == 0, final_count == 0) {
                (true, false) => final_unique_terms += 1,
                (false, true) => final_unique_terms -= 1,
                _ => {}
            }
        }
        if usize::try_from(final_unique_terms).map_err(|_| SearchError::Capacity)?
            > self.limits.max_unique_terms
        {
            return Err(SearchError::Capacity);
        }

        let mut session_deltas = BTreeMap::<&str, isize>::new();
        for document in &removed_documents {
            *session_deltas
                .entry(document.public.session_id.as_str())
                .or_default() -= 1;
        }
        for document in &additions {
            *session_deltas
                .entry(document.public.session_id.as_str())
                .or_default() += 1;
        }
        for (session_id, delta) in session_deltas {
            let current = isize::try_from(
                self.session_counts
                    .get(session_id)
                    .copied()
                    .unwrap_or_default(),
            )
            .map_err(|_| SearchError::Capacity)?;
            let final_count = current.checked_add(delta).ok_or(SearchError::Capacity)?;
            if final_count < 0
                || usize::try_from(final_count).map_err(|_| SearchError::Capacity)?
                    > self.limits.max_documents_per_session
            {
                return Err(SearchError::Capacity);
            }
        }
        Ok(())
    }

    fn insert_unchecked(&mut self, key: DocumentKey, document: IndexedDocument) {
        debug_assert!(!self.documents.contains_key(&key));
        self.indexed_text_bytes = self
            .indexed_text_bytes
            .saturating_add(document.stored_text_bytes);
        self.posting_count = self.posting_count.saturating_add(document.terms.len());
        *self
            .session_counts
            .entry(document.public.session_id.clone())
            .or_default() += 1;
        for term in &document.terms {
            self.postings
                .entry(term.clone())
                .or_default()
                .insert(key.clone());
        }
        self.documents.insert(key, document);
    }

    fn remove_unchecked(&mut self, key: &DocumentKey) -> Option<IndexedDocument> {
        let document = self.documents.remove(key)?;
        self.indexed_text_bytes = self
            .indexed_text_bytes
            .saturating_sub(document.stored_text_bytes);
        self.posting_count = self.posting_count.saturating_sub(document.terms.len());

        let remove_session =
            if let Some(count) = self.session_counts.get_mut(&document.public.session_id) {
                *count = count.saturating_sub(1);
                *count == 0
            } else {
                false
            };
        if remove_session {
            self.session_counts.remove(&document.public.session_id);
        }

        for term in &document.terms {
            let remove_term = if let Some(keys) = self.postings.get_mut(term) {
                keys.remove(key);
                keys.is_empty()
            } else {
                false
            };
            if remove_term {
                self.postings.remove(term);
            }
        }
        Some(document)
    }
}

fn validate_limits(limits: TranscriptSearchLimits) -> Result<(), SearchError> {
    let valid = limits.max_documents > 0
        && limits.max_documents <= MAX_SEARCH_DOCUMENTS
        && limits.max_documents_per_session > 0
        && limits.max_documents_per_session <= limits.max_documents
        && limits.max_documents_per_session <= MAX_SEARCH_DOCUMENTS_PER_SESSION
        && limits.max_indexed_text_bytes > 0
        && limits.max_indexed_text_bytes <= MAX_SEARCH_INDEXED_TEXT_BYTES
        && limits.max_unique_terms > 0
        && limits.max_unique_terms <= MAX_SEARCH_UNIQUE_TERMS
        && limits.max_postings > 0
        && limits.max_postings <= MAX_SEARCH_POSTINGS;
    if valid {
        Ok(())
    } else {
        Err(SearchError::InvalidLimits)
    }
}

fn validate_identity(value: &str) -> Result<(), SearchError> {
    if value.is_empty() || value.len() > MAX_IDENTITY_BYTES {
        return Err(SearchError::InvalidText);
    }
    if value
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')))
    {
        return Err(SearchError::InvalidText);
    }
    Ok(())
}

fn validate_public_text(value: &str, max_bytes: usize) -> Result<(), SearchError> {
    if value.len() > max_bytes {
        return Err(SearchError::TooLarge);
    }
    if value.chars().any(is_forbidden_public_character) {
        return Err(SearchError::InvalidText);
    }
    Ok(())
}

fn is_forbidden_public_character(character: char) -> bool {
    matches!(
        character,
        '\0'..='\u{0008}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000e}'..='\u{001f}'
            | '\u{007f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn terms_for_document(document: &SearchDocument) -> Result<BTreeSet<String>, SearchError> {
    let (text_occurrences, text_overlong) = token_occurrences(&document.text);
    let (title_occurrences, title_overlong) = token_occurrences(&document.session_title);
    if text_overlong || title_overlong {
        return Err(SearchError::TooLarge);
    }
    let terms = text_occurrences
        .into_iter()
        .chain(title_occurrences)
        .map(|occurrence| occurrence.term)
        .collect::<BTreeSet<_>>();
    if terms.len() > MAX_SEARCH_TERMS_PER_DOCUMENT {
        return Err(SearchError::TooLarge);
    }
    Ok(terms)
}

fn query_terms(query: &str) -> Result<Vec<String>, SearchError> {
    if query.chars().count() > MAX_SEARCH_QUERY_CHARS {
        return Err(SearchError::TooLarge);
    }
    validate_public_text(query, MAX_SEARCH_QUERY_CHARS.saturating_mul(4))?;
    let (occurrences, overlong) = token_occurrences(query);
    if overlong {
        return Err(SearchError::TooLarge);
    }
    let terms = occurrences
        .into_iter()
        .map(|occurrence| occurrence.term)
        .collect::<BTreeSet<_>>();
    if terms.is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    if terms.len() > MAX_SEARCH_QUERY_TERMS {
        return Err(SearchError::TooLarge);
    }
    Ok(terms.into_iter().collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TokenOccurrence {
    term: String,
    start_char: usize,
    end_char: usize,
}

fn token_occurrences(value: &str) -> (Vec<TokenOccurrence>, bool) {
    let mut occurrences = Vec::new();
    let mut term = String::new();
    let mut start_char = None;
    let mut char_count = 0usize;
    let mut overlong = false;

    for character in value.chars() {
        if character.is_alphanumeric() || character == '_' {
            start_char.get_or_insert(char_count);
            for folded in character.to_lowercase() {
                term.push(folded);
            }
        } else if let Some(start) = start_char.take() {
            if term.chars().count() <= MAX_SEARCH_TERM_CHARS {
                occurrences.push(TokenOccurrence {
                    term: std::mem::take(&mut term),
                    start_char: start,
                    end_char: char_count,
                });
            } else {
                term.clear();
                overlong = true;
            }
        }
        char_count = char_count.saturating_add(1);
    }
    if let Some(start) = start_char {
        if term.chars().count() <= MAX_SEARCH_TERM_CHARS {
            occurrences.push(TokenOccurrence {
                term,
                start_char: start,
                end_char: char_count,
            });
        } else {
            overlong = true;
        }
    }
    (occurrences, overlong)
}

fn hit_for(document: &SearchDocument, terms: &[String]) -> SearchHit {
    let query_terms = terms.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let text_occurrences = token_occurrences(&document.text)
        .0
        .into_iter()
        .filter(|occurrence| query_terms.contains(occurrence.term.as_str()))
        .collect::<Vec<_>>();
    let title_occurrences = token_occurrences(&document.session_title)
        .0
        .into_iter()
        .filter(|occurrence| query_terms.contains(occurrence.term.as_str()))
        .collect::<Vec<_>>();

    let source = document.text.chars().collect::<Vec<_>>();
    let first_match = text_occurrences
        .iter()
        .map(|occurrence| occurrence.start_char)
        .min()
        .unwrap_or_default();
    let half = MAX_SEARCH_SNIPPET_CHARS / 2;
    let desired_start = first_match.saturating_sub(half);
    let end = source
        .len()
        .min(desired_start.saturating_add(MAX_SEARCH_SNIPPET_CHARS));
    let start = end
        .saturating_sub(MAX_SEARCH_SNIPPET_CHARS)
        .min(desired_start);
    let snippet = source[start..end].iter().collect::<String>();

    let mut match_ranges = text_occurrences
        .iter()
        .filter(|occurrence| occurrence.start_char >= start && occurrence.end_char <= end)
        .map(|occurrence| SearchMatchRange {
            start_char: occurrence.start_char - start,
            end_char: occurrence.end_char - start,
        })
        .collect::<Vec<_>>();
    match_ranges.sort_by_key(|range| (range.start_char, range.end_char));
    match_ranges.dedup();

    let mut title_match_ranges = title_occurrences
        .iter()
        .map(|occurrence| SearchMatchRange {
            start_char: occurrence.start_char,
            end_char: occurrence.end_char,
        })
        .collect::<Vec<_>>();
    title_match_ranges.sort_by_key(|range| (range.start_char, range.end_char));
    title_match_ranges.dedup();

    let score = u32::try_from(text_occurrences.len())
        .unwrap_or(u32::MAX)
        .saturating_mul(10)
        .saturating_add(u32::try_from(title_occurrences.len()).unwrap_or(u32::MAX));
    SearchHit {
        session_id: document.session_id.clone(),
        item_id: document.item_id.clone(),
        kind: document.kind,
        session_title: document.session_title.clone(),
        snippet,
        match_ranges,
        title_match_ranges,
        timestamp_ms: document.timestamp_ms,
        score,
    }
}
