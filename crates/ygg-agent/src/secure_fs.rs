//! Descriptor-bound, bounded local-file operations.
//!
//! On Unix, path components are opened one at a time with `O_NOFOLLOW` and
//! mutations are committed relative to the already-open parent directory.
//! This binds validation and use to the same directory descriptors instead of
//! re-resolving an attacker-controlled pathname after policy checks.

use std::io::{Read, Write};
#[cfg(not(unix))]
use std::path::PathBuf;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

/// Failures produced by bounded descriptor-based file access.
#[derive(Debug, thiserror::Error)]
pub enum SecureFileError {
    /// The path shape cannot identify a normal file.
    #[error("invalid file path: {0}")]
    InvalidPath(String),
    /// The opened object is not a regular file.
    #[error("not a regular file")]
    NotRegular,
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

fn read_open_regular(mut file: std::fs::File, limit: usize) -> Result<Vec<u8>, SecureFileError> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(SecureFileError::NotRegular);
    }
    if metadata.len() > limit as u64 {
        return Err(SecureFileError::TooLarge {
            actual: metadata.len(),
            limit,
        });
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(limit));
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(SecureFileError::TooLarge {
            actual: bytes.len() as u64,
            limit,
        });
    }
    Ok(bytes)
}

/// Read exactly one regular file, rejecting symlinks and special files and
/// enforcing the byte limit on bytes actually read rather than metadata alone.
///
/// `path` must be absolute. On Unix every component is opened relative to the
/// previously opened directory descriptor, so parent replacement cannot
/// redirect the read after validation.
pub fn read_regular_file_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, SecureFileError> {
    validate_absolute_file_path(path)?;
    imp::read_regular_file_bounded(path, limit)
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
    use rustix::fs::{AtFlags, Mode, OFlags};
    use rustix::io::Errno;
    use std::ffi::{OsStr, OsString};

    fn io_error(error: Errno) -> std::io::Error {
        std::io::Error::from_raw_os_error(error.raw_os_error())
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

    fn open_parent(
        path: &Path,
        create_parents: bool,
    ) -> Result<(OwnedFd, OsString), SecureFileError> {
        let mut components = components(path)?;
        let name = components
            .pop()
            .ok_or_else(|| SecureFileError::InvalidPath(path.display().to_string()))?;
        let mut current = open_root()?;
        for component in components {
            match open_directory(&current, &component) {
                Ok(next) => current = next,
                Err(Errno::NOENT) if create_parents => {
                    match rustix::fs::mkdirat(&current, &component, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => return Err(SecureFileError::Io(io_error(error))),
                    }
                    current = open_directory(&current, &component)
                        .map_err(|error| SecureFileError::Io(io_error(error)))?;
                }
                Err(error) => return Err(SecureFileError::Io(io_error(error))),
            }
        }
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

    pub(super) fn read_regular_file_bounded(
        path: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        let (parent, name) = open_parent(path, false)?;
        read_open_regular(open_regular_at(&parent, &name)?, limit)
    }

    fn read_optional(
        parent: &OwnedFd,
        name: &OsStr,
        limit: usize,
    ) -> Result<Option<(Vec<u8>, std::fs::Permissions)>, SecureFileError> {
        match open_regular_at(parent, name) {
            Ok(file) => {
                let permissions = file.metadata()?.permissions();
                Ok(Some((read_open_regular(file, limit)?, permissions)))
            }
            Err(SecureFileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) struct PreparedMutation {
        parent: OwnedFd,
        name: OsString,
        original: Option<Vec<u8>>,
        permissions: Option<std::fs::Permissions>,
        limit: usize,
    }

    impl PreparedMutation {
        pub(super) fn prepare(
            path: &Path,
            create_parents: bool,
            limit: usize,
        ) -> Result<Self, SecureFileError> {
            let (parent, name) = open_parent(path, create_parents)?;
            let current = read_optional(&parent, &name, limit)?;
            let (original, permissions) = current
                .map(|(bytes, permissions)| (Some(bytes), Some(permissions)))
                .unwrap_or((None, None));
            Ok(Self {
                parent,
                name,
                original,
                permissions,
                limit,
            })
        }

        pub(super) fn original(&self) -> Option<&[u8]> {
            self.original.as_deref()
        }

        fn unchanged(&self) -> Result<bool, SecureFileError> {
            let current = read_optional(&self.parent, &self.name, self.limit)?;
            Ok(match (&self.original, current) {
                (None, None) => true,
                (Some(expected), Some((actual, _))) => expected == &actual,
                _ => false,
            })
        }

        pub(super) fn commit(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
            let file_name = self.name.to_string_lossy();
            let (temp_name, mut temp_file) = loop {
                let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
                let candidate = OsString::from(format!(
                    ".{file_name}.ygg-tmp-{}-{nonce}",
                    std::process::id()
                ));
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
                    Ok(descriptor) => break (candidate, std::fs::File::from(descriptor)),
                    Err(Errno::EXIST) => continue,
                    Err(error) => return Err(SecureFileError::Io(io_error(error))),
                }
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
                if let Some(permissions) = self.permissions.clone() {
                    temp_file.set_permissions(permissions)?;
                    temp_file.sync_all()?;
                }
                if !self.unchanged()? {
                    return Err(SecureFileError::Changed);
                }
                if cancelled() {
                    return Err(SecureFileError::Cancelled);
                }
                rustix::fs::renameat(&self.parent, &temp_name, &self.parent, &self.name)
                    .map_err(|error| SecureFileError::Io(io_error(error)))?;
                rustix::fs::fsync(&self.parent)
                    .map_err(|error| SecureFileError::Io(io_error(error)))?;
                Ok(())
            })();

            if result.is_err() {
                let _ = rustix::fs::unlinkat(&self.parent, &temp_name, AtFlags::empty());
            }
            result
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub(super) fn read_regular_file_bounded(
        path: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, SecureFileError> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(SecureFileError::NotRegular);
        }
        read_open_regular(std::fs::File::open(path)?, limit)
    }

    pub(super) struct PreparedMutation {
        path: PathBuf,
        original: Option<Vec<u8>>,
        permissions: Option<std::fs::Permissions>,
        limit: usize,
    }

    impl PreparedMutation {
        pub(super) fn prepare(
            path: &Path,
            create_parents: bool,
            limit: usize,
        ) -> Result<Self, SecureFileError> {
            if create_parents {
                let parent = path
                    .parent()
                    .ok_or_else(|| SecureFileError::InvalidPath(path.display().to_string()))?;
                std::fs::create_dir_all(parent)?;
            }
            let current = match std::fs::symlink_metadata(path) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                        return Err(SecureFileError::NotRegular);
                    }
                    let permissions = metadata.permissions();
                    Some((read_regular_file_bounded(path, limit)?, permissions))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let (original, permissions) = current
                .map(|(bytes, permissions)| (Some(bytes), Some(permissions)))
                .unwrap_or((None, None));
            Ok(Self {
                path: path.to_owned(),
                original,
                permissions,
                limit,
            })
        }

        pub(super) fn original(&self) -> Option<&[u8]> {
            self.original.as_deref()
        }

        pub(super) fn commit(
            self,
            data: &[u8],
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), SecureFileError> {
            let current = match std::fs::symlink_metadata(&self.path) {
                Ok(_) => Some(read_regular_file_bounded(&self.path, self.limit)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if current.as_deref() != self.original.as_deref() {
                return Err(SecureFileError::Changed);
            }
            if cancelled() {
                return Err(SecureFileError::Cancelled);
            }
            static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
            let parent = self
                .path
                .parent()
                .ok_or_else(|| SecureFileError::InvalidPath(self.path.display().to_string()))?;
            let name = self
                .path
                .file_name()
                .ok_or_else(|| SecureFileError::InvalidPath(self.path.display().to_string()))?
                .to_string_lossy();
            let temp = parent.join(format!(
                ".{name}.ygg-tmp-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            let result = (|| -> Result<(), SecureFileError> {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp)?;
                for chunk in data.chunks(64 * 1024) {
                    if cancelled() {
                        return Err(SecureFileError::Cancelled);
                    }
                    file.write_all(chunk)?;
                }
                file.sync_all()?;
                if let Some(permissions) = self.permissions {
                    file.set_permissions(permissions)?;
                }
                if cancelled() {
                    return Err(SecureFileError::Cancelled);
                }
                std::fs::rename(&temp, &self.path)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(temp);
            }
            result
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
