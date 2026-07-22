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
    let Some(name) = path.file_name() else {
        anyhow::bail!("path {} has no file name", path.display());
    };
    let Some(parent) = path.parent() else {
        anyhow::bail!("path {} has no parent", path.display());
    };
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match ygg_agent::secure_fs::read_regular_file_bounded(&parent.join(name), limit) {
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
    temporary_prefix: &str,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", parent.display()))?;
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(temporary_prefix)
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
