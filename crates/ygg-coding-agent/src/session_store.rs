#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ygg_agent::{EntryValue, Session};
use ygg_ai::{Message, UserPart};

static NEXT_SESSION_SUFFIX: AtomicU64 = AtomicU64::new(1);

/// Filesystem-backed sessions scoped to one canonical workspace.
#[derive(Clone, Debug)]
pub struct SessionStore {
    dir: PathBuf,
}

/// Metadata used by startup and session pickers.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub title: String,
}

/// Derive a compact title from the oldest user text on the active branch.
pub fn active_branch_title(session: &Session) -> String {
    let mut oldest = None;
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(&id) else {
            break;
        };
        if let EntryValue::Message(Message::User(user)) = &entry.value {
            if let Some(UserPart::Text(text)) = user
                .content
                .iter()
                .find(|part| matches!(part, UserPart::Text(_)))
            {
                oldest = Some(text.clone());
            }
        }
        cursor = entry.parent.clone();
    }
    oldest.map_or_else(|| "(empty session)".to_owned(), |title| trim_title(&title))
}

fn trim_title(title: &str) -> String {
    const LIMIT: usize = 60;
    let normalized = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let clipped = normalized.chars().take(LIMIT).collect::<String>();
    format!("{clipped}…")
}

fn workspace_key(workspace: &Path) -> String {
    // FNV-1a is small, deterministic, and avoids another hashing dependency.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:012x}")
}

impl SessionStore {
    /// Create a store rooted at `<session_dir>/<workspace-key>`.
    pub fn new(session_dir: &Path, workspace: &Path) -> Self {
        Self {
            dir: session_dir.join(workspace_key(workspace)),
        }
    }

    /// The workspace-scoped session directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Allocate a new JSONL path. The caller supplies a timestamp for testability.
    pub fn new_path(&self, stamp: &str) -> PathBuf {
        let suffix = NEXT_SESSION_SUFFIX.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!("{stamp}-{suffix:04x}.jsonl"))
    }

    /// List sessions newest-first by filesystem modification time.
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut sessions = std::fs::read_dir(&self.dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    return None;
                }
                let id = path.file_stem()?.to_string_lossy().into_owned();
                let modified = entry.metadata().ok()?.modified().ok()?;
                let title = Session::open(&path)
                    .map(|session| active_branch_title(&session))
                    .unwrap_or_else(|_| "(unreadable session)".to_owned());
                Some(SessionMeta {
                    id,
                    path,
                    modified,
                    title,
                })
            })
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
        sessions
    }

    /// Return the newest session or an actionable error when none exists.
    pub fn latest(&self) -> anyhow::Result<SessionMeta> {
        self.list()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no sessions for this workspace yet"))
    }

    /// Resolve a filename stem from the store without accepting arbitrary paths.
    pub fn by_id(&self, id: &str) -> anyhow::Result<SessionMeta> {
        self.list()
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| anyhow::anyhow!("session {id:?} was not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_workspace_dirs_are_stable_and_distinct() {
        let root = tempfile::tempdir().unwrap();
        let workspace_a = tempfile::tempdir().unwrap();
        let workspace_b = tempfile::tempdir().unwrap();
        let first = SessionStore::new(root.path(), workspace_a.path());
        let second = SessionStore::new(root.path(), workspace_a.path());
        let other = SessionStore::new(root.path(), workspace_b.path());
        assert_eq!(first.dir(), second.dir());
        assert_ne!(first.dir(), other.dir());
        assert!(first.dir().starts_with(root.path()));
    }

    #[test]
    fn new_path_is_inside_dir_with_jsonl_extension_and_prefix() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        let path = store.new_path("2026-07-12T14-30-05Z");
        assert!(path.starts_with(store.dir()));
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("jsonl"));
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("2026-07-12T14-30-05Z-")));
    }

    #[test]
    fn latest_returns_newest_by_mtime() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.dir().join("2026-01-01T00-00-00Z-aaaa.jsonl"), b"").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(store.dir().join("2026-02-02T00-00-00Z-bbbb.jsonl"), b"").unwrap();
        assert_eq!(store.latest().unwrap().id, "2026-02-02T00-00-00Z-bbbb");
    }

    #[test]
    fn active_branch_title_uses_oldest_active_user_text() {
        use ygg_agent::{EntryValue, Session};
        use ygg_ai::{
            AssistantMessage, AssistantPart, Message, ModelId, Protocol, UserMessage, UserPart,
        };

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let root = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("active title".into())],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("abandoned".into())],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session.checkout(root).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("active".into())],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        assert_eq!(active_branch_title(&session), "active title");
    }

    #[test]
    fn by_id_finds_only_jsonl_sessions() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.dir().join("one.jsonl"), b"").unwrap();
        std::fs::write(store.dir().join("one.txt"), b"").unwrap();
        assert_eq!(store.by_id("one").unwrap().id, "one");
        assert!(store.by_id("../one").is_err());
    }
}
