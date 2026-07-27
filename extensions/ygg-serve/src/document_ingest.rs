//! Immutable, bytes-only ingestion for bounded text, Markdown, and PDF input.
//!
//! Text and Markdown share the same UTF-8 byte grammar, so their exact media
//! distinction is the declared media type plus a matching safe filename
//! extension. PDF input additionally requires a strict header, terminal EOF,
//! classic single-revision cross-reference table, and successful strict parse.
//!
//! PDF extraction is deliberately text-only and partial. Modern object/xref
//! streams and incremental revisions are rejected before parsing because the
//! selected parser eagerly expands those structures without caller-provided
//! decompression bounds. Page-content and ToUnicode streams are instead
//! decompressed here under explicit per-stream, aggregate, and ratio limits.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;

use bytes::Bytes;
use flate2::read::ZlibDecoder;
use lopdf::{Dictionary, Document, LoadOptions, Object, ObjectId, Stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Maximum accepted source bytes for one document.
pub const MAX_DOCUMENT_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum UTF-8 bytes exposed to a model after extraction.
pub const MAX_DOCUMENT_TEXT_BYTES: usize = 1024 * 1024;
/// Maximum pages accepted from one PDF.
pub const MAX_PDF_PAGES: usize = 200;
/// Maximum indirect objects accepted from one PDF.
pub const MAX_PDF_OBJECTS: usize = 20_000;
/// Maximum decoded bytes accepted from one extraction-relevant PDF stream.
pub const MAX_PDF_STREAM_DECOMPRESSED_BYTES: usize = 2 * 1024 * 1024;
/// Maximum aggregate decoded bytes accepted for PDF text extraction.
pub const MAX_PDF_TOTAL_DECOMPRESSED_BYTES: usize = 8 * 1024 * 1024;
/// Maximum direct array/dictionary nesting accepted before PDF parsing.
pub const MAX_PDF_NESTING_DEPTH: usize = 64;

const MAX_DISPLAY_NAME_BYTES: usize = 255;
const MAX_PDF_OBJECT_NODES: usize = 100_000;
const MAX_PDF_SYNTAX_NODES: usize = 250_000;
const MAX_PDF_STREAMS: usize = 4_096;
const MAX_PDF_TOUNICODE_BYTES: usize = 512 * 1024;
const MAX_PDF_COMPRESSION_RATIO: usize = 200;
const PDF_COMPRESSION_RATIO_SLOP: usize = 4 * 1024;
const MAX_PDF_NAME_BYTES: usize = 256;
const PDF_EXTRACTION_NOTICE: &str =
    "[PDF text extraction is partial: visual layout, images, annotations, and exact glyph positioning are not preserved.]";

/// Accepted authoritative media type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentMediaType {
    /// Exact UTF-8 plain text.
    #[serde(rename = "text/plain")]
    PlainText,
    /// Exact UTF-8 Markdown source.
    #[serde(rename = "text/markdown")]
    Markdown,
    /// Portable Document Format input.
    #[serde(rename = "application/pdf")]
    Pdf,
}

impl DocumentMediaType {
    /// Parses one exact, parameter-free media type.
    pub fn parse(value: &str) -> Result<Self, DocumentIngestError> {
        match value {
            "text/plain" => Ok(Self::PlainText),
            "text/markdown" => Ok(Self::Markdown),
            "application/pdf" => Ok(Self::Pdf),
            _ => Err(DocumentIngestError::UnsupportedMediaType),
        }
    }

    /// Returns the canonical media-type spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlainText => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Pdf => "application/pdf",
        }
    }
}

/// Fidelity contract for model-facing text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExtractionFidelity {
    /// The model text is the exact validated UTF-8 source text.
    ExactUtf8,
    /// PDF text was reconstructed in page order without layout fidelity.
    PdfTextOnlyPartial,
}

/// Path-free immutable source and extraction provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentProvenance {
    /// Safe display basename.
    pub display_name: String,
    /// Authoritatively validated source media type.
    pub media_type: DocumentMediaType,
    /// Exact source byte count.
    pub source_byte_count: u64,
    /// Lowercase SHA-256 of the exact source bytes.
    pub sha256: String,
    /// PDF page count, absent for text and Markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    /// Parsed PDF indirect-object count, absent for text and Markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_count: Option<u32>,
    /// Exact byte count of the model-facing UTF-8 text.
    pub extracted_text_byte_count: u64,
    /// Explicit extraction fidelity.
    pub fidelity: ExtractionFidelity,
}

/// Immutable validated document input.
#[derive(Clone, PartialEq, Eq)]
pub struct IngestedDocument {
    source_bytes: Bytes,
    model_text: String,
    provenance: DocumentProvenance,
}

impl fmt::Debug for IngestedDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IngestedDocument")
            .field("source_bytes", &"<redacted>")
            .field("model_text", &"<redacted>")
            .field("provenance", &self.provenance)
            .finish()
    }
}

impl IngestedDocument {
    /// Returns the exact immutable source bytes.
    pub fn source_bytes(&self) -> &Bytes {
        &self.source_bytes
    }

    /// Returns bounded UTF-8 text suitable for model context.
    pub fn model_text(&self) -> &str {
        &self.model_text
    }

    /// Returns path-free source and extraction provenance.
    pub fn provenance(&self) -> &DocumentProvenance {
        &self.provenance
    }
}

/// Bounded document validation or extraction failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum DocumentIngestError {
    /// No source bytes were supplied.
    #[error("the document is empty")]
    Empty,
    /// The source or extracted output exceeded a byte limit.
    #[error("the document exceeds a size limit")]
    TooLarge,
    /// The display basename was missing, path-like, deceptive, or oversized.
    #[error("the document display name is invalid")]
    InvalidDisplayName,
    /// The declared media type is not accepted exactly.
    #[error("the document media type is unsupported")]
    UnsupportedMediaType,
    /// The display extension, magic, and declared type disagree.
    #[error("the document media type does not match its content")]
    MediaTypeMismatch,
    /// Text source bytes were not exact UTF-8.
    #[error("the text document is not valid UTF-8")]
    InvalidUtf8,
    /// Text contained binary control characters.
    #[error("the text document contains binary controls")]
    BinaryText,
    /// PDF framing, cross-reference data, objects, or content were malformed.
    #[error("the PDF is malformed or truncated")]
    MalformedPdf,
    /// Encrypted and password-protected PDFs are deliberately unsupported.
    #[error("encrypted PDFs are not supported")]
    EncryptedPdf,
    /// Embedded files or file-attachment structures were detected.
    #[error("PDF embedded files are not supported")]
    EmbeddedFile,
    /// The PDF requires an eagerly expanded or incremental structure.
    #[error("the PDF structure is unsupported for bounded ingestion")]
    UnsupportedPdfStructure,
    /// A stream uses a filter that this bounded decoder does not support.
    #[error("the PDF stream filter is unsupported")]
    UnsupportedPdfFilter,
    /// PDF page count exceeded the conservative limit.
    #[error("the PDF has too many pages")]
    PdfPageLimit,
    /// PDF object count, nesting, or collection size exceeded a limit.
    #[error("the PDF has too many objects")]
    PdfObjectLimit,
    /// A PDF stream exceeded decompression or compression-ratio limits.
    #[error("the PDF exceeds a decompression limit")]
    PdfDecompressionLimit,
    /// PDF parsing succeeded but yielded no usable text.
    #[error("the PDF contains no extractable text")]
    NoExtractableText,
}

/// Validates and immutably ingests one fully buffered document.
pub fn ingest_document(
    display_name: &str,
    declared_media_type: &str,
    source_bytes: Bytes,
) -> Result<IngestedDocument, DocumentIngestError> {
    if source_bytes.is_empty() {
        return Err(DocumentIngestError::Empty);
    }
    if source_bytes.len() > MAX_DOCUMENT_FILE_BYTES {
        return Err(DocumentIngestError::TooLarge);
    }

    let media_type = DocumentMediaType::parse(declared_media_type)?;
    let display_name = safe_display_name(display_name, media_type)?;
    let sha256 = sha256_hex(&source_bytes);
    match media_type {
        DocumentMediaType::PlainText | DocumentMediaType::Markdown => {
            ingest_utf8(display_name, media_type, source_bytes, sha256)
        }
        DocumentMediaType::Pdf => ingest_pdf(display_name, source_bytes, sha256),
    }
}

fn ingest_utf8(
    display_name: String,
    media_type: DocumentMediaType,
    source_bytes: Bytes,
    sha256: String,
) -> Result<IngestedDocument, DocumentIngestError> {
    if has_pdf_magic(&source_bytes) {
        return Err(DocumentIngestError::MediaTypeMismatch);
    }
    let text = std::str::from_utf8(&source_bytes).map_err(|_| DocumentIngestError::InvalidUtf8)?;
    if text.chars().any(is_forbidden_text_control) {
        return Err(DocumentIngestError::BinaryText);
    }
    if text.len() > MAX_DOCUMENT_TEXT_BYTES {
        return Err(DocumentIngestError::TooLarge);
    }
    let model_text = text.to_owned();
    let provenance = DocumentProvenance {
        display_name,
        media_type,
        source_byte_count: source_bytes.len() as u64,
        sha256,
        page_count: None,
        object_count: None,
        extracted_text_byte_count: model_text.len() as u64,
        fidelity: ExtractionFidelity::ExactUtf8,
    };
    Ok(IngestedDocument {
        source_bytes,
        model_text,
        provenance,
    })
}

fn ingest_pdf(
    display_name: String,
    source_bytes: Bytes,
    sha256: String,
) -> Result<IngestedDocument, DocumentIngestError> {
    let envelope = preflight_pdf_envelope(&source_bytes)?;
    let facts = scan_pdf_names(
        &source_bytes,
        envelope.trailer_start,
        envelope.startxref_start,
    )?;
    if facts.encrypted {
        return Err(DocumentIngestError::EncryptedPdf);
    }
    if facts.object_stream || facts.incremental_or_hybrid {
        return Err(DocumentIngestError::UnsupportedPdfStructure);
    }
    let Some(declared_size) = facts.declared_size else {
        return Err(DocumentIngestError::MalformedPdf);
    };
    if declared_size > MAX_PDF_OBJECTS.saturating_add(1) {
        return Err(DocumentIngestError::PdfObjectLimit);
    }

    let options = LoadOptions {
        password: None,
        filter: None,
        strict: true,
    };
    let mut document = Document::load_mem_with_options(&source_bytes, options)
        .map_err(|_| DocumentIngestError::MalformedPdf)?;
    if document.is_encrypted() || document.was_encrypted() {
        return Err(DocumentIngestError::EncryptedPdf);
    }
    if document.objects.len() > MAX_PDF_OBJECTS {
        return Err(DocumentIngestError::PdfObjectLimit);
    }
    document
        .catalog()
        .map_err(|_| DocumentIngestError::MalformedPdf)?;

    let inspection = inspect_pdf_objects(&document)?;
    let pages = document.get_pages();
    if pages.is_empty() {
        return Err(DocumentIngestError::MalformedPdf);
    }
    if pages.len() > MAX_PDF_PAGES {
        return Err(DocumentIngestError::PdfPageLimit);
    }

    prepare_extraction_streams(&mut document, &pages, &inspection.to_unicode_references)?;
    let model_text = extract_pdf_text(&document, pages.len())?;
    let provenance = DocumentProvenance {
        display_name,
        media_type: DocumentMediaType::Pdf,
        source_byte_count: source_bytes.len() as u64,
        sha256,
        page_count: Some(pages.len() as u32),
        object_count: Some(document.objects.len() as u32),
        extracted_text_byte_count: model_text.len() as u64,
        fidelity: ExtractionFidelity::PdfTextOnlyPartial,
    };
    Ok(IngestedDocument {
        source_bytes,
        model_text,
        provenance,
    })
}

fn safe_display_name(
    value: &str,
    media_type: DocumentMediaType,
) -> Result<String, DocumentIngestError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_DISPLAY_NAME_BYTES
        || matches!(value, "." | "..")
        || value.chars().any(is_unsafe_display_character)
    {
        return Err(DocumentIngestError::InvalidDisplayName);
    }
    let lowercase = value.to_ascii_lowercase();
    let extension_matches = match media_type {
        DocumentMediaType::PlainText => {
            lowercase.ends_with(".txt")
                || lowercase.ends_with(".text")
                || lowercase.ends_with(".log")
        }
        DocumentMediaType::Markdown => {
            lowercase.ends_with(".md") || lowercase.ends_with(".markdown")
        }
        DocumentMediaType::Pdf => lowercase.ends_with(".pdf"),
    };
    if !extension_matches {
        return Err(DocumentIngestError::MediaTypeMismatch);
    }
    Ok(value.to_owned())
}

fn is_unsafe_display_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '/' | '\\'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

fn is_forbidden_text_control(character: char) -> bool {
    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
}

fn has_pdf_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

#[derive(Clone, Copy, Debug)]
struct PdfEnvelope {
    trailer_start: usize,
    startxref_start: usize,
}

fn preflight_pdf_envelope(bytes: &[u8]) -> Result<PdfEnvelope, DocumentIngestError> {
    if !valid_pdf_header(bytes) {
        return Err(DocumentIngestError::MediaTypeMismatch);
    }
    let trimmed_end = trim_pdf_whitespace_end(bytes);
    if trimmed_end < b"%%EOF".len() || &bytes[trimmed_end - b"%%EOF".len()..trimmed_end] != b"%%EOF"
    {
        return Err(DocumentIngestError::MalformedPdf);
    }
    let eof_start = trimmed_end - b"%%EOF".len();
    let startxref_start =
        rfind_bytes(&bytes[..eof_start], b"startxref").ok_or(DocumentIngestError::MalformedPdf)?;
    let mut cursor = startxref_start + b"startxref".len();
    skip_pdf_whitespace_and_comments(bytes, &mut cursor, eof_start);
    let xref_offset = parse_ascii_usize(bytes, &mut cursor, eof_start)
        .ok_or(DocumentIngestError::MalformedPdf)?;
    skip_pdf_whitespace_and_comments(bytes, &mut cursor, eof_start);
    if cursor != eof_start || xref_offset >= bytes.len() {
        return Err(DocumentIngestError::MalformedPdf);
    }
    let trailer_start = preflight_classic_xref(bytes, xref_offset)?;
    Ok(PdfEnvelope {
        trailer_start,
        startxref_start,
    })
}

fn valid_pdf_header(bytes: &[u8]) -> bool {
    bytes.len() >= 8
        && bytes.starts_with(b"%PDF-")
        && matches!(
            &bytes[5..8],
            b"1.0" | b"1.1" | b"1.2" | b"1.3" | b"1.4" | b"1.5" | b"1.6" | b"1.7" | b"2.0"
        )
        && bytes
            .get(8)
            .is_none_or(|byte| is_pdf_whitespace(*byte) || *byte == b'%')
}

fn preflight_classic_xref(bytes: &[u8], offset: usize) -> Result<usize, DocumentIngestError> {
    let mut cursor = PdfTokenCursor::new(bytes, offset);
    if cursor.next()? != Some(&b"xref"[..]) {
        return Err(DocumentIngestError::UnsupportedPdfStructure);
    }
    let mut entry_count = 0usize;
    loop {
        let token = cursor.next()?.ok_or(DocumentIngestError::MalformedPdf)?;
        if token == b"trailer" {
            if entry_count == 0 {
                return Err(DocumentIngestError::MalformedPdf);
            }
            return Ok(cursor.position());
        }
        let _first_object = parse_decimal_token(token).ok_or(DocumentIngestError::MalformedPdf)?;
        let count = cursor
            .next()?
            .and_then(parse_decimal_token)
            .ok_or(DocumentIngestError::MalformedPdf)?;
        entry_count = entry_count
            .checked_add(count)
            .ok_or(DocumentIngestError::PdfObjectLimit)?;
        if entry_count > MAX_PDF_OBJECTS.saturating_add(1) {
            return Err(DocumentIngestError::PdfObjectLimit);
        }
        for _ in 0..count {
            let offset_token = cursor.next()?.ok_or(DocumentIngestError::MalformedPdf)?;
            let generation_token = cursor.next()?.ok_or(DocumentIngestError::MalformedPdf)?;
            let status = cursor.next()?.ok_or(DocumentIngestError::MalformedPdf)?;
            if parse_decimal_token(offset_token).is_none()
                || parse_decimal_token(generation_token).is_none()
                || !matches!(status, b"n" | b"f")
            {
                return Err(DocumentIngestError::MalformedPdf);
            }
        }
    }
}

struct PdfTokenCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> PdfTokenCursor<'a> {
    fn new(bytes: &'a [u8], position: usize) -> Self {
        Self { bytes, position }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn next(&mut self) -> Result<Option<&'a [u8]>, DocumentIngestError> {
        skip_pdf_whitespace_and_comments(self.bytes, &mut self.position, self.bytes.len());
        if self.position >= self.bytes.len() {
            return Ok(None);
        }
        let start = self.position;
        if is_pdf_delimiter(self.bytes[start]) {
            self.position += 1;
        } else {
            while self.position < self.bytes.len()
                && !is_pdf_whitespace(self.bytes[self.position])
                && !is_pdf_delimiter(self.bytes[self.position])
            {
                self.position += 1;
            }
        }
        if start == self.position {
            return Err(DocumentIngestError::MalformedPdf);
        }
        Ok(Some(&self.bytes[start..self.position]))
    }
}

#[derive(Default)]
struct RawPdfFacts {
    encrypted: bool,
    object_stream: bool,
    incremental_or_hybrid: bool,
    declared_size: Option<usize>,
}

fn scan_pdf_names(
    bytes: &[u8],
    trailer_start: usize,
    trailer_end: usize,
) -> Result<RawPdfFacts, DocumentIngestError> {
    let mut facts = RawPdfFacts::default();
    let mut cursor = 0usize;
    let mut syntax_nodes = 0usize;
    let mut structures = Vec::new();
    while cursor < bytes.len() {
        if pdf_keyword_at(bytes, cursor, b"stream") {
            increment_pdf_syntax_node(&mut syntax_nodes)?;
            skip_pdf_stream(bytes, &mut cursor)?;
            continue;
        }
        match bytes[cursor] {
            b'%' => skip_pdf_comment(bytes, &mut cursor, bytes.len()),
            b'(' => {
                increment_pdf_syntax_node(&mut syntax_nodes)?;
                skip_pdf_literal_string(bytes, &mut cursor)?;
            }
            b'<' if bytes.get(cursor + 1) == Some(&b'<') => {
                increment_pdf_syntax_node(&mut syntax_nodes)?;
                structures.push(b'<');
                if structures.len() > MAX_PDF_NESTING_DEPTH {
                    return Err(DocumentIngestError::PdfObjectLimit);
                }
                cursor += 2;
            }
            b'<' if bytes.get(cursor + 1) != Some(&b'<') => {
                increment_pdf_syntax_node(&mut syntax_nodes)?;
                skip_pdf_hex_string(bytes, &mut cursor)?;
            }
            b'>' if bytes.get(cursor + 1) == Some(&b'>') => {
                if structures.pop() != Some(b'<') {
                    return Err(DocumentIngestError::MalformedPdf);
                }
                cursor += 2;
            }
            b'[' => {
                increment_pdf_syntax_node(&mut syntax_nodes)?;
                structures.push(b'[');
                if structures.len() > MAX_PDF_NESTING_DEPTH {
                    return Err(DocumentIngestError::PdfObjectLimit);
                }
                cursor += 1;
            }
            b']' => {
                if structures.pop() != Some(b'[') {
                    return Err(DocumentIngestError::MalformedPdf);
                }
                cursor += 1;
            }
            b'/' => {
                increment_pdf_syntax_node(&mut syntax_nodes)?;
                let name_start = cursor;
                let name = read_pdf_name(bytes, &mut cursor)?;
                match name.as_slice() {
                    b"Encrypt" => facts.encrypted = true,
                    b"ObjStm" => facts.object_stream = true,
                    b"Prev" | b"XRefStm" if (trailer_start..trailer_end).contains(&name_start) => {
                        facts.incremental_or_hybrid = true;
                    }
                    b"Size" if (trailer_start..trailer_end).contains(&name_start) => {
                        let mut value_cursor = cursor;
                        skip_pdf_whitespace_and_comments(bytes, &mut value_cursor, trailer_end);
                        facts.declared_size =
                            parse_ascii_usize(bytes, &mut value_cursor, trailer_end);
                    }
                    _ => {}
                }
            }
            byte if is_pdf_whitespace(byte) => cursor += 1,
            b')' | b'>' | b'{' | b'}' => return Err(DocumentIngestError::MalformedPdf),
            byte if is_pdf_delimiter(byte) => {
                increment_pdf_syntax_node(&mut syntax_nodes)?;
                cursor += 1;
            }
            _ => {
                increment_pdf_syntax_node(&mut syntax_nodes)?;
                while cursor < bytes.len()
                    && !is_pdf_whitespace(bytes[cursor])
                    && !is_pdf_delimiter(bytes[cursor])
                {
                    cursor += 1;
                }
            }
        }
    }
    if !structures.is_empty() {
        return Err(DocumentIngestError::MalformedPdf);
    }
    Ok(facts)
}

fn increment_pdf_syntax_node(nodes: &mut usize) -> Result<(), DocumentIngestError> {
    *nodes = nodes
        .checked_add(1)
        .ok_or(DocumentIngestError::PdfObjectLimit)?;
    if *nodes > MAX_PDF_SYNTAX_NODES {
        return Err(DocumentIngestError::PdfObjectLimit);
    }
    Ok(())
}

fn skip_pdf_stream(bytes: &[u8], cursor: &mut usize) -> Result<(), DocumentIngestError> {
    *cursor += b"stream".len();
    match bytes.get(*cursor) {
        Some(b'\n') => *cursor += 1,
        Some(b'\r') => {
            *cursor += 1;
            if bytes.get(*cursor) == Some(&b'\n') {
                *cursor += 1;
            }
        }
        _ => return Err(DocumentIngestError::MalformedPdf),
    }
    while *cursor < bytes.len() {
        if pdf_keyword_at(bytes, *cursor, b"endstream") {
            *cursor += b"endstream".len();
            return Ok(());
        }
        *cursor += 1;
    }
    Err(DocumentIngestError::MalformedPdf)
}

fn pdf_keyword_at(bytes: &[u8], position: usize, keyword: &[u8]) -> bool {
    bytes
        .get(position..position.saturating_add(keyword.len()))
        .is_some_and(|candidate| candidate == keyword)
        && position
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .is_none_or(|byte| is_pdf_whitespace(*byte) || is_pdf_delimiter(*byte))
        && bytes
            .get(position.saturating_add(keyword.len()))
            .is_none_or(|byte| is_pdf_whitespace(*byte) || is_pdf_delimiter(*byte))
}

fn read_pdf_name(bytes: &[u8], cursor: &mut usize) -> Result<Vec<u8>, DocumentIngestError> {
    *cursor += 1;
    let mut name = Vec::new();
    while *cursor < bytes.len()
        && !is_pdf_whitespace(bytes[*cursor])
        && !is_pdf_delimiter(bytes[*cursor])
    {
        if name.len() >= MAX_PDF_NAME_BYTES {
            return Err(DocumentIngestError::PdfObjectLimit);
        }
        if bytes[*cursor] == b'#' {
            let high = bytes
                .get(*cursor + 1)
                .and_then(|byte| hex_value(*byte))
                .ok_or(DocumentIngestError::MalformedPdf)?;
            let low = bytes
                .get(*cursor + 2)
                .and_then(|byte| hex_value(*byte))
                .ok_or(DocumentIngestError::MalformedPdf)?;
            name.push((high << 4) | low);
            *cursor += 3;
        } else {
            name.push(bytes[*cursor]);
            *cursor += 1;
        }
    }
    if name.is_empty() {
        return Err(DocumentIngestError::MalformedPdf);
    }
    Ok(name)
}

fn skip_pdf_literal_string(bytes: &[u8], cursor: &mut usize) -> Result<(), DocumentIngestError> {
    *cursor += 1;
    let mut depth = 1usize;
    while *cursor < bytes.len() {
        match bytes[*cursor] {
            b'\\' => {
                *cursor += 1;
                if *cursor < bytes.len() {
                    *cursor += 1;
                }
            }
            b'(' => {
                depth = depth
                    .checked_add(1)
                    .ok_or(DocumentIngestError::PdfObjectLimit)?;
                if depth > MAX_PDF_NESTING_DEPTH {
                    return Err(DocumentIngestError::PdfObjectLimit);
                }
                *cursor += 1;
            }
            b')' => {
                depth -= 1;
                *cursor += 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            _ => *cursor += 1,
        }
    }
    Err(DocumentIngestError::MalformedPdf)
}

fn skip_pdf_hex_string(bytes: &[u8], cursor: &mut usize) -> Result<(), DocumentIngestError> {
    *cursor += 1;
    while *cursor < bytes.len() {
        if bytes[*cursor] == b'>' {
            *cursor += 1;
            return Ok(());
        }
        *cursor += 1;
    }
    Err(DocumentIngestError::MalformedPdf)
}

fn inspect_pdf_objects(document: &Document) -> Result<PdfInspection, DocumentIngestError> {
    let mut inspection = PdfInspection::default();
    for object in document.objects.values() {
        inspect_object(object, 0, &mut inspection)?;
    }
    Ok(inspection)
}

#[derive(Default)]
struct PdfInspection {
    nodes: usize,
    streams: usize,
    raw_stream_bytes: usize,
    to_unicode_references: BTreeSet<ObjectId>,
}

fn inspect_object(
    object: &Object,
    depth: usize,
    inspection: &mut PdfInspection,
) -> Result<(), DocumentIngestError> {
    if depth > MAX_PDF_NESTING_DEPTH {
        return Err(DocumentIngestError::PdfObjectLimit);
    }
    inspection.nodes = inspection
        .nodes
        .checked_add(1)
        .ok_or(DocumentIngestError::PdfObjectLimit)?;
    if inspection.nodes > MAX_PDF_OBJECT_NODES {
        return Err(DocumentIngestError::PdfObjectLimit);
    }
    match object {
        Object::Array(values) => {
            for value in values {
                inspect_object(value, depth + 1, inspection)?;
            }
        }
        Object::Dictionary(dictionary) => {
            inspect_dictionary(dictionary, depth + 1, inspection)?;
        }
        Object::Stream(stream) => {
            inspection.streams = inspection
                .streams
                .checked_add(1)
                .ok_or(DocumentIngestError::PdfObjectLimit)?;
            if inspection.streams > MAX_PDF_STREAMS {
                return Err(DocumentIngestError::PdfObjectLimit);
            }
            inspection.raw_stream_bytes = inspection
                .raw_stream_bytes
                .checked_add(stream.content.len())
                .ok_or(DocumentIngestError::PdfObjectLimit)?;
            if inspection.raw_stream_bytes > MAX_DOCUMENT_FILE_BYTES {
                return Err(DocumentIngestError::PdfObjectLimit);
            }
            inspect_dictionary(&stream.dict, depth + 1, inspection)?;
        }
        Object::Null
        | Object::Boolean(_)
        | Object::Integer(_)
        | Object::Real(_)
        | Object::Name(_)
        | Object::String(_, _)
        | Object::Reference(_) => {}
    }
    Ok(())
}

fn inspect_dictionary(
    dictionary: &Dictionary,
    depth: usize,
    inspection: &mut PdfInspection,
) -> Result<(), DocumentIngestError> {
    if dictionary_is_embedded_file(dictionary) {
        return Err(DocumentIngestError::EmbeddedFile);
    }
    if dictionary_is_active_content(dictionary) {
        return Err(DocumentIngestError::UnsupportedPdfStructure);
    }
    if let Ok(to_unicode) = dictionary.get(b"ToUnicode") {
        match to_unicode {
            Object::Reference(id) => {
                inspection.to_unicode_references.insert(*id);
            }
            _ => return Err(DocumentIngestError::UnsupportedPdfStructure),
        }
    }
    for (_, value) in dictionary.iter() {
        inspect_object(value, depth, inspection)?;
    }
    Ok(())
}

fn dictionary_is_embedded_file(dictionary: &Dictionary) -> bool {
    dictionary.has_type(b"EmbeddedFile")
        || dictionary.has_type(b"Filespec")
        || dictionary.has(b"EF")
        || dictionary.has(b"EmbeddedFiles")
        || dictionary.has(b"AF")
        || dictionary.has(b"Collection")
        || dictionary
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|name| name == b"FileAttachment")
}

fn dictionary_is_active_content(dictionary: &Dictionary) -> bool {
    dictionary
        .get(b"S")
        .and_then(Object::as_name)
        .is_ok_and(|name| {
            matches!(
                name,
                b"JavaScript" | b"Launch" | b"GoToE" | b"SubmitForm" | b"ImportData"
            )
        })
}

fn prepare_extraction_streams(
    document: &mut Document,
    pages: &BTreeMap<u32, ObjectId>,
    to_unicode_references: &BTreeSet<ObjectId>,
) -> Result<(), DocumentIngestError> {
    let mut stream_limits = BTreeMap::<ObjectId, usize>::new();
    for page_id in pages.values() {
        let page = document
            .get_dictionary(*page_id)
            .map_err(|_| DocumentIngestError::MalformedPdf)?;
        if let Ok(contents) = page.get(b"Contents") {
            let mut reference_stack = BTreeSet::new();
            collect_content_streams(
                document,
                contents,
                0,
                &mut reference_stack,
                &mut stream_limits,
            )?;
        }
    }
    for reference in to_unicode_references {
        let id = resolve_stream_reference(document, *reference)?;
        stream_limits
            .entry(id)
            .and_modify(|limit| *limit = (*limit).min(MAX_PDF_TOUNICODE_BYTES))
            .or_insert(MAX_PDF_TOUNICODE_BYTES);
    }

    let mut aggregate = 0usize;
    for (id, per_stream_limit) in stream_limits {
        let decoded = {
            let stream = document
                .get_object(id)
                .and_then(Object::as_stream)
                .map_err(|_| DocumentIngestError::MalformedPdf)?;
            decode_stream_bounded(stream, per_stream_limit)?
        };
        aggregate = aggregate
            .checked_add(decoded.len())
            .ok_or(DocumentIngestError::PdfDecompressionLimit)?;
        if aggregate > MAX_PDF_TOTAL_DECOMPRESSED_BYTES {
            return Err(DocumentIngestError::PdfDecompressionLimit);
        }
        document
            .get_object_mut(id)
            .and_then(Object::as_stream_mut)
            .map_err(|_| DocumentIngestError::MalformedPdf)?
            .set_plain_content(decoded);
    }
    Ok(())
}

fn collect_content_streams(
    document: &Document,
    object: &Object,
    depth: usize,
    reference_stack: &mut BTreeSet<ObjectId>,
    streams: &mut BTreeMap<ObjectId, usize>,
) -> Result<(), DocumentIngestError> {
    if depth > MAX_PDF_NESTING_DEPTH {
        return Err(DocumentIngestError::PdfObjectLimit);
    }
    match object {
        Object::Null => Ok(()),
        Object::Array(values) => {
            for value in values {
                collect_content_streams(document, value, depth + 1, reference_stack, streams)?;
            }
            Ok(())
        }
        Object::Reference(id) => {
            if !reference_stack.insert(*id) {
                return Err(DocumentIngestError::MalformedPdf);
            }
            let target = document
                .get_object(*id)
                .map_err(|_| DocumentIngestError::MalformedPdf)?;
            if matches!(target, Object::Stream(_)) {
                streams
                    .entry(*id)
                    .or_insert(MAX_PDF_STREAM_DECOMPRESSED_BYTES);
            } else {
                collect_content_streams(document, target, depth + 1, reference_stack, streams)?;
            }
            reference_stack.remove(id);
            Ok(())
        }
        Object::Stream(_) => Err(DocumentIngestError::UnsupportedPdfStructure),
        _ => Err(DocumentIngestError::MalformedPdf),
    }
}

fn resolve_stream_reference(
    document: &Document,
    first: ObjectId,
) -> Result<ObjectId, DocumentIngestError> {
    let mut current = first;
    let mut visited = BTreeSet::new();
    for _ in 0..=MAX_PDF_NESTING_DEPTH {
        if !visited.insert(current) {
            return Err(DocumentIngestError::MalformedPdf);
        }
        match document
            .get_object(current)
            .map_err(|_| DocumentIngestError::MalformedPdf)?
        {
            Object::Stream(_) => return Ok(current),
            Object::Reference(next) => current = *next,
            _ => return Err(DocumentIngestError::MalformedPdf),
        }
    }
    Err(DocumentIngestError::PdfObjectLimit)
}

fn decode_stream_bounded(stream: &Stream, limit: usize) -> Result<Vec<u8>, DocumentIngestError> {
    if stream
        .dict
        .get(b"DecodeParms")
        .is_ok_and(|parameters| !matches!(parameters, Object::Null))
    {
        return Err(DocumentIngestError::UnsupportedPdfFilter);
    }
    let filter = match stream.dict.get(b"Filter") {
        Err(_) => {
            if stream.content.len() > limit {
                return Err(DocumentIngestError::PdfDecompressionLimit);
            }
            return Ok(stream.content.clone());
        }
        Ok(Object::Name(name)) if name == b"FlateDecode" => b"FlateDecode".as_slice(),
        Ok(Object::Array(filters))
            if filters.len() == 1
                && filters[0]
                    .as_name()
                    .is_ok_and(|name| name == b"FlateDecode") =>
        {
            b"FlateDecode".as_slice()
        }
        Ok(_) => return Err(DocumentIngestError::UnsupportedPdfFilter),
    };
    debug_assert_eq!(filter, b"FlateDecode");
    let mut decoder = ZlibDecoder::new(stream.content.as_slice());
    let mut decoded = Vec::with_capacity(stream.content.len().saturating_mul(2).min(limit));
    Read::by_ref(&mut decoder)
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| DocumentIngestError::MalformedPdf)?;
    if decoded.len() > limit
        || decoded.len()
            > stream
                .content
                .len()
                .saturating_mul(MAX_PDF_COMPRESSION_RATIO)
                .saturating_add(PDF_COMPRESSION_RATIO_SLOP)
    {
        return Err(DocumentIngestError::PdfDecompressionLimit);
    }
    Ok(decoded)
}

fn extract_pdf_text(document: &Document, page_count: usize) -> Result<String, DocumentIngestError> {
    let mut output = String::new();
    push_bounded(&mut output, PDF_EXTRACTION_NOTICE)?;
    let mut has_text = false;
    for page_number in 1..=page_count {
        let page_text = document
            .extract_text(&[page_number as u32])
            .map_err(|_| DocumentIngestError::MalformedPdf)?;
        if page_text.chars().any(is_forbidden_text_control) {
            return Err(DocumentIngestError::BinaryText);
        }
        let page_text = normalize_pdf_line_endings(&page_text);
        push_bounded(&mut output, "\n\n--- Page ")?;
        push_bounded(&mut output, &page_number.to_string())?;
        push_bounded(&mut output, " ---\n")?;
        let page_text = page_text.trim();
        if !page_text.is_empty() {
            has_text = true;
            push_bounded(&mut output, page_text)?;
        }
    }
    if !has_text {
        return Err(DocumentIngestError::NoExtractableText);
    }
    Ok(output)
}

fn normalize_pdf_line_endings(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn push_bounded(output: &mut String, value: &str) -> Result<(), DocumentIngestError> {
    if output
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > MAX_DOCUMENT_TEXT_BYTES)
    {
        return Err(DocumentIngestError::TooLarge);
    }
    output.push_str(value);
    Ok(())
}

fn parse_ascii_usize(bytes: &[u8], cursor: &mut usize, end: usize) -> Option<usize> {
    let start = *cursor;
    let mut value = 0usize;
    while *cursor < end && bytes[*cursor].is_ascii_digit() {
        value = value
            .checked_mul(10)?
            .checked_add((bytes[*cursor] - b'0') as usize)?;
        *cursor += 1;
    }
    (*cursor > start).then_some(value)
}

fn parse_decimal_token(token: &[u8]) -> Option<usize> {
    let mut cursor = 0usize;
    let value = parse_ascii_usize(token, &mut cursor, token.len())?;
    (cursor == token.len()).then_some(value)
}

fn skip_pdf_whitespace_and_comments(bytes: &[u8], cursor: &mut usize, end: usize) {
    loop {
        while *cursor < end && is_pdf_whitespace(bytes[*cursor]) {
            *cursor += 1;
        }
        if *cursor < end && bytes[*cursor] == b'%' {
            skip_pdf_comment(bytes, cursor, end);
        } else {
            return;
        }
    }
}

fn skip_pdf_comment(bytes: &[u8], cursor: &mut usize, end: usize) {
    while *cursor < end && !matches!(bytes[*cursor], b'\r' | b'\n') {
        *cursor += 1;
    }
}

fn trim_pdf_whitespace_end(bytes: &[u8]) -> usize {
    let mut end = bytes.len();
    while end > 0 && is_pdf_whitespace(bytes[end - 1]) {
        end -= 1;
    }
    end
}

fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, 0x00 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_pdf_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
