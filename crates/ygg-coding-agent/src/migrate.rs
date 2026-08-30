#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use clap::Subcommand;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

const MAX_SETTINGS_BYTES: usize = 1024 * 1024;
const MAX_PACKAGES: usize = 256;
const MAX_PATTERNS: usize = 1024;
const MAX_PACKAGE_JSON_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOCK_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCE_FILES: usize = 4096;
const MAX_WALK_ENTRIES: usize = 16_384;
const MAX_ANALYZED_FILES: usize = 512;
const MAX_EXTENSION_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_AST_NODES: usize = 2_000_000;
const MAX_SCAN_SOURCE_BYTES: usize = 128 * 1024 * 1024;
const MAX_SCAN_AST_NODES: usize = 8_000_000;
const MAX_SCAN_ANALYZED_FILES: usize = 8192;
const MAX_SCAN_RESOURCES: usize = 32_768;
const MAX_SCAN_HASH_BYTES: usize = 256 * 1024 * 1024;
const MAX_PACKAGE_DIAGNOSTICS: usize = 256;
const MAX_REPORT_DIAGNOSTICS: usize = 1024;
const MAX_PACKAGE_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_EXTENSION_MANIFEST_DEPTH: usize = 64;

#[derive(Clone, Debug, Subcommand)]
pub enum MigrationCommand {
    /// Inventory a Pi setup without executing packages or invoking a model.
    Pi {
        /// Explicitly state that this invocation must not modify either setup.
        #[arg(long)]
        dry_run: bool,
        /// Emit the versioned machine-readable report.
        #[arg(long, conflicts_with = "summary")]
        json: bool,
        /// Emit only aggregate counts and diagnostics.
        #[arg(long)]
        summary: bool,
        /// Pi's user agent directory (defaults to PI_CODING_AGENT_DIR or ~/.pi/agent).
        #[arg(long, value_name = "DIR")]
        pi_home: Option<PathBuf>,
        /// Project whose .pi/settings.json and resources should be inspected.
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
        /// Additional legacy global npm node_modules root (repeatable).
        #[arg(long = "npm-root", value_name = "DIR")]
        npm_roots: Vec<PathBuf>,
    },
}

#[derive(Clone, Debug)]
struct ScanOptions {
    pi_home: PathBuf,
    project: PathBuf,
    npm_roots: Vec<PathBuf>,
}

#[derive(Default)]
struct AnalysisBudget {
    source_bytes: usize,
    syntax_nodes: usize,
    files: usize,
    resources: usize,
    hashed_bytes: usize,
}

#[derive(Default)]
struct ResourceTraversal {
    active_extension_manifests: Vec<PathBuf>,
    extension_manifest_resolution_stopped: bool,
}

impl ResourceTraversal {
    fn extension_manifest_is_active(&self, path: &Path) -> bool {
        self.active_extension_manifests
            .iter()
            .any(|active| active == path)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    User,
    Project,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum ResourceKind {
    Extension,
    Skill,
    Prompt,
    Theme,
}

impl ResourceKind {
    const ALL: [Self; 4] = [Self::Extension, Self::Skill, Self::Prompt, Self::Theme];

    fn key(self) -> &'static str {
        match self {
            Self::Extension => "extensions",
            Self::Skill => "skills",
            Self::Prompt => "prompts",
            Self::Theme => "themes",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum MigrationPath {
    Direct,
    Replace,
    Bridge,
    NativePort,
    Manual,
    Blocked,
}

impl MigrationPath {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "DIRECT",
            Self::Replace => "REPLACE",
            Self::Bridge => "BRIDGE",
            Self::NativePort => "NATIVE PORT",
            Self::Manual => "MANUAL",
            Self::Blocked => "BLOCKED",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DiagnosticLevel {
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize)]
struct Diagnostic {
    level: DiagnosticLevel,
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
}

impl Diagnostic {
    fn warning(code: &'static str, message: impl Into<String>, path: Option<PathBuf>) -> Self {
        Self {
            level: DiagnosticLevel::Warning,
            code,
            message: message.into(),
            path,
        }
    }

    fn error(code: &'static str, message: impl Into<String>, path: Option<PathBuf>) -> Self {
        Self {
            level: DiagnosticLevel::Error,
            code,
            message: message.into(),
            path,
        }
    }
}

fn cap_diagnostics(diagnostics: &mut Vec<Diagnostic>, limit: usize, path: Option<PathBuf>) {
    if diagnostics.len() <= limit {
        return;
    }
    let omitted = diagnostics.len() - limit + 1;
    diagnostics.truncate(limit.saturating_sub(1));
    diagnostics.push(Diagnostic::warning(
        "diagnostic_limit",
        format!("omitted {omitted} additional migration diagnostics"),
        path,
    ));
}

#[derive(Clone, Debug, Serialize)]
struct SettingsReport {
    scope: Scope,
    path: PathBuf,
    found: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PiSettings {
    #[serde(default)]
    packages: Vec<PackageSetting>,
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PiSettings {
    fn overrides(&self, kind: ResourceKind) -> &[String] {
        match kind {
            ResourceKind::Extension => self.extensions.as_deref().unwrap_or(&[]),
            ResourceKind::Skill => self.skills.as_deref().unwrap_or(&[]),
            ResourceKind::Prompt => self.prompts.as_deref().unwrap_or(&[]),
            ResourceKind::Theme => self.themes.as_deref().unwrap_or(&[]),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum PackageSetting {
    Source(String),
    Filter(PackageFilter),
}

impl PackageSetting {
    fn source(&self) -> &str {
        match self {
            Self::Source(source) => source,
            Self::Filter(filter) => &filter.source,
        }
    }

    fn filter(&self) -> Option<&PackageFilter> {
        match self {
            Self::Source(_) => None,
            Self::Filter(filter) => Some(filter),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct PackageFilter {
    source: String,
    #[serde(default)]
    autoload: Option<bool>,
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PackageFilter {
    fn patterns(&self, kind: ResourceKind) -> Option<&[String]> {
        match kind {
            ResourceKind::Extension => self.extensions.as_deref(),
            ResourceKind::Skill => self.skills.as_deref(),
            ResourceKind::Prompt => self.prompts.as_deref(),
            ResourceKind::Theme => self.themes.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
    pi: Option<PiManifest>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PiManifest {
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PiManifest {
    fn entries(&self, kind: ResourceKind) -> Option<&[String]> {
        match kind {
            ResourceKind::Extension => self.extensions.as_deref(),
            ResourceKind::Skill => self.skills.as_deref(),
            ResourceKind::Prompt => self.prompts.as_deref(),
            ResourceKind::Theme => self.themes.as_deref(),
        }
    }
}

#[derive(Clone, Debug)]
struct ResolvedPackageSetting {
    identity: String,
    source: String,
    scope: Scope,
    root: Option<PathBuf>,
    single_extension: Option<PathBuf>,
    filter: Option<PackageFilter>,
    resolution_diagnostic: Option<Diagnostic>,
}

#[derive(Clone, Debug, Serialize)]
struct PackageReport {
    identity: String,
    source: String,
    scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock_hash: Option<String>,
    migration: MigrationPath,
    resources: Vec<ResourceReport>,
    extensions: Vec<ExtensionReport>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Serialize)]
struct ResourceReport {
    kind: ResourceKind,
    scope: Scope,
    path: PathBuf,
    enabled: bool,
    migration: MigrationPath,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ExtensionSurfaces {
    events: Vec<String>,
    registrations: Vec<String>,
    actions: Vec<String>,
    ui: Vec<String>,
    mutations: Vec<String>,
    imports: Vec<String>,
    unresolved_imports: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct SecuritySignals {
    filesystem: bool,
    process: bool,
    network: bool,
    secrets: bool,
    native_modules: bool,
    dynamic_imports: bool,
}

#[derive(Clone, Debug, Serialize)]
struct ExtensionReport {
    path: PathBuf,
    migration: MigrationPath,
    reasons: Vec<String>,
    analyzed_files: Vec<PathBuf>,
    analyzed_source_bytes: usize,
    syntax_nodes: usize,
    surfaces: ExtensionSurfaces,
    security: SecuritySignals,
    parse_errors: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ResourceCounts {
    packages: usize,
    extensions: usize,
    skills: usize,
    prompts: usize,
    themes: usize,
    disabled: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
struct MigrationCounts {
    direct: usize,
    replace: usize,
    bridge: usize,
    native_port: usize,
    manual: usize,
    blocked: usize,
}

impl MigrationCounts {
    fn add(&mut self, migration: MigrationPath) {
        match migration {
            MigrationPath::Direct => self.direct += 1,
            MigrationPath::Replace => self.replace += 1,
            MigrationPath::Bridge => self.bridge += 1,
            MigrationPath::NativePort => self.native_port += 1,
            MigrationPath::Manual => self.manual += 1,
            MigrationPath::Blocked => self.blocked += 1,
        }
    }

    fn get(&self, migration: MigrationPath) -> usize {
        match migration {
            MigrationPath::Direct => self.direct,
            MigrationPath::Replace => self.replace,
            MigrationPath::Bridge => self.bridge,
            MigrationPath::NativePort => self.native_port,
            MigrationPath::Manual => self.manual,
            MigrationPath::Blocked => self.blocked,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct MigrationReport {
    schema_version: u32,
    source: &'static str,
    mode: &'static str,
    model_usage: &'static str,
    package_code_executed: bool,
    settings: Vec<SettingsReport>,
    found: ResourceCounts,
    migration: MigrationCounts,
    packages: Vec<PackageReport>,
    extensions: Vec<ExtensionReport>,
    resources: Vec<ResourceReport>,
    diagnostics: Vec<Diagnostic>,
}

pub fn run(command: MigrationCommand, invocation_cwd: &Path) -> anyhow::Result<()> {
    match command {
        MigrationCommand::Pi {
            dry_run: _,
            json,
            summary,
            pi_home,
            project,
            npm_roots,
        } => {
            let project =
                absolute_path(project.as_deref().unwrap_or(invocation_cwd), invocation_cwd)?;
            let pi_home = match pi_home {
                Some(path) => absolute_path(&path, invocation_cwd)?,
                None => default_pi_home()?,
            };
            let npm_roots = npm_roots
                .iter()
                .map(|path| absolute_path(path, invocation_cwd))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let report = scan_pi(&ScanOptions {
                pi_home,
                project,
                npm_roots,
            });
            if json {
                crate::output::stdout_multiline(serde_json::to_string_pretty(&report)?);
            } else {
                print_human_report(&report, summary);
            }
            Ok(())
        }
    }
}

fn default_pi_home() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return absolute_path(Path::new(&path), &std::env::current_dir()?);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory is unavailable"))?;
    absolute_path(&home.join(".pi/agent"), &std::env::current_dir()?)
}

fn print_human_report(report: &MigrationReport, summary_only: bool) {
    crate::output::stdout_line("Pi migration dry run");
    crate::output::stdout_line("");
    crate::output::stdout_line("Sources:");
    for settings in &report.settings {
        let state = if settings.found { "found" } else { "not found" };
        crate::output::stdout_line(format!(
            "  {} settings: {} ({state})",
            settings.scope.label(),
            settings.path.display()
        ));
    }
    crate::output::stdout_line("");
    crate::output::stdout_line("Found:");
    crate::output::stdout_line(format!("  {} packages", report.found.packages));
    crate::output::stdout_line(format!("  {} extensions", report.found.extensions));
    crate::output::stdout_line(format!("  {} skills", report.found.skills));
    crate::output::stdout_line(format!("  {} prompts", report.found.prompts));
    crate::output::stdout_line(format!("  {} themes", report.found.themes));
    if report.found.disabled > 0 {
        crate::output::stdout_line(format!("  {} disabled resources", report.found.disabled));
    }
    crate::output::stdout_line("");
    crate::output::stdout_line("Migration:");
    for migration in [
        MigrationPath::Direct,
        MigrationPath::Replace,
        MigrationPath::Bridge,
        MigrationPath::NativePort,
        MigrationPath::Manual,
        MigrationPath::Blocked,
    ] {
        crate::output::stdout_line(format!(
            "  {:<11} {} items",
            migration.label(),
            report.migration.get(migration)
        ));
    }

    if !summary_only && !report.extensions.is_empty() {
        crate::output::stdout_line("");
        crate::output::stdout_line("Local extensions:");
        for extension in &report.extensions {
            crate::output::stdout_line(format!(
                "  {} -> {}",
                extension.path.display(),
                extension.migration.label()
            ));
            for reason in &extension.reasons {
                crate::output::stdout_line(format!("    - {reason}"));
            }
        }
    }

    if !summary_only && !report.packages.is_empty() {
        crate::output::stdout_line("");
        crate::output::stdout_line("Packages:");
        for package in &report.packages {
            let version = package.version.as_deref().unwrap_or("version unknown");
            let disabled = if !package.resources.is_empty()
                && package.resources.iter().all(|resource| !resource.enabled)
            {
                " [all resources disabled]"
            } else {
                ""
            };
            crate::output::stdout_line(format!(
                "  {} ({version}) -> {}{disabled}",
                package.source,
                package.migration.label()
            ));
            for extension in &package.extensions {
                crate::output::stdout_line(format!(
                    "    {} -> {}",
                    extension.path.display(),
                    extension.migration.label()
                ));
                for reason in &extension.reasons {
                    crate::output::stdout_line(format!("      - {reason}"));
                }
            }
        }
    }

    let diagnostic_count = report.diagnostics.len()
        + report
            .packages
            .iter()
            .map(|package| package.diagnostics.len())
            .sum::<usize>();
    if diagnostic_count > 0 {
        crate::output::stdout_line("");
        crate::output::stdout_line(format!("Diagnostics ({diagnostic_count}):"));
        for diagnostic in report.diagnostics.iter().chain(
            report
                .packages
                .iter()
                .flat_map(|package| &package.diagnostics),
        ) {
            let path = diagnostic
                .path
                .as_ref()
                .map(|path| format!(" [{}]", path.display()))
                .unwrap_or_default();
            crate::output::stdout_line(format!(
                "  {:?} {}: {}{path}",
                diagnostic.level, diagnostic.code, diagnostic.message
            ));
        }
    }

    crate::output::stdout_line("");
    crate::output::stdout_line("Estimated model usage: 0 tokens");
    crate::output::stdout_line("No files changed and no Pi package code executed.");
}

fn scan_pi(options: &ScanOptions) -> MigrationReport {
    let global_settings_path = options.pi_home.join("settings.json");
    let project_base = options.project.join(".pi");
    let project_settings_path = project_base.join("settings.json");
    let mut diagnostics = Vec::new();

    let (global_settings, global_found) =
        read_settings(&global_settings_path, Scope::User, &mut diagnostics);
    let (project_settings, project_found) =
        read_settings(&project_settings_path, Scope::Project, &mut diagnostics);

    let mut resolved = BTreeMap::<String, ResolvedPackageSetting>::new();
    resolve_settings_packages(
        &global_settings,
        Scope::User,
        &options.pi_home,
        options,
        &mut resolved,
        &mut diagnostics,
    );
    resolve_settings_packages(
        &project_settings,
        Scope::Project,
        &project_base,
        options,
        &mut resolved,
        &mut diagnostics,
    );

    let mut analysis_budget = AnalysisBudget::default();
    let mut packages = resolved
        .into_values()
        .map(|package| scan_package(package, &mut analysis_budget))
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| left.identity.cmp(&right.identity));

    let mut resources = Vec::new();
    let mut extensions = Vec::new();
    collect_top_level_resources(
        &options.pi_home,
        Scope::User,
        &global_settings,
        &mut resources,
        &mut extensions,
        &mut analysis_budget,
        &mut diagnostics,
    );
    collect_top_level_resources(
        &project_base,
        Scope::Project,
        &project_settings,
        &mut resources,
        &mut extensions,
        &mut analysis_budget,
        &mut diagnostics,
    );
    resources.sort_by(|left, right| (left.kind, &left.path).cmp(&(right.kind, &right.path)));
    resources.dedup_by(|left, right| left.kind == right.kind && left.path == right.path);
    extensions.sort_by(|left, right| left.path.cmp(&right.path));

    let mut found = ResourceCounts {
        packages: packages.len(),
        ..ResourceCounts::default()
    };
    let mut migration = MigrationCounts::default();
    for resource in resources
        .iter()
        .chain(packages.iter().flat_map(|package| &package.resources))
    {
        if !resource.enabled {
            found.disabled += 1;
            continue;
        }
        match resource.kind {
            ResourceKind::Extension => found.extensions += 1,
            ResourceKind::Skill => found.skills += 1,
            ResourceKind::Prompt => found.prompts += 1,
            ResourceKind::Theme => found.themes += 1,
        }
        migration.add(resource.migration);
    }
    for package in &packages {
        if package.resources.is_empty() && package.migration == MigrationPath::Blocked {
            migration.add(MigrationPath::Blocked);
        }
    }
    cap_diagnostics(
        &mut diagnostics,
        MAX_REPORT_DIAGNOSTICS,
        Some(options.project.clone()),
    );

    MigrationReport {
        schema_version: 1,
        source: "pi",
        mode: "dry_run",
        model_usage: "disabled",
        package_code_executed: false,
        settings: vec![
            SettingsReport {
                scope: Scope::User,
                path: global_settings_path,
                found: global_found,
            },
            SettingsReport {
                scope: Scope::Project,
                path: project_settings_path,
                found: project_found,
            },
        ],
        found,
        migration,
        packages,
        extensions,
        resources,
        diagnostics,
    }
}

fn read_settings(
    path: &Path,
    scope: Scope,
    diagnostics: &mut Vec<Diagnostic>,
) -> (PiSettings, bool) {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (PiSettings::default(), false);
        }
        Err(error) => {
            diagnostics.push(Diagnostic::warning(
                "settings_metadata",
                format!("could not inspect {} settings: {error}", scope.label()),
                Some(path.to_path_buf()),
            ));
            return (PiSettings::default(), false);
        }
    };
    if !metadata.file_type().is_file() {
        diagnostics.push(Diagnostic::error(
            "settings_not_regular",
            format!(
                "{} settings must be a regular, non-symlink file",
                scope.label()
            ),
            Some(path.to_path_buf()),
        ));
        return (PiSettings::default(), true);
    }
    let bytes = match ygg_agent::secure_fs::read_regular_file_bounded(path, MAX_SETTINGS_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                "settings_read",
                format!("could not read {} settings: {error}", scope.label()),
                Some(path.to_path_buf()),
            ));
            return (PiSettings::default(), true);
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(settings) => (settings, true),
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                "settings_json",
                format!("invalid {} settings JSON: {error}", scope.label()),
                Some(path.to_path_buf()),
            ));
            (PiSettings::default(), true)
        }
    }
}

fn resolve_settings_packages(
    settings: &PiSettings,
    scope: Scope,
    settings_base: &Path,
    options: &ScanOptions,
    resolved: &mut BTreeMap<String, ResolvedPackageSetting>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for setting in &settings.packages {
        let source = setting.source().trim().to_owned();
        let mut package = resolve_package_setting(
            &source,
            setting.filter().cloned(),
            scope,
            settings_base,
            options,
        );
        if scope == Scope::Project
            && package
                .filter
                .as_ref()
                .is_some_and(|filter| filter.autoload == Some(false))
        {
            if let Some(user_package) = resolved.get(&package.identity) {
                package.root.clone_from(&user_package.root);
                package
                    .single_extension
                    .clone_from(&user_package.single_extension);
                package
                    .resolution_diagnostic
                    .clone_from(&user_package.resolution_diagnostic);
            }
        }
        if !resolved.contains_key(&package.identity) && resolved.len() >= MAX_PACKAGES {
            diagnostics.push(Diagnostic::error(
                "package_limit",
                format!("Pi setup exceeds the {MAX_PACKAGES}-package scan limit"),
                Some(settings_base.join("settings.json")),
            ));
            break;
        }
        // Pi gives a project package precedence over the same user package.
        // Processing the project settings second mirrors that deterministic rule.
        resolved.insert(package.identity.clone(), package);
    }
}

fn resolve_package_setting(
    source: &str,
    filter: Option<PackageFilter>,
    scope: Scope,
    settings_base: &Path,
    options: &ScanOptions,
) -> ResolvedPackageSetting {
    if let Some(spec) = source.strip_prefix("npm:") {
        return resolve_npm_package(source, spec, filter, scope, options);
    }
    if let Some((host, repository)) = parse_git_source(source) {
        let root_base = match scope {
            Scope::User => options.pi_home.join("git"),
            Scope::Project => options.project.join(".pi/git"),
        };
        let root = root_base.join(&host).join(&repository);
        return resolved_root(
            format!("git:{host}/{}", repository.display()),
            source,
            filter,
            scope,
            root,
        );
    }

    let path = match expand_local_path(source, settings_base) {
        Ok(path) => path,
        Err(error) => {
            return ResolvedPackageSetting {
                identity: format!("local:{source}"),
                source: source.to_owned(),
                scope,
                root: None,
                single_extension: None,
                filter,
                resolution_diagnostic: Some(Diagnostic::error(
                    "package_path",
                    error.to_string(),
                    None,
                )),
            };
        }
    };
    let metadata = std::fs::symlink_metadata(&path);
    match metadata {
        Ok(metadata) if metadata.file_type().is_file() => {
            let canonical = match std::fs::canonicalize(&path)
                .ok()
                .and_then(|path| normalize_absolute(&path).ok())
            {
                Some(path) => path,
                None => {
                    return unresolved_local_package(
                        source,
                        filter,
                        scope,
                        path,
                        "local extension path could not be canonicalized",
                    );
                }
            };
            ResolvedPackageSetting {
                identity: format!("local:{}", canonical.display()),
                source: source.to_owned(),
                scope,
                root: canonical.parent().map(Path::to_path_buf),
                single_extension: Some(canonical),
                filter,
                resolution_diagnostic: None,
            }
        }
        Ok(metadata) if metadata.file_type().is_dir() => {
            let canonical = match std::fs::canonicalize(&path)
                .ok()
                .and_then(|path| normalize_absolute(&path).ok())
            {
                Some(path) => path,
                None => {
                    return unresolved_local_package(
                        source,
                        filter,
                        scope,
                        path,
                        "local package directory could not be canonicalized",
                    );
                }
            };
            resolved_root(
                format!("local:{}", canonical.display()),
                source,
                filter,
                scope,
                canonical,
            )
        }
        _ => resolved_root(
            format!("local:{}", path.display()),
            source,
            filter,
            scope,
            path,
        ),
    }
}

fn unresolved_local_package(
    source: &str,
    filter: Option<PackageFilter>,
    scope: Scope,
    path: PathBuf,
    message: &'static str,
) -> ResolvedPackageSetting {
    ResolvedPackageSetting {
        identity: format!("local:{}", path.display()),
        source: source.to_owned(),
        scope,
        root: None,
        single_extension: None,
        filter,
        resolution_diagnostic: Some(Diagnostic::error("package_unresolved", message, Some(path))),
    }
}

fn resolve_npm_package(
    source: &str,
    spec: &str,
    filter: Option<PackageFilter>,
    scope: Scope,
    options: &ScanOptions,
) -> ResolvedPackageSetting {
    let Some(name) = npm_package_name(spec) else {
        return ResolvedPackageSetting {
            identity: format!("npm:{spec}"),
            source: source.to_owned(),
            scope,
            root: None,
            single_extension: None,
            filter,
            resolution_diagnostic: Some(Diagnostic::error(
                "npm_spec",
                format!("invalid npm package source {source:?}"),
                None,
            )),
        };
    };
    let mut roots = Vec::new();
    match scope {
        Scope::User => roots.push(options.pi_home.join("npm/node_modules")),
        Scope::Project => roots.push(options.project.join(".pi/npm/node_modules")),
    }
    if scope == Scope::User {
        roots.extend(options.npm_roots.iter().cloned());
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".local/lib/node_modules"));
        }
        roots.push(PathBuf::from("/usr/local/lib/node_modules"));
        roots.push(PathBuf::from("/opt/homebrew/lib/node_modules"));
    }
    let mut seen_roots = BTreeSet::new();
    roots.retain(|root| seen_roots.insert(root.clone()));
    let candidates = roots
        .iter()
        .map(|root| root.join(&name))
        .collect::<Vec<_>>();
    let selected = candidates
        .iter()
        .find(|path| std::fs::symlink_metadata(path).is_ok())
        .cloned()
        .or_else(|| candidates.first().cloned());
    let Some(root) = selected else {
        return ResolvedPackageSetting {
            identity: format!("npm:{name}"),
            source: source.to_owned(),
            scope,
            root: None,
            single_extension: None,
            filter,
            resolution_diagnostic: Some(Diagnostic::error(
                "npm_root",
                "no npm installation root is available",
                None,
            )),
        };
    };
    resolved_root(format!("npm:{name}"), source, filter, scope, root)
}

fn resolved_root(
    identity: String,
    source: &str,
    filter: Option<PackageFilter>,
    scope: Scope,
    root: PathBuf,
) -> ResolvedPackageSetting {
    let resolution_diagnostic = match validate_directory_root(&root) {
        Ok(()) => None,
        Err(error) => Some(Diagnostic::error(
            "package_unresolved",
            error,
            Some(root.clone()),
        )),
    };
    ResolvedPackageSetting {
        identity,
        source: source.to_owned(),
        scope,
        root: resolution_diagnostic.is_none().then_some(root),
        single_extension: None,
        filter,
        resolution_diagnostic,
    }
}

fn scan_package(
    package: ResolvedPackageSetting,
    analysis_budget: &mut AnalysisBudget,
) -> PackageReport {
    let mut diagnostics = Vec::new();
    if let Some(diagnostic) = package.resolution_diagnostic {
        diagnostics.push(diagnostic);
    }
    let Some(root) = package.root.as_ref() else {
        return PackageReport {
            identity: package.identity,
            source: package.source,
            scope: package.scope,
            root: None,
            name: None,
            version: None,
            source_hash: None,
            lock_hash: None,
            migration: MigrationPath::Blocked,
            resources: Vec::new(),
            extensions: Vec::new(),
            diagnostics,
        };
    };

    let package_json_path = root.join("package.json");
    let single_extension = package.single_extension.is_some();
    let package_json = if single_extension {
        None
    } else {
        read_package_json(&package_json_path, &mut diagnostics)
    };
    let mut resources = if let Some(extension) = package.single_extension {
        vec![ResourceReport {
            kind: ResourceKind::Extension,
            scope: package.scope,
            path: extension,
            enabled: true,
            migration: MigrationPath::Bridge,
        }]
    } else {
        collect_package_resources(
            root,
            package_json.as_ref().and_then(|json| json.pi.as_ref()),
            package.filter.as_ref(),
            package.scope,
            &mut diagnostics,
        )
    };
    let resource_remaining = MAX_SCAN_RESOURCES.saturating_sub(analysis_budget.resources);
    if resources.len() > resource_remaining {
        diagnostics.push(Diagnostic::warning(
            "scan_resource_limit",
            format!("setup resource inventory stopped at {MAX_SCAN_RESOURCES} entries"),
            Some(root.clone()),
        ));
        resources.truncate(resource_remaining);
    }
    analysis_budget.resources = analysis_budget.resources.saturating_add(resources.len());

    let mut extensions = Vec::new();
    let mut analyzed_files = BTreeSet::new();
    for resource in &mut resources {
        if resource.kind != ResourceKind::Extension || !resource.enabled {
            continue;
        }
        let report =
            analyze_extension_with_budget(&resource.path, root, analysis_budget, &mut diagnostics);
        resource.migration = report.migration;
        analyzed_files.extend(report.analyzed_files.iter().cloned());
        extensions.push(report);
    }
    resources.sort_by(|left, right| (left.kind, &left.path).cmp(&(right.kind, &right.path)));

    let mut source_files = resources
        .iter()
        .map(|resource| resource.path.clone())
        .collect::<BTreeSet<_>>();
    source_files.extend(analyzed_files);
    if !single_extension {
        source_files.extend(
            walk_regular_files(root, &mut diagnostics)
                .into_iter()
                .filter(|path| is_package_source_file(path)),
        );
    }
    if !single_extension && std::fs::symlink_metadata(&package_json_path).is_ok() {
        source_files.insert(package_json_path);
    }
    let source_hash = hash_files(
        root,
        source_files.iter().map(PathBuf::as_path),
        MAX_PACKAGE_SOURCE_BYTES,
        "package source",
        analysis_budget,
        &mut diagnostics,
    );
    let lock_names = [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "yarn.lock",
    ];
    let mut lock_root: &Path = root;
    let mut lock_files = if single_extension {
        Vec::new()
    } else {
        lock_names
            .iter()
            .map(|name| root.join(name))
            .filter(|path| std::fs::symlink_metadata(path).is_ok())
            .collect::<Vec<_>>()
    };
    if lock_files.is_empty() && package.identity.starts_with("npm:") {
        if let Some(install_root) = root
            .ancestors()
            .find(|ancestor| ancestor.file_name() == Some(OsStr::new("node_modules")))
            .and_then(Path::parent)
        {
            lock_root = install_root;
            lock_files = lock_names
                .iter()
                .map(|name| install_root.join(name))
                .filter(|path| std::fs::symlink_metadata(path).is_ok())
                .collect();
        }
    }
    let lock_hash = if lock_files.is_empty() {
        None
    } else {
        hash_files(
            lock_root,
            lock_files.iter().map(PathBuf::as_path),
            MAX_LOCK_FILE_BYTES,
            "package lock",
            analysis_budget,
            &mut diagnostics,
        )
    };

    let migration = resources
        .iter()
        .filter(|resource| resource.enabled)
        .map(|resource| resource.migration)
        .max()
        .or_else(|| resources.iter().map(|resource| resource.migration).max())
        .unwrap_or(MigrationPath::Blocked);
    cap_diagnostics(
        &mut diagnostics,
        MAX_PACKAGE_DIAGNOSTICS,
        Some(root.clone()),
    );
    PackageReport {
        identity: package.identity,
        source: package.source,
        scope: package.scope,
        root: Some(root.clone()),
        name: package_json.as_ref().and_then(|json| json.name.clone()),
        version: package_json.as_ref().and_then(|json| json.version.clone()),
        source_hash,
        lock_hash,
        migration,
        resources,
        extensions,
        diagnostics,
    }
}

fn read_package_json(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Option<PackageJson> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            diagnostics.push(Diagnostic::warning(
                "package_json_metadata",
                format!("could not inspect package.json: {error}"),
                Some(path.to_path_buf()),
            ));
            return None;
        }
    };
    if !metadata.file_type().is_file() {
        diagnostics.push(Diagnostic::error(
            "package_json_not_regular",
            "package.json must be a regular, non-symlink file",
            Some(path.to_path_buf()),
        ));
        return None;
    }
    let bytes = match ygg_agent::secure_fs::read_regular_file_bounded(path, MAX_PACKAGE_JSON_BYTES)
    {
        Ok(bytes) => bytes,
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                "package_json_read",
                format!("could not read package.json: {error}"),
                Some(path.to_path_buf()),
            ));
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(package) => Some(package),
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                "package_json_invalid",
                format!("invalid package.json: {error}"),
                Some(path.to_path_buf()),
            ));
            None
        }
    }
}

fn collect_package_resources(
    root: &Path,
    manifest: Option<&PiManifest>,
    filter: Option<&PackageFilter>,
    scope: Scope,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<ResourceReport> {
    let mut reports = Vec::new();
    let mut traversal = ResourceTraversal::default();
    for kind in ResourceKind::ALL {
        let mut paths = match manifest {
            Some(manifest) => match manifest.entries(kind) {
                Some(entries) => {
                    collect_manifest_entries(root, entries, kind, &mut traversal, diagnostics)
                }
                None if filter.is_some() => collect_resource_path(
                    &root.join(kind.key()),
                    kind,
                    root,
                    &mut traversal,
                    diagnostics,
                ),
                None => Vec::new(),
            },
            None => collect_resource_path(
                &root.join(kind.key()),
                kind,
                root,
                &mut traversal,
                diagnostics,
            ),
        };
        paths.sort();
        paths.dedup();
        let enabled = apply_filter(
            root,
            &paths,
            filter.and_then(|filter| filter.patterns(kind)),
            filter.and_then(|filter| filter.autoload).unwrap_or(true),
            diagnostics,
        );
        reports.extend(paths.into_iter().map(|path| {
            let enabled = enabled.contains(&path);
            ResourceReport {
                kind,
                scope,
                path,
                enabled,
                migration: default_migration(kind),
            }
        }));
    }
    reports
}

fn collect_manifest_entries(
    root: &Path,
    entries: &[String],
    kind: ResourceKind,
    traversal: &mut ResourceTraversal,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<PathBuf> {
    let tracks_extension_manifest = kind == ResourceKind::Extension;
    if tracks_extension_manifest {
        if traversal.extension_manifest_resolution_stopped {
            return Vec::new();
        }
        if traversal.active_extension_manifests.len() >= MAX_EXTENSION_MANIFEST_DEPTH {
            traversal.extension_manifest_resolution_stopped = true;
            diagnostics.push(Diagnostic::warning(
                "manifest_depth_limit",
                format!(
                    "extension manifest resolution stopped at {MAX_EXTENSION_MANIFEST_DEPTH} nested directories"
                ),
                Some(root.to_path_buf()),
            ));
            return Vec::new();
        }
        if traversal.extension_manifest_is_active(root) {
            traversal.extension_manifest_resolution_stopped = true;
            diagnostics.push(Diagnostic::warning(
                "manifest_cycle",
                "extension manifest resolution revisited an active directory",
                Some(root.to_path_buf()),
            ));
            return Vec::new();
        }
        traversal
            .active_extension_manifests
            .push(root.to_path_buf());
    }

    let mut paths = Vec::new();
    if entries.len() > MAX_PATTERNS {
        diagnostics.push(Diagnostic::warning(
            "manifest_entry_limit",
            format!("only the first {MAX_PATTERNS} manifest entries were inspected"),
            Some(root.to_path_buf()),
        ));
    }
    for entry in entries
        .iter()
        .take(MAX_PATTERNS)
        .filter(|entry| !entry.starts_with(['!', '+', '-']))
    {
        if has_glob(entry) {
            let matcher = match compile_glob(entry) {
                Ok(matcher) => matcher,
                Err(error) => {
                    diagnostics.push(Diagnostic::warning(
                        "manifest_glob",
                        format!("invalid {} pattern {entry:?}: {error}", kind.key()),
                        Some(root.to_path_buf()),
                    ));
                    continue;
                }
            };
            for candidate in walk_regular_files(root, diagnostics) {
                let relative = relative_slash(root, &candidate);
                if matcher.is_match(&relative) {
                    paths.extend(collect_resource_path(
                        &candidate,
                        kind,
                        root,
                        traversal,
                        diagnostics,
                    ));
                }
            }
        } else {
            match confined_join(root, entry) {
                Ok(path) => paths.extend(collect_resource_path(
                    &path,
                    kind,
                    root,
                    traversal,
                    diagnostics,
                )),
                Err(error) => diagnostics.push(Diagnostic::warning(
                    "manifest_path",
                    error.to_string(),
                    Some(root.to_path_buf()),
                )),
            }
        }
        if paths.len() >= MAX_RESOURCE_FILES {
            diagnostics.push(Diagnostic::error(
                "resource_limit",
                format!("package exceeds the {MAX_RESOURCE_FILES}-resource scan limit"),
                Some(root.to_path_buf()),
            ));
            paths.truncate(MAX_RESOURCE_FILES);
            break;
        }
    }
    let paths = apply_manifest_overrides(root, paths, entries, diagnostics);
    if tracks_extension_manifest {
        let popped = traversal.active_extension_manifests.pop();
        debug_assert_eq!(popped.as_deref(), Some(root));
    }
    paths
}

fn apply_manifest_overrides(
    root: &Path,
    mut paths: Vec<PathBuf>,
    entries: &[String],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<PathBuf> {
    let patterns = entries
        .iter()
        .take(MAX_PATTERNS)
        .filter(|entry| entry.starts_with(['!', '+', '-']))
        .cloned()
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return paths;
    }
    let original = paths.clone();
    let enabled = apply_patterns(root, &original, &patterns, true, diagnostics);
    paths.retain(|path| enabled.contains(path));
    paths
}

fn apply_filter(
    root: &Path,
    paths: &[PathBuf],
    patterns: Option<&[String]>,
    autoload: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeSet<PathBuf> {
    match patterns {
        None if autoload => paths.iter().cloned().collect(),
        None => BTreeSet::new(),
        Some(patterns) => apply_patterns(root, paths, patterns, autoload, diagnostics),
    }
}

fn apply_patterns(
    root: &Path,
    paths: &[PathBuf],
    patterns: &[String],
    default_include: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeSet<PathBuf> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();
    if patterns.len() > MAX_PATTERNS {
        diagnostics.push(Diagnostic::warning(
            "pattern_limit",
            format!("only the first {MAX_PATTERNS} resource patterns were inspected"),
            Some(root.to_path_buf()),
        ));
    }
    for pattern in patterns.iter().take(MAX_PATTERNS) {
        let (target, value) = if let Some(value) = pattern.strip_prefix('+') {
            (&mut force_includes, value)
        } else if let Some(value) = pattern.strip_prefix('-') {
            (&mut force_excludes, value)
        } else if let Some(value) = pattern.strip_prefix('!') {
            (&mut excludes, value)
        } else {
            (&mut includes, pattern.as_str())
        };
        match compile_glob(value) {
            Ok(matcher) => target.push((value.to_owned(), matcher)),
            Err(error) => diagnostics.push(Diagnostic::warning(
                "filter_glob",
                format!("invalid package filter {pattern:?}: {error}"),
                Some(root.to_path_buf()),
            )),
        }
    }
    let include_by_default = includes.is_empty() && default_include;
    let mut selected = BTreeSet::new();
    for path in paths {
        let relative = relative_slash(root, path);
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        let skill_parent = (name == "SKILL.md")
            .then(|| path.parent().map(|parent| relative_slash(root, parent)))
            .flatten();
        let matches = |patterns: &[(String, GlobMatcher)]| {
            patterns.iter().any(|(raw, matcher)| {
                matcher.is_match(&relative)
                    || matcher.is_match(name)
                    || skill_parent
                        .as_deref()
                        .is_some_and(|parent| matcher.is_match(parent))
                    || (!has_glob(raw) && raw.trim_start_matches("./") == relative)
            })
        };
        let mut enabled = include_by_default || matches(&includes);
        if matches(&excludes) {
            enabled = false;
        }
        if matches(&force_includes) {
            enabled = true;
        }
        if matches(&force_excludes) {
            enabled = false;
        }
        if enabled {
            selected.insert(path.clone());
        }
    }
    selected
}

fn collect_top_level_resources(
    base: &Path,
    scope: Scope,
    settings: &PiSettings,
    reports: &mut Vec<ResourceReport>,
    extensions: &mut Vec<ExtensionReport>,
    analysis_budget: &mut AnalysisBudget,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut traversal = ResourceTraversal::default();
    for kind in ResourceKind::ALL {
        let mut paths = collect_resource_path(
            &base.join(kind.key()),
            kind,
            base,
            &mut traversal,
            diagnostics,
        );
        let resource_remaining = MAX_SCAN_RESOURCES.saturating_sub(analysis_budget.resources);
        if paths.len() > resource_remaining {
            diagnostics.push(Diagnostic::warning(
                "scan_resource_limit",
                format!("setup resource inventory stopped at {MAX_SCAN_RESOURCES} entries"),
                Some(base.to_path_buf()),
            ));
            paths.truncate(resource_remaining);
        }
        analysis_budget.resources = analysis_budget.resources.saturating_add(paths.len());
        if settings.overrides(kind).len() > MAX_PATTERNS {
            diagnostics.push(Diagnostic::warning(
                "pattern_limit",
                format!("only the first {MAX_PATTERNS} top-level overrides were inspected"),
                Some(base.join("settings.json")),
            ));
        }
        let overrides = settings
            .overrides(kind)
            .iter()
            .take(MAX_PATTERNS)
            .filter(|pattern| pattern.starts_with(['!', '+', '-']))
            .cloned()
            .collect::<Vec<_>>();
        let enabled = apply_patterns(base, &paths, &overrides, true, diagnostics);
        for path in paths {
            let enabled = enabled.contains(&path);
            let mut migration = default_migration(kind);
            if kind == ResourceKind::Extension && enabled {
                let extension =
                    analyze_extension_with_budget(&path, base, analysis_budget, diagnostics);
                migration = extension.migration;
                extensions.push(extension);
            }
            reports.push(ResourceReport {
                kind,
                scope,
                enabled,
                path,
                migration,
            });
        }
    }
}

fn collect_resource_path(
    path: &Path,
    kind: ResourceKind,
    package_root: &Path,
    traversal: &mut ResourceTraversal,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<PathBuf> {
    if kind == ResourceKind::Extension && traversal.extension_manifest_resolution_stopped {
        return Vec::new();
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            diagnostics.push(Diagnostic::warning(
                "resource_metadata",
                format!("could not inspect {} resource: {error}", kind.key()),
                Some(path.to_path_buf()),
            ));
            return Vec::new();
        }
    };
    if metadata.file_type().is_symlink() {
        diagnostics.push(Diagnostic::warning(
            "resource_symlink",
            "symlinked resources are not read by the migration scanner",
            Some(path.to_path_buf()),
        ));
        return Vec::new();
    }
    if metadata.is_file() {
        return is_resource_file(path, kind)
            .then(|| path.to_path_buf())
            .into_iter()
            .collect();
    }
    if !metadata.is_dir() {
        diagnostics.push(Diagnostic::warning(
            "resource_not_regular",
            "resource entry is neither a regular file nor directory",
            Some(path.to_path_buf()),
        ));
        return Vec::new();
    }

    match kind {
        ResourceKind::Extension => collect_extension_directory(path, traversal, diagnostics),
        ResourceKind::Skill => collect_skill_directory(path, diagnostics),
        ResourceKind::Prompt | ResourceKind::Theme => walk_regular_files(path, diagnostics)
            .into_iter()
            .filter(|candidate| is_resource_file(candidate, kind))
            .take(MAX_RESOURCE_FILES)
            .collect(),
    }
    .into_iter()
    .filter(|candidate| candidate.starts_with(package_root))
    .collect()
}

fn collect_extension_directory(
    path: &Path,
    traversal: &mut ResourceTraversal,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<PathBuf> {
    if !traversal.extension_manifest_is_active(path) {
        if let Some(entries) = extension_directory_entrypoints(path, traversal, diagnostics) {
            return entries;
        }
    }
    if let Some(entrypoint) = extension_index_entrypoint(path) {
        return vec![entrypoint];
    }
    let mut entries = Vec::new();
    let read_dir = match std::fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(error) => {
            diagnostics.push(Diagnostic::warning(
                "extension_directory",
                format!("could not read extension directory: {error}"),
                Some(path.to_path_buf()),
            ));
            return entries;
        }
    };
    for (visited, entry) in read_dir.flatten().enumerate() {
        if visited >= MAX_WALK_ENTRIES {
            diagnostics.push(Diagnostic::warning(
                "walk_entry_limit",
                format!("extension directory stopped at {MAX_WALK_ENTRIES} entries"),
                Some(path.to_path_buf()),
            ));
            break;
        }
        if entries.len() >= MAX_RESOURCE_FILES {
            diagnostics.push(Diagnostic::warning(
                "resource_limit",
                format!("extension discovery stopped at {MAX_RESOURCE_FILES} files"),
                Some(path.to_path_buf()),
            ));
            break;
        }
        let candidate = entry.path();
        if candidate.to_str().is_none() {
            diagnostics.push(Diagnostic::warning(
                "resource_path_utf8",
                "resource paths must be valid UTF-8",
                Some(path.to_path_buf()),
            ));
            continue;
        }
        let metadata = match entry.file_type() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_symlink() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if metadata.is_file() && is_resource_file(&candidate, ResourceKind::Extension) {
            entries.push(candidate);
        } else if metadata.is_dir() && entry.file_name() != "node_modules" {
            if let Some(mut nested) =
                extension_directory_entrypoints(&candidate, traversal, diagnostics)
            {
                entries.append(&mut nested);
            }
        }
    }
    entries
}

fn extension_directory_entrypoints(
    path: &Path,
    traversal: &mut ResourceTraversal,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<PathBuf>> {
    let package_json = read_package_json(&path.join("package.json"), diagnostics);
    if let Some(entries) = package_json
        .as_ref()
        .and_then(|package| package.pi.as_ref())
        .and_then(|manifest| manifest.extensions.as_deref())
    {
        let entries = collect_manifest_entries(
            path,
            entries,
            ResourceKind::Extension,
            traversal,
            diagnostics,
        );
        if traversal.extension_manifest_resolution_stopped || !entries.is_empty() {
            return Some(entries);
        }
    }
    extension_index_entrypoint(path).map(|entrypoint| vec![entrypoint])
}

fn extension_index_entrypoint(path: &Path) -> Option<PathBuf> {
    [
        "index.ts",
        "index.tsx",
        "index.js",
        "index.mjs",
        "index.cjs",
    ]
    .into_iter()
    .map(|name| path.join(name))
    .find(|candidate| {
        std::fs::symlink_metadata(candidate).is_ok_and(|metadata| metadata.file_type().is_file())
    })
}

fn collect_skill_directory(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<PathBuf> {
    walk_regular_files(path, diagnostics)
        .into_iter()
        .filter(|candidate| {
            candidate.file_name() == Some(OsStr::new("SKILL.md"))
                || (candidate.parent() == Some(path)
                    && candidate.extension() == Some(OsStr::new("md")))
        })
        .take(MAX_RESOURCE_FILES)
        .collect()
}

fn walk_regular_files(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<PathBuf> {
    if let Err(error) = validate_directory_root(root) {
        diagnostics.push(Diagnostic::warning(
            "resource_root",
            error,
            Some(root.to_path_buf()),
        ));
        return Vec::new();
    }
    let mut files = Vec::new();
    let mut visited = 0usize;
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .filter_entry(|entry| entry.file_name() != "node_modules" && entry.file_name() != ".git")
        .build();
    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(Diagnostic::warning(
                    "resource_walk",
                    format!("could not inspect resource entry: {error}"),
                    Some(root.to_path_buf()),
                ));
                continue;
            }
        };
        visited += 1;
        if visited > MAX_WALK_ENTRIES {
            diagnostics.push(Diagnostic::warning(
                "walk_entry_limit",
                format!("resource walk stopped at {MAX_WALK_ENTRIES} entries"),
                Some(root.to_path_buf()),
            ));
            break;
        }
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            diagnostics.push(Diagnostic::warning(
                "resource_symlink",
                "symlinked resources are not read by the migration scanner",
                Some(entry.path().to_path_buf()),
            ));
            continue;
        }
        if file_type.is_file() {
            if entry.path().to_str().is_none() {
                diagnostics.push(Diagnostic::warning(
                    "resource_path_utf8",
                    "resource paths must be valid UTF-8",
                    Some(root.to_path_buf()),
                ));
                continue;
            }
            files.push(entry.into_path());
            if files.len() >= MAX_RESOURCE_FILES {
                diagnostics.push(Diagnostic::warning(
                    "resource_limit",
                    format!("resource walk stopped at {MAX_RESOURCE_FILES} files"),
                    Some(root.to_path_buf()),
                ));
                break;
            }
        }
    }
    files
}

fn is_package_source_file(path: &Path) -> bool {
    if path.file_name().is_some_and(|name| {
        matches!(
            name.to_str(),
            Some("package-lock.json" | "npm-shrinkwrap.json" | "pnpm-lock.yaml" | "yarn.lock")
        )
    }) {
        return false;
    }
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some(
            "ts" | "tsx"
                | "mts"
                | "cts"
                | "js"
                | "mjs"
                | "cjs"
                | "json"
                | "md"
                | "toml"
                | "yaml"
                | "yml"
        )
    )
}

fn is_resource_file(path: &Path, kind: ResourceKind) -> bool {
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();
    match kind {
        ResourceKind::Extension => matches!(extension, "ts" | "tsx" | "js" | "mjs" | "cjs"),
        ResourceKind::Skill => {
            path.file_name() == Some(OsStr::new("SKILL.md")) || extension == "md"
        }
        ResourceKind::Prompt => extension == "md",
        ResourceKind::Theme => extension == "json",
    }
}

fn default_migration(kind: ResourceKind) -> MigrationPath {
    match kind {
        ResourceKind::Extension => MigrationPath::Bridge,
        ResourceKind::Skill | ResourceKind::Prompt => MigrationPath::Direct,
        ResourceKind::Theme => MigrationPath::Manual,
    }
}

#[derive(Default)]
struct AnalysisAccumulator {
    events: BTreeSet<String>,
    registrations: BTreeSet<String>,
    actions: BTreeSet<String>,
    ui: BTreeSet<String>,
    mutations: BTreeSet<String>,
    imports: BTreeSet<String>,
    unresolved_imports: BTreeSet<String>,
    security: SecuritySignals,
    parse_errors: usize,
    ast_nodes: usize,
}

#[cfg(test)]
fn analyze_extension(
    entrypoint: &Path,
    package_root: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> ExtensionReport {
    analyze_extension_with_budget(
        entrypoint,
        package_root,
        &mut AnalysisBudget::default(),
        diagnostics,
    )
}

fn analyze_extension_with_budget(
    entrypoint: &Path,
    package_root: &Path,
    analysis_budget: &mut AnalysisBudget,
    diagnostics: &mut Vec<Diagnostic>,
) -> ExtensionReport {
    let mut accumulator = AnalysisAccumulator::default();
    let mut queue = VecDeque::from([entrypoint.to_path_buf()]);
    let mut seen = BTreeSet::new();
    let mut source_bytes = 0usize;
    while let Some(path) = queue.pop_front() {
        if seen.contains(&path) {
            continue;
        }
        if seen.len() >= MAX_ANALYZED_FILES {
            diagnostics.push(Diagnostic::warning(
                "ast_file_limit",
                format!("extension analysis stopped at {MAX_ANALYZED_FILES} files"),
                Some(entrypoint.to_path_buf()),
            ));
            accumulator.parse_errors += 1;
            break;
        }
        if analysis_budget.files >= MAX_SCAN_ANALYZED_FILES {
            diagnostics.push(Diagnostic::warning(
                "scan_file_limit",
                format!("setup analysis stopped at {MAX_SCAN_ANALYZED_FILES} source files"),
                Some(entrypoint.to_path_buf()),
            ));
            accumulator.parse_errors += 1;
            break;
        }
        seen.insert(path.clone());
        analysis_budget.files += 1;
        let extension_remaining = MAX_EXTENSION_SOURCE_BYTES.saturating_sub(source_bytes);
        let scan_remaining = MAX_SCAN_SOURCE_BYTES.saturating_sub(analysis_budget.source_bytes);
        let remaining = extension_remaining.min(scan_remaining);
        if remaining == 0 {
            diagnostics.push(Diagnostic::warning(
                "ast_byte_limit",
                "extension or setup analysis exhausted its bounded source-byte budget",
                Some(entrypoint.to_path_buf()),
            ));
            accumulator.parse_errors += 1;
            break;
        }
        let bytes = match ygg_agent::secure_fs::read_regular_file_bounded(
            &path,
            MAX_SOURCE_FILE_BYTES.min(remaining),
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                diagnostics.push(Diagnostic::warning(
                    "source_read",
                    format!("could not read extension source: {error}"),
                    Some(path.clone()),
                ));
                accumulator.parse_errors += 1;
                continue;
            }
        };
        source_bytes = source_bytes.saturating_add(bytes.len());
        analysis_budget.source_bytes = analysis_budget.source_bytes.saturating_add(bytes.len());
        let source = match std::str::from_utf8(&bytes) {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(Diagnostic::warning(
                    "source_utf8",
                    format!("extension source is not UTF-8: {error}"),
                    Some(path.clone()),
                ));
                accumulator.parse_errors += 1;
                continue;
            }
        };
        let imports = analyze_source_ast(
            source,
            &path,
            &mut accumulator,
            analysis_budget,
            diagnostics,
        );
        for import in imports
            .into_iter()
            .filter(|import| import.starts_with('.') || import.starts_with('#'))
        {
            match resolve_local_import(&path, package_root, &import) {
                Some(import_path) if !seen.contains(&import_path) => queue.push_back(import_path),
                Some(_) => {}
                None if is_code_import(&import) => {
                    accumulator.unresolved_imports.insert(import);
                }
                None => {}
            }
        }
    }

    let (migration, reasons) = classify_extension(&accumulator);
    ExtensionReport {
        path: entrypoint.to_path_buf(),
        migration,
        reasons,
        analyzed_files: seen.into_iter().collect(),
        analyzed_source_bytes: source_bytes,
        syntax_nodes: accumulator.ast_nodes,
        surfaces: ExtensionSurfaces {
            events: accumulator.events.into_iter().collect(),
            registrations: accumulator.registrations.into_iter().collect(),
            actions: accumulator.actions.into_iter().collect(),
            ui: accumulator.ui.into_iter().collect(),
            mutations: accumulator.mutations.into_iter().collect(),
            imports: accumulator.imports.into_iter().collect(),
            unresolved_imports: accumulator.unresolved_imports.into_iter().collect(),
        },
        security: accumulator.security,
        parse_errors: accumulator.parse_errors,
    }
}

fn first_factory_api_binding(function: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(
        function.kind(),
        "arrow_function" | "function_expression" | "function_declaration"
    ) {
        return None;
    }
    let parameters = function.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let parameter = parameters.named_children(&mut cursor).next()?;
    let pattern = parameter
        .child_by_field_name("pattern")
        .unwrap_or(parameter);
    (pattern.kind() == "identifier")
        .then(|| node_text(pattern, source).map(str::to_owned))
        .flatten()
}

fn extension_api_bindings(root: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut bindings = BTreeSet::new();
    let mut exported_names = BTreeSet::new();
    let mut cursor = root.walk();
    let top_level = root.named_children(&mut cursor).collect::<Vec<_>>();

    for statement in &top_level {
        if statement.kind() == "export_statement" {
            if let Some(value) = statement.child_by_field_name("value") {
                if let Some(binding) = first_factory_api_binding(value, source) {
                    bindings.insert(binding);
                } else if value.kind() == "identifier" {
                    if let Some(name) = node_text(value, source) {
                        exported_names.insert(name.to_owned());
                    }
                }
            }
        }
        let expression = if statement.kind() == "expression_statement" {
            statement.named_child(0)
        } else {
            Some(*statement)
        };
        let Some(assignment) = expression.filter(|node| node.kind() == "assignment_expression")
        else {
            continue;
        };
        let Some(left) = assignment.child_by_field_name("left") else {
            continue;
        };
        let Some(chain) = member_chain(left, source) else {
            continue;
        };
        if matches!(chain.as_str(), "module.exports" | "exports.default") {
            if let Some(value) = assignment.child_by_field_name("right") {
                if let Some(binding) = first_factory_api_binding(value, source) {
                    bindings.insert(binding);
                }
            }
        }
    }

    if !exported_names.is_empty() {
        for statement in top_level {
            if statement.kind() == "function_declaration" {
                let exported = statement
                    .child_by_field_name("name")
                    .and_then(|name| node_text(name, source))
                    .is_some_and(|name| exported_names.contains(name));
                if exported {
                    if let Some(binding) = first_factory_api_binding(statement, source) {
                        bindings.insert(binding);
                    }
                }
                continue;
            }
            if !matches!(
                statement.kind(),
                "lexical_declaration" | "variable_declaration"
            ) {
                continue;
            }
            let mut cursor = statement.walk();
            for declarator in statement.named_children(&mut cursor) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                let Some(name) = declarator
                    .child_by_field_name("name")
                    .and_then(|name| node_text(name, source))
                else {
                    continue;
                };
                if !exported_names.contains(name) {
                    continue;
                }
                if let Some(value) = declarator.child_by_field_name("value") {
                    if let Some(binding) = first_factory_api_binding(value, source) {
                        bindings.insert(binding);
                    }
                }
            }
        }
    }
    bindings
}

fn is_extension_api_direct_method(
    chain: &str,
    method: &str,
    api_bindings: &BTreeSet<String>,
) -> bool {
    let Some((binding, suffix)) = chain.split_once('.') else {
        return false;
    };
    suffix == method && api_bindings.contains(binding)
}

fn extension_api_event_bus_method<'a>(
    chain: &'a str,
    api_bindings: &BTreeSet<String>,
) -> Option<&'a str> {
    let mut parts = chain.split('.');
    let binding = parts.next()?;
    if parts.next()? != "events" || !api_bindings.contains(binding) {
        return None;
    }
    let method = parts.next()?;
    parts.next().is_none().then_some(method)
}

fn analyze_source_ast(
    source: &str,
    path: &Path,
    accumulator: &mut AnalysisAccumulator,
    analysis_budget: &mut AnalysisBudget,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<String> {
    let mut parser = Parser::new();
    if let Err(error) = parser.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into()) {
        diagnostics.push(Diagnostic::error(
            "ast_language",
            format!("could not initialize the TypeScript parser: {error}"),
            Some(path.to_path_buf()),
        ));
        accumulator.parse_errors += 1;
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        diagnostics.push(Diagnostic::warning(
            "ast_parse",
            "TypeScript parser returned no syntax tree",
            Some(path.to_path_buf()),
        ));
        accumulator.parse_errors += 1;
        return Vec::new();
    };
    if tree.root_node().has_error() {
        diagnostics.push(Diagnostic::warning(
            "ast_syntax",
            "extension contains syntax the TypeScript parser could not fully recover",
            Some(path.to_path_buf()),
        ));
        accumulator.parse_errors += 1;
    }
    let mut local_imports = Vec::new();
    let api_bindings = extension_api_bindings(tree.root_node(), source.as_bytes());
    if !visit_ast(
        tree.root_node(),
        source.as_bytes(),
        &api_bindings,
        accumulator,
        analysis_budget,
        &mut local_imports,
    ) {
        diagnostics.push(Diagnostic::warning(
            "ast_node_limit",
            "extension or setup analysis exhausted its bounded syntax-node budget",
            Some(path.to_path_buf()),
        ));
        accumulator.parse_errors += 1;
    }
    local_imports
}

fn visit_ast(
    root: Node<'_>,
    source: &[u8],
    api_bindings: &BTreeSet<String>,
    accumulator: &mut AnalysisAccumulator,
    analysis_budget: &mut AnalysisBudget,
    local_imports: &mut Vec<String>,
) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if accumulator.ast_nodes >= MAX_AST_NODES
            || analysis_budget.syntax_nodes >= MAX_SCAN_AST_NODES
        {
            return false;
        }
        accumulator.ast_nodes += 1;
        analysis_budget.syntax_nodes += 1;
        match node.kind() {
            "import_statement" | "export_statement" => {
                if let Some(module) = node
                    .child_by_field_name("source")
                    .and_then(|source_node| string_value(source_node, source))
                {
                    record_import(&module, accumulator);
                    local_imports.push(module);
                }
            }
            "call_expression" => {
                inspect_call(node, source, api_bindings, accumulator, local_imports)
            }
            "new_expression" => inspect_new(node, source, accumulator),
            "member_expression" | "subscript_expression" => {
                if let Some(chain) = member_chain(node, source) {
                    inspect_member_chain(&chain, accumulator);
                }
            }
            "assignment_expression" | "augmented_assignment_expression" => {
                if let Some(left) = node
                    .child_by_field_name("left")
                    .and_then(|left| node_text(left, source))
                {
                    inspect_mutation(left, accumulator);
                }
            }
            "pair" => inspect_object_pair(node, source, api_bindings, accumulator),
            _ => {}
        }

        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    true
}

fn inspect_object_pair(
    node: Node<'_>,
    source: &[u8],
    api_bindings: &BTreeSet<String>,
    accumulator: &mut AnalysisAccumulator,
) {
    let Some(key) = node
        .child_by_field_name("key")
        .and_then(|key| node_text(key, source))
        .map(|key| key.trim_matches(['\'', '"']))
    else {
        return;
    };
    if matches!(key, "renderCall" | "renderResult" | "renderMessage") {
        accumulator.ui.insert(key.to_owned());
    }
    if matches!(
        key,
        "systemPrompt" | "input" | "arguments" | "args" | "result" | "content" | "isError"
    ) && enclosing_pi_event(node, source, api_bindings).is_some()
    {
        accumulator.mutations.insert(key.to_owned());
    }
}

fn enclosing_pi_event(
    mut node: Node<'_>,
    source: &[u8],
    api_bindings: &BTreeSet<String>,
) -> Option<String> {
    for _ in 0..64 {
        node = node.parent()?;
        if node.kind() != "call_expression" {
            continue;
        }
        let function = node.child_by_field_name("function")?;
        let chain = member_chain(function, source)?;
        if is_extension_api_direct_method(&chain, "on", api_bindings) {
            return first_string_argument(node, source);
        }
    }
    None
}

fn inspect_call(
    node: Node<'_>,
    source: &[u8],
    api_bindings: &BTreeSet<String>,
    accumulator: &mut AnalysisAccumulator,
    local_imports: &mut Vec<String>,
) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    if function.kind() == "import" {
        accumulator.security.dynamic_imports = true;
        if let Some(module) = first_string_argument(node, source) {
            record_import(&module, accumulator);
            local_imports.push(module);
        }
        return;
    }
    if function.kind() == "identifier" {
        match node_text(function, source) {
            Some("fetch") => accumulator.security.network = true,
            Some("require") => {
                if let Some(module) = first_string_argument(node, source) {
                    record_import(&module, accumulator);
                    local_imports.push(module);
                }
                return;
            }
            _ => {}
        }
    }
    let Some(chain) = member_chain(function, source) else {
        return;
    };
    let method = chain.rsplit('.').next().unwrap_or(&chain);
    if is_extension_api_direct_method(&chain, "on", api_bindings) {
        if let Some(event) = first_string_argument(node, source) {
            accumulator.events.insert(event);
        }
    }
    if let Some(event_bus_method) = extension_api_event_bus_method(&chain, api_bindings) {
        if matches!(event_bus_method, "on" | "emit") {
            accumulator.actions.insert("eventBus".to_owned());
        } else {
            accumulator
                .actions
                .insert(format!("unknown:events.{event_bus_method}"));
        }
    }
    if matches!(
        method,
        "registerTool"
            | "registerCommand"
            | "registerShortcut"
            | "registerFlag"
            | "registerProvider"
            | "unregisterProvider"
            | "registerMessageRenderer"
            | "registerMarkdownTransformer"
            | "registerEntryRenderer"
    ) {
        accumulator.registrations.insert(method.to_owned());
    }
    if method == "exec" {
        accumulator.security.process = true;
    }
    if matches!(
        method,
        "exec"
            | "appendEntry"
            | "setSessionName"
            | "getSessionName"
            | "setLabel"
            | "getActiveTools"
            | "setActiveTools"
            | "getAllTools"
            | "getCommands"
            | "getFlag"
            | "refreshTools"
            | "sendMessage"
            | "sendUserMessage"
            | "getModel"
            | "setModel"
            | "getThinkingLevel"
            | "setThinkingLevel"
            | "newSession"
            | "fork"
            | "navigateTree"
            | "switchSession"
            | "reload"
            | "compact"
            | "getSystemPrompt"
            | "getSystemPromptOptions"
            | "getContextUsage"
            | "waitForIdle"
            | "hasPendingMessages"
            | "shutdown"
    ) {
        accumulator.actions.insert(method.to_owned());
    }
    let direct_pi_method = is_extension_api_direct_method(&chain, method, api_bindings);
    if direct_pi_method
        && !matches!(
            method,
            "on" | "registerTool"
                | "registerCommand"
                | "registerShortcut"
                | "registerFlag"
                | "registerProvider"
                | "unregisterProvider"
                | "registerMessageRenderer"
                | "registerMarkdownTransformer"
                | "registerEntryRenderer"
                | "exec"
                | "appendEntry"
                | "setSessionName"
                | "getSessionName"
                | "setLabel"
                | "getActiveTools"
                | "setActiveTools"
                | "getAllTools"
                | "getCommands"
                | "getFlag"
                | "sendMessage"
                | "sendUserMessage"
                | "setModel"
                | "getThinkingLevel"
                | "setThinkingLevel"
        )
    {
        accumulator.actions.insert(format!("unknown:{method}"));
    }
    if let Some((owner, ui_suffix)) = chain.split_once(".ui.") {
        if !ui_suffix.contains('.') {
            if !owner.contains('.') && api_bindings.contains(owner) {
                accumulator
                    .actions
                    .insert(format!("unknown:ui.{ui_suffix}"));
            } else {
                accumulator.ui.insert(ui_suffix.to_owned());
            }
        }
    }
    inspect_member_chain(&chain, accumulator);
}

fn inspect_new(node: Node<'_>, source: &[u8], accumulator: &mut AnalysisAccumulator) {
    let constructor = node
        .child_by_field_name("constructor")
        .or_else(|| node.child_by_field_name("function"));
    if constructor
        .and_then(|constructor| node_text(constructor, source))
        .is_some_and(|name| matches!(name, "WebSocket" | "EventSource" | "XMLHttpRequest"))
    {
        accumulator.security.network = true;
    }
}

fn inspect_member_chain(chain: &str, accumulator: &mut AnalysisAccumulator) {
    if chain == "process.env" || chain.starts_with("process.env.") {
        accumulator.security.secrets = true;
    }
    if chain.starts_with("Deno.read")
        || chain.starts_with("Deno.write")
        || chain.starts_with("Bun.file")
        || chain.starts_with("Bun.write")
    {
        accumulator.security.filesystem = true;
    }
    if chain.starts_with("Deno.connect")
        || chain.starts_with("Deno.listen")
        || chain.starts_with("Bun.connect")
    {
        accumulator.security.network = true;
    }
    if chain.starts_with("Deno.Command") || chain.starts_with("Bun.spawn") {
        accumulator.security.process = true;
    }
    if chain.contains("sessionManager")
        || chain.contains("modelRegistry")
        || chain.contains("systemPrompt")
    {
        accumulator.actions.insert(chain.to_owned());
    }
}

fn inspect_mutation(left: &str, accumulator: &mut AnalysisAccumulator) {
    let compact = left.replace(' ', "");
    for field in [
        "arguments",
        ".args",
        ".input",
        ".result",
        ".content",
        "systemPrompt",
        "activeTools",
    ] {
        if compact.contains(field) {
            accumulator
                .mutations
                .insert(field.trim_start_matches('.').to_owned());
        }
    }
}

fn record_import(module: &str, accumulator: &mut AnalysisAccumulator) {
    accumulator.imports.insert(module.to_owned());
    let bare = module.strip_prefix("node:").unwrap_or(module);
    if matches!(bare, "fs" | "fs/promises") {
        accumulator.security.filesystem = true;
    }
    if bare == "process" {
        accumulator.security.secrets = true;
    }
    if matches!(bare, "child_process" | "cluster" | "worker_threads") {
        accumulator.security.process = true;
    }
    if matches!(
        bare,
        "http" | "https" | "http2" | "net" | "tls" | "dns" | "dgram"
    ) || module.starts_with("undici")
        || module.starts_with("node-fetch")
        || module.starts_with("axios")
    {
        accumulator.security.network = true;
    }
    if module.ends_with(".node") || module.contains("node-gyp") || module.contains("node-pre-gyp") {
        accumulator.security.native_modules = true;
    }
}

fn is_private_pi_import(module: &str) -> bool {
    module.starts_with("@earendil-works/pi-coding-agent/dist/")
        || module.starts_with("@earendil-works/pi-coding-agent/src/")
        || module.starts_with("@earendil-works/pi-tui/dist/")
        || module.starts_with("@earendil-works/pi-tui/src/")
}

const PI_0_84_4_EVENTS: &[&str] = &[
    "project_trust",
    "resources_discover",
    "session_start",
    "session_info_changed",
    "session_before_switch",
    "session_before_fork",
    "session_before_compact",
    "session_compact",
    "session_compact_failed",
    "session_shutdown",
    "session_before_tree",
    "session_tree",
    "context",
    "before_provider_request",
    "before_provider_headers",
    "after_provider_response",
    "before_agent_start",
    "agent_start",
    "agent_end",
    "agent_settled",
    "ui_prompt_start",
    "ui_prompt_end",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "model_select",
    "thinking_level_select",
    "user_bash",
    "input",
    "tool_call",
    "tool_result",
];

const PI_0_84_4_UI_METHODS: &[&str] = &[
    "select",
    "confirm",
    "input",
    "editor",
    "notify",
    "onTerminalInput",
    "setStatus",
    "setWorkingMessage",
    "setWorkingVisible",
    "setWorkingIndicator",
    "setHiddenThinkingLabel",
    "setWidget",
    "setFooter",
    "setHeader",
    "setTitle",
    "custom",
    "pasteToEditor",
    "setEditorText",
    "getEditorText",
    "addAutocompleteProvider",
    "setEditorComponent",
    "getEditorComponent",
    "getAllThemes",
    "getTheme",
    "setTheme",
    "getToolsExpanded",
    "setToolsExpanded",
];

const PI_0_84_4_RENDERER_FIELDS: &[&str] = &["renderCall", "renderResult", "renderMessage"];

const PI_0_84_4_REGISTRATIONS: &[&str] = &[
    "registerTool",
    "registerCommand",
    "registerShortcut",
    "registerFlag",
    "registerProvider",
    "unregisterProvider",
    "registerMessageRenderer",
    "registerMarkdownTransformer",
    "registerEntryRenderer",
];

fn classify_extension(accumulator: &AnalysisAccumulator) -> (MigrationPath, Vec<String>) {
    let mut reasons = Vec::new();
    let parse_incomplete =
        accumulator.parse_errors > 0 || !accumulator.unresolved_imports.is_empty();
    let private_pi_imports = accumulator
        .imports
        .iter()
        .filter(|module| is_private_pi_import(module))
        .cloned()
        .collect::<Vec<_>>();
    if !private_pi_imports.is_empty() {
        reasons.push(format!(
            "imports Pi private/internal modules outside the public 0.84.4 compatibility profile: {}",
            private_pi_imports.join(", ")
        ));
        return (MigrationPath::Blocked, reasons);
    }
    let unknown_events = accumulator
        .events
        .iter()
        .filter(|event| !PI_0_84_4_EVENTS.contains(&event.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let unknown_ui = accumulator
        .ui
        .iter()
        .filter(|method| {
            !PI_0_84_4_UI_METHODS.contains(&method.as_str())
                && !PI_0_84_4_RENDERER_FIELDS.contains(&method.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    let unknown_registrations = accumulator
        .registrations
        .iter()
        .filter(|method| !PI_0_84_4_REGISTRATIONS.contains(&method.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let unknown_actions = accumulator
        .actions
        .iter()
        .filter_map(|action| action.strip_prefix("unknown:").map(str::to_owned))
        .collect::<Vec<_>>();
    if !unknown_events.is_empty()
        || !unknown_ui.is_empty()
        || !unknown_registrations.is_empty()
        || !unknown_actions.is_empty()
    {
        if !unknown_events.is_empty() {
            reasons.push(format!(
                "uses event names outside the pinned Pi 0.84.4 compatibility profile: {}",
                unknown_events.join(", ")
            ));
        }
        if !unknown_ui.is_empty() {
            reasons.push(format!(
                "uses UI methods outside the pinned Pi 0.84.4 compatibility profile: {}",
                unknown_ui.join(", ")
            ));
        }
        if !unknown_registrations.is_empty() {
            reasons.push(format!(
                "uses registrations outside the pinned Pi 0.84.4 compatibility profile: {}",
                unknown_registrations.join(", ")
            ));
        }
        if !unknown_actions.is_empty() {
            reasons.push(format!(
                "uses ExtensionAPI methods outside the pinned Pi 0.84.4 compatibility profile: {}",
                unknown_actions.join(", ")
            ));
        }
        if parse_incomplete {
            reasons.push(
                "the source graph was only partially analyzed; review parse diagnostics and unresolved imports"
                    .to_owned(),
            );
        }
        return (MigrationPath::Blocked, reasons);
    }
    let imports_tui = accumulator
        .imports
        .iter()
        .any(|module| module.contains("pi-tui") || module.ends_with("/tui") || module == "ink");
    let arbitrary_ui = accumulator.ui.iter().any(|method| {
        matches!(
            method.as_str(),
            "setEditor"
                | "setEditorComponent"
                | "getEditorComponent"
                | "setFooter"
                | "setHeader"
                | "setWidget"
                | "custom"
                | "overlay"
                | "addAutocompleteProvider"
                | "setAutocompleteProvider"
                | "onTerminalInput"
        )
    });
    let provider = accumulator.registrations.iter().any(|registration| {
        matches!(
            registration.as_str(),
            "registerProvider" | "unregisterProvider"
        )
    }) || accumulator.events.iter().any(|event| {
        matches!(
            event.as_str(),
            "before_provider_request" | "before_provider_headers" | "after_provider_response"
        )
    });
    let message_renderer = accumulator.registrations.iter().any(|registration| {
        matches!(
            registration.as_str(),
            "registerMessageRenderer" | "registerMarkdownTransformer" | "registerEntryRenderer"
        )
    });
    let deep_session = accumulator.actions.iter().any(|action| {
        action.contains("sessionManager")
            || matches!(
                action.as_str(),
                "appendEntry"
                    | "setSessionName"
                    | "setLabel"
                    | "newSession"
                    | "fork"
                    | "navigateTree"
                    | "switchSession"
                    | "compact"
            )
    }) || accumulator.events.iter().any(|event| {
        matches!(
            event.as_str(),
            "session_info_changed"
                | "session_before_switch"
                | "session_before_fork"
                | "session_before_compact"
                | "session_compact"
                | "session_compact_failed"
                | "session_before_tree"
                | "session_tree"
        )
    });
    if arbitrary_ui || provider || deep_session {
        if arbitrary_ui {
            reasons.push("depends on Pi's arbitrary TUI/editor component surface".to_owned());
        }
        if provider {
            reasons.push("registers provider-native behavior".to_owned());
        }
        if deep_session {
            reasons.push("depends on Pi session, entry, or compaction internals".to_owned());
        }
        if parse_incomplete {
            reasons.push(
                "the source graph was only partially analyzed; review parse diagnostics and unresolved imports"
                    .to_owned(),
            );
        }
        return (MigrationPath::Manual, reasons);
    }

    let unsupported_registration = accumulator
        .registrations
        .iter()
        .any(|registration| matches!(registration.as_str(), "registerShortcut" | "registerFlag"));
    let unsupported_event = accumulator.events.iter().any(|event| {
        matches!(
            event.as_str(),
            "project_trust"
                | "resources_discover"
                | "input"
                | "before_agent_start"
                | "tool_result"
                | "context"
                | "message_start"
                | "message_update"
                | "message_end"
                | "model_select"
                | "thinking_level_select"
                | "user_bash"
                | "session_before_switch"
                | "session_before_fork"
                | "session_before_compact"
                | "session_before_tree"
        )
    });
    let active_tools = accumulator
        .actions
        .iter()
        .any(|action| matches!(action.as_str(), "getActiveTools" | "setActiveTools"));
    let unsupported_action = accumulator.actions.iter().any(|action| {
        matches!(
            action.as_str(),
            "getFlag"
                | "sendMessage"
                | "sendUserMessage"
                | "getModel"
                | "setModel"
                | "setThinkingLevel"
                | "reload"
                | "getSystemPrompt"
                | "getSystemPromptOptions"
                | "getContextUsage"
                | "waitForIdle"
                | "hasPendingMessages"
                | "shutdown"
        )
    });
    let custom_tool_renderer = accumulator
        .ui
        .iter()
        .any(|surface| matches!(surface.as_str(), "renderCall" | "renderResult"));
    let semantic_ui_port = imports_tui
        || message_renderer
        || accumulator
            .ui
            .iter()
            .any(|surface| matches!(surface.as_str(), "custom" | "select" | "theme"));
    if unsupported_registration
        || unsupported_event
        || active_tools
        || unsupported_action
        || custom_tool_renderer
        || semantic_ui_port
        || !accumulator.mutations.is_empty()
    {
        if unsupported_registration {
            reasons.push(
                "uses shortcut or CLI-flag registration that API 0.2 does not expose".to_owned(),
            );
        }
        if unsupported_event {
            reasons
                .push("subscribes to a mutating Pi hook without an equivalent Ygg hook".to_owned());
        }
        if active_tools {
            reasons.push(
                "mutates Pi's active tool set instead of a bounded Ygg policy overlay".to_owned(),
            );
        }
        if unsupported_action {
            reasons.push(
                "uses Pi host-state or agent-control actions without an equivalent Ygg bridge service"
                    .to_owned(),
            );
        }
        if custom_tool_renderer {
            reasons.push("uses a Pi component renderer that needs a semantic Ygg port".to_owned());
        }
        if semantic_ui_port {
            reasons.push(
                "uses Pi UI components that need a semantic Ygg presentation port".to_owned(),
            );
        }
        if !accumulator.mutations.is_empty() {
            reasons.push("mutates prompt, tool arguments, input, or tool results".to_owned());
        }
        if parse_incomplete {
            reasons.push(
                "the source graph was only partially analyzed; review parse diagnostics and unresolved imports"
                    .to_owned(),
            );
        }
        return (MigrationPath::NativePort, reasons);
    }

    if parse_incomplete {
        reasons.push(
            "source graph could not be analyzed completely, so compatibility is unknown".to_owned(),
        );
        return (MigrationPath::Blocked, reasons);
    }

    reasons.push(
        "uses capability-shaped registrations supported by a compatibility process".to_owned(),
    );
    (MigrationPath::Bridge, reasons)
}

fn is_code_import(import: &str) -> bool {
    matches!(
        Path::new(import).extension().and_then(OsStr::to_str),
        None | Some("ts" | "tsx" | "mts" | "cts" | "js" | "mjs" | "cjs")
    )
}

fn resolve_local_import(importer: &Path, package_root: &Path, import: &str) -> Option<PathBuf> {
    let parent = importer.parent()?;
    let base = if import.starts_with('.') {
        normalize_absolute(&parent.join(import)).ok()?
    } else {
        let import = import.strip_prefix("#src/")?;
        normalize_absolute(&package_root.join("src").join(import)).ok()?
    };
    if !base.starts_with(package_root) {
        return None;
    }
    let candidates = if base.extension().is_some() {
        let mut candidates = vec![base.clone()];
        if matches!(
            base.extension().and_then(OsStr::to_str),
            Some("js" | "mjs" | "cjs")
        ) {
            candidates.extend(
                ["ts", "tsx", "mts", "cts", "d.ts"]
                    .into_iter()
                    .map(|extension| base.with_extension(extension)),
            );
        }
        candidates
    } else {
        let mut candidates = ["ts", "tsx", "mts", "cts", "js", "mjs", "cjs", "d.ts"]
            .into_iter()
            .map(|extension| base.with_extension(extension))
            .collect::<Vec<_>>();
        candidates.extend(
            [
                "index.ts",
                "index.tsx",
                "index.mts",
                "index.cts",
                "index.js",
                "index.mjs",
                "index.cjs",
            ]
            .into_iter()
            .map(|name| base.join(name)),
        );
        candidates
    };
    candidates.into_iter().find(|candidate| {
        candidate.starts_with(package_root)
            && std::fs::symlink_metadata(candidate)
                .is_ok_and(|metadata| metadata.file_type().is_file())
    })
}

fn first_string_argument(node: Node<'_>, source: &[u8]) -> Option<String> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let value = arguments
        .named_children(&mut cursor)
        .find_map(|argument| string_value(argument, source));
    value
}

fn string_value(node: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(
        node.kind(),
        "string" | "string_fragment" | "template_string"
    ) {
        return None;
    }
    let text = node_text(node, source)?;
    let value = text
        .strip_prefix(['\'', '"', '`'])
        .and_then(|text| text.strip_suffix(['\'', '"', '`']))
        .unwrap_or(text);
    (!value.contains("${")).then(|| value.to_owned())
}

fn member_chain(node: Node<'_>, source: &[u8]) -> Option<String> {
    member_chain_inner(node, source, 0)
}

fn member_chain_inner(node: Node<'_>, source: &[u8], depth: usize) -> Option<String> {
    if depth >= 64 {
        return None;
    }
    match node.kind() {
        "identifier" | "property_identifier" | "private_property_identifier" | "this" => {
            node_text(node, source).map(str::to_owned)
        }
        "member_expression" => {
            let object = node.child_by_field_name("object")?;
            let property = node.child_by_field_name("property")?;
            Some(format!(
                "{}.{}",
                member_chain_inner(object, source, depth + 1)?,
                node_text(property, source)?
            ))
        }
        "subscript_expression" => {
            let object = node.child_by_field_name("object")?;
            let index = node.child_by_field_name("index")?;
            let property = string_value(index, source)?;
            Some(format!(
                "{}.{}",
                member_chain_inner(object, source, depth + 1)?,
                property
            ))
        }
        _ => None,
    }
}

fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(&source[node.byte_range()]).ok()
}

fn hash_files<'a>(
    root: &Path,
    files: impl IntoIterator<Item = &'a Path>,
    total_limit: usize,
    label: &str,
    analysis_budget: &mut AnalysisBudget,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    let mut files = files.into_iter().map(Path::to_path_buf).collect::<Vec<_>>();
    files.sort();
    files.dedup();
    let mut total = 0usize;
    let mut hasher = Sha256::new();
    let mut hashed = 0usize;
    let mut complete = true;
    for path in files {
        let package_remaining = total_limit.saturating_sub(total);
        let scan_remaining = MAX_SCAN_HASH_BYTES.saturating_sub(analysis_budget.hashed_bytes);
        let remaining = package_remaining.min(scan_remaining);
        if remaining == 0 {
            diagnostics.push(Diagnostic::warning(
                "hash_limit",
                format!("{label} hashing exhausted its package or setup byte budget"),
                Some(root.to_path_buf()),
            ));
            complete = false;
            break;
        }
        let per_file_limit = if path.file_name().is_some_and(|name| {
            matches!(
                name.to_str(),
                Some("package-lock.json" | "npm-shrinkwrap.json" | "pnpm-lock.yaml" | "yarn.lock")
            )
        }) {
            MAX_LOCK_FILE_BYTES.min(remaining)
        } else {
            MAX_SOURCE_FILE_BYTES.min(remaining)
        };
        let bytes = match ygg_agent::secure_fs::read_regular_file_bounded(&path, per_file_limit) {
            Ok(bytes) => bytes,
            Err(error) => {
                diagnostics.push(Diagnostic::warning(
                    "hash_read",
                    format!("could not include file in {label} hash: {error}"),
                    Some(path),
                ));
                complete = false;
                continue;
            }
        };
        total += bytes.len();
        analysis_budget.hashed_bytes = analysis_budget.hashed_bytes.saturating_add(bytes.len());
        let relative = relative_slash(root, &path);
        hasher.update((relative.len() as u64).to_be_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(&bytes);
        hashed += 1;
    }
    (complete && hashed > 0).then(|| format!("{:x}", hasher.finalize()))
}

fn npm_package_name(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let name = if spec.starts_with('@') {
        let slash = spec.find('/')?;
        match spec[slash + 1..].rfind('@') {
            Some(offset) => &spec[..slash + 1 + offset],
            None => spec,
        }
    } else {
        spec.rsplit_once('@').map_or(spec, |(name, _)| name)
    };
    let valid = !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "@._~-".contains(character)
                })
        });
    valid.then(|| name.to_owned())
}

fn parse_git_source(source: &str) -> Option<(String, PathBuf)> {
    let trimmed = source.trim();
    let explicit = trimmed.strip_prefix("git:");
    let value = explicit.unwrap_or(trimmed);
    let (host, path) = if let Some(rest) = value.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        (host.to_owned(), path.to_owned())
    } else if value.contains("://") {
        let url = url::Url::parse(value).ok()?;
        (
            url.host_str()?.to_owned(),
            url.path().trim_start_matches('/').to_owned(),
        )
    } else if explicit.is_some() {
        let (host, path) = value.split_once('/')?;
        (host.to_owned(), path.to_owned())
    } else {
        return None;
    };
    let path = strip_git_ref(&path).trim_end_matches(".git");
    let components = path.split('/').collect::<Vec<_>>();
    let valid = !host.is_empty()
        && components.len() >= 2
        && components.iter().all(|component| {
            !component.is_empty()
                && *component != "."
                && *component != ".."
                && !component.contains(['\\', '\0'])
        });
    valid.then(|| (host, components.into_iter().collect()))
}

fn strip_git_ref(path: &str) -> &str {
    path.rsplit_once('@').map_or(path, |(path, _)| path)
}

fn expand_local_path(source: &str, base: &Path) -> anyhow::Result<PathBuf> {
    let path = if source == "~" {
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory is unavailable"))?
    } else if let Some(path) = source.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("home directory is unavailable"))?
            .join(path)
    } else {
        let path = Path::new(source);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        }
    };
    normalize_absolute(&path)
}

fn absolute_path(path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = if path.is_absolute() {
        normalize_absolute(path)?
    } else {
        normalize_absolute(&cwd.join(path))?
    };
    let resolved = match std::fs::canonicalize(&path) {
        Ok(canonical) => normalize_absolute(&canonical)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
        Err(error) => return Err(error.into()),
    };
    if resolved.to_str().is_none() {
        anyhow::bail!(
            "migration paths must be valid UTF-8: {}",
            resolved.display()
        );
    }
    Ok(resolved)
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
    Ok(normalized)
}

fn confined_join(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        anyhow::bail!("package resource path must be relative: {relative:?}");
    }
    let path = normalize_absolute(&root.join(candidate))?;
    if !path.starts_with(root) {
        anyhow::bail!("package resource path escapes its package: {relative:?}");
    }
    Ok(path)
}

fn validate_directory_root(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("package directory is unavailable: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("symlinked package directories are not scanned".to_owned());
    }
    if !metadata.is_dir() {
        return Err("package path is not a directory".to_owned());
    }
    let normalized = normalize_absolute(path).map_err(|error| error.to_string())?;
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("package directory could not be canonicalized: {error}"))?;
    let canonical = normalize_absolute(&canonical).map_err(|error| error.to_string())?;
    if normalized != canonical {
        return Err("package directory traverses a symbolic link".to_owned());
    }
    Ok(())
}

fn has_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map(|glob| glob.compile_matcher())
}

fn relative_slash(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(component) => Some(component.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn options(root: &Path) -> ScanOptions {
        let root = fs::canonicalize(root).unwrap();
        ScanOptions {
            pi_home: root.join("home/.pi/agent"),
            project: root.join("workspace"),
            npm_roots: Vec::new(),
        }
    }

    #[test]
    fn inventories_local_package_without_executing_it() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path());
        let package = temp.path().join("package");
        let marker = temp.path().join("executed");
        write(
            &options.pi_home.join("settings.json"),
            &format!(
                r#"{{"packages":[{}]}}"#,
                serde_json::to_string(package.to_str().unwrap()).unwrap()
            ),
        );
        write(
            &package.join("package.json"),
            r#"{
              "name": "fixture-pi-package",
              "version": "1.2.3",
              "pi": {
                "extensions": ["src/index.ts"],
                "skills": ["skills"],
                "prompts": ["prompts"],
                "themes": ["themes"]
              }
            }"#,
        );
        write(
            &package.join("src/index.ts"),
            &format!(
                r#"import {{ writeFileSync }} from "node:fs";
                   import helper from "./helper.js";
                   writeFileSync({}, "ran");
                   export default function (pi: ExtensionAPI) {{
                     pi.registerTool({{ name: "fixture", execute: helper }});
                     pi.registerCommand("fixture", {{ handler: helper }});
                     pi.on("input", (event) => ({{ input: event.text.trim() }}));
                   }}"#,
                serde_json::to_string(marker.to_str().unwrap()).unwrap()
            ),
        );
        write(
            &package.join("src/helper.ts"),
            r#"import { spawn } from "node:child_process";
               export default () => spawn("true");"#,
        );
        write(&package.join("skills/review/SKILL.md"), "# Review\n");
        write(&package.join("prompts/review.md"), "Review this.\n");
        write(&package.join("themes/dark.json"), "{}\n");
        write(&package.join("package-lock.json"), "{}\n");

        let report = scan_pi(&options);

        assert!(!marker.exists(), "the scanner executed extension source");
        assert_eq!(report.model_usage, "disabled");
        assert!(!report.package_code_executed);
        assert_eq!(report.found.packages, 1);
        assert_eq!(report.found.extensions, 1);
        assert_eq!(report.found.skills, 1);
        assert_eq!(report.found.prompts, 1);
        assert_eq!(report.found.themes, 1);
        let package = &report.packages[0];
        assert_eq!(package.name.as_deref(), Some("fixture-pi-package"));
        assert_eq!(package.version.as_deref(), Some("1.2.3"));
        assert!(package.source_hash.is_some());
        assert!(package.lock_hash.is_some());
        assert_eq!(package.migration, MigrationPath::Manual);
        let extension = &package.extensions[0];
        assert_eq!(extension.migration, MigrationPath::NativePort);
        assert!(extension
            .surfaces
            .registrations
            .contains(&"registerTool".to_owned()));
        assert!(extension.surfaces.events.contains(&"input".to_owned()));
        assert!(extension.security.filesystem);
        assert!(extension.security.process);
        assert_eq!(extension.analyzed_files.len(), 2);

        let original_hash = package.source_hash.clone();
        write(
            &temp.path().join("package/src/helper.ts"),
            r#"export default () => "changed";"#,
        );
        let changed = scan_pi(&options);
        assert_ne!(changed.packages[0].source_hash, original_hash);
    }

    #[test]
    fn manifest_root_extension_resolves_index_once() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path());
        let package = temp.path().join("self-extension");
        let source = serde_json::to_string(package.to_str().unwrap()).unwrap();
        write(
            &options.pi_home.join("settings.json"),
            &format!(r#"{{"packages":[{source}]}}"#),
        );
        write(
            &package.join("package.json"),
            r#"{"name":"self-extension","version":"1.0.0","pi":{"extensions":["./"]}}"#,
        );
        write(
            &package.join("index.ts"),
            "export default (pi) => pi.registerTool({ name: 'self' });\n",
        );

        let report = scan_pi(&options);

        let package_root = fs::canonicalize(&package).unwrap();
        let package_report = &report.packages[0];
        assert_eq!(package_report.resources.len(), 1);
        assert_eq!(
            package_report.resources[0].path,
            package_root.join("index.ts")
        );
        assert!(package_report.resources[0].enabled);
        assert_eq!(package_report.extensions.len(), 1);
        assert!(!package_report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "manifest_cycle"));

        write(
            &options.pi_home.join("settings.json"),
            &format!(r#"{{"packages":[{{"source":{source},"autoload":false}}]}}"#),
        );
        let disabled = scan_pi(&options);
        assert_eq!(disabled.packages[0].resources.len(), 1);
        assert!(!disabled.packages[0].resources[0].enabled);
        assert!(disabled.packages[0].extensions.is_empty());
    }

    #[test]
    fn analyzes_bundled_extension_files_above_the_legacy_two_mib_limit() {
        let temp = tempfile::tempdir().unwrap();
        let extension = temp.path().join("bundle.js");
        let mut source = vec![b' '; 3 * 1024 * 1024];
        source.extend_from_slice(
            b"\nexport default (api) => api.registerCommand('ok', { handler() {} });\n",
        );
        fs::write(&extension, source).unwrap();
        let mut diagnostics = Vec::new();
        let report = analyze_extension(&extension, temp.path(), &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Bridge);
        assert!(report.analyzed_source_bytes > 2 * 1024 * 1024);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn bounds_nested_extension_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("manifest-chain");
        let mut current = root.clone();
        for _ in 0..=MAX_EXTENSION_MANIFEST_DEPTH {
            write(
                &current.join("package.json"),
                r#"{"pi":{"extensions":["next"]}}"#,
            );
            current = current.join("next");
        }
        let root = fs::canonicalize(root).unwrap();
        let mut traversal = ResourceTraversal::default();
        let mut diagnostics = Vec::new();

        let paths = collect_resource_path(
            &root,
            ResourceKind::Extension,
            &root,
            &mut traversal,
            &mut diagnostics,
        );

        assert!(paths.is_empty());
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "manifest_depth_limit"));
        assert!(traversal.active_extension_manifests.is_empty());
    }

    #[test]
    fn single_file_package_hash_excludes_unrelated_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path());
        let directory = fs::canonicalize(temp.path()).unwrap().join("loose");
        let extension = directory.join("extension.ts");
        let unrelated = directory.join("unrelated.ts");
        write(
            &options.pi_home.join("settings.json"),
            &format!(
                r#"{{"packages":[{}]}}"#,
                serde_json::to_string(extension.to_str().unwrap()).unwrap()
            ),
        );
        write(
            &extension,
            "export default (pi) => pi.registerCommand('x', {});\n",
        );
        write(&unrelated, "export const value = 1;\n");

        let first = scan_pi(&options).packages[0].source_hash.clone();
        write(&unrelated, "export const value = 2;\n");
        let second = scan_pi(&options).packages[0].source_hash.clone();

        assert_eq!(first, second);
    }

    #[test]
    fn analyzes_top_level_extensions_with_the_same_ast_pipeline() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path());
        write(
            &options.pi_home.join("extensions/normalize.ts"),
            r#"export default (pi) => pi.on("tool_result", (event) => ({ content: event.content }));"#,
        );

        let report = scan_pi(&options);

        assert_eq!(report.found.extensions, 1);
        assert_eq!(report.extensions.len(), 1);
        assert_eq!(report.extensions[0].migration, MigrationPath::NativePort);
        assert_eq!(report.resources[0].scope, Scope::User);
        assert_eq!(report.resources[0].migration, MigrationPath::NativePort);
    }

    #[test]
    fn project_package_wins_over_same_user_identity() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path());
        let user_package = options.pi_home.join("npm/node_modules/@scope/example");
        let project_package = options.project.join(".pi/npm/node_modules/@scope/example");
        write(
            &options.pi_home.join("settings.json"),
            r#"{"packages":["npm:@scope/example@1.0.0"]}"#,
        );
        write(
            &options.project.join(".pi/settings.json"),
            r#"{"packages":["npm:@scope/example@2.0.0"]}"#,
        );
        write(
            &user_package.join("package.json"),
            r#"{"name":"@scope/example","version":"1.0.0","pi":{"skills":["skills"]}}"#,
        );
        write(&user_package.join("skills/a/SKILL.md"), "# A\n");
        write(
            &project_package.join("package.json"),
            r#"{"name":"@scope/example","version":"2.0.0","pi":{"skills":["skills"]}}"#,
        );
        write(&project_package.join("skills/b/SKILL.md"), "# B\n");

        let report = scan_pi(&options);

        assert_eq!(report.packages.len(), 1);
        assert_eq!(report.packages[0].scope, Scope::Project);
        assert_eq!(report.packages[0].version.as_deref(), Some("2.0.0"));
        assert_eq!(report.packages[0].resources.len(), 1);
        assert!(report.packages[0].resources[0]
            .path
            .ends_with("skills/b/SKILL.md"));
    }

    #[test]
    fn project_autoload_delta_filters_the_user_install() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path());
        let user_package = options.pi_home.join("npm/node_modules/example");
        write(
            &options.pi_home.join("settings.json"),
            r#"{"packages":["npm:example@1.0.0"]}"#,
        );
        write(
            &options.project.join(".pi/settings.json"),
            r#"{"packages":[{"source":"npm:example","autoload":false,"skills":["skills/one/**"]}]}"#,
        );
        write(
            &user_package.join("package.json"),
            r#"{"name":"example","version":"1.0.0","pi":{"skills":["skills"]}}"#,
        );
        write(&user_package.join("skills/one/SKILL.md"), "# One\n");
        write(&user_package.join("skills/two/SKILL.md"), "# Two\n");

        let report = scan_pi(&options);

        assert_eq!(report.packages.len(), 1);
        let package = &report.packages[0];
        assert_eq!(package.scope, Scope::Project);
        assert_eq!(package.root.as_deref(), Some(user_package.as_path()));
        assert_eq!(package.resources.len(), 2);
        assert_eq!(
            package
                .resources
                .iter()
                .filter(|resource| resource.enabled)
                .count(),
            1
        );
    }

    #[test]
    fn classifies_capability_shaped_and_pi_native_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let bridge = root.join("bridge.ts");
        write(
            &bridge,
            r#"export default (pi) => {
                 pi.registerTool({ name: "search", execute: async () => ({ content: [{ type: "text", text: "ok" }] }) });
                 pi.on("session_start", (_event, ctx) => {
                   pi.events.emit("ready");
                   ctx.ui.notify("ready");
                 });
               };"#,
        );
        let mut diagnostics = Vec::new();
        let report = analyze_extension(&bridge, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Bridge);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let manual = root.join("manual.tsx");
        write(
            &manual,
            r#"import { Component } from "@earendil-works/pi-tui";
               export default (pi) => pi.registerProvider("custom", {});"#,
        );
        let report = analyze_extension(&manual, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Manual);
    }

    #[test]
    fn classifies_pi_0844_surfaces_and_fails_closed_on_unknown_apis() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut diagnostics = Vec::new();

        let provider = root.join("provider.ts");
        write(
            &provider,
            r#"export default (pi) => {
                 pi.on("before_provider_headers", () => {});
                 pi.registerProvider("custom", {});
               };"#,
        );
        let report = analyze_extension(&provider, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Manual);

        let autocomplete = root.join("autocomplete.ts");
        write(
            &autocomplete,
            r#"export default (pi) => pi.on("session_start", (_event, ctx) => {
                 ctx.ui.addAutocompleteProvider((current) => current);
               });"#,
        );
        let report = analyze_extension(&autocomplete, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Manual);

        let input = root.join("input.ts");
        write(
            &input,
            r#"export default (pi) => pi.on("input", (event) => ({
                 action: "transform", text: event.text.trim()
               }));"#,
        );
        let report = analyze_extension(&input, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::NativePort);

        let renderer = root.join("renderer.ts");
        write(
            &renderer,
            r#"export default (api) => api.registerTool({
                 name: "rendered",
                 execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
                 renderCall: () => null,
                 renderResult: () => null
               });"#,
        );
        let report = analyze_extension(&renderer, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::NativePort);
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.contains("component renderer")));

        let ordinary_pi = root.join("ordinary-pi.ts");
        write(
            &ordinary_pi,
            r#"import { normalize } from "./ordinary-helper.js";
               export default (api) => api.registerTool({
                 name: "ordinary",
                 execute: async () => ({ content: [{ type: "text", text: normalize(" pi ") }] })
               });"#,
        );
        write(
            &root.join("ordinary-helper.ts"),
            r#"export function normalize(pi: string): string { return pi.trim(); }"#,
        );
        let report = analyze_extension(&ordinary_pi, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Bridge);
        assert!(!report
            .surfaces
            .actions
            .iter()
            .any(|action| action == "unknown:trim"));

        let private_import = root.join("private-import.ts");
        write(
            &private_import,
            r#"import { ExtensionRunner } from "@earendil-works/pi-coding-agent/dist/core/extensions/runner.js";
               export default (_api) => void ExtensionRunner;"#,
        );
        let report = analyze_extension(&private_import, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Blocked);
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.contains("private/internal")));

        let event_bus = root.join("event-bus.ts");
        write(
            &event_bus,
            r#"export default (extensionApi) => {
                 extensionApi.events.on("acme:ready", () => {});
                 extensionApi.events.emit("acme:ready", { ok: true });
               };"#,
        );
        let report = analyze_extension(&event_bus, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Bridge);
        assert!(report.surfaces.events.is_empty());
        assert_eq!(report.surfaces.actions, ["eventBus"]);

        let unknown = root.join("unknown.ts");
        write(
            &unknown,
            r#"function extension(api) {
                 api.on("future_event", () => {});
                 api.futureCapability();
                 api.events.future();
               }
               export default extension;"#,
        );
        let report = analyze_extension(&unknown, root, &mut diagnostics);
        assert_eq!(report.migration, MigrationPath::Blocked);
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.contains("outside the pinned Pi 0.84.4 compatibility profile")));
    }

    #[test]
    fn nested_pi_theme_methods_are_not_mistaken_for_unknown_ui_apis() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let extension = root.join("theme.ts");
        write(
            &extension,
            r#"export default (pi) => {
                 pi.registerCommand("status", {
                   handler: async (_args, ctx) => ctx.ui.notify(ctx.ui.theme.fg("accent", "ok"))
                 });
               };"#,
        );
        let mut diagnostics = Vec::new();
        let report = analyze_extension(&extension, root, &mut diagnostics);
        assert_eq!(
            report.migration,
            MigrationPath::Bridge,
            "{:?}",
            report.reasons
        );
    }

    #[test]
    fn blocks_a_thin_wrapper_when_its_internal_source_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entrypoint = root.join("index.ts");
        write(&entrypoint, r#"export { default } from "./missing.js";"#);
        let mut diagnostics = Vec::new();

        let report = analyze_extension(&entrypoint, root, &mut diagnostics);

        assert_eq!(report.migration, MigrationPath::Blocked);
        assert_eq!(
            report.surfaces.unresolved_imports,
            vec!["./missing.js".to_owned()]
        );
    }

    #[test]
    fn exhausted_setup_budget_blocks_remaining_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let entrypoint = root.join("extension.ts");
        write(&entrypoint, "export default (pi) => pi.registerTool({});\n");
        let mut diagnostics = Vec::new();
        let mut budget = AnalysisBudget {
            files: MAX_SCAN_ANALYZED_FILES,
            ..AnalysisBudget::default()
        };

        let report =
            analyze_extension_with_budget(&entrypoint, &root, &mut budget, &mut diagnostics);

        assert_eq!(report.migration, MigrationPath::Blocked);
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "scan_file_limit"));
    }

    #[test]
    fn package_filter_uses_conventions_for_manifest_omissions() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        write(&root.join("skills/one/SKILL.md"), "# One\n");
        let manifest = PiManifest {
            extensions: Some(Vec::new()),
            ..PiManifest::default()
        };
        let filter = PackageFilter {
            source: root.display().to_string(),
            autoload: None,
            extensions: None,
            skills: None,
            prompts: None,
            themes: None,
        };
        let mut diagnostics = Vec::new();

        let unfiltered =
            collect_package_resources(&root, Some(&manifest), None, Scope::User, &mut diagnostics);
        let filtered = collect_package_resources(
            &root,
            Some(&manifest),
            Some(&filter),
            Scope::User,
            &mut diagnostics,
        );

        assert!(unfiltered.is_empty());
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].kind, ResourceKind::Skill);
    }

    #[test]
    fn filters_package_resources_without_losing_disabled_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        write(&root.join("skills/one/SKILL.md"), "# One\n");
        write(&root.join("skills/two/SKILL.md"), "# Two\n");
        let filter = PackageFilter {
            source: root.display().to_string(),
            autoload: None,
            extensions: None,
            skills: Some(vec!["skills/one/**".to_owned()]),
            prompts: None,
            themes: None,
        };
        let mut diagnostics = Vec::new();
        let resources =
            collect_package_resources(&root, None, Some(&filter), Scope::User, &mut diagnostics);
        let skills = resources
            .iter()
            .filter(|resource| resource.kind == ResourceKind::Skill)
            .collect::<Vec<_>>();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills.iter().filter(|resource| resource.enabled).count(), 1);
    }

    #[test]
    fn parses_scoped_and_unscoped_npm_specs() {
        assert_eq!(npm_package_name("pkg@1.2.3").as_deref(), Some("pkg"));
        assert_eq!(
            npm_package_name("@scope/pkg@^2").as_deref(),
            Some("@scope/pkg")
        );
        assert_eq!(
            npm_package_name("@scope/pkg").as_deref(),
            Some("@scope/pkg")
        );
        assert!(npm_package_name("../pkg").is_none());
    }

    #[test]
    fn parses_supported_git_sources_without_ref_in_install_path() {
        assert_eq!(
            parse_git_source("git:github.com/user/repo@v1"),
            Some(("github.com".to_owned(), PathBuf::from("user/repo")))
        );
        assert_eq!(
            parse_git_source("https://github.com/user/repo.git@abc"),
            Some(("github.com".to_owned(), PathBuf::from("user/repo")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_package_roots() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path());
        let real = temp.path().join("real-package");
        let link = temp.path().join("linked-package");
        fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();
        write(
            &options.pi_home.join("settings.json"),
            &format!(
                r#"{{"packages":[{}]}}"#,
                serde_json::to_string(link.to_str().unwrap()).unwrap()
            ),
        );

        let report = scan_pi(&options);

        assert_eq!(report.packages[0].migration, MigrationPath::Blocked);
        assert!(report.packages[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package_unresolved"));
    }
}
