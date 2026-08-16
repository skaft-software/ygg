//! Descriptor-bound, bounded local-file operations.
//!
//! On Unix, path components are opened one at a time with `O_NOFOLLOW`; on
//! Windows they are opened relative to already-authorized directory handles
//! with reparse-point traversal disabled. Mutations stay bound to those parent
//! handles, and private Windows objects use a protected current-user-only ACL.
//! Platforms without descriptor-relative primitives fail closed.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Failures produced by bounded descriptor-based file access.
#[derive(Debug, thiserror::Error)]
pub enum SecureFileError {
    /// The path shape cannot identify a normal file.
    #[error("invalid file path: {0}")]
    InvalidPath(String),
    /// The opened object is not a regular file.
    #[error("not a regular file")]
    NotRegular,
    /// A secret-bearing file or directory does not have owner-only identity
    /// and permissions.
    #[error("private filesystem object is not owner-only: {0}")]
    InsecurePrivateObject(String),
    /// Reading one regular file crossed the supplied hard byte limit.
    #[error("file is too large to read ({actual} bytes, limit {limit})")]
    TooLarge {
        /// Bytes observed, or the minimum known size when a stream crossed the cap.
        actual: u64,
        /// Configured maximum bytes.
        limit: usize,
    },
    /// The target changed between inspection and commit.
    #[error("file changed while the operation was in progress")]
    Changed,
    /// Cooperative cancellation won before the rename commit point.
    #[error("file operation cancelled")]
    Cancelled,
    /// The platform or filesystem cannot atomically replace an existing target
    /// while preserving compare-and-swap semantics.
    #[error("atomic conditional file replacement is unavailable")]
    PublicationUnavailable,
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn validate_absolute_file_path(path: &Path) -> Result<(), SecureFileError> {
    if !path.is_absolute() {
        return Err(SecureFileError::InvalidPath(format!(
            "{} is not absolute",
            path.display()
        )));
    }
    let mut normal = 0usize;
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {}
            Component::Normal(_) => normal += 1,
            Component::CurDir | Component::ParentDir => {
                return Err(SecureFileError::InvalidPath(path.display().to_string()))
            }
        }
    }
    if normal == 0 {
        return Err(SecureFileError::InvalidPath(path.display().to_string()));
    }
    Ok(())
}

const INSPECTION_BYTES: usize = 512;
const TEMP_NAME_ATTEMPTS: usize = 128;

fn random_temp_suffix() -> Result<String, SecureFileError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        SecureFileError::Io(std::io::Error::other(format!(
            "secure random generation failed: {error}"
        )))
    })?;
    let mut suffix = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        suffix.push(char::from(HEX[usize::from(byte >> 4)]));
        suffix.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(suffix)
}

fn read_open_regular_bounded_by(
    mut file: std::fs::File,
    upper_limit: usize,
    byte_limit: &dyn Fn(&[u8]) -> usize,
) -> Result<Vec<u8>, SecureFileError> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(SecureFileError::NotRegular);
    }
    if metadata.len() > upper_limit as u64 {
        return Err(SecureFileError::TooLarge {
            actual: metadata.len(),
            limit: upper_limit,
        });
    }

    // Inspect a fixed-size prefix before reserving for the complete file. This
    // lets callers apply a tighter content-derived cap without first buffering
    // up to the more permissive fallback limit.
    let prefix_len = (metadata.len() as usize)
        .min(upper_limit)
        .min(INSPECTION_BYTES);
    let mut bytes = Vec::with_capacity(prefix_len);
    Read::by_ref(&mut file)
        .take(prefix_len as u64)
        .read_to_end(&mut bytes)?;
    let limit = byte_limit(&bytes).min(upper_limit);
    if metadata.len() > limit as u64 {
        return Err(SecureFileError::TooLarge {
            actual: metadata.len(),
            limit,
        });
    }

    bytes.reserve((metadata.len() as usize).saturating_sub(bytes.len()));
    let remaining_limit = limit.saturating_add(1).saturating_sub(bytes.len());
    Read::by_ref(&mut file)
        .take(remaining_limit as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(SecureFileError::TooLarge {
            actual: bytes.len() as u64,
            limit,
        });
    }
    Ok(bytes)
}

fn read_open_regular(file: std::fs::File, limit: usize) -> Result<Vec<u8>, SecureFileError> {
    read_open_regular_bounded_by(file, limit, &|_| limit)
}

/// Read exactly one regular file, rejecting symlinks and special files and
/// enforcing the byte limit on bytes actually read rather than metadata alone.
///
/// `path` must be absolute. On Unix and Windows every component is opened
/// relative to the previously opened directory handle, so parent replacement
/// cannot redirect the read after validation.
pub fn read_regular_file_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::read_regular_file_bounded(path, limit)
}

/// Remove one existing regular file through a descriptor-bound path walk.
///
/// Returns `true` if a file was removed and `false` when it was already
/// absent. Symbolic links and special files are rejected. On Unix, the final
/// name is atomically moved to a private random name and revalidated before it
/// is unlinked, so a replacement cannot be removed by cleanup.
pub fn remove_regular_file_if_exists(path: &Path) -> Result<bool, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::remove_regular_file_if_exists(path)
}

/// Read one owner-only regular file through a descriptor-bound path walk.
///
/// In addition to rejecting symbolic links and special files, this requires
/// the file to be owned by the current user, to have no additional hard links,
/// and to expose no access outside the owner security boundary.
pub fn read_private_file_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::read_private_file_bounded(path, limit)
}

/// Read one regular file with a content-derived limit selected from its first
/// 512 bytes. `upper_limit` remains an unconditional hard cap.
///
/// Inspection and reading use the same descriptor, so a path replacement
/// cannot switch content between classification and buffering.
pub fn read_regular_file_bounded_by(
    path: &Path,
    upper_limit: usize,
    byte_limit: impl Fn(&[u8]) -> usize,
) -> Result<Vec<u8>, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::read_regular_file_bounded_by(path, upper_limit, &byte_limit)
}

/// Open one existing regular file for descriptor-bound reads.
///
/// `path` must be absolute. Symbolic links and special files are rejected.
pub fn open_regular_file_for_read(path: &Path) -> Result<std::fs::File, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::open_regular_file_for_read(path)
}

/// Open one existing regular file for reading and durable appends.
///
/// `path` must be absolute. On Unix every component is opened relative to the
/// previously opened directory descriptor with symlink following disabled.
/// The returned descriptor therefore remains bound to the validated file even
/// if an ancestor or the final pathname is replaced concurrently.
pub fn open_regular_file_for_append(path: &Path) -> Result<std::fs::File, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::open_regular_file_for_append(path)
}

/// Atomically create one new regular file for reading and durable appends.
///
/// `path` must be absolute and its parent directories must already exist. On
/// Unix every parent component and the final create are descriptor-relative
/// with symlink following disabled. Existing targets are never overwritten.
pub fn create_regular_file_for_append(path: &Path) -> Result<std::fs::File, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::create_regular_file_for_append(path)
}

/// Create an absolute directory tree without following symbolic links and make
/// the final directory owner-only. Existing non-directory components fail.
pub fn create_private_directory_all(path: &Path) -> Result<(), SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::create_private_directory_all(path)
}

/// Create a uniquely named owner-only child directory without following path
/// replacements during allocation.
///
/// `parent` must be absolute. `prefix` is restricted to portable filename
/// characters and the random suffix is generated from the operating system's
/// cryptographically secure random source. The final directory create is
/// exclusive and descriptor-relative to the validated private parent.
pub(crate) fn create_unique_private_directory(
    parent: &Path,
    prefix: &str,
) -> Result<PathBuf, SecureFileError> {
    validate_absolute_file_path(parent)?;
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SecureFileError::InvalidPath(prefix.to_owned()));
    }
    imp::create_private_directory_all(parent)?;
    let name = imp::create_unique_private_directory(parent, prefix)?;
    Ok(parent.join(name))
}

/// Open an owner-only directory as a stable advisory-lock anchor.
///
/// The returned descriptor is bound to the private directory rather than to a
/// replaceable lock-file pathname. Retain it while holding the OS lock.
pub fn open_private_directory_for_lock(path: &Path) -> Result<std::fs::File, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::create_private_directory_all(path)?;
    imp::open_private_directory_for_lock(path)
}

/// Atomically publish owner-only bytes beneath an owner-only parent directory.
/// Existing targets must be regular files no larger than `limit`; concurrent
/// target replacement is rejected rather than overwritten.
pub fn write_private_atomic(path: &Path, data: &[u8], limit: usize) -> Result<(), SecureFileError> {
    validate_absolute_file_path(path)?;
    if data.len() > limit {
        return Err(SecureFileError::TooLarge {
            actual: data.len() as u64,
            limit,
        });
    }
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::InvalidPath(path.display().to_string()))?;
    imp::create_private_directory_all(parent)?;
    imp::PreparedMutation::prepare_private(path, limit)?.commit_private(data, &|| false)
}

/// Identity captured after a private lock file has been acquired and repaired.
///
/// Retain this value and revalidate it immediately before releasing the
/// operating-system lock.
pub struct PrivateLockIdentity(imp::PrivateLockIdentity);

/// Open or create a private regular advisory-lock file without following
/// symbolic links.
///
/// Existing metadata is checked for safe ownership, type, and link count, but
/// mode or ACL repair is deferred until after the caller acquires the
/// operating-system lock.
pub fn open_private_lock_file(path: &Path) -> Result<std::fs::File, SecureFileError> {
    validate_absolute_file_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::InvalidPath(path.display().to_string()))?;
    imp::create_private_directory_all(parent)?;
    imp::open_private_lock_file(path)
}

/// Repair and bind a private lock file after acquiring its OS-level lock.
pub fn validate_private_lock_after_acquire(
    path: &Path,
    file: &std::fs::File,
) -> Result<PrivateLockIdentity, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::validate_private_lock_after_acquire(path, file).map(PrivateLockIdentity)
}

/// Revalidate a private lock file immediately before releasing its OS lock.
pub fn revalidate_private_lock_before_release(
    path: &Path,
    file: &std::fs::File,
    identity: &PrivateLockIdentity,
) -> Result<(), SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::revalidate_private_lock_before_release(path, file, &identity.0)
}

/// A target inspected through an already-open parent directory. The original
/// bytes are retained both for caller-side edits/diffs and for the final
/// compare-before-rename conflict check.
pub(crate) struct PreparedMutation {
    inner: imp::PreparedMutation,
}

impl PreparedMutation {
    /// Open a target for a later atomic replacement. Missing parents are made
    /// only when `create_parents` is true. Existing targets must be regular
    /// files no larger than `limit`.
    pub(crate) fn prepare(
        path: &Path,
        create_parents: bool,
        limit: usize,
    ) -> Result<Self, SecureFileError> {
        validate_absolute_file_path(path)?;
        Ok(Self {
            inner: imp::PreparedMutation::prepare(path, create_parents, limit)?,
        })
    }

    /// Original target bytes, or `None` when the target did not exist.
    pub(crate) fn original(&self) -> Option<&[u8]> {
        self.inner.original()
    }

    /// Atomically install `data` if the target still has exactly the state
    /// observed by [`prepare`](Self::prepare).
    #[cfg(test)]
    pub(crate) fn commit(self, data: &[u8]) -> Result<(), SecureFileError> {
        self.commit_if(data, || false)
    }

    /// Commit while polling a cooperative cancellation flag during bounded
    /// writes and immediately before rename.
    pub(crate) fn commit_if(
        self,
        data: &[u8],
        cancelled: impl Fn() -> bool,
    ) -> Result<(), SecureFileError> {
        self.inner.commit(data, &cancelled)
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rustix::fd::OwnedFd;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use rustix::fs::RenameFlags;
    use rustix::fs::{AtFlags, Mode, OFlags};
    use rustix::io::Errno;
    use std::ffi::{OsStr, OsString};
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    fn io_error(error: Errno) -> std::io::Error {
        std::io::Error::from_raw_os_error(error.raw_os_error())
    }

    fn effective_user_id() -> u32 {
        // SAFETY: `geteuid` has no preconditions and does not dereference
        // caller-provided memory.
        unsafe { libc::geteuid() }
    }

    fn insecure_private(reason: &str) -> SecureFileError {
        SecureFileError::InsecurePrivateObject(reason.to_owned())
    }

    fn validate_private_file_identity(
        file: &std::fs::File,
    ) -> Result<std::fs::Metadata, SecureFileError> {
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(SecureFileError::NotRegular);
        }
        if metadata.uid() != effective_user_id() {
            return Err(insecure_private("file is not owned by the current user"));
        }
        if metadata.nlink() != 1 {
            return Err(insecure_private("file has additional hard links"));
        }
        Ok(metadata)
    }

    fn validate_private_file(file: &std::fs::File) -> Result<(), SecureFileError> {
        let metadata = validate_private_file_identity(file)?;
        if metadata.mode() & 0o7777 != 0o600 {
            return Err(insecure_private("file mode is not 0600"));
        }
        Ok(())
    }

    fn make_private_file(file: &std::fs::File) -> Result<(), SecureFileError> {
        let metadata = validate_private_file_identity(file)?;
        if metadata.mode() & 0o7777 != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        validate_private_file(file)
    }

    fn make_private_directory(directory: &OwnedFd) -> Result<(), SecureFileError> {
        let metadata =
            rustix::fs::fstat(directory).map_err(|error| SecureFileError::Io(io_error(error)))?;
        if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::Directory
        {
            return Err(SecureFileError::NotRegular);
        }
        if metadata.st_uid != effective_user_id() {
            return Err(insecure_private(
                "directory is not owned by the current user",
            ));
        }
        if metadata.st_mode & 0o7777 != 0o700 {
            rustix::fs::fchmod(directory, Mode::from_raw_mode(0o700))
                .map_err(|error| SecureFileError::Io(io_error(error)))?;
        }
        let repaired =
            rustix::fs::fstat(directory).map_err(|error| SecureFileError::Io(io_error(error)))?;
        if repaired.st_uid != effective_user_id() || repaired.st_mode & 0o7777 != 0o700 {
            return Err(insecure_private("directory mode is not 0700"));
        }
        Ok(())
    }

    fn components(path: &Path) -> Result<Vec<OsString>, SecureFileError> {
        validate_absolute_file_path(path)?;
        Ok(path
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_os_string()),
                _ => None,
            })
            .collect())
    }

    fn open_root() -> Result<OwnedFd, SecureFileError> {
        rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| SecureFileError::Io(io_error(error)))
    }

    fn open_directory(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd, Errno> {
        rustix::fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }

    /// Directories created while walking a path. On an unsuccessful walk these
    /// are removed deepest-first, but only while their original parent entry
    /// still names the exact directory we created.
    struct CreatedDirectories {
        entries: Vec<CreatedDirectory>,
    }

    struct CreatedDirectory {
        parent: OwnedFd,
        name: OsString,
        device: rustix::fs::Dev,
        inode: u64,
    }

    impl CreatedDirectories {
        fn record(
            &mut self,
            parent: &OwnedFd,
            name: &OsStr,
            directory: &OwnedFd,
        ) -> Result<(), SecureFileError> {
            let metadata = rustix::fs::fstat(directory)
                .map_err(|error| SecureFileError::Io(io_error(error)))?;
            if rustix::fs::FileType::from_raw_mode(metadata.st_mode)
                != rustix::fs::FileType::Directory
            {
                return Err(SecureFileError::NotRegular);
            }
            let parent =
                rustix::io::dup(parent).map_err(|error| SecureFileError::Io(io_error(error)))?;
            self.entries.push(CreatedDirectory {
                parent,
                name: name.to_os_string(),
                device: metadata.st_dev,
                inode: metadata.st_ino,
            });
            Ok(())
        }

        fn disarm(mut self) {
            self.entries.clear();
        }
    }

    impl Drop for CreatedDirectories {
        fn drop(&mut self) {
            for created in self.entries.iter().rev() {
                let Ok(actual) =
                    rustix::fs::statat(&created.parent, &created.name, AtFlags::SYMLINK_NOFOLLOW)
                else {
                    continue;
                };
                if rustix::fs::FileType::from_raw_mode(actual.st_mode)
                    != rustix::fs::FileType::Directory
                    || (actual.st_dev, actual.st_ino) != (created.device, created.inode)
                {
                    continue;
                }
                // The parent descriptor and the checked directory identity keep
                // cleanup confined to the path walk. A non-empty or concurrently
                // replaced directory is deliberately retained.
                let _ = rustix::fs::unlinkat(&created.parent, &created.name, AtFlags::REMOVEDIR);
            }
        }
    }

    fn create_directory_at(
        parent: &OwnedFd,
        name: &OsStr,
        mode: Mode,
        created: &mut CreatedDirectories,
    ) -> Result<Option<OwnedFd>, SecureFileError> {
        match rustix::fs::mkdirat(parent, name, mode) {
            Ok(()) => {}
            Err(Errno::EXIST) => return Ok(None),
            Err(error) => return Err(SecureFileError::Io(io_error(error))),
        }
        let directory =
            open_directory(parent, name).map_err(|error| SecureFileError::Io(io_error(error)))?;
        // Record only after acquiring a descriptor for the object we made. If
        // a hostile rename wins before this open, leaving the entry behind is
        // safer than guessing which object to remove.
        created.record(parent, name, &directory)?;
        Ok(Some(directory))
    }

    #[cfg(not(target_os = "macos"))]
    fn open_root_component(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd, Errno> {
        open_directory(parent, name)
    }

    #[cfg(target_os = "macos")]
    fn open_root_component(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd, Errno> {
        // Root components are no different from caller-controlled descendants,
        // except for macOS's system-owned `/var -> private/var` compatibility
        // alias. Never follow arbitrary first-component links.
        match open_directory(parent, name) {
            Ok(directory) => Ok(directory),
            Err(error) if name == OsStr::new("var") => open_macos_var_alias(parent).or(Err(error)),
            Err(error) => Err(error),
        }
    }

    #[cfg(target_os = "macos")]
    fn open_macos_var_alias(root: &OwnedFd) -> Result<OwnedFd, Errno> {
        let before = rustix::fs::statat(root, "var", AtFlags::SYMLINK_NOFOLLOW)?;
        if rustix::fs::FileType::from_raw_mode(before.st_mode) != rustix::fs::FileType::Symlink
            || before.st_uid != 0
        {
            return Err(Errno::LOOP);
        }
        let target = rustix::fs::readlinkat(root, "var", Vec::new())?;
        if !matches!(target.as_bytes(), b"private/var" | b"/private/var") {
            return Err(Errno::LOOP);
        }

        let followed = rustix::fs::openat(
            root,
            "var",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let private = open_directory(root, OsStr::new("private"))?;
        let expected = open_directory(&private, OsStr::new("var"))?;
        let followed_stat = rustix::fs::fstat(&followed)?;
        let expected_stat = rustix::fs::fstat(&expected)?;
        let after = rustix::fs::statat(root, "var", AtFlags::SYMLINK_NOFOLLOW)?;
        if rustix::fs::FileType::from_raw_mode(followed_stat.st_mode)
            != rustix::fs::FileType::Directory
            || followed_stat.st_uid != 0
            || (followed_stat.st_dev, followed_stat.st_ino)
                != (expected_stat.st_dev, expected_stat.st_ino)
            || (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino)
        {
            return Err(Errno::LOOP);
        }
        Ok(followed)
    }

    fn open_parent(
        path: &Path,
        create_parents: bool,
    ) -> Result<(OwnedFd, OsString), SecureFileError> {
        let mut components = components(path)?;
        let name = components
            .pop()
            .ok_or_else(|| SecureFileError::InvalidPath(path.display().to_string()))?;
        let mut current = open_root()?;
        let mut created = CreatedDirectories {
            entries: Vec::new(),
        };
        for (index, component) in components.into_iter().enumerate() {
            let opened = if index == 0 {
                open_root_component(&current, &component)
            } else {
                open_directory(&current, &component)
            };
            match opened {
                Ok(next) => current = next,
                Err(Errno::NOENT) if create_parents => {
                    current = match create_directory_at(
                        &current,
                        &component,
                        Mode::from_raw_mode(0o755),
                        &mut created,
                    )? {
                        Some(next) => next,
                        None => open_directory(&current, &component)
                            .map_err(|error| SecureFileError::Io(io_error(error)))?,
                    };
                }
                Err(error) => return Err(SecureFileError::Io(io_error(error))),
            }
        }
        created.disarm();
        Ok((current, name))
    }

    fn open_regular_at(parent: &OwnedFd, name: &OsStr) -> Result<std::fs::File, SecureFileError> {
        let descriptor = rustix::fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| SecureFileError::Io(io_error(error)))?;
        let file = std::fs::File::from(descriptor);
        if !file.metadata()?.file_type().is_file() {
            return Err(SecureFileError::NotRegular);
        }
        Ok(file)
    }

    pub(super) fn create_private_directory_all(path: &Path) -> Result<(), SecureFileError> {
        let path_components = components(path)?;
        let mut current = open_root()?;
        let mut created = CreatedDirectories {
            entries: Vec::new(),
        };
        for (index, component) in path_components.iter().enumerate() {
            let opened = if index == 0 {
                open_root_component(&current, component)
            } else {
                open_directory(&current, component)
            };
            let (next, was_created) = match opened {
                Ok(next) => (next, false),
                Err(Errno::NOENT) => match create_directory_at(
                    &current,
                    component,
                    Mode::from_raw_mode(0o700),
                    &mut created,
                )? {
                    Some(next) => (next, true),
                    None => (
                        open_directory(&current, component)
                            .map_err(|error| SecureFileError::Io(io_error(error)))?,
                        false,
                    ),
                },
                Err(error) => return Err(SecureFileError::Io(io_error(error))),
            };
            if was_created || index + 1 == path_components.len() {
                make_private_directory(&next)?;
            }
            current = next;
        }
        created.disarm();
        Ok(())
    }

    pub(super) fn create_unique_private_directory(
        parent: &Path,
        prefix: &str,
    ) -> Result<OsString, SecureFileError> {
        let parent: OwnedFd = open_private_directory_for_lock(parent)?.into();
        for _ in 0..TEMP_NAME_ATTEMPTS {
            let name = OsString::from(format!("{prefix}{}", random_temp_suffix()?));
            let mut created = CreatedDirectories {
                entries: Vec::new(),
            };
            let Some(directory) =
                create_directory_at(&parent, &name, Mode::from_raw_mode(0o700), &mut created)?
            else {
                continue;
            };
            make_private_directory(&directory)?;

            let expected = rustix::fs::fstat(&directory)
                .map_err(|error| SecureFileError::Io(io_error(error)))?;
            let actual = rustix::fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| SecureFileError::Io(io_error(error)))?;
            if rustix::fs::FileType::from_raw_mode(actual.st_mode)
                != rustix::fs::FileType::Directory
                || (actual.st_dev, actual.st_ino) != (expected.st_dev, expected.st_ino)
            {
                return Err(SecureFileError::Changed);
            }

            created.disarm();
            return Ok(name);
        }
        Err(SecureFileError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique private directory",
        )))
    }

    pub(super) fn open_private_directory_for_lock(
        path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        let path_components = components(path)?;
        let mut current = open_root()?;
        for (index, component) in path_components.iter().enumerate() {
            current = if index == 0 {
                open_root_component(&current, component)
            } else {
                open_directory(&current, component)
            }
            .map_err(|error| SecureFileError::Io(io_error(error)))?;
        }
        make_private_directory(&current)?;
        Ok(std::fs::File::from(current))
    }

    pub(super) fn open_private_lock_file(path: &Path) -> Result<std::fs::File, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let mut transient_missing = 0;
        let descriptor = loop {
            match rustix::fs::openat(
                &parent,
                &name,
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(descriptor) => break descriptor,
                // APFS can report a transient ENOENT when two creators race on
                // the same absent name. The already-open parent still binds
                // every retry to the authorized directory.
                Err(Errno::NOENT) if transient_missing < 4 => {
                    transient_missing += 1;
                    std::thread::yield_now();
                }
                Err(error) => return Err(SecureFileError::Io(io_error(error))),
            }
        };
        let file = std::fs::File::from(descriptor);
        validate_private_file_identity(&file)?;
        Ok(file)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn remove_regular_file_if_exists(path: &Path) -> Result<bool, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let file = match open_regular_at(&parent, &name) {
            Ok(file) => file,
            Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let expected = file_identity(&file.metadata()?);

        for _ in 0..TEMP_NAME_ATTEMPTS {
            let temporary = OsString::from(format!(".ygg-delete-{}", random_temp_suffix()?));
            match rustix::fs::renameat_with(
                &parent,
                &name,
                &parent,
                &temporary,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {}
                Err(Errno::EXIST) => continue,
                Err(Errno::NOENT) => return Err(SecureFileError::Changed),
                Err(Errno::NOSYS | Errno::OPNOTSUPP | Errno::INVAL) => {
                    return Err(SecureFileError::PublicationUnavailable);
                }
                Err(error) => return Err(SecureFileError::Io(io_error(error))),
            }

            let moved_is_expected = matches!(
                named_file_identity(&parent, &temporary, false),
                Ok(actual) if same_object(actual, expected)
            );
            if !moved_is_expected {
                // Restore only into an empty original name. If another writer
                // has already published there, preserve both objects and let
                // later recovery handle the randomized orphan.
                let _ = rustix::fs::renameat_with(
                    &parent,
                    &temporary,
                    &parent,
                    &name,
                    RenameFlags::NOREPLACE,
                );
                return Err(SecureFileError::Changed);
            }

            match rustix::fs::unlinkat(&parent, &temporary, AtFlags::empty()) {
                Ok(()) => {
                    rustix::fs::fsync(&parent)
                        .map_err(|error| SecureFileError::Io(io_error(error)))?;
                    return Ok(true);
                }
                Err(error) => {
                    let _ = rustix::fs::renameat_with(
                        &parent,
                        &temporary,
                        &parent,
                        &name,
                        RenameFlags::NOREPLACE,
                    );
                    return Err(SecureFileError::Io(io_error(error)));
                }
            }
        }
        Err(SecureFileError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique secure deletion name",
        )))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn remove_regular_file_if_exists(_path: &Path) -> Result<bool, SecureFileError> {
        Err(SecureFileError::PublicationUnavailable)
    }

    pub(super) fn read_regular_file_bounded(
        path: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        read_open_regular(open_regular_at(&parent, &name)?, limit)
    }

    pub(super) fn read_private_file_bounded(
        path: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let file = open_regular_at(&parent, &name)?;
        validate_private_file(&file)?;
        read_open_regular(file, limit)
    }

    pub(super) fn read_regular_file_bounded_by(
        path: &Path,
        upper_limit: usize,
        byte_limit: &dyn Fn(&[u8]) -> usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        read_open_regular_bounded_by(open_regular_at(&parent, &name)?, upper_limit, byte_limit)
    }

    pub(super) fn open_regular_file_for_read(
        path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        open_regular_at(&parent, &name)
    }

    pub(super) fn open_regular_file_for_append(
        path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let descriptor = rustix::fs::openat(
            &parent,
            &name,
            OFlags::RDWR | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| SecureFileError::Io(io_error(error)))?;
        let file = std::fs::File::from(descriptor);
        make_private_file(&file)?;
        Ok(file)
    }

    pub(super) fn create_regular_file_for_append(
        path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let descriptor = rustix::fs::openat(
            &parent,
            &name,
            OFlags::RDWR
                | OFlags::APPEND
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| SecureFileError::Io(io_error(error)))?;
        let file = std::fs::File::from(descriptor);
        validate_private_file(&file)?;
        Ok(file)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FileIdentity {
        device: u64,
        inode: u64,
        mode: u32,
        links: u64,
        owner: u32,
        group: u32,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        changed_seconds: i64,
        changed_nanoseconds: i64,
    }

    fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
        FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            owner: metadata.uid(),
            group: metadata.gid(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    pub(super) struct PrivateLockIdentity(FileIdentity);

    fn current_private_file_identity(path: &Path) -> Result<FileIdentity, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let file = open_regular_at(&parent, &name)?;
        validate_private_file(&file)?;
        Ok(file_identity(&file.metadata()?))
    }

    pub(super) fn validate_private_lock_after_acquire(
        path: &Path,
        file: &std::fs::File,
    ) -> Result<PrivateLockIdentity, SecureFileError> {
        make_private_file(file)?;
        let identity = file_identity(&file.metadata()?);
        if current_private_file_identity(path)? != identity {
            return Err(SecureFileError::Changed);
        }
        Ok(PrivateLockIdentity(identity))
    }

    pub(super) fn revalidate_private_lock_before_release(
        path: &Path,
        file: &std::fs::File,
        expected: &PrivateLockIdentity,
    ) -> Result<(), SecureFileError> {
        validate_private_file(file)?;
        if file_identity(&file.metadata()?) != expected.0
            || current_private_file_identity(path)? != expected.0
        {
            return Err(SecureFileError::Changed);
        }
        Ok(())
    }

    fn read_for_mutation(
        file: std::fs::File,
        limit: usize,
    ) -> Result<(Vec<u8>, std::fs::Permissions, FileIdentity), SecureFileError> {
        let before = file.metadata()?;
        let permissions = before.permissions();
        let identity = file_identity(&before);
        let bytes = read_open_regular(file.try_clone()?, limit)?;
        if file_identity(&file.metadata()?) != identity {
            return Err(SecureFileError::Changed);
        }
        Ok((bytes, permissions, identity))
    }

    fn read_optional(
        parent: &OwnedFd,
        name: &OsStr,
        limit: usize,
    ) -> Result<Option<(Vec<u8>, std::fs::Permissions, FileIdentity)>, SecureFileError> {
        match open_regular_at(parent, name) {
            Ok(file) => Ok(Some(read_for_mutation(file, limit)?)),
            Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn read_optional_private(
        parent: &OwnedFd,
        name: &OsStr,
        limit: usize,
    ) -> Result<Option<(Vec<u8>, std::fs::Permissions, FileIdentity)>, SecureFileError> {
        match open_regular_at(parent, name) {
            Ok(file) => {
                make_private_file(&file)?;
                Ok(Some(read_for_mutation(file, limit)?))
            }
            Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn read_optional_private_strict(
        parent: &OwnedFd,
        name: &OsStr,
        limit: usize,
    ) -> Result<Option<(Vec<u8>, std::fs::Permissions, FileIdentity)>, SecureFileError> {
        match open_regular_at(parent, name) {
            Ok(file) => {
                validate_private_file(&file)?;
                Ok(Some(read_for_mutation(file, limit)?))
            }
            Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn named_file_identity(
        parent: &OwnedFd,
        name: &OsStr,
        private: bool,
    ) -> Result<FileIdentity, SecureFileError> {
        let file = open_regular_at(parent, name)?;
        if private {
            validate_private_file(&file)?;
        }
        Ok(file_identity(&file.metadata()?))
    }

    fn read_named_state(
        parent: &OwnedFd,
        name: &OsStr,
        limit: usize,
        private: bool,
    ) -> Result<(Vec<u8>, FileIdentity), SecureFileError> {
        let file = open_regular_at(parent, name)?;
        if private {
            validate_private_file(&file)?;
        }
        let (bytes, _, identity) = read_for_mutation(file, limit)?;
        Ok((bytes, identity))
    }

    fn same_object(left: FileIdentity, right: FileIdentity) -> bool {
        (left.device, left.inode) == (right.device, right.inode)
    }

    fn same_stable_state(left: FileIdentity, right: FileIdentity) -> bool {
        left.device == right.device
            && left.inode == right.inode
            && left.mode == right.mode
            && left.links == right.links
            && left.owner == right.owner
            && left.group == right.group
            && left.size == right.size
            && left.modified_seconds == right.modified_seconds
            && left.modified_nanoseconds == right.modified_nanoseconds
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn exchange_names(
        parent: &OwnedFd,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), SecureFileError> {
        match rustix::fs::renameat_with(parent, source, parent, destination, RenameFlags::EXCHANGE)
        {
            Ok(()) => Ok(()),
            Err(Errno::NOSYS | Errno::OPNOTSUPP | Errno::INVAL) => {
                Err(SecureFileError::PublicationUnavailable)
            }
            Err(error) => Err(SecureFileError::Io(io_error(error))),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn exchange_names(
        _parent: &OwnedFd,
        _source: &OsStr,
        _destination: &OsStr,
    ) -> Result<(), SecureFileError> {
        Err(SecureFileError::PublicationUnavailable)
    }

    fn unlink_if_still_named(parent: &OwnedFd, name: &OsStr, expected: FileIdentity) {
        let Ok(actual) = named_file_identity(parent, name, false) else {
            return;
        };
        if same_object(actual, expected) {
            let _ = rustix::fs::unlinkat(parent, name, AtFlags::empty());
        }
    }

    pub(super) struct PreparedMutation {
        parent: OwnedFd,
        name: OsString,
        original: Option<Vec<u8>>,
        original_identity: Option<FileIdentity>,
        permissions: Option<std::fs::Permissions>,
        limit: usize,
        private: bool,
    }

    impl PreparedMutation {
        pub(super) fn prepare(
            path: &Path,
            create_parents: bool,
            limit: usize,
        ) -> Result<Self, SecureFileError> {
            Self::prepare_impl(path, create_parents, limit, false)
        }

        pub(super) fn prepare_private(path: &Path, limit: usize) -> Result<Self, SecureFileError> {
            Self::prepare_impl(path, false, limit, true)
        }

        fn prepare_impl(
            path: &Path,
            create_parents: bool,
            limit: usize,
            private: bool,
        ) -> Result<Self, SecureFileError> {
            let (parent, name) = open_parent(path, create_parents)?;
            let current = if private {
                read_optional_private(&parent, &name, limit)
            } else {
                read_optional(&parent, &name, limit)
            }?;
            let (original, permissions, original_identity) = match current {
                Some((bytes, permissions, identity)) => {
                    (Some(bytes), Some(permissions), Some(identity))
                }
                None => (None, None, None),
            };
            Ok(Self {
                parent,
                name,
                original,
                original_identity,
                permissions,
                limit,
                private,
            })
        }

        pub(super) fn original(&self) -> Option<&[u8]> {
            self.original.as_deref()
        }

        fn unchanged(&self) -> Result<bool, SecureFileError> {
            let current = if self.private {
                read_optional_private_strict(&self.parent, &self.name, self.limit)?
            } else {
                read_optional(&self.parent, &self.name, self.limit)?
            };
            Ok(match (&self.original, self.original_identity, current) {
                (None, None, None) => true,
                (Some(expected), Some(expected_identity), Some((actual, _, actual_identity))) => {
                    expected == &actual && expected_identity == actual_identity
                }
                _ => false,
            })
        }

        /// Atomically swap the staged file with an existing destination, then
        /// verify that the displaced object is still the one observed during
        /// preparation. If it is not, restore the displaced object only while
        /// the destination still names our staged file.
        fn publish_existing(
            &self,
            temp_name: &OsStr,
            temporary_identity: FileIdentity,
        ) -> Result<(), SecureFileError> {
            let expected_bytes = self
                .original
                .as_deref()
                .expect("existing target has original bytes");
            let expected_identity = self
                .original_identity
                .expect("existing target has original identity");

            exchange_names(&self.parent, temp_name, &self.name)?;

            let displaced =
                read_named_state(&self.parent, temp_name, self.limit, self.private).ok();
            let destination_is_temporary = matches!(
                named_file_identity(&self.parent, &self.name, self.private),
                Ok(identity) if same_object(identity, temporary_identity)
            );
            let displaced_is_expected = matches!(
                displaced.as_ref(),
                Some((bytes, identity))
                    if bytes == expected_bytes && same_stable_state(*identity, expected_identity)
            );

            if displaced_is_expected && destination_is_temporary {
                let (_, displaced_identity) = displaced.expect("displaced state was checked");
                unlink_if_still_named(&self.parent, temp_name, displaced_identity);
                return Ok(());
            }

            // A concurrent writer won the race. Do not replace anything it
            // published after the swap; roll back only while the destination
            // still names our staged object.
            if destination_is_temporary {
                exchange_names(&self.parent, temp_name, &self.name)?;
                unlink_if_still_named(&self.parent, temp_name, temporary_identity);
            }
            Err(SecureFileError::Changed)
        }

        pub(super) fn commit(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            self.commit_with_permissions(data, cancelled, None)
        }

        pub(super) fn commit_private(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            self.commit_with_permissions(
                data,
                cancelled,
                Some(std::fs::Permissions::from_mode(0o600)),
            )
        }

        fn commit_with_permissions(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
            forced_permissions: Option<std::fs::Permissions>,
        ) -> Result<(), SecureFileError> {
            let (temp_name, mut temp_file) = {
                let mut created = None;
                for _ in 0..TEMP_NAME_ATTEMPTS {
                    let candidate = OsString::from(format!(".ygg-tmp-{}", random_temp_suffix()?));
                    match rustix::fs::openat(
                        &self.parent,
                        &candidate,
                        OFlags::WRONLY
                            | OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC,
                        Mode::from_raw_mode(0o600),
                    ) {
                        Ok(descriptor) => {
                            created = Some((candidate, std::fs::File::from(descriptor)));
                            break;
                        }
                        Err(Errno::EXIST) => continue,
                        Err(error) => return Err(SecureFileError::Io(io_error(error))),
                    }
                }
                created.ok_or_else(|| {
                    SecureFileError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "could not allocate a unique secure temporary file",
                    ))
                })?
            };

            let result = (|| -> Result<(), SecureFileError> {
                for chunk in data.chunks(64 * 1024) {
                    if cancelled() {
                        return Err(SecureFileError::Cancelled);
                    }
                    temp_file.write_all(chunk)?;
                }
                if cancelled() {
                    return Err(SecureFileError::Cancelled);
                }
                temp_file.sync_all()?;
                if let Some(permissions) = forced_permissions.or_else(|| self.permissions.clone()) {
                    temp_file.set_permissions(permissions)?;
                    temp_file.sync_all()?;
                }
                let temporary_identity = file_identity(&temp_file.metadata()?);
                if !self.unchanged()? {
                    return Err(SecureFileError::Changed);
                }
                if cancelled() {
                    return Err(SecureFileError::Cancelled);
                }
                if self.original.is_none() {
                    // Publishing a newly created file through a hard link is an
                    // atomic no-replace operation. A plain rename here would
                    // overwrite a target created after `unchanged()` returned.
                    match rustix::fs::linkat(
                        &self.parent,
                        &temp_name,
                        &self.parent,
                        &self.name,
                        AtFlags::empty(),
                    ) {
                        Ok(()) => {
                            unlink_if_still_named(&self.parent, &temp_name, temporary_identity);
                        }
                        Err(Errno::EXIST) => return Err(SecureFileError::Changed),
                        Err(error) => return Err(SecureFileError::Io(io_error(error))),
                    }
                } else {
                    self.publish_existing(&temp_name, temporary_identity)?;
                }
                rustix::fs::fsync(&self.parent)
                    .map_err(|error| SecureFileError::Io(io_error(error)))?;
                Ok(())
            })();

            if result.is_err() {
                if let Ok(metadata) = temp_file.metadata() {
                    unlink_if_still_named(&self.parent, &temp_name, file_identity(&metadata));
                }
            }
            result
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::{c_void, OsStr, OsString};
    use std::fs::{File, OpenOptions, Permissions};
    use std::io::{Read, Write};
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::{Component, PathBuf, Prefix};
    use std::ptr::{null, null_mut};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        NtCreateFile, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, RtlNtStatusToDosError, HANDLE, INVALID_HANDLE_VALUE,
        OBJ_CASE_INSENSITIVE, UNICODE_STRING,
    };
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, SET_ACCESS,
        SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    };
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetTokenInformation, InitializeSecurityDescriptor, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TokenUser, ACCESS_ALLOWED_ACE,
        ACE_HEADER, ACL, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
        INHERITED_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, FileDispositionInfoEx, FileRenameInfo, GetFileInformationByHandle,
        SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ALL_ACCESS,
        FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_RENAME_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES,
        READ_CONTROL, SYNCHRONIZE, WRITE_DAC,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const BASIC_DIRECTORY_ACCESS: u32 =
        FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    const PRIVATE_DIRECTORY_INSPECTION_ACCESS: u32 =
        BASIC_DIRECTORY_ACCESS | READ_CONTROL | WRITE_DAC;
    const PRIVATE_DIRECTORY_CREATE_ACCESS: u32 =
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | WRITE_DAC;
    const PRIVATE_INSPECTION_ACCESS: u32 =
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | SYNCHRONIZE;
    const PRIVATE_FILE_ACCESS: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | WRITE_DAC;
    const APPEND_ACCESS: u32 = FILE_READ_DATA
        | FILE_APPEND_DATA
        | FILE_READ_ATTRIBUTES
        | FILE_WRITE_ATTRIBUTES
        | READ_CONTROL
        | SYNCHRONIZE;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FileIdentity {
        volume: u32,
        index: u64,
        links: u32,
        attributes: u32,
        size: u64,
        creation_time: u64,
        last_write_time: u64,
    }

    impl FileIdentity {
        fn is_directory(self) -> bool {
            self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
        }

        fn is_reparse_point(self) -> bool {
            self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        }
    }

    struct TokenHandle(HANDLE);

    impl Drop for TokenHandle {
        fn drop(&mut self) {
            // SAFETY: this type exclusively owns the successful token handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: Security APIs documented to allocate these buffers with LocalAlloc.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    struct PrivateSecurityDescriptor {
        descriptor: SECURITY_DESCRIPTOR,
        _acl: LocalAllocation,
    }

    fn invalid_path(path: &Path) -> SecureFileError {
        SecureFileError::InvalidPath(path.display().to_string())
    }

    fn private_error(message: &str) -> SecureFileError {
        SecureFileError::InsecurePrivateObject(message.to_owned())
    }

    fn win32_error(code: u32) -> std::io::Error {
        std::io::Error::from_raw_os_error(code as i32)
    }

    fn last_error() -> std::io::Error {
        // SAFETY: GetLastError has no preconditions.
        win32_error(unsafe { GetLastError() })
    }

    fn ntstatus_error(status: i32) -> std::io::Error {
        // SAFETY: RtlNtStatusToDosError accepts every NTSTATUS value.
        win32_error(unsafe { RtlNtStatusToDosError(status) })
    }

    fn with_current_user_sid<T>(
        operation: impl FnOnce(PSID) -> Result<T, SecureFileError>,
    ) -> Result<T, SecureFileError> {
        let mut token = null_mut();
        // SAFETY: token is writable and GetCurrentProcess returns a pseudo-handle valid here.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_error().into());
        }
        let _token = TokenHandle(token);
        let mut required = 0_u32;
        // SAFETY: a null/zero query is the documented way to obtain the required size.
        unsafe {
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(last_error().into());
        }
        let words = (required as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        // SAFETY: buffer contains at least required writable bytes and remains alive for operation.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(last_error().into());
        }
        // SAFETY: successful TokenUser initializes a TOKEN_USER at the aligned buffer start.
        let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        operation(user.User.Sid)
    }

    fn build_private_descriptor(
        sid: PSID,
        directory: bool,
    ) -> Result<PrivateSecurityDescriptor, SecureFileError> {
        let mut entry = EXPLICIT_ACCESS_W::default();
        entry.grfAccessPermissions = FILE_ALL_ACCESS;
        entry.grfAccessMode = SET_ACCESS;
        entry.grfInheritance = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        entry.Trustee.TrusteeType = TRUSTEE_IS_USER;
        entry.Trustee.ptstrName = sid.cast();
        let mut acl: *mut ACL = null_mut();
        // SAFETY: entry and output pointers are valid; no old ACL is supplied.
        let status = unsafe { SetEntriesInAclW(1, &entry, null(), &mut acl) };
        if status != 0 {
            return Err(win32_error(status).into());
        }
        let acl = LocalAllocation(acl.cast());
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_pointer = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
        // SAFETY: descriptor is writable and revision 1 is the supported SECURITY_DESCRIPTOR ABI.
        if unsafe { InitializeSecurityDescriptor(descriptor_pointer, 1) } == 0 {
            return Err(last_error().into());
        }
        // SAFETY: sid and ACL remain valid through NtCreateFile; descriptor is initialized.
        if unsafe { SetSecurityDescriptorOwner(descriptor_pointer, sid, 0) } == 0
            || unsafe { SetSecurityDescriptorDacl(descriptor_pointer, 1, acl.0.cast(), 0) } == 0
            || unsafe {
                SetSecurityDescriptorControl(
                    descriptor_pointer,
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                )
            } == 0
        {
            return Err(last_error().into());
        }
        Ok(PrivateSecurityDescriptor {
            descriptor,
            _acl: acl,
        })
    }

    fn with_private_descriptor<T>(
        directory: bool,
        operation: impl FnOnce(*const SECURITY_DESCRIPTOR) -> Result<T, SecureFileError>,
    ) -> Result<T, SecureFileError> {
        with_current_user_sid(|sid| {
            let descriptor = build_private_descriptor(sid, directory)?;
            operation(&descriptor.descriptor)
        })
    }

    fn component_is_safe(name: &OsStr) -> bool {
        let units = name.encode_wide().collect::<Vec<_>>();
        if units.is_empty()
            || units
                .last()
                .is_some_and(|unit| *unit == u16::from(b'.') || *unit == u16::from(b' '))
            || units.iter().any(|unit| {
                *unit == 0
                    || *unit < 32
                    || matches!(*unit, 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124)
            })
        {
            return false;
        }
        let text = name.to_string_lossy();
        let stem = text.split('.').next().unwrap_or_default();
        let stem = stem.trim_end_matches(['.', ' ']).to_ascii_uppercase();
        !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0')
    }

    fn split_absolute(path: &Path) -> Result<(PathBuf, Vec<OsString>), SecureFileError> {
        validate_absolute_file_path(path)?;
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(invalid_path(path));
        };
        match prefix.kind() {
            Prefix::Disk(_)
            | Prefix::UNC(_, _)
            | Prefix::VerbatimDisk(_)
            | Prefix::VerbatimUNC(_, _) => {}
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => return Err(invalid_path(path)),
        }
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(invalid_path(path));
        }
        let mut root = prefix.as_os_str().to_os_string();
        root.push("\\");
        let mut names = Vec::new();
        for component in components {
            let Component::Normal(name) = component else {
                return Err(invalid_path(path));
            };
            if !component_is_safe(name) {
                return Err(invalid_path(path));
            }
            names.push(name.to_os_string());
        }
        Ok((PathBuf::from(root), names))
    }

    fn open_root(path: &Path) -> Result<File, SecureFileError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .access_mode(BASIC_DIRECTORY_ACCESS)
            .share_mode(SHARE_ALL)
            .custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
                    | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        let file = options.open(path)?;
        let identity = file_identity(&file)?;
        if !identity.is_directory() || identity.is_reparse_point() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "path component is not a directory",
            )
            .into());
        }
        Ok(file)
    }

    fn nt_open_at(
        parent: HANDLE,
        name: &OsStr,
        access: u32,
        disposition: u32,
        options: u32,
        attributes: u32,
        security_descriptor: *const SECURITY_DESCRIPTOR,
    ) -> Result<(File, usize), SecureFileError> {
        if !component_is_safe(name) {
            return Err(SecureFileError::InvalidPath(
                name.to_string_lossy().into_owned(),
            ));
        }
        let mut wide = name.encode_wide().collect::<Vec<_>>();
        let byte_len = wide
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(|| SecureFileError::InvalidPath(name.to_string_lossy().into_owned()))?;
        let mut unicode = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: wide.as_mut_ptr(),
        };
        let object = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent,
            ObjectName: &mut unicode,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: security_descriptor,
            SecurityQualityOfService: null(),
        };
        let mut handle = INVALID_HANDLE_VALUE;
        let mut io_status = IO_STATUS_BLOCK::default();
        // SAFETY: all pointers reference initialized storage for the duration of the synchronous call.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access,
                &object,
                &mut io_status,
                null(),
                attributes,
                SHARE_ALL,
                disposition,
                options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                null(),
                0,
            )
        };
        if status < 0 {
            return Err(ntstatus_error(status).into());
        }
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::other("NtCreateFile returned an invalid handle").into());
        }
        // SAFETY: successful NtCreateFile transfers one owned handle to this File.
        let file = unsafe { File::from_raw_handle(handle) };
        Ok((file, io_status.Information))
    }

    fn open_directory_at(
        parent: &File,
        name: &OsStr,
        create: bool,
        private_access: bool,
    ) -> Result<(File, bool), SecureFileError> {
        let access = if create {
            PRIVATE_DIRECTORY_CREATE_ACCESS
        } else if private_access {
            PRIVATE_DIRECTORY_INSPECTION_ACCESS
        } else {
            BASIC_DIRECTORY_ACCESS
        };
        let disposition = if create { FILE_OPEN_IF } else { FILE_OPEN };
        let open = |descriptor| {
            nt_open_at(
                parent.as_raw_handle(),
                name,
                access,
                disposition,
                FILE_DIRECTORY_FILE,
                FILE_ATTRIBUTE_DIRECTORY,
                descriptor,
            )
        };
        let (file, information) = if create {
            with_private_descriptor(true, open)?
        } else {
            open(null())?
        };
        let identity = file_identity(&file)?;
        if !identity.is_directory() || identity.is_reparse_point() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "path component is not a directory",
            )
            .into());
        }
        Ok((file, information == 2))
    }

    /// Newly-created directory handles retained until a path walk succeeds.
    /// Handle deletion is object-bound, so a name replacement cannot cause
    /// rollback to delete the replacement.
    struct CreatedDirectories {
        entries: Vec<CreatedDirectory>,
    }

    struct CreatedDirectory {
        directory: File,
        identity: FileIdentity,
    }

    impl CreatedDirectories {
        fn record(&mut self, directory: &File) -> Result<(), SecureFileError> {
            let identity = file_identity(directory)?;
            if !identity.is_directory() || identity.is_reparse_point() {
                return Err(SecureFileError::NotRegular);
            }
            self.entries.push(CreatedDirectory {
                directory: directory.try_clone()?,
                identity,
            });
            Ok(())
        }

        fn disarm(mut self) {
            self.entries.clear();
        }

        fn rollback(&mut self) {
            while let Some(created) = self.entries.pop() {
                if file_identity(&created.directory)
                    .is_ok_and(|identity| identity == created.identity)
                    && created.identity.is_directory()
                    && !created.identity.is_reparse_point()
                {
                    let _ = delete_handle(&created.directory);
                }
            }
        }
    }

    impl Drop for CreatedDirectories {
        fn drop(&mut self) {
            self.rollback();
        }
    }

    fn open_parent(path: &Path, create: bool) -> Result<(File, OsString), SecureFileError> {
        let mut created = CreatedDirectories {
            entries: Vec::new(),
        };
        let result = (|| {
            let (root_path, mut names) = split_absolute(path)?;
            let name = names.pop().ok_or_else(|| invalid_path(path))?;
            let mut directory = open_root(&root_path)?;
            for component in names {
                match open_directory_at(&directory, &component, false, false) {
                    Ok((next, _)) => directory = next,
                    Err(SecureFileError::Io(error))
                        if create && error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        let (next, was_created) =
                            open_directory_at(&directory, &component, true, true)?;
                        if was_created {
                            created.record(&next)?;
                            make_private_acl(&next, true)?;
                        }
                        directory = next;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok((directory, name))
        })();
        match result {
            Ok(parent) => {
                created.disarm();
                Ok(parent)
            }
            Err(error) => {
                created.rollback();
                Err(error)
            }
        }
    }

    fn file_identity(file: &File) -> Result<FileIdentity, SecureFileError> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: information is writable and file owns a valid handle.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(last_error().into());
        }
        Ok(FileIdentity {
            volume: information.dwVolumeSerialNumber,
            index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
            links: information.nNumberOfLinks,
            attributes: information.dwFileAttributes,
            size: (u64::from(information.nFileSizeHigh) << 32)
                | u64::from(information.nFileSizeLow),
            creation_time: (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
                | u64::from(information.ftCreationTime.dwLowDateTime),
            last_write_time: (u64::from(information.ftLastWriteTime.dwHighDateTime) << 32)
                | u64::from(information.ftLastWriteTime.dwLowDateTime),
        })
    }

    fn ensure_regular(identity: FileIdentity) -> Result<(), SecureFileError> {
        if identity.is_directory() || identity.is_reparse_point() {
            return Err(SecureFileError::NotRegular);
        }
        Ok(())
    }

    fn validate_private_acl(file: &File, directory: bool) -> Result<(), SecureFileError> {
        let identity = file_identity(file)?;
        if identity.is_reparse_point()
            || identity.is_directory() != directory
            || (!directory && identity.links != 1)
        {
            return Err(private_error("private object identity is unsafe"));
        }
        with_current_user_sid(|current_sid| {
            let mut owner: PSID = null_mut();
            let mut dacl: *mut ACL = null_mut();
            let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
            // SAFETY: output pointers are writable and file owns a READ_CONTROL handle.
            let status = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    null_mut(),
                    &mut dacl,
                    null_mut(),
                    &mut descriptor,
                )
            };
            if status != 0 {
                return Err(win32_error(status).into());
            }
            let _descriptor = LocalAllocation(descriptor.cast());
            if owner.is_null() || unsafe { EqualSid(owner, current_sid) } == 0 {
                return Err(private_error(
                    "private object is not owned by the current user",
                ));
            }
            if dacl.is_null() {
                return Err(private_error("private object has an unrestricted ACL"));
            }
            let mut control = 0_u16;
            let mut revision = 0_u32;
            // SAFETY: descriptor is a valid descriptor returned by GetSecurityInfo.
            if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
                || control & SE_DACL_PROTECTED == 0
            {
                return Err(private_error("private object ACL inherits access"));
            }
            let mut acl_info = ACL_SIZE_INFORMATION::default();
            // SAFETY: dacl and output buffer are valid for this query.
            if unsafe {
                GetAclInformation(
                    dacl,
                    (&mut acl_info as *mut ACL_SIZE_INFORMATION).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            } == 0
                || acl_info.AceCount != 1
            {
                return Err(private_error("private object ACL is not owner-only"));
            }
            let mut ace_pointer: *mut c_void = null_mut();
            // SAFETY: a one-entry ACL guarantees index zero exists when GetAce succeeds.
            if unsafe { GetAce(dacl, 0, &mut ace_pointer) } == 0 || ace_pointer.is_null() {
                return Err(private_error("private object ACL is malformed"));
            }
            // SAFETY: every valid ACE starts with an ACE_HEADER.
            let header = unsafe { &*(ace_pointer.cast::<ACE_HEADER>()) };
            if header.AceType != 0 || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
            {
                return Err(private_error("private object ACL is not owner-only"));
            }
            // SAFETY: the allowed-ACE type and size checks prove the fixed prefix is present.
            let ace = unsafe { &*(ace_pointer.cast::<ACCESS_ALLOWED_ACE>()) };
            if u32::from(header.AceFlags) & INHERITED_ACE != 0
                || ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
            {
                return Err(private_error("private object ACL is not owner-only"));
            }
            let inheritance =
                u32::from(header.AceFlags) & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE);
            if (directory && inheritance != (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE))
                || (!directory && inheritance != 0)
            {
                return Err(private_error("private object ACL has unsafe inheritance"));
            }
            let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast();
            if unsafe { EqualSid(ace_sid, current_sid) } == 0 {
                return Err(private_error("private object ACL grants another principal"));
            }
            Ok(())
        })
    }

    fn verify_private_owner(file: &File, directory: bool) -> Result<(), SecureFileError> {
        let identity = file_identity(file)?;
        if identity.is_reparse_point()
            || identity.is_directory() != directory
            || (!directory && identity.links != 1)
        {
            return Err(private_error("private object identity is unsafe"));
        }
        with_current_user_sid(|current_sid| {
            let mut owner: PSID = null_mut();
            let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
            // SAFETY: output pointers are writable and file owns a READ_CONTROL handle.
            let status = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION,
                    &mut owner,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut descriptor,
                )
            };
            if status != 0 {
                return Err(win32_error(status).into());
            }
            let _descriptor = LocalAllocation(descriptor.cast());
            if owner.is_null() || unsafe { EqualSid(owner, current_sid) } == 0 {
                return Err(private_error(
                    "private object is not owned by the current user",
                ));
            }
            Ok(())
        })
    }

    fn apply_private_acl(file: &File, directory: bool) -> Result<(), SecureFileError> {
        verify_private_owner(file, directory)?;
        with_current_user_sid(|sid| {
            let descriptor = build_private_descriptor(sid, directory)?;
            // SAFETY: file owns WRITE_DAC and the ACL allocation outlives this call.
            let status = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    descriptor.descriptor.Dacl,
                    null(),
                )
            };
            if status != 0 {
                return Err(win32_error(status).into());
            }
            validate_private_acl(file, directory)
        })
    }

    fn make_private_acl(file: &File, directory: bool) -> Result<(), SecureFileError> {
        match validate_private_acl(file, directory) {
            Ok(()) => Ok(()),
            Err(SecureFileError::InsecurePrivateObject(_)) => apply_private_acl(file, directory),
            Err(error) => Err(error),
        }
    }

    fn read_open_file_bounded(mut file: &File, limit: usize) -> Result<Vec<u8>, SecureFileError> {
        let metadata = file.metadata()?;
        let advertised =
            usize::try_from(metadata.len()).map_err(|_| SecureFileError::TooLarge {
                limit,
                actual: u64::MAX,
            })?;
        if advertised > limit {
            return Err(SecureFileError::TooLarge {
                limit,
                actual: advertised as u64,
            });
        }
        let capacity = advertised.min(limit);
        let mut bytes = Vec::with_capacity(capacity);
        let mut take = (&mut file).take(limit.saturating_add(1) as u64);
        take.read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(SecureFileError::TooLarge {
                limit,
                actual: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }

    fn open_file_at(
        parent: &File,
        name: &OsStr,
        access: u32,
        disposition: u32,
        private_creation: bool,
    ) -> Result<(File, bool), SecureFileError> {
        let open = |descriptor| {
            nt_open_at(
                parent.as_raw_handle(),
                name,
                access,
                disposition,
                FILE_NON_DIRECTORY_FILE,
                FILE_ATTRIBUTE_NORMAL,
                descriptor,
            )
        };
        let (file, information) = if private_creation {
            with_private_descriptor(false, open)?
        } else {
            open(null())?
        };
        Ok((file, information == 2))
    }

    pub(super) fn create_private_directory_all(path: &Path) -> Result<(), SecureFileError> {
        let mut created = CreatedDirectories {
            entries: Vec::new(),
        };
        let result = (|| {
            let (root_path, names) = split_absolute(path)?;
            if names.is_empty() {
                return Err(invalid_path(path));
            }
            let mut directory = open_root(&root_path)?;
            for (index, component) in names.iter().enumerate() {
                let final_component = index + 1 == names.len();
                let (next, was_created) =
                    match open_directory_at(&directory, component, false, final_component) {
                        Ok(result) => result,
                        Err(SecureFileError::Io(error))
                            if error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            open_directory_at(&directory, component, true, true)?
                        }
                        Err(error) => return Err(error),
                    };
                if was_created {
                    created.record(&next)?;
                }
                if was_created || final_component {
                    make_private_acl(&next, true)?;
                }
                directory = next;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                created.disarm();
                Ok(())
            }
            Err(error) => {
                created.rollback();
                Err(error)
            }
        }
    }

    pub(super) fn create_unique_private_directory(
        parent: &Path,
        prefix: &str,
    ) -> Result<OsString, SecureFileError> {
        let parent = open_private_directory_for_lock(parent)?;
        for _ in 0..TEMP_NAME_ATTEMPTS {
            let name = OsString::from(format!("{prefix}{}", random_temp_suffix()?));
            let created_directory = with_private_descriptor(true, |descriptor| {
                nt_open_at(
                    parent.as_raw_handle(),
                    &name,
                    PRIVATE_DIRECTORY_CREATE_ACCESS,
                    FILE_CREATE,
                    FILE_DIRECTORY_FILE,
                    FILE_ATTRIBUTE_DIRECTORY,
                    descriptor,
                )
            });
            let (directory, _) = match created_directory {
                Ok(created) => created,
                Err(SecureFileError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut created = CreatedDirectories {
                entries: Vec::new(),
            };
            created.record(&directory)?;
            make_private_acl(&directory, true)?;

            let expected = file_identity(&directory)?;
            let (actual, _) = open_directory_at(&parent, &name, false, true)?;
            let actual = file_identity(&actual)?;
            if !actual.is_directory()
                || actual.is_reparse_point()
                || (actual.volume, actual.index) != (expected.volume, expected.index)
            {
                return Err(SecureFileError::Changed);
            }

            created.disarm();
            return Ok(name);
        }
        Err(SecureFileError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique private directory",
        )))
    }

    pub(super) fn open_private_directory_for_lock(path: &Path) -> Result<File, SecureFileError> {
        let (root_path, names) = split_absolute(path)?;
        if names.is_empty() {
            return Err(invalid_path(path));
        }
        let mut directory = open_root(&root_path)?;
        for (index, component) in names.iter().enumerate() {
            let final_component = index + 1 == names.len();
            let (next, _) = open_directory_at(&directory, component, false, final_component)?;
            if final_component {
                validate_private_acl(&next, true)?;
            }
            directory = next;
        }
        Ok(directory)
    }

    pub(super) fn remove_regular_file_if_exists(path: &Path) -> Result<bool, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let (file, _) =
            match open_file_at(&parent, &name, FILE_GENERIC_READ | DELETE, FILE_OPEN, false) {
                Ok(result) => result,
                Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
        ensure_regular(file_identity(&file)?)?;
        delete_handle(&file)?;
        Ok(true)
    }

    pub(super) fn read_regular_file_bounded(
        path: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        read_open_regular(open_regular_file_for_read(path)?, limit)
    }

    pub(super) fn read_regular_file_bounded_by(
        path: &Path,
        upper_limit: usize,
        byte_limit: &dyn Fn(&[u8]) -> usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        read_open_regular_bounded_by(open_regular_file_for_read(path)?, upper_limit, byte_limit)
    }

    pub(super) fn read_private_file_bounded(
        path: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let (file, _) = open_file_at(&parent, &name, FILE_GENERIC_READ, FILE_OPEN, false)?;
        validate_private_acl(&file, false)?;
        read_open_file_bounded(&file, limit)
    }

    pub(super) fn open_regular_file_for_read(path: &Path) -> Result<File, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let (file, _) = open_file_at(&parent, &name, FILE_GENERIC_READ, FILE_OPEN, false)?;
        ensure_regular(file_identity(&file)?)?;
        Ok(file)
    }

    pub(super) fn open_regular_file_for_append(path: &Path) -> Result<File, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let (file, _) = open_file_at(&parent, &name, APPEND_ACCESS, FILE_OPEN, false)?;
        ensure_regular(file_identity(&file)?)?;
        Ok(file)
    }

    pub(super) fn create_regular_file_for_append(path: &Path) -> Result<File, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let (file, _) = open_file_at(&parent, &name, APPEND_ACCESS, FILE_CREATE, false)?;
        ensure_regular(file_identity(&file)?)?;
        Ok(file)
    }

    pub(super) fn open_private_lock_file(path: &Path) -> Result<File, SecureFileError> {
        let (parent, name) = open_parent(path, true)?;
        for _ in 0..16 {
            match open_file_at(&parent, &name, PRIVATE_INSPECTION_ACCESS, FILE_OPEN, false) {
                Ok((existing, _)) => {
                    verify_private_owner(&existing, false)?;
                    return Ok(existing);
                }
                Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    match open_file_at(&parent, &name, PRIVATE_FILE_ACCESS, FILE_CREATE, true) {
                        Ok((file, _)) => {
                            validate_private_acl(&file, false)?;
                            return Ok(file);
                        }
                        Err(SecureFileError::Io(error))
                            if error.kind() == std::io::ErrorKind::AlreadyExists =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(SecureFileError::Changed)
    }

    pub(super) struct PrivateLockIdentity(FileIdentity);

    fn current_private_lock_identity(path: &Path) -> Result<FileIdentity, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        let (file, _) = open_file_at(&parent, &name, PRIVATE_INSPECTION_ACCESS, FILE_OPEN, false)?;
        validate_private_acl(&file, false)?;
        file_identity(&file)
    }

    pub(super) fn validate_private_lock_after_acquire(
        path: &Path,
        file: &File,
    ) -> Result<PrivateLockIdentity, SecureFileError> {
        make_private_acl(file, false)?;
        let identity = file_identity(file)?;
        if current_private_lock_identity(path)? != identity {
            return Err(SecureFileError::Changed);
        }
        Ok(PrivateLockIdentity(identity))
    }

    pub(super) fn revalidate_private_lock_before_release(
        path: &Path,
        file: &File,
        expected: &PrivateLockIdentity,
    ) -> Result<(), SecureFileError> {
        validate_private_acl(file, false)?;
        if file_identity(file)? != expected.0 || current_private_lock_identity(path)? != expected.0
        {
            return Err(SecureFileError::Changed);
        }
        Ok(())
    }

    enum Original {
        Missing,
        Regular {
            bytes: Vec<u8>,
            identity: FileIdentity,
            permissions: Permissions,
        },
    }

    impl Original {
        fn bytes(&self) -> Option<&[u8]> {
            match self {
                Self::Regular { bytes, .. } => Some(bytes),
                Self::Missing => None,
            }
        }
    }

    fn inspect_target(
        parent: &File,
        name: &OsStr,
        limit: usize,
        private: bool,
        repair_private: bool,
    ) -> Result<(Original, Option<File>), SecureFileError> {
        let access = if private {
            PRIVATE_INSPECTION_ACCESS
        } else {
            FILE_GENERIC_READ
        };
        let (file, _) = match open_file_at(parent, name, access, FILE_OPEN, false) {
            Ok(result) => result,
            Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Original::Missing, None));
            }
            Err(error) => return Err(error),
        };
        let identity = file_identity(&file)?;
        ensure_regular(identity)?;
        if private {
            if repair_private {
                make_private_acl(&file, false)?;
            } else {
                validate_private_acl(&file, false)?;
            }
        }
        let permissions = file.metadata()?.permissions();
        let bytes = read_open_file_bounded(&file, limit)?;
        if file_identity(&file)? != identity {
            return Err(SecureFileError::Changed);
        }
        Ok((
            Original::Regular {
                bytes,
                identity,
                permissions,
            },
            Some(file),
        ))
    }

    fn unchanged(
        parent: &File,
        name: &OsStr,
        original: &Original,
        limit: usize,
        private: bool,
    ) -> Result<Option<File>, SecureFileError> {
        let (current, handle) = inspect_target(parent, name, limit, private, false)?;
        let same = match (original, current) {
            (Original::Missing, Original::Missing) => true,
            (
                Original::Regular {
                    bytes, identity, ..
                },
                Original::Regular {
                    bytes: current_bytes,
                    identity: current_identity,
                    ..
                },
            ) => *identity == current_identity && *bytes == current_bytes,
            _ => false,
        };
        if !same {
            return Err(SecureFileError::Changed);
        }
        Ok(handle)
    }

    fn rename_handle(
        file: &File,
        parent: &File,
        name: &OsStr,
        replace: bool,
    ) -> Result<(), SecureFileError> {
        let wide = name.encode_wide().collect::<Vec<_>>();
        let name_bytes = wide
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| SecureFileError::InvalidPath(name.to_string_lossy().into_owned()))?;
        let offset = offset_of!(FILE_RENAME_INFO, FileName);
        let bytes = offset
            .checked_add(name_bytes as usize)
            .ok_or_else(|| SecureFileError::InvalidPath(name.to_string_lossy().into_owned()))?;
        let words = bytes.div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: the aligned buffer is at least offset + name_bytes bytes long.
        unsafe {
            (*info).Anonymous.ReplaceIfExists = replace;
            (*info).RootDirectory = parent.as_raw_handle();
            (*info).FileNameLength = name_bytes;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr().cast::<u8>(),
                buffer.as_mut_ptr().cast::<u8>().add(offset),
                name_bytes as usize,
            );
        }
        // SAFETY: info points to a correctly sized FILE_RENAME_INFO variable-length buffer.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileRenameInfo,
                info.cast(),
                bytes as u32,
            )
        } == 0
        {
            let error = last_error();
            if !replace && error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(SecureFileError::Changed);
            }
            return Err(error.into());
        }
        Ok(())
    }

    fn delete_handle(file: &File) -> Result<(), SecureFileError> {
        let extended = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        // SAFETY: the input struct and handle remain valid for this call.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfoEx,
                (&extended as *const FILE_DISPOSITION_INFO_EX).cast(),
                size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        } != 0
        {
            return Ok(());
        }
        let legacy = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: fallback for Windows versions lacking FileDispositionInfoEx.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&legacy as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(last_error().into());
        }
        Ok(())
    }

    pub(super) struct PreparedMutation {
        parent: File,
        name: OsString,
        original: Original,
        limit: usize,
        private: bool,
    }

    impl PreparedMutation {
        pub(super) fn prepare(
            path: &Path,
            create_parents: bool,
            limit: usize,
        ) -> Result<Self, SecureFileError> {
            Self::prepare_inner(path, create_parents, limit, false)
        }

        pub(super) fn prepare_private(path: &Path, limit: usize) -> Result<Self, SecureFileError> {
            Self::prepare_inner(path, true, limit, true)
        }

        fn prepare_inner(
            path: &Path,
            create_parents: bool,
            limit: usize,
            private: bool,
        ) -> Result<Self, SecureFileError> {
            let (parent, name) = open_parent(path, create_parents)?;
            let (original, _handle) = inspect_target(&parent, &name, limit, private, private)?;
            Ok(Self {
                parent,
                name,
                original,
                limit,
                private,
            })
        }

        pub(super) fn original(&self) -> Option<&[u8]> {
            self.original.bytes()
        }

        pub(super) fn commit(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            self.commit_inner(data, cancelled, false)
        }

        pub(super) fn commit_private(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            self.commit_inner(data, cancelled, true)
        }

        fn commit_inner(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
            private: bool,
        ) -> Result<(), SecureFileError> {
            if private != self.private {
                return Err(private_error("private mutation mode changed"));
            }
            let _initial = unchanged(
                &self.parent,
                &self.name,
                &self.original,
                self.limit,
                private,
            )?;
            if cancelled() {
                return Err(SecureFileError::Cancelled);
            }
            let mut temp = {
                let mut created = None;
                for _ in 0..TEMP_NAME_ATTEMPTS {
                    let candidate = OsString::from(format!(".ygg-tmp-{}", random_temp_suffix()?));
                    match open_file_at(
                        &self.parent,
                        &candidate,
                        PRIVATE_FILE_ACCESS,
                        FILE_CREATE,
                        private,
                    ) {
                        Ok((file, _)) => {
                            created = Some(file);
                            break;
                        }
                        Err(SecureFileError::Io(error))
                            if error.kind() == std::io::ErrorKind::AlreadyExists =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                created.ok_or_else(|| {
                    SecureFileError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "could not allocate a unique secure temporary file",
                    ))
                })?
            };
            let result = (|| -> Result<(), SecureFileError> {
                ensure_regular(file_identity(&temp)?)?;
                if private {
                    make_private_acl(&temp, false)?;
                }
                for chunk in data.chunks(64 * 1024) {
                    if cancelled() {
                        return Err(SecureFileError::Cancelled);
                    }
                    temp.write_all(chunk)?;
                }
                temp.sync_all()?;
                if !private {
                    if let Original::Regular { permissions, .. } = &self.original {
                        temp.set_permissions(permissions.clone())?;
                    }
                }
                if cancelled() {
                    return Err(SecureFileError::Cancelled);
                }
                let _final = unchanged(
                    &self.parent,
                    &self.name,
                    &self.original,
                    self.limit,
                    private,
                )?;
                if cancelled() {
                    return Err(SecureFileError::Cancelled);
                }
                if !matches!(self.original, Original::Missing) {
                    // Windows exposes atomic no-replace rename but no atomic
                    // exchange/CAS primitive for an existing destination.
                    // Do not fall back to ReplaceIfExists after `unchanged()`.
                    return Err(SecureFileError::PublicationUnavailable);
                }
                rename_handle(&temp, &self.parent, &self.name, false)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = delete_handle(&temp);
            }
            result
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::*;

    fn unsupported<T>() -> Result<T, SecureFileError> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "descriptor-bound filesystem access is unavailable on this platform",
        )
        .into())
    }

    pub(super) fn create_private_directory_all(_path: &Path) -> Result<(), SecureFileError> {
        unsupported()
    }

    pub(super) fn create_unique_private_directory(
        _parent: &Path,
        _prefix: &str,
    ) -> Result<std::ffi::OsString, SecureFileError> {
        unsupported()
    }

    pub(super) fn open_private_directory_for_lock(
        _path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        unsupported()
    }

    pub(super) fn remove_regular_file_if_exists(_path: &Path) -> Result<bool, SecureFileError> {
        unsupported()
    }

    pub(super) fn read_regular_file_bounded(
        _path: &Path,
        _limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        unsupported()
    }

    pub(super) fn read_regular_file_bounded_by(
        _path: &Path,
        _upper_limit: usize,
        _byte_limit: &dyn Fn(&[u8]) -> usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        unsupported()
    }

    pub(super) fn read_private_file_bounded(
        _path: &Path,
        _limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        unsupported()
    }

    pub(super) fn open_regular_file_for_read(
        _path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        unsupported()
    }

    pub(super) fn open_regular_file_for_append(
        _path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        unsupported()
    }

    pub(super) fn create_regular_file_for_append(
        _path: &Path,
    ) -> Result<std::fs::File, SecureFileError> {
        unsupported()
    }

    pub(super) fn open_private_lock_file(_path: &Path) -> Result<std::fs::File, SecureFileError> {
        unsupported()
    }

    pub(super) struct PrivateLockIdentity;

    pub(super) fn validate_private_lock_after_acquire(
        _path: &Path,
        _file: &std::fs::File,
    ) -> Result<PrivateLockIdentity, SecureFileError> {
        unsupported()
    }

    pub(super) fn revalidate_private_lock_before_release(
        _path: &Path,
        _file: &std::fs::File,
        _expected: &PrivateLockIdentity,
    ) -> Result<(), SecureFileError> {
        unsupported()
    }

    pub(super) struct PreparedMutation;

    impl PreparedMutation {
        pub(super) fn prepare(
            _path: &Path,
            _create_parents: bool,
            _limit: usize,
        ) -> Result<Self, SecureFileError> {
            unsupported()
        }

        pub(super) fn prepare_private(
            _path: &Path,
            _limit: usize,
        ) -> Result<Self, SecureFileError> {
            unsupported()
        }

        pub(super) fn original(&self) -> Option<&[u8]> {
            None
        }

        pub(super) fn commit(
            self,
            _data: &[u8],
            _cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            unsupported()
        }

        pub(super) fn commit_private(
            self,
            _data: &[u8],
            _cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            unsupported()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_read_rejects_extra_byte() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("large");
        std::fs::write(&path, vec![b'x'; 17]).unwrap();
        assert!(matches!(
            read_regular_file_bounded(&path, 16),
            Err(SecureFileError::TooLarge { .. })
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn unique_private_directories_are_distinct_and_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap().join("private");

        let first = create_unique_private_directory(&parent, "team-").unwrap();
        let second = create_unique_private_directory(&parent, "team-").unwrap();

        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(parent.as_path()));
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("team-"));
        open_private_directory_for_lock(&first).unwrap();
        open_private_directory_for_lock(&second).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unique_private_directory_rejects_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = root.join("outside");
        let parent = root.join("private");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, &parent).unwrap();

        assert!(create_unique_private_directory(&parent, "team-").is_err());
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn unique_private_directory_rejects_path_prefixes() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap().join("private");

        assert!(matches!(
            create_unique_private_directory(&parent, "../team-"),
            Err(SecureFileError::InvalidPath(_))
        ));
        assert!(!parent.exists());
    }

    #[test]
    fn concurrent_target_change_is_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("target.txt");
        std::fs::write(&path, "version one").unwrap();
        let prepared = PreparedMutation::prepare(&path, false, 1024).unwrap();
        std::fs::write(&path, "version two").unwrap();

        assert!(matches!(
            prepared.commit(b"stale replacement"),
            Err(SecureFileError::Changed)
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "version two");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn same_content_target_replacement_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("target.txt");
        let displaced = root.join("displaced.txt");
        std::fs::write(&path, "unchanged bytes").unwrap();
        let prepared = PreparedMutation::prepare(&path, false, 1024).unwrap();

        std::fs::rename(&path, &displaced).unwrap();
        std::fs::write(&path, "unchanged bytes").unwrap();

        assert!(matches!(
            prepared.commit(b"stale replacement"),
            Err(SecureFileError::Changed)
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "unchanged bytes");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn target_created_immediately_before_publish_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("target.txt");
        let prepared = PreparedMutation::prepare(&path, false, 1024).unwrap();
        let cancellation_checks = std::cell::Cell::new(0);

        let result = prepared.commit_if(b"stale replacement", || {
            let check = cancellation_checks.get() + 1;
            cancellation_checks.set(check);
            // The third check occurs after the final unchanged-state check and
            // immediately before publication.
            if check == 3 {
                std::fs::write(&path, "competing creation").unwrap();
            }
            false
        });

        assert!(matches!(result, Err(SecureFileError::Changed)));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "competing creation");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn existing_target_changed_after_final_check_is_rolled_back() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("target.txt");
        let displaced = root.join("prepared-version.txt");
        std::fs::write(&path, "prepared version").unwrap();
        let prepared = PreparedMutation::prepare(&path, false, 1024).unwrap();
        let cancellation_checks = std::cell::Cell::new(0);

        let result = prepared.commit_if(b"stale replacement", || {
            let check = cancellation_checks.get() + 1;
            cancellation_checks.set(check);
            // The third check occurs after the final unchanged-state check and
            // immediately before publication.
            if check == 3 {
                std::fs::rename(&path, &displaced).unwrap();
                std::fs::write(&path, "competing replacement").unwrap();
            }
            false
        });

        assert!(matches!(result, Err(SecureFileError::Changed)));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "competing replacement"
        );
        assert_eq!(
            std::fs::read_to_string(displaced).unwrap(),
            "prepared version"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_existing_target_publication_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("target.txt");
        std::fs::write(&path, "prepared version").unwrap();
        let prepared = PreparedMutation::prepare(&path, false, 1024).unwrap();

        assert!(matches!(
            prepared.commit(b"replacement"),
            Err(SecureFileError::PublicationUnavailable)
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "prepared version");
    }

    #[cfg(windows)]
    #[test]
    fn windows_publication_stays_bound_to_original_parent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("slot")).unwrap();
        let target = workspace.join("slot/new/victim.txt");
        let prepared = PreparedMutation::prepare(&target, true, 1024).unwrap();

        std::fs::rename(workspace.join("slot"), workspace.join("original-slot")).unwrap();
        std::fs::create_dir_all(workspace.join("slot/new")).unwrap();
        prepared.commit(b"bound to original parent").unwrap();

        assert!(!target.exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join("original-slot/new/victim.txt")).unwrap(),
            "bound to original parent"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_files_reject_hard_links_and_reparse_points() {
        use std::io::Write as _;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("private/state.json");
        write_private_atomic(&path, b"secret", 1024).unwrap();
        assert_eq!(read_private_file_bounded(&path, 1024).unwrap(), b"secret");

        let lock_path = root.join("private/state.lock");
        let mut lock = open_private_lock_file(&lock_path).unwrap();
        lock.write_all(b"locked").unwrap();
        drop(lock);
        assert_eq!(
            read_private_file_bounded(&lock_path, 1024).unwrap(),
            b"locked"
        );

        let alias = root.join("private/alias.json");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(matches!(
            read_private_file_bounded(&path, 1024),
            Err(SecureFileError::InsecurePrivateObject(_))
        ));
        assert!(matches!(
            write_private_atomic(&path, b"replacement", 1024),
            Err(SecureFileError::InsecurePrivateObject(_))
        ));

        let link = root.join("private/link.json");
        match std::os::windows::fs::symlink_file(&alias, &link) {
            Ok(()) => {
                assert!(read_regular_file_bounded(&link, 1024).is_err());
                assert!(write_private_atomic(&link, b"replacement", 1024).is_err());
                assert_eq!(std::fs::read(&alias).unwrap(), b"secret");
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(error) => panic!("could not create test symlink: {error}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_creation_rolls_back_created_descendants_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let existing = root.join("existing");
        std::fs::create_dir(&existing).unwrap();
        let created = existing.join("created");
        let too_long = "x".repeat(512);

        assert!(create_private_directory_all(&created.join(too_long)).is_err());
        assert!(existing.is_dir());
        assert!(!created.exists());

        let target = created.join(too_long).join("state.json");
        assert!(PreparedMutation::prepare(&target, true, 1024).is_err());
        assert!(!created.exists());
    }

    #[cfg(unix)]
    #[test]
    fn parent_symlink_swap_cannot_redirect_a_prepared_mutation() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("slot")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = workspace.join("slot/new/victim.txt");
        let prepared = PreparedMutation::prepare(&target, true, 1024).unwrap();

        std::fs::rename(workspace.join("slot"), workspace.join("original-slot")).unwrap();
        symlink(&outside, workspace.join("slot")).unwrap();
        prepared.commit(b"bound to original parent").unwrap();

        assert!(!outside.join("new/victim.txt").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join("original-slot/new/victim.txt")).unwrap(),
            "bound to original parent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_open_rejects_parent_replacement_with_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("slot")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let candidate = workspace.join("slot/session.jsonl");
        std::fs::write(&candidate, "inside\n").unwrap();
        std::fs::write(outside.join("session.jsonl"), "outside\n").unwrap();

        std::fs::rename(workspace.join("slot"), workspace.join("original-slot")).unwrap();
        symlink(&outside, workspace.join("slot")).unwrap();

        assert!(open_regular_file_for_append(&candidate).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("session.jsonl")).unwrap(),
            "outside\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_open_rejects_parent_replacement_with_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("slot")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let candidate = workspace.join("slot/session.jsonl");

        std::fs::rename(workspace.join("slot"), workspace.join("original-slot")).unwrap();
        symlink(&outside, workspace.join("slot")).unwrap();

        assert!(create_regular_file_for_append(&candidate).is_err());
        assert!(!outside.join("session.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_files_require_owner_only_mode_and_one_link() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("private/state.json");
        write_private_atomic(&path, b"secret", 1024).unwrap();
        assert_eq!(read_private_file_bounded(&path, 1024).unwrap(), b"secret");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            read_private_file_bounded(&path, 1024),
            Err(SecureFileError::InsecurePrivateObject(_))
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let alias = root.join("private/alias.json");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(matches!(
            read_private_file_bounded(&path, 1024),
            Err(SecureFileError::InsecurePrivateObject(_))
        ));
        assert!(matches!(
            write_private_atomic(&path, b"replacement", 1024),
            Err(SecureFileError::InsecurePrivateObject(_))
        ));
        assert_eq!(std::fs::read(&alias).unwrap(), b"secret");
    }

    #[cfg(unix)]
    #[test]
    fn private_atomic_write_rejects_a_symlink_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let private = root.join("private");
        let outside = root.join("outside.json");
        let link = private.join("state.json");
        create_private_directory_all(&private).unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, &link).unwrap();

        assert!(write_private_atomic(&link, b"replacement", 1024).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn secure_remove_rejects_symlinks_and_removes_only_regular_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let target = root.join("target.txt");
        let link = root.join("link.txt");
        std::fs::write(&target, b"target").unwrap();
        symlink(&target, &link).unwrap();

        assert!(remove_regular_file_if_exists(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"target");
        assert!(remove_regular_file_if_exists(&target).unwrap());
        assert!(!target.exists());
        assert!(!remove_regular_file_if_exists(&target).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_creation_rolls_back_created_descendants_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let existing = root.join("existing");
        std::fs::create_dir(&existing).unwrap();
        let created = existing.join("created");
        let too_long = "x".repeat(512);

        assert!(create_private_directory_all(&created.join(too_long)).is_err());
        assert!(existing.is_dir());
        assert!(!created.exists());
    }

    #[cfg(unix)]
    #[test]
    fn parent_creation_rolls_back_created_descendants_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let existing = root.join("existing");
        std::fs::create_dir(&existing).unwrap();
        let created = existing.join("created");
        let too_long = "x".repeat(512);
        let target = created.join(too_long).join("state.json");

        assert!(PreparedMutation::prepare(&target, true, 1024).is_err());
        assert!(existing.is_dir());
        assert!(!created.exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_repairs_only_an_owner_controlled_final_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let private = root.join("one/two");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();

        create_private_directory_all(&private).unwrap();
        assert_eq!(
            std::fs::metadata(private).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_rejects_symlink_and_fifo_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let regular = root.join("regular");
        let link = root.join("link");
        let fifo = root.join("fifo");
        std::fs::write(&regular, "secret").unwrap();
        symlink(&regular, &link).unwrap();
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo_c` is a valid NUL-terminated path and mode is valid.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

        assert!(read_regular_file_bounded(&link, 1024).is_err());
        let started = std::time::Instant::now();
        assert!(matches!(
            read_regular_file_bounded(&fifo, 1024),
            Err(SecureFileError::NotRegular)
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
