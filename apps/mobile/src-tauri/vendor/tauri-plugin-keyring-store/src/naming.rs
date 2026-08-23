//! Helpers for **application-level** account naming (`prefix.name`).
//!
//! OS keyrings do not support listing or prefix search; use a stable convention plus your own
//! index of logical keys if you need enumeration.
//!
//! Rules:
//! - `prefix` and `name` are trimmed and must be non-empty.
//! - Neither segment may contain the separator character ([`PREFIX_SEPARATOR`]), so `split_prefixed`
//!   always yields exactly two segments.

use crate::error::{Error, Result};

/// Separator between prefix and name (`prefix.name`).
///
/// See [`join_prefix`] and [`split_prefixed`].
pub const PREFIX_SEPARATOR: char = '.';

/// Builds `prefix.name` after validation.
///
/// # Example
///
/// ```
/// use tauri_plugin_keyring_store::join_prefix;
///
/// assert_eq!(join_prefix("app", "token").unwrap(), "app.token");
/// ```
pub fn join_prefix(prefix: &str, name: &str) -> Result<String> {
    let prefix = prefix.trim();
    let name = name.trim();
    if prefix.is_empty() || name.is_empty() {
        return Err(Error::Naming(
            "prefix and name must be non-empty after trim".into(),
        ));
    }
    if prefix.contains(PREFIX_SEPARATOR) || name.contains(PREFIX_SEPARATOR) {
        return Err(Error::Naming(
            "prefix and name must not contain the separator '.'".into(),
        ));
    }
    let mut out = String::with_capacity(prefix.len() + 1 + name.len());
    out.push_str(prefix);
    out.push(PREFIX_SEPARATOR);
    out.push_str(name);
    Ok(out)
}

/// Parses `prefix.name` into two segments (exactly one separator, no empty segments).
///
/// # Example
///
/// ```
/// use tauri_plugin_keyring_store::{join_prefix, split_prefixed};
///
/// let s = join_prefix("my", "key").unwrap();
/// assert_eq!(split_prefixed(&s).unwrap(), ("my".into(), "key".into()));
/// ```
pub fn split_prefixed(account: &str) -> Result<(String, String)> {
    let account = account.trim();
    let (p, n) = account.split_once(PREFIX_SEPARATOR).ok_or_else(|| {
        Error::Naming("account must contain exactly one '.' between prefix and name".into())
    })?;
    if p.is_empty() || n.is_empty() {
        return Err(Error::Naming(
            "prefix and name segments must be non-empty".into(),
        ));
    }
    if n.contains(PREFIX_SEPARATOR) {
        return Err(Error::Naming(
            "only one separator allowed use nested naming in the app layer".into(),
        ));
    }
    Ok((p.to_string(), n.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_ok() {
        assert_eq!(join_prefix("app", "token").unwrap(), "app.token");
    }

    #[test]
    fn join_rejects_extra_dot() {
        assert!(join_prefix("a.b", "c").is_err());
    }

    #[test]
    fn split_roundtrip() {
        let s = join_prefix("my", "key").unwrap();
        assert_eq!(split_prefixed(&s).unwrap(), ("my".into(), "key".into()));
    }
}
