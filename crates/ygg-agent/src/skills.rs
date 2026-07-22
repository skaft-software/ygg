use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::session::EntryId;

/// The unique identifier of a skill.
pub type SkillId = String;

/// A deterministic FNV-1a content hash used for optimistic concurrency checks.
pub type ContentHash = String;

/// The activation identifier of a skill, tied to the EntryId of its activation event.
pub type SkillActivationId = EntryId;

/// The source of a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillSource {
    /// A built-in skill compiled into the agent or loaded from embedded static assets.
    BuiltIn,
    /// A filesystem-backed skill.
    FileSystem {
        /// The root directory containing the skill configuration and resources.
        root: PathBuf,
        /// The main SKILL.md instructions entrypoint path.
        entrypoint: PathBuf,
    },
}

/// The trust classification of a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillTrust {
    /// Built-in skills that ship with the agent.
    BuiltIn,
    /// Skills installed by the user globally.
    UserInstalled,
    /// Skills defined under the workspace's `.ygg/skills/` directory.
    Workspace,
    /// Explicitly targeted external directory paths.
    ExplicitExternal,
}

/// Metadata description of a discovered skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDescriptor {
    /// The unique skill ID (lowercase, alphanumeric, hyphens).
    pub id: SkillId,
    /// The human-readable name of the skill.
    pub name: String,
    /// A short description of the skill's purpose.
    pub description: String,
    /// Optional version string.
    pub version: Option<String>,
    /// The location source of the skill.
    pub source: SkillSource,
    /// The trust provenance level of the skill.
    pub trust: SkillTrust,
    /// Tools that must be registered for this skill to be loaded.
    pub required_tools: Vec<String>,
    /// Categorization tags.
    pub tags: Vec<String>,
}

/// A snapshot of a loaded skill, containing its descriptor, core instructions, and hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSkill {
    /// The skill metadata descriptor.
    pub descriptor: SkillDescriptor,
    /// The core markdown procedure content of SKILL.md.
    pub instructions: String,
    /// Deterministic hash of the full skill file content.
    pub content_hash: ContentHash,
}

/// Search result for query matches.
pub struct SkillSearchResult {
    /// Discovered descriptor.
    pub descriptor: SkillDescriptor,
}

/// Inspectable best-effort discovery or validation problem. Registries keep
/// healthy skills available even when one filesystem candidate is malformed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiagnostic {
    /// Candidate or entrypoint associated with the problem.
    pub path: PathBuf,
    /// Human-readable, credential-free diagnostic.
    pub message: String,
}

/// Structured search query.
pub struct SkillQuery {
    /// Search terms.
    pub text: String,
}

/// Structured skill loading and execution errors.
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillLoadError {
    /// Requested skill ID could not be found.
    #[error("Skill not found: {0}")]
    NotFound(SkillId),
    /// Skill ID matched multiple resources.
    #[error("Ambiguous skill ID: {0}")]
    AmbiguousId(SkillId),
    /// Workspace trust is required but not granted.
    #[error("Workspace is untrusted")]
    UntrustedWorkspace,
    /// Skill requires tools that are missing or disabled.
    #[error("Missing required tools: {0:?}")]
    MissingRequiredTools(Vec<String>),
    /// Skill YAML header/frontmatter was malformed.
    #[error("Invalid manifest: {0}")]
    InvalidManifest(String),
    /// Target resource path is invalid (contains traversal components like `..`).
    #[error("Invalid resource path")]
    InvalidResourcePath,
    /// Symlink encountered in path resolution was rejected.
    #[error("Symlink rejected")]
    SymlinkRejected,
    /// File size limit exceeded.
    #[error("Resource too large: {0} bytes")]
    ResourceTooLarge(u64),
    /// Text resource was not valid UTF-8.
    #[error("Invalid UTF-8 content")]
    InvalidUtf8,
    /// Skill source directory was modified or files changed during validation.
    #[error("Skill source changed")]
    SourceChanged,
    /// The descriptor's source kind is not supported by this registry.
    #[error("Unsupported skill source: {0}")]
    UnsupportedSource(String),
    /// Resource requested to read escaped the containment root of the skill.
    #[error("Security violation: {0}")]
    SecurityViolation(String),
    /// Underling I/O failure.
    #[error("I/O error: {0}")]
    Io(String),
}

/// Abstract registry capability interface for skills retrieval and lazy resource loading.
pub trait SkillRegistry: Send + Sync {
    /// Returns an immutable snapshot list of all discovered skill descriptors.
    fn descriptors(&self) -> Arc<[SkillDescriptor]>;

    /// Returns discovery and validation problems for the current immutable
    /// registry generation. In-memory registries may use the empty default.
    fn diagnostics(&self) -> Arc<[SkillDiagnostic]> {
        Arc::from([])
    }

    /// Queries the registry for skill descriptors matching terms.
    fn find(&self, query: &SkillQuery) -> Vec<SkillSearchResult>;

    /// Reads and validates the core SKILL.md file, returning a snapshot.
    fn load(&self, id: &SkillId) -> Result<LoadedSkill, SkillLoadError>;

    /// Lazy reads a supporting text document under references/ or templates/.
    fn read_resource(&self, snapshot: &LoadedSkill, path: &str) -> Result<String, SkillLoadError>;
}
