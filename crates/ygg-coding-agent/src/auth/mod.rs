#![allow(missing_docs)]

//! Provider authentication flows for subscription-backed models.
//!
//! Currently this is OpenAI Codex ("Sign in with ChatGPT") OAuth and
//! custom OpenAI-compatible endpoint credentials. Everything here lives in the
//! product crate and implements the *public* [`ygg_ai::CredentialResolver`]
//! trait, so the frozen `ygg-ai` crate is not touched.

pub mod codex;
pub mod custom;

pub(crate) fn read_bounded_regular(
    path: &std::path::Path,
    limit: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    read_optional(path, limit, false)
}

pub(crate) fn read_bounded_private(
    path: &std::path::Path,
    limit: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    read_optional(path, limit, true)
}

fn read_optional(
    path: &std::path::Path,
    limit: usize,
    private: bool,
) -> anyhow::Result<Option<Vec<u8>>> {
    let result = if private {
        ygg_agent::secure_fs::read_private_file_bounded(path, limit)
    } else {
        ygg_agent::secure_fs::read_regular_file_bounded(path, limit)
    };
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(ygg_agent::secure_fs::SecureFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(anyhow::anyhow!("refusing {}: {error}", path.display())),
    }
}

/// Atomically persist non-secret authentication-adjacent metadata (for example
/// provider model inventories) under an owner-only directory and file.
pub(crate) fn write_private_atomic(
    path: &std::path::Path,
    bytes: &[u8],
    _temporary_prefix: &str,
) -> anyhow::Result<()> {
    const MAX_PRIVATE_ATOMIC_BYTES: usize = 256 * 1024 * 1024;
    ygg_agent::secure_fs::write_private_atomic(path, bytes, MAX_PRIVATE_ATOMIC_BYTES)
        .map_err(anyhow::Error::from)
}
