//! Explicit, bounded release update check and self-update.
//!
//! The check fetches the latest GitHub release with a short timeout, a hard
//! response-size limit, and no redirects. The update delegates to the
//! channel that installed the running binary: the version-pinned installer
//! for installer installs, or a pinned `cargo install` for Cargo installs.
//! Ygg never replaces itself in process; the channel swaps the installed
//! files under the running process, and the user restarts ygg.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
            InstallMethod::Local | InstallMethod::Unknown => None,
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
        }
    }
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
        }
    }
}

/// Detects how the executable at `exe` was installed.
pub(crate) fn detect_install_method(exe: &Path) -> InstallMethod {
    detect_install_method_in(exe, &InstallEnvironment::current())
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
    InstallMethod::Unknown
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
/// Otherwise the update is executed for installer and Cargo installs;
/// development builds and unrecognized install locations fail with manual
/// instructions.
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
            UpdateAction::for_method(&InstallMethod::Local, &version),
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
    }
}
