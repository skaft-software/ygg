//! Disposable SQLite acceleration for workspace-scoped session discovery.
//!
//! JSONL transcripts remain authoritative. The catalog stores only the bounded
//! title projection and a filesystem fingerprint, so any missing, stale, or
//! unusable row can be rebuilt without changing session data.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{params, Connection, OpenFlags};

const CATALOG_DIRECTORY: &str = ".catalog";
const CATALOG_FILE: &str = "sessions-v1.sqlite3";
const CATALOG_SCHEMA_VERSION: i64 = 3;
const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;
const STATUS_SUMMARY: i64 = 0;
const STATUS_UNREADABLE: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogFingerprint {
    pub(crate) file_size: u64,
    pub(crate) modified_ns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CachedTranscriptSummary {
    Summary {
        title: Option<String>,
        configured_model: Option<String>,
        configured_reasoning: Option<String>,
        message_count: usize,
    },
    Unreadable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedSession {
    pub(crate) fingerprint: CatalogFingerprint,
    pub(crate) summary: CachedTranscriptSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogUpdate {
    pub(crate) id: String,
    pub(crate) fingerprint: CatalogFingerprint,
    pub(crate) summary: CachedTranscriptSummary,
}

pub(crate) struct SessionCatalog {
    connection: Connection,
}

impl SessionCatalog {
    pub(crate) fn open_loaded(
        workspace_store: &Path,
    ) -> anyhow::Result<(Self, HashMap<String, CachedSession>)> {
        match Self::open(workspace_store).and_then(|catalog| {
            let cached = catalog.load()?;
            Ok((catalog, cached))
        }) {
            Ok(loaded) => Ok(loaded),
            Err(error) if catalog_error_is_rebuildable(&error) => {
                reset_catalog_files(workspace_store)?;
                let catalog = Self::open(workspace_store)?;
                let cached = catalog.load()?;
                Ok((catalog, cached))
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn open(workspace_store: &Path) -> anyhow::Result<Self> {
        if !workspace_store.is_absolute() {
            anyhow::bail!("session catalog path must be absolute");
        }
        let directory = workspace_store.join(CATALOG_DIRECTORY);
        ygg_agent::secure_fs::create_private_directory_all(&directory)?;
        // SQLite's NOFOLLOW flag rejects paths containing an intermediate
        // symlink (for example macOS' /var -> /private/var). Resolve the
        // directory only after the secure path walk has created and validated
        // it, then keep symlink following disabled for the database open.
        let path = directory.canonicalize()?.join(CATALOG_FILE);
        prepare_private_database_file(&path)?;

        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_millis(100))?;
        // Check forward compatibility before any persistent PRAGMA. In
        // particular, changing journal_mode would rewrite a future catalog
        // before this binary has established that it understands the schema.
        let schema_version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema_version > CATALOG_SCHEMA_VERSION {
            anyhow::bail!(
                "session catalog schema {schema_version} is newer than supported schema {CATALOG_SCHEMA_VERSION}"
            );
        }

        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        connection.pragma_update(None, "trusted_schema", false)?;

        if schema_version < CATALOG_SCHEMA_VERSION {
            connection.execute_batch(
                "DROP TABLE IF EXISTS sessions;
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY NOT NULL,
                     file_size INTEGER NOT NULL CHECK (file_size >= 0),
                     modified_ns INTEGER NOT NULL CHECK (modified_ns >= 0),
                     status INTEGER NOT NULL CHECK (status IN (0, 1)),
                     title TEXT,
                     configured_model TEXT,
                     configured_reasoning TEXT,
                     message_count INTEGER NOT NULL CHECK (message_count >= 0)
                 ) WITHOUT ROWID;
                 PRAGMA user_version = 3;",
            )?;
        } else {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                     id TEXT PRIMARY KEY NOT NULL,
                     file_size INTEGER NOT NULL CHECK (file_size >= 0),
                     modified_ns INTEGER NOT NULL CHECK (modified_ns >= 0),
                     status INTEGER NOT NULL CHECK (status IN (0, 1)),
                     title TEXT,
                     configured_model TEXT,
                     configured_reasoning TEXT,
                     message_count INTEGER NOT NULL CHECK (message_count >= 0)
                 ) WITHOUT ROWID;",
            )?;
        }

        Ok(Self { connection })
    }

    pub(crate) fn load(&self) -> anyhow::Result<HashMap<String, CachedSession>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, file_size, modified_ns, status, title, configured_model, configured_reasoning, message_count FROM sessions")?;
        let rows = statement.query_map([], |row| {
            let configured_reasoning_type = row.get_ref(6)?.data_type();
            let configured_reasoning = match configured_reasoning_type {
                Type::Null => None,
                Type::Text => Some(row.get::<_, String>(6)?),
                _ => return Ok(None),
            };
            Ok(Some((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                configured_reasoning,
                row.get::<_, i64>(7)?,
            )))
        })?;
        let mut sessions = HashMap::new();
        for row in rows {
            let Some((
                id,
                file_size,
                modified_ns,
                status,
                title,
                configured_model,
                configured_reasoning,
                message_count,
            )) = row?
            else {
                continue;
            };
            let Ok(message_count) = usize::try_from(message_count) else {
                continue;
            };
            let Ok(file_size) = u64::try_from(file_size) else {
                continue;
            };
            let summary = match status {
                STATUS_SUMMARY => CachedTranscriptSummary::Summary {
                    title,
                    configured_model,
                    configured_reasoning,
                    message_count,
                },
                STATUS_UNREADABLE => CachedTranscriptSummary::Unreadable,
                _ => continue,
            };
            sessions.insert(
                id,
                CachedSession {
                    fingerprint: CatalogFingerprint {
                        file_size,
                        modified_ns,
                    },
                    summary,
                },
            );
        }
        Ok(sessions)
    }

    pub(crate) fn apply(
        &mut self,
        updates: &[CatalogUpdate],
        stale_ids: &HashSet<String>,
    ) -> anyhow::Result<()> {
        if updates.is_empty() && stale_ids.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.transaction()?;
        {
            let mut upsert = transaction.prepare(
                "INSERT INTO sessions (id, file_size, modified_ns, status, title, configured_model, configured_reasoning, message_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                     file_size = excluded.file_size,
                     modified_ns = excluded.modified_ns,
                     status = excluded.status,
                     title = excluded.title,
                     configured_model = excluded.configured_model,
                     configured_reasoning = excluded.configured_reasoning,
                      message_count = excluded.message_count",
            )?;
            for update in updates {
                let file_size = i64::try_from(update.fingerprint.file_size)
                    .map_err(|_| anyhow::anyhow!("session size does not fit SQLite INTEGER"))?;
                let (status, title, configured_model, configured_reasoning, message_count) =
                    match &update.summary {
                        CachedTranscriptSummary::Summary {
                            title,
                            configured_model,
                            configured_reasoning,
                            message_count,
                        } => (
                            STATUS_SUMMARY,
                            title.as_deref(),
                            configured_model.as_deref(),
                            configured_reasoning.as_deref(),
                            i64::try_from(*message_count).map_err(|_| {
                                anyhow::anyhow!("session message count does not fit SQLite INTEGER")
                            })?,
                        ),
                        CachedTranscriptSummary::Unreadable => {
                            (STATUS_UNREADABLE, None, None, None, 0)
                        }
                    };
                upsert.execute(params![
                    update.id,
                    file_size,
                    update.fingerprint.modified_ns,
                    status,
                    title,
                    configured_model,
                    configured_reasoning,
                    message_count
                ])?;
            }
        }
        {
            let mut delete = transaction.prepare("DELETE FROM sessions WHERE id = ?1")?;
            for id in stale_ids {
                delete.execute([id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn exists(workspace_store: &Path) -> bool {
        Self::path(workspace_store)
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_file())
    }

    pub(crate) fn path(workspace_store: &Path) -> std::path::PathBuf {
        workspace_store.join(CATALOG_DIRECTORY).join(CATALOG_FILE)
    }
}

fn catalog_error_is_rebuildable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let Some(rusqlite::Error::SqliteFailure(error, _)) =
            cause.downcast_ref::<rusqlite::Error>()
        else {
            return false;
        };
        matches!(
            error.code,
            rusqlite::ErrorCode::DatabaseCorrupt
                | rusqlite::ErrorCode::NotADatabase
                | rusqlite::ErrorCode::Unknown
        )
    })
}

fn reset_catalog_files(workspace_store: &Path) -> anyhow::Result<()> {
    let directory = workspace_store.join(CATALOG_DIRECTORY).canonicalize()?;
    let path = directory.join(CATALOG_FILE);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path
            .file_name()
            .expect("catalog path always has a filename")
            .to_os_string();
        name.push(suffix);
        ygg_agent::secure_fs::remove_regular_file_if_exists(&path.with_file_name(name))?;
    }
    ygg_agent::secure_fs::remove_regular_file_if_exists(&path)?;
    Ok(())
}

fn prepare_private_database_file(path: &Path) -> anyhow::Result<()> {
    let file = match ygg_agent::secure_fs::open_regular_file_for_append(path) {
        Ok(file) => file,
        Err(ygg_agent::secure_fs::SecureFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            ygg_agent::secure_fs::create_regular_file_for_append(path)?
        }
        Err(error) => return Err(error.into()),
    };
    let file_size = file.metadata()?.len();
    if file_size > MAX_CATALOG_BYTES {
        anyhow::bail!("session catalog is {file_size} bytes (limit {MAX_CATALOG_BYTES})");
    }
    drop(file);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_text_reasoning_rows_are_ignored_for_transcript_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_store = temp.path().join("sessions");
        std::fs::create_dir(&workspace_store).unwrap();
        let catalog = SessionCatalog::open(&workspace_store).unwrap();
        catalog
            .connection
            .execute_batch(
                "DROP TABLE sessions;
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY NOT NULL,
                     file_size INTEGER NOT NULL,
                     modified_ns INTEGER NOT NULL,
                     status INTEGER NOT NULL,
                     title TEXT,
                     configured_model TEXT,
                     configured_reasoning,
                     message_count INTEGER NOT NULL
                 ) WITHOUT ROWID;",
            )
            .unwrap();
        catalog
            .connection
            .execute(
                "INSERT INTO sessions (id, file_size, modified_ns, status, title, configured_model, configured_reasoning, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, TRUE, ?7)",
                params!["invalid-reasoning", 1_i64, 2_i64, STATUS_SUMMARY, "title", "model", 3_i64],
            )
            .unwrap();
        catalog
            .connection
            .execute(
                "INSERT INTO sessions (id, file_size, modified_ns, status, title, configured_model, configured_reasoning, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params!["valid-reasoning", 1_i64, 2_i64, STATUS_SUMMARY, "title", "model", "high", 3_i64],
            )
            .unwrap();

        let loaded = catalog.load().unwrap();

        assert!(!loaded.contains_key("invalid-reasoning"));
        assert!(matches!(
            loaded.get("valid-reasoning").map(|entry| &entry.summary),
            Some(CachedTranscriptSummary::Summary {
                configured_reasoning: Some(reasoning),
                ..
            }) if reasoning == "high"
        ));
    }
}
