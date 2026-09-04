# ygg-migrate-types

`ygg-migrate-types` is the versioned, dependency-light wire schema shared by
migration adapters, importers, backups, and paired benchmark reports.

## Validated wire boundary

`MigratedSetup`, `CompareReportHeader`, and `CompareReport` are **validated
wire values**, not generic Serde inputs. They intentionally do not implement
`Deserialize`: `serde_json::Value` collapses duplicate object names before a
schema can inspect them, so neither `serde_json::from_value` nor a generic
`serde_json::from_str` can establish the wire contract.

Decode a wire artifact only from its original raw JSON text:

```rust
use ygg_migrate_types::CompareReport;

let json_text = r#"{"header":{"schema_version":1,"versions":{},"hardware":{}},"tasks":[]}"#;
let report = CompareReport::from_json(json_text)?;
let canonical = report.to_canonical_json()?;
# Ok::<(), ygg_migrate_types::ValidationError>(())
```

The three `from_json` methods first run a bounded token-preserving preflight,
probe the version from that raw stream, then strictly decode private,
unvalidated DTOs. They never decode through `serde_json::Value` or `RawValue`.
Validated values can also be assembled with checked typed constructors and
`push_*` methods. Public fields are private and accessors are read-only, so a
value that can render or canonicalize has already passed validation. To
round-trip `serde_json::to_string(&value)`, feed that JSON text back to the
matching `from_json` method.

## Limits

All limits are public constants and apply before a decoded value crosses the
validated boundary. Typed constructors and mutators reapply the applicable
payload limits; canonical JSON and Markdown revalidate before rendering.

| Limit | Value | Scope |
| --- | ---: | --- |
| `MAX_JSON_INPUT_BYTES` | 1,048,576 bytes | Each raw `from_json` input, before parsing/copying values |
| `MAX_JSON_NESTING` | 32 | Object/array nesting; root container is depth 1 |
| `MAX_STRING_BYTES` | 16,384 bytes | Every decoded JSON string and every typed string input |
| `MAX_MAP_ENTRIES` | 128 | Every raw JSON object and metadata map |
| `MAX_LIST_ENTRIES` | 128 | Every raw JSON array |
| `MAX_TOTAL_JSON_STRING_BYTES` | 131,072 bytes | All raw JSON strings, including object names and unknown fields |
| `MAX_TOTAL_JSON_ENTRIES` | 16,384 | All raw object members and array elements |
| `MAX_TOTAL_DECODED_STRING_BYTES` | 65,536 bytes | Dynamic payload strings in one validated value |
| `MAX_TOTAL_DECODED_RECORDS` | 256 | Setup outcomes plus setup diagnostics, or report task rows |
| `MAX_TOTAL_DECODED_COLLECTION_ENTRIES` | 16,384 | Dynamic map/list entries across one typed value, including nested stdio arguments |
| `MAX_MODELS`, `MAX_SKILLS`, `MAX_MCP_SERVERS`, `MAX_PERMISSIONS`, `MAX_DIAGNOSTICS`, `MAX_TASKS` | 128 each | Respective category/list counts |
| `MAX_MCP_ARGUMENTS` | 128 | Stdio transport arguments |
| `MAX_RENDERED_MARKDOWN_BYTES` | 1,048,576 bytes | Rendered comparison Markdown |

Canonical JSON must also fit `MAX_JSON_INPUT_BYTES`, including its required
trailing newline.

Raw preflight is a bounded visitor rather than a `RawValue` capture: it checks
input size first, counts nesting/collections before deserializing their child
values, and never copies an entire report merely to inspect it. Strict DTO
conversion moves decoded strings into validated values and applies aggregate
payload/record limits without cloning the report. Canonical report JSON and
Markdown sort a vector of task references, not cloned task rows.

## Migrated setup v1

`MigratedSetup` serializes these required top-level fields, in order:

```text
schema_version, source_agent, models, skills, mcp_servers, permissions, diagnostics
```

Each category contains `MigrationOutcome<T>` rather than bare values:

- `mapped` records a bounded source path and its checked v1 target value.
- `unmapped` records a checked `Diagnostic { path, severity, reason }` in that
  same category.

An adapter therefore has a schema-level representation for every input item;
it cannot express an unsupported item as an unobservable absence. The top-level
`diagnostics` list is for setup-level findings that do not belong to one model,
skill, MCP server, or permission.

Diagnostics must be actionable. A `reason` must contain at least one Unicode
scalar that is neither whitespace nor `Default_Ignorable_Code_Point`; this
prevents a whitespace- or invisible-only reason while preserving literal visible
international text. A visible reason may retain non-bidi default-ignorable
scalars such as a variation selector; values are never normalized or rewritten.
Both fields reject C0/C1 controls, line/paragraph separators, and bidirectional
formatting controls.

A `path` must be the explicit root sentinel `ROOT_DIAGNOSTIC_PATH` (`"$"`) or a
normalized, trimmed source-relative path: it uses nonempty `/`-separated
segments, has no `.` or `..` segment or backslash, and contains no
`Default_Ignorable_Code_Point` scalar at all. The v1 bounded scalar table is
`00AD`, `034F`, `061C`, `115F–1160`, `17B4–17B5`, `180B–180F`, `200B–200F`,
`202A–202E`, `2060–206F`, `3164`, `FE00–FE0F`, `FEFF`, `FFA0`, `FFF0–FFF8`,
`1BCA0–1BCA3`, `1D173–1D17A`, and `E0000–E0FFF`. It includes zero-width,
word-joiner, BOM, tag, variation-selector, and bidi-format scalars without a
runtime Unicode database dependency. These checks apply to typed constructors,
mutators, raw decoding, and canonical revalidation.

MCP declarations intentionally contain only non-secret stdio or HTTP connection
data. Environment variables, headers, credentials, and other secret-bearing
source fields are outside v1 and must become unmapped diagnostics.

## Comparison report v1

`CompareReport` is JSON-first. Its header is
`{ schema_version, versions, hardware }`, where `versions` and `hardware` are
deterministic string maps. `tasks` is a top-level sibling of `header`; each row
contains `task_id`, `agent`, `wall_clock` (milliseconds), `peak_rss_bytes`,
`tokens_in`, `tokens_out`, and `success`.

The four task metrics are JSON integer tokens in the inclusive range
`0..=MAX_PORTABLE_JSON_INTEGER` (`(1 << 53) - 1`). Fractions, exponents,
negative forms, and larger integers are rejected on raw decode; checked
`CompareTaskRow::new` enforces the same maximum. This is intentionally stricter
than Rust `u64`: JavaScript and many cross-language JSON implementations use
IEEE-754 binary64 and can represent every integer through 2^53 - 1 exactly,
whereas larger integers can round silently. Decimal-string metrics are not used
in v1, so consumers get one portable numeric domain.

`versions` and `hardware` reject duplicate decoded keys, including
escape-equivalent JSON names. A duplicate key is detected before the strict map
visitor asks for its duplicate value, so a wrong-typed duplicate value cannot
mask the duplicate-key error. Valid map entries are retained in `BTreeMap`
order. `CompareReport::to_canonical_json()` also sorts task references by their
complete row values; source task order remains available through `tasks()`.

Use `CompareReport::to_canonical_json()` to write the canonical artifact. It
uses fixed struct-field order, lexicographic map order, sorted task rows, and
one trailing newline. Render a human view with
`render_compare_report_markdown(json)`; it validates and decodes the raw JSON
first, so Markdown is not a second source of report data. Report strings render
as literal text: HTML and Markdown syntax are escaped, terminal controls and
bidirectional characters are made visible, and only trusted line-break markup
is emitted.

## Strictness and schema-version behavior

Every v1 envelope, comparison header/report, nested mapped value, diagnostic,
tagged outcome, and tagged transport rejects unknown and duplicate fields.
Metadata maps are the deliberate exception to fixed field names, but still
reject duplicate decoded keys. This strictness is applied from raw JSON tokens,
so duplicate keys cannot be silently collapsed before validation.

V1 readers first perform their bounded raw version probe and reject all versions
other than `1`. A newer document therefore reports a version mismatch even if
it carries bounded additive fields or wrong-typed sibling fields; once v1 is
selected, normal strict field validation applies. Readers do not infer
compatibility, downgrade a newer document, or silently accept a version
mismatch. For example:

```text
unsupported MigratedSetup schema version 2; expected 1
```

A producer that changes the wire contract must publish and select a new schema
version. Adapters must record unsupported source material through an explicit
`unmapped` diagnostic rather than relying on an older reader to drop it during
parsing or canonical serialization.
