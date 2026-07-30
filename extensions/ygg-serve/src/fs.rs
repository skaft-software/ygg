//! Root-confined file browsing and editing for explicitly trusted projects.
//!
//! This module accepts only project-relative paths and resolves the trusted
//! root for every operation. It rejects traversal, symlinks, non-regular files,
//! and hard-linked files before returning or changing content.

#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::project_registry::{
    ProjectId as RegistryProjectId, ProjectRegistry, ProjectRegistryError,
};

/// Maximum UTF-8 bytes accepted in a project-relative path.
pub const MAX_PROJECT_FILE_PATH_BYTES: usize = 2_048;
/// Maximum path components accepted in a project-relative path.
pub const MAX_PROJECT_FILE_PATH_COMPONENTS: usize = 64;
/// Maximum entries returned for one directory listing.
pub const MAX_PROJECT_FILE_TREE_ENTRIES: usize = 1_000;
/// Maximum physical directory entries inspected for one directory listing.
pub const MAX_PROJECT_FILE_TREE_SCANNED_ENTRIES: usize = MAX_PROJECT_FILE_TREE_ENTRIES + 1;
/// Maximum bytes returned for one file read.
pub const MAX_PROJECT_FILE_READ_BYTES: u64 = 1024 * 1024;
/// Maximum bytes accepted for one full-file replacement.
pub const MAX_PROJECT_FILE_WRITE_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes accepted in a full-text search query.
pub const MAX_PROJECT_FILE_SEARCH_QUERY_BYTES: usize = 256;
/// Maximum search results returned by one request.
pub const MAX_PROJECT_FILE_SEARCH_RESULTS: usize = 100;
/// Maximum files inspected by one full-text search.
pub const MAX_PROJECT_FILE_SEARCH_FILES: usize = 20_000;
/// Maximum bytes inspected by one full-text search.
pub const MAX_PROJECT_FILE_SEARCH_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum directory depth inspected by one full-text search.
pub const MAX_PROJECT_FILE_SEARCH_DEPTH: usize = 32;
/// Maximum physical entries retained from one searched directory.
pub const MAX_PROJECT_FILE_SEARCH_ENTRIES_PER_DIRECTORY: usize = 1_000;
/// Maximum physical directory entries inspected by one full-text search.
pub const MAX_PROJECT_FILE_SEARCH_DIRECTORY_ENTRIES: usize = 20_000;

/// Kind of one immediate project directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectFileEntryKind {
    /// A regular file.
    File,
    /// A directory that can be listed recursively by the client.
    Directory,
}

/// Safe metadata for one immediate project directory entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFileEntry {
    /// Final UTF-8 path component, never a host path.
    pub name: String,
    /// Coarse entry kind.
    pub kind: ProjectFileEntryKind,
    /// Byte size for files; directories report zero.
    pub size: u64,
    /// Best-effort modification time in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at_ms: Option<u64>,
}

/// One bounded immediate directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFileTree {
    /// Normalized relative directory path; the project root is an empty string.
    pub path: String,
    /// Safe immediate child entries.
    pub entries: Vec<ProjectFileEntry>,
    /// Whether the directory contained more entries than the fixed response bound.
    pub truncated: bool,
}

/// Bounded text returned for a project file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFileRead {
    /// Normalized project-relative file path.
    pub path: String,
    /// UTF-8 text for the requested bounded range.
    pub content: String,
    /// One-based first line represented in `content`; zero for an empty result.
    pub start_line: u32,
    /// One-based final line represented in `content`; zero for an empty result.
    pub end_line: u32,
    /// Number of lines observed while reading the bounded file content.
    pub line_count: u32,
    /// Whether bytes or lines outside `content` were omitted.
    pub truncated: bool,
    /// SHA-256 for a complete file read. Omitted for a partial read, which cannot
    /// safely be written back as a full-file replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// One bounded project full-text search hit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFileSearchHit {
    /// Normalized project-relative file path.
    pub path: String,
    /// One-based matching line when content matched. Path-only matches omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Bounded matching line excerpt, or empty for a path-only match.
    pub snippet: String,
}

/// Bounded result from a trusted-project full-text search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFileSearchResult {
    /// Matching files in deterministic path order.
    pub hits: Vec<ProjectFileSearchHit>,
    /// Whether a fixed scan, depth, byte, or result bound omitted possible hits.
    pub truncated: bool,
    /// Exact bytes inspected from accepted text files.
    pub scanned_bytes: u64,
}

/// Result of a successful full-file replacement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFileWrite {
    /// Normalized project-relative file path.
    pub path: String,
    /// SHA-256 of the exact saved UTF-8 content.
    pub sha256: String,
    /// Best-effort modification time in Unix milliseconds after saving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at_ms: Option<u64>,
}

/// Trusted-project filesystem validation or I/O failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum ProjectFileSystemError {
    /// The project is absent, archived, or no longer explicitly trusted.
    #[error("trusted project access is required")]
    TrustRequired,
    /// The registered project root changed or is no longer a safe directory.
    #[error("the trusted project root changed")]
    RootChanged,
    /// The client supplied an invalid relative path.
    #[error("the project-relative path is invalid")]
    InvalidPath,
    /// A line range is malformed or inverted.
    #[error("the requested line range is invalid")]
    InvalidRange,
    /// A full-text search query is malformed or exceeds a fixed bound.
    #[error("the project file search is invalid")]
    InvalidSearch,
    /// The requested path does not exist.
    #[error("the project file was not found")]
    NotFound,
    /// The requested path is not a directory.
    #[error("the project path is not a directory")]
    NotDirectory,
    /// The requested path is not a regular file.
    #[error("the project path is not a regular file")]
    NotFile,
    /// The selected file cannot be safely represented as text.
    #[error("the project file is not accepted text")]
    NotText,
    /// A requested replacement exceeds the full-file content bound.
    #[error("the project file content exceeds its size limit")]
    ContentTooLarge,
    /// The file changed since the caller read its current version.
    #[error("the project file changed before it could be saved")]
    Conflict,
    /// The concrete host does not expose trusted-project filesystem operations.
    #[error("project filesystem access is not available")]
    Unavailable,
    /// A filesystem operation failed closed.
    #[error("trusted project file access is unavailable")]
    Storage,
    /// The concrete host intentionally does not grant project-file writes.
    #[error("project file editing is not available")]
    WriteUnavailable,
}

#[derive(Clone, Debug)]
struct RelativePath {
    path: PathBuf,
    display: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: Option<u64>,
    inode: Option<u64>,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

struct OpenedFile {
    path: PathBuf,
    file: File,
    identity: FileIdentity,
}

/// Stateless filesystem operations for a registry-managed trusted project.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProjectFileSystem;

impl ProjectFileSystem {
    /// Lists one trusted project directory without exposing its host path.
    pub fn tree(
        registry: &ProjectRegistry,
        project_id: &RegistryProjectId,
        path: &str,
    ) -> Result<ProjectFileTree, ProjectFileSystemError> {
        let relative = parse_relative_path(path, true)?;
        let root = trusted_root(registry, project_id)?;
        let directory = resolve_directory(&root, &relative.path)?;
        let entries = std::fs::read_dir(&directory).map_err(map_read_error)?;
        let mut public_entries = Vec::new();
        let mut truncated = false;
        let mut scanned_entries = 0usize;

        for entry in entries {
            if scanned_entries >= MAX_PROJECT_FILE_TREE_SCANNED_ENTRIES {
                truncated = true;
                break;
            }
            scanned_entries = scanned_entries.saturating_add(1);
            let entry = entry.map_err(|_| ProjectFileSystemError::Storage)?;
            let name = match entry.file_name().into_string() {
                Ok(name) if safe_path_component(&name) => name,
                _ => continue,
            };
            let metadata = entry
                .path()
                .symlink_metadata()
                .map_err(|_| ProjectFileSystemError::Storage)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let kind = if metadata.file_type().is_dir() {
                ProjectFileEntryKind::Directory
            } else if metadata.file_type().is_file() && hard_link_count(&metadata) <= 1 {
                ProjectFileEntryKind::File
            } else {
                continue;
            };
            if public_entries.len() >= MAX_PROJECT_FILE_TREE_ENTRIES {
                truncated = true;
                continue;
            }
            public_entries.push(ProjectFileEntry {
                name,
                kind,
                size: if matches!(kind, ProjectFileEntryKind::File) {
                    metadata.len()
                } else {
                    0
                },
                modified_at_ms: modified_at_ms(&metadata),
            });
        }

        public_entries.sort_by(|left, right| {
            entry_kind_order(left.kind)
                .cmp(&entry_kind_order(right.kind))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(ProjectFileTree {
            path: relative.display,
            entries: public_entries,
            truncated,
        })
    }

    /// Reads one bounded UTF-8 project file, optionally restricted to a line range.
    pub fn read(
        registry: &ProjectRegistry,
        project_id: &RegistryProjectId,
        path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> Result<ProjectFileRead, ProjectFileSystemError> {
        validate_line_range(start_line, end_line)?;
        let relative = parse_relative_path(path, false)?;
        let root = trusted_root(registry, project_id)?;
        let mut opened = open_regular_file(&root, &relative.path, false)?;
        let metadata = opened
            .file
            .metadata()
            .map_err(|_| ProjectFileSystemError::Storage)?;
        let mut bytes = Vec::with_capacity(
            metadata
                .len()
                .min(MAX_PROJECT_FILE_READ_BYTES)
                .try_into()
                .unwrap_or(0),
        );
        Read::by_ref(&mut opened.file)
            .take(MAX_PROJECT_FILE_READ_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ProjectFileSystemError::Storage)?;
        let after = opened
            .file
            .metadata()
            .map_err(|_| ProjectFileSystemError::Storage)?;
        if capture_identity(&after) != opened.identity {
            return Err(ProjectFileSystemError::Conflict);
        }

        let byte_truncated = metadata.len() > MAX_PROJECT_FILE_READ_BYTES
            || bytes.len() as u64 > MAX_PROJECT_FILE_READ_BYTES;
        if bytes.len() as u64 > MAX_PROJECT_FILE_READ_BYTES {
            bytes.truncate(MAX_PROJECT_FILE_READ_BYTES as usize);
        }
        let text = decoded_text(&bytes)?.to_owned();
        let (content, start_line, end_line, line_truncated, line_count) =
            select_lines(&text, start_line, end_line)?;
        let truncated = byte_truncated || line_truncated;
        Ok(ProjectFileRead {
            path: relative.display,
            content,
            start_line,
            end_line,
            line_count,
            truncated,
            sha256: (!truncated).then(|| sha256_hex(text.as_bytes())),
        })
    }

    /// Searches bounded UTF-8 project files under the trusted root.
    pub fn search(
        registry: &ProjectRegistry,
        project_id: &RegistryProjectId,
        query: &str,
    ) -> Result<ProjectFileSearchResult, ProjectFileSystemError> {
        let query = validate_search_query(query)?;
        let root = trusted_root(registry, project_id)?;
        let mut stack = vec![(PathBuf::new(), 0usize)];
        let mut hits = Vec::new();
        let mut scanned_bytes = 0u64;
        let mut scanned_files = 0usize;
        let mut scanned_directory_entries = 0usize;
        let mut truncated = false;

        while let Some((relative_directory, depth)) = stack.pop() {
            let directory = resolve_directory(&root, &relative_directory)?;
            let (entries, directory_truncated) =
                bounded_directory_entries(&directory, &mut scanned_directory_entries)?;
            truncated |= directory_truncated;
            for entry in entries {
                let name = match entry.file_name().into_string() {
                    Ok(name) if safe_path_component(&name) => name,
                    _ => continue,
                };
                let relative_path = relative_directory.join(&name);
                let Some(display_path) = display_relative_path(&relative_path) else {
                    continue;
                };
                let metadata = entry
                    .path()
                    .symlink_metadata()
                    .map_err(|_| ProjectFileSystemError::Storage)?;
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.file_type().is_dir() {
                    if depth >= MAX_PROJECT_FILE_SEARCH_DEPTH {
                        truncated = true;
                    } else {
                        stack.push((relative_path, depth.saturating_add(1)));
                    }
                    continue;
                }
                if !metadata.file_type().is_file() || hard_link_count(&metadata) > 1 {
                    continue;
                }
                if scanned_files >= MAX_PROJECT_FILE_SEARCH_FILES {
                    truncated = true;
                    continue;
                }
                if metadata.len() > MAX_PROJECT_FILE_READ_BYTES
                    || scanned_bytes
                        .checked_add(metadata.len())
                        .is_none_or(|total| total > MAX_PROJECT_FILE_SEARCH_BYTES)
                {
                    truncated = true;
                    if find_match(&display_path, query).is_some()
                        && hits.len() < MAX_PROJECT_FILE_SEARCH_RESULTS
                    {
                        hits.push(ProjectFileSearchHit {
                            path: display_path,
                            line: None,
                            snippet: String::new(),
                        });
                    }
                    continue;
                }
                scanned_files = scanned_files.saturating_add(1);
                let text = match read_complete_text(&root, &relative_path) {
                    Ok((text, byte_len)) => {
                        scanned_bytes = scanned_bytes.saturating_add(byte_len);
                        text
                    }
                    Err(ProjectFileSystemError::NotText) => continue,
                    Err(ProjectFileSystemError::NotFound | ProjectFileSystemError::Conflict) => {
                        truncated = true;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let content_match = find_match(&text, query);
                if find_match(&display_path, query).is_some() || content_match.is_some() {
                    if hits.len() >= MAX_PROJECT_FILE_SEARCH_RESULTS {
                        truncated = true;
                        continue;
                    }
                    let (line, snippet) = match content_match {
                        Some(position) => (
                            Some(line_number(&text, position)),
                            line_snippet(&text, position),
                        ),
                        None => (None, String::new()),
                    };
                    hits.push(ProjectFileSearchHit {
                        path: display_path,
                        line,
                        snippet,
                    });
                }
            }
            if directory_truncated
                && scanned_directory_entries >= MAX_PROJECT_FILE_SEARCH_DIRECTORY_ENTRIES
            {
                break;
            }
        }

        hits.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(ProjectFileSearchResult {
            hits,
            truncated,
            scanned_bytes,
        })
    }

    /// Replaces one complete, previously read project file after an optimistic
    /// SHA-256 version check. A caller may explicitly force a replacement after
    /// presenting a conflict confirmation to its user.
    pub fn write(
        registry: &ProjectRegistry,
        project_id: &RegistryProjectId,
        path: &str,
        content: &str,
        expected_sha256: &str,
        force: bool,
    ) -> Result<ProjectFileWrite, ProjectFileSystemError> {
        if content.len() > MAX_PROJECT_FILE_WRITE_BYTES {
            return Err(ProjectFileSystemError::ContentTooLarge);
        }
        if content.chars().any(is_binary_control) || !valid_sha256(expected_sha256) {
            return Err(ProjectFileSystemError::InvalidPath);
        }
        let relative = parse_relative_path(path, false)?;
        let root = trusted_root(registry, project_id)?;
        let mut opened = open_regular_file(&root, &relative.path, true)?;
        let current = read_opened_complete_text(&mut opened)?;
        let current_sha256 = sha256_hex(current.as_bytes());
        if !force && current_sha256 != expected_sha256 {
            return Err(ProjectFileSystemError::Conflict);
        }

        opened
            .file
            .seek(SeekFrom::Start(0))
            .and_then(|_| opened.file.set_len(0))
            .and_then(|_| opened.file.write_all(content.as_bytes()))
            .and_then(|_| opened.file.sync_data())
            .map_err(|_| ProjectFileSystemError::Storage)?;
        let written = opened
            .file
            .metadata()
            .map_err(|_| ProjectFileSystemError::Storage)?;
        if !written.file_type().is_file() || hard_link_count(&written) > 1 {
            return Err(ProjectFileSystemError::Conflict);
        }
        let path_metadata = opened
            .path
            .symlink_metadata()
            .map_err(|_| ProjectFileSystemError::Conflict)?;
        if path_metadata.file_type().is_symlink()
            || !same_file_node(capture_identity(&path_metadata), capture_identity(&written))
        {
            return Err(ProjectFileSystemError::Conflict);
        }
        Ok(ProjectFileWrite {
            path: relative.display,
            sha256: sha256_hex(content.as_bytes()),
            modified_at_ms: modified_at_ms(&written),
        })
    }
}

fn trusted_root(
    registry: &ProjectRegistry,
    project_id: &RegistryProjectId,
) -> Result<PathBuf, ProjectFileSystemError> {
    let root = registry
        .resolve_trusted_root(project_id)
        .map_err(map_registry_error)?
        .as_path()
        .to_owned();
    let metadata = root
        .symlink_metadata()
        .map_err(|_| ProjectFileSystemError::RootChanged)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || root
            .canonicalize()
            .map_err(|_| ProjectFileSystemError::RootChanged)?
            != root
    {
        return Err(ProjectFileSystemError::RootChanged);
    }
    Ok(root)
}

fn parse_relative_path(
    value: &str,
    allow_root: bool,
) -> Result<RelativePath, ProjectFileSystemError> {
    if value.len() > MAX_PROJECT_FILE_PATH_BYTES {
        return Err(ProjectFileSystemError::InvalidPath);
    }
    if value.is_empty() {
        return allow_root
            .then_some(RelativePath {
                path: PathBuf::new(),
                display: String::new(),
            })
            .ok_or(ProjectFileSystemError::InvalidPath);
    }
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        return Err(ProjectFileSystemError::InvalidPath);
    }
    let mut path = PathBuf::new();
    let mut components = Vec::new();
    for component in candidate.components() {
        let Component::Normal(component) = component else {
            return Err(ProjectFileSystemError::InvalidPath);
        };
        let component = component
            .to_str()
            .filter(|component| safe_path_component(component))
            .ok_or(ProjectFileSystemError::InvalidPath)?;
        components.push(component.to_owned());
        if components.len() > MAX_PROJECT_FILE_PATH_COMPONENTS {
            return Err(ProjectFileSystemError::InvalidPath);
        }
        path.push(component);
    }
    if components.is_empty() {
        return Err(ProjectFileSystemError::InvalidPath);
    }
    Ok(RelativePath {
        path,
        display: components.join("/"),
    })
}

fn resolve_directory(root: &Path, relative: &Path) -> Result<PathBuf, ProjectFileSystemError> {
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(ProjectFileSystemError::InvalidPath);
        };
        current.push(component);
        let metadata = current.symlink_metadata().map_err(map_read_error)?;
        if metadata.file_type().is_symlink() {
            return Err(ProjectFileSystemError::InvalidPath);
        }
        if !metadata.file_type().is_dir() {
            return Err(ProjectFileSystemError::NotDirectory);
        }
    }
    let canonical = current.canonicalize().map_err(map_read_error)?;
    if canonical != current || !canonical.starts_with(root) {
        return Err(ProjectFileSystemError::InvalidPath);
    }
    Ok(current)
}

fn open_regular_file(
    root: &Path,
    relative: &Path,
    write: bool,
) -> Result<OpenedFile, ProjectFileSystemError> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let directory = resolve_directory(root, parent)?;
    let file_name = relative
        .file_name()
        .ok_or(ProjectFileSystemError::InvalidPath)?;
    let path = directory.join(file_name);
    let metadata = path.symlink_metadata().map_err(map_read_error)?;
    if metadata.file_type().is_symlink() {
        return Err(ProjectFileSystemError::InvalidPath);
    }
    if !metadata.file_type().is_file() || hard_link_count(&metadata) > 1 {
        return Err(ProjectFileSystemError::NotFile);
    }
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&path).map_err(map_read_error)?;
    let opened = file
        .metadata()
        .map_err(|_| ProjectFileSystemError::Storage)?;
    if !opened.file_type().is_file()
        || hard_link_count(&opened) > 1
        || !same_file_node(capture_identity(&metadata), capture_identity(&opened))
    {
        return Err(ProjectFileSystemError::Conflict);
    }
    Ok(OpenedFile {
        path,
        file,
        identity: capture_identity(&opened),
    })
}

fn bounded_directory_entries(
    directory: &Path,
    scanned_entries: &mut usize,
) -> Result<(Vec<std::fs::DirEntry>, bool), ProjectFileSystemError> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(directory).map_err(map_read_error)? {
        if *scanned_entries >= MAX_PROJECT_FILE_SEARCH_DIRECTORY_ENTRIES
            || entries.len() >= MAX_PROJECT_FILE_SEARCH_ENTRIES_PER_DIRECTORY
        {
            truncated = true;
            break;
        }
        entries.push(entry.map_err(|_| ProjectFileSystemError::Storage)?);
        *scanned_entries = scanned_entries.saturating_add(1);
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok((entries, truncated))
}

fn read_complete_text(
    root: &Path,
    relative: &Path,
) -> Result<(String, u64), ProjectFileSystemError> {
    let mut opened = open_regular_file(root, relative, false)?;
    let text = read_opened_complete_text(&mut opened)?;
    Ok((text, opened.identity.size))
}

fn read_opened_complete_text(opened: &mut OpenedFile) -> Result<String, ProjectFileSystemError> {
    if opened.identity.size > MAX_PROJECT_FILE_READ_BYTES {
        return Err(ProjectFileSystemError::ContentTooLarge);
    }
    let mut bytes = Vec::with_capacity(opened.identity.size as usize);
    opened
        .file
        .seek(SeekFrom::Start(0))
        .and_then(|_| {
            Read::by_ref(&mut opened.file)
                .take(MAX_PROJECT_FILE_READ_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)
        })
        .map_err(|_| ProjectFileSystemError::Storage)?;
    let after = opened
        .file
        .metadata()
        .map_err(|_| ProjectFileSystemError::Storage)?;
    if capture_identity(&after) != opened.identity
        || bytes.len() as u64 > MAX_PROJECT_FILE_READ_BYTES
        || bytes.len() as u64 != opened.identity.size
    {
        return Err(ProjectFileSystemError::Conflict);
    }
    Ok(decoded_text(&bytes)?.to_owned())
}

fn validate_line_range(
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> Result<(), ProjectFileSystemError> {
    if start_line == Some(0)
        || end_line == Some(0)
        || matches!((start_line, end_line), (Some(start), Some(end)) if end < start)
    {
        return Err(ProjectFileSystemError::InvalidRange);
    }
    Ok(())
}

fn select_lines(
    text: &str,
    requested_start: Option<u32>,
    requested_end: Option<u32>,
) -> Result<(String, u32, u32, bool, u32), ProjectFileSystemError> {
    validate_line_range(requested_start, requested_end)?;
    let lines = if text.is_empty() {
        Vec::new()
    } else {
        text.split_inclusive('\n').collect::<Vec<_>>()
    };
    let line_count = lines.len().min(u32::MAX as usize) as u32;
    if requested_start.is_none() && requested_end.is_none() {
        return Ok((
            text.to_owned(),
            if line_count == 0 { 0 } else { 1 },
            line_count,
            false,
            line_count,
        ));
    }

    let start = requested_start.unwrap_or(1);
    let requested_end = requested_end.unwrap_or(line_count);
    if start > line_count || line_count == 0 {
        return Ok((String::new(), 0, 0, true, line_count));
    }
    let end = requested_end.min(line_count);
    if end < start {
        return Ok((String::new(), 0, 0, true, line_count));
    }
    let start_index = start.saturating_sub(1) as usize;
    let end_index = end as usize;
    Ok((
        lines[start_index..end_index].concat(),
        start,
        end,
        start != 1 || end != line_count,
        line_count,
    ))
}

fn validate_search_query(query: &str) -> Result<&str, ProjectFileSystemError> {
    let query = query.trim();
    if query.is_empty()
        || query.len() > MAX_PROJECT_FILE_SEARCH_QUERY_BYTES
        || query.chars().any(char::is_control)
    {
        return Err(ProjectFileSystemError::InvalidSearch);
    }
    Ok(query)
}

fn decoded_text(bytes: &[u8]) -> Result<&str, ProjectFileSystemError> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])
                .map_err(|_| ProjectFileSystemError::NotText)?
        }
        Err(_) => return Err(ProjectFileSystemError::NotText),
    };
    if text.chars().any(is_binary_control) {
        return Err(ProjectFileSystemError::NotText);
    }
    Ok(text)
}

fn map_registry_error(error: ProjectRegistryError) -> ProjectFileSystemError {
    match error {
        ProjectRegistryError::ProjectNotFound
        | ProjectRegistryError::ProjectArchived
        | ProjectRegistryError::ProjectUntrusted => ProjectFileSystemError::TrustRequired,
        ProjectRegistryError::RootUnavailable
        | ProjectRegistryError::RootSymlink
        | ProjectRegistryError::RootNotDirectory
        | ProjectRegistryError::RootIdentityChanged => ProjectFileSystemError::RootChanged,
        _ => ProjectFileSystemError::Storage,
    }
}

fn map_read_error(error: std::io::Error) -> ProjectFileSystemError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ProjectFileSystemError::NotFound,
        _ => ProjectFileSystemError::Storage,
    }
}

fn capture_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        FileIdentity {
            device: Some(metadata.dev()),
            inode: Some(metadata.ino()),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
        FileIdentity {
            device: None,
            inode: None,
            size: metadata.len(),
            modified_seconds: modified
                .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
                .unwrap_or_default(),
            modified_nanoseconds: modified
                .map(|duration| duration.subsec_nanos() as i64)
                .unwrap_or_default(),
        }
    }
}

fn same_file_node(left: FileIdentity, right: FileIdentity) -> bool {
    match (left.device, left.inode, right.device, right.inode) {
        (Some(left_device), Some(left_inode), Some(right_device), Some(right_inode)) => {
            left_device == right_device && left_inode == right_inode
        }
        _ => true,
    }
}

fn hard_link_count(metadata: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        1
    }
}

fn modified_at_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
}

fn entry_kind_order(kind: ProjectFileEntryKind) -> u8 {
    match kind {
        ProjectFileEntryKind::Directory => 0,
        ProjectFileEntryKind::File => 1,
    }
}

fn display_relative_path(path: &Path) -> Option<String> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let display = components.join("/");
    (display.len() <= MAX_PROJECT_FILE_PATH_BYTES).then_some(display)
}

fn safe_path_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn find_match(haystack: &str, needle: &str) -> Option<usize> {
    if haystack.is_ascii() && needle.is_ascii() {
        haystack
            .to_ascii_lowercase()
            .find(&needle.to_ascii_lowercase())
    } else {
        haystack.find(needle)
    }
}

fn line_number(text: &str, byte_position: usize) -> u32 {
    text[..byte_position]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1)
        .min(u32::MAX as usize) as u32
}

fn line_snippet(text: &str, byte_position: usize) -> String {
    const MAX_SNIPPET_BYTES: usize = 480;
    let start = text[..byte_position]
        .rfind('\n')
        .map(|position| position + 1)
        .unwrap_or(0);
    let end = text[byte_position..]
        .find('\n')
        .map(|offset| byte_position + offset)
        .unwrap_or(text.len());
    truncate_utf8(text[start..end].trim(), MAX_SNIPPET_BYTES)
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut end = maximum_bytes
        .saturating_sub('…'.len_utf8())
        .min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = value[..end].to_owned();
    truncated.push('…');
    truncated
}

fn is_binary_control(character: char) -> bool {
    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
