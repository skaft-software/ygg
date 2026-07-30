//! Bounded local pseudo-terminal sessions for the graphical host.
//!
//! The manager deliberately owns shell processes for the lifetime of the
//! loopback server. Browser connections only attach to a session; disconnecting
//! one never terminates its shell. The server owns the authority boundary and
//! bounds session count, retained replay, input, and terminal dimensions.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

/// Maximum number of retained terminal sessions for one graphical host.
pub const MAX_PTY_SESSIONS: usize = 4;
/// Maximum bytes retained for reattaching a terminal session.
pub const MAX_PTY_REPLAY_BYTES: usize = 64 * 1024;
/// Maximum bytes accepted in one browser input frame.
pub const MAX_PTY_INPUT_BYTES: usize = 8 * 1024;
/// Maximum accepted terminal columns.
pub const MAX_PTY_COLUMNS: u16 = 500;
/// Maximum accepted terminal rows.
pub const MAX_PTY_ROWS: u16 = 300;

const PTY_OUTPUT_CHANNEL_CAPACITY: usize = 64;
const PTY_READ_BUFFER_BYTES: usize = 4 * 1024;
static NEXT_PTY_ACCESS_ORDER: AtomicU64 = AtomicU64::new(1);

/// Local terminal configuration supplied by the host process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalConfig {
    /// Workspace in which new shell sessions start.
    pub cwd: PathBuf,
    /// Optional shell selected by the product configuration.
    pub shell: Option<PathBuf>,
}

impl TerminalConfig {
    /// Creates a terminal configuration with automatic shell selection.
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, shell: None }
    }
}

/// Terminal-session setup, lookup, or I/O failure.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// Terminal dimensions are outside the bounded protocol range.
    #[error("invalid terminal dimensions")]
    InvalidDimensions,
    /// A client-provided owner key is not a bounded opaque key.
    #[error("invalid terminal owner key")]
    InvalidOwnerKey,
    /// A client-provided session identifier is not a terminal identifier.
    #[error("invalid terminal session identifier")]
    InvalidSessionId,
    /// A requested working directory is outside the configured workspace.
    #[error("invalid terminal working directory")]
    InvalidWorkingDirectory,
    /// The configured terminal working directory is unavailable.
    #[error("terminal working directory is unavailable")]
    WorkingDirectoryUnavailable,
    /// The requested terminal session no longer exists.
    #[error("terminal session was not found")]
    NotFound,
    /// The shell already exited.
    #[error("terminal session has exited")]
    Exited,
    /// Input exceeds the terminal protocol limit.
    #[error("terminal input exceeds the limit")]
    InputTooLarge,
    /// The pseudo-terminal or shell could not be started.
    #[error("terminal could not be started")]
    Start,
    /// Pseudo-terminal I/O failed.
    #[error("terminal I/O failed")]
    Io(#[source] std::io::Error),
}

/// One requested terminal attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtyOpenRequest {
    /// Requested terminal columns.
    pub cols: u16,
    /// Requested terminal rows.
    pub rows: u16,
    /// Stable opaque owner key used to reattach after a browser remount.
    pub owner_key: Option<String>,
    /// Optional client working directory. It may only name the configured root.
    pub cwd: Option<String>,
}

/// Public lifecycle metadata for one retained terminal session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSession {
    /// Opaque terminal identifier.
    pub id: String,
    /// Stable owner key supplied by the browser.
    pub owner_key: String,
    /// Initial shell working directory.
    pub cwd: PathBuf,
    /// Creation time in Unix milliseconds.
    pub created_at: u64,
    /// Most recent open, input, resize, or detach time in Unix milliseconds.
    pub last_used_at: u64,
    /// Whether the shell is still running.
    pub alive: bool,
}

/// Shell-exit facts forwarded to connected clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtyExit {
    /// Shell exit code.
    pub exit_code: u32,
    /// Signal description when the platform exposed one.
    pub signal: Option<String>,
}

/// An event emitted by one terminal session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PtyEvent {
    /// Bytes emitted by the shell, decoded lossily as UTF-8 for the WebSocket protocol.
    Output(String),
    /// The shell exited and will not accept more input.
    Exit(PtyExit),
}

/// A new or reattached terminal connection.
pub struct PtyAttachment {
    /// Opaque terminal identifier.
    pub id: String,
    /// Stable owner key, including a generated key when the client omitted one.
    pub owner_key: String,
    /// Bounded terminal output retained before this attachment.
    pub replay: String,
    /// Exit state when the retained shell already ended.
    pub exit: Option<PtyExit>,
    /// Live terminal output after the replay snapshot.
    pub events: broadcast::Receiver<PtyEvent>,
}

/// Bounded, in-process terminal-session manager.
#[derive(Clone)]
pub struct PtyManager {
    inner: Arc<PtyManagerInner>,
}

struct PtyManagerInner {
    config: TerminalConfig,
    sessions: Mutex<PtySessions>,
}

#[derive(Default)]
struct PtySessions {
    by_id: HashMap<String, Arc<PtySession>>,
    by_owner: HashMap<String, String>,
}

struct PtySession {
    id: String,
    owner_key: String,
    cwd: PathBuf,
    created_at: u64,
    last_used_at: AtomicU64,
    last_used_order: AtomicU64,
    alive: AtomicBool,
    master: Mutex<Option<Box<dyn MasterPty>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Mutex<PtyOutput>,
    events: broadcast::Sender<PtyEvent>,
}

#[derive(Default)]
struct PtyOutput {
    replay: VecDeque<u8>,
    exit: Option<PtyExit>,
}

impl PtyManager {
    /// Creates a bounded manager rooted at the configured local workspace.
    pub fn new(mut config: TerminalConfig) -> Result<Self, PtyError> {
        config.cwd = config
            .cwd
            .canonicalize()
            .map_err(|_| PtyError::WorkingDirectoryUnavailable)?;
        if !config.cwd.is_dir() {
            return Err(PtyError::WorkingDirectoryUnavailable);
        }
        Ok(Self {
            inner: Arc::new(PtyManagerInner {
                config,
                sessions: Mutex::new(PtySessions::default()),
            }),
        })
    }

    /// Opens a new shell or reattaches to the live shell for the owner key.
    pub fn open(&self, request: PtyOpenRequest) -> Result<PtyAttachment, PtyError> {
        validate_dimensions(request.cols, request.rows)?;
        self.validate_cwd(request.cwd.as_deref())?;
        let owner_key = match request.owner_key {
            Some(owner_key) => {
                validate_owner_key(&owner_key)?;
                owner_key
            }
            None => random_hex(16)?,
        };

        // Keep creation under the session lock: a newly spawned shell must not
        // briefly exceed the advertised four-session process bound when several
        // browser panes open at once.
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .expect("terminal sessions poisoned");
        for stale in remove_exited_sessions(&mut sessions) {
            stale.stop();
        }
        if let Some(existing_id) = sessions.by_owner.get(&owner_key).cloned() {
            if let Some(existing) = sessions.by_id.get(&existing_id).cloned() {
                if existing.alive.load(Ordering::Acquire) {
                    existing.touch();
                    drop(sessions);
                    existing.resize(request.cols, request.rows)?;
                    return Ok(existing.attach());
                }
                if let Some(stale) = remove_session(&mut sessions, &existing_id) {
                    stale.stop();
                }
            } else {
                sessions.by_owner.remove(&owner_key);
            }
        }
        if let Some(evicted) = evict_lru_if_full(&mut sessions) {
            evicted.stop();
        }

        let session = PtySession::spawn(
            random_hex(16)?,
            owner_key.clone(),
            self.inner.config.cwd.clone(),
            self.inner.config.shell.clone(),
            request.cols,
            request.rows,
        )?;
        sessions.by_owner.insert(owner_key, session.id.clone());
        sessions
            .by_id
            .insert(session.id.clone(), Arc::clone(&session));
        drop(sessions);
        Ok(session.attach())
    }

    /// Resizes a retained terminal session.
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), PtyError> {
        validate_dimensions(cols, rows)?;
        let session = self.session(id)?;
        session.resize(cols, rows)?;
        session.touch();
        Ok(())
    }

    /// Forwards bounded browser input to the selected shell.
    pub fn input(&self, id: &str, data: &str) -> Result<(), PtyError> {
        if data.len() > MAX_PTY_INPUT_BYTES {
            return Err(PtyError::InputTooLarge);
        }
        let session = self.session(id)?;
        session.write(data.as_bytes())?;
        session.touch();
        Ok(())
    }

    /// Marks an attachment as detached while retaining its shell for reattach.
    pub fn detach(&self, id: &str) -> Result<(), PtyError> {
        let session = self.session(id)?;
        session.touch();
        Ok(())
    }

    /// Stops every retained shell. This is called during loopback-server shutdown.
    pub fn shutdown(&self) {
        self.inner.stop_all();
    }

    /// Returns metadata for one retained session.
    pub fn session_info(&self, id: &str) -> Result<TerminalSession, PtyError> {
        Ok(self.session(id)?.info())
    }

    fn validate_cwd(&self, requested: Option<&str>) -> Result<(), PtyError> {
        let Some(requested) = requested else {
            return Ok(());
        };
        let requested = Path::new(requested);
        if !requested.is_absolute() {
            return Err(PtyError::InvalidWorkingDirectory);
        }
        let requested = requested
            .canonicalize()
            .map_err(|_| PtyError::InvalidWorkingDirectory)?;
        if requested == self.inner.config.cwd {
            Ok(())
        } else {
            Err(PtyError::InvalidWorkingDirectory)
        }
    }

    fn session(&self, id: &str) -> Result<Arc<PtySession>, PtyError> {
        validate_session_id(id)?;
        self.inner
            .sessions
            .lock()
            .expect("terminal sessions poisoned")
            .by_id
            .get(id)
            .cloned()
            .ok_or(PtyError::NotFound)
    }
}

impl PtyManagerInner {
    fn stop_all(&self) {
        let sessions = {
            let mut sessions = match self.sessions.lock() {
                Ok(sessions) => sessions,
                Err(error) => error.into_inner(),
            };
            sessions.by_owner.clear();
            sessions
                .by_id
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>()
        };
        for session in sessions {
            session.stop();
        }
    }
}

impl Drop for PtyManagerInner {
    fn drop(&mut self) {
        self.stop_all();
    }
}

impl PtySession {
    fn spawn(
        id: String,
        owner_key: String,
        cwd: PathBuf,
        configured_shell: Option<PathBuf>,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<Self>, PtyError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| PtyError::Start)?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|_| PtyError::Start)?;
        let writer = pair.master.take_writer().map_err(|_| PtyError::Start)?;
        let mut command = CommandBuilder::new(select_shell(configured_shell)?);
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("YGG_TERMINAL", "1");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|_| PtyError::Start)?;
        let killer = child.clone_killer();
        let (events, _) = broadcast::channel(PTY_OUTPUT_CHANNEL_CAPACITY);
        let created_at = now_millis();
        let last_used_order = next_pty_access_order();
        let session = Arc::new(Self {
            id,
            owner_key,
            cwd,
            created_at,
            last_used_at: AtomicU64::new(created_at),
            last_used_order: AtomicU64::new(last_used_order),
            alive: AtomicBool::new(true),
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(Some(writer)),
            killer: Mutex::new(killer),
            output: Mutex::new(PtyOutput::default()),
            events,
        });

        let output_session = Arc::clone(&session);
        if thread::Builder::new()
            .name("ygg-pty-output".into())
            .spawn(move || read_output(reader, output_session))
            .is_err()
        {
            session.stop();
            return Err(PtyError::Start);
        }
        let exit_session = Arc::clone(&session);
        if thread::Builder::new()
            .name("ygg-pty-exit".into())
            .spawn(move || wait_for_exit(child, exit_session))
            .is_err()
        {
            session.stop();
            return Err(PtyError::Start);
        }
        Ok(session)
    }

    fn attach(&self) -> PtyAttachment {
        // Output writers use this lock while broadcasting. Subscribing and
        // snapshotting under the same lock keeps replay and live output ordered.
        let output = self.output.lock().expect("terminal output poisoned");
        let events = self.events.subscribe();
        let replay = String::from_utf8_lossy(&output.replay.iter().copied().collect::<Vec<_>>())
            .into_owned();
        PtyAttachment {
            id: self.id.clone(),
            owner_key: self.owner_key.clone(),
            replay,
            exit: output.exit.clone(),
            events,
        }
    }

    fn append_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let output = String::from_utf8_lossy(bytes).into_owned();
        let mut retained = self.output.lock().expect("terminal output poisoned");
        for byte in bytes {
            if retained.replay.len() == MAX_PTY_REPLAY_BYTES {
                retained.replay.pop_front();
            }
            retained.replay.push_back(*byte);
        }
        let _ = self.events.send(PtyEvent::Output(output));
    }

    fn finish(&self, exit: PtyExit) {
        self.alive.store(false, Ordering::Release);
        let mut output = self.output.lock().expect("terminal output poisoned");
        if output.exit.is_none() {
            output.exit = Some(exit.clone());
            let _ = self.events.send(PtyEvent::Exit(exit));
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(PtyError::Exited);
        }
        let master = self.master.lock().expect("terminal master poisoned");
        let master = master.as_ref().ok_or(PtyError::Exited)?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| PtyError::Exited)
    }

    fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(PtyError::Exited);
        }
        let mut writer = self.writer.lock().expect("terminal writer poisoned");
        let writer = writer.as_mut().ok_or(PtyError::Exited)?;
        writer.write_all(bytes).map_err(PtyError::Io)?;
        writer.flush().map_err(PtyError::Io)
    }

    fn stop(&self) {
        self.writer.lock().expect("terminal writer poisoned").take();
        self.master.lock().expect("terminal master poisoned").take();
        let _ = self.killer.lock().expect("terminal killer poisoned").kill();
    }

    fn touch(&self) {
        self.last_used_at.store(now_millis(), Ordering::Release);
        self.last_used_order
            .store(next_pty_access_order(), Ordering::Release);
    }

    fn info(&self) -> TerminalSession {
        TerminalSession {
            id: self.id.clone(),
            owner_key: self.owner_key.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
            last_used_at: self.last_used_at.load(Ordering::Acquire),
            alive: self.alive.load(Ordering::Acquire),
        }
    }
}

fn read_output(mut reader: Box<dyn Read + Send>, session: Arc<PtySession>) {
    let mut buffer = [0; PTY_READ_BUFFER_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => session.append_output(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn wait_for_exit(mut child: Box<dyn Child + Send + Sync>, session: Arc<PtySession>) {
    let exit = match child.wait() {
        Ok(status) => PtyExit {
            exit_code: status.exit_code(),
            signal: status.signal().map(ToOwned::to_owned),
        },
        Err(_) => PtyExit {
            exit_code: 1,
            signal: None,
        },
    };
    session.finish(exit);
}

fn remove_session(sessions: &mut PtySessions, id: &str) -> Option<Arc<PtySession>> {
    let session = sessions.by_id.remove(id)?;
    if sessions
        .by_owner
        .get(&session.owner_key)
        .is_some_and(|known_id| known_id == id)
    {
        sessions.by_owner.remove(&session.owner_key);
    }
    Some(session)
}

fn remove_exited_sessions(sessions: &mut PtySessions) -> Vec<Arc<PtySession>> {
    let exited = sessions
        .by_id
        .iter()
        .filter(|(_, session)| !session.alive.load(Ordering::Acquire))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    exited
        .into_iter()
        .filter_map(|id| remove_session(sessions, &id))
        .collect()
}

fn evict_lru_if_full(sessions: &mut PtySessions) -> Option<Arc<PtySession>> {
    if sessions.by_id.len() < MAX_PTY_SESSIONS {
        return None;
    }
    let id = sessions
        .by_id
        .iter()
        .min_by_key(|(_, session)| session.last_used_order.load(Ordering::Acquire))
        .map(|(id, _)| id.clone())?;
    remove_session(sessions, &id)
}

fn validate_dimensions(cols: u16, rows: u16) -> Result<(), PtyError> {
    if cols == 0 || rows == 0 || cols > MAX_PTY_COLUMNS || rows > MAX_PTY_ROWS {
        return Err(PtyError::InvalidDimensions);
    }
    Ok(())
}

fn validate_owner_key(owner_key: &str) -> Result<(), PtyError> {
    if owner_key.is_empty()
        || owner_key.len() > 128
        || !owner_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(PtyError::InvalidOwnerKey);
    }
    Ok(())
}

fn validate_session_id(id: &str) -> Result<(), PtyError> {
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PtyError::InvalidSessionId);
    }
    Ok(())
}

fn select_shell(configured_shell: Option<PathBuf>) -> Result<PathBuf, PtyError> {
    if let Some(shell) = configured_shell {
        return shell.is_file().then_some(shell).ok_or(PtyError::Start);
    }
    if let Some(shell) = std::env::var_os("SHELL").map(PathBuf::from) {
        if shell.is_absolute() && shell.is_file() {
            return Ok(shell);
        }
    }
    #[cfg(target_os = "macos")]
    let fallback = PathBuf::from("/bin/zsh");
    #[cfg(all(unix, not(target_os = "macos")))]
    let fallback = PathBuf::from("/bin/bash");
    #[cfg(windows)]
    let fallback = PathBuf::from("cmd.exe");
    #[cfg(not(any(unix, windows)))]
    let fallback = PathBuf::from("sh");
    if fallback.is_absolute() && !fallback.is_file() {
        return Err(PtyError::Start);
    }
    Ok(fallback)
}

fn random_hex(bytes: usize) -> Result<String, PtyError> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value).map_err(|_| PtyError::Start)?;
    Ok(value.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn next_pty_access_order() -> u64 {
    NEXT_PTY_ACCESS_ORDER.fetch_add(1, Ordering::Relaxed)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn rejects_unbounded_protocol_values() {
        assert!(matches!(
            validate_dimensions(0, 24),
            Err(PtyError::InvalidDimensions)
        ));
        assert!(matches!(
            validate_dimensions(80, 0),
            Err(PtyError::InvalidDimensions)
        ));
        assert!(matches!(
            validate_dimensions(MAX_PTY_COLUMNS + 1, 24),
            Err(PtyError::InvalidDimensions)
        ));
        assert!(matches!(
            validate_owner_key(""),
            Err(PtyError::InvalidOwnerKey)
        ));
        assert!(matches!(
            validate_owner_key("owner key"),
            Err(PtyError::InvalidOwnerKey)
        ));
        assert!(matches!(
            validate_session_id("not-a-terminal-id"),
            Err(PtyError::InvalidSessionId)
        ));
        assert!(validate_session_id(&"a".repeat(32)).is_ok());
        assert!(validate_owner_key("pane-01_b.c").is_ok());
    }

    #[cfg(unix)]
    fn test_manager() -> PtyManager {
        PtyManager::new(TerminalConfig {
            cwd: std::env::temp_dir(),
            shell: Some(PathBuf::from("/bin/sh")),
        })
        .unwrap()
    }

    #[cfg(unix)]
    fn open(manager: &PtyManager, owner_key: &str) -> PtyAttachment {
        manager
            .open(PtyOpenRequest {
                cols: 80,
                rows: 24,
                owner_key: Some(owner_key.into()),
                cwd: None,
            })
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reattaches_by_owner_key_with_replay() {
        let manager = test_manager();
        let mut attachment = open(&manager, "owner-a");
        manager
            .input(&attachment.id, "printf 'hello from pty\n'\n")
            .unwrap();
        let output = recv_output_containing(&mut attachment.events, "hello from pty").await;
        assert!(output.contains("hello from pty"));

        let reattached = open(&manager, "owner-a");
        assert_eq!(reattached.id, attachment.id);
        assert!(reattached.replay.contains("hello from pty"));
        manager.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn evicts_the_least_recently_used_session_at_capacity() {
        let manager = test_manager();
        let first = open(&manager, "owner-0");
        let second = open(&manager, "owner-1");
        for index in 2..MAX_PTY_SESSIONS {
            let _ = open(&manager, &format!("owner-{index}"));
        }
        assert_eq!(open(&manager, "owner-0").id, first.id);

        let _ = open(&manager, "owner-new");
        assert!(matches!(
            manager.session_info(&second.id),
            Err(PtyError::NotFound)
        ));
        assert!(manager.session_info(&first.id).is_ok());
        manager.shutdown();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn emits_shell_exit_and_rejects_later_input() {
        let manager = test_manager();
        let mut attachment = open(&manager, "owner-exit");
        manager.input(&attachment.id, "exit 7\n").unwrap();
        let exit = recv_exit(&mut attachment.events).await;
        assert_eq!(exit.exit_code, 7);
        assert!(matches!(
            manager.input(&attachment.id, "echo no\\n"),
            Err(PtyError::Exited)
        ));
        manager.shutdown();
    }

    #[cfg(unix)]
    async fn recv_output_containing(
        events: &mut broadcast::Receiver<PtyEvent>,
        expected: &str,
    ) -> String {
        let mut output = String::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
                Ok(Ok(PtyEvent::Output(chunk))) => {
                    output.push_str(&chunk);
                    if output.contains(expected) {
                        return output;
                    }
                }
                Ok(Ok(PtyEvent::Exit(exit))) => panic!("shell exited early: {exit:?}"),
                Ok(Err(error)) => panic!("terminal stream ended: {error}"),
                Err(_) => panic!("timed out waiting for terminal output"),
            }
        }
    }

    #[cfg(unix)]
    async fn recv_exit(events: &mut broadcast::Receiver<PtyEvent>) -> PtyExit {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
                Ok(Ok(PtyEvent::Exit(exit))) => return exit,
                Ok(Ok(PtyEvent::Output(_))) => {}
                Ok(Err(error)) => panic!("terminal stream ended: {error}"),
                Err(_) => panic!("timed out waiting for shell exit"),
            }
        }
    }
}
