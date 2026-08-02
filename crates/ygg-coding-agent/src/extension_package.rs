#![allow(missing_docs)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
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

const PACKAGE_ID: &str = "ygg-serve";
const PACKAGE_MANIFEST: &str = "package.toml";
const INSTALL_RECORD: &str = "install.json";
const ENTRYPOINT: &str = "bin/ygg-serve-runtime";
const RELEASE_REPOSITORY: &str = "https://github.com/skaft-software/ygg";
const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRYPOINT_BYTES: u64 = 384 * 1024 * 1024;

#[derive(Clone, Debug, Subcommand)]
pub enum ExtensionCommand {
    /// Install an official application extension or a local package archive.
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
    /// List installed application extensions.
    List,
    /// Reinstall the release compatible with this Ygg version.
    Update { name: String },
    /// Remove an installed application extension without deleting its data.
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

struct PackageLock(File);

impl Drop for PackageLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub async fn run(command: ExtensionCommand) -> anyhow::Result<()> {
    match command {
        ExtensionCommand::Install { name, path } => {
            let root = extensions_root()?;
            let manifest = if let Some(path) = path {
                install_local(&root, &path, false)?
            } else {
                let name = name.expect("clap requires a name unless --path is present");
                validate_supported_name(&name)?;
                install_official(&root, false).await?
            };
            crate::output::stdout_line(format!(
                "Installed {} {} for {}.",
                manifest.id, manifest.version, manifest.target
            ));
            Ok(())
        }
        ExtensionCommand::List => list_installed(&extensions_root()?),
        ExtensionCommand::Update { name } => {
            validate_supported_name(&name)?;
            let root = extensions_root()?;
            ensure_package_directory(&root).with_context(|| {
                format!("{PACKAGE_ID} is not installed; run 'ygg extension install {PACKAGE_ID}'")
            })?;
            let manifest = install_official(&root, true).await?;
            crate::output::stdout_line(format!(
                "Updated {} to {} for {}.",
                manifest.id, manifest.version, manifest.target
            ));
            Ok(())
        }
        ExtensionCommand::Remove { name } => {
            validate_supported_name(&name)?;
            remove_installed(&extensions_root()?)?;
            crate::output::stdout_line(format!(
                "Removed {PACKAGE_ID}. Serve sessions and other user data were preserved."
            ));
            Ok(())
        }
    }
}

#[allow(dead_code)]
pub fn run_serve(no_open: bool, port: u16, web_root: Option<PathBuf>) -> anyhow::Result<()> {
    let root = extensions_root()?;
    let manifest = load_installed(&root).with_context(|| {
        format!("Ygg Serve is not installed; run 'ygg extension install {PACKAGE_ID}'")
    })?;
    let package_dir = root.join(PACKAGE_ID);
    let entrypoint = package_dir.join(&manifest.entrypoint.path);
    validate_entrypoint(&entrypoint, &manifest.entrypoint.sha256)?;

    let mut command = Command::new(&entrypoint);
    command.args(&manifest.entrypoint.args);
    if no_open {
        command.arg("--no-open");
    }
    command.arg("--port").arg(port.to_string());
    if let Some(web_root) = web_root {
        command.arg("--web-root").arg(web_root);
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
            "unsupported application extension {name:?}; this alpha supports only {PACKAGE_ID:?}"
        )
    }
}

fn extensions_root() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("cannot manage extensions: user home is unavailable"))?;
    Ok(home.join(".ygg").join("extensions"))
}

fn acquire_lock(root: &Path) -> anyhow::Result<PackageLock> {
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
    extract_archive(archive, staging.path())?;
    let manifest = load_manifest(&staging.path().join(PACKAGE_MANIFEST))?;
    validate_manifest(&manifest)?;
    let entrypoint = staging.path().join(&manifest.entrypoint.path);
    validate_entrypoint(&entrypoint, &manifest.entrypoint.sha256)?;
    write_install_record(staging.path(), &manifest, source, archive_sha256)?;

    publish_staging(root, staging.path(), &destination, replace)?;
    Ok(manifest)
}

fn publish_staging(
    root: &Path,
    staging: &Path,
    destination: &Path,
    replace: bool,
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

    let backup = root.join(format!(
        ".{PACKAGE_ID}.previous-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::rename(destination, &backup).with_context(|| {
        format!(
            "cannot stage installed extension {} for replacement",
            destination.display()
        )
    })?;
    if let Err(error) = fs::rename(staging, destination) {
        let restore = fs::rename(&backup, destination);
        return match restore {
            Ok(()) => Err(error).context("cannot publish extension update; previous install restored"),
            Err(restore_error) => anyhow::bail!(
                "cannot publish extension update ({error}); cannot restore previous install ({restore_error}); previous files remain at {}",
                backup.display()
            ),
        };
    }
    sync_directory(root);
    if let Err(error) = fs::remove_dir_all(&backup) {
        crate::output::stderr_line(format!(
            "warning: extension updated, but previous package cleanup failed at {}: {error}",
            backup.display()
        ));
    }
    Ok(())
}

fn extract_archive(archive: &Path, destination: &Path) -> anyhow::Result<()> {
    let file = File::open(archive)
        .with_context(|| format!("cannot open package archive {}", archive.display()))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(decoder);
    let bin = destination.join("bin");
    fs::create_dir(&bin).context("cannot create package bin directory")?;

    let mut found_root = false;
    let mut found_bin = false;
    let mut found_manifest = false;
    let mut found_entrypoint = false;
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
            "package requires Ygg {:?}; this alpha requires an exact {:?} package",
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
    if manifest.capabilities.network != "loopback"
        || !manifest.capabilities.process
        || manifest.capabilities.filesystem != "workspace"
    {
        anyhow::bail!(
            "Ygg Serve must declare network='loopback', process=true, and filesystem='workspace'"
        );
    }
    Ok(())
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
    Ok(())
}

fn list_installed(root: &Path) -> anyhow::Result<()> {
    match load_installed(root) {
        Ok(manifest) => {
            crate::output::stdout_table_line("ID\tVERSION\tTARGET");
            crate::output::stdout_table_line(format!(
                "{}\t{}\t{}",
                manifest.id, manifest.version, manifest.target
            ));
            Ok(())
        }
        Err(error) if error_chain_has_io_kind(&error, io::ErrorKind::NotFound) => {
            crate::output::stdout_line("No application extensions installed.");
            Ok(())
        }
        Err(error) => Err(error),
    }
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

async fn download_bytes(url: &str, maximum: usize) -> anyhow::Result<Vec<u8>> {
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

async fn download_file(url: &str, path: &Path, maximum: u64) -> anyhow::Result<String> {
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

fn checksum_for_asset(checksums: &str, asset: &str) -> anyhow::Result<String> {
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

fn validate_sha256(digest: &str) -> anyhow::Result<()> {
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

fn digest_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn sync_directory(path: &Path) {
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
             network = \"loopback\"\n\
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
            "https://github.com/skaft-software/ygg/releases/download/v0.3.3-alpha/SHA256SUMS",
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
