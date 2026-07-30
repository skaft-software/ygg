//! Deterministic graphical-shell bytes for the loopback transport.

use std::fs;
use std::io;
use std::path::{Component, Path};

use bytes::Bytes;
use sha2::{Digest as _, Sha256};

const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;
const INDEX_HTML: &[u8] = include_bytes!("../web/index.html");
const APP_CSS: &[u8] = include_bytes!("../web/assets/app.css");
const APP_JS: &[u8] = include_bytes!("../web/assets/app.js");
const FILES_PANEL_JS: &[u8] = include_bytes!("../web/assets/chunk-FilesPanel.js");
const FILE_LANGUAGES_JS: &[u8] = include_bytes!("../web/assets/chunk-file-languages.js");
const JSX_RUNTIME_JS: &[u8] = include_bytes!("../web/assets/chunk-jsx-runtime.js");
const MARKDOWN_JS: &[u8] = include_bytes!("../web/assets/chunk-MarkdownMessage.js");
const SHA256_SUMS: &str = include_str!("../web/SHA256SUMS");
const BUNDLE_SHA256: &str = include_str!("../web/bundle.sha256");
const PAYLOAD_PATHS: [&str; 7] = [
    "assets/app.css",
    "assets/app.js",
    "assets/chunk-FilesPanel.js",
    "assets/chunk-file-languages.js",
    "assets/chunk-jsx-runtime.js",
    "assets/chunk-MarkdownMessage.js",
    "index.html",
];

/// One immutable web response body and its declared media type.
pub(crate) struct WebAsset {
    pub(crate) bytes: Bytes,
    pub(crate) media_type: &'static str,
}

/// A fully validated graphical shell held in memory.
#[derive(Debug)]
pub(crate) struct WebBundle {
    index_html: Bytes,
    app_css: Bytes,
    app_js: Bytes,
    files_panel_js: Bytes,
    file_languages_js: Bytes,
    jsx_runtime_js: Bytes,
    markdown_js: Bytes,
    bundle_sha256: String,
}

impl WebBundle {
    /// Loads and validates the bytes compiled into the backend crate.
    pub(crate) fn embedded() -> io::Result<Self> {
        Self::from_parts(
            Bytes::from_static(INDEX_HTML),
            Bytes::from_static(APP_CSS),
            Bytes::from_static(APP_JS),
            Bytes::from_static(FILES_PANEL_JS),
            Bytes::from_static(FILE_LANGUAGES_JS),
            Bytes::from_static(JSX_RUNTIME_JS),
            Bytes::from_static(MARKDOWN_JS),
            SHA256_SUMS,
            BUNDLE_SHA256,
        )
    }

    /// Loads an explicit built-web override into memory after validating its
    /// complete file set and checked hashes.
    pub(crate) fn from_root(root: &Path) -> io::Result<Self> {
        let root_metadata = fs::symlink_metadata(root)?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(invalid_data(
                "web root must be a real directory, not a symbolic link",
            ));
        }
        let root = fs::canonicalize(root)?;
        let files = list_relative_files(&root)?;
        let mut expected_files = PAYLOAD_PATHS
            .iter()
            .map(|path| (*path).to_owned())
            .collect::<Vec<_>>();
        expected_files.sort_unstable();
        if files != expected_files {
            return Err(invalid_data(format!(
                "web root file set differs: expected {expected_files:?}, found {files:?}"
            )));
        }

        let index_html = read_asset(&root, "index.html")?;
        let app_css = read_asset(&root, "assets/app.css")?;
        let app_js = read_asset(&root, "assets/app.js")?;
        let files_panel_js = read_asset(&root, "assets/chunk-FilesPanel.js")?;
        let file_languages_js = read_asset(&root, "assets/chunk-file-languages.js")?;
        let jsx_runtime_js = read_asset(&root, "assets/chunk-jsx-runtime.js")?;
        let markdown_js = read_asset(&root, "assets/chunk-MarkdownMessage.js")?;
        let sums = canonical_sums(
            &index_html,
            &app_css,
            &app_js,
            &files_panel_js,
            &file_languages_js,
            &jsx_runtime_js,
            &markdown_js,
        );
        let bundle_sha256 = sha256_hex(sums.as_bytes());
        Self::from_parts(
            index_html,
            app_css,
            app_js,
            files_panel_js,
            file_languages_js,
            jsx_runtime_js,
            markdown_js,
            &sums,
            &bundle_sha256,
        )
    }

    fn from_parts(
        index_html: Bytes,
        app_css: Bytes,
        app_js: Bytes,
        files_panel_js: Bytes,
        file_languages_js: Bytes,
        jsx_runtime_js: Bytes,
        markdown_js: Bytes,
        sums: &str,
        bundle_sha256: &str,
    ) -> io::Result<Self> {
        validate_hash(bundle_sha256)?;
        if sha256_hex(sums.as_bytes()) != bundle_sha256 {
            return Err(invalid_data("bundle.sha256 does not match SHA256SUMS"));
        }

        let hashes = parse_sums(sums)?;
        for ((path, expected), bytes) in PAYLOAD_PATHS.iter().zip(hashes.iter()).zip([
            &app_css,
            &app_js,
            &files_panel_js,
            &file_languages_js,
            &jsx_runtime_js,
            &markdown_js,
            &index_html,
        ]) {
            if sha256_hex(bytes) != *expected {
                return Err(invalid_data(format!(
                    "{path} does not match its declared SHA-256"
                )));
            }
        }

        Ok(Self {
            index_html,
            app_css,
            app_js,
            files_panel_js,
            file_languages_js,
            jsx_runtime_js,
            markdown_js,
            bundle_sha256: bundle_sha256.to_owned(),
        })
    }

    /// Returns one allowlisted browser asset.
    pub(crate) fn asset(&self, path: &str) -> Option<WebAsset> {
        let (bytes, media_type) = match path {
            "index.html" => (&self.index_html, "text/html; charset=utf-8"),
            "assets/app.css" => (&self.app_css, "text/css; charset=utf-8"),
            "assets/app.js" => (&self.app_js, "text/javascript; charset=utf-8"),
            "assets/chunk-FilesPanel.js" => {
                (&self.files_panel_js, "text/javascript; charset=utf-8")
            }
            "assets/chunk-file-languages.js" => {
                (&self.file_languages_js, "text/javascript; charset=utf-8")
            }
            "assets/chunk-jsx-runtime.js" => {
                (&self.jsx_runtime_js, "text/javascript; charset=utf-8")
            }
            "assets/chunk-MarkdownMessage.js" => {
                (&self.markdown_js, "text/javascript; charset=utf-8")
            }
            _ => return None,
        };
        Some(WebAsset {
            bytes: bytes.clone(),
            media_type,
        })
    }

    /// Stable digest for the complete path-and-content manifest.
    pub(crate) fn bundle_sha256(&self) -> &str {
        &self.bundle_sha256
    }
}

fn parse_sums(sums: &str) -> io::Result<Vec<String>> {
    if !sums.ends_with('\n') || sums.contains('\r') {
        return Err(invalid_data(
            "SHA256SUMS must use canonical LF-terminated lines",
        ));
    }
    let lines = sums.lines().collect::<Vec<_>>();
    if lines.len() != PAYLOAD_PATHS.len() {
        return Err(invalid_data("SHA256SUMS has an unexpected line count"));
    }

    lines
        .iter()
        .zip(PAYLOAD_PATHS)
        .map(|(line, expected_path)| {
            let (hash, path) = line
                .split_once("  ")
                .ok_or_else(|| invalid_data("invalid SHA256SUMS record"))?;
            validate_hash(hash)?;
            if path != expected_path {
                return Err(invalid_data(format!(
                    "SHA256SUMS path order differs: expected {expected_path}, found {path}"
                )));
            }
            Ok(hash.to_owned())
        })
        .collect()
}

fn validate_hash(hash: &str) -> io::Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_data("invalid lowercase SHA-256 digest"));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonical_sums(
    index_html: &[u8],
    app_css: &[u8],
    app_js: &[u8],
    files_panel_js: &[u8],
    file_languages_js: &[u8],
    jsx_runtime_js: &[u8],
    markdown_js: &[u8],
) -> String {
    PAYLOAD_PATHS
        .iter()
        .zip([
            app_css,
            app_js,
            files_panel_js,
            file_languages_js,
            jsx_runtime_js,
            markdown_js,
            index_html,
        ])
        .map(|(path, bytes)| format!("{}  {path}\n", sha256_hex(bytes)))
        .collect()
}

fn read_asset(root: &Path, relative: &str) -> io::Result<Bytes> {
    read_regular_file(root, relative, MAX_ASSET_BYTES)
}

fn read_regular_file(root: &Path, relative: &str, max_bytes: u64) -> io::Result<Bytes> {
    let path = root.join(relative);
    let canonical = fs::canonicalize(&path)?;
    if !canonical.starts_with(root) {
        return Err(invalid_data(format!("{relative} escapes the web root")));
    }
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(invalid_data(format!(
            "{relative} must be a non-empty regular file no larger than {max_bytes} bytes"
        )));
    }
    Ok(Bytes::from(fs::read(canonical)?))
}

fn list_relative_files(root: &Path) -> io::Result<Vec<String>> {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<String>) -> io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(invalid_data(format!(
                    "web root contains a symbolic link: {}",
                    path.display()
                )));
            }
            if file_type.is_dir() {
                visit(root, &path, files)?;
                continue;
            }
            if !file_type.is_file() {
                return Err(invalid_data(format!(
                    "web root contains a non-regular entry: {}",
                    path.display()
                )));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| invalid_data("web-root traversal failed"))?;
            if !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            {
                return Err(invalid_data("web root contains an invalid path"));
            }
            let portable = relative
                .to_str()
                .ok_or_else(|| invalid_data("web root path is not UTF-8"))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            files.push(portable);
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_valid_bundle(root: &Path) {
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("index.html"), INDEX_HTML).unwrap();
        fs::write(root.join("assets/app.css"), APP_CSS).unwrap();
        fs::write(root.join("assets/app.js"), APP_JS).unwrap();
        fs::write(root.join("assets/chunk-FilesPanel.js"), FILES_PANEL_JS).unwrap();
        fs::write(
            root.join("assets/chunk-file-languages.js"),
            FILE_LANGUAGES_JS,
        )
        .unwrap();
        fs::write(root.join("assets/chunk-jsx-runtime.js"), JSX_RUNTIME_JS).unwrap();
        fs::write(root.join("assets/chunk-MarkdownMessage.js"), MARKDOWN_JS).unwrap();
    }

    #[test]
    fn embedded_bundle_matches_checked_manifests() {
        let bundle = WebBundle::embedded().unwrap();
        assert_eq!(bundle.bundle_sha256(), BUNDLE_SHA256);
        assert_eq!(
            bundle.asset("index.html").unwrap().bytes.as_ref(),
            INDEX_HTML
        );
        assert_eq!(
            bundle.asset("assets/app.css").unwrap().bytes.as_ref(),
            APP_CSS
        );
        assert_eq!(
            bundle.asset("assets/app.js").unwrap().bytes.as_ref(),
            APP_JS
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-FilesPanel.js")
                .unwrap()
                .bytes
                .as_ref(),
            FILES_PANEL_JS
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-file-languages.js")
                .unwrap()
                .bytes
                .as_ref(),
            FILE_LANGUAGES_JS
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-jsx-runtime.js")
                .unwrap()
                .bytes
                .as_ref(),
            JSX_RUNTIME_JS
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-MarkdownMessage.js")
                .unwrap()
                .bytes
                .as_ref(),
            MARKDOWN_JS
        );
        assert!(bundle.asset("assets/app.js.map").is_none());
    }

    #[test]
    fn explicit_override_is_loaded_after_complete_validation() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path());
        let bundle = WebBundle::from_root(directory.path()).unwrap();
        assert_eq!(bundle.bundle_sha256(), BUNDLE_SHA256);
        fs::write(directory.path().join("assets/app.js"), b"changed later").unwrap();
        assert_eq!(
            bundle.asset("assets/app.js").unwrap().bytes.as_ref(),
            APP_JS
        );
    }

    #[test]
    fn explicit_override_rejects_empty_and_unknown_files() {
        let empty = tempfile::tempdir().unwrap();
        write_valid_bundle(empty.path());
        fs::write(empty.path().join("assets/app.js"), b"").unwrap();
        assert_eq!(
            WebBundle::from_root(empty.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let expanded = tempfile::tempdir().unwrap();
        write_valid_bundle(expanded.path());
        fs::write(expanded.path().join("debug.txt"), b"not served").unwrap();
        assert_eq!(
            WebBundle::from_root(expanded.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
