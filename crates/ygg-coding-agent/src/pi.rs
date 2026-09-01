#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use ygg_agent::extension_process::{
    ExtensionCapabilities, ExtensionEntrypoint, ExtensionFilesystemAccess, ExtensionHook,
    ExtensionManifest, ExtensionUiSurface, ManifestContributions,
};
use ygg_agent::EXTENSION_API_VERSION_0_2;

const BRIDGE_VERSION: &str = "0.2.0";
const SUPPORTED_PI_VERSION: &str = "0.84.4";
const YGG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROFILE_ID: &str = "pi-0.84.4";
const PROFILE_REPOSITORY: &str = "https://github.com/earendil-works/pi.git";
const PROFILE_REVISION: &str = "b79e4cc834970cca69daebffab7df1da7d1e52c4";
const PROFILE_TAG: &str = "v0.84.4";
const PI_PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";
const PI_PACKAGE_INTEGRITY: &str =
    "sha512-jmOlrqUmvhh/siNWFRXjYLJzhKFIHNsAQaysRwzQPQFnPAaV/vhqHsLH/MBsIISA1Rjj7WTUFR3nJrpXoLx39w==";
const PI_TUI_PACKAGE_NAME: &str = "@earendil-works/pi-tui";
const PI_TUI_PACKAGE_INTEGRITY: &str =
    "sha512-nPUnwDkLtupPXnZQYrCwPFcuTydCDqTY6ZbFqhsL4S4kVq0AT418kPa/6uXwtaCD+MjBNBltb7ScTYX65yeE1w==";
const MINIMUM_NODE_VERSION: &str = "22.19.0";
const LINK_SCHEMA_VERSION: u32 = 2;
const LINK_RECORD: &str = "pi-link.json";
const PI_LOCK_SCHEMA_VERSION: u32 = 2;
pub(crate) const PI_LOCK_RECORD: &str = "pi-lock.json";
pub(crate) const DEFAULT_AGGREGATE_NAME: &str = "pi-compat-0-84-4";
const MAX_AGGREGATE_SOURCES: usize = 256;
const SOURCE_FINGERPRINT_ALGORITHM: &str = "sha256";
const SOURCE_FINGERPRINT_FORMAT: u32 = 1;
const MAX_SOURCE_PATH_BYTES: usize = 4096;
const MAX_SOURCE_FILES: usize = 4096;
const MAX_SOURCE_ENTRIES: usize = 8192;
const MAX_SOURCE_DEPTH: usize = 64;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_LINK_RECORD_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PI_LOCK_BYTES: usize = 256 * 1024;
const MAX_PI_PACKAGE_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_GENERATED_FILE_BYTES: usize = 4 * 1024 * 1024;
const AGGREGATE_DIGEST_DOMAIN: &[u8] = b"ygg-pi-aggregate-lock-v2\0";
const OUTPUT_DIGEST_DOMAIN: &[u8] = b"ygg-pi-aggregate-output-v1\0";
const SOURCE_ID_DOMAIN: &[u8] = b"ygg-pi-source-id-v1\0";
const AGGREGATE_DIGEST_ENV: &str = "YGG_PI_AGGREGATE_DIGEST";

#[derive(Clone, Copy, Debug)]
struct FingerprintLimits {
    max_files: usize,
    max_entries: usize,
    max_bytes: usize,
}

const FINGERPRINT_LIMITS: FingerprintLimits = FingerprintLimits {
    max_files: MAX_SOURCE_FILES,
    max_entries: MAX_SOURCE_ENTRIES,
    max_bytes: MAX_SOURCE_BYTES,
};

#[derive(Clone, Debug, Subcommand)]
pub enum PiCommand {
    /// Create an inert Ygg wrapper for an existing local Pi extension/package.
    Install {
        /// A local .ts/.js extension file or an installed Pi package directory.
        source: PathBuf,
        /// Additional reviewed Pi sources loaded into the same ordered process.
        #[arg(long = "with", value_name = "SOURCE")]
        additional_sources: Vec<PathBuf>,
        /// Override the generated Ygg extension name.
        #[arg(long)]
        name: Option<String>,
        /// Pi's user agent directory. Defaults to PI_CODING_AGENT_DIR or ~/.pi/agent.
        #[arg(long, value_name = "DIR")]
        pi_home: Option<PathBuf>,
        /// Exact @earendil-works/pi-coding-agent package root for bridge profile 0.84.4.
        #[arg(long, value_name = "DIR")]
        pi_package: Option<PathBuf>,
        /// Ygg extension root used for the generated compatibility link.
        #[arg(long, value_name = "DIR")]
        extension_root: Option<PathBuf>,
    },
    /// List generated Pi compatibility links.
    List {
        /// Ygg extension root used for generated compatibility links.
        #[arg(long, value_name = "DIR")]
        extension_root: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceFingerprint {
    pub(crate) algorithm: String,
    pub(crate) format_version: u32,
    pub(crate) digest: String,
    pub(crate) file_count: u64,
    pub(crate) byte_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PiLinkRecord {
    schema_version: u32,
    bridge_version: String,
    pi_version: String,
    ygg_version: String,
    source_fingerprint: SourceFingerprint,
    name: String,
    source: PathBuf,
    pi_home: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi_package: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiProfilePackage {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) npm_integrity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiProfileMetadata {
    pub(crate) id: String,
    pub(crate) repository: String,
    pub(crate) revision: String,
    pub(crate) tag: String,
    pub(crate) coding_agent: PiProfilePackage,
    pub(crate) tui: PiProfilePackage,
    pub(crate) node_minimum_version: String,
}

pub(crate) fn supported_profile() -> PiProfileMetadata {
    PiProfileMetadata {
        id: PROFILE_ID.to_owned(),
        repository: PROFILE_REPOSITORY.to_owned(),
        revision: PROFILE_REVISION.to_owned(),
        tag: PROFILE_TAG.to_owned(),
        coding_agent: PiProfilePackage {
            name: PI_PACKAGE_NAME.to_owned(),
            version: SUPPORTED_PI_VERSION.to_owned(),
            npm_integrity: PI_PACKAGE_INTEGRITY.to_owned(),
        },
        tui: PiProfilePackage {
            name: PI_TUI_PACKAGE_NAME.to_owned(),
            version: SUPPORTED_PI_VERSION.to_owned(),
            npm_integrity: PI_TUI_PACKAGE_INTEGRITY.to_owned(),
        },
        node_minimum_version: MINIMUM_NODE_VERSION.to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiBridgeMetadata {
    pub(crate) version: String,
    pub(crate) script_digest: String,
}

pub(crate) fn bridge_script_bytes() -> &'static [u8] {
    include_bytes!("../../../extensions/ygg-pi-compat/bridge.mjs")
}

pub(crate) fn bridge_metadata() -> PiBridgeMetadata {
    PiBridgeMetadata {
        version: BRIDGE_VERSION.to_owned(),
        script_digest: format!("{:x}", Sha256::digest(bridge_script_bytes())),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiSourceKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiLockedSource {
    pub(crate) id: String,
    pub(crate) canonical_path: PathBuf,
    pub(crate) kind: PiSourceKind,
    pub(crate) source_fingerprint: SourceFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dependency_lock_hash: Option<String>,
    pub(crate) enabled: bool,
}

impl PiLockedSource {
    pub(crate) fn from_canonical_path(
        canonical_path: PathBuf,
        dependency_lock_hash: Option<String>,
    ) -> anyhow::Result<Self> {
        validate_canonical_source_path(&canonical_path)?;
        let metadata = fs::symlink_metadata(&canonical_path).with_context(|| {
            format!(
                "cannot inspect Pi extension source {}",
                canonical_path.display()
            )
        })?;
        let kind = if metadata.file_type().is_file() {
            PiSourceKind::File
        } else if metadata.file_type().is_dir() {
            PiSourceKind::Directory
        } else {
            anyhow::bail!(
                "Pi extension source must be a regular file or directory: {}",
                canonical_path.display()
            );
        };
        let id = stable_source_id(&canonical_path, kind)?;
        let source_fingerprint = fingerprint_source(&canonical_path)?;
        if dependency_lock_hash
            .as_deref()
            .is_some_and(|digest| !valid_sha256(digest))
        {
            anyhow::bail!("Pi dependency lock hash must be a lowercase SHA-256 digest");
        }
        Ok(Self {
            id,
            canonical_path,
            kind,
            source_fingerprint,
            dependency_lock_hash,
            enabled: true,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiPackageMetadata {
    pub(crate) canonical_path: PathBuf,
    pub(crate) metadata_path: PathBuf,
    pub(crate) metadata_digest: String,
    pub(crate) name: String,
    pub(crate) version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiLockRecord {
    pub(crate) schema_version: u32,
    pub(crate) profile: PiProfileMetadata,
    pub(crate) bridge: PiBridgeMetadata,
    pub(crate) ygg_version: String,
    pub(crate) name: String,
    pub(crate) sources: Vec<PiLockedSource>,
    pub(crate) pi_home: PathBuf,
    pub(crate) pi_package: PiPackageMetadata,
    pub(crate) aggregate_digest: String,
}

impl PiLockRecord {
    pub(crate) fn new(
        name: String,
        sources: Vec<PiLockedSource>,
        pi_home: PathBuf,
        pi_package: PiPackageMetadata,
    ) -> anyhow::Result<Self> {
        let mut record = Self {
            schema_version: PI_LOCK_SCHEMA_VERSION,
            profile: supported_profile(),
            bridge: bridge_metadata(),
            ygg_version: YGG_VERSION.to_owned(),
            name,
            sources,
            pi_home,
            pi_package,
            aggregate_digest: String::new(),
        };
        validate_lock_shape(&record)?;
        record.aggregate_digest = aggregate_lock_digest(&record)?;
        Ok(record)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct LegacyPiLinkRecord {
    schema_version: u32,
    bridge_version: String,
    name: String,
    source: PathBuf,
    pi_home: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ParsedPiLinkRecord {
    Legacy(LegacyPiLinkRecord),
    V2(PiLinkRecord),
}

impl ParsedPiLinkRecord {
    fn name(&self) -> &str {
        match self {
            Self::Legacy(record) => &record.name,
            Self::V2(record) => &record.name,
        }
    }

    fn source(&self) -> &Path {
        match self {
            Self::Legacy(record) => &record.source,
            Self::V2(record) => &record.source,
        }
    }

    fn pi_home(&self) -> &Path {
        match self {
            Self::Legacy(record) => &record.pi_home,
            Self::V2(record) => &record.pi_home,
        }
    }

    fn pi_package(&self) -> Option<&Path> {
        match self {
            Self::Legacy(_) => None,
            Self::V2(record) => record.pi_package.as_deref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct LegacyPiLockedSource {
    source: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct LegacyPiLockRecord {
    schema_version: u32,
    name: String,
    sources: Vec<LegacyPiLockedSource>,
    pi_home: PathBuf,
    pi_package: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ParsedPiInstallation {
    Link(ParsedPiLinkRecord),
    LegacyLock(LegacyPiLockRecord),
    Lock(Box<PiLockRecord>),
}

impl ParsedPiInstallation {
    fn name(&self) -> &str {
        match self {
            Self::Link(record) => record.name(),
            Self::LegacyLock(record) => &record.name,
            Self::Lock(record) => &record.name,
        }
    }

    fn source_summary(&self) -> String {
        match self {
            Self::Link(record) => record.source().display().to_string(),
            Self::LegacyLock(record) => format!(
                "{} legacy ordered source(s): {}",
                record.sources.len(),
                record
                    .sources
                    .iter()
                    .map(|source| source.source.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Lock(record) => format!(
                "{} ordered source(s): {}",
                record.sources.len(),
                record
                    .sources
                    .iter()
                    .map(|source| source.canonical_path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn pi_home(&self) -> &Path {
        match self {
            Self::Link(record) => record.pi_home(),
            Self::LegacyLock(record) => &record.pi_home,
            Self::Lock(record) => &record.pi_home,
        }
    }

    fn pi_package(&self) -> Option<&Path> {
        match self {
            Self::Link(record) => record.pi_package(),
            Self::LegacyLock(record) => record.pi_package.as_deref(),
            Self::Lock(record) => Some(&record.pi_package.canonical_path),
        }
    }

    fn status(&self) -> String {
        match self {
            Self::Link(record) => link_status(record),
            Self::LegacyLock(record) => format!(
                "legacy/stale (aggregate lock schema {} lacks digest-bound profile metadata)",
                record.schema_version
            ),
            Self::Lock(record) => aggregate_status(record),
        }
    }
}

#[derive(Deserialize)]
struct LinkRecordSchema {
    schema_version: u32,
}

#[derive(Deserialize)]
struct PiPackageManifest {
    name: String,
    version: String,
}

pub fn run(command: PiCommand, invocation_cwd: &Path) -> anyhow::Result<()> {
    match command {
        PiCommand::Install {
            source,
            additional_sources,
            name,
            pi_home,
            pi_package,
            extension_root,
        } => {
            let mut sources = Vec::with_capacity(1 + additional_sources.len());
            sources.push(source);
            sources.extend(additional_sources);
            install_sources(
                &sources,
                name.as_deref(),
                pi_home.as_deref(),
                pi_package.as_deref(),
                extension_root.as_deref(),
                invocation_cwd,
            )
        }
        PiCommand::List { extension_root } => list(extension_root.as_deref(), invocation_cwd),
    }
}

#[cfg(test)]
fn install(
    source: &Path,
    requested_name: Option<&str>,
    requested_pi_home: Option<&Path>,
    requested_pi_package: Option<&Path>,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    install_sources(
        &[source.to_path_buf()],
        requested_name,
        requested_pi_home,
        requested_pi_package,
        requested_extension_root,
        invocation_cwd,
    )
}

fn install_sources(
    requested_sources: &[PathBuf],
    requested_name: Option<&str>,
    requested_pi_home: Option<&Path>,
    requested_pi_package: Option<&Path>,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    if requested_sources.is_empty() {
        anyhow::bail!("at least one Pi extension source is required");
    }
    if requested_sources.len() > MAX_AGGREGATE_SOURCES {
        anyhow::bail!(
            "Pi compatibility source set contains {} sources; limit is {MAX_AGGREGATE_SOURCES}",
            requested_sources.len()
        );
    }
    let mut sources = Vec::with_capacity(requested_sources.len());
    let mut unique_sources = BTreeSet::new();
    for requested_source in requested_sources {
        let source = resolve_source(requested_source, invocation_cwd)?;
        if !unique_sources.insert(source.clone()) {
            anyhow::bail!("duplicate Pi extension source {}", source.display());
        }
        sources.push(PiLockedSource::from_canonical_path(source, None)?);
    }
    let pi_home = resolve_pi_home(requested_pi_home, invocation_cwd)?;
    let pi_package = select_pi_package(requested_pi_package, &sources, &pi_home, invocation_cwd)?;
    let extension_root = resolve_extension_root(requested_extension_root, invocation_cwd)?;

    let aggregate = sources.len() > 1;
    let name = requested_name
        .map(validate_name)
        .transpose()?
        .unwrap_or_else(|| {
            if aggregate {
                DEFAULT_AGGREGATE_NAME.to_owned()
            } else {
                generated_name(&sources[0].canonical_path)
            }
        });
    let package = extension_root.join(&name);
    let record = PiLockRecord::new(name.clone(), sources.clone(), pi_home, pi_package)?;
    let outcome = publish_lock_extension(&record, &package)?;

    match outcome {
        PublishOutcome::Published => crate::output::stdout_line(format!(
            "Installed Pi compatibility aggregate {name} with {} ordered source(s).",
            sources.len()
        )),
        PublishOutcome::AlreadyPresent => crate::output::stdout_line(format!(
            "Pi compatibility aggregate {name} is already installed with identical locked output."
        )),
    }
    crate::output::stdout_line(
        "The aggregate remains disabled and untrusted until you explicitly enable and trust it.",
    );
    crate::output::stdout_line(format!(
        "Run: ygg --enable-extension {name} --trust-extension {name}"
    ));
    crate::output::stdout_line(
        "No Pi package code, npm lifecycle hook, or dependency installer was run.",
    );
    Ok(())
}

fn list(requested_extension_root: Option<&Path>, invocation_cwd: &Path) -> anyhow::Result<()> {
    let requested_root = resolve_extension_root(requested_extension_root, invocation_cwd)?;
    let root = match requested_root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::output::stdout_line("No Pi compatibility links installed.");
            return Ok(());
        }
        Err(error) => return Err(error).context("cannot resolve Pi compatibility link root"),
    };
    let entries = fs::read_dir(&root).context("cannot read Pi compatibility links")?;
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let lock_path = path.join(PI_LOCK_RECORD);
        if let Ok(bytes) =
            ygg_agent::secure_fs::read_regular_file_bounded(&lock_path, MAX_PI_LOCK_BYTES)
        {
            if let Ok(schema) = serde_json::from_slice::<LinkRecordSchema>(&bytes) {
                if schema.schema_version == 1 {
                    if let Ok(record) = serde_json::from_slice::<LegacyPiLockRecord>(&bytes) {
                        records.push(ParsedPiInstallation::LegacyLock(record));
                        continue;
                    }
                } else if let Ok(record) = parse_lock_record(&bytes) {
                    records.push(ParsedPiInstallation::Lock(Box::new(record)));
                    continue;
                }
            }
        }
        let record_path = path.join(LINK_RECORD);
        let bytes = match ygg_agent::secure_fs::read_regular_file_bounded(
            &record_path,
            MAX_LINK_RECORD_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let record = match parse_link_record(&bytes) {
            Ok(record) => record,
            Err(_) => continue,
        };
        records.push(ParsedPiInstallation::Link(record));
    }
    records.sort_by(|left, right| left.name().cmp(right.name()));
    if records.is_empty() {
        crate::output::stdout_line("No Pi compatibility links installed.");
        return Ok(());
    }
    for record in records {
        let pi_package = record
            .pi_package()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "auto-discovery".to_owned());
        crate::output::stdout_line(format!(
            "{} · status={} · source={} · pi_home={} · pi_package={pi_package}",
            record.name(),
            record.status(),
            record.source_summary(),
            record.pi_home().display()
        ));
    }
    Ok(())
}

fn manifest_for_lock(
    record: &PiLockRecord,
    bridge_path: &Path,
    lock_path: &Path,
) -> anyhow::Result<ExtensionManifest> {
    validate_lock_shape(record)?;
    let bridge_text = bridge_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi bridge path is not valid UTF-8: {}",
            bridge_path.display()
        )
    })?;
    let lock_text = lock_path.to_str().ok_or_else(|| {
        anyhow::anyhow!("Pi lock path is not valid UTF-8: {}", lock_path.display())
    })?;
    // Ygg stages the selected entrypoint bytes before execution. On Unix,
    // staging a dynamically linked `node` binary can break loader-relative
    // library paths, so stage the trusted bridge script and let its env shebang
    // resolve Node from Ygg's sanitized PATH instead.
    let entrypoint_command = if cfg!(unix) {
        bridge_text.to_owned()
    } else {
        "node".to_owned()
    };
    let mut entrypoint_args = Vec::new();
    if !cfg!(unix) {
        entrypoint_args.push(bridge_text.to_owned());
    }
    entrypoint_args.extend(["--lock".to_owned(), lock_text.to_owned()]);
    let mut entrypoint_env = BTreeMap::new();
    entrypoint_env.insert(
        AGGREGATE_DIGEST_ENV.to_owned(),
        record.aggregate_digest.clone(),
    );

    Ok(ExtensionManifest {
        name: record.name.clone(),
        version: BRIDGE_VERSION.to_owned(),
        api_version: EXTENSION_API_VERSION_0_2.to_owned(),
        requires_ygg: Some(format!("={YGG_VERSION}")),
        description: Some(format!(
            "Disabled-by-default Pi compatibility aggregate for {} ordered source(s), lock {}",
            record.sources.len(),
            record.aggregate_digest
        )),
        entrypoint: ExtensionEntrypoint {
            command: entrypoint_command,
            sha256: Some(record.bridge.script_digest.clone()),
            args: entrypoint_args,
            env: entrypoint_env,
        },
        capabilities: ExtensionCapabilities {
            filesystem: ExtensionFilesystemAccess::Unrestricted,
            process: true,
            network: true,
            secrets: Vec::new(),
            environment: Vec::new(),
            host_services: Vec::new(),
        },
        contributes: ManifestContributions {
            tools: Vec::new(),
            commands: vec![record.name.clone()],
            hooks: vec![
                ExtensionHook::AfterResponse,
                ExtensionHook::BeforeToolCall,
                ExtensionHook::AfterToolCall,
            ],
            ui: vec![ExtensionUiSurface::Status],
            context: true,
            tool_renderers: Vec::new(),
            notifications: true,
            confirmations: true,
            presentation: false,
            runtime_catalog: false,
            events: Vec::new(),
            roles: Vec::new(),
        },
    })
}

fn parse_link_record(bytes: &[u8]) -> anyhow::Result<ParsedPiLinkRecord> {
    let schema: LinkRecordSchema = serde_json::from_slice(bytes)?;
    match schema.schema_version {
        1 => Ok(ParsedPiLinkRecord::Legacy(serde_json::from_slice(bytes)?)),
        LINK_SCHEMA_VERSION => Ok(ParsedPiLinkRecord::V2(serde_json::from_slice(bytes)?)),
        version => anyhow::bail!("unsupported Pi link record schema {version}"),
    }
}

fn parse_lock_record(bytes: &[u8]) -> anyhow::Result<PiLockRecord> {
    let record: PiLockRecord = serde_json::from_slice(bytes)?;
    validate_lock_shape(&record)?;
    if !valid_sha256(&record.aggregate_digest) {
        anyhow::bail!("invalid Pi aggregate digest");
    }
    let actual = aggregate_lock_digest(&record)?;
    if actual != record.aggregate_digest {
        anyhow::bail!(
            "Pi aggregate lock digest mismatch: expected {}, found {actual}",
            record.aggregate_digest
        );
    }
    Ok(record)
}

pub(crate) fn validate_lock_shape(record: &PiLockRecord) -> anyhow::Result<()> {
    if record.schema_version != PI_LOCK_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported Pi aggregate lock schema {}",
            record.schema_version
        );
    }
    if record.sources.is_empty() || record.sources.len() > MAX_AGGREGATE_SOURCES {
        anyhow::bail!("invalid Pi aggregate source count {}", record.sources.len());
    }
    validate_name(&record.name)?;
    if record.profile != supported_profile() {
        anyhow::bail!("Pi aggregate lock does not select the exact supported {PROFILE_ID} profile");
    }
    if record.bridge.version != BRIDGE_VERSION || !valid_sha256(&record.bridge.script_digest) {
        anyhow::bail!("Pi aggregate lock bridge metadata is invalid");
    }
    if record.ygg_version != YGG_VERSION {
        anyhow::bail!(
            "Pi aggregate lock targets Ygg {}, but this binary is Ygg {YGG_VERSION}",
            record.ygg_version
        );
    }
    if !record.pi_home.is_absolute() || record.pi_home.to_str().is_none() {
        anyhow::bail!("Pi aggregate lock pi_home must be an absolute UTF-8 path");
    }
    if record.pi_package.name != PI_PACKAGE_NAME
        || record.pi_package.version != SUPPORTED_PI_VERSION
        || !record.pi_package.canonical_path.is_absolute()
        || record.pi_package.canonical_path.to_str().is_none()
        || !record.pi_package.metadata_path.is_absolute()
        || record.pi_package.metadata_path.to_str().is_none()
        || record.pi_package.metadata_path != record.pi_package.canonical_path.join("package.json")
        || !valid_sha256(&record.pi_package.metadata_digest)
    {
        anyhow::bail!("Pi aggregate lock package metadata is invalid");
    }
    if !record.aggregate_digest.is_empty() && !valid_sha256(&record.aggregate_digest) {
        anyhow::bail!("invalid Pi aggregate digest");
    }
    let mut unique_paths = BTreeSet::new();
    let mut unique_ids = BTreeSet::new();
    for source in &record.sources {
        if !source.enabled {
            anyhow::bail!("Pi aggregate locks contain only enabled sources");
        }
        validate_canonical_source_path(&source.canonical_path)?;
        if !unique_paths.insert(&source.canonical_path) {
            anyhow::bail!(
                "duplicate Pi aggregate source {}",
                source.canonical_path.display()
            );
        }
        if !unique_ids.insert(&source.id) {
            anyhow::bail!("duplicate Pi aggregate source id {}", source.id);
        }
        let expected_id = stable_source_id(&source.canonical_path, source.kind)?;
        if source.id != expected_id {
            anyhow::bail!("Pi aggregate source {} has an invalid stable id", source.id);
        }
        if source.source_fingerprint.algorithm != SOURCE_FINGERPRINT_ALGORITHM
            || source.source_fingerprint.format_version != SOURCE_FINGERPRINT_FORMAT
            || !valid_sha256(&source.source_fingerprint.digest)
            || source.source_fingerprint.file_count == 0
            || source.source_fingerprint.file_count > MAX_SOURCE_FILES as u64
            || source.source_fingerprint.byte_count > MAX_SOURCE_BYTES as u64
            || (source.kind == PiSourceKind::File && source.source_fingerprint.file_count != 1)
        {
            anyhow::bail!("Pi aggregate source {} fingerprint is invalid", source.id);
        }
        if source
            .dependency_lock_hash
            .as_deref()
            .is_some_and(|digest| !valid_sha256(digest))
        {
            anyhow::bail!(
                "Pi aggregate source {} dependency lock hash is invalid",
                source.id
            );
        }
    }
    let mut bounded = record.clone();
    bounded.aggregate_digest = "0".repeat(64);
    let encoded_len = serde_json::to_vec_pretty(&bounded)?
        .len()
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Pi aggregate lock size overflow"))?;
    if encoded_len > MAX_PI_LOCK_BYTES {
        anyhow::bail!(
            "Pi aggregate lock is {encoded_len} bytes; limit is {MAX_PI_LOCK_BYTES} bytes"
        );
    }
    Ok(())
}

pub(crate) fn validate_lock_preconditions(record: &PiLockRecord) -> anyhow::Result<()> {
    validate_lock_shape(record)?;
    let actual_aggregate = aggregate_lock_digest(record)?;
    if actual_aggregate != record.aggregate_digest {
        anyhow::bail!("Pi aggregate lock digest changed");
    }
    if record.bridge != bridge_metadata() {
        anyhow::bail!("Pi bridge script/profile changed after the lock was planned");
    }
    let actual_package =
        inspect_pi_package(&record.pi_package.canonical_path).map_err(|error| {
            anyhow::anyhow!(
                "selected Pi package metadata changed after the lock was planned: {error:#}"
            )
        })?;
    if actual_package != record.pi_package {
        anyhow::bail!("selected Pi package metadata changed after the lock was planned");
    }
    for source in &record.sources {
        let metadata = fs::symlink_metadata(&source.canonical_path).with_context(|| {
            format!(
                "cannot inspect locked Pi source {}",
                source.canonical_path.display()
            )
        })?;
        let actual_kind = if metadata.file_type().is_file() {
            PiSourceKind::File
        } else if metadata.file_type().is_dir() {
            PiSourceKind::Directory
        } else {
            anyhow::bail!("locked Pi source is no longer a regular file or directory");
        };
        if actual_kind != source.kind {
            anyhow::bail!("locked Pi source {} changed kind", source.id);
        }
        let actual = fingerprint_source(&source.canonical_path)?;
        if actual != source.source_fingerprint {
            anyhow::bail!("locked Pi source {} changed after planning", source.id);
        }
    }
    Ok(())
}

pub(crate) fn aggregate_lock_digest(record: &PiLockRecord) -> anyhow::Result<String> {
    let mut unsigned = record.clone();
    unsigned.aggregate_digest.clear();
    canonical_content_digest(AGGREGATE_DIGEST_DOMAIN, &unsigned)
}

pub(crate) fn canonical_content_digest<T: Serialize>(
    domain: &[u8],
    value: &T,
) -> anyhow::Result<String> {
    let value = serde_json::to_value(value)?;
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical)?;
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(canonical.as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_canonical_json(value: &Value, output: &mut String) -> anyhow::Result<()> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(&serde_json::to_string(value)?),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key)?);
                output.push(':');
                write_canonical_json(&values[key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn link_status(record: &ParsedPiLinkRecord) -> String {
    let ParsedPiLinkRecord::V2(record) = record else {
        return "legacy/stale (schema v1 lacks compatibility and source trust metadata)".to_owned();
    };
    let mut stale = Vec::new();
    if record.bridge_version != BRIDGE_VERSION {
        stale.push("bridge profile changed".to_owned());
    }
    if record.pi_version != SUPPORTED_PI_VERSION {
        stale.push("supported Pi version changed".to_owned());
    }
    if record.ygg_version != YGG_VERSION {
        stale.push("Ygg version changed".to_owned());
    }
    if record.source_fingerprint.algorithm != SOURCE_FINGERPRINT_ALGORITHM
        || record.source_fingerprint.format_version != SOURCE_FINGERPRINT_FORMAT
        || !valid_sha256(&record.source_fingerprint.digest)
    {
        stale.push("source fingerprint metadata is invalid".to_owned());
    }
    match fingerprint_source(&record.source) {
        Ok(actual) if actual != record.source_fingerprint => {
            stale.push("source changed".to_owned());
        }
        Err(error) => stale.push(format!("source cannot be verified: {error:#}")),
        Ok(_) => {}
    }
    if let Some(pi_package) = record.pi_package.as_deref() {
        if let Err(error) = resolve_pi_package(pi_package, Path::new("/")) {
            stale.push(format!("Pi package cannot be verified: {error:#}"));
        }
    }
    if stale.is_empty() {
        "metadata-current (trust not asserted)".to_owned()
    } else {
        format!("stale ({})", stale.join("; "))
    }
}

fn aggregate_status(record: &PiLockRecord) -> String {
    match validate_lock_preconditions(record) {
        Ok(()) => "aggregate-current (disabled; trust not asserted)".to_owned(),
        Err(error) => format!("stale ({error:#})"),
    }
}

fn validate_canonical_source_path(path: &Path) -> anyhow::Result<()> {
    let text = path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi extension source path is not valid UTF-8: {}",
            path.display()
        )
    })?;
    if !path.is_absolute() {
        anyhow::bail!(
            "Pi extension source path must be absolute: {}",
            path.display()
        );
    }
    if text.len() > MAX_SOURCE_PATH_BYTES {
        anyhow::bail!("Pi extension source path exceeds {MAX_SOURCE_PATH_BYTES} bytes");
    }
    Ok(())
}

fn stable_source_id(path: &Path, kind: PiSourceKind) -> anyhow::Result<String> {
    validate_canonical_source_path(path)?;
    let path = path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi extension source path is not valid UTF-8: {}",
            path.display()
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(SOURCE_ID_DOMAIN);
    hasher.update(match kind {
        PiSourceKind::File => b"file".as_slice(),
        PiSourceKind::Directory => b"directory".as_slice(),
    });
    hasher.update([0]);
    hasher.update(path.as_bytes());
    Ok(format!("pi-source-{:x}", hasher.finalize()))
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceEntry {
    Directory { relative: String },
    File { relative: String, path: PathBuf },
}

impl SourceEntry {
    fn relative(&self) -> &str {
        match self {
            Self::Directory { relative } | Self::File { relative, .. } => relative,
        }
    }

    fn tag(&self) -> u8 {
        match self {
            Self::Directory { .. } => b'd',
            Self::File { .. } => b'f',
        }
    }
}

pub(crate) fn fingerprint_source(source: &Path) -> anyhow::Result<SourceFingerprint> {
    fingerprint_source_with_limits(source, FINGERPRINT_LIMITS)
}

fn fingerprint_source_with_limits(
    source: &Path,
    limits: FingerprintLimits,
) -> anyhow::Result<SourceFingerprint> {
    if !source.is_absolute() {
        anyhow::bail!("Pi extension source must be absolute for fingerprinting");
    }
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("cannot inspect Pi extension source {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "Pi extension source fingerprint rejects symbolic links: {}",
            source.display()
        );
    }
    let canonical = source
        .canonicalize()
        .with_context(|| format!("cannot resolve Pi extension source {}", source.display()))?;
    if canonical != source {
        anyhow::bail!(
            "Pi extension source fingerprint requires a canonical, non-symlink path: {}",
            source.display()
        );
    }

    let (root_tag, mut entries) = if metadata.is_file() {
        if limits.max_files == 0 {
            anyhow::bail!("Pi extension source exceeds the 0-file fingerprint limit");
        }
        (
            b'f',
            vec![SourceEntry::File {
                relative: ".".to_owned(),
                path: source.to_owned(),
            }],
        )
    } else if metadata.is_dir() {
        (b'd', collect_source_entries(source, limits)?)
    } else {
        anyhow::bail!(
            "Pi extension source fingerprint accepts only regular files or directories: {}",
            source.display()
        );
    };
    entries.sort_by(|left, right| {
        left.relative()
            .cmp(right.relative())
            .then_with(|| left.tag().cmp(&right.tag()))
    });

    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-source-fingerprint\0");
    hasher.update(SOURCE_FINGERPRINT_FORMAT.to_be_bytes());
    hasher.update([root_tag]);
    let mut total_bytes = 0usize;
    let mut file_count = 0usize;
    for entry in &entries {
        hasher.update([entry.tag()]);
        hash_framed(&mut hasher, entry.relative().as_bytes());
        if let SourceEntry::File { path, .. } = entry {
            let remaining = limits.max_bytes.saturating_sub(total_bytes);
            let read = hash_regular_file(&mut hasher, path, remaining, limits.max_bytes)?;
            total_bytes = total_bytes
                .checked_add(read)
                .ok_or_else(|| anyhow::anyhow!("Pi source fingerprint byte count overflow"))?;
            file_count += 1;
        }
    }

    if metadata.is_dir() {
        let mut after = collect_source_entries(source, limits)?;
        after.sort_by(|left, right| {
            left.relative()
                .cmp(right.relative())
                .then_with(|| left.tag().cmp(&right.tag()))
        });
        if after != entries {
            anyhow::bail!("Pi extension source tree changed while it was being fingerprinted");
        }
    }

    Ok(SourceFingerprint {
        algorithm: SOURCE_FINGERPRINT_ALGORITHM.to_owned(),
        format_version: SOURCE_FINGERPRINT_FORMAT,
        digest: format!("{:x}", hasher.finalize()),
        file_count: u64::try_from(file_count)?,
        byte_count: u64::try_from(total_bytes)?,
    })
}

fn should_skip_fingerprint_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | ".pytest_cache" | "__pycache__" | "node_modules" | "target")
    )
}

fn collect_source_entries(
    root: &Path,
    limits: FingerprintLimits,
) -> anyhow::Result<Vec<SourceEntry>> {
    let mut entries = Vec::new();
    let mut directories = vec![root.to_owned()];
    let mut files = 0usize;
    while let Some(directory) = directories.pop() {
        let children = fs::read_dir(&directory).with_context(|| {
            format!(
                "cannot completely fingerprint Pi source directory {}",
                directory.display()
            )
        })?;
        for child in children {
            let child = child?;
            let path = child.path();
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("cannot inspect Pi source entry {}", path.display()))?;
            if metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "Pi extension source fingerprint rejects symbolic link {}",
                    path.display()
                );
            }
            if entries.len() >= limits.max_entries {
                anyhow::bail!(
                    "Pi extension source exceeds the {}-entry fingerprint limit",
                    limits.max_entries
                );
            }
            let relative = stable_relative_path(root, &path)?;
            if metadata.is_dir() {
                if should_skip_fingerprint_directory(&path) {
                    continue;
                }
                entries.push(SourceEntry::Directory { relative });
                directories.push(path);
            } else if metadata.is_file() {
                if files >= limits.max_files {
                    anyhow::bail!(
                        "Pi extension source exceeds the {}-file fingerprint limit",
                        limits.max_files
                    );
                }
                files += 1;
                entries.push(SourceEntry::File { relative, path });
            } else {
                anyhow::bail!(
                    "Pi extension source fingerprint rejects non-regular entry {}",
                    path.display()
                );
            }
        }
    }
    Ok(entries)
}

fn stable_relative_path(root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "Pi source entry {} is outside {}",
            path.display(),
            root.display()
        )
    })?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            anyhow::bail!(
                "Pi source entry has an unstable relative path: {}",
                path.display()
            );
        };
        let component = component.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "Pi source entry path is not valid UTF-8: {}",
                path.display()
            )
        })?;
        components.push(component);
    }
    if components.is_empty() || components.len() > MAX_SOURCE_DEPTH {
        anyhow::bail!(
            "Pi source entry has an unsupported path depth: {}",
            path.display()
        );
    }
    let relative = components.join("/");
    if relative.len() > MAX_SOURCE_PATH_BYTES {
        anyhow::bail!("Pi source relative path exceeds {MAX_SOURCE_PATH_BYTES} bytes");
    }
    Ok(relative)
}

fn hash_framed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hash_regular_file(
    hasher: &mut Sha256,
    path: &Path,
    remaining: usize,
    total_limit: usize,
) -> anyhow::Result<usize> {
    let mut file = ygg_agent::secure_fs::open_regular_file_for_read(path)
        .with_context(|| format!("cannot securely fingerprint {}", path.display()))?;
    let before = file.metadata()?;
    if before.len() > remaining as u64 {
        anyhow::bail!(
            "Pi extension source exceeds the {total_limit}-byte fingerprint limit at {}",
            path.display()
        );
    }
    let expected = usize::try_from(before.len())?;
    hasher.update(before.len().to_be_bytes());
    let mut read = 0usize;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let allowed = remaining.saturating_sub(read);
        let request = allowed.saturating_add(1).min(buffer.len());
        let count = match file.read(&mut buffer[..request]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        if count > allowed {
            anyhow::bail!(
                "Pi extension source exceeds the {total_limit}-byte fingerprint limit at {}",
                path.display()
            );
        }
        hasher.update(&buffer[..count]);
        read += count;
    }
    let after = file.metadata()?;
    if read != expected
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        anyhow::bail!(
            "Pi source file changed while being fingerprinted: {}",
            path.display()
        );
    }
    Ok(read)
}

fn resolve_source(source: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = if source.is_absolute() {
        source.to_owned()
    } else {
        cwd.join(source)
    };
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect Pi extension source {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "Pi extension source must not be a symlink: {}",
            path.display()
        );
    }
    if !metadata.is_file() && !metadata.is_dir() {
        anyhow::bail!(
            "Pi extension source must be a regular file or directory: {}",
            path.display()
        );
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve Pi extension source {}", path.display()))?;
    if canonical.to_string_lossy().len() > MAX_SOURCE_PATH_BYTES {
        anyhow::bail!("Pi extension source path exceeds {MAX_SOURCE_PATH_BYTES} bytes");
    }
    Ok(canonical)
}

pub(crate) fn resolve_pi_package(path: &Path, cwd: &Path) -> anyhow::Result<PiPackageMetadata> {
    let selected = absolute_path(path, cwd)?;
    inspect_pi_package(&selected)
}

fn inspect_pi_package(selected: &Path) -> anyhow::Result<PiPackageMetadata> {
    let metadata = fs::symlink_metadata(selected)
        .with_context(|| format!("cannot inspect Pi package root {}", selected.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "Pi package root must be a non-symlink directory: {}",
            selected.display()
        );
    }
    let root = selected
        .canonicalize()
        .with_context(|| format!("cannot resolve Pi package root {}", selected.display()))?;
    if root != selected {
        anyhow::bail!(
            "Pi package root must already be canonical and may not traverse symlinks: {}",
            selected.display()
        );
    }
    let manifest_path = root.join("package.json");
    let bytes = ygg_agent::secure_fs::read_regular_file_bounded(
        &manifest_path,
        MAX_PI_PACKAGE_MANIFEST_BYTES,
    )
    .with_context(|| {
        format!(
            "cannot read Pi package manifest {}",
            manifest_path.display()
        )
    })?;
    let manifest: PiPackageManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid Pi package manifest {}", manifest_path.display()))?;
    if manifest.name != PI_PACKAGE_NAME || manifest.version != SUPPORTED_PI_VERSION {
        anyhow::bail!(
            "Pi package must be {PI_PACKAGE_NAME}@{SUPPORTED_PI_VERSION}; found {}@{}",
            manifest.name,
            manifest.version
        );
    }
    let entrypoint = root.join("dist/index.js");
    let entrypoint_metadata = fs::symlink_metadata(&entrypoint)
        .with_context(|| format!("Pi package entrypoint is missing: {}", entrypoint.display()))?;
    if entrypoint_metadata.file_type().is_symlink() || !entrypoint_metadata.is_file() {
        anyhow::bail!(
            "Pi package entrypoint must be a regular non-symlink file: {}",
            entrypoint.display()
        );
    }
    let resolved_entrypoint = entrypoint.canonicalize().with_context(|| {
        format!(
            "cannot resolve Pi package entrypoint {}",
            entrypoint.display()
        )
    })?;
    if resolved_entrypoint != entrypoint || !resolved_entrypoint.starts_with(&root) {
        anyhow::bail!(
            "Pi package entrypoint must remain inside the canonical package root: {}",
            entrypoint.display()
        );
    }
    Ok(PiPackageMetadata {
        canonical_path: root,
        metadata_path: manifest_path,
        metadata_digest: format!("{:x}", Sha256::digest(&bytes)),
        name: manifest.name,
        version: manifest.version,
    })
}

pub(crate) fn select_pi_package(
    requested: Option<&Path>,
    sources: &[PiLockedSource],
    pi_home: &Path,
    cwd: &Path,
) -> anyhow::Result<PiPackageMetadata> {
    if let Some(path) = requested {
        return resolve_pi_package(path, cwd);
    }

    let mut candidates = Vec::new();
    for name in ["YGG_PI_PACKAGE", "PI_CODING_AGENT_PACKAGE"] {
        if let Some(path) = std::env::var_os(name) {
            candidates.push(PathBuf::from(path));
        }
    }
    candidates.push(
        pi_home
            .join("npm/node_modules")
            .join("@earendil-works/pi-coding-agent"),
    );
    for source in sources {
        let mut current = if source.kind == PiSourceKind::File {
            source.canonical_path.parent()
        } else {
            Some(source.canonical_path.as_path())
        };
        for _ in 0..8 {
            let Some(directory) = current else { break };
            candidates.push(directory.to_path_buf());
            candidates.push(
                directory
                    .join("node_modules")
                    .join("@earendil-works/pi-coding-agent"),
            );
            current = directory.parent();
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let executable = directory.join(if cfg!(windows) { "pi.exe" } else { "pi" });
            if let Ok(executable) = executable.canonicalize() {
                for ancestor in executable.ancestors().take(8).skip(1) {
                    candidates.push(ancestor.to_path_buf());
                    candidates.push(
                        ancestor
                            .join("node_modules")
                            .join("@earendil-works/pi-coding-agent"),
                    );
                }
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.extend([
            home.join(".local/lib/node_modules/@earendil-works/pi-coding-agent"),
            home.join(".npm-global/lib/node_modules/@earendil-works/pi-coding-agent"),
        ]);
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent"),
        PathBuf::from("/usr/local/lib/node_modules/@earendil-works/pi-coding-agent"),
    ]);

    let mut seen = BTreeSet::new();
    let mut incompatible = Vec::new();
    for candidate in candidates.into_iter().take(1024) {
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            cwd.join(candidate)
        };
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        match inspect_pi_package(&canonical) {
            Ok(package) => return Ok(package),
            Err(error) => {
                if incompatible.len() < 8 {
                    incompatible.push(error.to_string());
                }
            }
        }
    }
    let detail = if incompatible.is_empty() {
        String::new()
    } else {
        format!(
            " Inspected incompatible candidates: {}",
            incompatible.join("; ")
        )
    };
    anyhow::bail!(
        "could not locate exact {PI_PACKAGE_NAME}@{SUPPORTED_PI_VERSION} metadata without executing Pi; pass --pi-package DIR.{detail}"
    )
}

fn resolve_pi_home(requested: Option<&Path>, cwd: &Path) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        return absolute_path(path, cwd);
    }
    if let Some(path) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return absolute_path(Path::new(&path), cwd);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("user home is unavailable"))?;
    Ok(home.join(".pi/agent"))
}

pub(crate) fn resolve_extension_root(
    requested: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        return absolute_path(path, cwd);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("user home is unavailable"))?;
    Ok(home.join(".ygg/extensions"))
}

pub(crate) fn absolute_path(path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    normalize_absolute(&path)
}

fn normalize_absolute(path: &Path) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("path must be absolute: {}", path.display());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("path escapes its filesystem root: {}", path.display());
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    if normalized.to_str().is_none() {
        anyhow::bail!("path must be valid UTF-8: {}", normalized.display());
    }
    Ok(normalized)
}

fn path_matches_canonical_identity(path: &Path, canonical: &Path) -> bool {
    if path == canonical {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(relative) = path.strip_prefix("/var") {
            return Path::new("/private/var").join(relative) == canonical;
        }
    }
    false
}

fn reject_symlink(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("{label} must not be a symlink: {}", path.display());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DestinationInspection {
    Absent,
    Identical { output_digest: String },
    Conflict { reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    Published,
    AlreadyPresent,
}

struct GeneratedOutput {
    bridge: Vec<u8>,
    manifest: Vec<u8>,
    lock: Vec<u8>,
    digest: String,
}

fn generated_output(record: &PiLockRecord, destination: &Path) -> anyhow::Result<GeneratedOutput> {
    validate_lock_shape(record)?;
    if aggregate_lock_digest(record)? != record.aggregate_digest {
        anyhow::bail!("cannot generate output from a tampered Pi aggregate lock");
    }
    if !destination.is_absolute()
        || destination.file_name().and_then(|name| name.to_str()) != Some(record.name.as_str())
    {
        anyhow::bail!(
            "Pi aggregate destination must be an absolute directory named {:?}",
            record.name
        );
    }
    let bridge_path = destination.join("bridge.mjs");
    let lock_path = destination.join(PI_LOCK_RECORD);
    let bridge = bridge_script_bytes().to_vec();
    let manifest =
        toml::to_string_pretty(&manifest_for_lock(record, &bridge_path, &lock_path)?)?.into_bytes();
    let mut lock = serde_json::to_string_pretty(record)?.into_bytes();
    lock.push(b'\n');
    let mut hasher = Sha256::new();
    hasher.update(OUTPUT_DIGEST_DOMAIN);
    for (name, mode, bytes) in [
        ("bridge.mjs", 0o700_u32, bridge.as_slice()),
        ("extension.toml", 0o600_u32, manifest.as_slice()),
        (PI_LOCK_RECORD, 0o600_u32, lock.as_slice()),
    ] {
        hash_framed(&mut hasher, name.as_bytes());
        hasher.update(mode.to_be_bytes());
        hash_framed(&mut hasher, bytes);
    }
    Ok(GeneratedOutput {
        bridge,
        manifest,
        lock,
        digest: format!("{:x}", hasher.finalize()),
    })
}

pub(crate) fn inspect_destination(
    record: &PiLockRecord,
    destination: &Path,
) -> anyhow::Result<DestinationInspection> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DestinationInspection::Absent);
        }
        Err(error) => return Err(error).context("cannot inspect Pi aggregate destination"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(DestinationInspection::Conflict {
            reason: "destination is not a regular non-symlink directory".to_owned(),
        });
    }
    let canonical = destination
        .canonicalize()
        .context("cannot canonicalize Pi aggregate destination")?;
    if !path_matches_canonical_identity(destination, &canonical) {
        return Ok(DestinationInspection::Conflict {
            reason: "destination path traverses an unrecognized symlink or is not canonical"
                .to_owned(),
        });
    }
    let output = generated_output(record, &canonical)?;
    let destination = canonical.as_path();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7777 != 0o700 {
            return Ok(DestinationInspection::Conflict {
                reason: "destination directory is not private mode 0700".to_owned(),
            });
        }
    }

    let expected_names = BTreeSet::from([
        "bridge.mjs".to_owned(),
        "extension.toml".to_owned(),
        PI_LOCK_RECORD.to_owned(),
    ]);
    let mut actual_names = BTreeSet::new();
    for entry in fs::read_dir(destination).context("cannot read Pi aggregate destination")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Ok(DestinationInspection::Conflict {
                reason: "destination contains a non-UTF-8 entry".to_owned(),
            });
        };
        actual_names.insert(name);
    }
    if actual_names != expected_names {
        return Ok(DestinationInspection::Conflict {
            reason: "destination contains missing or unrelated files".to_owned(),
        });
    }

    for (name, expected_mode, expected_bytes, limit) in [
        (
            "bridge.mjs",
            0o700_u32,
            output.bridge.as_slice(),
            MAX_GENERATED_FILE_BYTES,
        ),
        (
            "extension.toml",
            0o600_u32,
            output.manifest.as_slice(),
            MAX_GENERATED_FILE_BYTES,
        ),
        (
            PI_LOCK_RECORD,
            0o600_u32,
            output.lock.as_slice(),
            MAX_PI_LOCK_BYTES,
        ),
    ] {
        let path = destination.join(name);
        let actual = match ygg_agent::secure_fs::read_regular_file_bounded(&path, limit) {
            Ok(actual) => actual,
            Err(error) => {
                return Ok(DestinationInspection::Conflict {
                    reason: format!("{name} is not the expected regular file: {error}"),
                });
            }
        };
        if actual != expected_bytes {
            return Ok(DestinationInspection::Conflict {
                reason: format!("{name} content differs from the planned output"),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::symlink_metadata(&path)?.permissions().mode() & 0o7777;
            if mode != expected_mode {
                return Ok(DestinationInspection::Conflict {
                    reason: format!(
                        "{name} permissions are {mode:04o}, expected {expected_mode:04o}"
                    ),
                });
            }
        }
        #[cfg(not(unix))]
        let _ = expected_mode;
    }
    Ok(DestinationInspection::Identical {
        output_digest: output.digest,
    })
}

pub(crate) fn publish_lock_extension(
    record: &PiLockRecord,
    destination: &Path,
) -> anyhow::Result<PublishOutcome> {
    validate_lock_preconditions(record)?;
    match inspect_destination(record, destination)? {
        DestinationInspection::Identical { .. } => return Ok(PublishOutcome::AlreadyPresent),
        DestinationInspection::Conflict { reason } => {
            anyhow::bail!(
                "Pi aggregate destination conflict at {}: {reason}",
                destination.display()
            );
        }
        DestinationInspection::Absent => {}
    }

    let root = destination.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi aggregate destination has no extension root: {}",
            destination.display()
        )
    })?;
    ygg_agent::secure_fs::create_private_directory_all(root).with_context(|| {
        format!(
            "cannot create private Ygg extension root {}",
            root.display()
        )
    })?;
    reject_symlink(root, "Ygg extension root")?;
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("cannot canonicalize Ygg extension root {}", root.display()))?;
    if !path_matches_canonical_identity(root, &canonical_root) {
        anyhow::bail!(
            "Ygg extension root path traverses an unrecognized symlink: {}",
            root.display()
        );
    }
    let destination_name = destination.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi aggregate destination has no directory name: {}",
            destination.display()
        )
    })?;
    let canonical_destination = canonical_root.join(destination_name);
    let root = canonical_root.as_path();
    let destination = canonical_destination.as_path();

    let output = generated_output(record, destination)?;
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{}-staging-", record.name))
        .tempdir_in(root)
        .context("cannot create private same-filesystem Pi aggregate staging directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))?;
    }
    for (name, bytes) in [
        ("bridge.mjs", output.bridge.as_slice()),
        ("extension.toml", output.manifest.as_slice()),
        (PI_LOCK_RECORD, output.lock.as_slice()),
    ] {
        ygg_agent::secure_fs::write_private_atomic(
            &staging.path().join(name),
            bytes,
            MAX_GENERATED_FILE_BYTES,
        )
        .with_context(|| format!("cannot stage generated Pi aggregate {name}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bridge = staging.path().join("bridge.mjs");
        fs::set_permissions(&bridge, fs::Permissions::from_mode(0o700))?;
        fs::File::open(&bridge)?.sync_all()?;
    }
    sync_directory(staging.path())?;

    // Close the planning-to-publication window as far as a local filesystem
    // transaction permits. The runtime repeats these checks before import and
    // again before constructing its single ExtensionRunner.
    validate_lock_preconditions(record)?;
    match inspect_destination(record, destination)? {
        DestinationInspection::Absent => {}
        DestinationInspection::Identical { .. } => return Ok(PublishOutcome::AlreadyPresent),
        DestinationInspection::Conflict { reason } => {
            anyhow::bail!("Pi aggregate destination drifted during staging: {reason}");
        }
    }

    match atomic_rename_noreplace(staging.path(), destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return match inspect_destination(record, destination)? {
                DestinationInspection::Identical { .. } => Ok(PublishOutcome::AlreadyPresent),
                DestinationInspection::Conflict { reason } => anyhow::bail!(
                    "atomic Pi aggregate publication lost a destination race: {reason}"
                ),
                DestinationInspection::Absent => Err(error).context(
                    "atomic Pi aggregate publication reported a conflict but no destination exists",
                ),
            };
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot atomically publish Pi aggregate from {} to {}",
                    staging.path().display(),
                    destination.display()
                )
            });
        }
    }
    sync_directory(root)?;
    match inspect_destination(record, destination)? {
        DestinationInspection::Identical { output_digest } if output_digest == output.digest => {
            Ok(PublishOutcome::Published)
        }
        DestinationInspection::Identical { .. } => {
            anyhow::bail!("published Pi aggregate output digest is inconsistent")
        }
        DestinationInspection::Conflict { reason } => {
            anyhow::bail!("published Pi aggregate failed verification: {reason}")
        }
        DestinationInspection::Absent => {
            anyhow::bail!("published Pi aggregate destination disappeared")
        }
    }
}

#[cfg(target_os = "linux")]
fn atomic_rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: both C strings are live and NUL-terminated for the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn atomic_rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: both C strings are live and NUL-terminated for renamex_np.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn atomic_rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn atomic_rename_noreplace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace directory publication is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> anyhow::Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn validate_name(name: &str) -> anyhow::Result<String> {
    if name.len() > 64
        || name.is_empty()
        || !name.chars().enumerate().all(|(index, character)| {
            (index == 0 && character.is_ascii_lowercase())
                || (index > 0
                    && (character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || character == '-'))
        })
    {
        anyhow::bail!(
            "invalid Pi compatibility name {name:?}; use a lowercase letter followed by lowercase letters, digits, or '-'")
    }
    Ok(name.to_owned())
}

fn generated_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .or_else(|| source.file_name())
        .and_then(|value| value.to_str())
        .unwrap_or("extension")
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { "extension" } else { stem };
    const MAX_GENERATED_STEM_BYTES: usize = 52;
    let stem = stem[..stem.len().min(MAX_GENERATED_STEM_BYTES)].trim_end_matches('-');
    let stem = if stem.is_empty() { "extension" } else { stem };
    let digest = format!("{:x}", Sha256::digest(source.to_string_lossy().as_bytes()));
    format!("pi-{stem}-{}", &digest[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    fn create_pi_package(root: &Path) -> PathBuf {
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(
            root.join("package.json"),
            format!(r#"{{"name":"{PI_PACKAGE_NAME}","version":"{SUPPORTED_PI_VERSION}"}}"#),
        )
        .unwrap();
        fs::write(root.join("dist/index.js"), b"export {};\n").unwrap();
        canonical(root)
    }

    #[test]
    fn generated_names_are_stable_and_lowercase() {
        let name = generated_name(Path::new("/tmp/My Extension.ts"));
        assert!(name.starts_with("pi-my-extension-"));
        assert!(validate_name(&name).is_ok());

        let long = generated_name(Path::new(&format!("/tmp/{}.ts", "A".repeat(200))));
        assert_eq!(long.len(), 64);
        assert!(validate_name(&long).is_ok());
    }

    #[test]
    fn fingerprints_are_deterministic_and_path_relative() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(first.join("nested")).unwrap();
        fs::write(first.join("z.ts"), b"export const z = 1;\n").unwrap();
        fs::write(first.join("nested/a.ts"), b"export const a = 2;\n").unwrap();
        fs::create_dir_all(second.join("nested")).unwrap();
        fs::write(second.join("nested/a.ts"), b"export const a = 2;\n").unwrap();
        fs::write(second.join("z.ts"), b"export const z = 1;\n").unwrap();

        let first = canonical(&first);
        let second = canonical(&second);
        let expected = fingerprint_source(&first).unwrap();
        assert_eq!(expected, fingerprint_source(&first).unwrap());
        assert_eq!(expected, fingerprint_source(&second).unwrap());
    }

    #[test]
    fn fingerprints_ignore_dependency_and_build_output_directories() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("package");
        fs::create_dir_all(source.join("node_modules/dependency")).unwrap();
        fs::create_dir_all(source.join("target/debug")).unwrap();
        fs::write(source.join("extension.ts"), b"source").unwrap();
        fs::write(source.join("node_modules/dependency/index.js"), b"first").unwrap();
        fs::write(source.join("target/debug/output"), b"first").unwrap();
        let source = canonical(&source);
        let before = fingerprint_source(&source).unwrap();
        fs::write(source.join("node_modules/dependency/index.js"), b"second").unwrap();
        fs::write(source.join("target/debug/output"), b"second").unwrap();
        assert_eq!(before, fingerprint_source(&source).unwrap());
    }

    #[test]
    fn fingerprints_detect_source_changes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("extension.ts");
        fs::write(&source, b"before").unwrap();
        let source = canonical(&source);
        let before = fingerprint_source(&source).unwrap();
        let locked = PiLockedSource::from_canonical_path(source.clone(), None).unwrap();
        let pi_package = resolve_pi_package(
            &create_pi_package(&temp.path().join("pi-package")),
            temp.path(),
        )
        .unwrap();
        let record = PiLockRecord::new(
            "pi-example".to_owned(),
            vec![locked],
            temp.path().join("pi-home"),
            pi_package,
        )
        .unwrap();

        fs::write(&source, b"after").unwrap();
        let after = fingerprint_source(&source).unwrap();
        assert_ne!(before.digest, after.digest);
        assert!(aggregate_status(&record).contains("changed after planning"));
    }

    #[test]
    fn fingerprint_bounds_reject_incomplete_trees() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.ts"), b"abc").unwrap();
        fs::write(temp.path().join("b.ts"), b"def").unwrap();
        let root = canonical(temp.path());
        let file_error = fingerprint_source_with_limits(
            &root,
            FingerprintLimits {
                max_files: 1,
                max_entries: 8,
                max_bytes: 32,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(file_error.contains("1-file fingerprint limit"));

        let byte_error = fingerprint_source_with_limits(
            &root,
            FingerprintLimits {
                max_files: 8,
                max_entries: 8,
                max_bytes: 5,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(byte_error.contains("5-byte fingerprint limit"));
    }

    #[cfg(unix)]
    #[test]
    fn fingerprints_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("target.ts"), b"target").unwrap();
        symlink("target.ts", temp.path().join("linked.ts")).unwrap();
        let error = fingerprint_source(&canonical(temp.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("symbolic link"));
    }

    #[test]
    fn legacy_v1_records_remain_readable_and_are_marked_stale() {
        let bytes = br#"{
            "schema_version": 1,
            "bridge_version": "0.1.1",
            "name": "pi-legacy",
            "source": "/tmp/legacy.ts",
            "pi_home": "/tmp/pi-home"
        }"#;
        let record = parse_link_record(bytes).unwrap();
        assert!(matches!(record, ParsedPiLinkRecord::Legacy(_)));
        assert!(link_status(&record).starts_with("legacy/stale"));
    }

    #[test]
    fn schema_v2_lock_and_manifest_bind_the_ordered_runtime_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.ts");
        let second = temp.path().join("second.ts");
        fs::write(&first, b"export default () => 'first';\n").unwrap();
        fs::write(&second, b"export default () => 'second';\n").unwrap();
        let first = canonical(&first);
        let second = canonical(&second);
        let sources = vec![
            PiLockedSource::from_canonical_path(first.clone(), None).unwrap(),
            PiLockedSource::from_canonical_path(second.clone(), Some("a".repeat(64))).unwrap(),
        ];
        let package_root = create_pi_package(&temp.path().join("pi-package"));
        let package = resolve_pi_package(&package_root, temp.path()).unwrap();
        let record = PiLockRecord::new(
            "pi-aggregate".into(),
            sources.clone(),
            temp.path().join("pi-home"),
            package,
        )
        .unwrap();
        let encoded = serde_json::to_vec(&record).unwrap();
        let decoded = parse_lock_record(&encoded).unwrap();
        assert_eq!(decoded.sources, sources);
        assert_eq!(decoded.schema_version, PI_LOCK_SCHEMA_VERSION);
        assert_eq!(decoded.profile, supported_profile());
        assert_eq!(decoded.bridge, bridge_metadata());
        assert_eq!(
            aggregate_status(&decoded),
            "aggregate-current (disabled; trust not asserted)"
        );

        let destination = temp.path().join("pi-aggregate");
        let bridge_path = destination.join("bridge.mjs");
        let lock_path = destination.join(PI_LOCK_RECORD);
        let manifest = manifest_for_lock(&record, &bridge_path, &lock_path).unwrap();
        assert_eq!(manifest.version, BRIDGE_VERSION);
        assert_eq!(
            manifest.requires_ygg.as_deref(),
            Some(concat!("=", env!("CARGO_PKG_VERSION")))
        );
        let mut expected_args = Vec::new();
        if cfg!(unix) {
            assert_eq!(manifest.entrypoint.command, bridge_path.to_str().unwrap());
        } else {
            assert_eq!(manifest.entrypoint.command, "node");
            expected_args.push(bridge_path.to_string_lossy().into_owned());
        }
        expected_args.extend([
            "--lock".to_owned(),
            lock_path.to_string_lossy().into_owned(),
        ]);
        assert_eq!(manifest.entrypoint.args, expected_args);
        assert_eq!(
            manifest.entrypoint.sha256.as_deref(),
            Some(record.bridge.script_digest.as_str())
        );
        assert_eq!(
            manifest.entrypoint.env.get(AGGREGATE_DIGEST_ENV),
            Some(&record.aggregate_digest)
        );
        assert_eq!(manifest.contributes.commands, ["pi-aggregate"]);
        assert_eq!(manifest.contributes.hooks.len(), 3);
        assert!(!manifest
            .contributes
            .hooks
            .contains(&ExtensionHook::BeforePrompt));
        assert!(manifest.contributes.context);

        fs::write(&second, b"export default () => 'changed';\n").unwrap();
        assert!(aggregate_status(&decoded).contains("changed after planning"));
    }

    #[test]
    fn aggregate_install_publishes_one_inert_locked_process_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.ts");
        let second = temp.path().join("second.ts");
        fs::write(&first, b"export default () => {};\n").unwrap();
        fs::write(&second, b"export default () => {};\n").unwrap();
        let pi_package = create_pi_package(&temp.path().join("pi-package"));
        let extension_root = temp.path().join("extensions");
        let install_once = || {
            install_sources(
                &[first.clone(), second.clone()],
                Some("pi-aggregate"),
                Some(&temp.path().join("pi-home")),
                Some(&pi_package),
                Some(&extension_root),
                temp.path(),
            )
        };
        install_once().unwrap();
        install_once().unwrap();

        let destination = extension_root.join("pi-aggregate");
        assert!(destination.join("bridge.mjs").is_file());
        assert!(destination.join("extension.toml").is_file());
        assert!(destination.join(PI_LOCK_RECORD).is_file());
        assert!(!destination.join(LINK_RECORD).exists());
        let lock = parse_lock_record(&fs::read(destination.join(PI_LOCK_RECORD)).unwrap()).unwrap();
        assert_eq!(lock.sources.len(), 2);
        assert_eq!(lock.name, "pi-aggregate");
        let effective_destination = destination.canonicalize().unwrap();
        let expected = generated_output(&lock, &effective_destination)
            .unwrap()
            .digest;
        assert_eq!(
            inspect_destination(&lock, &destination).unwrap(),
            DestinationInspection::Identical {
                output_digest: expected
            }
        );
    }

    #[test]
    fn atomic_publisher_refuses_to_replace_a_conflicting_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("extension.ts");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let package_root = create_pi_package(&temp.path().join("pi-package"));
        let record = PiLockRecord::new(
            "pi-conflict".to_owned(),
            vec![PiLockedSource::from_canonical_path(canonical(&source), None).unwrap()],
            temp.path().join("pi-home"),
            resolve_pi_package(&package_root, temp.path()).unwrap(),
        )
        .unwrap();
        let extension_root = canonical(temp.path()).join("extensions");
        fs::create_dir_all(extension_root.join("pi-conflict")).unwrap();
        fs::write(
            extension_root.join("pi-conflict/unrelated"),
            b"must survive",
        )
        .unwrap();
        let destination = extension_root.join("pi-conflict");

        let error = publish_lock_extension(&record, &destination)
            .unwrap_err()
            .to_string();
        assert!(error.contains("destination conflict"));
        assert_eq!(
            fs::read(destination.join("unrelated")).unwrap(),
            b"must survive"
        );
        assert!(!destination.join(PI_LOCK_RECORD).exists());
    }

    #[test]
    fn selected_pi_package_metadata_is_digest_bound() {
        let temp = tempfile::tempdir().unwrap();
        let package_root = create_pi_package(&temp.path().join("pi-package"));
        let package = resolve_pi_package(&package_root, temp.path()).unwrap();
        let source = temp.path().join("extension.ts");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let record = PiLockRecord::new(
            "pi-example".to_owned(),
            vec![PiLockedSource::from_canonical_path(canonical(&source), None).unwrap()],
            temp.path().join("pi-home"),
            package,
        )
        .unwrap();

        fs::write(
            package_root.join("package.json"),
            r#"{"name":"@earendil-works/pi-coding-agent","version":"0.85.0"}"#,
        )
        .unwrap();
        let error = resolve_pi_package(&package_root, temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains(SUPPORTED_PI_VERSION));
        assert!(aggregate_status(&record).contains("package metadata changed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_link_negotiates_runtime_commands_with_the_real_ygg_host() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let fake_pi = workspace_root.join("extensions/ygg-pi-compat/tests/fixtures/fake-pi");
        if !fake_pi.exists() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let fixture_extension = temp.path().join("pi-source");
        fs::create_dir_all(fixture_extension.join("nested")).unwrap();
        fs::create_dir_all(fixture_extension.join("node_modules/ignored")).unwrap();
        fs::write(
            fixture_extension.join("index.mjs"),
            b"export default function fixtureExtension() {}\n",
        )
        .unwrap();
        fs::write(
            fixture_extension.join("nested/state.json"),
            b"{\"ok\":true}\n",
        )
        .unwrap();
        fs::write(
            fixture_extension.join("node_modules/ignored/index.js"),
            b"ignored build output\n",
        )
        .unwrap();
        let extension_root = temp.path().join("extensions");
        install(
            &fixture_extension,
            Some("pi-host-integration"),
            Some(&temp.path().join("pi-home")),
            Some(&fake_pi),
            Some(&extension_root),
            temp.path(),
        )
        .unwrap();
        let manifest_path = extension_root.join("pi-host-integration/extension.toml");
        let mut policy = ygg_agent::ExtensionPolicy::default();
        policy.enable("pi-host-integration");
        policy.trust_for_invocation("pi-host-integration");
        let catalog = ygg_agent::ExtensionCatalog::load_resolved(
            [ygg_agent::extension_process::ExtensionManifestInput {
                path: manifest_path,
                source: ygg_agent::ExtensionSource::Explicit,
            }],
            &policy,
            64 * 1024,
        );
        assert!(catalog.diagnostics.is_empty(), "{:?}", catalog.diagnostics);
        let descriptor = catalog.extensions.into_iter().next().unwrap();
        let stale_descriptor = descriptor.clone();
        let process = ygg_agent::ExtensionProcess::start(
            descriptor,
            ygg_agent::ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        .unwrap();
        assert!(process
            .negotiated_features()
            .contains(ygg_agent::EXTENSION_FEATURE_RUNTIME_COMMANDS));
        let commands = &process.contributions().commands;
        assert!(commands.iter().any(|command| command.name == "ui-methods"));
        assert!(!commands
            .iter()
            .any(|command| command.name == "pi-host-integration"));
        let output = process
            .execute_command("ui-methods", Vec::new(), process.current_context())
            .await
            .unwrap();
        assert!(output.text.contains("completed"));
        assert!(process.shutdown().await);

        fs::write(
            fixture_extension.join("nested/state.json"),
            b"{\"ok\":false}\n",
        )
        .unwrap();
        let stale_error = match ygg_agent::ExtensionProcess::start(
            stale_descriptor,
            ygg_agent::ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        {
            Ok(process) => {
                process.shutdown().await;
                panic!("changed Pi source unexpectedly started")
            }
            Err(error) => error,
        };
        assert!(stale_error
            .to_string()
            .contains("source changed after aggregate locking"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_link_runs_the_pinned_real_pi_hello_example_when_selected() {
        let Some(pi_package) = std::env::var_os("YGG_PI_REAL_PACKAGE").map(PathBuf::from) else {
            return;
        };
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let source = pi_package.join("examples/extensions/hello.ts");
        if !source.is_file() {
            panic!("YGG_PI_REAL_PACKAGE does not contain examples/extensions/hello.ts");
        }
        let temp = tempfile::tempdir().unwrap();
        let extension_root = temp.path().join("extensions");
        install(
            &source,
            Some("pi-real-hello"),
            Some(&temp.path().join("pi-home")),
            Some(&pi_package),
            Some(&extension_root),
            temp.path(),
        )
        .unwrap();
        let manifest_path = extension_root.join("pi-real-hello/extension.toml");
        let mut policy = ygg_agent::ExtensionPolicy::default();
        policy.enable("pi-real-hello");
        policy.trust_for_invocation("pi-real-hello");
        let catalog = ygg_agent::ExtensionCatalog::load_resolved(
            [ygg_agent::extension_process::ExtensionManifestInput {
                path: manifest_path,
                source: ygg_agent::ExtensionSource::Explicit,
            }],
            &policy,
            64 * 1024,
        );
        assert!(catalog.diagnostics.is_empty(), "{:?}", catalog.diagnostics);
        let descriptor = catalog.extensions.into_iter().next().unwrap();
        let process = ygg_agent::ExtensionProcess::start(
            descriptor,
            ygg_agent::ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        .unwrap();
        assert!(process
            .contributions()
            .tools
            .iter()
            .any(|tool| tool.name == "hello"));
        let output = process
            .call_tool(
                "hello",
                serde_json::json!({"name": "Ygg"}),
                process.current_context(),
            )
            .await
            .unwrap();
        assert!(output.content.contains("Hello, Ygg!"));
        assert!(process.shutdown().await);
    }

    #[test]
    fn publisher_rechecks_sources_before_creating_any_output() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("extension.ts");
        fs::write(&source, b"before").unwrap();
        let source = canonical(&source);
        let package_root = create_pi_package(&temp.path().join("pi-package"));
        let record = PiLockRecord::new(
            "pi-example".to_owned(),
            vec![PiLockedSource::from_canonical_path(source.clone(), None).unwrap()],
            temp.path().join("pi-home"),
            resolve_pi_package(&package_root, temp.path()).unwrap(),
        )
        .unwrap();
        fs::write(&source, b"after").unwrap();
        let destination = canonical(temp.path()).join("extensions/pi-example");

        let error = publish_lock_extension(&record, &destination)
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed after planning"));
        assert!(!destination.exists());
        assert!(!destination.parent().unwrap().exists());
    }
}
