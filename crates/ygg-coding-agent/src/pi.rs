#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ygg_agent::extension_process::{
    ExtensionCapabilities, ExtensionEntrypoint, ExtensionFilesystemAccess, ExtensionHook,
    ExtensionLifecycleProfile, ExtensionManifest, ExtensionRuntimeSettings,
    ExtensionRuntimeSharing, ExtensionUiSurface, ManifestContributions,
};
use ygg_agent::{EXTENSION_API_VERSION_0_2, EXTENSION_API_VERSION_0_3};

const BRIDGE_VERSION: &str = "0.3.0";
const SUPPORTED_PI_VERSION: &str = "0.84.4";
const YGG_VERSION: &str = env!("CARGO_PKG_VERSION");
const LINK_SCHEMA_VERSION: u32 = 3;
const LINK_RECORD: &str = "pi-link.json";
const PI_LOCK_SCHEMA_VERSION: u32 = 2;
const PI_LOCK_RECORD: &str = "pi-lock.json";
const PI_PLAN_SCHEMA_VERSION: u32 = 1;
const PI_PLAN_SCHEMA: &str = "ygg.pi.aggregate-plan.v1";
const PI_RUNTIME_EVIDENCE_SCHEMA: &str = "ygg.pi.runtime.evidence.v1";
const PI_RUNTIME_EVIDENCE_RECORD: &str = "pi-runtime-evidence.json";
const DEFAULT_AGGREGATE_NAME: &str = "pi-compat-0-84-4";
const MAX_AGGREGATE_SOURCES: usize = 256;
const SOURCE_FINGERPRINT_ALGORITHM: &str = "sha256";
const SOURCE_FINGERPRINT_FORMAT: u32 = 1;
const SOURCE_LOCK_FINGERPRINT_FORMAT: u32 = 1;
const PI_RUNTIME_INTEGRITY_FORMAT: u32 = 1;
const LINK_IDENTITY_FORMAT: u32 = 1;
const EXPLICIT_TRUST_MODE: &str = "explicit_enable_and_trust_required";
const PI_AGGREGATE_LIFECYCLE_PROFILE: &str = "pi_aggregate";
const MAX_SOURCE_PATH_BYTES: usize = 4096;
const MAX_SOURCE_FILES: usize = 4096;
const MAX_SOURCE_ENTRIES: usize = 8192;
const MAX_SOURCE_DEPTH: usize = 64;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_LOCK_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOCK_BYTES: usize = 64 * 1024 * 1024;
const MAX_LINK_RECORD_BYTES: usize = 64 * 1024;
const MAX_PI_LOCK_BYTES: usize = 256 * 1024;
const MAX_PI_PLAN_BYTES: usize = 256 * 1024;
const MAX_PI_PACKAGE_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_GENERATED_FILE_BYTES: usize = 4 * 1024 * 1024;
const SUPPORTED_LOCK_FILES: [&str; 5] = [
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
];

/// The host extension contract selected for a generated Pi bridge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum PiBridgeApiVersion {
    /// Preserve the established Pi tool/command compatibility bridge.
    #[default]
    #[value(name = "0.2")]
    V02,
    /// Enable only the secret-free, host-owned API 0.3 provider bridge.
    #[value(name = "0.3")]
    V03,
}

impl PiBridgeApiVersion {
    fn extension_api_version(self) -> &'static str {
        match self {
            Self::V02 => EXTENSION_API_VERSION_0_2,
            Self::V03 => EXTENSION_API_VERSION_0_3,
        }
    }

    fn argument(self) -> &'static str {
        match self {
            Self::V02 => "0.2",
            Self::V03 => "0.3",
        }
    }
}

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
        /// Generate against API 0.2 (default) or the host-owned API 0.3 provider contract.
        #[arg(long, value_enum, default_value = "0.2")]
        api_version: PiBridgeApiVersion,
        /// Ygg extension root used for the generated compatibility link.
        #[arg(long, value_name = "DIR")]
        extension_root: Option<PathBuf>,
    },
    /// Compile an inert, ordered Pi aggregate plan without executing source.
    Plan {
        /// A local .ts/.js extension file or an installed Pi package directory.
        source: PathBuf,
        /// Additional reviewed Pi sources, retained in this exact load order.
        #[arg(long = "with", value_name = "SOURCE")]
        additional_sources: Vec<PathBuf>,
        /// Name reserved for the generated compatibility link.
        #[arg(long)]
        name: Option<String>,
        /// Pi's user agent directory. Defaults to PI_CODING_AGENT_DIR or ~/.pi/agent.
        #[arg(long, value_name = "DIR")]
        pi_home: Option<PathBuf>,
        /// Exact @earendil-works/pi-coding-agent package root for bridge profile 0.84.4.
        #[arg(long, value_name = "DIR")]
        pi_package: Option<PathBuf>,
        /// Write the canonical plan to a new regular file instead of stdout.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Validate a previously compiled Pi aggregate plan without executing source.
    Preflight {
        /// Canonical plan generated by `ygg pi plan`.
        plan: PathBuf,
    },
    /// Publish a preflighted Pi aggregate plan as an inert compatibility link.
    Publish {
        /// Canonical plan generated by `ygg pi plan`.
        plan: PathBuf,
        /// Ygg extension root used for the generated compatibility link.
        #[arg(long, value_name = "DIR")]
        extension_root: Option<PathBuf>,
    },
    /// Move a generated Pi compatibility link out of discovery without deleting it.
    Rollback {
        /// Generated Pi compatibility link name.
        name: String,
        /// Ygg extension root used for generated compatibility links.
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
#[serde(deny_unknown_fields)]
struct SourceLockFingerprint {
    algorithm: String,
    format_version: u32,
    digest: String,
    file_count: u64,
    byte_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiRuntimeIntegrity {
    algorithm: String,
    format_version: u32,
    digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiRuntimeIdentity {
    path: PathBuf,
    package_integrity: PiRuntimeIntegrity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiPlanTrustRequirement {
    mode: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiTrustBinding {
    mode: String,
    extension_name: String,
    manifest_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiLockedSource {
    source: PathBuf,
    source_fingerprint: SourceFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lock_fingerprint: Option<SourceLockFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiAggregatePlan {
    schema: String,
    schema_version: u32,
    bridge_version: String,
    pi_version: String,
    ygg_version: String,
    lifecycle_profile: String,
    name: String,
    sources: Vec<PiLockedSource>,
    pi_home: PathBuf,
    pi_runtime: PiRuntimeIdentity,
    trust: PiPlanTrustRequirement,
    plan_digest: String,
}

impl PiAggregatePlan {
    fn new(
        name: String,
        sources: Vec<PiLockedSource>,
        pi_home: PathBuf,
        pi_runtime: PiRuntimeIdentity,
    ) -> anyhow::Result<Self> {
        let mut plan = Self {
            schema: PI_PLAN_SCHEMA.to_owned(),
            schema_version: PI_PLAN_SCHEMA_VERSION,
            bridge_version: BRIDGE_VERSION.to_owned(),
            pi_version: SUPPORTED_PI_VERSION.to_owned(),
            ygg_version: YGG_VERSION.to_owned(),
            lifecycle_profile: PI_AGGREGATE_LIFECYCLE_PROFILE.to_owned(),
            name,
            sources,
            pi_home,
            pi_runtime,
            trust: PiPlanTrustRequirement {
                mode: EXPLICIT_TRUST_MODE.to_owned(),
            },
            plan_digest: String::new(),
        };
        plan.plan_digest = aggregate_plan_digest(&plan)?;
        Ok(plan)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PiLinkRecord {
    schema_version: u32,
    bridge_version: String,
    pi_version: String,
    ygg_version: String,
    source_fingerprint: SourceFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_lock_fingerprint: Option<SourceLockFingerprint>,
    name: String,
    source: PathBuf,
    pi_home: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi_package: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi_runtime: Option<PiRuntimeIdentity>,
    #[serde(default)]
    aggregate_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trust_binding: Option<PiTrustBinding>,
    #[serde(default)]
    link_identity: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi_runtime: Option<PiRuntimeIdentity>,
    aggregate_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trust_binding: Option<PiTrustBinding>,
    #[serde(default)]
    link_identity: String,
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
    V2(Box<PiLinkRecord>),
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
    Lock(Box<PiLockRecord>),
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
            api_version,
            extension_root,
        } => {
            let plan = compile_requested_plan(
                &source,
                &additional_sources,
                name.as_deref(),
                pi_home.as_deref(),
                pi_package.as_deref(),
                invocation_cwd,
            )?;
            publish_plan_for_api(
                &plan,
                extension_root.as_deref(),
                invocation_cwd,
                api_version,
            )
        }
        PiCommand::Plan {
            source,
            additional_sources,
            name,
            pi_home,
            pi_package,
            output,
        } => {
            let plan = compile_requested_plan(
                &source,
                &additional_sources,
                name.as_deref(),
                pi_home.as_deref(),
                pi_package.as_deref(),
                invocation_cwd,
            )?;
            let text = format!("{}\n", serde_json::to_string_pretty(&plan)?);
            if let Some(output) = output {
                let output = resolve_new_plan_path(&output, invocation_cwd)?;
                write_new_private_file(&output, &text)?;
                crate::output::stdout_line(
                    "Pi aggregate plan written. Run `ygg pi preflight --plan FILE` before publishing.",
                );
                crate::output::stdout_line(
                    "No Pi package code, npm lifecycle hook, dependency installer, or extension source was run.",
                );
            } else {
                // Keep stdout directly usable as the canonical plan artifact.
                crate::output::stdout_multiline(&text);
                crate::output::stderr_line(
                    "No Pi package code, npm lifecycle hook, dependency installer, or extension source was run.",
                );
            }
            Ok(())
        }
        PiCommand::Preflight { plan } => {
            let plan = read_plan(&plan, invocation_cwd)?;
            preflight_plan(&plan)?;
            crate::output::stdout_line(format!(
                "Pi aggregate preflight passed for {} ({} ordered source(s), pinned Pi {}).",
                plan.name,
                plan.sources.len(),
                plan.pi_version
            ));
            crate::output::stdout_line(
                "No Pi package code, npm lifecycle hook, dependency installer, or extension source was run.",
            );
            Ok(())
        }
        PiCommand::Publish {
            plan,
            extension_root,
        } => {
            let plan = read_plan(&plan, invocation_cwd)?;
            publish_plan(&plan, extension_root.as_deref(), invocation_cwd)
        }
        PiCommand::Rollback {
            name,
            extension_root,
        } => rollback(&name, extension_root.as_deref(), invocation_cwd),
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

#[cfg(test)]
fn install_sources(
    requested_sources: &[PathBuf],
    requested_name: Option<&str>,
    requested_pi_home: Option<&Path>,
    requested_pi_package: Option<&Path>,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    install_sources_for_api(
        requested_sources,
        requested_name,
        requested_pi_home,
        requested_pi_package,
        requested_extension_root,
        invocation_cwd,
        PiBridgeApiVersion::V02,
    )
}

#[cfg(test)]
fn install_sources_for_api(
    requested_sources: &[PathBuf],
    requested_name: Option<&str>,
    requested_pi_home: Option<&Path>,
    requested_pi_package: Option<&Path>,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
    api_version: PiBridgeApiVersion,
) -> anyhow::Result<()> {
    let (source, additional_sources) = requested_sources
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("at least one Pi extension source is required"))?;
    let plan = compile_requested_plan(
        source,
        additional_sources,
        requested_name,
        requested_pi_home,
        requested_pi_package,
        invocation_cwd,
    )?;
    publish_plan_for_api(&plan, requested_extension_root, invocation_cwd, api_version)
}

fn compile_requested_plan(
    source: &Path,
    additional_sources: &[PathBuf],
    requested_name: Option<&str>,
    requested_pi_home: Option<&Path>,
    requested_pi_package: Option<&Path>,
    invocation_cwd: &Path,
) -> anyhow::Result<PiAggregatePlan> {
    let mut requested_sources = Vec::with_capacity(1 + additional_sources.len());
    requested_sources.push(source.to_path_buf());
    requested_sources.extend_from_slice(additional_sources);
    if requested_sources.len() > MAX_AGGREGATE_SOURCES {
        anyhow::bail!(
            "Pi compatibility source set contains {} sources; limit is {MAX_AGGREGATE_SOURCES}",
            requested_sources.len()
        );
    }

    let mut sources = Vec::with_capacity(requested_sources.len());
    let mut unique_sources = std::collections::BTreeSet::new();
    for (index, requested_source) in requested_sources.iter().enumerate() {
        let source = resolve_source(requested_source, invocation_cwd)?;
        if !unique_sources.insert(source.clone()) {
            anyhow::bail!("duplicate Pi extension source; remove the duplicate before planning");
        }
        let source_fingerprint = fingerprint_source(&source)
            .map_err(|_| source_preflight_error(index, "source bytes cannot be verified"))?;
        let lock_fingerprint = fingerprint_source_locks(&source).map_err(|_| {
            source_preflight_error(index, "dependency lock bytes cannot be verified")
        })?;
        sources.push(PiLockedSource {
            source,
            source_fingerprint,
            lock_fingerprint: Some(lock_fingerprint),
        });
    }
    let pi_home = resolve_pi_home(requested_pi_home, invocation_cwd)?;
    let pi_runtime = resolve_pi_runtime(requested_pi_package, &sources, invocation_cwd)?;
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
    PiAggregatePlan::new(name, sources, pi_home, pi_runtime)
}

fn read_plan(requested_plan: &Path, invocation_cwd: &Path) -> anyhow::Result<PiAggregatePlan> {
    let plan = resolve_existing_regular_file(requested_plan, invocation_cwd, "Pi aggregate plan")?;
    let bytes = ygg_agent::secure_fs::read_regular_file_bounded(&plan, MAX_PI_PLAN_BYTES)
        .map_err(|_| anyhow::anyhow!("Pi aggregate plan cannot be read safely; regenerate it"))?;
    let plan: PiAggregatePlan = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Pi aggregate plan is invalid; regenerate it"))?;
    validate_plan_shape(&plan)?;
    Ok(plan)
}

fn preflight_plan(plan: &PiAggregatePlan) -> anyhow::Result<()> {
    validate_plan_shape(plan)?;
    let actual_digest = aggregate_plan_digest(plan)?;
    if actual_digest != plan.plan_digest {
        anyhow::bail!(
            "Pi aggregate plan changed after it was compiled; review sources and run `ygg pi plan` again"
        );
    }
    for (index, source) in plan.sources.iter().enumerate() {
        let actual_source = fingerprint_source(&source.source)
            .map_err(|_| source_preflight_error(index, "source bytes cannot be verified"))?;
        if actual_source != source.source_fingerprint {
            anyhow::bail!(
                "Pi source {} changed after planning; review it and compile a replacement plan",
                source_label(index)
            );
        }
        let expected_lock = source.lock_fingerprint.as_ref().ok_or_else(|| {
            source_preflight_error(index, "dependency lock fingerprint is missing")
        })?;
        let actual_lock = fingerprint_source_locks(&source.source).map_err(|_| {
            source_preflight_error(index, "dependency lock bytes cannot be verified")
        })?;
        if actual_lock != *expected_lock {
            anyhow::bail!(
                "Pi source {} dependency lock changed after planning; review it and compile a replacement plan",
                source_label(index)
            );
        }
    }
    let actual_runtime = runtime_identity(&plan.pi_runtime.path)
        .map_err(|_| anyhow::anyhow!("pinned Pi runtime cannot be verified; select a reviewed package and compile a replacement plan"))?;
    if actual_runtime != plan.pi_runtime {
        anyhow::bail!(
            "pinned Pi runtime changed after planning; review the package and compile a replacement plan"
        );
    }
    Ok(())
}

fn publish_plan(
    plan: &PiAggregatePlan,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    publish_plan_for_api(
        plan,
        requested_extension_root,
        invocation_cwd,
        PiBridgeApiVersion::V02,
    )
}

fn publish_plan_for_api(
    plan: &PiAggregatePlan,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
    api_version: PiBridgeApiVersion,
) -> anyhow::Result<()> {
    // This is intentionally repeated immediately before writing a discoverable package.
    preflight_plan(plan)?;
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

    let package = extension_root.join(&plan.name);
    match fs::symlink_metadata(&package) {
        Ok(_) => anyhow::bail!(
            "Pi compatibility link {:?} already exists; run `ygg pi rollback {}` or choose a new name",
            plan.name,
            plan.name
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context("cannot inspect Pi compatibility link destination");
        }
    }

    let bridge_path = package.join("bridge.mjs");
    let manifest_path = package.join("extension.toml");
    let trust_binding = PiTrustBinding {
        mode: EXPLICIT_TRUST_MODE.to_owned(),
        extension_name: plan.name.clone(),
        manifest_path: manifest_path.clone(),
    };
    let (record_name, record_text, aggregate_digest, link_identity) = if plan.sources.len() == 1 {
        let record = link_record_from_plan(plan, trust_binding.clone())?;
        (
            LINK_RECORD,
            format!("{}\n", serde_json::to_string_pretty(&record)?),
            record.aggregate_digest,
            record.link_identity,
        )
    } else {
        let record = lock_record_from_plan(plan, trust_binding.clone())?;
        (
            PI_LOCK_RECORD,
            format!("{}\n", serde_json::to_string_pretty(&record)?),
            record.aggregate_digest,
            record.link_identity,
        )
    };
    let manifest = manifest_for_plan(
        plan,
        &bridge_path,
        &manifest_path,
        &aggregate_digest,
        &link_identity,
        api_version,
    )?;
    let manifest_text = toml::to_string_pretty(&manifest)?;
    let evidence_text =
        runtime_evidence_text(plan, &trust_binding, &aggregate_digest, &link_identity)?;

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
        publication
            .write_private_file(&package.join(PI_RUNTIME_EVIDENCE_RECORD), &evidence_text)?;
        publication.write_private_file(&package.join(record_name), &record_text)?;
        // Manifest discovery is the activation boundary: write it atomically last
        // so another Ygg process cannot launch a partially generated package.
        publication.write_private_file(&manifest_path, &manifest_text)?;
        sync_directory(&package)?;
        Ok(())
    })();
    if let Err(error) = publish_result {
        return match publication.rollback() {
            Ok(()) => Err(error),
            Err(rollback) => Err(anyhow::anyhow!(
                "{error:#}; rollback of generated Pi compatibility package also failed: {rollback:#}"
            )),
        };
    }
    publication.commit();

    crate::output::stdout_line(format!(
        "Published Pi compatibility aggregate {} with {} exact ordered source(s).",
        plan.name,
        plan.sources.len()
    ));
    crate::output::stdout_line(
        "The link remains disabled and untrusted until you explicitly enable and trust it.",
    );
    crate::output::stdout_line(format!(
        "Run: ygg --enable-extension {} --trust-extension {}",
        plan.name, plan.name
    ));
    crate::output::stdout_line(
        "No Pi package code, npm lifecycle hook, dependency installer, or extension source was run.",
    );
    Ok(())
}

fn rollback(
    requested_name: &str,
    requested_extension_root: Option<&Path>,
    invocation_cwd: &Path,
) -> anyhow::Result<()> {
    let name = validate_name(requested_name)?;
    let root = resolve_extension_root(requested_extension_root, invocation_cwd)?;
    let root = root
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("Pi compatibility link root does not exist"))?;
    reject_symlink(&root, "Ygg extension root")?;
    let package = root.join(&name);
    let metadata = fs::symlink_metadata(&package)
        .map_err(|_| anyhow::anyhow!("Pi compatibility link {name:?} does not exist"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!("Pi compatibility link {name:?} is not a managed regular directory");
    }
    let installation = load_installation(&package).ok_or_else(|| {
        anyhow::anyhow!(
            "Pi compatibility link {name:?} is not a generated Pi link; it was not moved"
        )
    })?;
    if installation.name() != name {
        anyhow::bail!(
            "Pi compatibility link {name:?} has a mismatched generated record; it was not moved"
        );
    }
    for (file, label) in [("bridge.mjs", "bridge"), ("extension.toml", "manifest")] {
        let path = package.join(file);
        let metadata = fs::symlink_metadata(&path).map_err(|_| {
            anyhow::anyhow!(
                "Pi compatibility link {name:?} has no generated {label}; it was not moved"
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!(
                "Pi compatibility link {name:?} has an unsafe generated {label}; it was not moved"
            );
        }
    }
    let archived = root.join(format!(
        ".pi-rollback-{name}-{}",
        crate::extension_package::unique_suffix()
    ));
    fs::rename(&package, &archived)
        .context("cannot move Pi compatibility link into local rollback storage")?;
    sync_directory(&root)?;
    crate::output::stdout_line(format!(
        "Rolled back Pi compatibility link {name}; it no longer participates in extension discovery."
    ));
    crate::output::stdout_line(
        "The generated package was preserved in local rollback storage. Restore it only after reviewing its pinned records.",
    );
    Ok(())
}

fn load_installation(package: &Path) -> Option<ParsedPiInstallation> {
    let lock_path = package.join(PI_LOCK_RECORD);
    if let Ok(bytes) =
        ygg_agent::secure_fs::read_regular_file_bounded(&lock_path, MAX_PI_LOCK_BYTES)
    {
        if let Ok(record) = parse_lock_record(&bytes) {
            return Some(ParsedPiInstallation::Lock(Box::new(record)));
        }
    }
    let record_path = package.join(LINK_RECORD);
    let bytes =
        ygg_agent::secure_fs::read_regular_file_bounded(&record_path, MAX_LINK_RECORD_BYTES)
            .ok()?;
    parse_link_record(&bytes)
        .ok()
        .map(ParsedPiInstallation::Link)
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
        if let Some(record) = load_installation(&path) {
            records.push(record);
        }
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

fn manifest_for_plan(
    plan: &PiAggregatePlan,
    bridge_path: &Path,
    manifest_path: &Path,
    aggregate_digest: &str,
    link_identity: &str,
    api_version: PiBridgeApiVersion,
) -> anyhow::Result<ExtensionManifest> {
    if plan.sources.is_empty() || plan.sources.len() > MAX_AGGREGATE_SOURCES {
        anyhow::bail!(
            "invalid Pi compatibility source count {}",
            plan.sources.len()
        );
    }
    let pi_home_text = path_to_utf8(&plan.pi_home, "Pi home path")?;
    let bridge_text = path_to_utf8(bridge_path, "Pi bridge path")?;
    let runtime_path = path_to_utf8(&plan.pi_runtime.path, "Pi runtime path")?;
    let manifest_text = path_to_utf8(manifest_path, "Pi link manifest path")?;
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
    for locked in &plan.sources {
        let source_text = path_to_utf8(&locked.source, "Pi extension source path")?;
        let lock = locked.lock_fingerprint.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Pi compatibility source lock fingerprint is missing; compile a replacement plan"
            )
        })?;
        entrypoint_args.extend([
            "--extension".to_owned(),
            source_text.to_owned(),
            "--source-fingerprint".to_owned(),
            locked.source_fingerprint.digest.clone(),
            "--source-lock-fingerprint".to_owned(),
            lock.digest.clone(),
        ]);
    }
    entrypoint_args.extend([
        "--agent-dir".to_owned(),
        pi_home_text.to_owned(),
        "--pi-package".to_owned(),
        runtime_path.to_owned(),
        "--pi-runtime-integrity".to_owned(),
        plan.pi_runtime.package_integrity.digest.clone(),
        "--aggregate-digest".to_owned(),
        aggregate_digest.to_owned(),
        "--link-manifest".to_owned(),
        manifest_text.to_owned(),
        "--link-identity".to_owned(),
        link_identity.to_owned(),
        "--ygg-version".to_owned(),
        plan.ygg_version.clone(),
        "--api-version".to_owned(),
        api_version.argument().to_owned(),
        "--command".to_owned(),
        plan.name.clone(),
    ]);

    Ok(ExtensionManifest {
        name: plan.name.clone(),
        version: BRIDGE_VERSION.to_owned(),
        api_version: api_version.extension_api_version().to_owned(),
        requires_ygg: Some(format!("={YGG_VERSION}")),
        description: Some(format!(
            "Pinned Pi compatibility aggregate for {} ordered source(s)",
            plan.sources.len()
        )),
        entrypoint: ExtensionEntrypoint {
            command: entrypoint_command,
            args: entrypoint_args,
            env: Default::default(),
        },
        capabilities: ExtensionCapabilities {
            filesystem: ExtensionFilesystemAccess::Unrestricted,
            process: api_version == PiBridgeApiVersion::V02,
            // API 0.3 provider declarations receive no network authority;
            // the host owns credentials, endpoints, and provider transport.
            network: api_version == PiBridgeApiVersion::V02,
            secrets: Vec::new(),
            environment: Vec::new(),
        },
        contributes: ManifestContributions {
            tools: if api_version == PiBridgeApiVersion::V03 {
                // API 0.3 has a fixed initial tool catalog. The bridge exposes
                // this declared dispatcher and resolves Pi's runtime tool name
                // inside its canonical arguments.
                vec![plan.name.clone()]
            } else {
                Vec::new()
            },
            commands: if api_version == PiBridgeApiVersion::V02 {
                vec![plan.name.clone()]
            } else {
                Vec::new()
            },
            shortcuts: Vec::new(),
            hooks: if api_version == PiBridgeApiVersion::V02 {
                vec![
                    ExtensionHook::AfterResponse,
                    ExtensionHook::BeforeToolCall,
                    ExtensionHook::AfterToolCall,
                ]
            } else {
                Vec::new()
            },
            ui: if api_version == PiBridgeApiVersion::V02 {
                vec![ExtensionUiSurface::Status]
            } else {
                Vec::new()
            },
            flags: Vec::new(),
            context: api_version == PiBridgeApiVersion::V02,
            tool_renderers: Vec::new(),
            notifications: api_version == PiBridgeApiVersion::V02,
            confirmations: api_version == PiBridgeApiVersion::V02,
            presentation: false,
            providers: api_version == PiBridgeApiVersion::V03,
        },
        // An aggregate is one exact ordered bridge invocation. Its generated
        // arguments carry every locked source fingerprint, so the runtime
        // catalog's complete manifest digest cannot pool a subset/reordering.
        runtime: if plan.sources.len() > 1 {
            ExtensionRuntimeSettings {
                lifecycle: ExtensionLifecycleProfile::PiAggregate,
                sharing: ExtensionRuntimeSharing::Workspace,
            }
        } else {
            ExtensionRuntimeSettings::default()
        },
    })
}

fn path_to_utf8<'a>(path: &'a Path, kind: &str) -> anyhow::Result<&'a str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("{kind} is not valid UTF-8"))
}

fn validate_plan_shape(plan: &PiAggregatePlan) -> anyhow::Result<()> {
    if plan.schema != PI_PLAN_SCHEMA || plan.schema_version != PI_PLAN_SCHEMA_VERSION {
        anyhow::bail!("unsupported Pi aggregate plan schema; compile a replacement plan");
    }
    if plan.bridge_version != BRIDGE_VERSION
        || plan.pi_version != SUPPORTED_PI_VERSION
        || plan.ygg_version != YGG_VERSION
        || plan.lifecycle_profile != PI_AGGREGATE_LIFECYCLE_PROFILE
        || plan.trust.mode != EXPLICIT_TRUST_MODE
    {
        anyhow::bail!(
            "Pi aggregate plan targets a different compatibility profile; compile a replacement plan"
        );
    }
    validate_name(&plan.name)?;
    if plan.sources.is_empty() || plan.sources.len() > MAX_AGGREGATE_SOURCES {
        anyhow::bail!("Pi aggregate plan has an invalid source count");
    }
    let mut unique = std::collections::BTreeSet::new();
    for (index, source) in plan.sources.iter().enumerate() {
        if !unique.insert(&source.source) {
            anyhow::bail!(
                "Pi aggregate plan has duplicate source {}",
                source_label(index)
            );
        }
        if !source.source.is_absolute() || !valid_source_fingerprint(&source.source_fingerprint) {
            anyhow::bail!("Pi aggregate plan has invalid source fingerprint metadata");
        }
        if !source
            .lock_fingerprint
            .as_ref()
            .is_some_and(valid_lock_fingerprint)
        {
            anyhow::bail!("Pi aggregate plan has invalid dependency lock fingerprint metadata");
        }
    }
    if !plan.pi_home.is_absolute()
        || !plan.pi_runtime.path.is_absolute()
        || !valid_runtime_integrity(&plan.pi_runtime.package_integrity)
    {
        anyhow::bail!("Pi aggregate plan has invalid pinned path or runtime metadata");
    }
    if !valid_sha256(&plan.plan_digest) {
        anyhow::bail!("Pi aggregate plan has an invalid digest");
    }
    Ok(())
}

fn aggregate_plan_digest(plan: &PiAggregatePlan) -> anyhow::Result<String> {
    let mut unsigned = plan.clone();
    unsigned.plan_digest.clear();
    let bytes = serde_json::to_vec(&unsigned)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-aggregate-plan\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn link_record_from_plan(
    plan: &PiAggregatePlan,
    trust_binding: PiTrustBinding,
) -> anyhow::Result<PiLinkRecord> {
    let source = plan
        .sources
        .first()
        .ok_or_else(|| anyhow::anyhow!("Pi aggregate plan has no sources"))?;
    let mut record = PiLinkRecord {
        schema_version: LINK_SCHEMA_VERSION,
        bridge_version: plan.bridge_version.clone(),
        pi_version: plan.pi_version.clone(),
        ygg_version: plan.ygg_version.clone(),
        source_fingerprint: source.source_fingerprint.clone(),
        source_lock_fingerprint: source.lock_fingerprint.clone(),
        name: plan.name.clone(),
        source: source.source.clone(),
        pi_home: plan.pi_home.clone(),
        pi_package: Some(plan.pi_runtime.path.clone()),
        pi_runtime: Some(plan.pi_runtime.clone()),
        aggregate_digest: String::new(),
        trust_binding: None,
        link_identity: String::new(),
    };
    record.aggregate_digest = link_record_digest(&record)?;
    record.trust_binding = Some(trust_binding.clone());
    record.link_identity = link_identity(
        &record.aggregate_digest,
        &record.name,
        &record.pi_home,
        &plan.pi_runtime,
        &plan.sources,
        &trust_binding,
    )?;
    Ok(record)
}

fn lock_record_from_plan(
    plan: &PiAggregatePlan,
    trust_binding: PiTrustBinding,
) -> anyhow::Result<PiLockRecord> {
    let mut record = PiLockRecord {
        schema_version: PI_LOCK_SCHEMA_VERSION,
        bridge_version: plan.bridge_version.clone(),
        pi_version: plan.pi_version.clone(),
        ygg_version: plan.ygg_version.clone(),
        name: plan.name.clone(),
        sources: plan.sources.clone(),
        pi_home: plan.pi_home.clone(),
        pi_package: Some(plan.pi_runtime.path.clone()),
        pi_runtime: Some(plan.pi_runtime.clone()),
        aggregate_digest: String::new(),
        trust_binding: None,
        link_identity: String::new(),
    };
    record.aggregate_digest = aggregate_lock_digest(&record)?;
    record.trust_binding = Some(trust_binding.clone());
    record.link_identity = link_identity(
        &record.aggregate_digest,
        &record.name,
        &record.pi_home,
        &plan.pi_runtime,
        &plan.sources,
        &trust_binding,
    )?;
    Ok(record)
}

fn link_record_digest(record: &PiLinkRecord) -> anyhow::Result<String> {
    let mut unsigned = record.clone();
    unsigned.aggregate_digest.clear();
    unsigned.trust_binding = None;
    unsigned.link_identity.clear();
    let bytes = serde_json::to_vec(&unsigned)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-link-record\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn aggregate_lock_digest(record: &PiLockRecord) -> anyhow::Result<String> {
    let mut unsigned = record.clone();
    unsigned.aggregate_digest.clear();
    unsigned.trust_binding = None;
    unsigned.link_identity.clear();
    let bytes = serde_json::to_vec(&unsigned)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-aggregate-lock\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn link_identity(
    aggregate_digest: &str,
    name: &str,
    pi_home: &Path,
    pi_runtime: &PiRuntimeIdentity,
    sources: &[PiLockedSource],
    trust: &PiTrustBinding,
) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-aggregate-link-identity\0");
    hasher.update(LINK_IDENTITY_FORMAT.to_be_bytes());
    for value in [
        BRIDGE_VERSION,
        SUPPORTED_PI_VERSION,
        YGG_VERSION,
        name,
        path_to_utf8(&trust.manifest_path, "Pi link manifest path")?,
        path_to_utf8(&pi_runtime.path, "Pi runtime path")?,
        &pi_runtime.package_integrity.digest,
        aggregate_digest,
        EXPLICIT_TRUST_MODE,
        path_to_utf8(pi_home, "Pi home path")?,
    ] {
        hash_framed(&mut hasher, value.as_bytes());
    }
    hasher.update((sources.len() as u32).to_be_bytes());
    for source in sources {
        let lock = source.lock_fingerprint.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Pi compatibility source lock fingerprint is missing")
        })?;
        for value in [
            path_to_utf8(&source.source, "Pi extension source path")?,
            &source.source_fingerprint.digest,
            &lock.digest,
        ] {
            hash_framed(&mut hasher, value.as_bytes());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn runtime_evidence_text(
    plan: &PiAggregatePlan,
    trust: &PiTrustBinding,
    aggregate_digest: &str,
    link_identity: &str,
) -> anyhow::Result<String> {
    let sources = plan
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            serde_json::json!({
                "position": index + 1,
                "source_sha256": source.source_fingerprint.digest,
                "lock_sha256": source.lock_fingerprint.as_ref().map(|lock| lock.digest.clone()),
            })
        })
        .collect::<Vec<_>>();
    let evidence = serde_json::json!({
        "schema": PI_RUNTIME_EVIDENCE_SCHEMA,
        "schema_version": 1,
        "api": {
            "version": ygg_agent::extension_api_v03::API_VERSION,
            "schema": ygg_agent::extension_api_v03::SCHEMA_ID,
            "schema_sha256": ygg_agent::extension_api_v03::SCHEMA_SHA256,
        },
        "lifecycle_profile": PI_AGGREGATE_LIFECYCLE_PROFILE,
        "link_identity": link_identity,
        "aggregate_digest": aggregate_digest,
        "source_count": plan.sources.len(),
        "sources": sources,
        "runtime": {
            "pi_version": plan.pi_version,
            "package_sha256": plan.pi_runtime.package_integrity.digest,
        },
        "trust": {
            "mode": trust.mode,
            "extension_name": trust.extension_name,
        },
    });
    let canonical = ygg_agent::extension_api_v03::canonical_json(&evidence)
        .map_err(|_| anyhow::anyhow!("cannot canonicalize Pi runtime evidence"))?;
    Ok(format!("{canonical}\n"))
}

fn valid_source_fingerprint(fingerprint: &SourceFingerprint) -> bool {
    fingerprint.algorithm == SOURCE_FINGERPRINT_ALGORITHM
        && fingerprint.format_version == SOURCE_FINGERPRINT_FORMAT
        && valid_sha256(&fingerprint.digest)
}

fn valid_lock_fingerprint(fingerprint: &SourceLockFingerprint) -> bool {
    fingerprint.algorithm == SOURCE_FINGERPRINT_ALGORITHM
        && fingerprint.format_version == SOURCE_LOCK_FINGERPRINT_FORMAT
        && valid_sha256(&fingerprint.digest)
}

fn valid_runtime_integrity(integrity: &PiRuntimeIntegrity) -> bool {
    integrity.algorithm == SOURCE_FINGERPRINT_ALGORITHM
        && integrity.format_version == PI_RUNTIME_INTEGRITY_FORMAT
        && valid_sha256(&integrity.digest)
}

fn source_label(index: usize) -> String {
    format!("#{}", index + 1)
}

fn source_preflight_error(index: usize, reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Pi source {} {reason}; review it and compile a replacement plan",
        source_label(index)
    )
}

fn parse_link_record(bytes: &[u8]) -> anyhow::Result<ParsedPiLinkRecord> {
    let schema: LinkRecordSchema = serde_json::from_slice(bytes)?;
    match schema.schema_version {
        1 => Ok(ParsedPiLinkRecord::Legacy(serde_json::from_slice(bytes)?)),
        2 | LINK_SCHEMA_VERSION => Ok(ParsedPiLinkRecord::V2(Box::new(serde_json::from_slice(
            bytes,
        )?))),
        version => anyhow::bail!("unsupported Pi link record schema {version}"),
    }
}

fn parse_lock_record(bytes: &[u8]) -> anyhow::Result<PiLockRecord> {
    let record: PiLockRecord = serde_json::from_slice(bytes)?;
    if record.schema_version != 1 && record.schema_version != PI_LOCK_SCHEMA_VERSION {
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
            anyhow::bail!("duplicate Pi aggregate source");
        }
    }
    Ok(record)
}

fn link_status(record: &ParsedPiLinkRecord) -> String {
    let ParsedPiLinkRecord::V2(record) = record else {
        return "legacy/stale (schema v1 lacks compatibility and source trust metadata)".to_owned();
    };
    let mut stale = Vec::new();
    if record.schema_version != LINK_SCHEMA_VERSION {
        stale.push("link schema changed".to_owned());
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
    if !valid_source_fingerprint(&record.source_fingerprint) {
        stale.push("source fingerprint metadata is invalid".to_owned());
    } else {
        match fingerprint_source(&record.source) {
            Ok(actual) if actual != record.source_fingerprint => {
                stale.push("source changed".to_owned())
            }
            Err(_) => stale.push("source cannot be verified".to_owned()),
            Ok(_) => {}
        }
    }
    let Some(lock) = record.source_lock_fingerprint.as_ref() else {
        stale.push("dependency lock fingerprint is missing".to_owned());
        return format!("stale ({})", stale.join("; "));
    };
    if !valid_lock_fingerprint(lock) {
        stale.push("dependency lock fingerprint metadata is invalid".to_owned());
    } else {
        match fingerprint_source_locks(&record.source) {
            Ok(actual) if actual != *lock => stale.push("dependency lock changed".to_owned()),
            Err(_) => stale.push("dependency lock cannot be verified".to_owned()),
            Ok(_) => {}
        }
    }
    let Some(runtime) = record.pi_runtime.as_ref() else {
        stale.push("pinned Pi runtime metadata is missing".to_owned());
        return format!("stale ({})", stale.join("; "));
    };
    if !valid_runtime_integrity(&runtime.package_integrity) {
        stale.push("pinned Pi runtime integrity metadata is invalid".to_owned());
    } else {
        match runtime_identity(&runtime.path) {
            Ok(actual) if actual != *runtime => stale.push("pinned Pi runtime changed".to_owned()),
            Err(_) => stale.push("pinned Pi runtime cannot be verified".to_owned()),
            Ok(_) => {}
        }
    }
    let Some(trust) = record.trust_binding.as_ref() else {
        stale.push("explicit enable/trust binding is missing".to_owned());
        return format!("stale ({})", stale.join("; "));
    };
    if trust.mode != EXPLICIT_TRUST_MODE
        || trust.extension_name != record.name
        || !trust.manifest_path.is_absolute()
    {
        stale.push("explicit enable/trust binding is invalid".to_owned());
    }
    if !valid_sha256(&record.aggregate_digest)
        || link_record_digest(record).ok().as_deref() != Some(&record.aggregate_digest)
    {
        stale.push("link record digest changed".to_owned());
    }
    if let (Some(runtime), Some(trust)) =
        (record.pi_runtime.as_ref(), record.trust_binding.as_ref())
    {
        match link_identity(
            &record.aggregate_digest,
            &record.name,
            &record.pi_home,
            runtime,
            &[PiLockedSource {
                source: record.source.clone(),
                source_fingerprint: record.source_fingerprint.clone(),
                lock_fingerprint: record.source_lock_fingerprint.clone(),
            }],
            trust,
        ) {
            Ok(actual) if actual != record.link_identity => {
                stale.push("link identity changed".to_owned())
            }
            Err(_) => stale.push("link identity cannot be verified".to_owned()),
            Ok(_) => {}
        }
    }
    if stale.is_empty() {
        "metadata-current (explicit enable/trust required; trust decision not asserted)".to_owned()
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
    if !valid_sha256(&record.aggregate_digest)
        || aggregate_lock_digest(record).ok().as_deref() != Some(&record.aggregate_digest)
    {
        stale.push("aggregate lock digest changed".to_owned());
    }
    let mut unique = std::collections::BTreeSet::new();
    for (index, source) in record.sources.iter().enumerate() {
        if !unique.insert(&source.source) {
            stale.push(format!("source {} is duplicated", index + 1));
            continue;
        }
        if !valid_source_fingerprint(&source.source_fingerprint) {
            stale.push(format!("source {} fingerprint is invalid", index + 1));
            continue;
        }
        match fingerprint_source(&source.source) {
            Ok(actual) if actual != source.source_fingerprint => {
                stale.push(format!("source {} changed", index + 1));
            }
            Err(_) => stale.push(format!("source {} cannot be verified", index + 1)),
            Ok(_) => {}
        }
        let Some(lock) = source.lock_fingerprint.as_ref() else {
            stale.push(format!(
                "source {} dependency lock fingerprint is missing",
                index + 1
            ));
            continue;
        };
        if !valid_lock_fingerprint(lock) {
            stale.push(format!(
                "source {} dependency lock fingerprint is invalid",
                index + 1
            ));
            continue;
        }
        match fingerprint_source_locks(&source.source) {
            Ok(actual) if actual != *lock => {
                stale.push(format!("source {} dependency lock changed", index + 1));
            }
            Err(_) => stale.push(format!(
                "source {} dependency lock cannot be verified",
                index + 1
            )),
            Ok(_) => {}
        }
    }
    let Some(runtime) = record.pi_runtime.as_ref() else {
        stale.push("pinned Pi runtime metadata is missing".to_owned());
        return format!("stale ({})", stale.join("; "));
    };
    if !valid_runtime_integrity(&runtime.package_integrity) {
        stale.push("pinned Pi runtime integrity metadata is invalid".to_owned());
    } else {
        match runtime_identity(&runtime.path) {
            Ok(actual) if actual != *runtime => stale.push("pinned Pi runtime changed".to_owned()),
            Err(_) => stale.push("pinned Pi runtime cannot be verified".to_owned()),
            Ok(_) => {}
        }
    }
    let Some(trust) = record.trust_binding.as_ref() else {
        stale.push("explicit enable/trust binding is missing".to_owned());
        return format!("stale ({})", stale.join("; "));
    };
    if trust.mode != EXPLICIT_TRUST_MODE
        || trust.extension_name != record.name
        || !trust.manifest_path.is_absolute()
    {
        stale.push("explicit enable/trust binding is invalid".to_owned());
    }
    if let (Some(runtime), Some(trust)) =
        (record.pi_runtime.as_ref(), record.trust_binding.as_ref())
    {
        match link_identity(
            &record.aggregate_digest,
            &record.name,
            &record.pi_home,
            runtime,
            &record.sources,
            trust,
        ) {
            Ok(actual) if actual != record.link_identity => {
                stale.push("link identity changed".to_owned())
            }
            Err(_) => stale.push("link identity cannot be verified".to_owned()),
            Ok(_) => {}
        }
    }
    if stale.is_empty() {
        "aggregate-current (explicit enable/trust required; trust decision not asserted)".to_owned()
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

fn fingerprint_source_locks(source: &Path) -> anyhow::Result<SourceLockFingerprint> {
    if !source.is_absolute() {
        anyhow::bail!("Pi source lock fingerprint requires an absolute source path");
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        anyhow::bail!("Pi source lock fingerprint requires a regular source");
    }
    let canonical = source.canonicalize()?;
    if canonical != source {
        anyhow::bail!("Pi source lock fingerprint requires a canonical source path");
    }
    let root = if metadata.is_dir() {
        source.to_owned()
    } else {
        source
            .parent()
            .ok_or_else(|| {
                anyhow::anyhow!("Pi source has no parent for dependency lock verification")
            })?
            .to_owned()
    };
    let before = source_lock_paths(&root)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-source-lock-fingerprint\0");
    hasher.update(SOURCE_LOCK_FINGERPRINT_FORMAT.to_be_bytes());
    hasher.update((before.len() as u32).to_be_bytes());
    let mut total_bytes = 0usize;
    for (name, path) in &before {
        hash_framed(&mut hasher, name.as_bytes());
        let remaining = MAX_LOCK_BYTES.saturating_sub(total_bytes);
        let read = hash_regular_file(&mut hasher, path, remaining, MAX_LOCK_BYTES)?;
        total_bytes = total_bytes
            .checked_add(read)
            .ok_or_else(|| anyhow::anyhow!("Pi source lock fingerprint byte count overflow"))?;
    }
    let after = source_lock_paths(&root)?;
    if before != after {
        anyhow::bail!("Pi source dependency lock set changed while it was being fingerprinted");
    }
    Ok(SourceLockFingerprint {
        algorithm: SOURCE_FINGERPRINT_ALGORITHM.to_owned(),
        format_version: SOURCE_LOCK_FINGERPRINT_FORMAT,
        digest: format!("{:x}", hasher.finalize()),
        file_count: u64::try_from(before.len())?,
        byte_count: u64::try_from(total_bytes)?,
    })
}

fn source_lock_paths(root: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut paths = Vec::new();
    for name in SUPPORTED_LOCK_FILES {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    anyhow::bail!("Pi source dependency lock must be a regular non-symlink file");
                }
                if metadata.len() > MAX_LOCK_FILE_BYTES as u64 {
                    anyhow::bail!(
                        "Pi source dependency lock exceeds the {MAX_LOCK_FILE_BYTES}-byte limit"
                    );
                }
                paths.push((name.to_owned(), path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(paths)
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

fn runtime_identity(path: &Path) -> anyhow::Result<PiRuntimeIdentity> {
    let root = resolve_pi_package(path, Path::new("/"))?;
    let package_integrity = pi_runtime_integrity(&root)?;
    Ok(PiRuntimeIdentity {
        path: root,
        package_integrity,
    })
}

fn pi_runtime_integrity(root: &Path) -> anyhow::Result<PiRuntimeIntegrity> {
    let manifest = ygg_agent::secure_fs::read_regular_file_bounded(
        &root.join("package.json"),
        MAX_PI_PACKAGE_MANIFEST_BYTES,
    )?;
    let distribution = fingerprint_source(&root.join("dist"))?;
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-pi-runtime-integrity\0");
    hasher.update(PI_RUNTIME_INTEGRITY_FORMAT.to_be_bytes());
    hash_framed(&mut hasher, &manifest);
    hash_framed(&mut hasher, distribution.digest.as_bytes());
    Ok(PiRuntimeIntegrity {
        algorithm: SOURCE_FINGERPRINT_ALGORITHM.to_owned(),
        format_version: PI_RUNTIME_INTEGRITY_FORMAT,
        digest: format!("{:x}", hasher.finalize()),
    })
}

fn resolve_pi_runtime(
    requested: Option<&Path>,
    sources: &[PiLockedSource],
    cwd: &Path,
) -> anyhow::Result<PiRuntimeIdentity> {
    if let Some(path) = requested {
        let root = resolve_pi_package(path, cwd)
            .map_err(|_| anyhow::anyhow!("selected Pi runtime is not @earendil-works/pi-coding-agent@{SUPPORTED_PI_VERSION}; review --pi-package"))?;
        return runtime_identity(&root).map_err(|_| {
            anyhow::anyhow!(
                "selected Pi runtime cannot be integrity-verified; review --pi-package and compile a replacement plan"
            )
        });
    }

    for variable in ["YGG_PI_PACKAGE", "PI_CODING_AGENT_PACKAGE"] {
        if let Some(path) = env::var_os(variable) {
            let root = resolve_pi_package(Path::new(&path), cwd).map_err(|_| {
                anyhow::anyhow!(
                    "configured Pi runtime is not @earendil-works/pi-coding-agent@{SUPPORTED_PI_VERSION}; review {variable}"
                )
            })?;
            return runtime_identity(&root).map_err(|_| {
                anyhow::anyhow!(
                    "configured Pi runtime cannot be integrity-verified; review {variable} and compile a replacement plan"
                )
            });
        }
    }

    let mut candidates = Vec::new();
    for source in sources {
        let mut current = if source.source.is_dir() {
            source.source.clone()
        } else {
            source.source.parent().unwrap_or(&source.source).to_owned()
        };
        for _ in 0..8 {
            candidates.push(current.clone());
            let Some(parent) = current.parent() else {
                break;
            };
            current = parent.to_owned();
        }
    }
    candidates.push(cwd.join("node_modules/@earendil-works/pi-coding-agent"));
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".pi/agent/node_modules/@earendil-works/pi-coding-agent"));
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            if directory.file_name().is_some_and(|name| name == ".bin") {
                if let Some(node_modules) = directory.parent() {
                    candidates.push(node_modules.join("@earendil-works/pi-coding-agent"));
                }
            }
        }
    }

    let mut seen = std::collections::BTreeSet::new();
    for candidate in candidates {
        let candidate = if candidate
            .file_name()
            .is_some_and(|name| name == "pi-coding-agent")
        {
            candidate
        } else {
            candidate.join("node_modules/@earendil-works/pi-coding-agent")
        };
        if !seen.insert(candidate.clone()) || !candidate.exists() {
            continue;
        }
        if let Ok(identity) = runtime_identity(&candidate) {
            return Ok(identity);
        }
    }
    anyhow::bail!(
        "could not locate a pinned @earendil-works/pi-coding-agent@{SUPPORTED_PI_VERSION} runtime; install it separately or pass --pi-package DIR. No package code was run."
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
    absolute_path(&home.join(".pi/agent"), cwd)
}

fn resolve_extension_root(requested: Option<&Path>, cwd: &Path) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        return absolute_path(path, cwd);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("user home is unavailable"))?;
    absolute_path(&home.join(".ygg/extensions"), cwd)
}

fn absolute_path(path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    if !path.is_absolute() {
        anyhow::bail!("Pi path must resolve from an absolute invocation directory");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // `PathBuf::pop` stops at a filesystem root, matching ordinary
                // absolute path resolution while retaining a non-existent Pi home.
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn resolve_existing_regular_file(
    requested: &Path,
    cwd: &Path,
    label: &str,
) -> anyhow::Result<PathBuf> {
    let path = absolute_path(requested, cwd)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| anyhow::anyhow!("{label} does not exist or cannot be inspected"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("{label} must be a regular non-symlink file");
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("{label} cannot be resolved"))?;
    if canonical != path {
        anyhow::bail!("{label} must use a canonical path");
    }
    Ok(canonical)
}

fn resolve_new_plan_path(requested: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = absolute_path(requested, cwd)?;
    let Some(parent) = path.parent() else {
        anyhow::bail!("Pi aggregate plan output requires a parent directory");
    };
    let Some(name) = path.file_name() else {
        anyhow::bail!("Pi aggregate plan output requires a file name");
    };
    if name == "." || name == ".." {
        anyhow::bail!("Pi aggregate plan output has an invalid file name");
    }
    let parent = parent
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("Pi aggregate plan output parent does not exist"))?;
    reject_symlink(&parent, "Pi aggregate plan output parent")?;
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(_) => anyhow::bail!("Pi aggregate plan output already exists; choose a new file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(error.into()),
    }
}

fn write_new_private_file(path: &Path, text: &str) -> anyhow::Result<()> {
    ygg_agent::secure_fs::write_atomic_if_unchanged(path, None, text.as_bytes(), MAX_PI_PLAN_BYTES)
        .map_err(|_| {
            anyhow::anyhow!(
                "cannot safely create Pi aggregate plan output; choose a private new file"
            )
        })
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
            "invalid Pi compatibility name {name:?}; use a lowercase letter followed by lowercase letters, digits, or '-'"
        )
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

    fn fake_pi_package(temp: &tempfile::TempDir) -> PathBuf {
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
        canonical(&package)
    }

    fn test_plan(temp: &tempfile::TempDir, sources: &[PathBuf], name: &str) -> PiAggregatePlan {
        compile_requested_plan(
            &sources[0],
            &sources[1..],
            Some(name),
            Some(&temp.path().join("pi-home")),
            Some(&fake_pi_package(temp)),
            temp.path(),
        )
        .unwrap()
    }

    fn test_trust(temp: &tempfile::TempDir, name: &str) -> PiTrustBinding {
        PiTrustBinding {
            mode: EXPLICIT_TRUST_MODE.to_owned(),
            extension_name: name.to_owned(),
            manifest_path: canonical(temp.path()).join("link/extension.toml"),
        }
    }

    #[test]
    fn absolute_paths_are_lexically_normalized_for_bridge_identity() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = canonical(temp.path());
        assert_eq!(
            absolute_path(Path::new("nested/../pi-home"), &cwd).unwrap(),
            cwd.join("pi-home")
        );
        assert_eq!(
            absolute_path(Path::new("../sibling-pi-home"), &cwd).unwrap(),
            cwd.parent().unwrap().join("sibling-pi-home")
        );
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
        let plan = test_plan(&temp, std::slice::from_ref(&source), "pi-example");
        let record = ParsedPiLinkRecord::V2(Box::new(
            link_record_from_plan(&plan, test_trust(&temp, "pi-example")).unwrap(),
        ));
        fs::write(&source, b"after").unwrap();
        let after = fingerprint_source(&source).unwrap();
        assert_ne!(before.digest, after.digest);
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
    fn v3_record_manifest_and_evidence_bind_exact_compatibility_identity() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("example.ts");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let source = canonical(&source);
        let plan = test_plan(&temp, std::slice::from_ref(&source), "pi-example");
        let trust = test_trust(&temp, "pi-example");
        let record = link_record_from_plan(&plan, trust.clone()).unwrap();
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
        assert!(valid_sha256(
            record
                .source_lock_fingerprint
                .as_ref()
                .unwrap()
                .digest
                .as_str()
        ));
        assert!(valid_sha256(
            record
                .pi_runtime
                .as_ref()
                .unwrap()
                .package_integrity
                .digest
                .as_str()
        ));
        assert!(valid_sha256(&record.link_identity));
        assert_eq!(
            link_status(&ParsedPiLinkRecord::V2(Box::new(record.clone()))),
            "metadata-current (explicit enable/trust required; trust decision not asserted)"
        );

        let bridge = canonical(temp.path()).join("link/bridge.mjs");
        let manifest = manifest_for_plan(
            &plan,
            &bridge,
            &trust.manifest_path,
            &record.aggregate_digest,
            &record.link_identity,
            PiBridgeApiVersion::V02,
        )
        .unwrap();
        assert_eq!(manifest.version, BRIDGE_VERSION);
        assert_eq!(
            manifest.requires_ygg.as_deref(),
            Some(concat!("=", env!("CARGO_PKG_VERSION")))
        );
        if cfg!(unix) {
            assert_eq!(manifest.entrypoint.command, bridge.to_str().unwrap());
            assert_eq!(manifest.entrypoint.args.first().unwrap(), "--extension");
        } else {
            assert_eq!(manifest.entrypoint.command, "node");
            assert!(manifest.entrypoint.args[0].ends_with("bridge.mjs"));
        }
        for expected in [
            "--source-fingerprint",
            "--source-lock-fingerprint",
            "--pi-runtime-integrity",
            "--aggregate-digest",
            "--link-manifest",
            "--link-identity",
            "--ygg-version",
        ] {
            assert!(manifest.entrypoint.args.iter().any(|arg| arg == expected));
        }
        assert_eq!(manifest.contributes.commands, ["pi-example"]);
        assert_eq!(manifest.contributes.hooks.len(), 3);
        assert!(!manifest
            .contributes
            .hooks
            .contains(&ExtensionHook::BeforePrompt));
        assert!(manifest.contributes.context);

        let evidence = runtime_evidence_text(
            &plan,
            &trust,
            &record.aggregate_digest,
            &record.link_identity,
        )
        .unwrap();
        let evidence_value: serde_json::Value = serde_json::from_str(&evidence).unwrap();
        assert_eq!(
            evidence_value["api"]["version"],
            ygg_agent::extension_api_v03::API_VERSION
        );
        assert_eq!(
            ygg_agent::extension_api_v03::canonical_json(&evidence_value).unwrap(),
            evidence.trim()
        );
    }

    #[test]
    fn api_03_manifest_selects_only_host_owned_provider_contracts() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("provider.mjs");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let source = canonical(&source);
        let plan = test_plan(&temp, std::slice::from_ref(&source), "pi-provider");
        let trust = test_trust(&temp, "pi-provider");
        let record = link_record_from_plan(&plan, trust.clone()).unwrap();
        let manifest = manifest_for_plan(
            &plan,
            &canonical(temp.path()).join("link/bridge.mjs"),
            &trust.manifest_path,
            &record.aggregate_digest,
            &record.link_identity,
            PiBridgeApiVersion::V03,
        )
        .unwrap();

        assert_eq!(manifest.api_version, EXTENSION_API_VERSION_0_3);
        assert!(!manifest.capabilities.process);
        assert!(!manifest.capabilities.network);
        assert!(manifest.contributes.providers);
        assert_eq!(manifest.contributes.tools, vec!["pi-provider".to_owned()]);
        assert!(manifest.contributes.commands.is_empty());
        assert!(manifest.contributes.hooks.is_empty());
        assert!(manifest.contributes.ui.is_empty());
        assert!(!manifest.contributes.context);
        assert!(!manifest.contributes.notifications);
        assert!(!manifest.contributes.confirmations);
        assert!(manifest
            .entrypoint
            .args
            .windows(2)
            .any(|args| args == ["--api-version", "0.3"]));
        manifest.validate().unwrap();
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
        let sources = vec![first.clone(), second.clone()];
        let plan = test_plan(&temp, &sources, "pi-aggregate");
        assert_eq!(plan.sources[0].source, first);
        assert_eq!(plan.sources[1].source, second);
        let trust = test_trust(&temp, "pi-aggregate");
        let record = lock_record_from_plan(&plan, trust.clone()).unwrap();
        let encoded = serde_json::to_vec(&record).unwrap();
        let decoded = parse_lock_record(&encoded).unwrap();
        assert_eq!(decoded.sources, plan.sources);
        assert_eq!(
            aggregate_status(&decoded),
            "aggregate-current (explicit enable/trust required; trust decision not asserted)"
        );

        let manifest = manifest_for_plan(
            &plan,
            &canonical(temp.path()).join("link/bridge.mjs"),
            &trust.manifest_path,
            &record.aggregate_digest,
            &record.link_identity,
            PiBridgeApiVersion::V02,
        )
        .unwrap();
        assert_eq!(
            manifest.runtime,
            ExtensionRuntimeSettings {
                lifecycle: ExtensionLifecycleProfile::PiAggregate,
                sharing: ExtensionRuntimeSharing::Workspace,
            }
        );
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
            Some(&fake_pi_package(&temp)),
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
            "aggregate-current (explicit enable/trust required; trust decision not asserted)"
        );
    }

    #[test]
    fn plan_preflight_rejects_dependency_lock_mutation_without_executing_source() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("source-ran");
        let source = temp.path().join("extension.mjs");
        fs::write(
            &source,
            format!(
                "import {{ writeFileSync }} from 'node:fs'; writeFileSync({:?}, 'ran');\n",
                marker
            ),
        )
        .unwrap();
        fs::write(
            temp.path().join("package-lock.json"),
            b"{\"lockfileVersion\":3}\n",
        )
        .unwrap();
        let source = canonical(&source);
        let plan = test_plan(&temp, std::slice::from_ref(&source), "pi-preflight");
        preflight_plan(&plan).unwrap();
        assert!(!marker.exists(), "preflight must not import source code");
        fs::write(
            temp.path().join("package-lock.json"),
            b"{\"lockfileVersion\":4}\n",
        )
        .unwrap();
        let error = preflight_plan(&plan).unwrap_err().to_string();
        assert!(error.contains("dependency lock changed"));
        assert!(
            !marker.exists(),
            "stale preflight must not import source code"
        );
    }

    #[test]
    fn publish_writes_api_v03_evidence_and_rollback_preserves_the_package() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("extension.mjs");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let source = canonical(&source);
        let plan = test_plan(&temp, std::slice::from_ref(&source), "pi-rollback");
        let extension_root = temp.path().join("extensions");
        publish_plan(&plan, Some(&extension_root), temp.path()).unwrap();
        let package = extension_root.join("pi-rollback");
        assert!(package.join(PI_RUNTIME_EVIDENCE_RECORD).is_file());
        let evidence: serde_json::Value =
            serde_json::from_slice(&fs::read(package.join(PI_RUNTIME_EVIDENCE_RECORD)).unwrap())
                .unwrap();
        assert_eq!(evidence["schema"], PI_RUNTIME_EVIDENCE_SCHEMA);
        assert_eq!(
            evidence["api"]["version"],
            ygg_agent::extension_api_v03::API_VERSION
        );
        rollback("pi-rollback", Some(&extension_root), temp.path()).unwrap();
        assert!(!package.exists());
        assert!(fs::read_dir(&extension_root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pi-rollback-pi-rollback-")));
    }

    #[test]
    fn rollback_refuses_a_lookalike_package() {
        let temp = tempfile::tempdir().unwrap();
        let extension_root = temp.path().join("extensions");
        let package = extension_root.join("pi-lookalike");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join(LINK_RECORD),
            r#"{
                "schema_version": 1,
                "bridge_version": "0.1.1",
                "name": "pi-lookalike",
                "source": "/tmp/source.mjs",
                "pi_home": "/tmp/pi-home"
            }"#,
        )
        .unwrap();
        let error = rollback("pi-lookalike", Some(&extension_root), temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no generated bridge"));
        assert!(package.is_dir());
    }

    #[test]
    fn selected_pi_package_integrity_is_pinned_and_runtime_changes_are_stale() {
        let temp = tempfile::tempdir().unwrap();
        let package = fake_pi_package(&temp);
        let source = temp.path().join("extension.ts");
        fs::write(&source, b"export default () => {};\n").unwrap();
        let source = canonical(&source);
        let plan = compile_requested_plan(
            &source,
            &[],
            Some("pi-example"),
            Some(&temp.path().join("pi-home")),
            Some(&package),
            temp.path(),
        )
        .unwrap();
        let trust = test_trust(&temp, "pi-example");
        let record = link_record_from_plan(&plan, trust).unwrap();
        assert!(valid_sha256(&plan.pi_runtime.package_integrity.digest));
        fs::write(
            package.join("dist/index.js"),
            b"export const changed = true;\n",
        )
        .unwrap();
        assert!(link_status(&ParsedPiLinkRecord::V2(Box::new(record)))
            .contains("pinned Pi runtime changed"));
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
        let extension_store = temp.path().join("extension-store");
        fs::create_dir_all(&extension_store).unwrap();
        // Model macOS's /var -> /private/var alias with a deterministic Unix symlink.
        let extension_root_alias = temp.path().join("extension-root-alias");
        std::os::unix::fs::symlink(&extension_store, &extension_root_alias).unwrap();
        let extension_root = extension_root_alias.join("extensions");
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
        assert_ne!(manifest_path, canonical(&manifest_path));
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
            .contains("changed after link publication"));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_aggregate_runs_pinned_real_pi_examples_when_selected() {
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
        let hello = pi_package.join("examples/extensions/hello.ts");
        let plan_mode = pi_package.join("examples/extensions/plan-mode/index.ts");
        if !hello.is_file() || !plan_mode.is_file() {
            panic!(
                "YGG_PI_REAL_PACKAGE does not contain the reviewed hello and plan-mode examples"
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let extension_root = temp.path().join("extensions");
        install_sources(
            &[hello, plan_mode],
            Some("pi-real-aggregate"),
            Some(&temp.path().join("pi-home")),
            Some(&pi_package),
            Some(&extension_root),
            temp.path(),
        )
        .unwrap();
        let manifest_path = extension_root.join("pi-real-aggregate/extension.toml");
        let mut policy = ygg_agent::ExtensionPolicy::default();
        policy.enable("pi-real-aggregate");
        policy.trust_for_invocation("pi-real-aggregate");
        let catalog = ygg_agent::ExtensionCatalog::load_resolved(
            [ygg_agent::extension_process::ExtensionManifestInput {
                path: manifest_path,
                source: ygg_agent::ExtensionSource::Explicit,
            }],
            &policy,
            64 * 1024,
        );
        assert!(catalog.diagnostics.is_empty(), "{:?}", catalog.diagnostics);
        let process = ygg_agent::ExtensionProcess::start(
            catalog.extensions.into_iter().next().unwrap(),
            ygg_agent::ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        .unwrap();
        assert!(process
            .contributions()
            .tools
            .iter()
            .any(|tool| tool.name == "hello"));
        let commands = process
            .contributions()
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>();
        assert!(commands.contains(&"plan"));
        assert!(commands.contains(&"todos"));
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
