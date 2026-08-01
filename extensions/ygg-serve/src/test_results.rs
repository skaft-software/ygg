//! Structured, bounded projections of supported test-run output.
//!
//! This module is deliberately a text-to-facts boundary, not a log viewer.
//! Parsers recognize only deterministic reporter lines and never retain or
//! return the input stream, failure bodies, stack traces, or diagnostics.
//! Labels that are safe to expose are control-cleaned, path-redacted,
//! secret-redacted, and byte-bounded before entering the result.
//!
//! The adapter integration seam is [`parse_test_output`]: invoke it while
//! settling a recognized verification tool, pass that tool's [`ItemId`] and
//! frozen command status/exit facts plus the transport truncation bit, and
//! persist the serializable result beside the immutable completion-review run
//! record. A later protocol field can expose this DTO directly without
//! reparsing logs during replay.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ItemId;

/// Maximum accepted test output (2 MiB).
pub const MAX_TEST_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum accepted bytes in one physical output line.
pub const MAX_TEST_OUTPUT_LINE_BYTES: usize = 32 * 1024;
/// Maximum physical lines inspected from one output.
pub const MAX_TEST_OUTPUT_LINES: usize = 65_536;
/// Maximum number of structured suites retained from one command.
pub const MAX_TEST_SUITES: usize = 256;
/// Maximum number of structured cases retained from one command.
pub const MAX_TEST_CASES: usize = 5_000;
/// Maximum number of structured cases retained in one suite.
pub const MAX_TEST_CASES_PER_SUITE: usize = 2_500;
/// Maximum bytes in a public suite or case label.
pub const MAX_TEST_LABEL_BYTES: usize = 512;
/// Maximum aggregate bytes retained across suite and case labels.
pub const MAX_TEST_RESULT_LABEL_BYTES: usize = 128 * 1024;
/// Maximum retained terminal summary records.
pub const MAX_TEST_SUMMARIES: usize = 512;
/// Maximum serialized structured result or persisted decode input (512 KiB).
pub const MAX_STRUCTURED_TEST_RESULTS_BYTES: usize = 512 * 1024;
/// Maximum accepted reporter count.
pub const MAX_REPORTED_TESTS: u32 = 10_000_000;

/// Supported test framework.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestFramework {
    /// Rust's built-in libtest runner, normally launched by Cargo.
    CargoLibtest,
    /// Vitest's default text reporter.
    Vitest,
    /// Jest's default text reporter.
    Jest,
    /// Pytest's default or verbose text reporter.
    Pytest,
    /// Go's verbose test reporter.
    GoTest,
}

/// Exact deterministic parser revision used for a projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestResultParser {
    /// Bounded parser for Cargo/libtest text output.
    CargoLibtestTextV1,
    /// Bounded parser for Vitest text output.
    VitestTextV1,
    /// Bounded parser for Jest text output.
    JestTextV1,
    /// Bounded parser for Pytest text output.
    PytestTextV1,
    /// Bounded parser for `go test -v` text output.
    GoTestTextV1,
}

impl TestFramework {
    fn parser(self) -> TestResultParser {
        match self {
            Self::CargoLibtest => TestResultParser::CargoLibtestTextV1,
            Self::Vitest => TestResultParser::VitestTextV1,
            Self::Jest => TestResultParser::JestTextV1,
            Self::Pytest => TestResultParser::PytestTextV1,
            Self::GoTest => TestResultParser::GoTestTextV1,
        }
    }
}

/// Public terminal status for a suite or case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestStatus {
    /// The suite or case passed.
    Passed,
    /// The suite or case failed an assertion or test condition.
    Failed,
    /// The suite or case was explicitly skipped or ignored.
    Skipped,
    /// The test runner explicitly classified the case as an error.
    Error,
}

/// Terminal state of the originating tool execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestCommandStatus {
    /// The tool and its process settled successfully.
    Succeeded,
    /// The tool or its process failed.
    Failed,
    /// The tool was stopped before normal completion.
    Stopped,
}

/// Immutable terminal facts from the tool result that carried the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestCommandOutcome {
    /// Terminal tool status.
    pub status: TestCommandStatus,
    /// Process exit code, when the host captured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Process signal, when the host captured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

/// Review-level result after binding reporter evidence to the tool outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestVerificationOutcome {
    /// Complete non-empty reporter evidence and the tool both succeeded.
    Passed,
    /// The originating tool failed, including after otherwise-passing tests.
    Failed,
    /// The originating tool was stopped.
    Stopped,
    /// Evidence was empty, partial, or truncated despite tool success.
    Inconclusive,
}

/// Whether a category of evidence is absent, partial, or complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestEvidenceCoverage {
    /// No evidence of this kind was recognized.
    None,
    /// Some facts were recognized, but the parser cannot prove full coverage.
    Partial,
    /// Reporter facts prove that all records of this kind were captured.
    Complete,
}

/// Explicit parse-coverage metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestParseCoverage {
    /// Whether the caller reported that bytes were dropped before parsing.
    pub input_truncated: bool,
    /// Whether one or more records were omitted by parser safety bounds.
    pub records_truncated: bool,
    /// Whether non-zero reporter categories lack a safe v1 representation.
    pub unsupported_summary_fields: bool,
    /// Coverage of deterministic terminal reporter summaries.
    pub summaries: TestEvidenceCoverage,
    /// Coverage of individual case status records.
    pub cases: TestEvidenceCoverage,
}

/// Counts explicitly printed by a supported reporter.
///
/// Missing categories stay `None`; this parser never turns an omitted category
/// into a claimed zero and never synthesizes a total.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReportedTestCounts {
    /// Explicit total count, when the reporter printed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// Explicit passed count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<u32>,
    /// Explicit failed count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<u32>,
    /// Explicit skipped or ignored count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<u32>,
    /// Explicit error count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<u32>,
}

impl ReportedTestCounts {
    fn is_empty(&self) -> bool {
        self.total.is_none()
            && self.passed.is_none()
            && self.failed.is_none()
            && self.skipped.is_none()
            && self.errors.is_none()
    }

    fn merge_explicit(&mut self, incoming: Self) -> Result<(), TestResultParseError> {
        merge_count(&mut self.total, incoming.total)?;
        merge_count(&mut self.passed, incoming.passed)?;
        merge_count(&mut self.failed, incoming.failed)?;
        merge_count(&mut self.skipped, incoming.skipped)?;
        merge_count(&mut self.errors, incoming.errors)?;
        Ok(())
    }
}

/// One explicitly reported test case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StructuredTestCase {
    /// Sanitized, bounded case name.
    pub name: String,
    /// Explicit reporter status.
    pub status: TestStatus,
}

/// One explicitly named suite or safe parser-owned fallback grouping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StructuredTestSuite {
    /// Sanitized, bounded suite name.
    pub name: String,
    /// Explicit suite status, when the reporter emitted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TestStatus>,
    /// Counts explicitly printed for this suite.
    pub reported: ReportedTestCounts,
    /// Bounded explicit case records.
    pub cases: Vec<StructuredTestCase>,
}

/// Safe structured facts produced from one tool result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StructuredTestResults {
    /// Semantic tool item that produced the parsed test output.
    pub origin_item_id: ItemId,
    /// Detected or caller-selected framework.
    pub framework: TestFramework,
    /// Exact parser revision.
    pub parser: TestResultParser,
    /// Terminal originating-tool facts frozen with the projection.
    pub command: TestCommandOutcome,
    /// Outcome safe for the completion-review surface.
    pub verification: TestVerificationOutcome,
    /// Explicit aggregate counts, when the framework printed them.
    pub reported: ReportedTestCounts,
    /// Explicit suite/file counts from reporters that print a second aggregate.
    pub reported_suites: ReportedTestCounts,
    /// Number of recognized terminal summary records.
    pub summary_count: u32,
    /// Bounded suites and cases.
    pub suites: Vec<StructuredTestSuite>,
    /// Evidence completeness and truncation facts.
    pub coverage: TestParseCoverage,
}

impl<'de> Deserialize<'de> for StructuredTestResults {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let stored = StoredStructuredTestResults::deserialize(deserializer)?;
        validate_stored_test_results(stored).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredTestParseCoverage {
    input_truncated: bool,
    records_truncated: bool,
    unsupported_summary_fields: bool,
    summaries: TestEvidenceCoverage,
    cases: TestEvidenceCoverage,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredReportedTestCounts {
    #[serde(default)]
    total: Option<u32>,
    #[serde(default)]
    passed: Option<u32>,
    #[serde(default)]
    failed: Option<u32>,
    #[serde(default)]
    skipped: Option<u32>,
    #[serde(default)]
    errors: Option<u32>,
}

impl From<StoredReportedTestCounts> for ReportedTestCounts {
    fn from(stored: StoredReportedTestCounts) -> Self {
        Self {
            total: stored.total,
            passed: stored.passed,
            failed: stored.failed,
            skipped: stored.skipped,
            errors: stored.errors,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredStructuredTestCase {
    name: String,
    status: TestStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredStructuredTestSuite {
    name: String,
    #[serde(default)]
    status: Option<TestStatus>,
    reported: StoredReportedTestCounts,
    cases: Vec<StoredStructuredTestCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredStructuredTestResults {
    origin_item_id: ItemId,
    framework: TestFramework,
    parser: TestResultParser,
    command: TestCommandOutcome,
    verification: TestVerificationOutcome,
    reported: StoredReportedTestCounts,
    reported_suites: StoredReportedTestCounts,
    summary_count: u32,
    suites: Vec<StoredStructuredTestSuite>,
    coverage: StoredTestParseCoverage,
}

/// Borrowed parser input.
///
/// This type intentionally does not implement `Debug`: diagnostic formatting
/// must not accidentally expose the raw test stream.
pub struct TestOutputInput<'a> {
    /// Semantic tool item that produced the output.
    pub origin_item_id: ItemId,
    /// Transport-bounded stdout/stderr bytes.
    pub output: &'a [u8],
    /// Whether the transport dropped bytes before this parser saw them.
    pub input_truncated: bool,
    /// Terminal status and process facts from the matching tool result.
    pub command: TestCommandOutcome,
    /// Optional authoritative framework selection from the command adapter.
    pub framework_hint: Option<TestFramework>,
}

/// Deterministic parse failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TestResultParseError {
    /// No bytes were supplied.
    #[error("test output is empty")]
    Empty,
    /// The transport did not enforce the parser's input limit.
    #[error("test output exceeds the parser input limit")]
    TooLarge,
    /// Structured facts could not fit the persisted-result boundary.
    #[error("structured test results exceed the persisted-result limit")]
    StructuredResultTooLarge,
    /// No supported deterministic reporter markers were found.
    #[error("test output is not a supported deterministic reporter format")]
    Unsupported,
    /// Multiple supported reporters produced equally strong markers.
    #[error("test output matches multiple supported reporter formats")]
    AmbiguousFramework,
    /// Explicit reporter claims conflict with each other.
    #[error("test output contains conflicting or impossible reporter claims")]
    AmbiguousClaims,
}

/// Checked persisted-result decode failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TestResultDecodeError {
    /// No persisted bytes were supplied.
    #[error("persisted test results are empty")]
    Empty,
    /// Persisted bytes exceed the bounded record limit.
    #[error("persisted test results exceed the record limit")]
    TooLarge,
    /// JSON, fields, labels, bounds, or derived claims are invalid.
    #[error("persisted test results failed validation")]
    Invalid,
}

/// Parses one bounded test output into safe, reviewable facts.
///
/// The caller owns storage and authentication for the raw output. This
/// function returns no raw lines or failure bodies. Adapters that already know
/// the invoked reporter should provide `framework_hint`; detection is a
/// fail-closed fallback for legacy or imported run records.
pub fn parse_test_output(
    input: TestOutputInput<'_>,
) -> Result<StructuredTestResults, TestResultParseError> {
    if input.output.is_empty() {
        return Err(TestResultParseError::Empty);
    }
    if input.output.len() > MAX_TEST_OUTPUT_BYTES {
        return Err(TestResultParseError::TooLarge);
    }

    let normalized = normalize_input(input.output);
    let framework = match input.framework_hint {
        Some(framework) => match detect_framework(&normalized.lines) {
            Ok(detected) if detected != framework => {
                return Err(TestResultParseError::AmbiguousFramework);
            }
            Err(TestResultParseError::AmbiguousFramework) => {
                return Err(TestResultParseError::AmbiguousFramework);
            }
            Ok(_) | Err(TestResultParseError::Unsupported) => framework,
            Err(error) => return Err(error),
        },
        None => detect_framework(&normalized.lines)?,
    };
    let parsed = match framework {
        TestFramework::CargoLibtest => parse_cargo(&normalized.lines)?,
        TestFramework::Vitest => parse_vitest(&normalized.lines)?,
        TestFramework::Jest => parse_jest(&normalized.lines)?,
        TestFramework::Pytest => parse_pytest(&normalized.lines)?,
        TestFramework::GoTest => parse_go_test(&normalized.lines)?,
    };
    if !parsed.recognized {
        return Err(TestResultParseError::Unsupported);
    }

    validate_claims(&parsed)?;
    validate_command_outcome(&parsed, input.command)?;
    let summaries = if parsed.summary_count == 0 {
        TestEvidenceCoverage::None
    } else {
        summary_coverage(
            framework,
            &parsed,
            input.input_truncated || normalized.lines_dropped,
        )
    };
    let cases = case_coverage(
        framework,
        &parsed,
        input.input_truncated || normalized.lines_dropped,
    );
    let verification = verification_outcome(
        framework,
        &parsed,
        input.command,
        input.input_truncated || normalized.lines_dropped,
    );

    let result = StructuredTestResults {
        origin_item_id: input.origin_item_id,
        framework,
        parser: framework.parser(),
        command: input.command,
        verification,
        reported: parsed.reported,
        reported_suites: parsed.reported_suites,
        summary_count: u32::try_from(parsed.summary_count)
            .expect("summary count is bounded below u32"),
        suites: parsed.suites,
        coverage: TestParseCoverage {
            input_truncated: input.input_truncated,
            records_truncated: parsed.records_truncated || normalized.lines_dropped,
            unsupported_summary_fields: parsed.unsupported_summary_fields,
            summaries,
            cases,
        },
    };
    if serde_json::to_vec(&result)
        .map_err(|_| TestResultParseError::StructuredResultTooLarge)?
        .len()
        > MAX_STRUCTURED_TEST_RESULTS_BYTES
    {
        return Err(TestResultParseError::StructuredResultTooLarge);
    }
    Ok(result)
}

/// Decodes one persisted structured result through all parser safety
/// invariants.
///
/// Replay callers should prefer this bounded loader. Nested protocol records
/// use the same checked conversion through `StructuredTestResults`' custom
/// `Deserialize` implementation.
pub fn decode_structured_test_results(
    bytes: &[u8],
) -> Result<StructuredTestResults, TestResultDecodeError> {
    if bytes.is_empty() {
        return Err(TestResultDecodeError::Empty);
    }
    if bytes.len() > MAX_STRUCTURED_TEST_RESULTS_BYTES {
        return Err(TestResultDecodeError::TooLarge);
    }
    serde_json::from_slice::<StructuredTestResults>(bytes)
        .map_err(|_| TestResultDecodeError::Invalid)
}

struct NormalizedInput {
    lines: Vec<String>,
    lines_dropped: bool,
}

fn normalize_input(output: &[u8]) -> NormalizedInput {
    let text = String::from_utf8_lossy(output);
    let mut lines = Vec::new();
    let mut lines_dropped = false;
    for line in text.replace('\r', "\n").split('\n') {
        if lines.len() >= MAX_TEST_OUTPUT_LINES {
            lines_dropped = true;
            break;
        }
        if line.len() > MAX_TEST_OUTPUT_LINE_BYTES {
            lines_dropped = true;
            continue;
        }
        let stripped = strip_terminal_sequences(line);
        let cleaned = stripped
            .chars()
            .map(|character| {
                if character == '\t' {
                    ' '
                } else if character.is_control() || is_directional_control(character) {
                    '\u{fffd}'
                } else {
                    character
                }
            })
            .collect::<String>();
        lines.push(cleaned);
    }
    NormalizedInput {
        lines,
        lines_dropped,
    }
}

fn strip_terminal_sequences(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut output = String::with_capacity(line.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0xc2 && index + 1 < bytes.len() {
            match bytes[index + 1] {
                0x9b => {
                    index += 2;
                    while index < bytes.len() {
                        let byte = bytes[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                    continue;
                }
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => {
                    let osc = bytes[index + 1] == 0x9d;
                    index += 2;
                    while index < bytes.len() {
                        if osc && bytes[index] == 0x07 {
                            index += 1;
                            break;
                        }
                        if bytes[index] == 0x1b
                            && index + 1 < bytes.len()
                            && bytes[index + 1] == b'\\'
                        {
                            index += 2;
                            break;
                        }
                        if bytes[index] == 0xc2
                            && index + 1 < bytes.len()
                            && bytes[index + 1] == 0x9c
                        {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                    continue;
                }
                _ => {}
            }
        }
        if bytes[index] != 0x1b {
            let character = line[index..].chars().next().expect("valid UTF-8 boundary");
            output.push(character);
            index += character.len_utf8();
            continue;
        }

        index += 1;
        if index >= bytes.len() {
            break;
        }
        match bytes[index] {
            b'[' => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'\\'
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            b'P' | b'_' | b'^' | b'X' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'\\'
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => {
                let character = line[index..].chars().next().expect("valid UTF-8 boundary");
                index += character.len_utf8();
            }
        }
    }
    output
}

fn is_directional_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn detect_framework(lines: &[String]) -> Result<TestFramework, TestResultParseError> {
    let mut candidates = Vec::new();
    if cargo_markers(lines) {
        candidates.push(TestFramework::CargoLibtest);
    }
    if vitest_markers(lines) {
        candidates.push(TestFramework::Vitest);
    }
    if jest_markers(lines) {
        candidates.push(TestFramework::Jest);
    }
    if pytest_markers(lines) {
        candidates.push(TestFramework::Pytest);
    }
    if go_test_markers(lines) {
        candidates.push(TestFramework::GoTest);
    }
    match candidates.as_slice() {
        [framework] => Ok(*framework),
        [] => Err(TestResultParseError::Unsupported),
        _ => Err(TestResultParseError::AmbiguousFramework),
    }
}

fn cargo_markers(lines: &[String]) -> bool {
    let summary = lines
        .iter()
        .any(|line| line.trim_start().starts_with("test result: "));
    let runner = lines.iter().any(|line| {
        let line = line.trim();
        line.starts_with("running ") && line.ends_with(" tests")
            || parse_cargo_case(line).is_some()
            || line.starts_with("Running unittests ")
            || line.starts_with("Doc-tests ")
    });
    summary && runner
}

fn vitest_markers(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|line| line.trim_start().starts_with("Test Files "))
        && lines.iter().any(|line| {
            let line = line.trim_start();
            line.starts_with("Tests ") && !line.starts_with("Tests:")
        })
}

fn jest_markers(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|line| line.trim_start().starts_with("Test Suites:"))
        && lines
            .iter()
            .any(|line| line.trim_start().starts_with("Tests:"))
}

fn pytest_markers(lines: &[String]) -> bool {
    let node_status = lines
        .iter()
        .any(|line| parse_pytest_case(line.trim()).is_some());
    let collected = lines.iter().any(|line| {
        let line = line.trim();
        line.starts_with("collected ") && (line.ends_with(" item") || line.ends_with(" items"))
    });
    let summary = lines
        .iter()
        .any(|line| parse_pytest_summary(line).is_some());
    node_status || (collected && summary)
}

fn go_test_markers(lines: &[String]) -> bool {
    let run = lines
        .iter()
        .any(|line| line.trim_start().starts_with("=== RUN   "));
    let terminal_case = lines.iter().any(|line| {
        let line = line.trim_start();
        line.starts_with("--- PASS:")
            || line.starts_with("--- FAIL:")
            || line.starts_with("--- SKIP:")
    });
    let package = lines.iter().any(|line| parse_go_package(line).is_some());
    (run && terminal_case) || package
}

#[derive(Default)]
struct ParsedResults {
    reported: ReportedTestCounts,
    reported_suites: ReportedTestCounts,
    suites: Vec<StructuredTestSuite>,
    summary_count: usize,
    records_truncated: bool,
    recognized: bool,
    unsupported_summary_fields: bool,
    retained_case_count: usize,
    retained_label_bytes: usize,
    suite_keys: BTreeMap<String, usize>,
    case_statuses: BTreeMap<(usize, String), TestStatus>,
}

impl ParsedResults {
    fn record_summary(&mut self) {
        if self.summary_count >= MAX_TEST_SUMMARIES {
            self.records_truncated = true;
        } else {
            self.summary_count += 1;
        }
    }

    fn suite_index(&mut self, identity: &str) -> Option<usize> {
        self.suite_index_with_display(identity, identity)
    }

    fn suite_index_with_display(&mut self, identity: &str, display: &str) -> Option<usize> {
        if let Some(index) = self.suite_keys.get(identity) {
            return Some(*index);
        }
        let display = sanitize_label(display);
        if display.is_empty() {
            self.records_truncated = true;
            return None;
        }
        if self.suites.len() >= MAX_TEST_SUITES {
            self.records_truncated = true;
            return None;
        }
        let name = unique_label(
            &display,
            self.suites.iter().map(|suite| suite.name.as_str()),
        );
        if self.retained_label_bytes.saturating_add(name.len()) > MAX_TEST_RESULT_LABEL_BYTES {
            self.records_truncated = true;
            return None;
        }
        self.retained_label_bytes += name.len();
        self.suites.push(StructuredTestSuite {
            name,
            status: None,
            reported: ReportedTestCounts::default(),
            cases: Vec::new(),
        });
        let index = self.suites.len() - 1;
        self.suite_keys.insert(identity.to_owned(), index);
        Some(index)
    }

    fn add_case(
        &mut self,
        suite_index: usize,
        name: &str,
        status: TestStatus,
    ) -> Result<(), TestResultParseError> {
        let identity = name;
        if let Some(existing) = self.case_statuses.get(&(suite_index, identity.to_owned())) {
            if *existing != status {
                return Err(TestResultParseError::AmbiguousClaims);
            }
            self.records_truncated = true;
            return Ok(());
        }
        let display = sanitize_label(identity);
        if display.is_empty() {
            self.records_truncated = true;
            return Ok(());
        }
        let name = unique_label(
            &display,
            self.suites[suite_index]
                .cases
                .iter()
                .map(|case| case.name.as_str()),
        );
        if self.retained_case_count >= MAX_TEST_CASES
            || self.suites[suite_index].cases.len() >= MAX_TEST_CASES_PER_SUITE
            || self.retained_label_bytes.saturating_add(name.len()) > MAX_TEST_RESULT_LABEL_BYTES
        {
            self.records_truncated = true;
            return Ok(());
        }
        self.case_statuses
            .insert((suite_index, identity.to_owned()), status);
        self.retained_case_count += 1;
        self.retained_label_bytes += name.len();
        self.suites[suite_index]
            .cases
            .push(StructuredTestCase { name, status });
        Ok(())
    }
}

fn parse_cargo(lines: &[String]) -> Result<ParsedResults, TestResultParseError> {
    let mut parsed = ParsedResults::default();
    let mut current_suite = None;
    for line in lines {
        let trimmed = line.trim();
        if let Some((identity, display)) = cargo_suite_heading(trimmed) {
            current_suite = parsed.suite_index_with_display(&identity, &display);
            parsed.recognized = true;
            continue;
        }
        if let Some((name, status)) = parse_cargo_case(trimmed) {
            let index = current_suite.or_else(|| parsed.suite_index("libtest"));
            if let Some(index) = index {
                parsed.add_case(index, name, status)?;
                current_suite = Some(index);
            }
            parsed.recognized = true;
            continue;
        }
        if let Some((status, counts, unsupported)) = parse_cargo_summary(trimmed)? {
            let index = current_suite.or_else(|| parsed.suite_index("libtest"));
            if let Some(index) = index {
                merge_status(&mut parsed.suites[index].status, status)?;
                parsed.suites[index].reported.merge_explicit(counts)?;
            }
            parsed.unsupported_summary_fields |= unsupported;
            parsed.record_summary();
            parsed.recognized = true;
            current_suite = None;
        }
    }
    Ok(parsed)
}

fn cargo_suite_heading(line: &str) -> Option<(String, String)> {
    if let Some(name) = line.strip_prefix("Doc-tests ") {
        return Some((
            line.to_owned(),
            format!("doc-tests {}", safe_relative_tail(name)),
        ));
    }
    let rest = line.strip_prefix("Running ")?;
    let (subject, executable) = rest.rsplit_once(" (")?;
    let executable = executable.strip_suffix(')')?;
    if !(subject.starts_with("unittests ")
        || subject.starts_with("tests/")
        || subject.starts_with("tests\\")
        || subject.starts_with("benches/")
        || subject.starts_with("benches\\")
        || subject.starts_with("examples/")
        || subject.starts_with("examples\\"))
    {
        return None;
    }
    let subject = if let Some(path) = subject.strip_prefix("unittests ") {
        format!("unittests {}", safe_relative_tail(path))
    } else {
        safe_relative_tail(subject)
    };
    let binary = cargo_executable_name(executable);
    let display = if binary.is_empty() {
        subject
    } else {
        format!("{subject} [{binary}]")
    };
    Some((line.to_owned(), display))
}

fn parse_cargo_case(line: &str) -> Option<(&str, TestStatus)> {
    let rest = line.strip_prefix("test ")?;
    for (suffix, status) in [
        (" ... ok", TestStatus::Passed),
        (" ... FAILED", TestStatus::Failed),
        (" ... ignored", TestStatus::Skipped),
    ] {
        if let Some(name) = rest.strip_suffix(suffix) {
            if !name.trim().is_empty() {
                return Some((name.trim(), status));
            }
        }
    }
    None
}

fn parse_cargo_summary(
    line: &str,
) -> Result<Option<(TestStatus, ReportedTestCounts, bool)>, TestResultParseError> {
    let Some(rest) = line.strip_prefix("test result: ") else {
        return Ok(None);
    };
    let (status, remainder) = if let Some(remainder) = rest.strip_prefix("ok.") {
        (TestStatus::Passed, remainder)
    } else if let Some(remainder) = rest.strip_prefix("FAILED.") {
        (TestStatus::Failed, remainder)
    } else {
        return Ok(None);
    };
    let counts = parse_reported_counts(
        remainder,
        &[
            ("passed", CountField::Passed),
            ("failed", CountField::Failed),
            ("ignored", CountField::Skipped),
        ],
    )?;
    if counts.passed.is_none() || counts.failed.is_none() || counts.skipped.is_none() {
        return Err(TestResultParseError::AmbiguousClaims);
    }
    let unsupported = has_nonzero_reported_category(remainder, &["measured", "filtered"])?;
    Ok(Some((status, counts, unsupported)))
}

fn parse_vitest(lines: &[String]) -> Result<ParsedResults, TestResultParseError> {
    let mut parsed = ParsedResults::default();
    let mut current_suite = None;
    for line in lines {
        let trimmed = line.trim();
        if let Some((name, status)) = parse_vitest_suite(trimmed) {
            if let Some(index) = parsed.suite_index(name) {
                merge_status(&mut parsed.suites[index].status, status)?;
                current_suite = Some(index);
            } else {
                current_suite = None;
            }
            parsed.recognized = true;
            continue;
        }
        if let Some((name, status)) = parse_glyph_case(trimmed, true) {
            let index = current_suite.or_else(|| parsed.suite_index("vitest"));
            if let Some(index) = index {
                parsed.add_case(index, &name, status)?;
                current_suite = Some(index);
            }
            parsed.recognized = true;
            continue;
        }
        if trimmed.starts_with("Test Files ") {
            let (counts, unsupported) = parse_javascript_summary(trimmed, false)?;
            parsed.reported_suites.merge_explicit(counts)?;
            parsed.unsupported_summary_fields |= unsupported;
            parsed.record_summary();
            parsed.recognized = true;
            continue;
        }
        if trimmed.starts_with("Tests ") && !trimmed.starts_with("Tests:") {
            let (counts, unsupported) = parse_javascript_summary(trimmed, false)?;
            parsed.reported.merge_explicit(counts)?;
            parsed.unsupported_summary_fields |= unsupported;
            parsed.record_summary();
            parsed.recognized = true;
        }
    }
    Ok(parsed)
}

fn parse_vitest_suite(line: &str) -> Option<(&str, TestStatus)> {
    let (rest, status) = strip_status_glyph(line)?;
    let rest = strip_trailing_bare_duration(rest);
    let open = rest.rfind('(')?;
    let parenthetical = rest.get(open + 1..rest.len().checked_sub(1)?)?;
    if !rest.ends_with(')') || !(parenthetical.contains("test") || parenthetical.contains("tests"))
    {
        return None;
    }
    let name = rest[..open].trim();
    (!name.is_empty()).then_some((name, status))
}

fn parse_jest(lines: &[String]) -> Result<ParsedResults, TestResultParseError> {
    let mut parsed = ParsedResults::default();
    let mut current_suite = None;
    for line in lines {
        let trimmed = line.trim();
        if let Some((name, status)) = parse_jest_suite(trimmed) {
            if let Some(index) = parsed.suite_index(name) {
                merge_status(&mut parsed.suites[index].status, status)?;
                current_suite = Some(index);
            } else {
                current_suite = None;
            }
            parsed.recognized = true;
            continue;
        }
        if let Some((name, status)) = parse_glyph_case(trimmed, false) {
            let index = current_suite.or_else(|| parsed.suite_index("jest"));
            if let Some(index) = index {
                parsed.add_case(index, &name, status)?;
                current_suite = Some(index);
            }
            parsed.recognized = true;
            continue;
        }
        if let Some(summary) = trimmed.strip_prefix("Test Suites:") {
            let (counts, unsupported) = parse_javascript_summary(summary, true)?;
            parsed.reported_suites.merge_explicit(counts)?;
            parsed.unsupported_summary_fields |= unsupported;
            parsed.record_summary();
            parsed.recognized = true;
            continue;
        }
        if let Some(summary) = trimmed.strip_prefix("Tests:") {
            let (counts, unsupported) = parse_javascript_summary(summary, true)?;
            parsed.reported.merge_explicit(counts)?;
            parsed.unsupported_summary_fields |= unsupported;
            parsed.record_summary();
            parsed.recognized = true;
        }
    }
    Ok(parsed)
}

fn parse_jest_suite(line: &str) -> Option<(&str, TestStatus)> {
    if let Some(name) = line.strip_prefix("PASS ") {
        let name = strip_trailing_duration(name);
        return (!name.is_empty()).then_some((name, TestStatus::Passed));
    }
    let name = strip_trailing_duration(line.strip_prefix("FAIL ")?);
    (!name.is_empty()).then_some((name, TestStatus::Failed))
}

fn parse_glyph_case(line: &str, reject_suite_parenthetical: bool) -> Option<(String, TestStatus)> {
    let (name, status) = strip_status_glyph(line)?;
    if reject_suite_parenthetical {
        if let Some(open) = name.rfind('(') {
            let parenthetical = &name[open..];
            if parenthetical.contains(" test") {
                return None;
            }
        }
    }
    let name = strip_trailing_duration(name);
    if name.is_empty() {
        None
    } else {
        Some((name.to_owned(), status))
    }
}

fn strip_status_glyph(line: &str) -> Option<(&str, TestStatus)> {
    for (prefix, status) in [
        ("✓ ", TestStatus::Passed),
        ("√ ", TestStatus::Passed),
        ("× ", TestStatus::Failed),
        ("✕ ", TestStatus::Failed),
        ("✗ ", TestStatus::Failed),
        ("↓ ", TestStatus::Skipped),
        ("○ ", TestStatus::Skipped),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((rest.trim(), status));
        }
    }
    if let Some(rest) = line.strip_prefix("❯ ") {
        return Some((rest.trim(), TestStatus::Failed));
    }
    None
}

fn strip_trailing_duration(value: &str) -> &str {
    let value = value.trim();
    let Some(open) = value.rfind(" (") else {
        return value;
    };
    let Some(duration) = value.get(open + 2..value.len().saturating_sub(1)) else {
        return value;
    };
    let numeric = duration
        .strip_suffix("ms")
        .or_else(|| duration.strip_suffix('s'))
        .map(str::trim);
    if value.ends_with(')')
        && numeric.is_some_and(|numeric| {
            !numeric.is_empty()
                && numeric
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.')
        })
    {
        value[..open].trim()
    } else {
        value
    }
}

fn strip_trailing_bare_duration(value: &str) -> &str {
    let value = value.trim();
    let Some((prefix, token)) = value.rsplit_once(char::is_whitespace) else {
        return value;
    };
    let digits = token.strip_suffix("ms").or_else(|| token.strip_suffix('s'));
    if digits.is_some_and(|digits| {
        !digits.is_empty()
            && digits
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
    }) {
        prefix.trim()
    } else {
        value
    }
}

fn parse_pytest(lines: &[String]) -> Result<ParsedResults, TestResultParseError> {
    let mut parsed = ParsedResults::default();
    let mut suite_indexes = BTreeMap::new();
    for line in lines {
        let trimmed = line.trim();
        if let Some((node, status)) = parse_pytest_case(trimmed) {
            let (suite_name, case_name) = split_pytest_node(node);
            let index = if let Some(index) = suite_indexes.get(&suite_name) {
                *index
            } else {
                let Some(index) = parsed.suite_index(&suite_name) else {
                    parsed.recognized = true;
                    continue;
                };
                suite_indexes.insert(suite_name, index);
                index
            };
            parsed.add_case(index, &case_name, status)?;
            parsed.recognized = true;
            continue;
        }
        if let Some(counts) = parse_pytest_summary(trimmed) {
            let (counts, unsupported) = counts?;
            if counts.is_empty() {
                continue;
            }
            parsed.reported.merge_explicit(counts)?;
            parsed.unsupported_summary_fields |= unsupported;
            parsed.record_summary();
            parsed.recognized = true;
        }
    }
    if parsed.suites.is_empty() && !parsed.reported.is_empty() {
        let _ = parsed.suite_index("pytest");
    }
    Ok(parsed)
}

fn parse_pytest_case(line: &str) -> Option<(&str, TestStatus)> {
    for (marker, status) in [
        (" PASSED", TestStatus::Passed),
        (" FAILED", TestStatus::Failed),
        (" SKIPPED", TestStatus::Skipped),
        (" ERROR", TestStatus::Error),
    ] {
        if let Some((node, tail)) = line.rsplit_once(marker) {
            let tail = tail.trim_start();
            if node.contains("::")
                && !node.trim().is_empty()
                && (tail.is_empty() || tail.starts_with('[') || tail.starts_with('('))
            {
                return Some((node, status));
            }
        }
    }
    None
}

fn split_pytest_node(node: &str) -> (String, String) {
    let (suite, case) = node
        .split_once("::")
        .map_or(("pytest", node), |(suite, case)| (suite, case));
    (suite.to_owned(), case.to_owned())
}

fn parse_pytest_summary(
    line: &str,
) -> Option<Result<(ReportedTestCounts, bool), TestResultParseError>> {
    let stripped = line.trim_matches('=').trim();
    if !stripped.contains(" in ")
        || !(stripped.contains(" passed")
            || stripped.contains(" failed")
            || stripped.contains(" skipped")
            || stripped.contains(" error"))
    {
        return None;
    }
    Some(
        parse_reported_counts(
            stripped,
            &[
                ("passed", CountField::Passed),
                ("failed", CountField::Failed),
                ("skipped", CountField::Skipped),
                ("error", CountField::Errors),
                ("errors", CountField::Errors),
            ],
        )
        .and_then(|counts| {
            has_nonzero_reported_category(
                stripped,
                &["xfailed", "xpassed", "deselected", "warning", "warnings"],
            )
            .map(|unsupported| (counts, unsupported))
        }),
    )
}

fn parse_go_test(lines: &[String]) -> Result<ParsedResults, TestResultParseError> {
    let mut parsed = ParsedResults::default();
    let mut pending_order = Vec::new();
    let mut pending_statuses = BTreeMap::new();
    let mut pending_label_bytes = 0usize;
    let mut seen_run_names = BTreeSet::new();
    for line in lines {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("=== RUN   ") {
            let identity = name.trim().to_owned();
            let display = sanitize_label(&identity);
            if !display.is_empty() {
                if seen_run_names.len() >= MAX_TEST_CASES_PER_SUITE
                    || parsed
                        .retained_label_bytes
                        .saturating_add(pending_label_bytes)
                        .saturating_add(display.len())
                        > MAX_TEST_RESULT_LABEL_BYTES
                {
                    parsed.records_truncated = true;
                } else {
                    seen_run_names.insert(identity);
                }
                parsed.recognized = true;
            }
            continue;
        }
        if let Some((name, status)) = parse_go_case(trimmed) {
            let identity = name.trim().to_owned();
            let display = sanitize_label(&identity);
            if !seen_run_names.contains(&identity) {
                if parsed.records_truncated {
                    continue;
                }
                return Err(TestResultParseError::AmbiguousClaims);
            }
            if let Some(existing) = pending_statuses.get(&identity) {
                if *existing != status {
                    return Err(TestResultParseError::AmbiguousClaims);
                }
                parsed.records_truncated = true;
            } else {
                if parsed
                    .retained_case_count
                    .saturating_add(pending_order.len())
                    >= MAX_TEST_CASES
                    || pending_order.len() >= MAX_TEST_CASES_PER_SUITE
                    || parsed
                        .retained_label_bytes
                        .saturating_add(pending_label_bytes)
                        .saturating_add(display.len())
                        > MAX_TEST_RESULT_LABEL_BYTES
                {
                    parsed.records_truncated = true;
                    continue;
                }
                pending_label_bytes += display.len();
                pending_order.push(identity.clone());
                pending_statuses.insert(identity, status);
            }
            parsed.recognized = true;
            continue;
        }
        if let Some((package, status)) = parse_go_package(trimmed) {
            if seen_run_names
                .iter()
                .any(|name| !pending_statuses.contains_key(name))
            {
                parsed.records_truncated = true;
            }
            if let Some(index) = parsed.suite_index(package) {
                merge_status(&mut parsed.suites[index].status, status)?;
                for name in pending_order.drain(..) {
                    let case_status = pending_statuses
                        .remove(&name)
                        .expect("pending order and statuses stay synchronized");
                    parsed.add_case(index, &name, case_status)?;
                }
            } else {
                pending_order.clear();
                pending_statuses.clear();
            }
            pending_label_bytes = 0;
            seen_run_names.clear();
            parsed.record_summary();
            parsed.recognized = true;
        }
    }
    if !pending_order.is_empty() {
        if let Some(index) = parsed.suite_index("go test") {
            for name in pending_order {
                let status = pending_statuses
                    .remove(&name)
                    .expect("pending order and statuses stay synchronized");
                parsed.add_case(index, &name, status)?;
            }
        }
    }
    Ok(parsed)
}

fn parse_go_case(line: &str) -> Option<(&str, TestStatus)> {
    for (prefix, status) in [
        ("--- PASS: ", TestStatus::Passed),
        ("--- FAIL: ", TestStatus::Failed),
        ("--- SKIP: ", TestStatus::Skipped),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((rest.split(" (").next().unwrap_or(rest).trim(), status));
        }
    }
    None
}

fn parse_go_package(line: &str) -> Option<(&str, TestStatus)> {
    if let Some(rest) = line.strip_prefix("ok ") {
        return rest
            .split_whitespace()
            .next()
            .filter(|name| !name.is_empty())
            .map(|name| (name, TestStatus::Passed));
    }
    if let Some(rest) = line.strip_prefix("FAIL ") {
        return rest
            .split_whitespace()
            .next()
            .filter(|name| !name.is_empty())
            .map(|name| (name, TestStatus::Failed));
    }
    if let Some(rest) = line.strip_prefix("? ") {
        if rest.contains("[no test files]") {
            return rest
                .split_whitespace()
                .next()
                .filter(|name| !name.is_empty())
                .map(|name| (name, TestStatus::Skipped));
        }
    }
    None
}

#[derive(Clone, Copy)]
enum CountField {
    Total,
    Passed,
    Failed,
    Skipped,
    Errors,
}

fn parse_javascript_summary(
    line: &str,
    explicit_total_word: bool,
) -> Result<(ReportedTestCounts, bool), TestResultParseError> {
    let mut counts = parse_reported_counts(
        line,
        &[
            ("passed", CountField::Passed),
            ("failed", CountField::Failed),
            ("skipped", CountField::Skipped),
            ("error", CountField::Errors),
            ("errors", CountField::Errors),
            ("total", CountField::Total),
        ],
    )?;
    if !explicit_total_word {
        merge_count(&mut counts.total, parse_parenthesized_total(line)?)?;
    }
    if counts.is_empty() {
        return Err(TestResultParseError::AmbiguousClaims);
    }
    let unsupported = has_nonzero_reported_category(line, &["todo"])?;
    Ok((counts, unsupported))
}

fn parse_reported_counts(
    line: &str,
    words: &[(&str, CountField)],
) -> Result<ReportedTestCounts, TestResultParseError> {
    let normalized = line
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    let mut counts = ReportedTestCounts::default();
    for pair in tokens.windows(2) {
        let Ok(value) = pair[0].parse::<u32>() else {
            continue;
        };
        if value > MAX_REPORTED_TESTS {
            return Err(TestResultParseError::AmbiguousClaims);
        }
        let Some((_, field)) = words.iter().find(|(word, _)| *word == pair[1]) else {
            continue;
        };
        let target = match field {
            CountField::Total => &mut counts.total,
            CountField::Passed => &mut counts.passed,
            CountField::Failed => &mut counts.failed,
            CountField::Skipped => &mut counts.skipped,
            CountField::Errors => &mut counts.errors,
        };
        merge_count(target, Some(value))?;
    }
    Ok(counts)
}

fn has_nonzero_reported_category(line: &str, words: &[&str]) -> Result<bool, TestResultParseError> {
    let normalized = line
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    for pair in tokens.windows(2) {
        if !words.contains(&pair[1]) {
            continue;
        }
        let value = pair[0]
            .parse::<u32>()
            .map_err(|_| TestResultParseError::AmbiguousClaims)?;
        if value > MAX_REPORTED_TESTS {
            return Err(TestResultParseError::AmbiguousClaims);
        }
        if value > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_parenthesized_total(line: &str) -> Result<Option<u32>, TestResultParseError> {
    let Some(open) = line.rfind('(') else {
        return Ok(None);
    };
    let Some(value) = line
        .get(open + 1..line.len().saturating_sub(1))
        .filter(|_| line.ends_with(')'))
    else {
        return Ok(None);
    };
    if !value.chars().all(|character| character.is_ascii_digit()) {
        return Ok(None);
    }
    let value = value
        .parse::<u32>()
        .map_err(|_| TestResultParseError::AmbiguousClaims)?;
    if value > MAX_REPORTED_TESTS {
        return Err(TestResultParseError::AmbiguousClaims);
    }
    Ok(Some(value))
}

fn merge_count(
    current: &mut Option<u32>,
    incoming: Option<u32>,
) -> Result<(), TestResultParseError> {
    let Some(incoming) = incoming else {
        return Ok(());
    };
    match current {
        Some(existing) if *existing != incoming => Err(TestResultParseError::AmbiguousClaims),
        Some(_) => Ok(()),
        None => {
            *current = Some(incoming);
            Ok(())
        }
    }
}

fn merge_status(
    current: &mut Option<TestStatus>,
    incoming: TestStatus,
) -> Result<(), TestResultParseError> {
    match current {
        Some(existing) if *existing != incoming => Err(TestResultParseError::AmbiguousClaims),
        Some(_) => Ok(()),
        None => {
            *current = Some(incoming);
            Ok(())
        }
    }
}

fn validate_stored_test_results(
    stored: StoredStructuredTestResults,
) -> Result<StructuredTestResults, TestResultParseError> {
    if stored.parser != stored.framework.parser()
        || stored.summary_count as usize > MAX_TEST_SUMMARIES
        || stored.suites.len() > MAX_TEST_SUITES
    {
        return Err(TestResultParseError::AmbiguousClaims);
    }

    let reported = ReportedTestCounts::from(stored.reported);
    let reported_suites = ReportedTestCounts::from(stored.reported_suites);
    validate_stored_counts(&reported)?;
    validate_stored_counts(&reported_suites)?;

    let mut suite_names = BTreeSet::new();
    let mut total_cases = 0usize;
    let mut label_bytes = 0usize;
    let mut suites = Vec::with_capacity(stored.suites.len());
    for stored_suite in stored.suites {
        if !valid_stored_label(&stored_suite.name) || !suite_names.insert(stored_suite.name.clone())
        {
            return Err(TestResultParseError::AmbiguousClaims);
        }
        label_bytes = label_bytes.saturating_add(stored_suite.name.len());
        if stored_suite.cases.len() > MAX_TEST_CASES_PER_SUITE {
            return Err(TestResultParseError::AmbiguousClaims);
        }
        let suite_reported = ReportedTestCounts::from(stored_suite.reported);
        validate_stored_counts(&suite_reported)?;
        let mut case_names = BTreeSet::new();
        let mut cases = Vec::with_capacity(stored_suite.cases.len());
        for stored_case in stored_suite.cases {
            if !valid_stored_label(&stored_case.name)
                || !case_names.insert(stored_case.name.clone())
            {
                return Err(TestResultParseError::AmbiguousClaims);
            }
            label_bytes = label_bytes.saturating_add(stored_case.name.len());
            total_cases = total_cases.saturating_add(1);
            if total_cases > MAX_TEST_CASES || label_bytes > MAX_TEST_RESULT_LABEL_BYTES {
                return Err(TestResultParseError::AmbiguousClaims);
            }
            cases.push(StructuredTestCase {
                name: stored_case.name,
                status: stored_case.status,
            });
        }
        suites.push(StructuredTestSuite {
            name: stored_suite.name,
            status: stored_suite.status,
            reported: suite_reported,
            cases,
        });
    }
    if label_bytes > MAX_TEST_RESULT_LABEL_BYTES {
        return Err(TestResultParseError::AmbiguousClaims);
    }

    let coverage = TestParseCoverage {
        input_truncated: stored.coverage.input_truncated,
        records_truncated: stored.coverage.records_truncated,
        unsupported_summary_fields: stored.coverage.unsupported_summary_fields,
        summaries: stored.coverage.summaries,
        cases: stored.coverage.cases,
    };
    let parsed = ParsedResults {
        reported: reported.clone(),
        reported_suites: reported_suites.clone(),
        suites: suites.clone(),
        summary_count: stored.summary_count as usize,
        records_truncated: coverage.records_truncated,
        recognized: true,
        unsupported_summary_fields: coverage.unsupported_summary_fields,
        retained_case_count: total_cases,
        retained_label_bytes: label_bytes,
        suite_keys: BTreeMap::new(),
        case_statuses: BTreeMap::new(),
    };
    validate_claims(&parsed)?;
    validate_command_outcome(&parsed, stored.command)?;

    let expected_summaries = if parsed.summary_count == 0 {
        TestEvidenceCoverage::None
    } else {
        summary_coverage(stored.framework, &parsed, coverage.input_truncated)
    };
    let expected_cases = case_coverage(stored.framework, &parsed, coverage.input_truncated);
    let expected_verification = verification_outcome(
        stored.framework,
        &parsed,
        stored.command,
        coverage.input_truncated,
    );
    if coverage.summaries != expected_summaries
        || coverage.cases != expected_cases
        || stored.verification != expected_verification
    {
        return Err(TestResultParseError::AmbiguousClaims);
    }

    Ok(StructuredTestResults {
        origin_item_id: stored.origin_item_id,
        framework: stored.framework,
        parser: stored.parser,
        command: stored.command,
        verification: stored.verification,
        reported,
        reported_suites,
        summary_count: stored.summary_count,
        suites,
        coverage,
    })
}

fn validate_stored_counts(counts: &ReportedTestCounts) -> Result<(), TestResultParseError> {
    if [
        counts.total,
        counts.passed,
        counts.failed,
        counts.skipped,
        counts.errors,
    ]
    .into_iter()
    .flatten()
    .any(|count| count > MAX_REPORTED_TESTS)
    {
        return Err(TestResultParseError::AmbiguousClaims);
    }
    validate_count_total(counts)
}

fn valid_stored_label(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_TEST_LABEL_BYTES && sanitize_label(value) == value
}

fn validate_claims(parsed: &ParsedResults) -> Result<(), TestResultParseError> {
    validate_count_claims(
        &parsed.reported,
        parsed.suites.iter().flat_map(|suite| &suite.cases),
    )?;
    validate_suite_count_claims(&parsed.reported_suites, &parsed.suites)?;
    for suite in &parsed.suites {
        validate_count_claims(&suite.reported, suite.cases.iter())?;
        if suite.status == Some(TestStatus::Passed)
            && (suite.reported.failed.is_some_and(|count| count > 0)
                || suite.reported.errors.is_some_and(|count| count > 0)
                || suite
                    .cases
                    .iter()
                    .any(|case| matches!(case.status, TestStatus::Failed | TestStatus::Error)))
        {
            return Err(TestResultParseError::AmbiguousClaims);
        }
        if suite.status == Some(TestStatus::Skipped)
            && suite
                .cases
                .iter()
                .any(|case| case.status != TestStatus::Skipped)
        {
            return Err(TestResultParseError::AmbiguousClaims);
        }
        if let Some(total) = suite.reported.total {
            let known = suite.reported.passed.unwrap_or(0)
                + suite.reported.failed.unwrap_or(0)
                + suite.reported.skipped.unwrap_or(0)
                + suite.reported.errors.unwrap_or(0);
            if known > total {
                return Err(TestResultParseError::AmbiguousClaims);
            }
        }
    }
    validate_count_total(&parsed.reported)?;
    validate_count_total(&parsed.reported_suites)?;
    Ok(())
}

fn validate_command_outcome(
    parsed: &ParsedResults,
    command: TestCommandOutcome,
) -> Result<(), TestResultParseError> {
    if command.status != TestCommandStatus::Succeeded {
        return Ok(());
    }
    if command.exit_code.is_some_and(|code| code != 0)
        || command.signal.is_some()
        || has_failure_evidence(parsed)
    {
        return Err(TestResultParseError::AmbiguousClaims);
    }
    Ok(())
}

fn verification_outcome(
    framework: TestFramework,
    parsed: &ParsedResults,
    command: TestCommandOutcome,
    input_incomplete: bool,
) -> TestVerificationOutcome {
    match command.status {
        TestCommandStatus::Failed => TestVerificationOutcome::Failed,
        TestCommandStatus::Stopped => TestVerificationOutcome::Stopped,
        TestCommandStatus::Succeeded
            if input_incomplete
                || parsed.records_truncated
                || parsed.unsupported_summary_fields
                || parsed.summary_count == 0
                || !has_pass_evidence(framework, parsed) =>
        {
            TestVerificationOutcome::Inconclusive
        }
        TestCommandStatus::Succeeded => TestVerificationOutcome::Passed,
    }
}

fn has_failure_evidence(parsed: &ParsedResults) -> bool {
    parsed.reported.failed.is_some_and(|count| count > 0)
        || parsed.reported.errors.is_some_and(|count| count > 0)
        || parsed.reported_suites.failed.is_some_and(|count| count > 0)
        || parsed.reported_suites.errors.is_some_and(|count| count > 0)
        || parsed.suites.iter().any(|suite| {
            suite.status == Some(TestStatus::Failed)
                || suite.reported.failed.is_some_and(|count| count > 0)
                || suite.reported.errors.is_some_and(|count| count > 0)
                || suite
                    .cases
                    .iter()
                    .any(|case| matches!(case.status, TestStatus::Failed | TestStatus::Error))
        })
}

fn has_pass_evidence(framework: TestFramework, parsed: &ParsedResults) -> bool {
    parsed.reported.passed.is_some_and(|count| count > 0)
        || parsed.suites.iter().any(|suite| {
            suite.reported.passed.is_some_and(|count| count > 0)
                || suite
                    .cases
                    .iter()
                    .any(|case| case.status == TestStatus::Passed)
                || (framework == TestFramework::GoTest && suite.status == Some(TestStatus::Passed))
        })
}

fn validate_count_claims<'a>(
    counts: &ReportedTestCounts,
    cases: impl Iterator<Item = &'a StructuredTestCase>,
) -> Result<(), TestResultParseError> {
    let mut observed = BTreeMap::new();
    for case in cases {
        *observed.entry(case.status).or_insert(0_u32) += 1;
    }
    for (reported, status) in [
        (counts.passed, TestStatus::Passed),
        (counts.failed, TestStatus::Failed),
        (counts.skipped, TestStatus::Skipped),
        (counts.errors, TestStatus::Error),
    ] {
        if reported.is_some_and(|count| count < observed.get(&status).copied().unwrap_or(0)) {
            return Err(TestResultParseError::AmbiguousClaims);
        }
    }
    if counts
        .total
        .is_some_and(|total| total < observed.values().copied().sum())
    {
        return Err(TestResultParseError::AmbiguousClaims);
    }
    if counts.total == Some(observed.values().copied().sum())
        && explicit_known_count(counts) == observed.values().copied().sum::<u32>() as usize
    {
        for (reported, status) in [
            (counts.passed, TestStatus::Passed),
            (counts.failed, TestStatus::Failed),
            (counts.skipped, TestStatus::Skipped),
            (counts.errors, TestStatus::Error),
        ] {
            if reported.unwrap_or(0) != observed.get(&status).copied().unwrap_or(0) {
                return Err(TestResultParseError::AmbiguousClaims);
            }
        }
    }
    Ok(())
}

fn validate_suite_count_claims(
    counts: &ReportedTestCounts,
    suites: &[StructuredTestSuite],
) -> Result<(), TestResultParseError> {
    let mut observed = BTreeMap::new();
    for status in suites.iter().filter_map(|suite| suite.status) {
        *observed.entry(status).or_insert(0_u32) += 1;
    }
    for (reported, status) in [
        (counts.passed, TestStatus::Passed),
        (counts.failed, TestStatus::Failed),
        (counts.skipped, TestStatus::Skipped),
        (counts.errors, TestStatus::Error),
    ] {
        if counts.total == u32::try_from(suites.len()).ok()
            && explicit_known_count(counts) == suites.len()
            && reported.unwrap_or(0) != observed.get(&status).copied().unwrap_or(0)
        {
            return Err(TestResultParseError::AmbiguousClaims);
        }
        if reported.is_some_and(|count| count < observed.get(&status).copied().unwrap_or(0)) {
            return Err(TestResultParseError::AmbiguousClaims);
        }
    }
    if counts
        .total
        .is_some_and(|total| total < u32::try_from(suites.len()).unwrap_or(u32::MAX))
    {
        return Err(TestResultParseError::AmbiguousClaims);
    }
    validate_count_total(counts)
}

fn validate_count_total(counts: &ReportedTestCounts) -> Result<(), TestResultParseError> {
    if let Some(total) = counts.total {
        let known = counts.passed.unwrap_or(0)
            + counts.failed.unwrap_or(0)
            + counts.skipped.unwrap_or(0)
            + counts.errors.unwrap_or(0);
        if known > total {
            return Err(TestResultParseError::AmbiguousClaims);
        }
    }
    Ok(())
}

fn summary_coverage(
    framework: TestFramework,
    parsed: &ParsedResults,
    input_incomplete: bool,
) -> TestEvidenceCoverage {
    if parsed.summary_count == 0 {
        return TestEvidenceCoverage::None;
    }
    if input_incomplete || parsed.records_truncated || parsed.unsupported_summary_fields {
        return TestEvidenceCoverage::Partial;
    }
    let complete = match framework {
        TestFramework::CargoLibtest => {
            parsed.summary_count == parsed.suites.len()
                && parsed.suites.iter().all(|suite| {
                    suite.status.is_some()
                        && suite.reported.passed.is_some()
                        && suite.reported.failed.is_some()
                        && suite.reported.skipped.is_some()
                })
        }
        TestFramework::Vitest | TestFramework::Jest => {
            parsed.summary_count >= 2
                && !parsed.reported.is_empty()
                && !parsed.reported_suites.is_empty()
        }
        TestFramework::Pytest => !parsed.reported.is_empty(),
        TestFramework::GoTest => {
            parsed.summary_count == parsed.suites.len()
                && parsed.suites.iter().all(|suite| suite.status.is_some())
        }
    };
    if complete {
        TestEvidenceCoverage::Complete
    } else {
        TestEvidenceCoverage::Partial
    }
}

fn case_coverage(
    framework: TestFramework,
    parsed: &ParsedResults,
    truncated: bool,
) -> TestEvidenceCoverage {
    let case_count = parsed
        .suites
        .iter()
        .map(|suite| suite.cases.len())
        .sum::<usize>();
    if case_count == 0 {
        return TestEvidenceCoverage::None;
    }
    if truncated || parsed.records_truncated || parsed.unsupported_summary_fields {
        return TestEvidenceCoverage::Partial;
    }
    match framework {
        TestFramework::CargoLibtest => {
            if parsed.suites.iter().all(|suite| {
                suite.reported.passed.is_some()
                    && suite.reported.failed.is_some()
                    && suite.reported.skipped.is_some()
                    && explicit_known_count(&suite.reported) == suite.cases.len()
            }) {
                TestEvidenceCoverage::Complete
            } else {
                TestEvidenceCoverage::Partial
            }
        }
        TestFramework::GoTest => {
            if parsed.summary_count == parsed.suites.len()
                && parsed.suites.iter().all(|suite| suite.status.is_some())
            {
                TestEvidenceCoverage::Complete
            } else {
                TestEvidenceCoverage::Partial
            }
        }
        TestFramework::Vitest | TestFramework::Jest | TestFramework::Pytest => {
            if parsed.reported.total == u32::try_from(case_count).ok() {
                TestEvidenceCoverage::Complete
            } else {
                TestEvidenceCoverage::Partial
            }
        }
    }
}

fn explicit_known_count(counts: &ReportedTestCounts) -> usize {
    [counts.passed, counts.failed, counts.skipped, counts.errors]
        .into_iter()
        .flatten()
        .map(|value| value as usize)
        .sum()
}

fn unique_label<'a>(base: &str, existing: impl Iterator<Item = &'a str>) -> String {
    let existing = existing.collect::<Vec<_>>();
    if !existing.contains(&base) {
        return base.to_owned();
    }
    for ordinal in 2_u32.. {
        let suffix = format!(" #{ordinal}");
        let prefix = truncate_utf8(base, MAX_TEST_LABEL_BYTES.saturating_sub(suffix.len()));
        let candidate = format!("{prefix}{suffix}");
        if !existing.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("an unbounded ordinal always provides a unique bounded label")
}

fn sanitize_label(value: &str) -> String {
    let value = redact_paths(value);
    let value = redact_secrets(&value);
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_control() || is_directional_control(character) {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect::<String>();
    truncate_utf8(cleaned.trim(), MAX_TEST_LABEL_BYTES)
}

fn safe_relative_tail(value: &str) -> String {
    let trimmed = value.trim();
    if looks_like_absolute_path(trimmed) {
        return redact_path_token(trimmed);
    }
    trimmed.to_owned()
}

fn cargo_executable_name(value: &str) -> String {
    let basename = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .strip_suffix(".exe")
        .unwrap_or_else(|| value.rsplit(['/', '\\']).next().unwrap_or(value));
    let stable = basename
        .rsplit_once('-')
        .filter(|(_, suffix)| {
            suffix.len() >= 8 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(|(prefix, _)| prefix)
        .unwrap_or(basename);
    sanitize_label(stable)
}

fn redact_paths(value: &str) -> String {
    let Some(path_start) = find_absolute_path_start(value) else {
        return value.to_owned();
    };
    let candidate = &value[path_start..];
    if let Some(suffix_index) = candidate.find("::") {
        let path = &candidate[..suffix_index];
        let suffix = &candidate[suffix_index..];
        let basename = if path.chars().any(char::is_whitespace) {
            ""
        } else {
            path.trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("")
        };
        if basename.is_empty() {
            format!("{}<path>{suffix}", &value[..path_start])
        } else {
            format!("{}<path>/{basename}{suffix}", &value[..path_start])
        }
    } else if candidate.chars().any(char::is_whitespace) {
        format!("{}<path>", &value[..path_start])
    } else {
        redact_path_token(value)
    }
}

fn find_absolute_path_start(value: &str) -> Option<usize> {
    value.char_indices().find_map(|(index, _)| {
        let candidate = &value[index..];
        let allowed_prefix = index == 0
            || value[..index].chars().next_back().is_some_and(|character| {
                character.is_whitespace()
                    || matches!(character, '(' | '[' | '{' | '"' | '\'' | '=' | ':' | ',')
            });
        (allowed_prefix && looks_like_absolute_path(candidate)).then_some(index)
    })
}

fn redact_path_token(token: &str) -> String {
    let mut path_start = None;
    for (index, _) in token.char_indices() {
        let candidate = &token[index..];
        let allowed_prefix = index == 0
            || token[..index].chars().next_back().is_some_and(|character| {
                character.is_whitespace()
                    || matches!(character, '(' | '[' | '{' | '"' | '\'' | '=' | ':' | ',')
            });
        if allowed_prefix && looks_like_absolute_path(candidate) {
            path_start = Some(index);
            break;
        }
    }
    let Some(path_start) = path_start else {
        return token.to_owned();
    };
    let candidate = &token[path_start..];
    let suffix_index = candidate.find("::").unwrap_or(candidate.len());
    let path = &candidate[..suffix_index];
    let suffix = &candidate[suffix_index..];
    let basename = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");
    if basename.is_empty() {
        format!("{}<path>{suffix}", &token[..path_start])
    } else {
        format!("{}<path>/{basename}{suffix}", &token[..path_start])
    }
}

fn looks_like_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('\\')
        || value.starts_with("~/")
        || value.starts_with("file:///")
        || (value.len() >= 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'\\' | b'/'))
}

fn redact_secrets(value: &str) -> String {
    if let Some(delimiter) = sensitive_assignment_delimiter(value) {
        return format!("{}<redacted>", &value[..=delimiter]);
    }
    let lower = value.to_ascii_lowercase();
    if let Some(index) = ["bearer ", "basic ", "password ", "secret "]
        .into_iter()
        .filter_map(|needle| lower.find(needle))
        .min()
    {
        return format!("{}<redacted>", &value[..index]);
    }

    let mut output = Vec::new();
    for token in value.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if lower == "bearer"
            || lower == "bearer:"
            || lower == "authorization:"
            || lower == "authorization"
        {
            output.push(format!("{token}<redacted>"));
            break;
        }
        if looks_like_secret_token(token) {
            output.push("<redacted>".to_owned());
        } else {
            output.push(token.to_owned());
        }
    }
    output.join(" ")
}

fn sensitive_assignment_delimiter(value: &str) -> Option<usize> {
    let lower = value.to_ascii_lowercase();
    let mut earliest = None;
    for key in [
        "authorization",
        "aws_secret_access_key",
        "access-token",
        "access_token",
        "api-key",
        "auth_token",
        "client_secret",
        "credential",
        "cookie",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "secret",
        "session_token",
        "token",
    ] {
        let mut offset = 0;
        while let Some(relative) = lower[offset..].find(key) {
            let start = offset + relative;
            let end = start + key.len();
            let boundary_before =
                start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
            let mut separator_index = end;
            while lower
                .as_bytes()
                .get(separator_index)
                .is_some_and(u8::is_ascii_whitespace)
            {
                separator_index += 1;
            }
            let separator = lower.as_bytes().get(separator_index).copied();
            if boundary_before && matches!(separator, Some(b'=') | Some(b':')) {
                earliest = Some(
                    earliest.map_or(separator_index, |value: usize| value.min(separator_index)),
                );
                break;
            }
            offset = end;
        }
    }
    earliest
}

fn looks_like_secret_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(character, ',' | ';' | ')' | ']' | '}' | '"' | '\'')
    });
    let lower = trimmed.to_ascii_lowercase();
    let known_prefix = [
        "akia",
        "asia",
        "aiza",
        "dop_v1_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "hf_",
        "npm_",
        "pypi-",
        "sk-",
        "sk_live_",
        "rk_live_",
        "xoxb-",
        "xoxa-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "xoxapp-",
        "ya29.",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix) || lower.contains(prefix));
    (lower.len() >= 12 && known_prefix)
        || (trimmed.len() >= 24 && trimmed.starts_with("eyJ") && trimmed.matches('.').count() == 2)
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let marker = "…";
    let mut end = maximum_bytes.saturating_sub(marker.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = value[..end].to_owned();
    truncated.push_str(marker);
    truncated
}
