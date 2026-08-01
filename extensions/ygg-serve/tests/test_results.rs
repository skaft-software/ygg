//! Standalone coverage for safe structured test-result parsing.

#[allow(dead_code)]
#[path = "../src/ids.rs"]
mod ids;
pub use ids::ItemId;
#[path = "../src/test_results.rs"]
mod test_results;

use test_results::{
    decode_structured_test_results, parse_test_output, ReportedTestCounts, TestCommandOutcome,
    TestCommandStatus, TestEvidenceCoverage, TestFramework, TestOutputInput, TestResultDecodeError,
    TestResultParseError, TestResultParser, TestStatus, TestVerificationOutcome,
    MAX_REPORTED_TESTS, MAX_STRUCTURED_TEST_RESULTS_BYTES, MAX_TEST_CASES_PER_SUITE,
    MAX_TEST_LABEL_BYTES, MAX_TEST_OUTPUT_BYTES, MAX_TEST_SUITES,
};

fn item_id() -> ItemId {
    ItemId::new("item-tool-tests").unwrap()
}

fn parse(
    output: &[u8],
    input_truncated: bool,
    framework_hint: Option<TestFramework>,
) -> Result<test_results::StructuredTestResults, TestResultParseError> {
    parse_with_command(
        output,
        input_truncated,
        framework_hint,
        TestCommandOutcome {
            status: TestCommandStatus::Succeeded,
            exit_code: Some(0),
            signal: None,
        },
    )
}

fn parse_failed(
    output: &[u8],
    input_truncated: bool,
    framework_hint: Option<TestFramework>,
) -> Result<test_results::StructuredTestResults, TestResultParseError> {
    parse_with_command(
        output,
        input_truncated,
        framework_hint,
        TestCommandOutcome {
            status: TestCommandStatus::Failed,
            exit_code: Some(1),
            signal: None,
        },
    )
}

fn parse_with_command(
    output: &[u8],
    input_truncated: bool,
    framework_hint: Option<TestFramework>,
    command: TestCommandOutcome,
) -> Result<test_results::StructuredTestResults, TestResultParseError> {
    parse_test_output(TestOutputInput {
        origin_item_id: item_id(),
        output,
        input_truncated,
        command,
        framework_hint,
    })
}

#[test]
fn parses_complete_cargo_libtest_without_exposing_paths_or_secrets() {
    let report = parse(
        include_bytes!("../fixtures/test-results/cargo-libtest.txt"),
        false,
        None,
    )
    .unwrap();

    assert_eq!(report.origin_item_id, item_id());
    assert_eq!(report.framework, TestFramework::CargoLibtest);
    assert_eq!(report.parser, TestResultParser::CargoLibtestTextV1);
    assert_eq!(report.command.status, TestCommandStatus::Succeeded);
    assert_eq!(report.verification, TestVerificationOutcome::Passed);
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::Complete);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Complete);
    assert!(!report.coverage.input_truncated);
    assert!(!report.coverage.records_truncated);
    assert_eq!(report.suites.len(), 1);
    assert_eq!(report.suites[0].name, "unittests <path>/lib.rs [ygg]");
    assert_eq!(
        report.suites[0].reported,
        ReportedTestCounts {
            total: None,
            passed: Some(2),
            failed: Some(0),
            skipped: Some(1),
            errors: None,
        }
    );
    assert_eq!(
        report.suites[0]
            .cases
            .iter()
            .map(|case| (case.name.as_str(), case.status))
            .collect::<Vec<_>>(),
        vec![
            ("parses_normal_case", TestStatus::Passed),
            (
                "case(<path>/secret.rs::does_not_leak_paths)",
                TestStatus::Passed
            ),
            ("auth_token=<redacted>", TestStatus::Skipped),
        ]
    );

    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("/Users/alice"));
    assert!(!json.contains("sk-example"));
    assert!(!json.contains("target/debug"));
}

#[test]
fn marks_caller_truncation_and_missing_summary_as_partial() {
    let report = parse(
        include_bytes!("../fixtures/test-results/cargo-truncated.txt"),
        true,
        Some(TestFramework::CargoLibtest),
    )
    .unwrap();

    assert!(report.coverage.input_truncated);
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::None);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Partial);
    assert_eq!(report.verification, TestVerificationOutcome::Inconclusive);
    assert_eq!(report.suites[0].cases.len(), 1);
    assert_eq!(report.suites[0].reported, ReportedTestCounts::default());
}

#[test]
fn preserves_distinct_cargo_workspace_binaries_without_target_paths() {
    let report = parse(
        include_bytes!("../fixtures/test-results/cargo-workspace.txt"),
        false,
        None,
    )
    .unwrap();

    assert_eq!(
        report
            .suites
            .iter()
            .map(|suite| suite.name.as_str())
            .collect::<Vec<_>>(),
        [
            "unittests src/lib.rs [alpha_core]",
            "tests/http.rs [http]",
            "unittests src/lib.rs [beta_core]",
        ]
    );
    assert_eq!(
        report
            .suites
            .iter()
            .map(|suite| suite.cases[0].name.as_str())
            .collect::<Vec<_>>(),
        ["alpha_works", "request_works", "beta_works"]
    );
    assert!(report
        .suites
        .iter()
        .all(|suite| suite.status == Some(TestStatus::Passed)));
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Complete);
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("target/debug"));
}

#[test]
fn parses_vitest_suites_cases_and_only_explicit_counts() {
    let report = parse_failed(
        include_bytes!("../fixtures/test-results/vitest.txt"),
        false,
        None,
    )
    .unwrap();

    assert_eq!(report.framework, TestFramework::Vitest);
    assert_eq!(report.suites.len(), 1);
    assert_eq!(report.suites[0].name, "src/math.test.ts");
    assert_eq!(report.suites[0].status, Some(TestStatus::Failed));
    assert_eq!(report.suites[0].cases.len(), 3);
    assert_eq!(report.reported.total, Some(3));
    assert_eq!(report.reported.passed, Some(1));
    assert_eq!(report.reported.failed, Some(1));
    assert_eq!(report.reported.skipped, Some(1));
    assert_eq!(report.reported.errors, None);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Complete);
    assert_eq!(report.verification, TestVerificationOutcome::Failed);
}

#[test]
fn parses_jest_and_does_not_invent_omitted_zeroes() {
    let output = b"PASS src/math.test.ts\n  \xe2\x9c\x93 adds (2 ms)\n\
        \xE2\x97\x8B pending behavior\n\
        Test Suites: 1 passed, 1 total\n\
        Tests: 1 skipped, 1 passed, 2 total\n";
    let report = parse(output, false, None).unwrap();

    assert_eq!(report.framework, TestFramework::Jest);
    assert_eq!(report.suites[0].status, Some(TestStatus::Passed));
    assert_eq!(report.suites[0].cases[0].name, "adds");
    assert_eq!(report.reported.total, Some(2));
    assert_eq!(report.reported.passed, Some(1));
    assert_eq!(report.reported.skipped, Some(1));
    assert_eq!(report.reported.failed, None);
    assert_eq!(report.reported.errors, None);
    assert_eq!(report.reported_suites.total, Some(1));
    assert_eq!(report.reported_suites.passed, Some(1));
    assert_eq!(report.summary_count, 2);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Complete);
}

#[test]
fn suite_and_file_summaries_cannot_hide_failures() {
    let jest = b"PASS src/looks-passing.test.ts\n\
        \xe2\x9c\x93 visible case (1 ms)\n\
        Test Suites: 1 failed, 1 total\n\
        Tests: 1 passed, 1 total\n";
    assert_eq!(
        parse(jest, false, Some(TestFramework::Jest)),
        Err(TestResultParseError::AmbiguousClaims)
    );

    let vitest = b"\xe2\x9c\x93 src/looks-passing.test.ts (1 test) 1ms\n\
        Test Files  1 failed (1)\n\
             Tests  1 passed (1)\n";
    assert_eq!(
        parse(vitest, false, Some(TestFramework::Vitest)),
        Err(TestResultParseError::AmbiguousClaims)
    );
}

#[test]
fn parses_pytest_pass_fail_skip_and_error_but_never_failure_bodies() {
    let report = parse_failed(
        include_bytes!("../fixtures/test-results/pytest-hostile.txt"),
        false,
        None,
    )
    .unwrap();

    assert_eq!(report.framework, TestFramework::Pytest);
    assert_eq!(report.suites.len(), 1);
    assert_eq!(report.suites[0].name, "<path>/test_api.py");
    assert_eq!(
        report.suites[0]
            .cases
            .iter()
            .map(|case| case.status)
            .collect::<Vec<_>>(),
        vec![
            TestStatus::Passed,
            TestStatus::Failed,
            TestStatus::Skipped,
            TestStatus::Error,
        ]
    );
    assert_eq!(report.reported.passed, Some(1));
    assert_eq!(report.reported.failed, Some(1));
    assert_eq!(report.reported.skipped, Some(1));
    assert_eq!(report.reported.errors, Some(1));
    assert_eq!(report.reported.total, None);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Partial);
    assert_eq!(report.verification, TestVerificationOutcome::Failed);

    let json = serde_json::to_string(&report).unwrap();
    for forbidden in [
        "/Users/alice",
        "Bearer",
        "fake-secret",
        "hunter2",
        "RuntimeError",
    ] {
        assert!(!json.contains(forbidden), "leaked {forbidden}");
    }
}

#[test]
fn parses_pytest_parameter_spaces_and_skip_reasons_without_reason_text() {
    let output = b"collected 1 item\n\
        /Users/alice/project/test_api.py::test_param[value with spaces] SKIPPED (requires secret service) [100%]\n\
        ================ 1 skipped in 0.01s ================\n";
    let report = parse(output, false, Some(TestFramework::Pytest)).unwrap();

    assert_eq!(report.suites[0].name, "<path>/test_api.py");
    assert_eq!(
        report.suites[0].cases[0].name,
        "test_param[value with spaces]"
    );
    assert_eq!(report.suites[0].cases[0].status, TestStatus::Skipped);
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("requires secret service"));
    assert_eq!(report.verification, TestVerificationOutcome::Inconclusive);
}

#[test]
fn parses_verbose_go_test_and_requires_run_to_terminal_linkage() {
    let output = b"=== RUN   TestAdd\n\
        --- PASS: TestAdd (0.00s)\n\
        === RUN   TestSkip\n\
        --- SKIP: TestSkip (0.00s)\n\
        ok  example.test/pkg  0.004s\n";
    let report = parse(output, false, None).unwrap();

    assert_eq!(report.framework, TestFramework::GoTest);
    assert_eq!(report.suites[0].name, "example.test/pkg");
    assert_eq!(report.suites[0].status, Some(TestStatus::Passed));
    assert_eq!(report.suites[0].cases.len(), 2);
    assert_eq!(report.reported, ReportedTestCounts::default());
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::Complete);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Complete);

    let unlinked = b"--- PASS: TestWasNeverRun (0.00s)\nok example/pkg 0.01s\n";
    assert_eq!(
        parse(unlinked, false, Some(TestFramework::GoTest)),
        Err(TestResultParseError::AmbiguousClaims)
    );
}

#[test]
fn go_unterminated_runs_are_explicitly_partial() {
    let output = b"=== RUN   TestFinished\n\
        === RUN   TestMissingTerminal\n\
        --- PASS: TestFinished (0.00s)\n\
        ok example/pkg 0.01s\n";
    let report = parse(output, false, Some(TestFramework::GoTest)).unwrap();

    assert!(report.coverage.records_truncated);
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::Partial);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Partial);
    assert_eq!(report.verification, TestVerificationOutcome::Inconclusive);
    assert_eq!(report.suites[0].cases.len(), 1);
}

#[test]
fn strips_terminal_controls_and_bounds_public_labels() {
    let long_name = "x".repeat(MAX_TEST_LABEL_BYTES * 2);
    let output = format!(
        "\x1b[31mRunning unittests src/lib.rs (target/debug/x)\x1b[0m\n\
         running 1 tests\n\
         test {long_name}\u{202e}token=sk-this-is-a-long-fake-secret-value ... ok\n\
         test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
         \x1b]0;Authorization: Bearer should-not-survive\u{7}"
    );
    let report = parse(output.as_bytes(), false, None).unwrap();
    let case = &report.suites[0].cases[0];

    assert!(case.name.len() <= MAX_TEST_LABEL_BYTES);
    assert!(case.name.ends_with('…'));
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains('\u{1b}'));
    assert!(!json.contains('\u{202e}'));
    assert!(!json.contains("should-not-survive"));
    assert!(!json.contains("long-fake-secret"));
}

#[test]
fn redacts_spaced_and_embedded_secret_assignments_in_case_names() {
    let output = b"PASS src/security.test.ts\n\
        \xe2\x9c\x93 request Authorization: Bearer fake-value (1 ms)\n\
        \xe2\x9c\x93 handles auth-token = sk-fake-value-that-must-not-survive (1 ms)\n\
        Test Suites: 1 passed, 1 total\n\
        Tests: 2 passed, 2 total\n";
    let report = parse(output, false, None).unwrap();
    let names = report.suites[0]
        .cases
        .iter()
        .map(|case| case.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            "request Authorization:<redacted>",
            "handles auth-token =<redacted>",
        ]
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("Bearer"));
    assert!(!json.contains("fake-value"));
}

#[test]
fn redacts_unc_spaced_paths_and_provider_secret_canaries() {
    let output = b"PASS src/security.test.ts\n\
        \xe2\x9c\x93 reads \\\\server\\private\\secret.txt (1 ms)\n\
        \xe2\x9c\x93 reads /Users/alice/Secret Folder/private.txt (1 ms)\n\
        \xe2\x9c\x93 uses xoxb-123456789012345678901234 (1 ms)\n\
        \xe2\x9c\x93 uses glpat-12345678901234567890123 (1 ms)\n\
        \xe2\x9c\x93 uses npm_123456789012345678901234 (1 ms)\n\
        \xe2\x9c\x93 uses hf_1234567890123456789012345 (1 ms)\n\
        \xe2\x9c\x93 client_secret = never-project-this (1 ms)\n\
        \xe2\x9c\x93 AWS_SECRET_ACCESS_KEY = never-project-this-either (1 ms)\n\
        Test Suites: 1 passed, 1 total\n\
        Tests: 8 passed, 8 total\n";
    let report = parse(output, false, None).unwrap();
    let json = serde_json::to_string(&report).unwrap();

    for forbidden in [
        "server",
        "Secret Folder",
        "private.txt",
        "xoxb-",
        "glpat-",
        "npm_",
        "hf_",
        "never-project",
    ] {
        assert!(!json.contains(forbidden), "leaked {forbidden}");
    }
    assert_eq!(report.suites[0].cases[0].name, "reads <path>/secret.txt");
    assert_eq!(report.suites[0].cases[1].name, "reads <path>");
}

#[test]
fn strips_dcs_apc_pm_and_sos_terminal_string_payloads() {
    let output = b"PASS src/controls.test.ts\n\
        \xe2\x9c\x93 dcs-before \x1bPtoken=never-show-dcs\x1b\\ dcs-after (1 ms)\n\
        \xe2\x9c\x93 apc-before \x1b_token=never-show-apc\x1b\\ apc-after (1 ms)\n\
        \xe2\x9c\x93 pm-before \x1b^token=never-show-pm\x1b\\ pm-after (1 ms)\n\
        \xe2\x9c\x93 sos-before \x1bXtoken=never-show-sos\x1b\\ sos-after (1 ms)\n\
        \xe2\x9c\x93 c1-before \xc2\x90token=never-show-c1\xc2\x9c c1-after (1 ms)\n\
        Test Suites: 1 passed, 1 total\n\
        Tests: 5 passed, 5 total\n";
    let report = parse(output, false, None).unwrap();
    let json = serde_json::to_string(&report).unwrap();

    assert!(!json.contains("never-show"));
    assert_eq!(
        report.suites[0]
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        [
            "dcs-before dcs-after",
            "apc-before apc-after",
            "pm-before pm-after",
            "sos-before sos-after",
            "c1-before c1-after",
        ]
    );
}

#[test]
fn unsupported_nonzero_summary_categories_force_partial_evidence() {
    let cargo = b"running 1 tests\n\
        test unit_case ... ok\n\
        test result: ok. 1 passed; 0 failed; 0 ignored; 1 measured; 0 filtered out; finished in 0.00s\n";
    let report = parse(cargo, false, Some(TestFramework::CargoLibtest)).unwrap();
    assert!(report.coverage.unsupported_summary_fields);
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::Partial);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Partial);
    assert_eq!(report.verification, TestVerificationOutcome::Inconclusive);

    let jest = b"PASS src/todo.test.ts\n\
        \xe2\x9c\x93 implemented (1 ms)\n\
        Test Suites: 1 passed, 1 total\n\
        Tests: 1 passed, 1 todo, 2 total\n";
    let report = parse(jest, false, Some(TestFramework::Jest)).unwrap();
    assert!(report.coverage.unsupported_summary_fields);
    assert_eq!(report.verification, TestVerificationOutcome::Inconclusive);

    let pytest = b"collected 1 item\n\
        test_sample.py::test_ok PASSED [100%]\n\
        ================ 1 passed, 1 warning in 0.01s ================\n";
    let report = parse(pytest, false, Some(TestFramework::Pytest)).unwrap();
    assert!(report.coverage.unsupported_summary_fields);
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::Partial);
}

#[test]
fn sanitized_suite_and_case_collisions_remain_distinct() {
    let output = b"PASS /Users/alice/one/security.test.ts\n\
        \xe2\x9c\x93 reads /Users/alice/one/same.txt (1 ms)\n\
        \xe2\x9c\x93 reads /opt/private/two/same.txt (1 ms)\n\
        PASS /opt/private/two/security.test.ts\n\
        \xe2\x9c\x93 second suite (1 ms)\n\
        Test Suites: 2 passed, 2 total\n\
        Tests: 3 passed, 3 total\n";
    let report = parse(output, false, None).unwrap();

    assert_eq!(
        report
            .suites
            .iter()
            .map(|suite| suite.name.as_str())
            .collect::<Vec<_>>(),
        ["<path>/security.test.ts", "<path>/security.test.ts #2"]
    );
    assert_eq!(
        report.suites[0]
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        ["reads <path>/same.txt", "reads <path>/same.txt #2"]
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("/Users/alice"));
    assert!(!json.contains("/opt/private"));
}

#[test]
fn originating_tool_failure_overrides_otherwise_passing_test_counts() {
    let output = b"running 1 tests\n\
        test unit_case ... ok\n\
        test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    let report = parse_failed(output, false, Some(TestFramework::CargoLibtest)).unwrap();

    assert_eq!(report.suites[0].status, Some(TestStatus::Passed));
    assert_eq!(report.command.exit_code, Some(1));
    assert_eq!(report.verification, TestVerificationOutcome::Failed);

    let contradictory = parse_with_command(
        output,
        false,
        Some(TestFramework::CargoLibtest),
        TestCommandOutcome {
            status: TestCommandStatus::Succeeded,
            exit_code: Some(7),
            signal: None,
        },
    );
    assert_eq!(contradictory, Err(TestResultParseError::AmbiguousClaims));
}

#[test]
fn rejects_unsupported_ambiguous_and_conflicting_claims() {
    assert_eq!(
        parse(b"ordinary command output\n", false, None),
        Err(TestResultParseError::Unsupported)
    );
    assert_eq!(
        parse(
            include_bytes!("../fixtures/test-results/ambiguous.txt"),
            false,
            None,
        ),
        Err(TestResultParseError::AmbiguousFramework)
    );
    assert_eq!(
        parse(
            include_bytes!("../fixtures/test-results/ambiguous.txt"),
            false,
            Some(TestFramework::CargoLibtest),
        ),
        Err(TestResultParseError::AmbiguousFramework)
    );

    let conflicting = b"running 1 tests\n\
        test only_case ... ok\n\
        test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(
        parse(conflicting, false, None),
        Err(TestResultParseError::AmbiguousClaims)
    );

    let duplicate_summaries = b"running 1 tests\n\
        test only_case ... ok\n\
        test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
        test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(
        parse(
            duplicate_summaries,
            false,
            Some(TestFramework::CargoLibtest)
        ),
        Err(TestResultParseError::AmbiguousClaims)
    );

    let impossible_cargo_status = b"running 1 tests\n\
        test broken ... FAILED\n\
        test result: ok. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(
        parse(
            impossible_cargo_status,
            false,
            Some(TestFramework::CargoLibtest)
        ),
        Err(TestResultParseError::AmbiguousClaims)
    );

    let impossible_go_status =
        b"=== RUN   TestBroken\n--- FAIL: TestBroken (0.00s)\nok example/pkg 0.01s\n";
    assert_eq!(
        parse(impossible_go_status, false, Some(TestFramework::GoTest)),
        Err(TestResultParseError::AmbiguousClaims)
    );
}

#[test]
fn parser_bounds_are_fail_closed_and_record_omissions_are_explicit() {
    assert_eq!(
        parse(b"", false, Some(TestFramework::CargoLibtest)),
        Err(TestResultParseError::Empty)
    );
    assert_eq!(
        parse(
            &vec![b'x'; MAX_TEST_OUTPUT_BYTES + 1],
            false,
            Some(TestFramework::CargoLibtest),
        ),
        Err(TestResultParseError::TooLarge)
    );

    let mut output = String::from("running 10001 tests\n");
    for index in 0..=MAX_TEST_CASES_PER_SUITE {
        output.push_str(&format!("test case_{index} ... ok\n"));
    }
    output.push_str(
        "test result: ok. 10001 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s\n",
    );
    let report = parse(output.as_bytes(), false, Some(TestFramework::CargoLibtest)).unwrap();
    assert!(report.coverage.records_truncated);
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::Partial);
    assert_eq!(report.coverage.cases, TestEvidenceCoverage::Partial);
    assert_eq!(report.suites[0].cases.len(), MAX_TEST_CASES_PER_SUITE);
}

#[test]
fn persisted_results_round_trip_only_through_checked_validation() {
    let report = parse(
        include_bytes!("../fixtures/test-results/cargo-libtest.txt"),
        false,
        None,
    )
    .unwrap();
    let bytes = serde_json::to_vec(&report).unwrap();
    assert_eq!(decode_structured_test_results(&bytes).unwrap(), report);

    let mut hostile = serde_json::to_value(&report).unwrap();
    hostile["suites"][0]["name"] = serde_json::json!("/Users/alice/private/project");
    assert_eq!(
        decode_structured_test_results(&serde_json::to_vec(&hostile).unwrap()),
        Err(TestResultDecodeError::Invalid)
    );

    let mut hostile = serde_json::to_value(&report).unwrap();
    hostile["suites"][0]["cases"][0]["name"] =
        serde_json::json!("token=raw-secret-that-must-not-replay");
    assert_eq!(
        decode_structured_test_results(&serde_json::to_vec(&hostile).unwrap()),
        Err(TestResultDecodeError::Invalid)
    );

    let mut hostile = serde_json::to_value(&report).unwrap();
    hostile["suites"][0]["cases"][0]["name"] = serde_json::json!("safe\u{202e}spoofed");
    assert_eq!(
        decode_structured_test_results(&serde_json::to_vec(&hostile).unwrap()),
        Err(TestResultDecodeError::Invalid)
    );

    let truncated = parse(
        include_bytes!("../fixtures/test-results/cargo-truncated.txt"),
        true,
        Some(TestFramework::CargoLibtest),
    )
    .unwrap();
    let mut forged = serde_json::to_value(&truncated).unwrap();
    forged["coverage"]["inputTruncated"] = serde_json::json!(false);
    forged["coverage"]["recordsTruncated"] = serde_json::json!(false);
    forged["coverage"]["summaries"] = serde_json::json!("complete");
    forged["coverage"]["cases"] = serde_json::json!("complete");
    forged["verification"] = serde_json::json!("passed");
    assert_eq!(
        decode_structured_test_results(&serde_json::to_vec(&forged).unwrap()),
        Err(TestResultDecodeError::Invalid)
    );

    let mut unknown = serde_json::to_value(&report).unwrap();
    unknown["rawOutput"] = serde_json::json!("must not be accepted");
    assert_eq!(
        decode_structured_test_results(&serde_json::to_vec(&unknown).unwrap()),
        Err(TestResultDecodeError::Invalid)
    );

    let mut excessive_count = serde_json::to_value(&report).unwrap();
    excessive_count["reported"]["total"] = serde_json::json!(MAX_REPORTED_TESTS + 1);
    assert_eq!(
        decode_structured_test_results(&serde_json::to_vec(&excessive_count).unwrap()),
        Err(TestResultDecodeError::Invalid)
    );

    let mut excessive_suites = serde_json::to_value(&report).unwrap();
    let suite = excessive_suites["suites"][0].clone();
    excessive_suites["suites"] = serde_json::Value::Array(vec![suite; MAX_TEST_SUITES + 1]);
    assert_eq!(
        decode_structured_test_results(&serde_json::to_vec(&excessive_suites).unwrap()),
        Err(TestResultDecodeError::Invalid)
    );

    assert_eq!(
        decode_structured_test_results(b""),
        Err(TestResultDecodeError::Empty)
    );
    assert_eq!(
        decode_structured_test_results(&vec![b' '; MAX_STRUCTURED_TEST_RESULTS_BYTES + 1]),
        Err(TestResultDecodeError::TooLarge)
    );
}

#[test]
fn invalid_utf8_and_oversized_physical_lines_cannot_escape() {
    let huge_line = "z".repeat(test_results::MAX_TEST_OUTPUT_LINE_BYTES + 1);
    let mut output = format!("{huge_line}\nRunning unittests src/lib.rs (target/x)\n").into_bytes();
    output.extend_from_slice(b"running 1 tests\ntest safe_\xff_name ... ok\n");
    output.extend_from_slice(
        b"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
    );
    let report = parse(&output, false, None).unwrap();

    assert!(report.coverage.records_truncated);
    assert_eq!(report.coverage.summaries, TestEvidenceCoverage::Partial);
    assert!(report.suites[0].cases[0].name.contains('\u{fffd}'));
    assert!(serde_json::to_string(&report).is_ok());
}

#[test]
fn unknown_escape_before_multibyte_text_cannot_panic_or_split_utf8() {
    let output = "Running unittests src/lib.rs (target/debug/deps/example)\n\
        running 1 tests\n\
        test safe_\u{1b}é_name ... ok\n\
        test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    let report = parse(output.as_bytes(), false, None).unwrap();

    assert_eq!(report.suites[0].cases[0].name, "safe__name");
}
