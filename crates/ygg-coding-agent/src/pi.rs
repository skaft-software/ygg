#![allow(missing_docs)]

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ygg_agent::extension_process::{
    ExtensionCapabilities, ExtensionEntrypoint, ExtensionFilesystemAccess, ExtensionHook,
    ExtensionManifest, ExtensionUiSurface, ManifestContributions,
};
use ygg_agent::EXTENSION_API_VERSION_0_2;

const BRIDGE_VERSION: &str = "0.2.0";
const SUPPORTED_PI_VERSION: &str = "0.84.4";
const YGG_VERSION: &str = env!("CARGO_PKG_VERSION");
const LINK_SCHEMA_VERSION: u32 = 2;
const LINK_RECORD: &str = "pi-link.json";
const PI_LOCK_SCHEMA_VERSION: u32 = 1;
const PI_LOCK_RECORD: &str = "pi-lock.json";
const DEFAULT_AGGREGATE_NAME: &str = "pi-compat-0-84-4";
const MAX_AGGREGATE_SOURCES: usize = 256;
const SOURCE_FINGERPRINT_ALGORITHM: &str = "sha256";
const SOURCE_FINGERPRINT_FORMAT: u32 = 1;
const MAX_SOURCE_PATH_BYTES: usize = 4096;
const MAX_SOURCE_FILES: usize = 4096;
const MAX_SOURCE_ENTRIES: usize = 8192;
const MAX_SOURCE_DEPTH: usize = 64;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_LINK_RECORD_BYTES: usize = 64 * 1024;
const MAX_PI_LOCK_BYTES: usize = 256 * 1024;
const MAX_PI_PACKAGE_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_GENERATED_FILE_BYTES: usize = 4 * 1024 * 1024;

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
struct SourceFingerprint {
    algorithm: String,
    format_version: u32,
    digest: String,
    file_count: u64,
    byte_count: u64,
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

impl PiLinkRecord {
    fn new(
        name: String,
        source: PathBuf,
        pi_home: PathBuf,
        pi_package: Option<PathBuf>,
        source_fingerprint: SourceFingerprint,
    ) -> Self {
        Self {
            schema_version: LINK_SCHEMA_VERSION,
            bridge_version: BRIDGE_VERSION.to_owned(),
            pi_version: SUPPORTED_PI_VERSION.to_owned(),
            ygg_version: YGG_VERSION.to_owned(),
            source_fingerprint,
            name,
            source,
            pi_home,
            pi_package,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PiLockedSource {
    source: PathBuf,
    source_fingerprint: SourceFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PiLockRecord {
    schema_version: u32,
    bridge_version: String,
    pi_version: String,
    ygg_version: String,
    name: String,
    sources: Vec<PiLockedSource>,
    pi_home: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi_package: Option<PathBuf>,
    aggregate_digest: String,
}

impl PiLockRecord {
    fn new(
        name: String,
        sources: Vec<PiLockedSource>,
        pi_home: PathBuf,
        pi_package: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let mut record = Self {
            schema_version: PI_LOCK_SCHEMA_VERSION,
            bridge_version: BRIDGE_VERSION.to_owned(),
            pi_version: SUPPORTED_PI_VERSION.to_owned(),
            ygg_version: YGG_VERSION.to_owned(),
            name,
            sources,
            pi_home,
            pi_package,
            aggregate_digest: String::new(),
        };
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum ParsedPiInstallation {
    Link(ParsedPiLinkRecord),
    Lock(PiLockRecord),
}

impl ParsedPiInstallation {
    fn name(&self) -> &str {
        match self {
            Self::Link(record) => record.name(),
            Self::Lock(record) => &record.name,
        }
    }

    fn source_summary(&self) -> String {
        match self {
            Self::Link(record) => record.source().display().to_string(),
            Self::Lock(record) => format!(
                "{} ordered source(s): {}",
                record.sources.len(),
                record
                    .sources
                    .iter()
                    .map(|source| source.source.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn pi_home(&self) -> &Path {
        match self {
            Self::Link(record) => record.pi_home(),
            Self::Lock(record) => &record.pi_home,
        }
    }

    fn pi_package(&self) -> Option<&Path> {
        match self {
            Self::Link(record) => record.pi_package(),
            Self::Lock(record) => record.pi_package.as_deref(),
        }
    }

    fn status(&self) -> String {
        match self {
            Self::Link(record) => link_status(record),
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
    let mut unique_sources = std::collections::BTreeSet::new();
    for requested_source in requested_sources {
        let source = resolve_source(requested_source, invocation_cwd)?;
        if !unique_sources.insert(source.clone()) {
            anyhow::bail!("duplicate Pi extension source {}", source.display());
        }
        let source_fingerprint = fingerprint_source(&source)?;
        sources.push(PiLockedSource {
            source,
            source_fingerprint,
        });
    }
    let pi_home = resolve_pi_home(requested_pi_home, invocation_cwd)?;
    let pi_package = requested_pi_package
        .map(|path| resolve_pi_package(path, invocation_cwd))
        .transpose()?;
    let extension_root = resolve_extension_root(requested_extension_root, invocation_cwd)?;
    fs::create_dir_all(&extension_root).with_context(|| {
        format!(
            "cannot create Ygg extension root {}",
            extension_root.display()
        )
    })?;
    reject_symlink(&extension_root, "Ygg extension root")?;
    let extension_root = extension_root.canonicalize().with_context(|| {
        format!(
            "cannot resolve Ygg extension root {}",
            extension_root.display()
        )
    })?;

    let aggregate = sources.len() > 1;
    let name = requested_name
        .map(validate_name)
        .transpose()?
        .unwrap_or_else(|| {
            if aggregate {
                DEFAULT_AGGREGATE_NAME.to_owned()
            } else {
                generated_name(&sources[0].source)
            }
        });
    let package = extension_root.join(&name);
    match fs::symlink_metadata(&package) {
        Ok(_) => anyhow::bail!(
            "Pi compatibility link {name:?} already exists at {}; remove it manually before reinstalling",
            package.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot inspect Pi compatibility link destination {}",
                    package.display()
                )
            });
        }
    }

    let bridge_path = package.join("bridge.mjs");
    let manifest = manifest_for_sources(
        &name,
        &sources,
        &pi_home,
        pi_package.as_deref(),
        &bridge_path,
    )?;
    let manifest_text = toml::to_string_pretty(&manifest)?;
    let (record_name, record_text) = if aggregate {
        let record = PiLockRecord::new(
            name.clone(),
            sources.clone(),
            pi_home.clone(),
            pi_package.clone(),
        )?;
        (
            PI_LOCK_RECORD,
            format!("{}\n", serde_json::to_string_pretty(&record)?),
        )
    } else {
        let source = &sources[0];
        let record = PiLinkRecord::new(
            name.clone(),
            source.source.clone(),
            pi_home.clone(),
            pi_package.clone(),
            source.source_fingerprint.clone(),
        );
        (
            LINK_RECORD,
            format!("{}\n", serde_json::to_string_pretty(&record)?),
        )
    };

    let mut publication = PackagePublication::create(&package)?;
    let publish_result = (|| -> anyhow::Result<()> {
        publication.write_private_file(
            &bridge_path,
            include_str!("../../../extensions/ygg-pi-compat/bridge.mjs"),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bridge_path, fs::Permissions::from_mode(0o700))?;
        }
        publication.write_private_file(&package.join("extension.toml"), &manifest_text)?;
        // The lock/record is written last so an incomplete package is never listed.
        publication.write_private_file(&package.join(record_name), &record_text)?;
        sync_directory(&package)?;
        Ok(())
    })();
    if let Err(error) = publish_result {
        return match publication.rollback() {
            Ok(()) => Err(error),
            Err(rollback) => Err(anyhow::anyhow!(
                "{error:#}; rollback of {} also failed: {rollback:#}",
                package.display()
            )),
        };
    }
    publication.commit();

    if aggregate {
        crate::output::stdout_line(format!(
            "Installed Pi compatibility aggregate {name} with {} ordered sources.",
            sources.len()
        ));
    } else {
        crate::output::stdout_line(format!(
            "Installed Pi compatibility link {name} for {}.",
            sources[0].source.display()
        ));
    }
    crate::output::stdout_line(
        "The link remains disabled and untrusted until you explicitly enable and trust it.",
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
            if let Ok(record) = parse_lock_record(&bytes) {
                records.push(ParsedPiInstallation::Lock(record));
                continue;
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

#[cfg(test)]
fn manifest(
    name: &str,
    source: &Path,
    source_fingerprint: &SourceFingerprint,
    pi_home: &Path,
    pi_package: Option<&Path>,
    bridge_path: &Path,
) -> anyhow::Result<ExtensionManifest> {
    manifest_for_sources(
        name,
        &[PiLockedSource {
            source: source.to_path_buf(),
            source_fingerprint: source_fingerprint.clone(),
        }],
        pi_home,
        pi_package,
        bridge_path,
    )
}

fn manifest_for_sources(
    name: &str,
    sources: &[PiLockedSource],
    pi_home: &Path,
    pi_package: Option<&Path>,
    bridge_path: &Path,
) -> anyhow::Result<ExtensionManifest> {
    if sources.is_empty() || sources.len() > MAX_AGGREGATE_SOURCES {
        anyhow::bail!("invalid Pi compatibility source count {}", sources.len());
    }
    let pi_home_text = pi_home
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Pi home path is not valid UTF-8: {}", pi_home.display()))?;
    let bridge_text = bridge_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Pi bridge path is not valid UTF-8: {}",
            bridge_path.display()
        )
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
    for locked in sources {
        let source_text = locked.source.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "Pi extension source path is not valid UTF-8: {}",
                locked.source.display()
            )
        })?;
        entrypoint_args.extend([
            "--extension".to_owned(),
            source_text.to_owned(),
            "--source-fingerprint".to_owned(),
            locked.source_fingerprint.digest.clone(),
        ]);
    }
    entrypoint_args.extend(["--agent-dir".to_owned(), pi_home_text.to_owned()]);
    if let Some(pi_package) = pi_package {
        let pi_package = pi_package.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "Pi package path is not valid UTF-8: {}",
                pi_package.display()
            )
        })?;
        entrypoint_args.extend(["--pi-package".to_owned(), pi_package.to_owned()]);
    }
    entrypoint_args.extend(["--command".to_owned(), name.to_owned()]);

    let description = if sources.len() == 1 {
        format!("Pi compatibility link for {}", sources[0].source.display())
    } else {
        format!(
            "Pi compatibility aggregate for {} ordered sources",
            sources.len()
        )
    };
    Ok(ExtensionManifest {
        name: name.to_owned(),
        version: BRIDGE_VERSION.to_owned(),
        api_version: EXTENSION_API_VERSION_0_2.to_owned(),
        requires_ygg: Some(format!("={YGG_VERSION}")),
        description: Some(description),
        entrypoint: ExtensionEntrypoint {
            command: entrypoint_command,
            args: entrypoint_args,
            env: Default::default(),
        },
        capabilities: ExtensionCapabilities {
            filesystem: ExtensionFilesystemAccess::Unrestricted,
            process: true,
            network: true,
            secrets: Vec::new(),
            environment: Vec::new(),
        },
        contributes: ManifestContributions {
            tools: Vec::new(),
            commands: vec![name.to_owned()],
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
    if record.schema_version != PI_LOCK_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported Pi aggregate lock schema {}",
            record.schema_version
        );
    }
    if record.sources.len() < 2 || record.sources.len() > MAX_AGGREGATE_SOURCES {
        anyhow::bail!("invalid Pi aggregate source count {}", record.sources.len());
    }
    validate_name(&record.name)?;
    if !valid_sha256(&record.aggregate_digest) {
        anyhow::bail!("invalid Pi aggregate digest");
    }
    let mut unique = std::collections::BTreeSet::new();
    for source in &record.sources {
        if !unique.insert(&source.source) {
            anyhow::bail!("duplicate Pi aggregate source {}", source.source.display());
        }
    }
    Ok(record)
}

fn aggregate_lock_digest(record: &PiLockRecord) -> anyhow::Result<String> {
    let mut unsigned = record.clone();
    unsigned.aggregate_digest.clear();
    let bytes = serde_json::to_vec(&unsigned)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-aggregate-lock\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
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
    let mut stale = Vec::new();
    if record.schema_version != PI_LOCK_SCHEMA_VERSION {
        stale.push("lock schema changed".to_owned());
    }
    if record.bridge_version != BRIDGE_VERSION {
        stale.push("bridge profile changed".to_owned());
    }
    if record.pi_version != SUPPORTED_PI_VERSION {
        stale.push("supported Pi version changed".to_owned());
    }
    if record.ygg_version != YGG_VERSION {
        stale.push("Ygg version changed".to_owned());
    }
    match aggregate_lock_digest(record) {
        Ok(actual) if actual != record.aggregate_digest => {
            stale.push("aggregate lock digest changed".to_owned());
        }
        Err(error) => stale.push(format!("aggregate lock cannot be verified: {error:#}")),
        Ok(_) => {}
    }
    let mut unique = std::collections::BTreeSet::new();
    for (index, source) in record.sources.iter().enumerate() {
        if !unique.insert(&source.source) {
            stale.push(format!("source {} is duplicated", source.source.display()));
            continue;
        }
        if source.source_fingerprint.algorithm != SOURCE_FINGERPRINT_ALGORITHM
            || source.source_fingerprint.format_version != SOURCE_FINGERPRINT_FORMAT
            || !valid_sha256(&source.source_fingerprint.digest)
        {
            stale.push(format!("source {} fingerprint is invalid", index + 1));
            continue;
        }
        match fingerprint_source(&source.source) {
            Ok(actual) if actual != source.source_fingerprint => {
                stale.push(format!("source {} changed", index + 1));
            }
            Err(error) => stale.push(format!(
                "source {} cannot be verified: {error:#}",
                index + 1
            )),
            Ok(_) => {}
        }
    }
    if let Some(pi_package) = record.pi_package.as_deref() {
        if let Err(error) = resolve_pi_package(pi_package, Path::new("/")) {
            stale.push(format!("Pi package cannot be verified: {error:#}"));
        }
    }
    if stale.is_empty() {
        "aggregate-current (trust not asserted)".to_owned()
    } else {
        format!("stale ({})", stale.join("; "))
    }
}

fn valid_sha256(value: &str) -> bool {
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

fn fingerprint_source(source: &Path) -> anyhow::Result<SourceFingerprint> {
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

fn resolve_pi_package(path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let selected = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let metadata = fs::symlink_metadata(&selected)
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
    if manifest.name != "@earendil-works/pi-coding-agent"
        || manifest.version != SUPPORTED_PI_VERSION
    {
        anyhow::bail!(
            "Pi package must be @earendil-works/pi-coding-agent@{SUPPORTED_PI_VERSION}; found {}@{}",
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
    Ok(root)
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

fn resolve_extension_root(requested: Option<&Path>, cwd: &Path) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        return absolute_path(path, cwd);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("user home is unavailable"))?;
    Ok(home.join(".ygg/extensions"))
}

fn absolute_path(path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    Ok(path)
}

fn reject_symlink(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("{label} must not be a symlink: {}", path.display());
    }
    Ok(())
}

fn creation_error_with_rollback(path: &Path, error: anyhow::Error) -> anyhow::Error {
    match fs::remove_dir(path) {
        Ok(()) => error,
        Err(rollback) => anyhow::anyhow!(
            "{error:#}; rollback of newly created directory {} also failed: {rollback}",
            path.display()
        ),
    }
}

struct PackagePublication {
    path: PathBuf,
    files: Vec<PathBuf>,
    committed: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl PackagePublication {
    fn create(path: &Path) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path).with_context(|| {
                format!("cannot create Pi compatibility link {}", path.display())
            })?;
        }
        #[cfg(not(unix))]
        fs::create_dir(path)
            .with_context(|| format!("cannot create Pi compatibility link {}", path.display()))?;

        if let Err(error) = ygg_agent::secure_fs::create_private_directory_all(path) {
            return match fs::remove_dir(path) {
                Ok(()) => Err(error.into()),
                Err(rollback) => Err(anyhow::anyhow!(
                    "cannot make Pi compatibility link private: {error}; rollback also failed: {rollback}"
                )),
            };
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(creation_error_with_rollback(path, error.into()));
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(creation_error_with_rollback(
                path,
                anyhow::anyhow!("new Pi compatibility link is not a private directory"),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.permissions().mode() & 0o7777 != 0o700 {
                return Err(creation_error_with_rollback(
                    path,
                    anyhow::anyhow!("new Pi compatibility link directory is not mode 0700"),
                ));
            }
            Ok(Self {
                path: path.to_owned(),
                files: Vec::new(),
                committed: false,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        Ok(Self {
            path: path.to_owned(),
            files: Vec::new(),
            committed: false,
        })
    }

    fn write_private_file(&mut self, path: &Path, content: &str) -> anyhow::Result<()> {
        if path.parent() != Some(self.path.as_path()) || path.file_name().is_none() {
            anyhow::bail!("generated Pi link file is outside its private package");
        }
        self.files.push(path.to_owned());
        ygg_agent::secure_fs::write_private_atomic(
            path,
            content.as_bytes(),
            MAX_GENERATED_FILE_BYTES,
        )
        .with_context(|| format!("cannot create {}", path.display()))
    }

    fn verify_directory(&self) -> anyhow::Result<()> {
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            anyhow::bail!("Pi compatibility link directory was replaced during publication");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.dev() != self.device
                || metadata.ino() != self.inode
                || metadata.permissions().mode() & 0o7777 != 0o700
            {
                anyhow::bail!("Pi compatibility link private directory identity changed");
            }
        }
        Ok(())
    }

    fn rollback(&mut self) -> anyhow::Result<()> {
        self.verify_directory()?;
        let mut failures = Vec::new();
        for path in self.files.iter().rev() {
            if let Err(error) = ygg_agent::secure_fs::remove_regular_file_if_exists(path) {
                failures.push(format!("{}: {error}", path.display()));
            }
        }
        if let Err(error) = self.verify_directory() {
            failures.push(error.to_string());
        } else if let Err(error) = fs::remove_dir(&self.path) {
            failures.push(format!("{}: {error}", self.path.display()));
        }
        if failures.is_empty() {
            self.committed = true;
            Ok(())
        } else {
            anyhow::bail!("{}", failures.join("; "))
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PackagePublication {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.rollback();
        }
    }
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
        fs::write(&source, b"after").unwrap();
        let after = fingerprint_source(&source).unwrap();
        assert_ne!(before.digest, after.digest);

        let record = ParsedPiLinkRecord::V2(PiLinkRecord::new(
            "pi-example".to_owned(),
            source.clone(),
            temp.path().join("pi-home"),
            None,
            before,
        ));
        assert!(link_status(&record).starts_with("stale (source changed"));
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
    fn v2_record_and_manifest_pin_exact_compatibility_versions() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("example.ts");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let source = canonical(&source);
        let source_fingerprint = fingerprint_source(&source).unwrap();
        let record = PiLinkRecord::new(
            "pi-example".to_owned(),
            source.clone(),
            temp.path().join("pi-home"),
            None,
            source_fingerprint.clone(),
        );
        let encoded = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            parse_link_record(&encoded).unwrap(),
            ParsedPiLinkRecord::V2(_)
        ));
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["schema_version"], LINK_SCHEMA_VERSION);
        assert_eq!(value["pi_version"], SUPPORTED_PI_VERSION);
        assert_eq!(value["bridge_version"], BRIDGE_VERSION);
        assert_eq!(value["ygg_version"], YGG_VERSION);
        assert_eq!(
            value["source_fingerprint"]["algorithm"],
            SOURCE_FINGERPRINT_ALGORITHM
        );

        let manifest = manifest(
            "pi-example",
            &source,
            &source_fingerprint,
            &temp.path().join("pi-home"),
            None,
            &temp.path().join("link/bridge.mjs"),
        )
        .unwrap();
        assert_eq!(manifest.version, BRIDGE_VERSION);
        assert_eq!(
            manifest.requires_ygg.as_deref(),
            Some(concat!("=", env!("CARGO_PKG_VERSION")))
        );
        if cfg!(unix) {
            assert_eq!(
                manifest.entrypoint.command,
                temp.path().join("link/bridge.mjs").to_str().unwrap()
            );
            assert_eq!(manifest.entrypoint.args.first().unwrap(), "--extension");
        } else {
            assert_eq!(manifest.entrypoint.command, "node");
            assert!(manifest.entrypoint.args[0].ends_with("bridge.mjs"));
        }
        let fingerprint_arg = manifest
            .entrypoint
            .args
            .windows(2)
            .find(|args| args[0] == "--source-fingerprint")
            .map(|args| args[1].as_str());
        assert_eq!(fingerprint_arg, Some(source_fingerprint.digest.as_str()));
        assert_eq!(manifest.contributes.commands, ["pi-example"]);
        assert_eq!(manifest.contributes.hooks.len(), 3);
        assert!(!manifest
            .contributes
            .hooks
            .contains(&ExtensionHook::BeforePrompt));
        assert!(manifest.contributes.context);
    }

    #[test]
    fn aggregate_lock_and_manifest_preserve_source_order_and_detect_changes() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.ts");
        let second = temp.path().join("second.ts");
        fs::write(&first, b"export default () => 'first';\n").unwrap();
        fs::write(&second, b"export default () => 'second';\n").unwrap();
        let first = canonical(&first);
        let second = canonical(&second);
        let sources = vec![
            PiLockedSource {
                source: first.clone(),
                source_fingerprint: fingerprint_source(&first).unwrap(),
            },
            PiLockedSource {
                source: second.clone(),
                source_fingerprint: fingerprint_source(&second).unwrap(),
            },
        ];
        let record = PiLockRecord::new(
            "pi-aggregate".into(),
            sources.clone(),
            temp.path().join("pi-home"),
            None,
        )
        .unwrap();
        let encoded = serde_json::to_vec(&record).unwrap();
        let decoded = parse_lock_record(&encoded).unwrap();
        assert_eq!(decoded.sources, sources);
        assert_eq!(
            aggregate_status(&decoded),
            "aggregate-current (trust not asserted)"
        );

        let manifest = manifest_for_sources(
            "pi-aggregate",
            &sources,
            &temp.path().join("pi-home"),
            None,
            &temp.path().join("link/bridge.mjs"),
        )
        .unwrap();
        let ordered = manifest
            .entrypoint
            .args
            .windows(2)
            .filter(|args| args[0] == "--extension")
            .map(|args| args[1].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered,
            vec![first.to_string_lossy(), second.to_string_lossy()]
        );

        fs::write(&second, b"export default () => 'changed';\n").unwrap();
        assert!(aggregate_status(&decoded).contains("source 2 changed"));
    }

    #[test]
    fn aggregate_install_publishes_one_inert_locked_process() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.ts");
        let second = temp.path().join("second.ts");
        fs::write(&first, b"export default () => {};\n").unwrap();
        fs::write(&second, b"export default () => {};\n").unwrap();
        let extension_root = temp.path().join("extensions");
        install_sources(
            &[first, second],
            Some("pi-aggregate"),
            Some(&temp.path().join("pi-home")),
            None,
            Some(&extension_root),
            temp.path(),
        )
        .unwrap();

        let package = extension_root.join("pi-aggregate");
        assert!(package.join("bridge.mjs").is_file());
        assert!(package.join("extension.toml").is_file());
        assert!(package.join(PI_LOCK_RECORD).is_file());
        assert!(!package.join(LINK_RECORD).exists());
        let lock = parse_lock_record(&fs::read(package.join(PI_LOCK_RECORD)).unwrap()).unwrap();
        assert_eq!(lock.sources.len(), 2);
        assert_eq!(lock.name, "pi-aggregate");
        assert_eq!(
            aggregate_status(&lock),
            "aggregate-current (trust not asserted)"
        );
    }

    #[test]
    fn selected_pi_package_is_validated_and_pinned_into_the_bridge_args() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("pi-package");
        fs::create_dir_all(package.join("dist")).unwrap();
        fs::write(
            package.join("package.json"),
            format!(
                r#"{{"name":"@earendil-works/pi-coding-agent","version":"{SUPPORTED_PI_VERSION}"}}"#
            ),
        )
        .unwrap();
        fs::write(package.join("dist/index.js"), b"export {};\n").unwrap();
        let package = resolve_pi_package(&package, temp.path()).unwrap();
        let source = temp.path().join("extension.ts");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let source = canonical(&source);
        let source_fingerprint = fingerprint_source(&source).unwrap();
        let record = ParsedPiLinkRecord::V2(PiLinkRecord::new(
            "pi-example".to_owned(),
            source.clone(),
            temp.path().join("pi-home"),
            Some(package.clone()),
            source_fingerprint.clone(),
        ));
        let manifest = manifest(
            "pi-example",
            &source,
            &source_fingerprint,
            &temp.path().join("pi-home"),
            Some(&package),
            &temp.path().join("link/bridge.mjs"),
        )
        .unwrap();
        let package_arg = manifest
            .entrypoint
            .args
            .windows(2)
            .find(|args| args[0] == "--pi-package")
            .map(|args| args[1].as_str());
        assert_eq!(package_arg, package.to_str());

        fs::write(
            package.join("package.json"),
            r#"{"name":"@earendil-works/pi-coding-agent","version":"0.85.0"}"#,
        )
        .unwrap();
        let error = resolve_pi_package(&package, temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains(SUPPORTED_PI_VERSION));
        assert!(link_status(&record).contains("Pi package cannot be verified"));
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
            .contains("source changed after link installation"));
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
    fn uncommitted_private_publication_rolls_back_known_files() {
        let temp = tempfile::tempdir().unwrap();
        let package = canonical(temp.path()).join("pi-example");
        let mut publication = PackagePublication::create(&package).unwrap();
        publication
            .write_private_file(&package.join("extension.toml"), "name = 'test'\n")
            .unwrap();
        publication.rollback().unwrap();
        assert!(!package.exists());
    }
}
