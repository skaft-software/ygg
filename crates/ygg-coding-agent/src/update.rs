//! Explicit, bounded release update check and self-update.
//!
//! The check fetches the latest GitHub release with a short timeout, a hard
//! response-size limit, and no redirects. The update delegates to the channel
//! that installed the running binary: the version-pinned installer for
//! installer installs, a pinned `cargo install` for Cargo installs, or an exact
//! npm install for a validated global npm package. Ygg never replaces itself
//! in process; the channel swaps the installed files under the running
//! process, and the user restarts ygg.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::Context;

const REPOSITORY: &str = "https://github.com/skaft-software/ygg";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/skaft-software/ygg/releases/download";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/skaft-software/ygg/releases/latest";
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RELEASE_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, serde::Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateStatus {
    Current {
        version: semver::Version,
    },
    Available {
        current: semver::Version,
        latest: semver::Version,
        url: String,
    },
}

impl std::fmt::Display for UpdateStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Current { version } => write!(formatter, "Ygg {version} is up to date."),
            Self::Available {
                current,
                latest,
                url,
            } => write!(
                formatter,
                "Ygg {latest} is available (current: {current}).\n{url}"
            ),
        }
    }
}

pub(crate) async fn check() -> anyhow::Result<UpdateStatus> {
    check_url(LATEST_RELEASE_URL, env!("CARGO_PKG_VERSION")).await
}

async fn check_url(url: &str, current: &str) -> anyhow::Result<UpdateStatus> {
    let current = semver::Version::parse(current)?;
    let client = reqwest::Client::builder()
        .connect_timeout(CHECK_TIMEOUT)
        .timeout(CHECK_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, format!("ygg/{current}"))
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RELEASE_RESPONSE_BYTES as u64)
    {
        anyhow::bail!("release metadata exceeds the {MAX_RELEASE_RESPONSE_BYTES}-byte limit");
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_RELEASE_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await? {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_RELEASE_RESPONSE_BYTES)
        {
            anyhow::bail!("release metadata exceeds the {MAX_RELEASE_RESPONSE_BYTES}-byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    let release: LatestRelease = serde_json::from_slice(&body)?;
    let latest = semver::Version::parse(release.tag_name.trim().trim_start_matches('v'))?;
    if latest > current {
        Ok(UpdateStatus::Available {
            current,
            latest,
            url: release.html_url.unwrap_or_else(|| {
                format!(
                    "https://github.com/skaft-software/ygg/releases/tag/{}",
                    release.tag_name
                )
            }),
        })
    } else {
        Ok(UpdateStatus::Current { version: current })
    }
}

/// How the running Ygg binary was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallMethod {
    /// The version-pinned installer placed the binaries under a prefix whose
    /// documentation tree is still present.
    Installer { bin_dir: PathBuf },
    /// `cargo install` from the Ygg git repository.
    Cargo,
    /// A validated global npm installation of the public launcher and a
    /// platform package.
    Npm { package_root: PathBuf },
    /// An npm package was found, but it is local or npx rather than a
    /// corroborated global installation. It must never be mutated implicitly.
    NpmLocal { package_root: PathBuf },
    /// Workspace development build running from `target/debug` or
    /// `target/release`.
    Local,
    /// The executable location does not match a supported install layout.
    Unknown,
}

/// The update command for the channel that installed the running binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateAction {
    /// Re-run the version-pinned installer for the target release.
    Installer { version: semver::Version },
    /// Reinstall the target release from the Ygg git repository.
    Cargo { version: semver::Version },
    /// Install the exact public npm package version globally without running
    /// package scripts or audit/funding network operations.
    Npm { version: semver::Version },
}

impl UpdateAction {
    /// The action for a detected install method, when that method has an
    /// automated update path.
    pub(crate) fn for_method(
        method: &InstallMethod,
        version: &semver::Version,
    ) -> Option<UpdateAction> {
        match method {
            InstallMethod::Installer { .. } => Some(Self::Installer {
                version: version.clone(),
            }),
            InstallMethod::Cargo => Some(Self::Cargo {
                version: version.clone(),
            }),
            InstallMethod::Npm { .. } => Some(Self::Npm {
                version: version.clone(),
            }),
            InstallMethod::Local | InstallMethod::NpmLocal { .. } | InstallMethod::Unknown => None,
        }
    }

    /// The exact process invocation that updates this channel.
    pub(crate) fn command_args(&self) -> (OsString, Vec<OsString>) {
        match self {
            Self::Installer { version } => (
                OsString::from("sh"),
                vec![
                    OsString::from("-c"),
                    OsString::from(install_script(&version.to_string())),
                ],
            ),
            Self::Cargo { version } => (
                OsString::from("cargo"),
                vec![
                    OsString::from("install"),
                    OsString::from("--locked"),
                    OsString::from("--git"),
                    OsString::from(REPOSITORY),
                    OsString::from("--tag"),
                    OsString::from(format!("v{version}")),
                    OsString::from("--bins"),
                    OsString::from("ygg-coding-agent"),
                ],
            ),
            Self::Npm { version } => (
                OsString::from("npm"),
                npm_command_args(&version.to_string()),
            ),
        }
    }

    /// The user-runnable form of the update command, matching the commands
    /// documented in the README.
    pub(crate) fn command_str(&self) -> String {
        match self {
            Self::Installer { version } => install_script(&version.to_string()),
            Self::Cargo { version } => format!(
                "cargo install --locked --git {REPOSITORY} --tag v{version} --bins ygg-coding-agent"
            ),
            Self::Npm { version } => npm_command_str(&version.to_string()),
        }
    }
}

const NPM_LAUNCHER: &str = "@skaft-software/ygg";
const NPM_PLATFORM_PACKAGES: [&str; 3] = [
    "@skaft-software/ygg-darwin-arm64",
    "@skaft-software/ygg-darwin-x64",
    "@skaft-software/ygg-linux-x64-gnu",
];

fn npm_command_args(version: &str) -> Vec<OsString> {
    vec![
        OsString::from("install"),
        OsString::from("--global"),
        OsString::from("--ignore-scripts"),
        OsString::from("--no-audit"),
        OsString::from("--no-fund"),
        OsString::from(format!("{NPM_LAUNCHER}@{version}")),
    ]
}

fn npm_command_str(version: &str) -> String {
    format!("npm install --global --ignore-scripts --no-audit --no-fund {NPM_LAUNCHER}@{version}")
}

/// The version-pinned installer invocation for a release, as documented in
/// the README.
fn install_script(version: &str) -> String {
    format!(
        "curl --proto '=https' --tlsv1.2 -LsSf {RELEASE_DOWNLOAD_BASE}/v{version}/install-ygg.sh | sh"
    )
}

/// Environment inputs for install-method detection, separated from process
/// state so detection is deterministic in tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InstallEnvironment {
    /// The user home directory.
    home: Option<PathBuf>,
    /// Override for the Cargo home directory.
    cargo_home: Option<PathBuf>,
    /// Override for the installer binary directory.
    install_dir: Option<PathBuf>,
    /// Override for the installer data directory.
    data_dir: Option<PathBuf>,
    /// The exact root returned by `npm root -g`; injected in tests so layout
    /// detection itself never needs to spawn a process.
    npm_root: Option<PathBuf>,
}

impl InstallEnvironment {
    pub(crate) fn current() -> Self {
        Self {
            home: dirs::home_dir().filter(|path| path.is_absolute()),
            cargo_home: std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            install_dir: std::env::var_os("YGG_INSTALL_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            data_dir: std::env::var_os("YGG_DATA_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            npm_root: None,
        }
    }
}

/// Detects how the executable at `exe` was installed.
pub(crate) fn detect_install_method(exe: &Path) -> InstallMethod {
    let mut environment = InstallEnvironment::current();
    // Preserve the existing workspace, installer, and Cargo paths without
    // spawning npm. Only a structurally valid npm layout reaches the global
    // root probe, which is the trust boundary for automatic npm updates.
    if is_workspace_build(exe) {
        return InstallMethod::Local;
    }
    let Some(bin_dir) = exe.parent().map(Path::to_path_buf) else {
        return InstallMethod::Unknown;
    };
    if installer_docs_present(&bin_dir, &environment)
        && installer_target_matches(&bin_dir, &environment)
    {
        return InstallMethod::Installer { bin_dir };
    }
    if is_cargo_bin_dir(&bin_dir, &environment) {
        return InstallMethod::Cargo;
    }
    if validated_npm_local_package(&bin_dir).is_some() {
        environment.npm_root = npm_global_root();
    }
    detect_install_method_in(exe, &environment)
}

pub(crate) fn detect_install_method_in(exe: &Path, env: &InstallEnvironment) -> InstallMethod {
    if is_workspace_build(exe) {
        return InstallMethod::Local;
    }
    let Some(bin_dir) = exe.parent().map(Path::to_path_buf) else {
        return InstallMethod::Unknown;
    };
    if installer_docs_present(&bin_dir, env) && installer_target_matches(&bin_dir, env) {
        return InstallMethod::Installer { bin_dir };
    }
    if is_cargo_bin_dir(&bin_dir, env) {
        return InstallMethod::Cargo;
    }
    if let Some(package_root) = validated_npm_global_package(&bin_dir, env) {
        return InstallMethod::Npm { package_root };
    }
    if let Some(package_root) = validated_npm_local_package(&bin_dir) {
        return InstallMethod::NpmLocal { package_root };
    }
    InstallMethod::Unknown
}

/// Ask npm for the global package root without a shell. Any malformed or
/// unsuccessful response is deliberately treated as unavailable.
fn npm_global_root() -> Option<PathBuf> {
    let output = Command::new("npm").args(["root", "-g"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout.lines();
    let root = lines.next()?.trim();
    if root.is_empty() || lines.next().is_some() {
        return None;
    }
    let root = PathBuf::from(root);
    (root.is_absolute() && real_directory(&root)).then_some(root)
}

fn expected_npm_platform() -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some((
            "@skaft-software/ygg-darwin-arm64",
            "aarch64-apple-darwin",
            "darwin",
            "arm64",
        ))
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some((
            "@skaft-software/ygg-darwin-x64",
            "x86_64-apple-darwin",
            "darwin",
            "x64",
        ))
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Some((
            "@skaft-software/ygg-linux-x64-gnu",
            "x86_64-unknown-linux-gnu",
            "linux",
            "x64",
        ))
    } else {
        None
    }
}

/// Returns true only when every component of `path` is an ordinary directory.
/// This keeps a package rooted in a symlinked parent from being mistaken for a
/// package installed at the path the user actually invoked.
fn real_directory(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return false;
        }
        current.push(component);
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn regular_path(path: &Path) -> bool {
    path.parent().is_some_and(real_directory)
        && std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

/// Validate a package resource tree without following a link or accepting a
/// special file. The tarball verifier applies the same rule before packaging;
/// keeping it here makes update-channel detection fail closed after install.
fn safe_directory_tree(path: &Path) -> bool {
    if !real_directory(path) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let entry_path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&entry_path) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
        if metadata.is_dir() {
            if !safe_directory_tree(&entry_path) {
                return false;
            }
        } else if !metadata.is_file() {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn executable_path(path: &Path) -> bool {
    regular_path(path)
        && std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable_path(_path: &Path) -> bool {
    false
}

fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn json_string_array(value: &serde_json::Value, key: &str, expected: &[&str]) -> bool {
    let Some(array) = value.get(key).and_then(|value| value.as_array()) else {
        return false;
    };
    array.len() == expected.len()
        && array
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_str() == Some(*expected))
}

fn json_string_map(value: &serde_json::Value, key: &str, expected: &[(&str, &str)]) -> bool {
    let Some(object) = value.get(key).and_then(|value| value.as_object()) else {
        return false;
    };
    object.len() == expected.len()
        && expected.iter().all(|(key, expected)| {
            object.get(*key).and_then(|value| value.as_str()) == Some(*expected)
        })
}

fn manifest_keys_exact(value: &serde_json::Value, expected: &[&str]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn npm_manifest(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn valid_npm_platform_root(
    root: &Path,
    package_name: &str,
    target: &str,
    operating_system: &str,
    cpu: &str,
) -> bool {
    let version = env!("CARGO_PKG_VERSION");
    let description = format!("Native Ygg runtime for {target}");
    if !safe_directory_tree(root)
        || !real_directory(&root.join("bin"))
        || !regular_path(&root.join("package.json"))
        || !regular_path(&root.join("README.md"))
        || !regular_path(&root.join("LICENSE"))
        || !executable_path(&root.join("bin/ygg"))
        || !executable_path(&root.join("bin/ygg-host"))
        || !real_directory(&root.join("share/ygg"))
        || !regular_path(&root.join("share/ygg/.ygg-version"))
        || !regular_path(&root.join("share/ygg/README.md"))
        || !real_directory(&root.join("share/ygg/docs"))
        || !real_directory(&root.join("share/ygg/examples"))
        || !real_directory(&root.join("share/ygg/sdk"))
    {
        return false;
    }
    let Some(manifest) = npm_manifest(&root.join("package.json")) else {
        return false;
    };
    manifest_keys_exact(
        &manifest,
        &[
            "name",
            "version",
            "description",
            "license",
            "repository",
            "os",
            "cpu",
            "files",
        ],
    ) && json_string(&manifest, "name") == Some(package_name)
        && json_string(&manifest, "version") == Some(version)
        && json_string(&manifest, "description") == Some(description.as_str())
        && json_string(&manifest, "license") == Some("MIT")
        && json_string(&manifest, "repository") == Some(REPOSITORY)
        && json_string_array(&manifest, "os", &[operating_system])
        && json_string_array(&manifest, "cpu", &[cpu])
        && json_string_array(
            &manifest,
            "files",
            &["README.md", "LICENSE", "bin/", "share/ygg/"],
        )
        && std::fs::read_to_string(root.join("share/ygg/.ygg-version"))
            .map(|contents| contents == format!("{version}\n"))
            .unwrap_or(false)
}

fn valid_npm_launcher_root(root: &Path, platform_name: &str) -> bool {
    let version = env!("CARGO_PKG_VERSION");
    if !safe_directory_tree(root)
        || !real_directory(&root.join("bin"))
        || !real_directory(&root.join("lib"))
        || !regular_path(&root.join("package.json"))
        || !regular_path(&root.join("README.md"))
        || !regular_path(&root.join("LICENSE"))
        || !executable_path(&root.join("bin/ygg"))
        || !executable_path(&root.join("bin/ygg-host"))
        || !executable_path(&root.join("lib/launch.sh"))
    {
        return false;
    }
    let Some(manifest) = npm_manifest(&root.join("package.json")) else {
        return false;
    };
    manifest_keys_exact(
        &manifest,
        &[
            "name",
            "version",
            "description",
            "license",
            "repository",
            "files",
            "bin",
            "optionalDependencies",
        ],
    ) && json_string(&manifest, "name") == Some(NPM_LAUNCHER)
        && json_string(&manifest, "version") == Some(version)
        && json_string(&manifest, "description") == Some("Native Ygg coding agent launcher")
        && json_string(&manifest, "license") == Some("MIT")
        && json_string(&manifest, "repository") == Some(REPOSITORY)
        && json_string_array(
            &manifest,
            "files",
            &["README.md", "LICENSE", "bin/", "lib/"],
        )
        && json_string_map(
            &manifest,
            "bin",
            &[("ygg", "bin/ygg"), ("ygg-host", "bin/ygg-host")],
        )
        && json_string_map(
            &manifest,
            "optionalDependencies",
            &[
                (NPM_PLATFORM_PACKAGES[0], version),
                (NPM_PLATFORM_PACKAGES[1], version),
                (NPM_PLATFORM_PACKAGES[2], version),
            ],
        )
        && NPM_PLATFORM_PACKAGES
            .iter()
            .any(|package| package.rsplit('/').next() == Some(platform_name))
}

struct NpmLayout {
    launcher_root: PathBuf,
    platform_root: PathBuf,
}

fn npm_layout(bin_dir: &Path) -> Option<NpmLayout> {
    if bin_dir.file_name()?.to_str()? != "bin" {
        return None;
    }
    let platform_root = bin_dir.parent()?.to_path_buf();
    let platform_name = platform_root.file_name()?.to_str()?;
    let scope_directory = platform_root.parent()?;
    if scope_directory.file_name()?.to_str()? != "@skaft-software"
        || !real_directory(scope_directory)
    {
        return None;
    }
    let node_modules = scope_directory.parent()?;
    if node_modules.file_name()?.to_str()? != "node_modules" || !real_directory(node_modules) {
        return None;
    }
    if !real_directory(bin_dir) || !real_directory(&platform_root) {
        return None;
    }

    let node_modules_parent = node_modules.parent()?;
    if !real_directory(node_modules_parent) {
        return None;
    }
    let (launcher_root, expected_platform) = if node_modules_parent
        .file_name()
        .and_then(|name| name.to_str())
        == Some("ygg")
    {
        let launcher_root = node_modules_parent.to_path_buf();
        let nested_node_modules = launcher_root.join("node_modules");
        if !real_directory(&nested_node_modules)
            || !same_directory(node_modules, &nested_node_modules)
        {
            return None;
        }
        (
            launcher_root.clone(),
            launcher_root
                .join("node_modules/@skaft-software")
                .join(platform_name),
        )
    } else {
        (
            node_modules.join("@skaft-software/ygg"),
            node_modules.join("@skaft-software").join(platform_name),
        )
    };
    if !real_directory(&launcher_root) || !same_directory(&platform_root, &expected_platform) {
        return None;
    }
    Some(NpmLayout {
        launcher_root,
        platform_root,
    })
}

fn validated_npm_layout(bin_dir: &Path) -> Option<PathBuf> {
    let (platform_package, target, operating_system, cpu) = expected_npm_platform()?;
    let platform_name = platform_package.rsplit('/').next()?;
    let layout = npm_layout(bin_dir)?;
    if layout
        .platform_root
        .file_name()
        .and_then(|name| name.to_str())
        != Some(platform_name)
    {
        return None;
    }
    if !valid_npm_launcher_root(&layout.launcher_root, platform_name)
        || !valid_npm_platform_root(
            &layout.platform_root,
            platform_package,
            target,
            operating_system,
            cpu,
        )
    {
        return None;
    }
    Some(layout.platform_root)
}

fn validated_npm_global_package(bin_dir: &Path, env: &InstallEnvironment) -> Option<PathBuf> {
    let package_root = validated_npm_layout(bin_dir)?;
    let npm_root = env.npm_root.as_ref()?;
    if npm_root.file_name()?.to_str()? != "node_modules" || !real_directory(npm_root) {
        return None;
    }
    let layout = npm_layout(bin_dir)?;
    let global_public = npm_root.join("@skaft-software/ygg");
    if !real_directory(&global_public) || !same_directory(&layout.launcher_root, &global_public) {
        return None;
    }
    Some(package_root)
}

fn validated_npm_local_package(bin_dir: &Path) -> Option<PathBuf> {
    validated_npm_layout(bin_dir)
}

/// The install method of the running binary.
pub(crate) fn current_install_method() -> InstallMethod {
    std::env::current_exe()
        .ok()
        .map(|exe| detect_install_method(&exe))
        .unwrap_or(InstallMethod::Unknown)
}

fn is_workspace_build(exe: &Path) -> bool {
    let components: Vec<_> = exe.iter().collect();
    components.windows(2).any(|pair| {
        pair[0].to_str() == Some("target") && matches!(pair[1].to_str(), Some("debug" | "release"))
    })
}

fn is_cargo_bin_dir(bin_dir: &Path, env: &InstallEnvironment) -> bool {
    let cargo_home = env
        .cargo_home
        .clone()
        .or_else(|| env.home.clone().map(|home| home.join(".cargo")));
    cargo_home
        .as_ref()
        .is_some_and(|home| same_directory(bin_dir, &home.join("bin")))
}

fn installer_docs_present(bin_dir: &Path, env: &InstallEnvironment) -> bool {
    let docs = env.data_dir.clone().or_else(|| {
        bin_dir
            .parent()
            .map(|prefix| prefix.join("share").join("ygg"))
    });
    docs.as_ref().is_some_and(|path| path.is_dir())
}

/// The installer only updates this binary when the directory it would write
/// to is the directory this binary lives in.
fn installer_target_matches(bin_dir: &Path, env: &InstallEnvironment) -> bool {
    let target = env
        .install_dir
        .clone()
        .or_else(|| env.home.clone().map(|home| home.join(".local").join("bin")));
    target
        .as_ref()
        .is_some_and(|target| same_directory(bin_dir, target))
}

/// Treats two directories as equal via canonicalized paths when both exist,
/// falling back to raw comparison when either side cannot be canonicalized
/// (a missing directory must not equal another missing directory).
fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(canonical_left), Ok(canonical_right)) => canonical_left == canonical_right,
        _ => left == right,
    }
}

/// Runs the `ygg update` command.
///
/// `check_only` reports the latest release and the command that would run.
/// Otherwise the update is executed for installer, Cargo, and validated global
/// npm installs; development builds, local/npx npm layouts, and unrecognized
/// install locations fail with manual instructions.
pub(crate) async fn run(check_only: bool) -> anyhow::Result<()> {
    if !check_only && cfg!(debug_assertions) {
        anyhow::bail!(
            "ygg update cannot update a debug build; install a release build of Ygg first"
        );
    }
    let status = check().await?;
    match &status {
        UpdateStatus::Current { .. } => {
            crate::output::stdout_line(status.to_string());
            Ok(())
        }
        UpdateStatus::Available {
            current, latest, ..
        } => {
            let method = current_install_method();
            let action = UpdateAction::for_method(&method, latest);
            if check_only {
                crate::output::stdout_multiline(status.to_string());
                match action {
                    Some(action) => {
                        crate::output::stdout_line(format!("To update: {}", action.command_str()))
                    }
                    None => crate::output::stdout_line(manual_update_hint(&method, latest)),
                }
                return Ok(());
            }
            let action =
                action.ok_or_else(|| anyhow::anyhow!(manual_update_hint(&method, latest)))?;
            run_update(current, latest, &action).await
        }
    }
}

/// Instructions for reaching a release when the install method has no
/// automated update path.
fn manual_update_hint(method: &InstallMethod, latest: &semver::Version) -> String {
    match method {
        InstallMethod::Local => format!(
            "this is a development build; rebuild it in the Ygg workspace, or install a release from {REPOSITORY}#install"
        ),
        InstallMethod::NpmLocal { .. } => format!(
            "this Ygg is installed in a local or npx npm layout; update that project explicitly, or install globally with:\n  {}",
            npm_command_str(&latest.to_string())
        ),
        InstallMethod::Unknown => format!(
            "could not detect how this Ygg was installed; update manually:\n  {}\nSee {REPOSITORY}#install",
            install_script(&latest.to_string())
        ),
        _ => unreachable!("methods with an update action do not need manual instructions"),
    }
}

/// Executes `action` with inherited stdio, then reports the result.
async fn run_update(
    current: &semver::Version,
    latest: &semver::Version,
    action: &UpdateAction,
) -> anyhow::Result<()> {
    crate::output::stdout_line(format!("Updating Ygg {current} to {latest}."));
    crate::output::stdout_line(action.command_str());
    let (program, args) = action.command_args();
    let status = Command::new(&program)
        .args(&args)
        .status()
        .with_context(|| format!("failed to run {}", program.to_string_lossy()))?;
    if !status.success() {
        let detail = status
            .code()
            .map(|code| format!("exit code {code}"))
            .unwrap_or_else(|| "interrupted".to_string());
        anyhow::bail!(
            "the update command failed ({detail}); run it manually to update:\n  {}",
            action.command_str()
        );
    }
    crate::output::stdout_line(format!("Ygg updated to {latest}. Restart ygg to use it."));
    if let Some(serve_version) = crate::extension_package::installed_version() {
        if serve_version != *latest {
            crate::output::stdout_line(format!(
                "Ygg Serve is still at {serve_version}. Run `ygg extension update ygg-serve` to match the new release."
            ));
        }
    }
    for extension in crate::extension_package::installed_official_bundle_ids() {
        crate::output::stdout_line(format!(
            "Run `ygg extension update {extension}` to install the bundle matching Ygg {latest}."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn reports_newer_release_without_treating_older_tags_as_updates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .and(header("user-agent", "ygg/0.1.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v0.2.0",
                "html_url": "https://example.test/ygg/v0.2.0"
            })))
            .mount(&server)
            .await;
        assert!(matches!(
            check_url(&format!("{}/latest", server.uri()), "0.1.1")
                .await
                .unwrap(),
            UpdateStatus::Available { latest, .. } if latest == semver::Version::new(0, 2, 0)
        ));

        let old = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v0.1.0-alpha",
                "html_url": null
            })))
            .mount(&old)
            .await;
        assert!(matches!(
            check_url(&format!("{}/latest", old.uri()), "0.1.1")
                .await
                .unwrap(),
            UpdateStatus::Current { .. }
        ));
    }

    #[tokio::test]
    async fn rejects_malformed_release_metadata() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "not a version"
            })))
            .mount(&server)
            .await;
        assert!(check_url(&server.uri(), "0.1.1").await.is_err());
    }

    #[tokio::test]
    async fn rejects_chunked_release_metadata_over_the_hard_limit() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let chunk = vec![b'x'; 4096];
            for _ in 0..=(MAX_RELEASE_RESPONSE_BYTES / chunk.len()) {
                if write!(stream, "{:x}\r\n", chunk.len()).is_err()
                    || stream.write_all(&chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        });

        let result = check_url(&format!("http://{address}/latest"), "0.1.1").await;
        server.join().unwrap();
        let error = result.unwrap_err();
        assert!(error.to_string().contains("65536-byte limit"), "{error:#}");
    }

    #[tokio::test]
    async fn does_not_follow_release_metadata_redirects() {
        let origin = MockServer::start().await;
        let destination = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/sink", destination.uri())),
            )
            .mount(&origin)
            .await;
        Mock::given(method("GET"))
            .and(path("/sink"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v9.0.0"
            })))
            .mount(&destination)
            .await;

        assert!(check_url(&format!("{}/latest", origin.uri()), "0.1.1")
            .await
            .is_err());
        assert!(destination.received_requests().await.unwrap().is_empty());
    }

    fn home_environment(home: &Path) -> InstallEnvironment {
        InstallEnvironment {
            home: Some(home.to_path_buf()),
            ..InstallEnvironment::default()
        }
    }

    fn create_npm_manifest(
        root: &Path,
        name: &str,
        version: &str,
        os: Option<&str>,
        cpu: Option<&str>,
    ) {
        let optional = NPM_PLATFORM_PACKAGES
            .iter()
            .map(|package| format!(r#""{package}":"{version}""#))
            .collect::<Vec<_>>()
            .join(",");
        let manifest = if let (Some(os), Some(cpu)) = (os, cpu) {
            format!(
                r#"{{"name":"{name}","version":"{version}","description":"Native Ygg runtime for {target}","license":"MIT","repository":"https://github.com/skaft-software/ygg","os":["{os}"],"cpu":["{cpu}"],"files":["README.md","LICENSE","bin/","share/ygg/"]}}"#,
                target = expected_npm_platform().unwrap().1,
            )
        } else {
            format!(
                r#"{{"name":"{name}","version":"{version}","description":"Native Ygg coding agent launcher","license":"MIT","repository":"https://github.com/skaft-software/ygg","files":["README.md","LICENSE","bin/","lib/"],"bin":{{"ygg":"bin/ygg","ygg-host":"bin/ygg-host"}},"optionalDependencies":{{{optional}}}}}"#
            )
        };
        std::fs::write(root.join("package.json"), manifest).unwrap();
    }

    fn create_npm_fixture(root: &Path) -> (PathBuf, PathBuf, String) {
        let (platform_package, _target, os, cpu) = expected_npm_platform().unwrap();
        let platform_name = platform_package.rsplit('/').next().unwrap();
        let npm_root = root.join("prefix/node_modules");
        let launcher_root = npm_root.join(NPM_LAUNCHER);
        let platform_root = launcher_root
            .join("node_modules/@skaft-software")
            .join(platform_name);
        std::fs::create_dir_all(launcher_root.join("bin")).unwrap();
        std::fs::create_dir_all(launcher_root.join("lib")).unwrap();
        create_npm_manifest(
            &launcher_root,
            NPM_LAUNCHER,
            env!("CARGO_PKG_VERSION"),
            None,
            None,
        );
        for file in ["README.md", "LICENSE"] {
            std::fs::write(launcher_root.join(file), file).unwrap();
        }
        for file in ["bin/ygg", "bin/ygg-host", "lib/launch.sh"] {
            let path = launcher_root.join(file);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        std::fs::create_dir_all(platform_root.join("bin")).unwrap();
        for directory in ["docs", "examples", "sdk"] {
            std::fs::create_dir_all(platform_root.join("share/ygg").join(directory)).unwrap();
        }
        create_npm_manifest(
            &platform_root,
            platform_package,
            env!("CARGO_PKG_VERSION"),
            Some(os),
            Some(cpu),
        );
        for file in ["README.md", "LICENSE"] {
            std::fs::write(platform_root.join(file), file).unwrap();
        }
        for file in ["bin/ygg", "bin/ygg-host"] {
            let path = platform_root.join(file);
            std::fs::write(&path, "native").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        std::fs::write(
            platform_root.join("share/ygg/.ygg-version"),
            format!("{}\n", env!("CARGO_PKG_VERSION")),
        )
        .unwrap();
        std::fs::write(platform_root.join("share/ygg/README.md"), "# Ygg\n").unwrap();
        (npm_root, platform_root, platform_name.to_owned())
    }

    fn create_dir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    #[test]
    fn detects_installer_installation_by_docs_tree_and_target() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let bin_dir = home.join(".local/bin");
        create_dir(&bin_dir);
        create_dir(&home.join(".local/share/ygg"));
        let exe = bin_dir.join("ygg");
        assert_eq!(
            detect_install_method_in(&exe, &home_environment(&home)),
            InstallMethod::Installer { bin_dir }
        );
    }

    #[test]
    fn detects_installer_installation_with_explicit_install_dir() {
        let root = tempfile::tempdir().unwrap();
        let bin_dir = root.path().join("ygg/bin");
        create_dir(&bin_dir);
        create_dir(&root.path().join("ygg/share/ygg"));
        let exe = bin_dir.join("ygg");
        let env = InstallEnvironment {
            install_dir: Some(bin_dir.clone()),
            ..InstallEnvironment::default()
        };
        assert_eq!(
            detect_install_method_in(&exe, &env),
            InstallMethod::Installer { bin_dir }
        );
    }

    #[test]
    fn refuses_installer_installation_that_the_installer_would_not_update() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let bin_dir = root.path().join("ygg/bin");
        create_dir(&bin_dir);
        create_dir(&root.path().join("ygg/share/ygg"));
        let exe = bin_dir.join("ygg");
        assert_eq!(
            detect_install_method_in(&exe, &home_environment(&home)),
            InstallMethod::Unknown
        );
    }

    #[test]
    fn detects_cargo_installation() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let bin_dir = home.join(".cargo/bin");
        create_dir(&bin_dir);
        let exe = bin_dir.join("ygg");
        assert_eq!(
            detect_install_method_in(&exe, &home_environment(&home)),
            InstallMethod::Cargo
        );

        let custom_home = root.path().join("cargo-home");
        let custom_bin = custom_home.join("bin");
        create_dir(&custom_bin);
        let env = InstallEnvironment {
            home: Some(home),
            cargo_home: Some(custom_home),
            ..InstallEnvironment::default()
        };
        assert_eq!(
            detect_install_method_in(&custom_bin.join("ygg"), &env),
            InstallMethod::Cargo
        );
    }

    #[test]
    fn detects_workspace_builds() {
        let debug = Path::new("/repo/target/debug/ygg");
        let release = Path::new("/Users/x/ygg/target/release/ygg");
        let env = InstallEnvironment::default();
        assert_eq!(detect_install_method_in(debug, &env), InstallMethod::Local);
        assert_eq!(
            detect_install_method_in(release, &env),
            InstallMethod::Local
        );
    }

    #[test]
    fn reports_unrecognized_installations() {
        let env = home_environment(Path::new("/Users/x"));
        assert_eq!(
            detect_install_method_in(Path::new("/opt/custom/ygg"), &env),
            InstallMethod::Unknown
        );
        assert_eq!(
            detect_install_method_in(Path::new("ygg"), &env),
            InstallMethod::Unknown
        );
    }

    #[test]
    fn detects_only_a_corroborated_global_npm_layout() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let (npm_root, platform_root, platform_name) = create_npm_fixture(&root_path);
        let exe = platform_root.join("bin/ygg");
        let environment = InstallEnvironment {
            npm_root: Some(npm_root.clone()),
            ..InstallEnvironment::default()
        };
        assert_eq!(
            detect_install_method_in(&exe, &environment),
            InstallMethod::Npm {
                package_root: platform_root.clone()
            }
        );

        let local_environment = InstallEnvironment::default();
        assert_eq!(
            detect_install_method_in(&exe, &local_environment),
            InstallMethod::NpmLocal {
                package_root: platform_root.clone()
            }
        );
        let wrong_root = root.path().join("other/node_modules");
        create_dir(&wrong_root);
        let wrong_environment = InstallEnvironment {
            npm_root: Some(wrong_root),
            ..InstallEnvironment::default()
        };
        assert_eq!(
            detect_install_method_in(&exe, &wrong_environment),
            InstallMethod::NpmLocal {
                package_root: platform_root
            }
        );
        assert_eq!(
            platform_name,
            expected_npm_platform()
                .unwrap()
                .0
                .rsplit('/')
                .next()
                .unwrap()
        );
    }

    #[test]
    fn rejects_npm_layout_with_wrong_platform_metadata() {
        let root = tempfile::tempdir().unwrap();
        let (npm_root, platform_root, _) = create_npm_fixture(root.path());
        let manifest_path = platform_root.join("package.json");
        let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
        manifest = manifest.replace("\"license\":\"MIT\"", "\"license\":\"GPL\"");
        std::fs::write(manifest_path, manifest).unwrap();
        let environment = InstallEnvironment {
            npm_root: Some(npm_root),
            ..InstallEnvironment::default()
        };
        assert_eq!(
            detect_install_method_in(&platform_root.join("bin/ygg"), &environment),
            InstallMethod::Unknown
        );
    }

    #[test]
    fn maps_install_methods_to_update_actions() {
        let version = "0.5.0".parse::<semver::Version>().unwrap();
        let bin_dir = PathBuf::from("/home/user/.local/bin");
        assert_eq!(
            UpdateAction::for_method(
                &InstallMethod::Installer {
                    bin_dir: bin_dir.clone()
                },
                &version
            ),
            Some(UpdateAction::Installer {
                version: version.clone()
            })
        );
        assert_eq!(
            UpdateAction::for_method(&InstallMethod::Cargo, &version),
            Some(UpdateAction::Cargo {
                version: version.clone()
            })
        );
        assert_eq!(
            UpdateAction::for_method(
                &InstallMethod::Npm {
                    package_root: PathBuf::from(
                        "/npm/lib/node_modules/@skaft-software/ygg-linux-x64-gnu"
                    ),
                },
                &version,
            ),
            Some(UpdateAction::Npm {
                version: version.clone(),
            })
        );
        assert_eq!(
            UpdateAction::for_method(
                &InstallMethod::NpmLocal {
                    package_root: bin_dir.clone(),
                },
                &version
            ),
            None
        );

        assert_eq!(
            UpdateAction::for_method(&InstallMethod::Unknown, &version),
            None
        );
    }

    #[test]
    fn renders_documented_update_commands() {
        let installer = UpdateAction::Installer {
            version: "0.5.0".parse().unwrap(),
        };
        assert_eq!(
            installer.command_str(),
            "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/skaft-software/ygg/releases/download/v0.5.0/install-ygg.sh | sh"
        );
        let (program, args) = installer.command_args();
        assert_eq!(program, OsString::from("sh"));
        assert_eq!(
            args,
            vec![
                OsString::from("-c"),
                OsString::from(installer.command_str())
            ]
        );

        let cargo = UpdateAction::Cargo {
            version: "0.5.0".parse().unwrap(),
        };
        assert_eq!(
            cargo.command_str(),
            "cargo install --locked --git https://github.com/skaft-software/ygg --tag v0.5.0 --bins ygg-coding-agent"
        );
        let (program, args) = cargo.command_args();
        assert_eq!(program, OsString::from("cargo"));
        assert_eq!(
            args,
            vec![
                OsString::from("install"),
                OsString::from("--locked"),
                OsString::from("--git"),
                OsString::from(REPOSITORY),
                OsString::from("--tag"),
                OsString::from("v0.5.0"),
                OsString::from("--bins"),
                OsString::from("ygg-coding-agent"),
            ]
        );

        let npm = UpdateAction::Npm {
            version: "0.5.0".parse().unwrap(),
        };
        assert_eq!(
            npm.command_str(),
            "npm install --global --ignore-scripts --no-audit --no-fund @skaft-software/ygg@0.5.0"
        );
        let (program, args) = npm.command_args();
        assert_eq!(program, OsString::from("npm"));
        assert_eq!(
            args,
            vec![
                OsString::from("install"),
                OsString::from("--global"),
                OsString::from("--ignore-scripts"),
                OsString::from("--no-audit"),
                OsString::from("--no-fund"),
                OsString::from("@skaft-software/ygg@0.5.0"),
            ]
        );
    }
}
