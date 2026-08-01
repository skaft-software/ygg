//! Standalone hostile-input coverage for bounded document ingestion.

#[path = "../src/document_ingest.rs"]
mod document_ingest;

use bytes::Bytes;
use document_ingest::{
    ingest_document, DocumentIngestError, DocumentMediaType, ExtractionFidelity,
    MAX_DOCUMENT_FILE_BYTES, MAX_DOCUMENT_TEXT_BYTES, MAX_PDF_NESTING_DEPTH, MAX_PDF_OBJECTS,
    MAX_PDF_PAGES, MAX_PDF_STREAM_DECOMPRESSED_BYTES,
};
use lopdf::content::{Content, Operation};
use lopdf::xref::XrefType;
use lopdf::{
    dictionary, Document, EncryptionState, EncryptionVersion, Object, Permissions, Stream,
};

#[test]
fn exact_utf8_text_and_markdown_are_immutable_and_path_free() {
    let text = Bytes::from_static(b"hello\r\nworld\n");
    let ingested = ingest_document("notes.txt", "text/plain", text.clone()).unwrap();
    assert_eq!(ingested.source_bytes(), &text);
    assert_eq!(ingested.model_text(), "hello\r\nworld\n");
    assert_eq!(
        ingested.provenance().media_type,
        DocumentMediaType::PlainText
    );
    assert_eq!(
        ingested.provenance().fidelity,
        ExtractionFidelity::ExactUtf8
    );
    assert_eq!(ingested.provenance().source_byte_count, text.len() as u64);
    assert_eq!(
        ingested.provenance().extracted_text_byte_count,
        text.len() as u64
    );
    assert_eq!(
        ingested.provenance().sha256,
        "4375539f2263c313c68efccaa296d00e561e44e5cb4863dfffd2fed733a8bad8"
    );
    assert_eq!(DocumentMediaType::PlainText.as_str(), "text/plain");
    let public = serde_json::to_string(ingested.provenance()).unwrap();
    assert!(!public.contains("path"));
    assert!(!public.contains("sourceBytes"));
    assert!(!format!("{ingested:?}").contains("hello"));

    let markdown = Bytes::from_static(b"# Heading\n\n- one\n- two\n");
    let first = ingest_document("README.md", "text/markdown", markdown.clone()).unwrap();
    let second = ingest_document("README.md", "text/markdown", markdown).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.model_text(), "# Heading\n\n- one\n- two\n");
}

#[test]
fn rejects_spoofed_media_extensions_binary_text_and_oversize() {
    let pdf = ordinary_pdf(&["hello"], true, None);
    assert_eq!(
        ingest_document("fake.txt", "text/plain", Bytes::from(pdf)),
        Err(DocumentIngestError::MediaTypeMismatch)
    );
    assert_eq!(
        ingest_document(
            "fake.pdf",
            "application/pdf",
            Bytes::from_static(b"plain text")
        ),
        Err(DocumentIngestError::MediaTypeMismatch)
    );
    assert_eq!(
        ingest_document("notes.md", "text/plain", Bytes::from_static(b"plain text")),
        Err(DocumentIngestError::MediaTypeMismatch)
    );
    assert_eq!(
        ingest_document(
            "../notes.txt",
            "text/plain",
            Bytes::from_static(b"plain text")
        ),
        Err(DocumentIngestError::InvalidDisplayName)
    );
    assert_eq!(
        ingest_document(
            "notes.txt",
            "text/plain; charset=utf-8",
            Bytes::from_static(b"plain text")
        ),
        Err(DocumentIngestError::UnsupportedMediaType)
    );
    assert_eq!(
        ingest_document(
            "invalid.txt",
            "text/plain",
            Bytes::from_static(&[0xff, 0xfe])
        ),
        Err(DocumentIngestError::InvalidUtf8)
    );
    assert_eq!(
        ingest_document(
            "binary.txt",
            "text/plain",
            Bytes::from_static(b"hello\0world")
        ),
        Err(DocumentIngestError::BinaryText)
    );
    assert_eq!(
        ingest_document(
            "huge.txt",
            "text/plain",
            Bytes::from(vec![b'a'; MAX_DOCUMENT_TEXT_BYTES + 1])
        ),
        Err(DocumentIngestError::TooLarge)
    );
    assert_eq!(
        ingest_document(
            "huge.pdf",
            "application/pdf",
            Bytes::from(vec![b'x'; MAX_DOCUMENT_FILE_BYTES + 1])
        ),
        Err(DocumentIngestError::TooLarge)
    );
}

#[test]
fn ordinary_compressed_pdf_extracts_deterministically_with_partial_notice() {
    let first_page = format!("Hello PDF {}", "word ".repeat(200));
    let pdf = ordinary_pdf(&[&first_page, "Second page"], true, None);
    assert!(pdf
        .windows(b"/FlateDecode".len())
        .any(|window| window == b"/FlateDecode"));
    let source = Bytes::from(pdf);
    let first = ingest_document("paper.pdf", "application/pdf", source.clone()).unwrap();
    let second = ingest_document("paper.pdf", "application/pdf", source.clone()).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.source_bytes(), &source);
    assert_eq!(
        first.provenance().fidelity,
        ExtractionFidelity::PdfTextOnlyPartial
    );
    assert_eq!(first.provenance().page_count, Some(2));
    assert!(first.provenance().object_count.unwrap() >= 7);
    assert!(first.model_text().starts_with(
        "[PDF text extraction is partial: visual layout, images, annotations, and exact glyph positioning are not preserved.]"
    ));
    assert!(first.model_text().contains("--- Page 1 ---"));
    assert!(first.model_text().contains("Hello PDF"));
    assert!(first.model_text().contains("--- Page 2 ---"));
    assert!(first.model_text().contains("Second page"));
}

#[test]
fn rejects_truncated_malformed_and_empty_text_pdfs() {
    let mut truncated = ordinary_pdf(&["hello"], false, None);
    truncated.truncate(truncated.len() - 4);
    assert_eq!(
        ingest_document("truncated.pdf", "application/pdf", Bytes::from(truncated)),
        Err(DocumentIngestError::MalformedPdf)
    );

    let malformed = Bytes::from_static(
        b"%PDF-1.4\nxref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 1 >>\nstartxref\n9\n%%EOF",
    );
    assert_eq!(
        ingest_document("broken.pdf", "application/pdf", malformed),
        Err(DocumentIngestError::MalformedPdf)
    );

    let invalid_header = Bytes::from_static(
        b"%PDF-1.4 trailing\nxref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 1 >>\nstartxref\n18\n%%EOF",
    );
    assert_eq!(
        ingest_document("header.pdf", "application/pdf", invalid_header),
        Err(DocumentIngestError::MediaTypeMismatch)
    );

    let blank = ordinary_pdf(&[""], false, None);
    assert_eq!(
        ingest_document("blank.pdf", "application/pdf", Bytes::from(blank)),
        Err(DocumentIngestError::NoExtractableText)
    );

    let mut trailing_junk = ordinary_pdf(&["hello"], false, None);
    trailing_junk.extend_from_slice(b"\nnot-whitespace");
    assert_eq!(
        ingest_document(
            "trailing.pdf",
            "application/pdf",
            Bytes::from(trailing_junk)
        ),
        Err(DocumentIngestError::MalformedPdf)
    );
}

#[test]
fn encrypted_and_password_protected_pdfs_are_rejected_before_extraction() {
    let mut document = ordinary_document(&["secret"], false, None);
    document.trailer.set(
        "ID",
        vec![
            Object::string_literal("document-id"),
            Object::string_literal("document-id"),
        ],
    );
    let encryption = EncryptionState::try_from(EncryptionVersion::V2 {
        document: &document,
        owner_password: "owner",
        user_password: "password",
        key_length: 128,
        permissions: Permissions::all(),
    })
    .unwrap();
    document.encrypt(&encryption).unwrap();
    let encrypted = save_classic(document);
    assert_eq!(
        ingest_document("encrypted.pdf", "application/pdf", Bytes::from(encrypted)),
        Err(DocumentIngestError::EncryptedPdf)
    );
}

#[test]
fn embedded_files_and_active_content_are_rejected() {
    let embedded = ordinary_pdf(
        &["hello"],
        false,
        Some(Object::Stream(Stream::new(
            dictionary! {"Type" => "EmbeddedFile"},
            b"hidden payload".to_vec(),
        ))),
    );
    assert_eq!(
        ingest_document("embedded.pdf", "application/pdf", Bytes::from(embedded)),
        Err(DocumentIngestError::EmbeddedFile)
    );

    let active = ordinary_pdf(
        &["hello"],
        false,
        Some(Object::Dictionary(dictionary! {
            "Type" => "Action",
            "S" => "JavaScript",
            "JS" => Object::string_literal("app.alert('x')")
        })),
    );
    assert_eq!(
        ingest_document("active.pdf", "application/pdf", Bytes::from(active)),
        Err(DocumentIngestError::UnsupportedPdfStructure)
    );
}

#[test]
fn xref_object_stream_and_incremental_structures_are_rejected() {
    let mut modern = ordinary_document(&["modern"], true, None);
    let mut bytes = Vec::new();
    modern.save_modern(&mut bytes).unwrap();
    assert_eq!(
        ingest_document("modern.pdf", "application/pdf", Bytes::from(bytes)),
        Err(DocumentIngestError::UnsupportedPdfStructure)
    );

    let mut incremental = ordinary_pdf(&["hello"], false, None);
    let trailer = incremental
        .windows(b"/Size".len())
        .rposition(|window| window == b"/Size")
        .unwrap();
    incremental.splice(trailer..trailer, b"/Prev 1 ".iter().copied());
    assert_eq!(
        ingest_document(
            "incremental.pdf",
            "application/pdf",
            Bytes::from(incremental)
        ),
        Err(DocumentIngestError::UnsupportedPdfStructure)
    );
}

#[test]
fn page_and_xref_object_limits_are_enforced_before_extraction() {
    let pages = vec!["x"; MAX_PDF_PAGES + 1];
    let pdf = ordinary_pdf(&pages, false, None);
    assert_eq!(
        ingest_document("pages.pdf", "application/pdf", Bytes::from(pdf)),
        Err(DocumentIngestError::PdfPageLimit)
    );

    let xref_bomb = format!(
        "%PDF-1.4\nxref\n0 {}\ntrailer\n<< /Size {} >>\nstartxref\n9\n%%EOF",
        MAX_PDF_OBJECTS + 2,
        MAX_PDF_OBJECTS + 2
    );
    assert_eq!(
        ingest_document("objects.pdf", "application/pdf", Bytes::from(xref_bomb)),
        Err(DocumentIngestError::PdfObjectLimit)
    );

    let mut nested = Object::Integer(1);
    for _ in 0..=MAX_PDF_NESTING_DEPTH {
        nested = Object::Array(vec![nested]);
    }
    let nested = ordinary_pdf(&["hello"], false, Some(nested));
    assert_eq!(
        ingest_document("nested.pdf", "application/pdf", Bytes::from(nested)),
        Err(DocumentIngestError::PdfObjectLimit)
    );
}

#[test]
fn decompression_bombs_and_excessive_model_output_are_rejected() {
    let decompression_bomb = ordinary_pdf(
        &[&" ".repeat(MAX_PDF_STREAM_DECOMPRESSED_BYTES + 1)],
        true,
        None,
    );
    assert_eq!(
        ingest_document(
            "bomb.pdf",
            "application/pdf",
            Bytes::from(decompression_bomb)
        ),
        Err(DocumentIngestError::PdfDecompressionLimit)
    );

    let output_bomb = ordinary_pdf(&[&"a".repeat(MAX_DOCUMENT_TEXT_BYTES + 1)], false, None);
    assert_eq!(
        ingest_document("output.pdf", "application/pdf", Bytes::from(output_bomb)),
        Err(DocumentIngestError::TooLarge)
    );
}

#[test]
fn unsupported_filters_and_decode_parameters_are_rejected() {
    let mut document = ordinary_document(&["hello"], false, None);
    let content_id = first_content_stream_id(&document);
    let stream = document
        .get_object_mut(content_id)
        .unwrap()
        .as_stream_mut()
        .unwrap();
    stream.dict.set("Filter", "LZWDecode");
    let pdf = save_classic(document);
    assert_eq!(
        ingest_document("filter.pdf", "application/pdf", Bytes::from(pdf)),
        Err(DocumentIngestError::UnsupportedPdfFilter)
    );

    let mut document = ordinary_document(&["hello"], true, None);
    let content_id = first_content_stream_id(&document);
    document
        .get_object_mut(content_id)
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .dict
        .set("DecodeParms", dictionary! {"Predictor" => 12});
    let pdf = save_classic(document);
    assert_eq!(
        ingest_document("params.pdf", "application/pdf", Bytes::from(pdf)),
        Err(DocumentIngestError::UnsupportedPdfFilter)
    );
}

fn ordinary_pdf(texts: &[&str], compress: bool, extra: Option<Object>) -> Vec<u8> {
    save_classic(ordinary_document(texts, compress, extra))
}

fn ordinary_document(texts: &[&str], compress: bool, extra: Option<Object>) -> Document {
    let mut document = Document::with_version("1.4");
    document.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    let pages_id = document.new_object_id();
    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = document.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_id,
        },
    });
    let mut page_ids = Vec::new();
    for text in texts {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 720.into()]),
                Operation::new("Tj", vec![Object::string_literal(text.as_bytes())]),
                Operation::new("ET", vec![]),
            ],
        };
        let mut stream = Stream::new(dictionary! {}, content.encode().unwrap());
        if compress {
            stream.compress().unwrap();
        }
        let content_id = document.add_object(stream);
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        page_ids.push(page_id);
    }
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => page_ids.len() as i64,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    if let Some(extra) = extra {
        document.add_object(extra);
    }
    document
}

fn save_classic(mut document: Document) -> Vec<u8> {
    document.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn first_content_stream_id(document: &Document) -> (u32, u16) {
    let page_id = *document.get_pages().values().next().unwrap();
    document
        .get_dictionary(page_id)
        .unwrap()
        .get(b"Contents")
        .unwrap()
        .as_reference()
        .unwrap()
}
