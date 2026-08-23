#![allow(missing_docs)]

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, Write};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use clap::Subcommand;
use flate2::read::GzDecoder;
use fs2::FileExt;
use futures_util::StreamExt;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) const PACKAGE_ID: &str = "ygg-serve";
const PACKAGE_MANIFEST: &str = "package.toml";
const INSTALL_RECORD: &str = "install.json";
const ENTRYPOINT: &str = "bin/ygg-serve-runtime";
const SERVE_NETWORK_CAPABILITY: &str = "loopback+explicit-n0-relay";
const RELEASE_REPOSITORY: &str = "https://github.com/skaft-software/ygg";
pub(super) const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub(super) const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRYPOINT_BYTES: u64 = 384 * 1024 * 1024;
const MAX_EXPANDED_ARCHIVE_BYTES: u64 = MAX_ENTRYPOINT_BYTES + MAX_MANIFEST_BYTES;

#[derive(Clone, Debug, Subcommand)]
pub enum ExtensionCommand {
    /// Install an official extension package or a local release archive.
    Install {
        /// Official extension package name.
        #[arg(
            value_name = "NAME",
            required_unless_present = "path",
            conflicts_with = "path"
        )]
        name: Option<String>,
        /// Install a local release archive instead of downloading one.
        #[arg(long, value_name = "ARCHIVE")]
        path: Option<PathBuf>,
    },
    /// List installed extension packages.
    List,
    /// Install the matching official release or a local replacement atomically.
    Update {
        /// Official extension package name.
        #[arg(
            value_name = "NAME",
            required_unless_present = "path",
            conflicts_with = "path"
        )]
        name: Option<String>,
        /// Update from a local release archive instead of downloading one.
        #[arg(long, value_name = "ARCHIVE")]
        path: Option<PathBuf>,
    },
    /// Remove an installed package without deleting external data.
    Remove { name: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageManifest {
    schema_version: u32,
    id: String,
    version: String,
    requires_ygg: String,
    target: String,
    entrypoint: PackageEntrypoint,
    capabilities: PackageCapabilities,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageEntrypoint {
    path: String,
    args: Vec<String>,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageCapabilities {
    network: String,
    process: bool,
    filesystem: String,
}

#[derive(Debug, Serialize)]
struct InstallRecord<'a> {
    schema_version: u32,
    id: &'a str,
    version: &'a str,
    target: &'a str,
    source: &'a str,
    archive_sha256: &'a str,
    entrypoint_sha256: &'a str,
    installed_by_ygg: &'a str,
}

pub(super) struct PackageLock(File);

impl Drop for PackageLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub async fn run(command: ExtensionCommand) -> anyhow::Result<()> {
    match command {
        ExtensionCommand::Install { name, path } => {
            let root = extensions_root()?;
            if let Some(path) = path {
                let path = resolve_local_archive_path(&path)?;
                match classify_local_archive(&path)? {
                    LocalArchiveKind::Application => {
                        let manifest = install_local(&root, &path, false)?;
                        crate::output::stdout_line(format!(
                            "Installed {} {} for {}.",
                            manifest.id, manifest.version, manifest.target
                        ));
                    }
                    LocalArchiveKind::ExecutableBundle => {
                        let manifest = crate::extension_bundle::install_local(&root, &path, false)?;
                        print_bundle_installed("Installed", &manifest);
                    }
                }
            } else {
                let name = name.expect("clap requires a name unless --path is present");
                if name == PACKAGE_ID {
                    let manifest = install_official(&root, false).await?;
                    crate::output::stdout_line(format!(
                        "Installed {} {} for {}.",
                        manifest.id, manifest.version, manifest.target
                    ));
                } else {
                    let manifest =
                        crate::extension_bundle::install_official(&root, &name, false).await?;
                    print_bundle_installed("Installed", &manifest);
                }
            }
            Ok(())
        }
        ExtensionCommand::List => list_all_installed(&extensions_root()?),
        ExtensionCommand::Update { name, path } => {
            let root = extensions_root()?;
            if let Some(path) = path {
                let path = resolve_local_archive_path(&path)?;
                match classify_local_archive(&path)? {
                    LocalArchiveKind::Application => {
                        ensure_package_directory(&root).with_context(|| {
                            format!(
                                "{PACKAGE_ID} is not installed; run 'ygg extension install --path {}'",
                                path.display()
                            )
                        })?;
                        let manifest = install_local(&root, &path, true)?;
                        crate::output::stdout_line(format!(
                            "Updated {} to {} for {}.",
                            manifest.id, manifest.version, manifest.target
                        ));
                    }
                    LocalArchiveKind::ExecutableBundle => {
                        let manifest = crate::extension_bundle::install_local(&root, &path, true)?;
                        print_bundle_installed("Updated", &manifest);
                    }
                }
            } else {
                let name = name.expect("clap requires a name unless --path is present");
                if name == PACKAGE_ID {
                    ensure_package_directory(&root).with_context(|| {
                        format!(
                            "{PACKAGE_ID} is not installed; run 'ygg extension install {PACKAGE_ID}'"
                        )
                    })?;
                    let manifest = install_official(&root, true).await?;
                    crate::output::stdout_line(format!(
                        "Updated {} to {} for {}.",
                        manifest.id, manifest.version, manifest.target
                    ));
                } else {
                    if !crate::extension_bundle::is_official_bundle(&name) {
                        anyhow::bail!(
                            "{name:?} has no official update source; use 'ygg extension update --path ARCHIVE'"
                        );
                    }
                    crate::extension_bundle::ensure_installed(&root, &name).with_context(|| {
                        format!("{name} is not installed; run 'ygg extension install {name}'")
                    })?;
                    let manifest =
                        crate::extension_bundle::install_official(&root, &name, true).await?;
                    print_bundle_installed("Updated", &manifest);
                }
            }
            Ok(())
        }
        ExtensionCommand::Remove { name } => {
            let root = extensions_root()?;
            if name == PACKAGE_ID {
                remove_installed(&root)?;
                crate::output::stdout_line(format!(
                    "Removed {PACKAGE_ID}. Serve sessions and other user data were preserved."
                ));
            } else {
                crate::extension_bundle::remove_installed(&root, &name)?;
                crate::output::stdout_line(format!(
                    "Removed {name}. Configuration and other data outside the bundle were preserved."
                ));
            }
            Ok(())
        }
    }
}

fn print_bundle_installed(
    action: &str,
    manifest: &crate::extension_bundle::InstalledBundleManifest,
) {
    crate::output::stdout_line(format!(
        "{action} {} {} (API {}, requires Ygg {}).",
        manifest.id, manifest.version, manifest.api_version, manifest.requires_ygg
    ));
    crate::output::stdout_line(
        "The extension remains disabled and untrusted until you explicitly enable and trust it.",
    );
}

#[allow(dead_code)]
pub fn run_serve(
    no_open: bool,
    port: u16,
    web_root: Option<PathBuf>,
    companion: bool,
    companion_relay: Option<String>,
) -> anyhow::Result<()> {
    match (companion, companion_relay.as_deref()) {
        (false, None) | (true, Some("n0")) => {}
        _ => anyhow::bail!("companion mode requires both --companion and --companion-relay n0"),
    }
    let root = extensions_root()?;
    let manifest = load_installed(&root).with_context(|| {
        format!("Ygg Serve is not installed; run 'ygg extension install {PACKAGE_ID}'")
    })?;
    let package_dir = root.join(PACKAGE_ID);
    let entrypoint = package_dir.join(&manifest.entrypoint.path);
    let (_entrypoint_snapshot, staged_entrypoint) =
        stage_validated_entrypoint(&entrypoint, &manifest.entrypoint.sha256)?;

    let mut command = Command::new(&staged_entrypoint);
    // This exact-version, first-party runtime replaces the launcher process. It
    // must receive the same user-controlled configuration and provider
    // credentials as a directly launched Ygg binary; the sanitized environment
    // is reserved for model-controlled tool and executable-extension children.
    command.args(&manifest.entrypoint.args);
    if no_open {
        command.arg("--no-open");
    }
    command.arg("--port").arg(port.to_string());
    if let Some(web_root) = web_root {
        command.arg("--web-root").arg(web_root);
    }
    if companion {
        command.arg("--companion");
    }
    if let Some(relay) = companion_relay {
        command.arg("--companion-relay").arg(relay);
    }
    command
        .env("YGG_EXTENSION_PACKAGE_DIR", &package_dir)
        .env("YGG_EXTENSION_PACKAGE_VERSION", &manifest.version);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let error = command.exec();
        Err(anyhow::Error::new(error).context(format!(
            "cannot launch Ygg Serve at {}",
            entrypoint.display()
        )))
    }

    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("cannot launch Ygg Serve at {}", entrypoint.display()))?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("Ygg Serve exited with {status}")
        }
    }
}

fn validate_supported_name(name: &str) -> anyhow::Result<()> {
    if name == PACKAGE_ID {
        Ok(())
    } else {
        anyhow::bail!(
            "unsupported application extension {name:?}; this release supports only {PACKAGE_ID:?}"
        )
    }
}

pub(super) fn extensions_root() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("cannot manage extensions: user home is unavailable"))?;
    let home = home
        .canonicalize()
        .with_context(|| format!("cannot resolve user home {}", home.display()))?;
    Ok(home.join(".ygg").join("extensions"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalArchiveKind {
    Application,
    ExecutableBundle,
}

fn resolve_local_archive_path(path: &Path) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("cannot resolve the current directory")?;
    resolve_local_archive_path_from(path, &cwd)
}

fn resolve_local_archive_path_from(path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    let unresolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    unresolved
        .canonicalize()
        .with_context(|| format!("cannot resolve package archive {}", path.display()))
}

fn classify_local_archive(path: &Path) -> anyhow::Result<LocalArchiveKind> {
    const MAX_CLASSIFICATION_ENTRIES: usize = 4096;

    let file = open_archive_snapshot(path)?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(decoder);
    let mut root = None::<String>;
    let mut application_manifest = false;
    let mut bundle_manifest = false;
    let mut entries = 0usize;
    let mut expanded_bytes = 0u64;

    for entry in archive
        .entries()
        .context("cannot read local extension archive")?
    {
        entries = entries.saturating_add(1);
        if entries > MAX_CLASSIFICATION_ENTRIES {
            anyhow::bail!(
                "local extension archive exceeds the {MAX_CLASSIFICATION_ENTRIES}-entry limit"
            );
        }
        let entry = entry.context("cannot read local extension archive entry")?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_dir() && !entry_type.is_file() {
            anyhow::bail!("local extension archive contains a link or special entry");
        }
        let size = entry.header().size()?;
        expanded_bytes = expanded_bytes
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("local extension archive size overflow"))?;
        if expanded_bytes > MAX_ARCHIVE_BYTES {
            anyhow::bail!(
                "local extension archive expands beyond the {MAX_ARCHIVE_BYTES}-byte classification limit"
            );
        }

        let path = entry
            .path()
            .context("local extension archive contains an invalid path")?
            .into_owned();
        let components = path.components().collect::<Vec<_>>();
        if components.is_empty()
            || components.len() > 64
            || components
                .iter()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            anyhow::bail!(
                "local extension archive path is not portable: {}",
                path.display()
            );
        }
        let archive_root = match components[0] {
            Component::Normal(value) => value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("local extension archive root is not UTF-8"))?
                .to_owned(),
            _ => unreachable!("non-normal components were rejected"),
        };
        match &root {
            Some(expected) if expected != &archive_root => {
                anyhow::bail!("local extension archive contains multiple root directories")
            }
            None => root = Some(archive_root),
            _ => {}
        }
        if components.len() != 2 || !entry_type.is_file() {
            continue;
        }
        let Component::Normal(name) = components[1] else {
            unreachable!("non-normal components were rejected")
        };
        if name == PACKAGE_MANIFEST {
            if application_manifest {
                anyhow::bail!("local extension archive contains duplicate {PACKAGE_MANIFEST}");
            }
            application_manifest = true;
        } else if name == crate::extension_bundle::BUNDLE_MANIFEST {
            if bundle_manifest {
                anyhow::bail!(
                    "local extension archive contains duplicate {}",
                    crate::extension_bundle::BUNDLE_MANIFEST
                );
            }
            bundle_manifest = true;
        }
    }

    match (application_manifest, bundle_manifest) {
        (true, false) => Ok(LocalArchiveKind::Application),
        (false, true) => Ok(LocalArchiveKind::ExecutableBundle),
        (true, true) => anyhow::bail!(
            "local extension archive cannot contain both {PACKAGE_MANIFEST} and {}",
            crate::extension_bundle::BUNDLE_MANIFEST
        ),
        (false, false) => anyhow::bail!(
            "local extension archive must contain either {PACKAGE_MANIFEST} or {}",
            crate::extension_bundle::BUNDLE_MANIFEST
        ),
    }
}

pub(super) fn acquire_lock(root: &Path) -> anyhow::Result<PackageLock> {
    fs::create_dir_all(root)
        .with_context(|| format!("cannot create extension directory {}", root.display()))?;
    let path = root.join(".package.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open extension package lock {}", path.display()))?;
    file.try_lock_exclusive().with_context(|| {
        format!(
            "another extension install, update, or removal is already running ({})",
            path.display()
        )
    })?;
    Ok(PackageLock(file))
}

async fn install_official(root: &Path, replace: bool) -> anyhow::Result<PackageManifest> {
    let version = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let target = target_triple()?;
    let asset = format!("{PACKAGE_ID}-{version}-{target}.tar.gz");
    let tag = format!("v{version}");
    let release = format!("{RELEASE_REPOSITORY}/releases/download/{tag}");
    let checksums_url = format!("{release}/SHA256SUMS");
    let archive_url = format!("{release}/{asset}");

    let checksums = download_bytes(&checksums_url, MAX_CHECKSUM_BYTES).await?;
    let checksums =
        String::from_utf8(checksums).context("official release SHA256SUMS is not valid UTF-8")?;
    let expected = checksum_for_asset(&checksums, &asset)?;

    let temporary = tempfile::Builder::new()
        .prefix("ygg-serve-download-")
        .tempdir()
        .context("cannot create temporary extension download directory")?;
    let archive = temporary.path().join(&asset);
    let actual = download_file(&archive_url, &archive, MAX_ARCHIVE_BYTES).await?;
    if actual != expected {
        anyhow::bail!("checksum mismatch for {asset}: expected {expected}, downloaded {actual}");
    }

    install_archive(root, &archive, &archive_url, &actual, replace)
}

fn install_local(root: &Path, archive: &Path, replace: bool) -> anyhow::Result<PackageManifest> {
    let archive = archive
        .canonicalize()
        .with_context(|| format!("cannot resolve package archive {}", archive.display()))?;
    let metadata = fs::metadata(&archive)
        .with_context(|| format!("cannot inspect package archive {}", archive.display()))?;
    if !metadata.is_file() {
        anyhow::bail!(
            "package archive is not a regular file: {}",
            archive.display()
        );
    }
    let digest = sha256_file_bounded(&archive, MAX_ARCHIVE_BYTES)?;
    let source = archive.to_string_lossy();
    install_archive(root, &archive, &source, &digest, replace)
}

fn install_archive(
    root: &Path,
    archive: &Path,
    source: &str,
    archive_sha256: &str,
    replace: bool,
) -> anyhow::Result<PackageManifest> {
    let mut archive_file = open_archive_snapshot(archive)?;
    let bound_digest = sha256_open_file_bounded(&mut archive_file, MAX_ARCHIVE_BYTES)?;
    if bound_digest != archive_sha256 {
        anyhow::bail!(
            "package archive changed before extraction: expected {archive_sha256}, found {bound_digest}"
        );
    }
    archive_file.rewind()?;
    let _lock = acquire_lock(root)?;
    let destination = root.join(PACKAGE_ID);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!(
                    "extension destination is not a regular directory: {}",
                    destination.display()
                );
            }
            if !replace {
                anyhow::bail!(
                    "{PACKAGE_ID} is already installed; run 'ygg extension update {PACKAGE_ID}'"
                );
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect extension destination"),
    }

    let staging = tempfile::Builder::new()
        .prefix(".ygg-serve-install-")
        .tempdir_in(root)
        .context("cannot create extension staging directory")?;
    extract_archive_reader(&mut archive_file, staging.path())?;
    let manifest = load_manifest(&staging.path().join(PACKAGE_MANIFEST))?;
    validate_manifest(&manifest)?;
    let entrypoint = staging.path().join(&manifest.entrypoint.path);
    validate_entrypoint(&entrypoint, &manifest.entrypoint.sha256)?;
    write_install_record(staging.path(), &manifest, source, archive_sha256)?;

    publish_staging(root, staging.path(), &destination, replace, PACKAGE_ID)?;
    Ok(manifest)
}

pub(super) fn publish_staging(
    root: &Path,
    staging: &Path,
    destination: &Path,
    replace: bool,
    _package_id: &str,
) -> anyhow::Result<()> {
    if !replace {
        fs::rename(staging, destination).with_context(|| {
            format!(
                "cannot publish extension from {} to {}",
                staging.display(),
                destination.display()
            )
        })?;
        sync_directory(root);
        return Ok(());
    }

    atomic_exchange_directories(staging, destination).with_context(|| {
        format!(
            "cannot atomically publish extension update from {} to {}; previous install remains active",
            staging.display(),
            destination.display()
        )
    })?;
    sync_directory(root);
    if let Err(error) = fs::remove_dir_all(staging) {
        crate::output::stderr_line(format!(
            "warning: extension updated, but previous package cleanup failed at {}: {error}",
            staging.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn atomic_exchange_directories(left: &Path, right: &Path) -> io::Result<()> {
    let left = CString::new(left.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let right = CString::new(right.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: both C strings live through the call and point to NUL-terminated paths.
    let result = unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn atomic_exchange_directories(left: &Path, right: &Path) -> io::Result<()> {
    let left = CString::new(left.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let right = CString::new(right.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: both C strings live through the call and renameat2 reads only those paths.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn atomic_exchange_directories(_left: &Path, _right: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic directory exchange is unavailable on this platform",
    ))
}

pub(super) fn open_archive_snapshot(path: &Path) -> anyhow::Result<File> {
    let file = ygg_agent::secure_fs::open_regular_file_for_read(path)
        .with_context(|| format!("cannot open package archive {}", path.display()))?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_ARCHIVE_BYTES {
        anyhow::bail!(
            "package archive {} exceeds the {MAX_ARCHIVE_BYTES}-byte limit",
            path.display()
        );
    }
    Ok(file)
}

pub(super) fn sha256_open_file_bounded(file: &mut File, maximum: u64) -> anyhow::Result<String> {
    file.rewind()?;
    let mut hasher = Sha256::new();
    let copied = io::copy(&mut Read::by_ref(file).take(maximum + 1), &mut hasher)?;
    if copied > maximum {
        anyhow::bail!("package archive exceeds the {maximum}-byte limit");
    }
    Ok(digest_hex(&hasher.finalize()))
}

#[cfg(test)]
fn extract_archive(path: &Path, destination: &Path) -> anyhow::Result<()> {
    let mut file = open_archive_snapshot(path)?;
    extract_archive_reader(&mut file, destination)
}

fn extract_archive_reader<R: Read>(reader: R, destination: &Path) -> anyhow::Result<()> {
    let decoder = GzDecoder::new(BufReader::new(reader));
    let mut archive = tar::Archive::new(decoder);
    let bin = destination.join("bin");
    fs::create_dir(&bin).context("cannot create package bin directory")?;

    let mut found_root = false;
    let mut found_bin = false;
    let mut found_manifest = false;
    let mut found_entrypoint = false;
    let mut expanded_bytes = 0u64;
    for entry in archive.entries().context("cannot read package archive")? {
        let mut entry = entry.context("cannot read package archive entry")?;
        let path = entry
            .path()
            .context("package archive contains an invalid path")?
            .into_owned();
        match archive_member(&path)? {
            ArchiveMember::RootDirectory => {
                if found_root {
                    anyhow::bail!("package archive contains duplicate {PACKAGE_ID} directory");
                }
                if !entry.header().entry_type().is_dir() {
                    anyhow::bail!(
                        "package archive entry {} must be a directory",
                        path.display()
                    );
                }
                found_root = true;
            }
            ArchiveMember::BinDirectory => {
                if found_bin {
                    anyhow::bail!("package archive contains duplicate {PACKAGE_ID}/bin directory");
                }
                if !entry.header().entry_type().is_dir() {
                    anyhow::bail!(
                        "package archive entry {} must be a directory",
                        path.display()
                    );
                }
                found_bin = true;
            }
            ArchiveMember::Manifest => {
                if found_manifest {
                    anyhow::bail!("package archive contains duplicate {PACKAGE_MANIFEST}");
                }
                require_regular_entry(&entry, &path)?;
                account_expanded_entry(&entry, &mut expanded_bytes)?;
                copy_archive_entry(
                    &mut entry,
                    &destination.join(PACKAGE_MANIFEST),
                    MAX_MANIFEST_BYTES,
                )?;
                found_manifest = true;
            }
            ArchiveMember::Entrypoint => {
                if found_entrypoint {
                    anyhow::bail!("package archive contains duplicate {ENTRYPOINT}");
                }
                require_regular_entry(&entry, &path)?;
                account_expanded_entry(&entry, &mut expanded_bytes)?;
                copy_archive_entry(
                    &mut entry,
                    &destination.join(ENTRYPOINT),
                    MAX_ENTRYPOINT_BYTES,
                )?;
                found_entrypoint = true;
            }
        }
    }
    if !found_manifest || !found_entrypoint {
        anyhow::bail!(
            "package archive must contain {PACKAGE_ID}/{PACKAGE_MANIFEST} and {PACKAGE_ID}/{ENTRYPOINT}"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(
            destination.join(ENTRYPOINT),
            fs::Permissions::from_mode(0o755),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ArchiveMember {
    RootDirectory,
    BinDirectory,
    Manifest,
    Entrypoint,
}

fn archive_member(path: &Path) -> anyhow::Result<ArchiveMember> {
    let components = path.components().collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("package archive path is not portable: {}", path.display());
    }
    let names = components
        .iter()
        .map(|component| match component {
            Component::Normal(name) => name.to_string_lossy(),
            _ => unreachable!("non-normal components were rejected"),
        })
        .collect::<Vec<_>>();
    match names.as_slice() {
        [root] if root == PACKAGE_ID => Ok(ArchiveMember::RootDirectory),
        [root, bin] if root == PACKAGE_ID && bin == "bin" => Ok(ArchiveMember::BinDirectory),
        [root, manifest] if root == PACKAGE_ID && manifest == PACKAGE_MANIFEST => {
            Ok(ArchiveMember::Manifest)
        }
        [root, bin, executable]
            if root == PACKAGE_ID && bin == "bin" && executable == "ygg-serve-runtime" =>
        {
            Ok(ArchiveMember::Entrypoint)
        }
        _ => anyhow::bail!("unexpected package archive entry: {}", path.display()),
    }
}

fn require_regular_entry<R: Read>(entry: &tar::Entry<'_, R>, path: &Path) -> anyhow::Result<()> {
    if !entry.header().entry_type().is_file() {
        anyhow::bail!(
            "package archive entry {} must be a regular file",
            path.display()
        );
    }
    Ok(())
}

fn account_expanded_entry<R: Read>(
    entry: &tar::Entry<'_, R>,
    expanded_bytes: &mut u64,
) -> anyhow::Result<()> {
    let size = entry.header().size()?;
    *expanded_bytes = expanded_bytes
        .checked_add(size)
        .ok_or_else(|| anyhow::anyhow!("package archive expanded size overflow"))?;
    if *expanded_bytes > MAX_EXPANDED_ARCHIVE_BYTES {
        anyhow::bail!("package archive expands beyond the {MAX_EXPANDED_ARCHIVE_BYTES}-byte limit");
    }
    Ok(())
}

fn copy_archive_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    destination: &Path,
    maximum: u64,
) -> anyhow::Result<()> {
    let size = entry.header().size()?;
    if size > maximum {
        anyhow::bail!(
            "package archive entry {} exceeds the {maximum}-byte limit",
            destination.display()
        );
    }
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .with_context(|| format!("cannot create package file {}", destination.display()))?;
    let copied = io::copy(entry, &mut output)?;
    if copied != size {
        anyhow::bail!(
            "package archive entry {} ended after {copied} of {size} bytes",
            destination.display()
        );
    }
    output.sync_all()?;
    Ok(())
}

fn ensure_package_directory(root: &Path) -> anyhow::Result<PathBuf> {
    let package = root.join(PACKAGE_ID);
    let metadata = fs::symlink_metadata(&package)
        .with_context(|| format!("cannot inspect installed extension {}", package.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "installed extension is not a regular directory: {}",
            package.display()
        );
    }
    Ok(package)
}

fn load_installed(root: &Path) -> anyhow::Result<PackageManifest> {
    let package = ensure_package_directory(root)?;
    let manifest = load_manifest(&package.join(PACKAGE_MANIFEST))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// The version of the installed Ygg Serve extension, if any, read without
/// validating it against this binary. A fresh update can leave the
/// extension stale, and validation would fail in exactly that case.
pub(crate) fn installed_version() -> Option<Version> {
    let root = extensions_root().ok()?;
    let package = ensure_package_directory(&root).ok()?;
    let manifest = load_manifest(&package.join(PACKAGE_MANIFEST)).ok()?;
    Version::parse(&manifest.version).ok()
}

/// Managed executable bundles that can be refreshed from the official catalog.
pub(crate) fn installed_official_bundle_ids() -> Vec<String> {
    let Ok(root) = extensions_root() else {
        return Vec::new();
    };
    crate::extension_bundle::list_installed(&root)
        .unwrap_or_default()
        .into_iter()
        .filter(|bundle| crate::extension_bundle::is_official_bundle(&bundle.id))
        .map(|bundle| bundle.id)
        .collect()
}

fn load_manifest(path: &Path) -> anyhow::Result<PackageManifest> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect package manifest {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("package manifest is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        anyhow::bail!(
            "package manifest {} exceeds the {MAX_MANIFEST_BYTES}-byte limit",
            path.display()
        );
    }
    let source = fs::read_to_string(path)
        .with_context(|| format!("cannot read package manifest {}", path.display()))?;
    toml::from_str(&source).with_context(|| format!("invalid package manifest {}", path.display()))
}

fn validate_manifest(manifest: &PackageManifest) -> anyhow::Result<()> {
    if manifest.schema_version != 1 {
        anyhow::bail!(
            "unsupported package manifest schema {}; expected 1",
            manifest.schema_version
        );
    }
    validate_supported_name(&manifest.id)?;

    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let package_version = Version::parse(&manifest.version)
        .context("package version is not valid semantic versioning")?;
    if package_version != current {
        anyhow::bail!(
            "package version {package_version} is incompatible with Ygg {current}; install the matching release"
        );
    }
    let expected_requirement = format!("={current}");
    let requirement = VersionReq::parse(&manifest.requires_ygg)
        .context("package requires_ygg is not a valid semantic version requirement")?;
    if manifest.requires_ygg != expected_requirement || !requirement.matches(&current) {
        anyhow::bail!(
            "package requires Ygg {:?}; this release requires an exact {:?} package",
            manifest.requires_ygg,
            expected_requirement
        );
    }

    let target = target_triple()?;
    if manifest.target != target {
        anyhow::bail!(
            "package target {:?} does not match this Ygg binary ({target})",
            manifest.target
        );
    }
    if manifest.entrypoint.path != ENTRYPOINT || manifest.entrypoint.args != ["serve"] {
        anyhow::bail!("package entrypoint must be {ENTRYPOINT} with the single argument 'serve'");
    }
    validate_sha256(&manifest.entrypoint.sha256)?;
    if manifest.capabilities.network != SERVE_NETWORK_CAPABILITY
        || !manifest.capabilities.process
        || manifest.capabilities.filesystem != "workspace"
    {
        anyhow::bail!(
            "Ygg Serve must declare network='{SERVE_NETWORK_CAPABILITY}', process=true, and filesystem='workspace'"
        );
    }
    Ok(())
}

fn stage_validated_entrypoint(
    path: &Path,
    expected_sha256: &str,
) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
    let mut source = ygg_agent::secure_fs::open_regular_file_for_read(path)
        .with_context(|| format!("cannot open package entrypoint {}", path.display()))?;
    let metadata = source.metadata()?;
    if metadata.len() > MAX_ENTRYPOINT_BYTES {
        anyhow::bail!(
            "package entrypoint {} exceeds the {MAX_ENTRYPOINT_BYTES}-byte limit",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            anyhow::bail!("package entrypoint is not executable: {}", path.display());
        }
    }

    let temporary = tempfile::Builder::new()
        .prefix("ygg-package-entrypoint-")
        .tempdir()
        .context("cannot create private package entrypoint snapshot")?;
    let staged = temporary.path().join("ygg-serve-runtime");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o700);
    }
    let mut destination = options.open(&staged)?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("package entrypoint size overflow"))?;
        if copied > MAX_ENTRYPOINT_BYTES {
            anyhow::bail!("package entrypoint grew beyond its byte limit");
        }
        hasher.update(&buffer[..read]);
        destination.write_all(&buffer[..read])?;
    }
    let actual = digest_hex(hasher.finalize().as_slice());
    if actual != expected_sha256 {
        anyhow::bail!(
            "package entrypoint checksum mismatch: expected {expected_sha256}, found {actual}"
        );
    }
    destination.flush()?;
    destination.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        destination.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        destination.sync_all()?;
    }
    Ok((temporary, staged))
}

fn validate_entrypoint(path: &Path, expected_sha256: &str) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect package entrypoint {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "package entrypoint is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_ENTRYPOINT_BYTES {
        anyhow::bail!(
            "package entrypoint {} exceeds the {MAX_ENTRYPOINT_BYTES}-byte limit",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o111 == 0 {
            anyhow::bail!("package entrypoint is not executable: {}", path.display());
        }
    }
    let actual = sha256_file_bounded(path, MAX_ENTRYPOINT_BYTES)?;
    if actual != expected_sha256 {
        anyhow::bail!(
            "package entrypoint checksum mismatch: expected {expected_sha256}, found {actual}"
        );
    }
    Ok(())
}

fn write_install_record(
    package: &Path,
    manifest: &PackageManifest,
    source: &str,
    archive_sha256: &str,
) -> anyhow::Result<()> {
    validate_sha256(archive_sha256)?;
    let record = InstallRecord {
        schema_version: 1,
        id: &manifest.id,
        version: &manifest.version,
        target: &manifest.target,
        source,
        archive_sha256,
        entrypoint_sha256: &manifest.entrypoint.sha256,
        installed_by_ygg: env!("CARGO_PKG_VERSION"),
    };
    let mut encoded = serde_json::to_vec_pretty(&record)?;
    encoded.push(b'\n');
    let path = package.join(INSTALL_RECORD);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot create install record {}", path.display()))?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    sync_directory(package);
    Ok(())
}

fn list_all_installed(root: &Path) -> anyhow::Result<()> {
    let mut rows = Vec::<(String, String, String, String, String, String)>::new();
    match load_installed(root) {
        Ok(manifest) => rows.push((
            manifest.id,
            manifest.version,
            "application".to_owned(),
            "-".to_owned(),
            manifest.requires_ygg,
            manifest.target,
        )),
        Err(error) if error_chain_has_io_kind(&error, io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    for bundle in crate::extension_bundle::list_installed(root)? {
        rows.push((
            bundle.id,
            bundle.version,
            "executable".to_owned(),
            bundle.api_version,
            bundle.requires_ygg,
            "any".to_owned(),
        ));
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));

    if rows.is_empty() {
        crate::output::stdout_line("No extension packages installed.");
        return Ok(());
    }
    crate::output::stdout_table_line("ID\tVERSION\tKIND\tAPI\tYGG\tTARGET");
    for (id, version, kind, api, ygg, target) in rows {
        crate::output::stdout_table_line(format!(
            "{id}\t{version}\t{kind}\t{api}\t{ygg}\t{target}"
        ));
    }
    Ok(())
}

fn remove_installed(root: &Path) -> anyhow::Result<()> {
    let _lock = acquire_lock(root)?;
    let package = ensure_package_directory(root)?;
    let removed = root.join(format!(
        ".{PACKAGE_ID}.remove-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::rename(&package, &removed)
        .with_context(|| format!("cannot remove extension directory {}", package.display()))?;
    sync_directory(root);
    fs::remove_dir_all(&removed)
        .with_context(|| format!("cannot delete removed package files {}", removed.display()))?;
    Ok(())
}

fn error_chain_has_io_kind(error: &anyhow::Error, kind: io::ErrorKind) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|error| error.kind() == kind)
}

fn target_triple() -> anyhow::Result<&'static str> {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Ok("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else {
        anyhow::bail!(
            "Ygg Serve has no v{} package for {}/{}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }
}

pub(super) async fn download_bytes(url: &str, maximum: usize) -> anyhow::Result<Vec<u8>> {
    let response = send_download(url).await?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        anyhow::bail!("download exceeds the {maximum}-byte limit: {url}");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("cannot download {url}"))?;
        if bytes.len().saturating_add(chunk.len()) > maximum {
            anyhow::bail!("download exceeds the {maximum}-byte limit: {url}");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) async fn download_file(url: &str, path: &Path, maximum: u64) -> anyhow::Result<String> {
    let response = send_download(url).await?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum)
    {
        anyhow::bail!("download exceeds the {maximum}-byte limit: {url}");
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot create extension download {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("cannot download {url}"))?;
        downloaded = downloaded
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| anyhow::anyhow!("download size overflow for {url}"))?;
        if downloaded > maximum {
            anyhow::bail!("download exceeds the {maximum}-byte limit: {url}");
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    Ok(digest_hex(hasher.finalize().as_slice()))
}

fn is_trusted_release_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some("github.com" | "release-assets.githubusercontent.com")
        )
}

async fn send_download(url: &str) -> anyhow::Result<reqwest::Response> {
    let url = reqwest::Url::parse(url).context("invalid release download URL")?;
    if !is_trusted_release_url(&url) {
        anyhow::bail!("refusing untrusted release URL: {url}");
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("ygg/{}", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if is_trusted_release_url(attempt.url()) {
                reqwest::redirect::Policy::default().redirect(attempt)
            } else {
                let url = attempt.url().clone();
                attempt.error(io::Error::other(format!(
                    "refusing untrusted release redirect to {url}"
                )))
            }
        }))
        .build()?;
    let response = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?;
    if !is_trusted_release_url(response.url()) {
        anyhow::bail!("refusing untrusted release redirect to {}", response.url());
    }
    response
        .error_for_status()
        .with_context(|| format!("release download failed: {url}"))
}

pub(super) fn checksum_for_asset(checksums: &str, asset: &str) -> anyhow::Result<String> {
    let mut found = None;
    for line in checksums.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 {
            anyhow::bail!("invalid SHA256SUMS line: {line:?}");
        }
        let digest = fields[0].to_ascii_lowercase();
        validate_sha256(&digest)?;
        let name = fields[1]
            .strip_prefix('*')
            .unwrap_or(fields[1])
            .strip_prefix("./")
            .unwrap_or(fields[1].strip_prefix('*').unwrap_or(fields[1]));
        if name == asset && found.replace(digest).is_some() {
            anyhow::bail!("SHA256SUMS contains duplicate entries for {asset}");
        }
    }
    found.ok_or_else(|| anyhow::anyhow!("SHA256SUMS does not contain {asset}"))
}

pub(super) fn validate_sha256(digest: &str) -> anyhow::Result<()> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        anyhow::bail!("invalid lowercase SHA-256 digest {digest:?}")
    }
}

fn sha256_file_bounded(path: &Path, maximum: u64) -> anyhow::Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        anyhow::bail!("not a regular file: {}", path.display());
    }
    if metadata.len() > maximum {
        anyhow::bail!("{} exceeds the {maximum}-byte limit", path.display());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("file size overflow for {}", path.display()))?;
        if total > maximum {
            anyhow::bail!("{} exceeds the {maximum}-byte limit", path.display());
        }
        hasher.update(&buffer[..read]);
    }
    Ok(digest_hex(hasher.finalize().as_slice()))
}

pub(super) fn digest_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(super) fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(super) fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    fn package_manifest(binary: &[u8]) -> String {
        let digest = digest_hex(Sha256::digest(binary).as_slice());
        format!(
            "schema_version = 1\n\
             id = \"ygg-serve\"\n\
             version = \"{}\"\n\
             requires_ygg = \"={}\"\n\
             target = \"{}\"\n\n\
             [entrypoint]\n\
             path = \"bin/ygg-serve-runtime\"\n\
             args = [\"serve\"]\n\
             sha256 = \"{digest}\"\n\n\
             [capabilities]\n\
             network = \"{SERVE_NETWORK_CAPABILITY}\"\n\
             process = true\n\
             filesystem = \"workspace\"\n",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_VERSION"),
            target_triple().unwrap()
        )
    }

    fn create_package(directory: &Path, binary: &[u8]) -> PathBuf {
        let path = directory.join("package.tar.gz");
        let file = File::create(&path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append(
            &mut archive,
            PACKAGE_MANIFEST,
            package_manifest(binary).as_bytes(),
        );
        append(&mut archive, ENTRYPOINT, binary);
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        path
    }

    fn append<W: Write>(archive: &mut tar::Builder<W>, relative: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("{PACKAGE_ID}/{relative}"), bytes)
            .unwrap();
    }

    fn append_directory<W: Write>(archive: &mut tar::Builder<W>, relative: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_data(&mut header, relative, std::io::empty())
            .unwrap();
    }

    #[test]
    fn checksum_parser_accepts_release_tool_spelling() {
        let digest = "a".repeat(64);
        let sums = format!("{digest}  ./other.tar.gz\n{digest}  *wanted.tar.gz\n");
        assert_eq!(checksum_for_asset(&sums, "wanted.tar.gz").unwrap(), digest);
    }

    #[test]
    fn checksum_parser_rejects_duplicates() {
        let digest = "b".repeat(64);
        let sums = format!("{digest}  wanted.tar.gz\n{digest}  ./wanted.tar.gz\n");
        assert!(checksum_for_asset(&sums, "wanted.tar.gz").is_err());
    }

    #[test]
    fn official_downloads_only_trust_github_https_hosts() {
        for accepted in [
            "https://github.com/skaft-software/ygg/releases/download/v0.5.0/SHA256SUMS",
            "https://release-assets.githubusercontent.com/github-production-release-asset/file?token=signed",
        ] {
            assert!(is_trusted_release_url(
                &reqwest::Url::parse(accepted).unwrap()
            ));
        }

        for rejected in [
            "http://github.com/skaft-software/ygg/releases/download/file",
            "https://github.com.example.com/file",
            "https://raw.githubusercontent.com/skaft-software/ygg/main/file",
            "https://github.com:8443/file",
        ] {
            assert!(!is_trusted_release_url(
                &reqwest::Url::parse(rejected).unwrap()
            ));
        }
    }

    #[test]
    fn local_archive_classifier_keeps_application_and_bundle_formats_distinct() {
        let directory = tempfile::tempdir().unwrap();
        let application = create_package(directory.path(), b"runtime");
        assert_eq!(
            resolve_local_archive_path_from(Path::new("package.tar.gz"), directory.path()).unwrap(),
            application.canonicalize().unwrap()
        );
        #[cfg(unix)]
        {
            let linked_parent = directory.path().join("linked-parent");
            std::os::unix::fs::symlink(directory.path(), &linked_parent).unwrap();
            assert_eq!(
                resolve_local_archive_path_from(
                    &linked_parent.join("package.tar.gz"),
                    directory.path()
                )
                .unwrap(),
                application.canonicalize().unwrap()
            );
        }
        assert_eq!(
            classify_local_archive(&application).unwrap(),
            LocalArchiveKind::Application
        );

        let bundle = directory.path().join("bundle.tar.gz");
        let encoder = GzEncoder::new(File::create(&bundle).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append_directory(&mut archive, "example");
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_data(&mut header, "example/extension.toml", std::io::empty())
            .unwrap();
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        assert_eq!(
            classify_local_archive(&bundle).unwrap(),
            LocalArchiveKind::ExecutableBundle
        );
    }

    #[test]
    fn local_archive_installs_expected_shape_and_can_be_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("extensions");
        let first = create_package(directory.path(), b"first runtime");
        let digest = sha256_file_bounded(&first, MAX_ARCHIVE_BYTES).unwrap();
        let manifest = install_archive(&root, &first, "test", &digest, false).unwrap();
        assert_eq!(manifest.id, PACKAGE_ID);
        assert!(root.join(PACKAGE_ID).join(PACKAGE_MANIFEST).is_file());
        assert!(root.join(PACKAGE_ID).join(INSTALL_RECORD).is_file());
        assert_eq!(
            fs::read(root.join(PACKAGE_ID).join(ENTRYPOINT)).unwrap(),
            b"first runtime"
        );
        assert!(install_archive(&root, &first, "test", &digest, false).is_err());

        fs::write(
            root.join(PACKAGE_ID).join(PACKAGE_MANIFEST),
            "damaged = [\n",
        )
        .unwrap();
        fs::remove_file(&first).unwrap();
        let second = create_package(directory.path(), b"second runtime");
        let digest = sha256_file_bounded(&second, MAX_ARCHIVE_BYTES).unwrap();
        install_archive(&root, &second, "test", &digest, true).unwrap();
        assert_eq!(
            fs::read(root.join(PACKAGE_ID).join(ENTRYPOINT)).unwrap(),
            b"second runtime"
        );

        fs::write(
            root.join(PACKAGE_ID).join(PACKAGE_MANIFEST),
            "damaged = [\n",
        )
        .unwrap();
        remove_installed(&root).unwrap();
        assert!(!root.join(PACKAGE_ID).exists());
    }

    #[test]
    fn archive_rejects_unexpected_and_nonportable_members() {
        assert!(archive_member(Path::new("../escape")).is_err());
        assert!(archive_member(Path::new("/absolute")).is_err());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.tar.gz");
        let encoder = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append(&mut archive, "extra", b"bad");
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        assert!(extract_archive(&path, &output).is_err());
    }

    #[test]
    fn archive_rejects_duplicate_directories() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("duplicate.tar.gz");
        let encoder = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append_directory(&mut archive, PACKAGE_ID);
        append_directory(&mut archive, PACKAGE_ID);
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        assert!(extract_archive(&path, &output).is_err());
    }

    #[test]
    fn incompatible_manifest_is_rejected() {
        let manifest: PackageManifest = toml::from_str(&package_manifest(b"runtime")).unwrap();
        validate_manifest(&manifest).unwrap();

        let incompatible = package_manifest(b"runtime").replace(
            &format!("requires_ygg = \"={}\"", env!("CARGO_PKG_VERSION")),
            "requires_ygg = \">=0.1.0\"",
        );
        let manifest: PackageManifest = toml::from_str(&incompatible).unwrap();
        assert!(validate_manifest(&manifest).is_err());

        let undeclared_wan = package_manifest(b"runtime").replace(
            &format!("network = \"{SERVE_NETWORK_CAPABILITY}\""),
            "network = \"loopback\"",
        );
        let manifest: PackageManifest = toml::from_str(&undeclared_wan).unwrap();
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn removal_does_not_touch_data_outside_the_package() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("extensions");
        let archive = create_package(directory.path(), b"runtime");
        let digest = sha256_file_bounded(&archive, MAX_ARCHIVE_BYTES).unwrap();
        install_archive(&root, &archive, "test", &digest, false).unwrap();
        let data = directory.path().join("serve-data");
        fs::write(&data, "keep").unwrap();

        remove_installed(&root).unwrap();

        assert!(!root.join(PACKAGE_ID).exists());
        assert_eq!(fs::read_to_string(data).unwrap(), "keep");
    }
}
