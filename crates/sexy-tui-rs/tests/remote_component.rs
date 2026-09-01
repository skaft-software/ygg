use sexy_tui_rs::remote_component::{
    parse_pi_rendered_row, parse_pi_rendered_rows, RemoteColor, RemoteComponentId, RemoteCursor,
    RemoteFrame, RemoteFrameErrorKind, RemoteFrameValidator, RemoteImageId, RemoteImagePlacement,
    RemoteLink, RemoteRow, RemoteSpan, RemoteStyle, MAX_REMOTE_FRAME_BYTES, MAX_REMOTE_ROWS,
    MAX_REMOTE_SPANS_PER_ROW, MAX_REMOTE_SPAN_TEXT_BYTES, MAX_REMOTE_WIRE_INTEGER,
};

fn component_id() -> RemoteComponentId {
    RemoteComponentId::parse("component:alpha").unwrap()
}

fn frame(rows: Vec<RemoteRow>, width: u16, revision: u64) -> RemoteFrame {
    RemoteFrame {
        component_id: component_id(),
        generation: 7,
        revision,
        width,
        rows,
        cursor: None,
        desired_height: None,
    }
}

#[test]
fn parses_printable_sgr_and_safe_osc8_into_semantic_spans() {
    let rendered = concat!(
        "plain ",
        "\x1b[1;38;5;196;48;2;1;2;3m",
        "red",
        "\x1b[22;39;49m ",
        "\x1b]8;id=docs;https://example.com/a?q=1\x1b\\",
        "docs",
        "\x1b]8;;\x1b\\"
    );
    let row = parse_pi_rendered_row(rendered).unwrap();
    let spans = row.as_spans().unwrap();
    assert_eq!(spans.len(), 4);
    assert_eq!(spans[0], RemoteSpan::plain("plain "));
    assert_eq!(spans[1].text, "red");
    assert_eq!(spans[1].style.fg, RemoteColor::Indexed(196));
    assert_eq!(
        spans[1].style.bg,
        RemoteColor::Rgb {
            red: 1,
            green: 2,
            blue: 3,
        }
    );
    assert!(spans[1].style.attributes.bold);
    assert_eq!(spans[2], RemoteSpan::plain(" "));
    assert_eq!(spans[3].text, "docs");
    assert_eq!(
        spans[3].safe_link.as_ref().unwrap().as_str(),
        "https://example.com/a?q=1"
    );

    let parsed = parse_pi_rendered_rows(&[rendered, "界 and e\u{301}"]).unwrap();
    let semantic = frame(parsed, 40, 1);
    semantic.validate().unwrap();
}

#[test]
fn rejects_terminal_commands_and_control_injection() {
    let hostile = [
        "\x1b[2Jerase",
        "\x1b[1Acursor",
        "\x1b[1;1Hposition",
        "\x1b[6nquery",
        "\x1b[?25lhide",
        "\x1b]52;c;Y2xpcA==\x1b\\",
        "\x1b]0;title\x1b\\",
        "\x1bP1;2|payload\x1b\\",
        "\x1b_Ga=T,f=100;AAAA\x1b\\",
        "\x1b^private\x1b\\",
        "\x1bXstart\x1b\\",
        "\x1bcreset",
        "bell\x07",
        "line\nfeed",
        "tab\tinjection",
        "nul\0byte",
        "c1\u{009b}2J",
        "bidi\u{202e}txt",
        "\x1b]8;;https://example.com\x07label",
    ];
    for rendered in hostile {
        let error = parse_pi_rendered_row(rendered).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                RemoteFrameErrorKind::UnsupportedTerminalSequence
                    | RemoteFrameErrorKind::InvalidText
                    | RemoteFrameErrorKind::InvalidStyle
            ),
            "{rendered:?}: {error}"
        );
    }
}

#[test]
fn rejects_unsupported_sgr_and_malformed_control_strings() {
    for rendered in [
        "\x1b[5mblink",
        "\x1b[8mhidden",
        "\x1b[58;5;1munderline-color",
        "\x1b[38;5;999mcolor",
        "\x1b[38;2;1;2mcolor",
        "\x1b[999999999999999999999moverflow",
        "\x1b[31",
        "\x1b]8;;https://example.com",
        "\x1b]8;bad;https://example.com\x1b\\x\x1b]8;;\x1b\\",
        "\x1b]8;;https://example.com\x1b\\one\x1b]8;;https://example.org\x1b\\two",
        "\x1b]8;;https://example.com\x1bX",
    ] {
        assert!(
            parse_pi_rendered_row(rendered).is_err(),
            "accepted malformed row {rendered:?}"
        );
    }
}

#[test]
fn links_are_http_only_absolute_and_credential_free() {
    assert_eq!(
        RemoteLink::parse("HTTPS://example.com/docs")
            .unwrap()
            .as_str(),
        "HTTPS://example.com/docs"
    );
    assert_eq!(
        RemoteLink::parse("https://example.com/界")
            .unwrap()
            .as_str(),
        "https://example.com/%E7%95%8C"
    );

    for target in [
        "javascript:alert(1)",
        "file:///tmp/secret",
        "mailto:user@example.com",
        "https:user@example.com",
        "https://user:pass@example.com/private",
        "https:///missing-host",
        "https://example.com/a b",
        "https://example.com\\@evil.test",
        "https://example.com:99999",
    ] {
        let error = RemoteLink::parse(target).unwrap_err();
        assert_eq!(error.kind(), RemoteFrameErrorKind::UnsafeLink, "{target}");
    }

    for target in ["javascript:alert(1)", "file:///tmp/x", "http:no-host"] {
        let rendered = format!("\x1b]8;;{target}\x1b\\x\x1b]8;;\x1b\\");
        assert!(parse_pi_rendered_row(&rendered).is_err(), "{target}");
    }
}

#[test]
fn validates_unicode_span_and_cursor_cell_boundaries() {
    let rows = vec![RemoteRow::spans(vec![
        RemoteSpan::plain("A"),
        RemoteSpan::plain("界"),
        RemoteSpan::plain("e\u{301}"),
        RemoteSpan::plain("👩‍💻"),
    ])];
    let mut semantic = frame(rows, 6, 1);
    semantic.cursor = Some(RemoteCursor { row: 0, column: 3 });
    semantic.validate().unwrap();

    semantic.cursor = Some(RemoteCursor { row: 0, column: 2 });
    let error = semantic.validate().unwrap_err();
    assert_eq!(error.kind(), RemoteFrameErrorKind::InvalidGeometry);
    assert!(error.to_string().contains("splits a wide Unicode cell"));

    semantic.cursor = None;
    semantic.width = 5;
    assert!(semantic.validate().is_err());

    let split_grapheme = frame(
        vec![RemoteRow::spans(vec![
            RemoteSpan::plain("e"),
            RemoteSpan {
                text: "\u{301}".into(),
                style: RemoteStyle {
                    fg: RemoteColor::Ansi16(1),
                    ..RemoteStyle::plain()
                },
                safe_link: None,
            },
        ])],
        10,
        1,
    );
    let error = split_grapheme.validate().unwrap_err();
    assert_eq!(error.kind(), RemoteFrameErrorKind::InvalidText);
    assert!(error.to_string().contains("inside a Unicode grapheme"));

    assert!(parse_pi_rendered_row("\u{301}").is_err());
    assert!(parse_pi_rendered_row("e\x1b[31m\u{301}\x1b[0m").is_err());
}

#[test]
fn enforces_frame_row_span_text_and_encoded_byte_limits() {
    let ordinary = frame(vec![RemoteRow::plain("ok")], 10, 1);
    let error = ordinary
        .validate_with_encoded_size(MAX_REMOTE_FRAME_BYTES + 1)
        .unwrap_err();
    assert_eq!(error.kind(), RemoteFrameErrorKind::FrameTooLarge);

    let oversized_source = "x".repeat(MAX_REMOTE_FRAME_BYTES);
    assert_eq!(
        parse_pi_rendered_rows(&[oversized_source])
            .unwrap_err()
            .kind(),
        RemoteFrameErrorKind::FrameTooLarge
    );

    let too_many_rows = frame(
        (0..=MAX_REMOTE_ROWS)
            .map(|_| RemoteRow::spans(Vec::new()))
            .collect(),
        10,
        1,
    );
    assert_eq!(
        too_many_rows.validate().unwrap_err().kind(),
        RemoteFrameErrorKind::LimitExceeded
    );

    let too_many_spans = frame(
        vec![RemoteRow::spans(
            (0..=MAX_REMOTE_SPANS_PER_ROW)
                .map(|_| RemoteSpan::plain("x"))
                .collect(),
        )],
        4096,
        1,
    );
    assert!(too_many_spans.validate().is_err());

    let oversized_span = frame(
        vec![RemoteRow::spans(vec![RemoteSpan::plain(
            "x".repeat(MAX_REMOTE_SPAN_TEXT_BYTES + 1),
        )])],
        4096,
        1,
    );
    assert_eq!(
        oversized_span.validate().unwrap_err().kind(),
        RemoteFrameErrorKind::LimitExceeded
    );

    let escaped_payload = frame(
        vec![RemoteRow::plain("\"".repeat(MAX_REMOTE_SPAN_TEXT_BYTES))],
        4096,
        1,
    );
    assert!(escaped_payload.estimated_encoded_bytes() > MAX_REMOTE_SPAN_TEXT_BYTES);
}

#[test]
fn validates_opaque_image_placement_shape_without_image_bytes() {
    let placement = RemoteImagePlacement {
        image_id: RemoteImageId::parse("artifact:sha256:abc123").unwrap(),
        column: 2,
        width: 10,
        height: 2,
    };
    let valid = frame(
        vec![
            RemoteRow::image(placement.clone()),
            RemoteRow::plain(""),
            RemoteRow::plain("after"),
        ],
        20,
        1,
    );
    valid.validate().unwrap();

    assert!(RemoteImageId::parse("bad\x1b[id").is_err());

    let mut invalid = valid.clone();
    invalid.rows[1] = RemoteRow::plain("not reserved");
    assert!(invalid
        .validate()
        .unwrap_err()
        .to_string()
        .contains("reserved row"));

    let mut invalid = valid.clone();
    if let RemoteRow::ImagePlacement { placement } = &mut invalid.rows[0] {
        placement.column = 15;
    }
    assert!(invalid
        .validate()
        .unwrap_err()
        .to_string()
        .contains("frame width"));

    let mut invalid = valid.clone();
    if let RemoteRow::ImagePlacement { placement } = &mut invalid.rows[0] {
        placement.height = 0;
    }
    assert!(invalid.validate().is_err());

    let mut invalid = valid;
    invalid.cursor = Some(RemoteCursor { row: 1, column: 0 });
    assert!(invalid
        .validate()
        .unwrap_err()
        .to_string()
        .contains("image"));
}

#[test]
fn semantic_frames_reject_controls_and_invalid_style_values() {
    let mut semantic = frame(vec![RemoteRow::plain("safe")], 10, 1);
    if let RemoteRow::Spans { spans } = &mut semantic.rows[0] {
        spans[0].text = "unsafe\x1b[2J".into();
    }
    assert_eq!(
        semantic.validate().unwrap_err().kind(),
        RemoteFrameErrorKind::InvalidText
    );

    let invalid_style = frame(
        vec![RemoteRow::spans(vec![RemoteSpan {
            text: "bad color".into(),
            style: RemoteStyle {
                fg: RemoteColor::Ansi16(16),
                ..RemoteStyle::plain()
            },
            safe_link: None,
        }])],
        20,
        1,
    );
    assert_eq!(
        invalid_style.validate().unwrap_err().kind(),
        RemoteFrameErrorKind::InvalidStyle
    );
}

#[test]
fn validator_matches_identity_generation_revision_width_and_advances_atomically() {
    let mut validator = RemoteFrameValidator::new(component_id(), 7).unwrap();
    let first = frame(vec![RemoteRow::plain("one")], 10, 1);
    validator.validate(&first, 1, 10).unwrap();
    assert_eq!(validator.last_revision(), Some(1));

    let error = validator.validate(&first, 1, 10).unwrap_err();
    assert_eq!(error.kind(), RemoteFrameErrorKind::NonMonotonicRevision);

    let mut invalid_second = frame(vec![RemoteRow::plain("two")], 10, 2);
    if let RemoteRow::Spans { spans } = &mut invalid_second.rows[0] {
        spans[0].text.push('\n');
    }
    assert!(validator.validate(&invalid_second, 2, 10).is_err());
    assert_eq!(validator.last_revision(), Some(1));

    let second = frame(vec![RemoteRow::plain("two")], 10, 2);
    validator.validate(&second, 2, 10).unwrap();
    assert_eq!(validator.last_revision(), Some(2));

    let mut wrong = frame(vec![RemoteRow::plain("three")], 10, 3);
    wrong.component_id = RemoteComponentId::parse("component:other").unwrap();
    assert_eq!(
        validator.validate(&wrong, 3, 10).unwrap_err().kind(),
        RemoteFrameErrorKind::IdentityMismatch
    );
    wrong.component_id = component_id();
    wrong.generation = 8;
    assert_eq!(
        validator.validate(&wrong, 3, 10).unwrap_err().kind(),
        RemoteFrameErrorKind::GenerationMismatch
    );
    wrong.generation = 7;
    assert_eq!(
        validator.validate(&wrong, 4, 10).unwrap_err().kind(),
        RemoteFrameErrorKind::RevisionMismatch
    );
    assert_eq!(
        validator.validate(&wrong, 3, 11).unwrap_err().kind(),
        RemoteFrameErrorKind::WidthMismatch
    );

    let error = validator
        .validate_with_encoded_size(&wrong, 3, 10, MAX_REMOTE_FRAME_BYTES + 1)
        .unwrap_err();
    assert_eq!(error.kind(), RemoteFrameErrorKind::FrameTooLarge);
    assert_eq!(validator.last_revision(), Some(2));
    validator.validate(&wrong, 3, 10).unwrap();

    let stale = frame(vec![RemoteRow::plain("late")], 10, 2);
    assert_eq!(
        validator.validate(&stale, 2, 10).unwrap_err().kind(),
        RemoteFrameErrorKind::NonMonotonicRevision
    );
}

#[test]
fn validates_portable_generation_revision_and_width_bounds() {
    let mut semantic = frame(vec![], 10, 1);
    semantic.generation = MAX_REMOTE_WIRE_INTEGER + 1;
    assert!(semantic.validate().is_err());
    semantic.generation = 7;
    semantic.revision = MAX_REMOTE_WIRE_INTEGER + 1;
    assert!(semantic.validate().is_err());
    semantic.revision = 1;
    semantic.width = 0;
    assert!(semantic.validate().is_err());
}
