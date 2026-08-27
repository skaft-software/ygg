#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, BufReader, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use flate2::read::GzDecoder;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::extension_package::{
    acquire_lock, checksum_for_asset, download_bytes, download_file, open_archive_snapshot,
    publish_staging, sha256_open_file_bounded, sync_directory, validate_sha256, MAX_ARCHIVE_BYTES,
    MAX_CHECKSUM_BYTES,
};

pub(super) const BUNDLE_MANIFEST: &str = "extension.toml";
pub(super) const INSTALL_RECORD: &str = "install.json";
const RELEASE_REPOSITORY: &str = "https://github.com/skaft-software/ygg";
const RELEASE_CATALOG: &str = include_str!("../../../extensions/release-catalog.txt");
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_INSTALL_RECORD_BYTES: u64 = 64 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_ARCHIVE_PATH_BYTES: usize = 4096;
const MAX_ARCHIVE_PATH_COMPONENTS: usize = 64;
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXPANDED_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BundleSourceKind {
    Official,
    Local,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallRecord {
    schema_version: u32,
    id: String,
    version: String,
    api_version: String,
    requires_ygg: String,
    source_kind: BundleSourceKind,
    source: String,
    archive_sha256: String,
    installed_by_ygg: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InstalledBundle {
    pub id: String,
    pub version: String,
    pub api_version: String,
    pub requires_ygg: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InstalledBundleManifest {
    pub id: String,
    pub version: String,
    pub api_version: String,
    pub requires_ygg: String,
}

pub(super) fn official_bundle_ids() -> impl Iterator<Item = &'static str> {
    RELEASE_CATALOG
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

pub(super) fn is_official_bundle(id: &str) -> bool {
    official_bundle_ids().any(|candidate| candidate == id)
}

pub(super) async fn install_official(
    root: &Path,
    id: &str,
    replace: bool,
) -> anyhow::Result<InstalledBundleManifest> {
    validate_bundle_id(id)?;
    if !is_official_bundle(id) {
        anyhow::bail!(
            "unknown official extension bundle {id:?}; use 'ygg extension install --path ARCHIVE' for a local bundle"
        );
    }

    let ygg_version = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let asset = format!("{id}-{ygg_version}.tar.gz");
    let tag = format!("v{ygg_version}");
    let release = format!("{RELEASE_REPOSITORY}/releases/download/{tag}");
    let checksums_url = format!("{release}/SHA256SUMS");
    let archive_url = format!("{release}/{asset}");

    let checksums = download_bytes(&checksums_url, MAX_CHECKSUM_BYTES).await?;
    let checksums =
        String::from_utf8(checksums).context("official release SHA256SUMS is not valid UTF-8")?;
    let expected = checksum_for_asset(&checksums, &asset)?;

    let temporary = tempfile::Builder::new()
        .prefix("ygg-extension-download-")
        .tempdir()
        .context("cannot create temporary extension download directory")?;
    let archive = temporary.path().join(&asset);
    let actual = download_file(&archive_url, &archive, MAX_ARCHIVE_BYTES).await?;
    if actual != expected {
        anyhow::bail!("checksum mismatch for {asset}: expected {expected}, downloaded {actual}");
    }

    install_archive(
        root,
        &archive,
        &archive_url,
        BundleSourceKind::Official,
        &actual,
        replace,
        Some(id),
    )
}

pub(super) fn install_local(
    root: &Path,
    archive: &Path,
    replace: bool,
) -> anyhow::Result<InstalledBundleManifest> {
    let archive = archive
        .canonicalize()
        .with_context(|| format!("cannot resolve extension bundle {}", archive.display()))?;
    let mut archive_file = open_archive_snapshot(&archive)?;
    let digest = sha256_open_file_bounded(&mut archive_file, MAX_ARCHIVE_BYTES)?;
    let source = archive
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("extension bundle source path is not UTF-8"))?
        .to_owned();
    install_archive(
        root,
        &archive,
        &source,
        BundleSourceKind::Local,
        &digest,
        replace,
        None,
    )
}

fn install_archive(
    root: &Path,
    archive: &Path,
    source: &str,
    source_kind: BundleSourceKind,
    archive_sha256: &str,
    replace: bool,
    expected_id: Option<&str>,
) -> anyhow::Result<InstalledBundleManifest> {
    validate_sha256(archive_sha256)?;
    let mut archive_file = open_archive_snapshot(archive)?;
    let bound_digest = sha256_open_file_bounded(&mut archive_file, MAX_ARCHIVE_BYTES)?;
    if bound_digest != archive_sha256 {
        anyhow::bail!(
            "extension bundle changed before extraction: expected {archive_sha256}, found {bound_digest}"
        );
    }
    archive_file.rewind()?;

    let _lock = acquire_lock(root)?;
    let staging = tempfile::Builder::new()
        .prefix(".ygg-extension-install-")
        .tempdir_in(root)
        .context("cannot create extension staging directory")?;
    let archive_root = extract_archive_reader(&mut archive_file, staging.path())?;
    let manifest_path = staging.path().join(BUNDLE_MANIFEST);
    let manifest = ygg_agent::ExtensionManifest::load_bounded(&manifest_path, MAX_MANIFEST_BYTES)
        .with_context(|| {
        format!(
            "invalid executable-extension manifest {}",
            manifest_path.display()
        )
    })?;
    let installed = validate_bundle_manifest(&manifest, &archive_root)?;
    if let Some(expected_id) = expected_id {
        if installed.id != expected_id {
            anyhow::bail!(
                "official archive contains extension {:?}, expected {expected_id:?}",
                installed.id
            );
        }
    }
    validate_packaged_entrypoint(&manifest, staging.path())?;

    let destination = root.join(&installed.id);
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
                    "{} is already installed; run 'ygg extension update {}' for an official bundle",
                    installed.id,
                    installed.id
                );
            }
            load_install_record(&destination, &installed.id).with_context(|| {
                format!(
                    "refusing to replace unmanaged extension directory {}",
                    destination.display()
                )
            })?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if replace {
                anyhow::bail!(
                    "{} is not installed; run 'ygg extension install {}'",
                    installed.id,
                    installed.id
                );
            }
        }
        Err(error) => return Err(error).context("cannot inspect extension destination"),
    }

    write_install_record(
        staging.path(),
        &installed,
        source_kind,
        source,
        archive_sha256,
    )?;
    publish_staging(root, staging.path(), &destination, replace, &installed.id)?;
    Ok(installed)
}

fn validate_bundle_manifest(
    manifest: &ygg_agent::ExtensionManifest,
    archive_root: &str,
) -> anyhow::Result<InstalledBundleManifest> {
    validate_bundle_id(&manifest.name)?;
    if manifest.name == super::extension_package::PACKAGE_ID {
        anyhow::bail!(
            "ygg-serve is an application package and must use package.toml, not an executable-extension bundle"
        );
    }
    if manifest.name != archive_root {
        anyhow::bail!(
            "extension manifest name {:?} does not match archive directory {:?}",
            manifest.name,
            archive_root
        );
    }
    if manifest.api_version != ygg_agent::EXTENSION_API_VERSION {
        anyhow::bail!(
            "installable extension bundles require API {:?}; manifest declares {:?}",
            ygg_agent::EXTENSION_API_VERSION,
            manifest.api_version
        );
    }

    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let expected_requirement = format!("={current}");
    let requires_ygg = manifest.requires_ygg.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "installable extension manifest must declare requires_ygg = {expected_requirement:?}"
        )
    })?;
    let requirement = VersionReq::parse(requires_ygg)
        .context("extension requires_ygg is not a valid semantic version requirement")?;
    if requires_ygg != expected_requirement || !requirement.matches(&current) {
        anyhow::bail!(
            "extension requires Ygg {requires_ygg:?}; this release requires exact compatibility {expected_requirement:?}"
        );
    }

    Ok(InstalledBundleManifest {
        id: manifest.name.clone(),
        version: manifest.version.clone(),
        api_version: manifest.api_version.clone(),
        requires_ygg: requires_ygg.to_owned(),
    })
}

fn validate_packaged_entrypoint(
    manifest: &ygg_agent::ExtensionManifest,
    package: &Path,
) -> anyhow::Result<()> {
    let configured = Path::new(&manifest.entrypoint.command);
    if configured.is_absolute() {
        return Ok(());
    }
    let components = configured.components().collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!(
            "packaged extension entrypoint must be an absolute dependency, a bare command, or a portable relative path"
        );
    }

    let local = package.join(configured);
    match fs::symlink_metadata(&local) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!(
                    "packaged extension entrypoint is not a regular file: {}",
                    local.display()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    anyhow::bail!(
                        "packaged extension entrypoint is not executable: {}",
                        local.display()
                    );
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && components.len() == 1 => {
            // A missing bare command is an explicit external runtime
            // dependency resolved through PATH when the extension is started.
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => anyhow::bail!(
            "packaged extension entrypoint is missing: {}",
            local.display()
        ),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn extract_archive_reader<R: Read>(reader: R, destination: &Path) -> anyhow::Result<String> {
    let decoder = GzDecoder::new(BufReader::new(reader));
    let mut archive = tar::Archive::new(decoder);
    let mut archive_root = None::<String>;
    let mut found_root_directory = false;
    let mut found_manifest = false;
    let mut paths = BTreeSet::new();
    let mut entries = 0usize;
    let mut expanded_bytes = 0u64;

    for entry in archive.entries().context("cannot read extension bundle")? {
        entries = entries.saturating_add(1);
        if entries > MAX_ARCHIVE_ENTRIES {
            anyhow::bail!("extension bundle exceeds the {MAX_ARCHIVE_ENTRIES}-entry limit");
        }
        let mut entry = entry.context("cannot read extension bundle entry")?;
        let path = entry
            .path()
            .context("extension bundle contains an invalid path")?
            .into_owned();
        let (root, relative) = archive_member_path(&path)?;
        validate_bundle_id(&root)?;
        match &archive_root {
            Some(expected) if expected != &root => anyhow::bail!(
                "extension bundle contains multiple root directories: {expected:?} and {root:?}"
            ),
            None => archive_root = Some(root.clone()),
            _ => {}
        }

        let entry_type = entry.header().entry_type();
        if relative.as_os_str().is_empty() {
            if !entry_type.is_dir() {
                anyhow::bail!(
                    "extension bundle root {} must be a directory",
                    path.display()
                );
            }
            if entry.header().size()? != 0 {
                anyhow::bail!("extension bundle root directory has a non-zero size");
            }
            if found_root_directory {
                anyhow::bail!("extension bundle contains a duplicate root directory");
            }
            found_root_directory = true;
            continue;
        }
        if !paths.insert(relative.clone()) {
            anyhow::bail!(
                "extension bundle contains duplicate path {}",
                path.display()
            );
        }
        if relative == Path::new(INSTALL_RECORD) || relative == Path::new("package.toml") {
            anyhow::bail!(
                "extension bundle contains reserved package-manager file {}",
                path.display()
            );
        }

        let output = destination.join(&relative);
        if entry_type.is_dir() {
            if entry.header().size()? != 0 {
                anyhow::bail!(
                    "extension bundle directory {} has a non-zero size",
                    path.display()
                );
            }
            create_archive_directory(&output)?;
            continue;
        }
        if !entry_type.is_file() {
            anyhow::bail!(
                "extension bundle entry {} must be a regular file or directory",
                path.display()
            );
        }

        let size = entry.header().size()?;
        if size > MAX_ARCHIVE_ENTRY_BYTES {
            anyhow::bail!(
                "extension bundle entry {} exceeds the {MAX_ARCHIVE_ENTRY_BYTES}-byte limit",
                path.display()
            );
        }
        expanded_bytes = expanded_bytes
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("extension bundle expanded size overflow"))?;
        if expanded_bytes > MAX_EXPANDED_ARCHIVE_BYTES {
            anyhow::bail!(
                "extension bundle expands beyond the {MAX_EXPANDED_ARCHIVE_BYTES}-byte limit"
            );
        }
        if relative == Path::new(BUNDLE_MANIFEST) && size > MAX_MANIFEST_BYTES {
            anyhow::bail!("extension manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit");
        }

        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "cannot create extension bundle directory {}",
                    parent.display()
                )
            })?;
        }
        copy_archive_file(&mut entry, &output, size)?;
        if relative == Path::new(BUNDLE_MANIFEST) {
            found_manifest = true;
        }
    }

    let archive_root = archive_root.ok_or_else(|| anyhow::anyhow!("extension bundle is empty"))?;
    if !found_root_directory {
        anyhow::bail!("extension bundle must contain the {archive_root} root directory entry");
    }
    if !found_manifest {
        anyhow::bail!("extension bundle must contain {archive_root}/{BUNDLE_MANIFEST}");
    }
    sync_directory(destination);
    Ok(archive_root)
}

fn archive_member_path(path: &Path) -> anyhow::Result<(String, PathBuf)> {
    let encoded = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("extension bundle path is not UTF-8: {}", path.display()))?;
    if encoded.len() > MAX_ARCHIVE_PATH_BYTES || encoded.chars().any(char::is_control) {
        anyhow::bail!("extension bundle path is not portable: {}", path.display());
    }
    let components = path.components().collect::<Vec<_>>();
    if components.is_empty()
        || components.len() > MAX_ARCHIVE_PATH_COMPONENTS
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("extension bundle path is not portable: {}", path.display());
    }
    let root = match components[0] {
        Component::Normal(value) => value
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("extension bundle root is not UTF-8"))?
            .to_owned(),
        _ => unreachable!("non-normal path components were rejected"),
    };
    let relative = components
        .iter()
        .skip(1)
        .fold(PathBuf::new(), |mut path, component| {
            if let Component::Normal(value) = component {
                path.push(value);
            }
            path
        });
    Ok((root, relative))
}

fn create_archive_directory(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!(
            "extension bundle directory conflicts with a file: {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).with_context(|| {
                format!(
                    "cannot create extension bundle directory {}",
                    path.display()
                )
            })?;
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn copy_archive_file<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    destination: &Path,
    expected_size: u64,
) -> anyhow::Result<()> {
    let mode = entry.header().mode()?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if mode & 0o111 == 0 { 0o600 } else { 0o700 });
    }
    let mut output = options.open(destination).with_context(|| {
        format!(
            "cannot create extension bundle file {}",
            destination.display()
        )
    })?;
    let copied = io::copy(entry, &mut output)?;
    if copied != expected_size {
        anyhow::bail!(
            "extension bundle entry {} ended after {copied} of {expected_size} bytes",
            destination.display()
        );
    }
    output.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(fs::Permissions::from_mode(if mode & 0o111 == 0 {
            0o644
        } else {
            0o755
        }))?;
        output.sync_all()?;
    }
    Ok(())
}

fn write_install_record(
    package: &Path,
    manifest: &InstalledBundleManifest,
    source_kind: BundleSourceKind,
    source: &str,
    archive_sha256: &str,
) -> anyhow::Result<()> {
    validate_sha256(archive_sha256)?;
    if source.trim().is_empty() || source.chars().any(char::is_control) {
        anyhow::bail!("extension bundle source is invalid");
    }
    let record = InstallRecord {
        schema_version: 1,
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        api_version: manifest.api_version.clone(),
        requires_ygg: manifest.requires_ygg.clone(),
        source_kind,
        source: source.to_owned(),
        archive_sha256: archive_sha256.to_owned(),
        installed_by_ygg: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let mut encoded = serde_json::to_vec_pretty(&record)?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAX_INSTALL_RECORD_BYTES {
        anyhow::bail!("extension install record exceeds its byte limit");
    }
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

fn load_install_record(package: &Path, expected_id: &str) -> anyhow::Result<InstallRecord> {
    let path = package.join(INSTALL_RECORD);
    let mut file = ygg_agent::secure_fs::open_regular_file_for_read(&path)
        .with_context(|| format!("cannot open extension install record {}", path.display()))?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_INSTALL_RECORD_BYTES {
        anyhow::bail!(
            "extension install record {} exceeds the {MAX_INSTALL_RECORD_BYTES}-byte limit",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_INSTALL_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INSTALL_RECORD_BYTES {
        anyhow::bail!("extension install record grew beyond its byte limit");
    }
    let record: InstallRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid extension install record {}", path.display()))?;
    validate_install_record(&record, expected_id)?;
    Ok(record)
}

fn validate_install_record(record: &InstallRecord, expected_id: &str) -> anyhow::Result<()> {
    if record.schema_version != 1 {
        anyhow::bail!(
            "unsupported extension install record schema {}; expected 1",
            record.schema_version
        );
    }
    validate_bundle_id(&record.id)?;
    if record.id != expected_id {
        anyhow::bail!(
            "extension install record ID {:?} does not match directory {:?}",
            record.id,
            expected_id
        );
    }
    Version::parse(&record.version)
        .context("installed extension version is not semantic versioning")?;
    if record.api_version != ygg_agent::EXTENSION_API_VERSION {
        anyhow::bail!(
            "installed extension has unsupported API {:?}",
            record.api_version
        );
    }
    let installed_by_ygg = Version::parse(&record.installed_by_ygg)
        .context("installed_by_ygg is not semantic versioning")?;
    let requirement = VersionReq::parse(&record.requires_ygg)
        .context("installed extension requires_ygg is invalid")?;
    let expected_requirement = format!("={installed_by_ygg}");
    if record.requires_ygg != expected_requirement || !requirement.matches(&installed_by_ygg) {
        anyhow::bail!("installed extension Ygg requirement is not the exact installer version");
    }
    validate_sha256(&record.archive_sha256)?;
    if record.source.trim().is_empty() || record.source.chars().any(char::is_control) {
        anyhow::bail!("installed extension source is invalid");
    }
    Ok(())
}

pub(super) fn list_installed(root: &Path) -> anyhow::Result<Vec<InstalledBundle>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("cannot read extension package directory"),
    };
    let mut installed = Vec::new();
    for entry in entries {
        let entry = entry.context("cannot read extension package directory entry")?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_bundle_id(&id).is_err() || id == super::extension_package::PACKAGE_ID {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("cannot inspect extension package {id:?}"))?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        match fs::symlink_metadata(entry.path().join(INSTALL_RECORD)) {
            Ok(record_metadata)
                if record_metadata.is_file() && !record_metadata.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        let record = load_install_record(&entry.path(), &id)?;
        installed.push(InstalledBundle {
            id: record.id,
            version: record.version,
            api_version: record.api_version,
            requires_ygg: record.requires_ygg,
            source: record.source,
        });
    }
    installed.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(installed)
}

/// Managed bundles left by Ygg 0.6.0 or 0.6.1 that need the one-time 0.6.2 migration.
pub(super) fn managed_bundle_ids_for_v0_6_2_migration(root: &Path) -> anyhow::Result<Vec<String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("cannot read extension package directory"),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.context("cannot read extension package directory entry")?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_bundle_id(&id).is_err() || id == super::extension_package::PACKAGE_ID {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Ok(record) = load_install_record(&entry.path(), &id) else {
            continue;
        };
        if matches!(record.installed_by_ygg.as_str(), "0.6.0" | "0.6.1")
            && record.requires_ygg == format!("={}", record.installed_by_ygg)
        {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}

pub(super) fn installed_skill_roots(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let current = match Version::parse(env!("CARGO_PKG_VERSION")) {
        Ok(current) => current,
        Err(_) => return Vec::new(),
    };
    let mut roots = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            let package = entry.path();
            let id = entry.file_name();
            let id = id.to_str()?;
            let record = load_install_record(&package, id).ok()?;
            let compatible = VersionReq::parse(&record.requires_ygg)
                .ok()?
                .matches(&current);
            if !compatible {
                return None;
            }
            let skills = package.join("skills");
            let metadata = fs::symlink_metadata(&skills).ok()?;
            (metadata.is_dir() && !metadata.file_type().is_symlink()).then_some(skills)
        })
        .collect::<Vec<_>>();
    roots.sort();
    roots
}

pub(super) fn ensure_installed(root: &Path, id: &str) -> anyhow::Result<()> {
    validate_bundle_id(id)?;
    let package = root.join(id);
    let metadata = fs::symlink_metadata(&package)
        .with_context(|| format!("cannot inspect installed extension {}", package.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "installed extension is not a regular directory: {}",
            package.display()
        );
    }
    load_install_record(&package, id)?;
    Ok(())
}

pub(super) fn remove_installed(root: &Path, id: &str) -> anyhow::Result<()> {
    validate_bundle_id(id)?;
    let _lock = acquire_lock(root)?;
    ensure_installed(root, id)?;
    let package = root.join(id);
    let removed = root.join(format!(
        ".{id}.remove-{}-{}",
        std::process::id(),
        super::extension_package::unique_suffix()
    ));
    fs::rename(&package, &removed)
        .with_context(|| format!("cannot remove extension directory {}", package.display()))?;
    sync_directory(root);
    fs::remove_dir_all(&removed).with_context(|| {
        format!(
            "cannot delete removed extension files {}",
            removed.display()
        )
    })?;
    Ok(())
}

fn validate_bundle_id(id: &str) -> anyhow::Result<()> {
    let mut characters = id.chars();
    let first_valid = characters
        .next()
        .is_some_and(|character| character.is_ascii_lowercase());
    let rest_valid = characters.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    });
    if id.len() > 64 || !first_valid || !rest_valid {
        anyhow::bail!("invalid extension bundle ID {id:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::fs::File;

    fn manifest(id: &str) -> String {
        format!(
            "name = \"{id}\"\n\
             version = \"0.1.0\"\n\
             api_version = \"{}\"\n\
             requires_ygg = \"={}\"\n\n\
             [entrypoint]\n\
             command = \"{id}\"\n\n\
             [capabilities]\n\
             filesystem = \"none\"\n\
             process = false\n\
             network = false\n",
            ygg_agent::EXTENSION_API_VERSION,
            env!("CARGO_PKG_VERSION")
        )
    }

    fn create_bundle(directory: &Path, id: &str, body: &[u8]) -> PathBuf {
        let path = directory.join(format!("{id}.tar.gz"));
        let encoder = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append_directory(&mut archive, id);
        append_file(
            &mut archive,
            &format!("{id}/{BUNDLE_MANIFEST}"),
            manifest(id).as_bytes(),
            0o644,
        );
        append_file(&mut archive, &format!("{id}/{id}"), body, 0o755);
        append_directory(&mut archive, &format!("{id}/skills"));
        append_directory(&mut archive, &format!("{id}/skills/{id}"));
        let skill = format!("---\nid: {id}\nname: {id}\ndescription: Test skill.\n---\nTest\n");
        append_file(
            &mut archive,
            &format!("{id}/skills/{id}/SKILL.md"),
            skill.as_bytes(),
            0o644,
        );
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        path
    }

    fn append_directory<W: Write>(archive: &mut tar::Builder<W>, path: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        archive.append_data(&mut header, path, io::empty()).unwrap();
    }

    fn append_file<W: Write>(archive: &mut tar::Builder<W>, path: &str, bytes: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_mode(mode);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        archive.append_data(&mut header, path, bytes).unwrap();
    }

    #[test]
    fn catalog_is_sorted_unique_and_contains_the_first_party_portfolio() {
        let ids = official_bundle_ids().collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["ygg-browse", "ygg-mcp", "ygg-subagents", "ygg-web-search",]
        );
        for id in ids {
            validate_bundle_id(id).unwrap();
        }
    }

    #[test]
    fn local_bundle_installs_lists_updates_atomically_and_removes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("extensions");
        let first = create_bundle(directory.path(), "test-extension", b"first");
        let installed = install_local(&root, &first, false).unwrap();
        assert_eq!(installed.id, "test-extension");
        assert_eq!(
            fs::read(root.join("test-extension/test-extension")).unwrap(),
            b"first"
        );
        assert!(root.join("test-extension/install.json").is_file());
        let record = load_install_record(&root.join("test-extension"), "test-extension").unwrap();
        assert_eq!(record.id, "test-extension");
        assert_eq!(record.version, "0.1.0");
        assert_eq!(record.api_version, ygg_agent::EXTENSION_API_VERSION);
        assert_eq!(
            record.requires_ygg,
            format!("={}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(record.source_kind, BundleSourceKind::Local);
        assert_eq!(record.archive_sha256.len(), 64);
        assert!(!root.join("../config.toml").exists());
        assert_eq!(
            installed_skill_roots(&root),
            vec![root.join("test-extension/skills")]
        );

        let listed = list_installed(&root).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "test-extension");
        assert_eq!(listed[0].api_version, ygg_agent::EXTENSION_API_VERSION);
        assert!(listed[0].source.ends_with("test-extension.tar.gz"));

        fs::remove_file(&first).unwrap();
        let invalid = create_bundle(directory.path(), "test-extension", b"second");
        let mut bytes = fs::read(&invalid).unwrap();
        bytes.truncate(bytes.len() / 2);
        fs::write(&invalid, bytes).unwrap();
        assert!(install_local(&root, &invalid, true).is_err());
        assert_eq!(
            fs::read(root.join("test-extension/test-extension")).unwrap(),
            b"first"
        );

        fs::remove_file(&invalid).unwrap();
        let replacement = create_bundle(directory.path(), "test-extension", b"second");
        install_local(&root, &replacement, true).unwrap();
        assert_eq!(
            fs::read(root.join("test-extension/test-extension")).unwrap(),
            b"second"
        );

        remove_installed(&root, "test-extension").unwrap();
        assert!(!root.join("test-extension").exists());
    }

    #[test]
    fn finds_only_managed_v0_6_0_and_v0_6_1_bundles_for_v0_6_2_migration() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("extensions");
        for id in ["legacy-zero", "legacy-one", "current"] {
            let archive = create_bundle(directory.path(), id, b"runtime");
            install_local(&root, &archive, false).unwrap();
        }

        for (id, version) in [("legacy-zero", "0.6.0"), ("legacy-one", "0.6.1")] {
            let record_path = root.join(id).join("install.json");
            let mut record: serde_json::Value =
                serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
            record["installed_by_ygg"] = serde_json::json!(version);
            record["requires_ygg"] = serde_json::json!(format!("={version}"));
            fs::write(&record_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        }

        assert_eq!(
            managed_bundle_ids_for_v0_6_2_migration(&root).unwrap(),
            vec!["legacy-one", "legacy-zero"]
        );
    }

    #[test]
    fn archive_rejects_links_traversal_multiple_roots_and_api_mismatch() {
        assert!(archive_member_path(Path::new("../escape")).is_err());
        assert!(archive_member_path(Path::new("/absolute")).is_err());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.tar.gz");
        let encoder = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append_directory(&mut archive, "test-extension");
        append_directory(&mut archive, "other-extension");
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        assert!(extract_archive_reader(File::open(&path).unwrap(), &output).is_err());

        let link = directory.path().join("link.tar.gz");
        let encoder = GzEncoder::new(File::create(&link).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append_directory(&mut archive, "test-extension");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_link_name("/tmp/escape").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "test-extension/linked", std::io::empty())
            .unwrap();
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        let link_output = directory.path().join("link-output");
        fs::create_dir(&link_output).unwrap();
        assert!(extract_archive_reader(File::open(&link).unwrap(), &link_output).is_err());

        let bad_manifest = manifest("legacy-extension").replace(
            &format!("api_version = \"{}\"", ygg_agent::EXTENSION_API_VERSION),
            "api_version = \"0.1\"",
        );
        let legacy = directory.path().join("legacy.tar.gz");
        let encoder = GzEncoder::new(File::create(&legacy).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append_directory(&mut archive, "legacy-extension");
        append_file(
            &mut archive,
            "legacy-extension/extension.toml",
            bad_manifest.as_bytes(),
            0o644,
        );
        append_file(
            &mut archive,
            "legacy-extension/legacy-extension",
            b"runtime",
            0o755,
        );
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        assert!(install_local(&directory.path().join("legacy-root"), &legacy, false).is_err());
    }

    #[test]
    fn bundle_requires_exact_ygg_compatibility_and_cannot_claim_serve_id() {
        let source = manifest("test-extension");
        let parsed = ygg_agent::ExtensionManifest::parse(&source).unwrap();
        validate_bundle_manifest(&parsed, "test-extension").unwrap();

        let missing = source.replace(
            &format!("requires_ygg = \"={}\"\n", env!("CARGO_PKG_VERSION")),
            "",
        );
        let parsed = ygg_agent::ExtensionManifest::parse(&missing).unwrap();
        assert!(validate_bundle_manifest(&parsed, "test-extension").is_err());

        let serve = source.replace("test-extension", "ygg-serve");
        let parsed = ygg_agent::ExtensionManifest::parse(&serve).unwrap();
        assert!(validate_bundle_manifest(&parsed, "ygg-serve").is_err());
    }
}
