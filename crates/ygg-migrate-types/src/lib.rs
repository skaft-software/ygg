#![deny(missing_docs)]

//! Versioned, bounded wire schemas for migrated setups and paired comparisons.
//!
//! The migration schema records both successful mappings and unmapped source
//! items. The comparison schema is JSON-first: Markdown is rendered only from a
//! validated JSON report rather than maintained as a second report format.
//!
//! # Validated decoding boundary
//!
//! [`MigratedSetup`], [`CompareReportHeader`], and [`CompareReport`] deliberately
//! do **not** implement [`serde::Deserialize`]. A `serde_json::Value` has already
//! collapsed duplicate object names, so `serde_json::from_value` cannot prove
//! that a wire document was duplicate-free. Decode wire artifacts only through
//! their [`from_json`](MigratedSetup::from_json) methods, which inspect the raw
//! JSON token stream with bounded visitors before strict schema decoding.
//!
//! Checked typed constructors are the other way to make a validated value. All
//! fields are private, and constructors, mutators, canonicalization, and
//! rendering re-apply the documented bounds. To round-trip a serialized value,
//! call the matching `from_json` method on its canonical JSON; do not use a
//! generic Serde deserializer or a `serde_json::Value` conversion.
//!
//! ```compile_fail
//! use ygg_migrate_types::CompareReport;
//!
//! let value = serde_json::json!({"header": {}, "tasks": []});
//! let _: CompareReport = serde_json::from_value(value).unwrap();
//! ```
//!
//! ```compile_fail
//! use ygg_migrate_types::MigratedSetup;
//!
//! let value = serde_json::json!({});
//! let _: MigratedSetup = serde_json::from_value(value).unwrap();
//! ```
//!
//! ```compile_fail
//! use ygg_migrate_types::CompareReportHeader;
//!
//! let value = serde_json::json!({});
//! let _: CompareReportHeader = serde_json::from_value(value).unwrap();
//! ```
//!
//! # Schema versions
//!
//! V1 readers reject every unsupported schema version. Readers first perform a
//! bounded raw-token version probe, so a version mismatch wins over additive or
//! wrong-typed sibling fields in an otherwise bounded JSON document. Readers do
//! not guess an upgrade or downgrade; callers must explicitly migrate a newer
//! or older document before consuming it.

use std::collections::{btree_map::Entry, BTreeMap};
use std::fmt::{self, Write as _};
use std::marker::PhantomData;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, Error as _, IgnoredAny, MapAccess,
    SeqAccess, Visitor,
};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

/// The only `MigratedSetup` schema version supported by this crate release.
pub const MIGRATED_SETUP_SCHEMA_VERSION: u32 = 1;

/// The only comparison-report schema version supported by this crate release.
pub const COMPARE_REPORT_SCHEMA_VERSION: u32 = 1;

/// Maximum raw UTF-8 JSON input size accepted by any `from_json` entry point.
pub const MAX_JSON_INPUT_BYTES: usize = 1_048_576;

/// Maximum object/array nesting depth accepted by raw JSON entry points.
///
/// A root object or array has depth one. Scalars do not increase depth.
pub const MAX_JSON_NESTING: usize = 32;

/// Maximum UTF-8 byte length of one decoded JSON string or typed string input.
pub const MAX_STRING_BYTES: usize = 16_384;

/// Maximum members in any JSON object, including objects not recognized by v1.
pub const MAX_MAP_ENTRIES: usize = 128;

/// Maximum elements in any JSON array, including arrays not recognized by v1.
pub const MAX_LIST_ENTRIES: usize = 128;

/// Maximum total decoded UTF-8 string bytes inspected during raw JSON preflight.
///
/// This includes JSON object names, including fixed wire field names, so it
/// bounds hostile unknown material before strict schema validation.
pub const MAX_TOTAL_JSON_STRING_BYTES: usize = 131_072;

/// Maximum total object members and array elements inspected during raw JSON
/// preflight.
pub const MAX_TOTAL_JSON_ENTRIES: usize = 16_384;

/// Maximum decoded payload-string bytes in one validated schema value.
///
/// This counts dynamic payload strings (for example, metadata names and values,
/// source paths, task names, and skill content), but not fixed wire field names.
pub const MAX_TOTAL_DECODED_STRING_BYTES: usize = 65_536;

/// Maximum aggregate source-item records in one validated schema value.
///
/// For a migrated setup, this is the sum of category outcomes and setup-level
/// diagnostics. An unmapped outcome already counts as its source-item record;
/// its nested diagnostic does not count a second time. For a comparison report,
/// this is the number of task rows.
pub const MAX_TOTAL_DECODED_RECORDS: usize = 256;

/// Maximum aggregate dynamic collection entries in one validated schema value.
///
/// This counts category outcomes, setup diagnostics, task rows, metadata-map
/// members, and stdio arguments, but excludes fixed schema-field members.
pub const MAX_TOTAL_DECODED_COLLECTION_ENTRIES: usize = MAX_TOTAL_JSON_ENTRIES;

/// Maximum model outcomes in a migrated setup.
pub const MAX_MODELS: usize = MAX_LIST_ENTRIES;

/// Maximum skill outcomes in a migrated setup.
pub const MAX_SKILLS: usize = MAX_LIST_ENTRIES;

/// Maximum MCP-server outcomes in a migrated setup.
pub const MAX_MCP_SERVERS: usize = MAX_LIST_ENTRIES;

/// Maximum permission outcomes in a migrated setup.
pub const MAX_PERMISSIONS: usize = MAX_LIST_ENTRIES;

/// Maximum diagnostics in a migrated setup, including nested unmapped ones.
pub const MAX_DIAGNOSTICS: usize = MAX_LIST_ENTRIES;

/// Maximum command arguments in a stdio MCP transport.
pub const MAX_MCP_ARGUMENTS: usize = MAX_LIST_ENTRIES;

/// Maximum task rows in a comparison report.
pub const MAX_TASKS: usize = MAX_LIST_ENTRIES;

/// Largest exactly portable JSON integer: `(1 << 53) - 1`.
///
/// JavaScript and many JSON consumers store numbers in IEEE-754 binary64. Every
/// integer through this bound is represented exactly in those consumers, while
/// larger `u64` values can silently round. All four comparison task metrics use
/// this domain instead of accepting arbitrary Rust `u64` values.
pub const MAX_PORTABLE_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

/// Maximum bytes emitted by [`CompareReport::to_markdown`].
///
/// The decoded-string limit keeps valid input well below this output limit even
/// when every rendered character needs escaping.
pub const MAX_RENDERED_MARKDOWN_BYTES: usize = MAX_JSON_INPUT_BYTES;

/// Explicit normalized path denoting a setup-level diagnostic.
///
/// Any other diagnostic path is a trimmed source-relative path with nonempty
/// slash-separated segments, no `.` or `..` segment, no backslash, no C0/C1,
/// line/paragraph separator, or bidirectional control, and no Unicode
/// `Default_Ignorable_Code_Point` scalar. The v1 check is scalar-based and does
/// not normalize or rewrite the path.
pub const ROOT_DIAGNOSTIC_PATH: &str = "$";

/// An unsupported schema version encountered while decoding a document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaVersionMismatch {
    /// The schema-bearing document that rejected the version.
    pub document: &'static str,
    /// The exact portable integer supplied by the input document.
    pub found: u64,
    /// The version supported by this crate release.
    pub expected: u32,
}

impl fmt::Display for SchemaVersionMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsupported {} schema version {}; expected {}",
            self.document, self.found, self.expected
        )
    }
}

impl std::error::Error for SchemaVersionMismatch {}

/// A decoding, resource-limit, or typed-construction validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    message: String,
}

impl ValidationError {
    /// Returns the stable, human-readable validation failure message.
    pub fn message(&self) -> &str {
        &self.message
    }

    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn from_serde(error: serde_json::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

/// A result returned by validated schema constructors and wire entry points.
pub type Result<T> = std::result::Result<T, ValidationError>;

/// Severity assigned to a migration diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// The source item was retained but needs user review before it can be used.
    Warning,
    /// The source item cannot be used without an explicit correction or port.
    Error,
}

/// An actionable migration diagnostic for a normalized source path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    path: String,
    severity: DiagnosticSeverity,
    reason: String,
}

impl Diagnostic {
    /// Creates an actionable diagnostic.
    ///
    /// `path` must be [`ROOT_DIAGNOSTIC_PATH`] or a normalized source-relative
    /// path with no default-ignorable scalar. `reason` must contain at least one
    /// scalar that is neither Unicode whitespace nor default-ignorable. C0/C1,
    /// line/paragraph separators, and bidirectional controls are rejected in
    /// both fields. Validation does not normalize or rewrite either string, so
    /// visible international text and non-bidi variation selectors in a visible
    /// reason remain literal.
    pub fn new(
        path: impl Into<String>,
        severity: DiagnosticSeverity,
        reason: impl Into<String>,
    ) -> Result<Self> {
        let diagnostic = Self {
            path: path.into(),
            severity,
            reason: reason.into(),
        };
        let mut usage = ResourceUsage::default();
        diagnostic.validate_with_usage(&mut usage)?;
        Ok(diagnostic)
    }

    /// Returns the normalized source path or [`ROOT_DIAGNOSTIC_PATH`].
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the diagnostic severity.
    pub const fn severity(&self) -> DiagnosticSeverity {
        self.severity
    }

    /// Returns the actionable explanation.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        usage.take_diagnostic()?;
        validate_normalized_path(&self.path, "diagnostic path", usage)?;
        validate_diagnostic_reason(&self.reason, usage)
    }
}

/// The explicit outcome for one source item in a migrated setup.
///
/// Each source item belongs in one category list as either a successful mapping
/// or an unmapped diagnostic. Consumers therefore cannot mistake an omitted item
/// for a mapped one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationOutcome<T> {
    inner: MigrationOutcomeInner<T>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MigrationOutcomeInner<T> {
    Mapped { path: String, value: T },
    Unmapped { diagnostic: Diagnostic },
}

impl<T> MigrationOutcome<T> {
    /// Returns the mapped source path and value, if this outcome was mapped.
    pub fn as_mapped(&self) -> Option<(&str, &T)> {
        match &self.inner {
            MigrationOutcomeInner::Mapped { path, value } => Some((path, value)),
            MigrationOutcomeInner::Unmapped { .. } => None,
        }
    }

    /// Returns the unmapped diagnostic, if this outcome was not mapped.
    pub fn diagnostic(&self) -> Option<&Diagnostic> {
        match &self.inner {
            MigrationOutcomeInner::Mapped { .. } => None,
            MigrationOutcomeInner::Unmapped { diagnostic } => Some(diagnostic),
        }
    }

    /// Returns whether this is a successful mapping.
    pub const fn is_mapped(&self) -> bool {
        matches!(self.inner, MigrationOutcomeInner::Mapped { .. })
    }

    /// Returns whether this is an explicit unmapped diagnostic.
    pub const fn is_unmapped(&self) -> bool {
        matches!(self.inner, MigrationOutcomeInner::Unmapped { .. })
    }

    /// Creates a bounded mapped outcome.
    ///
    /// The mapped source path is bounded here. A [`MigratedSetup`] performs the
    /// concrete target and aggregate validation when it accepts the outcome.
    pub fn mapped(path: impl Into<String>, value: T) -> Result<Self> {
        let path = path.into();
        let mut usage = ResourceUsage::default();
        validate_source_path(&path, &mut usage)?;
        Ok(Self::mapped_unchecked(path, value))
    }

    /// Creates a checked unmapped outcome.
    pub fn unmapped(diagnostic: Diagnostic) -> Result<Self> {
        let mut usage = ResourceUsage::default();
        diagnostic.validate_with_usage(&mut usage)?;
        Ok(Self::unmapped_unchecked(diagnostic))
    }

    fn mapped_unchecked(path: String, value: T) -> Self {
        Self {
            inner: MigrationOutcomeInner::Mapped { path, value },
        }
    }

    fn unmapped_unchecked(diagnostic: Diagnostic) -> Self {
        Self {
            inner: MigrationOutcomeInner::Unmapped { diagnostic },
        }
    }
}

impl<T> Serialize for MigrationOutcome<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.inner {
            MigrationOutcomeInner::Mapped { path, value } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("outcome", "mapped")?;
                map.serialize_entry("path", path)?;
                map.serialize_entry("value", value)?;
                map.end()
            }
            MigrationOutcomeInner::Unmapped { diagnostic } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("outcome", "unmapped")?;
                map.serialize_entry("diagnostic", diagnostic)?;
                map.end()
            }
        }
    }
}

/// A model selection that can be imported without provider credentials.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Model {
    provider: String,
    model: String,
}

impl Model {
    /// Creates a bounded source-neutral provider and model selection.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let model = Self {
            provider: provider.into(),
            model: model.into(),
        };
        let mut usage = ResourceUsage::default();
        model.validate_with_usage(&mut usage)?;
        Ok(model)
    }

    /// Returns the source-neutral provider identifier.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the provider's model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        usage.take_string("model provider", &self.provider)?;
        usage.take_string("model identifier", &self.model)
    }
}

/// A portable skill's target name and Markdown instructions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Skill {
    name: String,
    content: String,
}

impl Skill {
    /// Creates a bounded skill name and Markdown content payload.
    pub fn new(name: impl Into<String>, content: impl Into<String>) -> Result<Self> {
        let skill = Self {
            name: name.into(),
            content: content.into(),
        };
        let mut usage = ResourceUsage::default();
        skill.validate_with_usage(&mut usage)?;
        Ok(skill)
    }

    /// Returns the target skill name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the portable Markdown instruction content.
    pub fn content(&self) -> &str {
        &self.content
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        usage.take_string("skill name", &self.name)?;
        usage.take_string("skill content", &self.content)
    }
}

/// The non-secret transport kind used by an MCP server declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpTransportKind {
    /// A local stdio command.
    Stdio,
    /// A remote HTTP endpoint.
    Http,
}

/// A non-secret MCP connection transport.
///
/// Stdio declarations are data only and must not be started merely by handling
/// them. HTTP declarations are data only and must not be contacted merely by
/// handling them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpTransport {
    inner: McpTransportInner,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum McpTransportInner {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
}

impl McpTransport {
    /// Creates a bounded local stdio command declaration.
    pub fn stdio(command: impl Into<String>, args: Vec<String>) -> Result<Self> {
        let transport = Self {
            inner: McpTransportInner::Stdio {
                command: command.into(),
                args,
            },
        };
        let mut usage = ResourceUsage::default();
        transport.validate_with_usage(&mut usage)?;
        Ok(transport)
    }

    /// Creates a bounded remote HTTP endpoint declaration.
    pub fn http(url: impl Into<String>) -> Result<Self> {
        let transport = Self {
            inner: McpTransportInner::Http { url: url.into() },
        };
        let mut usage = ResourceUsage::default();
        transport.validate_with_usage(&mut usage)?;
        Ok(transport)
    }

    /// Returns this transport's kind.
    pub const fn kind(&self) -> McpTransportKind {
        match self.inner {
            McpTransportInner::Stdio { .. } => McpTransportKind::Stdio,
            McpTransportInner::Http { .. } => McpTransportKind::Http,
        }
    }

    /// Returns the stdio command when this is a stdio transport.
    pub fn command(&self) -> Option<&str> {
        match &self.inner {
            McpTransportInner::Stdio { command, .. } => Some(command),
            McpTransportInner::Http { .. } => None,
        }
    }

    /// Returns the stdio command arguments when this is a stdio transport.
    pub fn args(&self) -> Option<&[String]> {
        match &self.inner {
            McpTransportInner::Stdio { args, .. } => Some(args),
            McpTransportInner::Http { .. } => None,
        }
    }

    /// Returns the HTTP URL when this is an HTTP transport.
    pub fn url(&self) -> Option<&str> {
        match &self.inner {
            McpTransportInner::Stdio { .. } => None,
            McpTransportInner::Http { url } => Some(url),
        }
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        match &self.inner {
            McpTransportInner::Stdio { command, args } => {
                validate_collection_count(args.len(), MAX_MCP_ARGUMENTS, "MCP arguments")?;
                usage.take_string("MCP command", command)?;
                for argument in args {
                    usage.take_collection_entry("MCP argument")?;
                    usage.take_string("MCP argument", argument)?;
                }
                Ok(())
            }
            McpTransportInner::Http { url } => usage.take_string("MCP HTTP URL", url),
        }
    }
}

impl Serialize for McpTransport {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.inner.serialize(serializer)
    }
}

/// An MCP server declaration retained as non-secret data only.
///
/// This schema does not carry environment variables, headers, or credentials.
/// Adapters must report those source fields as unmapped diagnostics instead of
/// serializing secrets into a setup artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct McpServer {
    name: String,
    transport: McpTransport,
}

impl McpServer {
    /// Creates a bounded named non-secret MCP server declaration.
    pub fn new(name: impl Into<String>, transport: McpTransport) -> Result<Self> {
        let server = Self {
            name: name.into(),
            transport,
        };
        let mut usage = ResourceUsage::default();
        server.validate_with_usage(&mut usage)?;
        Ok(server)
    }

    /// Returns the user-visible server name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the non-secret transport declaration.
    pub fn transport(&self) -> &McpTransport {
        &self.transport
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        usage.take_string("MCP server name", &self.name)?;
        self.transport.validate_with_usage(usage)
    }
}

/// A migrated capability decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Permission {
    capability: String,
    decision: PermissionDecision,
}

impl Permission {
    /// Creates a bounded source-neutral capability decision.
    pub fn new(capability: impl Into<String>, decision: PermissionDecision) -> Result<Self> {
        let permission = Self {
            capability: capability.into(),
            decision,
        };
        let mut usage = ResourceUsage::default();
        permission.validate_with_usage(&mut usage)?;
        Ok(permission)
    }

    /// Returns the source-neutral capability name.
    pub fn capability(&self) -> &str {
        &self.capability
    }

    /// Returns the decision that applies to the capability.
    pub const fn decision(&self) -> PermissionDecision {
        self.decision
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        usage.take_string("permission capability", &self.capability)
    }
}

/// A source-neutral permission decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow the capability without another prompt.
    Allow,
    /// Require a user decision when the capability is requested.
    Ask,
    /// Deny the capability.
    Deny,
}

/// A v1 migrated setup envelope.
///
/// Category lists hold [`MigrationOutcome`] values rather than bare target
/// values. This makes an unmapped source item part of the schema instead of an
/// implicit, silently dropped conversion result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MigratedSetup {
    schema_version: u32,
    source_agent: String,
    models: Vec<MigrationOutcome<Model>>,
    skills: Vec<MigrationOutcome<Skill>>,
    mcp_servers: Vec<MigrationOutcome<McpServer>>,
    permissions: Vec<MigrationOutcome<Permission>>,
    diagnostics: Vec<Diagnostic>,
}

impl MigratedSetup {
    /// Creates an empty checked v1 envelope for `source_agent`.
    pub fn new(source_agent: impl Into<String>) -> Result<Self> {
        Self::with_parts(
            source_agent,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    /// Creates a checked v1 envelope from its typed category lists.
    pub fn with_parts(
        source_agent: impl Into<String>,
        models: Vec<MigrationOutcome<Model>>,
        skills: Vec<MigrationOutcome<Skill>>,
        mcp_servers: Vec<MigrationOutcome<McpServer>>,
        permissions: Vec<MigrationOutcome<Permission>>,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<Self> {
        let setup = Self {
            schema_version: MIGRATED_SETUP_SCHEMA_VERSION,
            source_agent: source_agent.into(),
            models,
            skills,
            mcp_servers,
            permissions,
            diagnostics,
        };
        setup.validate()?;
        Ok(setup)
    }

    /// Decodes raw v1 JSON without passing through `serde_json::Value`.
    ///
    /// This is the only wire-decoding route for a validated `MigratedSetup`.
    /// It applies raw input, nesting, string, map/list, and aggregate preflight
    /// limits before strict duplicate and unknown-field validation.
    pub fn from_json(json: &str) -> Result<Self> {
        preflight_json(json)?;
        ensure_supported_version(
            probe_setup_version(json)?,
            "MigratedSetup",
            MIGRATED_SETUP_SCHEMA_VERSION,
        )?;
        decode_raw::<RawMigratedSetup>(json)?.into_validated()
    }

    /// Returns this envelope's schema version.
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Returns the source agent or setup format that produced this envelope.
    pub fn source_agent(&self) -> &str {
        &self.source_agent
    }

    /// Returns model-selection outcomes in source precedence order.
    pub fn models(&self) -> &[MigrationOutcome<Model>] {
        &self.models
    }

    /// Returns skill-conversion outcomes in source precedence order.
    pub fn skills(&self) -> &[MigrationOutcome<Skill>] {
        &self.skills
    }

    /// Returns MCP-server outcomes in source precedence order.
    pub fn mcp_servers(&self) -> &[MigrationOutcome<McpServer>] {
        &self.mcp_servers
    }

    /// Returns permission-conversion outcomes in source precedence order.
    pub fn permissions(&self) -> &[MigrationOutcome<Permission>] {
        &self.permissions
    }

    /// Returns setup-level diagnostics in source precedence order.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Appends a checked model outcome after enforcing all aggregate limits.
    pub fn push_model(&mut self, outcome: MigrationOutcome<Model>) -> Result<()> {
        ensure_can_append(self.models.len(), MAX_MODELS, "models")?;
        let mut usage = self.resource_usage()?;
        usage.take_collection_entry("model")?;
        usage.take_record("model")?;
        validate_model_outcome(&outcome, &mut usage)?;
        self.models.push(outcome);
        Ok(())
    }

    /// Appends a checked skill outcome after enforcing all aggregate limits.
    pub fn push_skill(&mut self, outcome: MigrationOutcome<Skill>) -> Result<()> {
        ensure_can_append(self.skills.len(), MAX_SKILLS, "skills")?;
        let mut usage = self.resource_usage()?;
        usage.take_collection_entry("skill")?;
        usage.take_record("skill")?;
        validate_skill_outcome(&outcome, &mut usage)?;
        self.skills.push(outcome);
        Ok(())
    }

    /// Appends a checked MCP-server outcome after enforcing all aggregate limits.
    pub fn push_mcp_server(&mut self, outcome: MigrationOutcome<McpServer>) -> Result<()> {
        ensure_can_append(self.mcp_servers.len(), MAX_MCP_SERVERS, "MCP servers")?;
        let mut usage = self.resource_usage()?;
        usage.take_collection_entry("MCP server")?;
        usage.take_record("MCP server")?;
        validate_mcp_server_outcome(&outcome, &mut usage)?;
        self.mcp_servers.push(outcome);
        Ok(())
    }

    /// Appends a checked permission outcome after enforcing all aggregate limits.
    pub fn push_permission(&mut self, outcome: MigrationOutcome<Permission>) -> Result<()> {
        ensure_can_append(self.permissions.len(), MAX_PERMISSIONS, "permissions")?;
        let mut usage = self.resource_usage()?;
        usage.take_collection_entry("permission")?;
        usage.take_record("permission")?;
        validate_permission_outcome(&outcome, &mut usage)?;
        self.permissions.push(outcome);
        Ok(())
    }

    /// Appends a checked setup-level diagnostic after enforcing all limits.
    pub fn push_diagnostic(&mut self, diagnostic: Diagnostic) -> Result<()> {
        ensure_can_append(self.diagnostics.len(), MAX_DIAGNOSTICS, "diagnostics")?;
        let mut usage = self.resource_usage()?;
        usage.take_collection_entry("diagnostic")?;
        usage.take_record("diagnostic")?;
        diagnostic.validate_with_usage(&mut usage)?;
        self.diagnostics.push(diagnostic);
        Ok(())
    }

    /// Serializes this envelope with fixed field order and a trailing newline.
    ///
    /// Category-list order is preserved because it records source precedence.
    pub fn to_canonical_json(&self) -> Result<String> {
        self.validate()?;
        canonical_json(self)
    }

    fn validate(&self) -> Result<()> {
        self.resource_usage().map(|_| ())
    }

    fn resource_usage(&self) -> Result<ResourceUsage> {
        if self.schema_version != MIGRATED_SETUP_SCHEMA_VERSION {
            return Err(ValidationError::new("invalid MigratedSetup schema version"));
        }
        validate_collection_count(self.models.len(), MAX_MODELS, "models")?;
        validate_collection_count(self.skills.len(), MAX_SKILLS, "skills")?;
        validate_collection_count(self.mcp_servers.len(), MAX_MCP_SERVERS, "MCP servers")?;
        validate_collection_count(self.permissions.len(), MAX_PERMISSIONS, "permissions")?;
        validate_collection_count(self.diagnostics.len(), MAX_DIAGNOSTICS, "diagnostics")?;

        let mut usage = ResourceUsage::default();
        usage.take_string("source agent", &self.source_agent)?;
        for outcome in &self.models {
            usage.take_collection_entry("model")?;
            usage.take_record("model")?;
            validate_model_outcome(outcome, &mut usage)?;
        }
        for outcome in &self.skills {
            usage.take_collection_entry("skill")?;
            usage.take_record("skill")?;
            validate_skill_outcome(outcome, &mut usage)?;
        }
        for outcome in &self.mcp_servers {
            usage.take_collection_entry("MCP server")?;
            usage.take_record("MCP server")?;
            validate_mcp_server_outcome(outcome, &mut usage)?;
        }
        for outcome in &self.permissions {
            usage.take_collection_entry("permission")?;
            usage.take_record("permission")?;
            validate_permission_outcome(outcome, &mut usage)?;
        }
        for diagnostic in &self.diagnostics {
            usage.take_collection_entry("diagnostic")?;
            usage.take_record("diagnostic")?;
            diagnostic.validate_with_usage(&mut usage)?;
        }
        Ok(usage)
    }
}

/// Header metadata for a [`CompareReport`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompareReportHeader {
    schema_version: u32,
    versions: BTreeMap<String, String>,
    hardware: BTreeMap<String, String>,
}

impl CompareReportHeader {
    /// Creates a checked v1 comparison header from deterministic key-value maps.
    pub fn new(
        versions: BTreeMap<String, String>,
        hardware: BTreeMap<String, String>,
    ) -> Result<Self> {
        let header = Self {
            schema_version: COMPARE_REPORT_SCHEMA_VERSION,
            versions,
            hardware,
        };
        header.validate()?;
        Ok(header)
    }

    /// Decodes raw v1 header JSON without passing through `serde_json::Value`.
    ///
    /// This is the only wire-decoding route for a validated
    /// `CompareReportHeader`.
    pub fn from_json(json: &str) -> Result<Self> {
        preflight_json(json)?;
        ensure_supported_version(
            probe_header_version(json)?,
            "CompareReportHeader",
            COMPARE_REPORT_SCHEMA_VERSION,
        )?;
        decode_raw::<RawCompareReportHeader>(json)?.into_validated()
    }

    /// Returns this header's schema version.
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Returns exact component versions in deterministic key order.
    pub fn versions(&self) -> &BTreeMap<String, String> {
        &self.versions
    }

    /// Returns normalized hardware facts in deterministic key order.
    pub fn hardware(&self) -> &BTreeMap<String, String> {
        &self.hardware
    }

    /// Serializes this header with fixed field and map order and a trailing newline.
    pub fn to_canonical_json(&self) -> Result<String> {
        self.validate()?;
        canonical_json(self)
    }

    fn validate(&self) -> Result<()> {
        self.validate_with_usage(&mut ResourceUsage::default())
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        if self.schema_version != COMPARE_REPORT_SCHEMA_VERSION {
            return Err(ValidationError::new(
                "invalid CompareReportHeader schema version",
            ));
        }
        validate_metadata_map(&self.versions, "versions", usage)?;
        validate_metadata_map(&self.hardware, "hardware", usage)
    }
}

/// One task result in a paired comparison report.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CompareTaskRow {
    task_id: String,
    agent: String,
    wall_clock: u64,
    peak_rss_bytes: u64,
    tokens_in: u64,
    tokens_out: u64,
    success: bool,
}

impl CompareTaskRow {
    /// Creates a checked task row with exact portable JSON integer metrics.
    pub fn new(
        task_id: impl Into<String>,
        agent: impl Into<String>,
        wall_clock: u64,
        peak_rss_bytes: u64,
        tokens_in: u64,
        tokens_out: u64,
        success: bool,
    ) -> Result<Self> {
        let row = Self {
            task_id: task_id.into(),
            agent: agent.into(),
            wall_clock,
            peak_rss_bytes,
            tokens_in,
            tokens_out,
            success,
        };
        row.validate_with_usage(&mut ResourceUsage::default())?;
        Ok(row)
    }

    /// Returns the stable task identifier shared across compared agents.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Returns the agent that produced this result.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Returns elapsed wall-clock time in milliseconds.
    pub const fn wall_clock(&self) -> u64 {
        self.wall_clock
    }

    /// Returns peak resident-set size in bytes.
    pub const fn peak_rss_bytes(&self) -> u64 {
        self.peak_rss_bytes
    }

    /// Returns input tokens reported for the task.
    pub const fn tokens_in(&self) -> u64 {
        self.tokens_in
    }

    /// Returns output tokens reported for the task.
    pub const fn tokens_out(&self) -> u64 {
        self.tokens_out
    }

    /// Returns whether the task's authoritative success check passed.
    pub const fn success(&self) -> bool {
        self.success
    }

    fn validate_with_usage(&self, usage: &mut ResourceUsage) -> Result<()> {
        usage.take_string("task identifier", &self.task_id)?;
        usage.take_string("task agent", &self.agent)?;
        validate_portable_metric(self.wall_clock, "wall_clock")?;
        validate_portable_metric(self.peak_rss_bytes, "peak_rss_bytes")?;
        validate_portable_metric(self.tokens_in, "tokens_in")?;
        validate_portable_metric(self.tokens_out, "tokens_out")
    }
}

/// A JSON-first paired comparison report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompareReport {
    header: CompareReportHeader,
    tasks: Vec<CompareTaskRow>,
}

impl CompareReport {
    /// Creates a checked comparison report from a v1 header and task rows.
    pub fn new(header: CompareReportHeader, tasks: Vec<CompareTaskRow>) -> Result<Self> {
        let report = Self { header, tasks };
        report.validate()?;
        Ok(report)
    }

    /// Decodes raw v1 report JSON without passing through `serde_json::Value`.
    ///
    /// This is the only wire-decoding route for a validated `CompareReport`.
    pub fn from_json(json: &str) -> Result<Self> {
        preflight_json(json)?;
        ensure_supported_version(
            probe_report_version(json)?,
            "CompareReport",
            COMPARE_REPORT_SCHEMA_VERSION,
        )?;
        decode_raw::<RawCompareReport>(json)?.into_validated()
    }

    /// Returns the version and metadata header.
    pub fn header(&self) -> &CompareReportHeader {
        &self.header
    }

    /// Returns task rows in their supplied source order.
    pub fn tasks(&self) -> &[CompareTaskRow] {
        &self.tasks
    }

    /// Appends a checked task row after enforcing all aggregate limits.
    pub fn push_task(&mut self, task: CompareTaskRow) -> Result<()> {
        ensure_can_append(self.tasks.len(), MAX_TASKS, "tasks")?;
        let mut usage = self.resource_usage()?;
        usage.take_collection_entry("task")?;
        usage.take_record("task")?;
        task.validate_with_usage(&mut usage)?;
        self.tasks.push(task);
        Ok(())
    }

    /// Serializes a deterministic JSON report with a trailing newline.
    ///
    /// Version and hardware maps are [`BTreeMap`]s. Task ordering is represented
    /// by a sorted vector of references, so this does not clone the report or
    /// task rows merely to render canonical JSON.
    pub fn to_canonical_json(&self) -> Result<String> {
        self.validate()?;
        let canonical = CanonicalCompareReport {
            header: &self.header,
            tasks: self.sorted_task_refs(),
        };
        canonical_json(&canonical)
    }

    /// Renders a human-readable Markdown view of this validated JSON schema.
    ///
    /// Rows use the same ordering as [`Self::to_canonical_json`]. Every
    /// report-provided string is rendered as literal text: HTML and Markdown
    /// syntax are escaped, and control or bidirectional formatting characters
    /// are shown as visible Unicode escapes. Newlines use trusted `<br>` markup.
    /// Sorting uses references and never clones report task rows.
    pub fn to_markdown(&self) -> Result<String> {
        self.validate()?;
        let tasks = self.sorted_task_refs();
        let mut markdown = String::from("# Compare report\n\n");
        markdown.push_str(&format!(
            "Schema version: {}\n\n",
            self.header.schema_version()
        ));

        markdown.push_str("## Versions\n\n| Component | Version |\n| --- | --- |\n");
        for (component, version) in self.header.versions() {
            markdown.push_str(&format!(
                "| {} | {} |\n",
                markdown_cell(component),
                markdown_cell(version)
            ));
        }

        markdown.push_str("\n## Hardware\n\n| Property | Value |\n| --- | --- |\n");
        for (property, value) in self.header.hardware() {
            markdown.push_str(&format!(
                "| {} | {} |\n",
                markdown_cell(property),
                markdown_cell(value)
            ));
        }

        markdown.push_str(
            "\n## Tasks\n\n| Task ID | Agent | Wall clock (ms) | Peak RSS (bytes) | Tokens in | Tokens out | Success |\n| --- | --- | ---: | ---: | ---: | ---: | --- |\n",
        );
        for task in tasks {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                markdown_cell(task.task_id()),
                markdown_cell(task.agent()),
                task.wall_clock(),
                task.peak_rss_bytes(),
                task.tokens_in(),
                task.tokens_out(),
                task.success(),
            ));
        }

        if markdown.len() > MAX_RENDERED_MARKDOWN_BYTES {
            return Err(ValidationError::new(format!(
                "rendered Markdown exceeds MAX_RENDERED_MARKDOWN_BYTES ({MAX_RENDERED_MARKDOWN_BYTES} bytes)"
            )));
        }
        Ok(markdown)
    }

    fn sorted_task_refs(&self) -> Vec<&CompareTaskRow> {
        let mut tasks = self.tasks.iter().collect::<Vec<_>>();
        tasks.sort_unstable();
        tasks
    }

    fn validate(&self) -> Result<()> {
        self.resource_usage().map(|_| ())
    }

    fn resource_usage(&self) -> Result<ResourceUsage> {
        validate_collection_count(self.tasks.len(), MAX_TASKS, "tasks")?;
        let mut usage = ResourceUsage::default();
        self.header.validate_with_usage(&mut usage)?;
        for task in &self.tasks {
            usage.take_collection_entry("task")?;
            usage.take_record("task")?;
            task.validate_with_usage(&mut usage)?;
        }
        Ok(usage)
    }
}

#[derive(Serialize)]
struct CanonicalCompareReport<'a> {
    header: &'a CompareReportHeader,
    tasks: Vec<&'a CompareTaskRow>,
}

/// Decodes a canonical comparison JSON report and renders its Markdown view.
///
/// This is the JSON-to-Markdown boundary: there is no separately deserialized
/// Markdown report schema.
pub fn render_compare_report_markdown(json: &str) -> Result<String> {
    CompareReport::from_json(json)?.to_markdown()
}

fn canonical_json<T>(value: &T) -> Result<String>
where
    T: Serialize,
{
    let mut json = serde_json::to_string_pretty(value).map_err(ValidationError::from_serde)?;
    let output_bytes = json
        .len()
        .checked_add(1)
        .ok_or_else(|| ValidationError::new("canonical JSON byte counter overflow"))?;
    if output_bytes > MAX_JSON_INPUT_BYTES {
        return Err(ValidationError::new(format!(
            "canonical JSON exceeds MAX_JSON_INPUT_BYTES ({MAX_JSON_INPUT_BYTES} bytes)"
        )));
    }
    json.push('\n');
    Ok(json)
}

fn validate_model_outcome(
    outcome: &MigrationOutcome<Model>,
    usage: &mut ResourceUsage,
) -> Result<()> {
    validate_migration_outcome(outcome, usage, Model::validate_with_usage)
}

fn validate_skill_outcome(
    outcome: &MigrationOutcome<Skill>,
    usage: &mut ResourceUsage,
) -> Result<()> {
    validate_migration_outcome(outcome, usage, Skill::validate_with_usage)
}

fn validate_mcp_server_outcome(
    outcome: &MigrationOutcome<McpServer>,
    usage: &mut ResourceUsage,
) -> Result<()> {
    validate_migration_outcome(outcome, usage, McpServer::validate_with_usage)
}

fn validate_permission_outcome(
    outcome: &MigrationOutcome<Permission>,
    usage: &mut ResourceUsage,
) -> Result<()> {
    validate_migration_outcome(outcome, usage, Permission::validate_with_usage)
}

fn validate_migration_outcome<T, F>(
    outcome: &MigrationOutcome<T>,
    usage: &mut ResourceUsage,
    validate_value: F,
) -> Result<()>
where
    F: FnOnce(&T, &mut ResourceUsage) -> Result<()>,
{
    match &outcome.inner {
        MigrationOutcomeInner::Mapped { path, value } => {
            validate_source_path(path, usage)?;
            validate_value(value, usage)
        }
        MigrationOutcomeInner::Unmapped { diagnostic } => diagnostic.validate_with_usage(usage),
    }
}

fn validate_metadata_map(
    values: &BTreeMap<String, String>,
    name: &str,
    usage: &mut ResourceUsage,
) -> Result<()> {
    validate_collection_count(values.len(), MAX_MAP_ENTRIES, name)?;
    for (key, value) in values {
        usage.take_collection_entry("comparison metadata")?;
        usage.take_string("comparison metadata key", key)?;
        usage.take_string("comparison metadata value", value)?;
    }
    Ok(())
}

fn validate_collection_count(count: usize, maximum: usize, name: &str) -> Result<()> {
    if count > maximum {
        return Err(ValidationError::new(format!(
            "{name} exceeds its limit of {maximum} entries"
        )));
    }
    Ok(())
}

fn ensure_can_append(current: usize, maximum: usize, name: &str) -> Result<()> {
    if current >= maximum {
        return Err(ValidationError::new(format!(
            "{name} exceeds its limit of {maximum} entries"
        )));
    }
    Ok(())
}

fn validate_portable_metric(value: u64, field: &str) -> Result<()> {
    if value > MAX_PORTABLE_JSON_INTEGER {
        return Err(ValidationError::new(format!(
            "{field} must be an exact portable JSON integer no greater than {MAX_PORTABLE_JSON_INTEGER}"
        )));
    }
    Ok(())
}

fn validate_source_path(path: &str, usage: &mut ResourceUsage) -> Result<()> {
    usage.take_string("mapped path", path)
}

fn validate_normalized_path(path: &str, field: &str, usage: &mut ResourceUsage) -> Result<()> {
    usage.take_string(field, path)?;
    if path.is_empty() {
        return Err(ValidationError::new(format!("{field} must not be empty")));
    }
    if path.trim() != path {
        return Err(ValidationError::new(format!(
            "{field} must not have leading or trailing whitespace"
        )));
    }
    if path.chars().any(requires_visible_escape) {
        return Err(ValidationError::new(format!(
            "{field} must not contain control or bidirectional-format characters"
        )));
    }
    if path.chars().any(is_default_ignorable_scalar) {
        return Err(ValidationError::new(format!(
            "{field} must not contain default-ignorable formatting or tag characters"
        )));
    }
    if !has_visible_diagnostic_scalar(path) {
        return Err(ValidationError::new(format!(
            "{field} must contain a visible non-whitespace scalar"
        )));
    }
    if path == ROOT_DIAGNOSTIC_PATH {
        return Ok(());
    }
    if path.contains('\\') || path.starts_with('/') || path.ends_with('/') {
        return Err(ValidationError::new(format!(
            "{field} must be a normalized source-relative path"
        )));
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(ValidationError::new(format!(
            "{field} must not contain empty, '.' or '..' segments"
        )));
    }
    Ok(())
}

fn validate_diagnostic_reason(reason: &str, usage: &mut ResourceUsage) -> Result<()> {
    usage.take_string("diagnostic reason", reason)?;
    if reason.chars().any(requires_visible_escape) {
        return Err(ValidationError::new(
            "diagnostic reason must not contain control or bidirectional-format characters",
        ));
    }
    if !has_visible_diagnostic_scalar(reason) {
        return Err(ValidationError::new(
            "diagnostic reason must contain a visible non-whitespace, non-default-ignorable scalar",
        ));
    }
    Ok(())
}

fn has_visible_diagnostic_scalar(value: &str) -> bool {
    value
        .chars()
        .any(|character| !character.is_whitespace() && !is_default_ignorable_scalar(character))
}

#[derive(Default)]
struct ResourceUsage {
    string_bytes: usize,
    records: usize,
    collection_entries: usize,
    diagnostics: usize,
}

impl ResourceUsage {
    fn take_string(&mut self, field: &str, value: &str) -> Result<()> {
        if value.len() > MAX_STRING_BYTES {
            return Err(ValidationError::new(format!(
                "{field} exceeds MAX_STRING_BYTES ({MAX_STRING_BYTES} bytes)"
            )));
        }
        self.string_bytes = self
            .string_bytes
            .checked_add(value.len())
            .ok_or_else(|| ValidationError::new("decoded string-byte counter overflow"))?;
        if self.string_bytes > MAX_TOTAL_DECODED_STRING_BYTES {
            return Err(ValidationError::new(format!(
                "decoded payload strings exceed MAX_TOTAL_DECODED_STRING_BYTES ({MAX_TOTAL_DECODED_STRING_BYTES} bytes)"
            )));
        }
        Ok(())
    }

    fn take_record(&mut self, kind: &str) -> Result<()> {
        self.records = self
            .records
            .checked_add(1)
            .ok_or_else(|| ValidationError::new("decoded record counter overflow"))?;
        if self.records > MAX_TOTAL_DECODED_RECORDS {
            return Err(ValidationError::new(format!(
                "decoded {kind} records exceed MAX_TOTAL_DECODED_RECORDS ({MAX_TOTAL_DECODED_RECORDS})"
            )));
        }
        Ok(())
    }

    fn take_collection_entry(&mut self, kind: &str) -> Result<()> {
        self.collection_entries = self
            .collection_entries
            .checked_add(1)
            .ok_or_else(|| ValidationError::new("decoded collection-entry counter overflow"))?;
        if self.collection_entries > MAX_TOTAL_DECODED_COLLECTION_ENTRIES {
            return Err(ValidationError::new(format!(
                "decoded {kind} collection entries exceed MAX_TOTAL_DECODED_COLLECTION_ENTRIES ({MAX_TOTAL_DECODED_COLLECTION_ENTRIES})"
            )));
        }
        Ok(())
    }

    fn take_diagnostic(&mut self) -> Result<()> {
        self.diagnostics = self
            .diagnostics
            .checked_add(1)
            .ok_or_else(|| ValidationError::new("diagnostic counter overflow"))?;
        if self.diagnostics > MAX_DIAGNOSTICS {
            return Err(ValidationError::new(format!(
                "diagnostics exceed their limit of {MAX_DIAGNOSTICS} entries"
            )));
        }
        Ok(())
    }
}

fn preflight_json(json: &str) -> Result<()> {
    if json.len() > MAX_JSON_INPUT_BYTES {
        return Err(ValidationError::new(format!(
            "JSON input exceeds MAX_JSON_INPUT_BYTES ({MAX_JSON_INPUT_BYTES} bytes)"
        )));
    }

    let mut deserializer = serde_json::Deserializer::from_str(json);
    let mut bounds = JsonBounds::default();
    BoundedJsonSeed {
        bounds: &mut bounds,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .map_err(ValidationError::from_serde)?;
    deserializer.end().map_err(ValidationError::from_serde)
}

#[derive(Default)]
struct JsonBounds {
    string_bytes: usize,
    entries: usize,
}

impl JsonBounds {
    fn enter_container(&self, depth: usize) -> Result<()> {
        if depth > MAX_JSON_NESTING {
            return Err(ValidationError::new(format!(
                "JSON nesting exceeds MAX_JSON_NESTING ({MAX_JSON_NESTING})"
            )));
        }
        Ok(())
    }

    fn take_string(&mut self, value: &str) -> Result<()> {
        if value.len() > MAX_STRING_BYTES {
            return Err(ValidationError::new(format!(
                "JSON string exceeds MAX_STRING_BYTES ({MAX_STRING_BYTES} bytes)"
            )));
        }
        self.string_bytes = self
            .string_bytes
            .checked_add(value.len())
            .ok_or_else(|| ValidationError::new("JSON string-byte counter overflow"))?;
        if self.string_bytes > MAX_TOTAL_JSON_STRING_BYTES {
            return Err(ValidationError::new(format!(
                "decoded JSON strings exceed MAX_TOTAL_JSON_STRING_BYTES ({MAX_TOTAL_JSON_STRING_BYTES} bytes)"
            )));
        }
        Ok(())
    }

    fn take_entry(&mut self, count: &mut usize, maximum: usize, kind: &str) -> Result<()> {
        if *count >= maximum {
            return Err(ValidationError::new(format!(
                "JSON {kind} exceeds its limit of {maximum} entries"
            )));
        }
        *count += 1;
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| ValidationError::new("JSON entry counter overflow"))?;
        if self.entries > MAX_TOTAL_JSON_ENTRIES {
            return Err(ValidationError::new(format!(
                "JSON entries exceed MAX_TOTAL_JSON_ENTRIES ({MAX_TOTAL_JSON_ENTRIES})"
            )));
        }
        Ok(())
    }
}

struct BoundedJsonSeed<'a> {
    bounds: &'a mut JsonBounds,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedJsonSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(BoundedJsonVisitor {
            bounds: self.bounds,
            depth: self.depth,
        })
    }
}

struct BoundedJsonVisitor<'a> {
    bounds: &'a mut JsonBounds,
    depth: usize,
}

impl<'de> Visitor<'de> for BoundedJsonVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON")
    }

    fn visit_bool<E>(self, _: bool) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        BoundedJsonSeed {
            bounds: self.bounds,
            depth: self.depth,
        }
        .deserialize(deserializer)
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.bounds.take_string(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.bounds.take_string(value).map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.bounds.take_string(&value).map_err(E::custom)
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.bounds
            .enter_container(self.depth + 1)
            .map_err(A::Error::custom)?;
        let bounds = self.bounds;
        let mut entries = 0;
        loop {
            let next = sequence.next_element_seed(BoundedSequenceElementSeed {
                bounds: &mut *bounds,
                entries: &mut entries,
                depth: self.depth + 1,
            })?;
            if next.is_none() {
                break;
            }
        }
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.bounds
            .enter_container(self.depth + 1)
            .map_err(A::Error::custom)?;
        let bounds = self.bounds;
        let mut entries = 0;
        loop {
            let key = map.next_key_seed(BoundedMapKeySeed {
                bounds: &mut *bounds,
                entries: &mut entries,
            })?;
            if key.is_none() {
                break;
            }
            map.next_value_seed(BoundedJsonSeed {
                bounds: &mut *bounds,
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }
}

struct BoundedMapKeySeed<'a> {
    bounds: &'a mut JsonBounds,
    entries: &'a mut usize,
}

impl<'de> DeserializeSeed<'de> for BoundedMapKeySeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.bounds
            .take_entry(self.entries, MAX_MAP_ENTRIES, "object")
            .map_err(D::Error::custom)?;
        deserializer.deserialize_str(BoundedJsonStringVisitor {
            bounds: self.bounds,
        })
    }
}

struct BoundedSequenceElementSeed<'a> {
    bounds: &'a mut JsonBounds,
    entries: &'a mut usize,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedSequenceElementSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.bounds
            .take_entry(self.entries, MAX_LIST_ENTRIES, "array")
            .map_err(D::Error::custom)?;
        BoundedJsonSeed {
            bounds: self.bounds,
            depth: self.depth,
        }
        .deserialize(deserializer)
    }
}

struct BoundedJsonStringVisitor<'a> {
    bounds: &'a mut JsonBounds,
}

impl<'de> Visitor<'de> for BoundedJsonStringVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON object key")
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.bounds.take_string(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.bounds.take_string(value).map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.bounds.take_string(&value).map_err(E::custom)
    }
}

fn ensure_supported_version(
    found: Option<u64>,
    document: &'static str,
    expected: u32,
) -> Result<()> {
    if let Some(found) = found {
        if found != u64::from(expected) {
            return Err(ValidationError::new(
                SchemaVersionMismatch {
                    document,
                    found,
                    expected,
                }
                .to_string(),
            ));
        }
    }
    Ok(())
}

fn decode_raw<T>(json: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(json).map_err(ValidationError::from_serde)
}

struct RawVersion(u64);

impl<'de> Deserialize<'de> for RawVersion {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_portable_integer(deserializer, "schema_version").map(Self)
    }
}

struct PortableIntegerVisitor {
    field: &'static str,
}

impl<'de> Visitor<'de> for PortableIntegerVisitor {
    type Value = u64;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "an exact portable JSON integer for {} no greater than {}",
            self.field, MAX_PORTABLE_JSON_INTEGER
        )
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value > MAX_PORTABLE_JSON_INTEGER {
            return Err(E::custom(format!(
                "{} must be an exact portable JSON integer no greater than {}",
                self.field, MAX_PORTABLE_JSON_INTEGER
            )));
        }
        Ok(value)
    }

    fn visit_u128<E>(self, value: u128) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value > u128::from(MAX_PORTABLE_JSON_INTEGER) {
            return Err(E::custom(format!(
                "{} must be an exact portable JSON integer no greater than {}",
                self.field, MAX_PORTABLE_JSON_INTEGER
            )));
        }
        Ok(value as u64)
    }

    fn visit_i64<E>(self, _: i64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(E::custom(format!(
            "{} must be a non-negative exact JSON integer",
            self.field
        )))
    }

    fn visit_i128<E>(self, _: i128) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(E::custom(format!(
            "{} must be a non-negative exact JSON integer",
            self.field
        )))
    }

    fn visit_f64<E>(self, _: f64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(E::custom(format!(
            "{} must be a JSON integer without a fraction or exponent",
            self.field
        )))
    }
}

fn deserialize_portable_integer<'de, D>(
    deserializer: D,
    field: &'static str,
) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(PortableIntegerVisitor { field })
}

fn deserialize_wall_clock<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_portable_integer(deserializer, "wall_clock")
}

fn deserialize_peak_rss_bytes<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_portable_integer(deserializer, "peak_rss_bytes")
}

fn deserialize_tokens_in<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_portable_integer(deserializer, "tokens_in")
}

fn deserialize_tokens_out<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_portable_integer(deserializer, "tokens_out")
}

struct BoundedString(String);

impl BoundedString {
    fn from_str(value: &str) -> Result<Self> {
        if value.len() > MAX_STRING_BYTES {
            return Err(ValidationError::new(format!(
                "JSON string exceeds MAX_STRING_BYTES ({MAX_STRING_BYTES} bytes)"
            )));
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for BoundedString {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_string(BoundedStringVisitor)
    }
}

struct BoundedStringVisitor;

impl<'de> Visitor<'de> for BoundedStringVisitor {
    type Value = BoundedString;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON string")
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        BoundedString::from_str(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        BoundedString::from_str(value).map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() > MAX_STRING_BYTES {
            return Err(E::custom(format!(
                "JSON string exceeds MAX_STRING_BYTES ({MAX_STRING_BYTES} bytes)"
            )));
        }
        Ok(BoundedString(value))
    }
}

fn deserialize_bounded_list<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedListVisitor<T>(PhantomData<T>);

    impl<'de, T> Visitor<'de> for BoundedListVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded JSON array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() >= MAX_LIST_ENTRIES {
                    return Err(A::Error::custom(format!(
                        "JSON array exceeds its limit of {MAX_LIST_ENTRIES} entries"
                    )));
                }
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedListVisitor(PhantomData))
}

struct UniqueStringMap(BTreeMap<String, String>);

impl<'de> Deserialize<'de> for UniqueStringMap {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueStringMapVisitor;

        impl<'de> Visitor<'de> for UniqueStringMapVisitor {
            type Value = UniqueStringMap;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded comparison metadata map with unique decoded keys")
            }

            fn visit_map<A>(self, mut entries: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some(BoundedString(key)) = entries.next_key()? {
                    // Detect a literal or escape-equivalent decoded duplicate before
                    // requesting its value, so an invalid duplicate value cannot
                    // mask the duplicate-key error or force a value allocation.
                    let at_capacity = values.len() >= MAX_MAP_ENTRIES;
                    match values.entry(key) {
                        Entry::Occupied(entry) => {
                            return Err(A::Error::custom(format!(
                                "duplicate comparison metadata key {:?}",
                                entry.key()
                            )));
                        }
                        Entry::Vacant(entry) => {
                            if at_capacity {
                                return Err(A::Error::custom(format!(
                                    "JSON object exceeds its limit of {MAX_MAP_ENTRIES} entries"
                                )));
                            }
                            let BoundedString(value) = entries.next_value()?;
                            entry.insert(value);
                        }
                    }
                }
                Ok(UniqueStringMap(values))
            }
        }

        deserializer.deserialize_map(UniqueStringMapVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMigratedSetup {
    schema_version: RawVersion,
    source_agent: BoundedString,
    #[serde(deserialize_with = "deserialize_bounded_list")]
    models: Vec<RawMigrationOutcome<RawModel>>,
    #[serde(deserialize_with = "deserialize_bounded_list")]
    skills: Vec<RawMigrationOutcome<RawSkill>>,
    #[serde(deserialize_with = "deserialize_bounded_list")]
    mcp_servers: Vec<RawMigrationOutcome<RawMcpServer>>,
    #[serde(deserialize_with = "deserialize_bounded_list")]
    permissions: Vec<RawMigrationOutcome<RawPermission>>,
    #[serde(deserialize_with = "deserialize_bounded_list")]
    diagnostics: Vec<RawDiagnostic>,
}

impl RawMigratedSetup {
    fn into_validated(self) -> Result<MigratedSetup> {
        if self.schema_version.0 != u64::from(MIGRATED_SETUP_SCHEMA_VERSION) {
            return Err(ValidationError::new(
                SchemaVersionMismatch {
                    document: "MigratedSetup",
                    found: self.schema_version.0,
                    expected: MIGRATED_SETUP_SCHEMA_VERSION,
                }
                .to_string(),
            ));
        }
        MigratedSetup::with_parts(
            self.source_agent.0,
            self.models
                .into_iter()
                .map(|outcome| outcome.into_public(RawModel::into_public))
                .collect(),
            self.skills
                .into_iter()
                .map(|outcome| outcome.into_public(RawSkill::into_public))
                .collect(),
            self.mcp_servers
                .into_iter()
                .map(|outcome| outcome.into_public(RawMcpServer::into_public))
                .collect(),
            self.permissions
                .into_iter()
                .map(|outcome| outcome.into_public(RawPermission::into_public))
                .collect(),
            self.diagnostics
                .into_iter()
                .map(RawDiagnostic::into_public)
                .collect(),
        )
    }
}

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum RawMigrationOutcome<T> {
    Mapped { path: BoundedString, value: T },
    Unmapped { diagnostic: RawDiagnostic },
}

impl<T> RawMigrationOutcome<T> {
    fn into_public<U>(self, map_value: impl FnOnce(T) -> U) -> MigrationOutcome<U> {
        match self {
            Self::Mapped { path, value } => {
                MigrationOutcome::mapped_unchecked(path.0, map_value(value))
            }
            Self::Unmapped { diagnostic } => {
                MigrationOutcome::unmapped_unchecked(diagnostic.into_public())
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    provider: BoundedString,
    model: BoundedString,
}

impl RawModel {
    fn into_public(self) -> Model {
        Model {
            provider: self.provider.0,
            model: self.model.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSkill {
    name: BoundedString,
    content: BoundedString,
}

impl RawSkill {
    fn into_public(self) -> Skill {
        Skill {
            name: self.name.0,
            content: self.content.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServer {
    name: BoundedString,
    transport: RawMcpTransport,
}

impl RawMcpServer {
    fn into_public(self) -> McpServer {
        McpServer {
            name: self.name.0,
            transport: self.transport.into_public(),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawMcpTransport {
    Stdio {
        command: BoundedString,
        #[serde(deserialize_with = "deserialize_bounded_list")]
        args: Vec<BoundedString>,
    },
    Http {
        url: BoundedString,
    },
}

impl RawMcpTransport {
    fn into_public(self) -> McpTransport {
        match self {
            Self::Stdio { command, args } => McpTransport {
                inner: McpTransportInner::Stdio {
                    command: command.0,
                    args: args.into_iter().map(|argument| argument.0).collect(),
                },
            },
            Self::Http { url } => McpTransport {
                inner: McpTransportInner::Http { url: url.0 },
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermission {
    capability: BoundedString,
    decision: RawPermissionDecision,
}

impl RawPermission {
    fn into_public(self) -> Permission {
        Permission {
            capability: self.capability.0,
            decision: self.decision.into_public(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawPermissionDecision {
    Allow,
    Ask,
    Deny,
}

impl RawPermissionDecision {
    fn into_public(self) -> PermissionDecision {
        match self {
            Self::Allow => PermissionDecision::Allow,
            Self::Ask => PermissionDecision::Ask,
            Self::Deny => PermissionDecision::Deny,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiagnostic {
    path: BoundedString,
    severity: RawDiagnosticSeverity,
    reason: BoundedString,
}

impl RawDiagnostic {
    fn into_public(self) -> Diagnostic {
        Diagnostic {
            path: self.path.0,
            severity: self.severity.into_public(),
            reason: self.reason.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawDiagnosticSeverity {
    Warning,
    Error,
}

impl RawDiagnosticSeverity {
    fn into_public(self) -> DiagnosticSeverity {
        match self {
            Self::Warning => DiagnosticSeverity::Warning,
            Self::Error => DiagnosticSeverity::Error,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompareReportHeader {
    schema_version: RawVersion,
    versions: UniqueStringMap,
    hardware: UniqueStringMap,
}

impl RawCompareReportHeader {
    fn into_validated(self) -> Result<CompareReportHeader> {
        if self.schema_version.0 != u64::from(COMPARE_REPORT_SCHEMA_VERSION) {
            return Err(ValidationError::new(
                SchemaVersionMismatch {
                    document: "CompareReportHeader",
                    found: self.schema_version.0,
                    expected: COMPARE_REPORT_SCHEMA_VERSION,
                }
                .to_string(),
            ));
        }
        CompareReportHeader::new(self.versions.0, self.hardware.0)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompareTaskRow {
    task_id: BoundedString,
    agent: BoundedString,
    #[serde(deserialize_with = "deserialize_wall_clock")]
    wall_clock: u64,
    #[serde(deserialize_with = "deserialize_peak_rss_bytes")]
    peak_rss_bytes: u64,
    #[serde(deserialize_with = "deserialize_tokens_in")]
    tokens_in: u64,
    #[serde(deserialize_with = "deserialize_tokens_out")]
    tokens_out: u64,
    success: bool,
}

impl RawCompareTaskRow {
    fn into_public(self) -> CompareTaskRow {
        CompareTaskRow {
            task_id: self.task_id.0,
            agent: self.agent.0,
            wall_clock: self.wall_clock,
            peak_rss_bytes: self.peak_rss_bytes,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            success: self.success,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompareReport {
    header: RawCompareReportHeader,
    #[serde(deserialize_with = "deserialize_bounded_list")]
    tasks: Vec<RawCompareTaskRow>,
}

impl RawCompareReport {
    fn into_validated(self) -> Result<CompareReport> {
        CompareReport::new(
            self.header.into_validated()?,
            self.tasks
                .into_iter()
                .map(RawCompareTaskRow::into_public)
                .collect(),
        )
    }
}

struct SetupVersionProbe {
    version: Option<u64>,
}

impl<'de> Deserialize<'de> for SetupVersionProbe {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SetupVersionProbeVisitor;

        impl<'de> Visitor<'de> for SetupVersionProbeVisitor {
            type Value = SetupVersionProbe;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a migrated setup JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut version = None;
                while let Some(BoundedString(key)) = map.next_key()? {
                    if key == "schema_version" && version.is_none() {
                        version = Some(map.next_value::<RawVersion>()?.0);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(SetupVersionProbe { version })
            }
        }

        deserializer.deserialize_map(SetupVersionProbeVisitor)
    }
}

fn probe_setup_version(json: &str) -> Result<Option<u64>> {
    Ok(decode_raw::<SetupVersionProbe>(json)?.version)
}

struct HeaderVersionProbe {
    version: Option<u64>,
}

impl<'de> Deserialize<'de> for HeaderVersionProbe {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HeaderVersionProbeVisitor;

        impl<'de> Visitor<'de> for HeaderVersionProbeVisitor {
            type Value = HeaderVersionProbe;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a comparison header JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut version = None;
                while let Some(BoundedString(key)) = map.next_key()? {
                    if key == "schema_version" && version.is_none() {
                        version = Some(map.next_value::<RawVersion>()?.0);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(HeaderVersionProbe { version })
            }
        }

        deserializer.deserialize_map(HeaderVersionProbeVisitor)
    }
}

fn probe_header_version(json: &str) -> Result<Option<u64>> {
    Ok(decode_raw::<HeaderVersionProbe>(json)?.version)
}

struct ReportVersionProbe {
    version: Option<u64>,
}

impl<'de> Deserialize<'de> for ReportVersionProbe {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ReportVersionProbeVisitor;

        impl<'de> Visitor<'de> for ReportVersionProbeVisitor {
            type Value = ReportVersionProbe;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a comparison report JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut version = None;
                while let Some(BoundedString(key)) = map.next_key()? {
                    if key == "header" {
                        let header = map.next_value::<HeaderVersionProbe>()?;
                        if version.is_none() {
                            version = header.version;
                        }
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(ReportVersionProbe { version })
            }
        }

        deserializer.deserialize_map(ReportVersionProbeVisitor)
    }
}

fn probe_report_version(json: &str) -> Result<Option<u64>> {
    Ok(decode_raw::<ReportVersionProbe>(json)?.version)
}

fn markdown_cell(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    let mut previous = None;
    let mut preceding = [None; 3];

    while let Some(character) = characters.next() {
        match character {
            '\n' => escaped.push_str("<br>"),
            '\r' if matches!(characters.peek(), Some('\n')) => {
                characters.next();
                escaped.push_str("<br>");
            }
            _ if requires_visible_escape(character) => {
                write!(escaped, "\\u{{{:04X}}}", u32::from(character))
                    .expect("writing a Unicode escape to a String cannot fail");
            }
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            '\\' => escaped.push_str("\\\\"),
            '.' if has_www_prefix(preceding) => {
                escaped.push('\\');
                escaped.push('.');
            }
            '_' if is_intraword_underscore(previous, characters.peek().copied()) => {
                escaped.push('_');
            }
            '!' | '#' | '$' | '(' | ')' | '*' | '+' | '/' | ':' | '@' | '[' | ']' | '`' | '_'
            | '{' | '|' | '}' | '~' | '^' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
        previous = Some(character);
        preceding.rotate_left(1);
        preceding[2] = Some(character);
    }

    escaped
}

fn has_www_prefix(preceding: [Option<char>; 3]) -> bool {
    preceding
        .iter()
        .copied()
        .all(|character| matches!(character, Some('w' | 'W')))
}

fn is_intraword_underscore(previous: Option<char>, next: Option<char>) -> bool {
    previous.is_some_and(char::is_alphanumeric) && next.is_some_and(char::is_alphanumeric)
}

// This fixed table is the v1 bounded policy for Unicode
// `Default_Ignorable_Code_Point`. It is intentionally scalar-based: diagnostic
// validation does not depend on normalization, Unicode database loading, or
// locale-specific rendering behavior.
fn is_default_ignorable_scalar(character: char) -> bool {
    matches!(
        character as u32,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

fn requires_visible_escape(character: char) -> bool {
    matches!(
        character,
        '\u{0000}'..='\u{001F}'
            | '\u{007F}'..='\u{009F}'
            | '\u{061C}'
            | '\u{200E}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2066}'..='\u{206F}'
    )
}
