//! Golden, strictness, resource-bound, and hostile-input tests for v1 schemas.

use std::collections::BTreeMap;

use ygg_migrate_types::{
    render_compare_report_markdown, CompareReport, CompareReportHeader, CompareTaskRow, Diagnostic,
    DiagnosticSeverity, McpServer, McpTransport, MigratedSetup, MigrationOutcome, Model,
    Permission, PermissionDecision, Skill, ValidationError, COMPARE_REPORT_SCHEMA_VERSION,
    MAX_DIAGNOSTICS, MAX_JSON_INPUT_BYTES, MAX_JSON_NESTING, MAX_LIST_ENTRIES, MAX_MAP_ENTRIES,
    MAX_MCP_ARGUMENTS, MAX_MCP_SERVERS, MAX_MODELS, MAX_PERMISSIONS, MAX_PORTABLE_JSON_INTEGER,
    MAX_SKILLS, MAX_STRING_BYTES, MAX_TASKS, MAX_TOTAL_DECODED_COLLECTION_ENTRIES,
    MAX_TOTAL_DECODED_RECORDS, MAX_TOTAL_DECODED_STRING_BYTES, MAX_TOTAL_JSON_ENTRIES,
    MAX_TOTAL_JSON_STRING_BYTES, MIGRATED_SETUP_SCHEMA_VERSION, ROOT_DIAGNOSTIC_PATH,
};

const MIGRATED_SETUP_GOLDEN: &str = include_str!("fixtures/migrated-setup-v1.json");
const COMPARE_REPORT_GOLDEN: &str = include_str!("fixtures/compare-report-v1.json");
const COMPARE_REPORT_MARKDOWN_GOLDEN: &str = include_str!("fixtures/compare-report-v1.md");

#[test]
fn canonical_raw_json_round_trips_keep_validated_types_off_generic_deserialize() {
    let setup = MigratedSetup::from_json(MIGRATED_SETUP_GOLDEN).expect("decode setup");
    assert_eq!(setup.schema_version(), MIGRATED_SETUP_SCHEMA_VERSION);
    assert_eq!(setup.source_agent(), "pi");
    assert_eq!(setup.models().len(), 1);
    assert_eq!(
        setup.to_canonical_json().expect("canonical setup"),
        MIGRATED_SETUP_GOLDEN
    );
    let serialized = serde_json::to_string(&setup).expect("serialize setup");
    assert_eq!(
        MigratedSetup::from_json(&serialized).expect("raw round-trip setup"),
        setup
    );

    let report = CompareReport::from_json(COMPARE_REPORT_GOLDEN).expect("decode report");
    assert_eq!(
        report.header().schema_version(),
        COMPARE_REPORT_SCHEMA_VERSION
    );
    assert_eq!(report.tasks().len(), 2);
    assert_eq!(
        report.to_canonical_json().expect("canonical report"),
        COMPARE_REPORT_GOLDEN
    );
    let serialized = serde_json::to_string(&report).expect("serialize report");
    assert_eq!(
        CompareReport::from_json(&serialized).expect("raw round-trip report"),
        report
    );

    let header_json = r#"{
  "schema_version": 1,
  "versions": {"ygg": "0.6.7"},
  "hardware": {"cpu": "Apple M4"}
}"#;
    let header = CompareReportHeader::from_json(header_json).expect("decode header");
    assert_eq!(header.schema_version(), COMPARE_REPORT_SCHEMA_VERSION);
    assert_eq!(
        CompareReportHeader::from_json(&serde_json::to_string(&header).expect("serialize header"),)
            .expect("raw round-trip header"),
        header
    );

    let mut setup_builder = MigratedSetup::new("builder").expect("checked setup builder");
    setup_builder
        .push_model(
            MigrationOutcome::mapped(
                "settings.model",
                Model::new("provider", "model").expect("checked model"),
            )
            .expect("checked model outcome"),
        )
        .expect("checked push model");
    assert_eq!(setup_builder.models().len(), 1);

    let mut reversed_tasks = report.tasks().to_vec();
    reversed_tasks.reverse();
    let unsorted = CompareReport::new(report.header().clone(), reversed_tasks)
        .expect("checked unsorted report");
    assert_eq!(unsorted.tasks()[0].task_id(), "task-002");
    assert_eq!(
        unsorted
            .to_canonical_json()
            .expect("canonical sorted report"),
        COMPARE_REPORT_GOLDEN
    );
    assert_eq!(unsorted.tasks()[0].task_id(), "task-002");
}

#[test]
fn comparison_metadata_maps_reject_literal_and_escape_equivalent_duplicates_before_values() {
    let literal = compare_report_with_metadata(
        r#""ygg": "0.6.7",
      "ygg": ["duplicate values are not decoded"]"#,
        r#""cpu": "Apple M4""#,
    );
    assert_error_contains(
        CompareReport::from_json(&literal),
        r#"duplicate comparison metadata key "ygg""#,
    );

    let escaped = compare_report_with_metadata(
        r#""ygg": "0.6.7",
      "y\u0067g": {"wrong": "duplicate values are not decoded"}"#,
        r#""cpu": "Apple M4""#,
    );
    assert_error_contains(
        CompareReport::from_json(&escaped),
        r#"duplicate comparison metadata key "ygg""#,
    );

    let hardware = compare_report_with_metadata(
        r#""ygg": "0.6.7""#,
        r#""cpu": "Apple M4",
      "c\u0070u": [false]"#,
    );
    assert_error_contains(
        CompareReport::from_json(&hardware),
        r#"duplicate comparison metadata key "cpu""#,
    );
}

#[test]
fn comparison_metadata_maps_sort_valid_entries_and_header_canonicalizes() {
    let report = CompareReport::from_json(
        r#"{
  "header": {
    "schema_version": 1,
    "versions": {
      "zeta": "3",
      "alpha": "1"
    },
    "hardware": {
      "operating_system": "macOS",
      "memory": "16"
    }
  },
  "tasks": []
}"#,
    )
    .expect("decode comparison metadata maps");

    assert_eq!(
        report
            .header()
            .versions()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["alpha", "zeta"]
    );
    assert_eq!(
        report
            .header()
            .hardware()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["memory", "operating_system"]
    );
    assert_eq!(
        report.to_canonical_json().expect("canonical report"),
        r#"{
  "header": {
    "schema_version": 1,
    "versions": {
      "alpha": "1",
      "zeta": "3"
    },
    "hardware": {
      "memory": "16",
      "operating_system": "macOS"
    }
  },
  "tasks": []
}
"#
    );
    assert_eq!(
        report
            .header()
            .to_canonical_json()
            .expect("canonical header"),
        r#"{
  "schema_version": 1,
  "versions": {
    "alpha": "1",
    "zeta": "3"
  },
  "hardware": {
    "memory": "16",
    "operating_system": "macOS"
  }
}
"#
    );
}

#[test]
fn strict_unknown_and_duplicate_fields_are_rejected_at_each_wire_nesting_level() {
    let top_unknown = MIGRATED_SETUP_GOLDEN.replacen(
        "\"source_agent\": \"pi\",",
        "\"source_agent\": \"pi\",\n  \"unexpected\": true,",
        1,
    );
    assert_error_contains(
        MigratedSetup::from_json(&top_unknown),
        "unknown field `unexpected`",
    );

    let top_duplicate = MIGRATED_SETUP_GOLDEN.replacen(
        "\"source_agent\": \"pi\",",
        "\"source_agent\": \"pi\",\n  \"source_agent\": \"pi\",",
        1,
    );
    assert_error_contains(
        MigratedSetup::from_json(&top_duplicate),
        "duplicate field `source_agent`",
    );

    let top_escape_equivalent_duplicate = MIGRATED_SETUP_GOLDEN.replacen(
        "\"source_agent\": \"pi\",",
        "\"source_agent\": \"pi\",\n  \"source_\\u0061gent\": \"pi\",",
        1,
    );
    assert_error_contains(
        MigratedSetup::from_json(&top_escape_equivalent_duplicate),
        "duplicate field `source_agent`",
    );

    let outcome_unknown = MIGRATED_SETUP_GOLDEN.replacen(
        "\"path\": \"settings.model\",",
        "\"path\": \"settings.model\",\n      \"future_outcome\": true,",
        1,
    );
    assert_error_contains(
        MigratedSetup::from_json(&outcome_unknown),
        "unknown field `future_outcome`",
    );

    let value_unknown = MIGRATED_SETUP_GOLDEN.replacen(
        "\"provider\": \"openai\",",
        "\"provider\": \"openai\",\n        \"future_model\": true,",
        1,
    );
    assert_error_contains(
        MigratedSetup::from_json(&value_unknown),
        "unknown field `future_model`",
    );

    let diagnostic_duplicate = MIGRATED_SETUP_GOLDEN.replacen(
        "\"reason\": \"legacy skill front matter has no v1 mapping\"",
        "\"reason\": \"legacy skill front matter has no v1 mapping\",\n        \"reason\": \"duplicate\"",
        1,
    );
    assert_error_contains(
        MigratedSetup::from_json(&diagnostic_duplicate),
        "duplicate field `reason`",
    );

    let transport_unknown =
        MIGRATED_SETUP_GOLDEN.replacen("\"args\": [", "\"headers\": {},\n          \"args\": [", 1);
    assert_error_contains(
        MigratedSetup::from_json(&transport_unknown),
        "unknown field `headers`",
    );

    let header_unknown = COMPARE_REPORT_GOLDEN.replacen(
        "\"schema_version\": 1,",
        "\"schema_version\": 1,\n    \"future_header\": true,",
        1,
    );
    assert_error_contains(
        CompareReport::from_json(&header_unknown),
        "unknown field `future_header`",
    );

    let task_unknown = COMPARE_REPORT_GOLDEN.replacen(
        "\"task_id\": \"task-001\",",
        "\"task_id\": \"task-001\",\n      \"future_task\": true,",
        1,
    );
    assert_error_contains(
        CompareReport::from_json(&task_unknown),
        "unknown field `future_task`",
    );

    let task_duplicate = COMPARE_REPORT_GOLDEN.replacen(
        "\"task_id\": \"task-001\",",
        "\"task_id\": \"task-001\",\n      \"task_id\": \"task-001\",",
        1,
    );
    assert_error_contains(
        CompareReport::from_json(&task_duplicate),
        "duplicate field `task_id`",
    );
}

#[test]
fn unsupported_versions_win_over_bounded_additive_and_wrong_typed_fields() {
    let setup = r#"{
  "future_top_level": true,
  "schema_version": 2,
  "source_agent": false,
  "models": true,
  "skills": {},
  "mcp_servers": 0,
  "permissions": null,
  "diagnostics": "wrong"
}"#;
    assert_error_contains(
        MigratedSetup::from_json(setup),
        "unsupported MigratedSetup schema version 2; expected 1",
    );

    let header = r#"{
  "future": true,
  "schema_version": 2,
  "versions": false,
  "hardware": []
}"#;
    assert_error_contains(
        CompareReportHeader::from_json(header),
        "unsupported CompareReportHeader schema version 2; expected 1",
    );

    let report = r#"{
  "future_top_level": true,
  "header": {
    "future_header": true,
    "schema_version": 2,
    "versions": false,
    "hardware": []
  },
  "tasks": {}
}"#;
    assert_error_contains(
        CompareReport::from_json(report),
        "unsupported CompareReport schema version 2; expected 1",
    );
}

#[test]
fn markdown_is_rendered_only_from_validated_json_and_is_literal_terminal_safe() {
    assert_eq!(
        render_compare_report_markdown(COMPARE_REPORT_GOLDEN).expect("render golden"),
        COMPARE_REPORT_MARKDOWN_GOLDEN
    );

    let hostile_json = r#"{
  "header": {
    "schema_version": 1,
    "versions": {
      "<script>alert('x')</script>": "[release](https://example.invalid/?q=<tag>)",
      "control\u001b\u0007\u202e": "pipe|slash\\line\nnext\r\nlast\u2028\rbare"
    },
    "hardware": {
      "![image](https://example.invalid/image)": "<br>&amp;`code`*em*",
      "key\u2066": "bidi\u2069"
    }
  },
  "tasks": [
    {
      "task_id": "[task](https://example.invalid) | `code` \\ slash www.example.invalid",
      "agent": "<img src=x onerror=alert(1)>\u001b]52;c;clipboard\u0007\u202e",
      "wall_clock": 1,
      "peak_rss_bytes": 2,
      "tokens_in": 3,
      "tokens_out": 4,
      "success": true
    }
  ]
}"#;

    let markdown = render_compare_report_markdown(hostile_json).expect("render hostile");
    for expected in [
        r"&lt;script&gt;alert\(&#39;x&#39;\)&lt;\/script&gt;",
        r"\[release\]\(https\:\/\/example.invalid\/?q=&lt;tag&gt;\)",
        r"control\u{001B}\u{0007}\u{202E}",
        r"pipe\|slash\\line<br>next<br>last\u{2028}\u{000D}bare",
        r"\!\[image\]\(https\:\/\/example.invalid\/image\)",
        r"&lt;br&gt;&amp;amp;\`code\`\*em\*",
        r"key\u{2066}",
        r"bidi\u{2069}",
        r"\[task\]\(https\:\/\/example.invalid\) \| \`code\` \\ slash www\.example.invalid",
        r"&lt;img src=x onerror=alert\(1\)&gt;\u{001B}\]52;c;clipboard\u{0007}\u{202E}",
    ] {
        assert!(
            markdown.contains(expected),
            "missing {expected:?} in {markdown}"
        );
    }
    assert_eq!(markdown.matches("<br>").count(), 2, "{markdown}");
    for character in [
        '\u{0007}', '\u{000D}', '\u{001B}', '\u{2028}', '\u{202E}', '\u{2066}', '\u{2069}',
    ] {
        assert!(
            !markdown.contains(character),
            "rendered Markdown retained U+{:04X}: {markdown}",
            u32::from(character)
        );
    }
    for active_fragment in [
        "<script>",
        "<img ",
        "[release](https://",
        "![image](https://",
        "[task](https://",
    ] {
        assert!(
            !markdown.contains(active_fragment),
            "rendered Markdown retained active {active_fragment:?}: {markdown}"
        );
    }
}

#[test]
fn total_input_and_nesting_bounds_accept_exact_limits_and_reject_plus_one() {
    let base = report_with_tasks(0);
    let mut exact_input = base.clone();
    exact_input.push_str(&" ".repeat(MAX_JSON_INPUT_BYTES - exact_input.len()));
    CompareReport::from_json(&exact_input).expect("input size exactly at limit");
    exact_input.push(' ');
    assert_error_contains(
        CompareReport::from_json(&exact_input),
        "MAX_JSON_INPUT_BYTES",
    );

    let exact_depth = report_with_future_nesting(MAX_JSON_NESTING - 1);
    assert_error_not_contains(CompareReport::from_json(&exact_depth), "MAX_JSON_NESTING");
    let plus_depth = report_with_future_nesting(MAX_JSON_NESTING);
    assert_error_contains(CompareReport::from_json(&plus_depth), "MAX_JSON_NESTING");
}

#[test]
fn raw_aggregate_string_and_entry_preflight_limits_are_exact_and_hostile_safe() {
    let exact_strings = report_with_raw_string_bytes(MAX_TOTAL_JSON_STRING_BYTES);
    assert_error_contains(
        CompareReport::from_json(&exact_strings),
        "unsupported CompareReport schema version 2; expected 1",
    );
    let plus_strings = report_with_raw_string_bytes(MAX_TOTAL_JSON_STRING_BYTES + 1);
    assert_error_contains(
        CompareReport::from_json(&plus_strings),
        "MAX_TOTAL_JSON_STRING_BYTES",
    );

    let exact_entries = report_with_raw_entries(MAX_TOTAL_JSON_ENTRIES);
    assert_error_contains(
        CompareReport::from_json(&exact_entries),
        "unsupported CompareReport schema version 2; expected 1",
    );
    let plus_entries = report_with_raw_entries(MAX_TOTAL_JSON_ENTRIES + 1);
    assert_error_contains(
        CompareReport::from_json(&plus_entries),
        "MAX_TOTAL_JSON_ENTRIES",
    );
}

#[test]
fn string_map_and_list_bounds_apply_to_typed_and_raw_entry_points() {
    let exact_string = "a".repeat(MAX_STRING_BYTES);
    Model::new(exact_string.clone(), "model").expect("exact string constructor bound");
    assert_error_contains(
        Model::new("a".repeat(MAX_STRING_BYTES + 1), "model"),
        "MAX_STRING_BYTES",
    );

    let setup_exact_string = migrated_setup_with_source_agent(&exact_string);
    MigratedSetup::from_json(&setup_exact_string).expect("exact raw string bound");
    let setup_plus_string = migrated_setup_with_source_agent(&"a".repeat(MAX_STRING_BYTES + 1));
    assert_error_contains(
        MigratedSetup::from_json(&setup_plus_string),
        "MAX_STRING_BYTES",
    );

    let exact_map = metadata_map(MAX_MAP_ENTRIES);
    CompareReportHeader::new(exact_map.clone(), BTreeMap::new()).expect("exact map bound");
    CompareReportHeader::from_json(&header_json(&exact_map, &BTreeMap::new()))
        .expect("exact raw map bound");
    let plus_map = metadata_map(MAX_MAP_ENTRIES + 1);
    assert_error_contains(
        CompareReportHeader::new(plus_map.clone(), BTreeMap::new()),
        "versions exceeds its limit",
    );
    assert_error_contains(
        CompareReportHeader::from_json(&header_json(&plus_map, &BTreeMap::new())),
        "JSON object exceeds its limit",
    );

    let exact_args = (0..MAX_MCP_ARGUMENTS)
        .map(|_| "a".to_owned())
        .collect::<Vec<_>>();
    McpTransport::stdio("command", exact_args).expect("exact MCP args bound");
    let plus_args = (0..(MAX_MCP_ARGUMENTS + 1))
        .map(|_| "a".to_owned())
        .collect::<Vec<_>>();
    assert_error_contains(
        McpTransport::stdio("command", plus_args),
        "MCP arguments exceeds its limit",
    );

    CompareReport::from_json(&report_with_tasks(MAX_LIST_ENTRIES))
        .expect("exact raw task list bound");
    assert_error_contains(
        CompareReport::from_json(&report_with_tasks(MAX_LIST_ENTRIES + 1)),
        "JSON array exceeds its limit",
    );
}

#[test]
fn aggregate_decoded_collection_limit_covers_nested_typed_lists() {
    let full_servers = MAX_MCP_SERVERS - 1;
    assert_eq!(
        full_servers * (MAX_MCP_ARGUMENTS + 1) + 1,
        MAX_TOTAL_DECODED_COLLECTION_ENTRIES
    );

    let arguments = vec![String::new(); MAX_MCP_ARGUMENTS];
    let mut servers = Vec::with_capacity(MAX_MCP_SERVERS);
    for index in 0..full_servers {
        let transport =
            McpTransport::stdio("command", arguments.clone()).expect("bounded stdio transport");
        let server = McpServer::new(format!("server-{index}"), transport).expect("bounded server");
        servers.push(
            MigrationOutcome::mapped(format!("servers/{index}"), server)
                .expect("bounded server outcome"),
        );
    }
    let transport = McpTransport::http("https://example.test").expect("bounded HTTP transport");
    let server = McpServer::new("last-server", transport).expect("bounded final server");
    servers.push(
        MigrationOutcome::mapped("servers/last", server).expect("bounded final server outcome"),
    );

    let mut setup = MigratedSetup::with_parts(
        "source",
        Vec::new(),
        Vec::new(),
        servers,
        Vec::new(),
        Vec::new(),
    )
    .expect("exact aggregate collection limit");
    assert_error_contains(
        setup.push_diagnostic(
            Diagnostic::new(
                ROOT_DIAGNOSTIC_PATH,
                DiagnosticSeverity::Warning,
                "one too many",
            )
            .expect("bounded diagnostic"),
        ),
        "MAX_TOTAL_DECODED_COLLECTION_ENTRIES",
    );
    assert!(
        setup.diagnostics().is_empty(),
        "failed pushes must not mutate setup"
    );
}

#[test]
fn category_task_and_aggregate_record_limits_accept_exact_and_reject_plus_one() {
    MigratedSetup::with_parts(
        "source",
        model_outcomes(MAX_MODELS),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .expect("exact model limit");
    assert_error_contains(
        MigratedSetup::with_parts(
            "source",
            model_outcomes(MAX_MODELS + 1),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
        "models exceeds its limit",
    );

    MigratedSetup::with_parts(
        "source",
        Vec::new(),
        skill_outcomes(MAX_SKILLS),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .expect("exact skill limit");
    assert_error_contains(
        MigratedSetup::with_parts(
            "source",
            Vec::new(),
            skill_outcomes(MAX_SKILLS + 1),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
        "skills exceeds its limit",
    );

    MigratedSetup::with_parts(
        "source",
        Vec::new(),
        Vec::new(),
        server_outcomes(MAX_MCP_SERVERS),
        Vec::new(),
        Vec::new(),
    )
    .expect("exact MCP-server limit");
    assert_error_contains(
        MigratedSetup::with_parts(
            "source",
            Vec::new(),
            Vec::new(),
            server_outcomes(MAX_MCP_SERVERS + 1),
            Vec::new(),
            Vec::new(),
        ),
        "MCP servers exceeds its limit",
    );

    MigratedSetup::with_parts(
        "source",
        Vec::new(),
        Vec::new(),
        Vec::new(),
        permission_outcomes(MAX_PERMISSIONS),
        Vec::new(),
    )
    .expect("exact permission limit");
    assert_error_contains(
        MigratedSetup::with_parts(
            "source",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            permission_outcomes(MAX_PERMISSIONS + 1),
            Vec::new(),
        ),
        "permissions exceeds its limit",
    );

    MigratedSetup::with_parts(
        "source",
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        diagnostics(MAX_DIAGNOSTICS),
    )
    .expect("exact diagnostic limit");
    assert_error_contains(
        MigratedSetup::with_parts(
            "source",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            diagnostics(MAX_DIAGNOSTICS + 1),
        ),
        "diagnostics exceeds its limit",
    );

    let nested_diagnostics = MigratedSetup::with_parts(
        "source",
        unmapped_model_outcomes(MAX_DIAGNOSTICS),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert!(nested_diagnostics.is_ok(), "{nested_diagnostics:?}");
    assert_error_contains(
        MigratedSetup::with_parts(
            "source",
            unmapped_model_outcomes(MAX_DIAGNOSTICS),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            diagnostics(1),
        ),
        "diagnostics exceed their limit",
    );

    CompareReport::new(valid_header(), tasks(MAX_TASKS)).expect("exact task limit");
    assert_error_contains(
        CompareReport::new(valid_header(), tasks(MAX_TASKS + 1)),
        "tasks exceeds its limit",
    );

    let exact_records = MigratedSetup::with_parts(
        "source",
        model_outcomes(MAX_MODELS),
        skill_outcomes(MAX_TOTAL_DECODED_RECORDS - MAX_MODELS),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert!(exact_records.is_ok(), "{exact_records:?}");
    assert_error_contains(
        MigratedSetup::with_parts(
            "source",
            model_outcomes(MAX_MODELS),
            skill_outcomes(MAX_TOTAL_DECODED_RECORDS - MAX_MODELS),
            Vec::new(),
            Vec::new(),
            diagnostics(1),
        ),
        "MAX_TOTAL_DECODED_RECORDS",
    );
}

#[test]
fn decoded_string_aggregate_limit_accepts_exact_and_rejects_plus_one_on_construction_and_decode() {
    let exact = metadata_with_payload_bytes(MAX_TOTAL_DECODED_STRING_BYTES);
    CompareReportHeader::new(exact.clone(), BTreeMap::new())
        .expect("exact decoded payload string limit");
    let report_json = header_json(&exact, &BTreeMap::new());
    CompareReportHeader::from_json(&report_json).expect("exact decoded raw payload limit");

    let plus = metadata_with_payload_bytes(MAX_TOTAL_DECODED_STRING_BYTES + 1);
    assert_error_contains(
        CompareReportHeader::new(plus.clone(), BTreeMap::new()),
        "MAX_TOTAL_DECODED_STRING_BYTES",
    );
    assert_error_contains(
        CompareReportHeader::from_json(&header_json(&plus, &BTreeMap::new())),
        "MAX_TOTAL_DECODED_STRING_BYTES",
    );
}

#[test]
fn all_metrics_accept_the_portable_boundary_and_reject_plus_one_on_decode_and_construction() {
    for metric_index in 0..4 {
        let mut metric_text = [
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
        ];
        metric_text[metric_index] = MAX_PORTABLE_JSON_INTEGER.to_string();
        CompareReport::from_json(&report_with_metric_texts(&metric_text))
            .unwrap_or_else(|error| panic!("metric {metric_index} exact decode failed: {error}"));

        let mut metric_values = [0_u64; 4];
        metric_values[metric_index] = MAX_PORTABLE_JSON_INTEGER;
        CompareTaskRow::new(
            "task",
            "agent",
            metric_values[0],
            metric_values[1],
            metric_values[2],
            metric_values[3],
            true,
        )
        .unwrap_or_else(|error| panic!("metric {metric_index} exact constructor failed: {error}"));

        metric_text[metric_index] = (MAX_PORTABLE_JSON_INTEGER + 1).to_string();
        assert_error_contains(
            CompareReport::from_json(&report_with_metric_texts(&metric_text)),
            metric_name(metric_index),
        );

        metric_values[metric_index] = MAX_PORTABLE_JSON_INTEGER + 1;
        assert_error_contains(
            CompareTaskRow::new(
                "task",
                "agent",
                metric_values[0],
                metric_values[1],
                metric_values[2],
                metric_values[3],
                true,
            ),
            metric_name(metric_index),
        );
    }

    for metric_index in 0..4 {
        let mut metric_text = [
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
        ];
        metric_text[metric_index] = "1.0".to_owned();
        assert_error_contains(
            CompareReport::from_json(&report_with_metric_texts(&metric_text)),
            metric_name(metric_index),
        );
        metric_text[metric_index] = "1e0".to_owned();
        assert_error_contains(
            CompareReport::from_json(&report_with_metric_texts(&metric_text)),
            metric_name(metric_index),
        );
        metric_text[metric_index] = "-1".to_owned();
        assert_error_contains(
            CompareReport::from_json(&report_with_metric_texts(&metric_text)),
            metric_name(metric_index),
        );
        metric_text[metric_index] = "-0".to_owned();
        assert_error_contains(
            CompareReport::from_json(&report_with_metric_texts(&metric_text)),
            metric_name(metric_index),
        );
    }
}

#[test]
fn diagnostics_reject_invisible_unicode_in_constructors_and_raw_json() {
    // Every non-bidi range boundary in the fixed Default_Ignorable_Code_Point
    // policy, plus the requested tag and supplementary variation-selector
    // examples, must fail through both validated entry points.
    for scalar in [
        "\u{00ad}",
        "\u{034f}",
        "\u{115f}",
        "\u{1160}",
        "\u{17b4}",
        "\u{17b5}",
        "\u{180b}",
        "\u{180f}",
        "\u{200b}",
        "\u{2060}",
        "\u{3164}",
        "\u{fe00}",
        "\u{fe0f}",
        "\u{feff}",
        "\u{ffa0}",
        "\u{fff0}",
        "\u{fff8}",
        "\u{1bca0}",
        "\u{1bca3}",
        "\u{1d173}",
        "\u{1d17a}",
        "\u{e0000}",
        "\u{e0061}",
        "\u{e0100}",
        "\u{e0fff}",
    ] {
        let path = format!("settings/{scalar}model");
        assert_diagnostic_path_rejected_in_constructor_and_raw(
            &path,
            "default-ignorable formatting or tag characters",
        );
        assert_diagnostic_reason_rejected_in_constructor_and_raw(
            scalar,
            "visible non-whitespace, non-default-ignorable scalar",
        );
    }

    // C0/C1 and bidi-format values use the more specific unsafe-character
    // diagnostic. The latter values are also default-ignorable range bounds.
    for unsafe_scalar in [
        "\u{001b}", "\u{0080}", "\u{061c}", "\u{200f}", "\u{202a}", "\u{202e}", "\u{206f}",
    ] {
        assert_diagnostic_path_rejected_in_constructor_and_raw(
            &format!("settings/{unsafe_scalar}model"),
            "control or bidirectional-format characters",
        );
        assert_diagnostic_reason_rejected_in_constructor_and_raw(
            &format!("reason{unsafe_scalar}"),
            "control or bidirectional-format characters",
        );
    }

    let invisible_only = "\u{200b}\u{2060}\u{feff}\u{e0061}\u{fe0f}\u{e0100}";
    assert_diagnostic_path_rejected_in_constructor_and_raw(
        invisible_only,
        "default-ignorable formatting or tag characters",
    );
    assert_diagnostic_reason_rejected_in_constructor_and_raw(
        invisible_only,
        "visible non-whitespace, non-default-ignorable scalar",
    );

    assert_diagnostic_path_rejected_in_constructor_and_raw(
        "\u{2003}\u{00a0}",
        "leading or trailing whitespace",
    );
    assert_diagnostic_reason_rejected_in_constructor_and_raw(
        "\u{2003}\u{00a0}",
        "visible non-whitespace, non-default-ignorable scalar",
    );
}

#[test]
fn diagnostics_require_actionable_normalized_paths_and_visible_reasons_in_all_paths() {
    Diagnostic::new(
        ROOT_DIAGNOSTIC_PATH,
        DiagnosticSeverity::Warning,
        "setup needs review",
    )
    .expect("explicit root diagnostic");
    let visible_path = "設定/δοκιμή/مرحبا";
    let visible_reason = "設定を確認してください — مرحبا κόσμε";
    let variation_reason = "確認 ⚠\u{fe0f}";
    Diagnostic::new(visible_path, DiagnosticSeverity::Warning, visible_reason)
        .expect("visible international diagnostic");
    let diagnostic = Diagnostic::new(visible_path, DiagnosticSeverity::Warning, variation_reason)
        .expect("visible reason may retain a variation selector");
    MigratedSetup::from_json(&diagnostic_setup_json(visible_path, variation_reason))
        .expect("raw visible reason may retain a variation selector");
    MigratedSetup::from_json(&diagnostic_setup_json(visible_path, visible_reason))
        .expect("raw visible international diagnostic");

    let mut setup = MigratedSetup::new("source").expect("checked setup");
    setup
        .push_diagnostic(diagnostic)
        .expect("checked diagnostic mutator");
    let canonical = setup
        .to_canonical_json()
        .expect("canonical revalidates visible diagnostic");
    assert_eq!(
        MigratedSetup::from_json(&canonical).expect("canonical diagnostic round-trip"),
        setup
    );

    for path in [
        "",
        "   ",
        " settings.model",
        "settings.model ",
        "a//b",
        "../a",
        "/a",
        "a\\b",
        "a\u{001b}b",
    ] {
        assert_diagnostic_path_rejected_in_constructor_and_raw(path, "diagnostic path");
    }

    for reason in ["", " \t\n", "because\u{001b}", "because\u{202e}"] {
        assert_diagnostic_reason_rejected_in_constructor_and_raw(reason, "diagnostic reason");
    }

    let exact_path = format!("a{}", "x".repeat(MAX_STRING_BYTES - 1));
    Diagnostic::new(exact_path, DiagnosticSeverity::Warning, "reason")
        .expect("exact path string limit");
    assert_error_contains(
        Diagnostic::new(
            format!("a{}", "x".repeat(MAX_STRING_BYTES)),
            DiagnosticSeverity::Warning,
            "reason",
        ),
        "MAX_STRING_BYTES",
    );

    let exact_reason = "r".repeat(MAX_STRING_BYTES);
    Diagnostic::new(
        ROOT_DIAGNOSTIC_PATH,
        DiagnosticSeverity::Warning,
        exact_reason,
    )
    .expect("exact reason string limit");
    assert_error_contains(
        Diagnostic::new(
            ROOT_DIAGNOSTIC_PATH,
            DiagnosticSeverity::Warning,
            "r".repeat(MAX_STRING_BYTES + 1),
        ),
        "MAX_STRING_BYTES",
    );
}

fn compare_report_with_metadata(versions: &str, hardware: &str) -> String {
    format!(
        r#"{{
  "header": {{
    "schema_version": 1,
    "versions": {{
      {versions}
    }},
    "hardware": {{
      {hardware}
    }}
  }},
  "tasks": []
}}"#
    )
}

fn report_with_tasks(count: usize) -> String {
    let tasks = (0..count)
        .map(|index| {
            format!(
                r#"{{"task_id":"task-{index}","agent":"agent","wall_clock":1,"peak_rss_bytes":2,"tokens_in":3,"tokens_out":4,"success":true}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"header":{{"schema_version":1,"versions":{{}},"hardware":{{}}}},"tasks":[{tasks}]}}"#
    )
}

fn report_with_future_nesting(array_depth: usize) -> String {
    let nested = format!("{}0{}", "[".repeat(array_depth), "]".repeat(array_depth));
    format!(
        r#"{{"header":{{"schema_version":1,"versions":{{}},"hardware":{{}}}},"tasks":[],"future":{nested}}}"#
    )
}

fn migrated_setup_with_source_agent(source_agent: &str) -> String {
    format!(
        r#"{{"schema_version":1,"source_agent":{},"models":[],"skills":[],"mcp_servers":[],"permissions":[],"diagnostics":[]}}"#,
        serde_json::to_string(source_agent).expect("encode source agent")
    )
}

fn report_with_raw_string_bytes(total_string_bytes: usize) -> String {
    // The raw document has three fixed string keys: "header" (6),
    // "schema_version" (14), and "payload" (7).
    const FIXED_KEY_BYTES: usize = 27;
    let mut remaining = total_string_bytes - FIXED_KEY_BYTES;
    let mut values = Vec::new();
    while remaining > 0 {
        let bytes = remaining.min(MAX_STRING_BYTES);
        values.push("v".repeat(bytes));
        remaining -= bytes;
    }
    format!(
        r#"{{"header":{{"schema_version":2}},"payload":{}}}"#,
        serde_json::to_string(&values).expect("encode raw string payload")
    )
}

fn report_with_raw_entries(total_entries: usize) -> String {
    // The outer report has two members and its minimal header has one. The
    // payload's outer list uses 128 entries, with child lists supplying the
    // remaining entries without breaching a per-list bound.
    const FIXED_ENTRIES: usize = 3;
    let payload_entries = total_entries - FIXED_ENTRIES;
    assert!(payload_entries >= MAX_LIST_ENTRIES);
    let mut remaining_child_entries = payload_entries - MAX_LIST_ENTRIES;
    let mut children = Vec::new();
    for _ in 0..MAX_LIST_ENTRIES {
        let entries = remaining_child_entries.min(MAX_LIST_ENTRIES);
        children.push(format!("[{}]", vec!["0"; entries].join(",")));
        remaining_child_entries -= entries;
    }
    assert_eq!(remaining_child_entries, 0, "test entry payload must fit");
    format!(
        r#"{{"header":{{"schema_version":2}},"payload":[{}]}}"#,
        children.join(",")
    )
}

fn valid_header() -> CompareReportHeader {
    CompareReportHeader::new(BTreeMap::new(), BTreeMap::new()).expect("valid header")
}

fn model_outcomes(count: usize) -> Vec<MigrationOutcome<Model>> {
    (0..count)
        .map(|index| {
            MigrationOutcome::<Model>::mapped(
                format!("models/{index}"),
                Model::new("provider", format!("model-{index}")).expect("valid model"),
            )
            .expect("valid model outcome")
        })
        .collect()
}

fn unmapped_model_outcomes(count: usize) -> Vec<MigrationOutcome<Model>> {
    (0..count)
        .map(|index| {
            MigrationOutcome::<Model>::unmapped(
                Diagnostic::new(
                    format!("models/{index}"),
                    DiagnosticSeverity::Warning,
                    "needs review",
                )
                .expect("valid nested diagnostic"),
            )
            .expect("valid unmapped model outcome")
        })
        .collect()
}

fn skill_outcomes(count: usize) -> Vec<MigrationOutcome<Skill>> {
    (0..count)
        .map(|index| {
            MigrationOutcome::<Skill>::mapped(
                format!("skills/{index}"),
                Skill::new(format!("skill-{index}"), "content").expect("valid skill"),
            )
            .expect("valid skill outcome")
        })
        .collect()
}

fn server_outcomes(count: usize) -> Vec<MigrationOutcome<McpServer>> {
    (0..count)
        .map(|index| {
            MigrationOutcome::<McpServer>::mapped(
                format!("mcp/{index}"),
                McpServer::new(
                    format!("server-{index}"),
                    McpTransport::http("https://example.invalid/mcp").expect("valid transport"),
                )
                .expect("valid server"),
            )
            .expect("valid server outcome")
        })
        .collect()
}

fn permission_outcomes(count: usize) -> Vec<MigrationOutcome<Permission>> {
    (0..count)
        .map(|index| {
            MigrationOutcome::<Permission>::mapped(
                format!("permissions/{index}"),
                Permission::new(format!("capability-{index}"), PermissionDecision::Ask)
                    .expect("valid permission"),
            )
            .expect("valid permission outcome")
        })
        .collect()
}

fn diagnostics(count: usize) -> Vec<Diagnostic> {
    (0..count)
        .map(|index| {
            Diagnostic::new(
                format!("diagnostics/{index}"),
                DiagnosticSeverity::Warning,
                "needs review",
            )
            .expect("valid diagnostic")
        })
        .collect()
}

fn tasks(count: usize) -> Vec<CompareTaskRow> {
    (0..count)
        .map(|index| {
            CompareTaskRow::new(format!("task-{index}"), "agent", 1, 2, 3, 4, true)
                .expect("valid task")
        })
        .collect()
}

fn metadata_map(count: usize) -> BTreeMap<String, String> {
    (0..count)
        .map(|index| (format!("key-{index}"), "value".to_owned()))
        .collect()
}

fn metadata_with_payload_bytes(total: usize) -> BTreeMap<String, String> {
    let keys = ["a", "b", "c", "d"];
    let key_bytes = keys.iter().map(|key| key.len()).sum::<usize>();
    let mut remaining = total - key_bytes;
    let mut values = BTreeMap::new();
    for key in keys {
        let bytes = remaining.min(MAX_STRING_BYTES);
        values.insert(key.to_owned(), "v".repeat(bytes));
        remaining -= bytes;
    }
    assert_eq!(remaining, 0, "test payload must fit four bounded strings");
    values
}

fn header_json(versions: &BTreeMap<String, String>, hardware: &BTreeMap<String, String>) -> String {
    format!(
        r#"{{"schema_version":1,"versions":{},"hardware":{}}}"#,
        serde_json::to_string(versions).expect("encode versions"),
        serde_json::to_string(hardware).expect("encode hardware"),
    )
}

fn report_with_metric_texts(metrics: &[String; 4]) -> String {
    format!(
        r#"{{"header":{{"schema_version":1,"versions":{{}},"hardware":{{}}}},"tasks":[{{"task_id":"task","agent":"agent","wall_clock":{},"peak_rss_bytes":{},"tokens_in":{},"tokens_out":{},"success":true}}]}}"#,
        metrics[0], metrics[1], metrics[2], metrics[3]
    )
}

fn metric_name(index: usize) -> &'static str {
    ["wall_clock", "peak_rss_bytes", "tokens_in", "tokens_out"][index]
}

fn diagnostic_setup_json(path: &str, reason: &str) -> String {
    format!(
        r#"{{"schema_version":1,"source_agent":"source","models":[],"skills":[],"mcp_servers":[],"permissions":[],"diagnostics":[{{"path":{},"severity":"warning","reason":{}}}]}}"#,
        serde_json::to_string(path).expect("encode path"),
        serde_json::to_string(reason).expect("encode reason"),
    )
}

fn assert_diagnostic_path_rejected_in_constructor_and_raw(path: &str, expected: &str) {
    assert_error_contains(
        Diagnostic::new(path, DiagnosticSeverity::Warning, "reason"),
        expected,
    );
    assert_error_contains(
        MigratedSetup::from_json(&diagnostic_setup_json(path, "reason")),
        expected,
    );
}

fn assert_diagnostic_reason_rejected_in_constructor_and_raw(reason: &str, expected: &str) {
    assert_error_contains(
        Diagnostic::new("settings.model", DiagnosticSeverity::Warning, reason),
        expected,
    );
    assert_error_contains(
        MigratedSetup::from_json(&diagnostic_setup_json("settings.model", reason)),
        expected,
    );
}

fn assert_error_contains<T>(result: std::result::Result<T, ValidationError>, expected: &str) {
    let error = match result {
        Ok(_) => panic!("input must be rejected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?} in {error}"
    );
}

fn assert_error_not_contains<T>(result: std::result::Result<T, ValidationError>, unexpected: &str) {
    let error = match result {
        Ok(_) => panic!("future field must be rejected after preflight"),
        Err(error) => error,
    };
    assert!(
        !error.to_string().contains(unexpected),
        "did not expect {unexpected:?} in {error}"
    );
}
