#![allow(missing_docs)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ygg_agent::extension_process::{ConfirmationRequest, ExtensionInputRequest};
use ygg_agent::tool::{ToolConfirmation, ToolInputRequest};
use ygg_ai::{ModelCatalog, ModelId};

use crate::config::ThinkingLevel;
use crate::modes::interactive::run_blocking_lifecycle;
use crate::presentation::{format_token_rate_value, ModelDisplayMetadata};
use crate::session_store::{SessionMeta, SessionStorageLifecycle, SessionStore};
use crate::tui::view::{
    ForkMessage, InteractiveShell, MessagePicker, Panel, PanelAction, PanelRequest, PanelResult,
    PickerState,
};

const MAX_SECRET_INPUT_BYTES: usize = 4096;
#[cfg(not(test))]
const SUBAGENT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(test)]
const SUBAGENT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Default)]
struct SecretInputBuffer(Vec<u8>);

impl SecretInputBuffer {
    fn push(&mut self, character: char) {
        let mut encoded = [0; 4];
        let bytes = character.encode_utf8(&mut encoded).as_bytes();
        if self.0.len().saturating_add(bytes.len()) <= MAX_SECRET_INPUT_BYTES {
            self.0.extend_from_slice(bytes);
        }
        encoded.fill(0);
    }

    fn extend_paste(&mut self, pasted: &str) {
        let pasted = pasted.trim_end_matches(['\r', '\n']);
        let remaining = MAX_SECRET_INPUT_BYTES.saturating_sub(self.0.len());
        let mut end = pasted.len().min(remaining);
        while end > 0 && !pasted.is_char_boundary(end) {
            end -= 1;
        }
        self.0.extend_from_slice(&pasted.as_bytes()[..end]);
    }

    fn backspace(&mut self) {
        let Some((start, _)) = std::str::from_utf8(&self.0)
            .ok()
            .and_then(|text| text.char_indices().last())
        else {
            return;
        };
        self.0[start..].fill(0);
        self.0.truncate(start);
    }

    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for SecretInputBuffer {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Give one running tool exclusive ownership of terminal input. The answer is
/// sent directly to its reply channel and never enters the ordinary editor.
pub async fn tool_input_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    request: &ToolInputRequest,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    shell.set_tool_input_prompt(Some(request.prompt.clone()));
    shell.render();
    let mut secret = SecretInputBuffer::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => None,
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                request.cancel();
                shell.set_tool_input_prompt(None);
                shell.render();
                return Err(error.into());
            }
            None => {
                request.cancel();
                shell.set_tool_input_prompt(None);
                shell.render();
                return Ok(false);
            }
        };
        if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
            request.cancel();
            shell.set_tool_input_prompt(None);
            shell.request_close();
            shell.render();
            return Ok(false);
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Enter => {
                        request.respond(secret.take());
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(true);
                    }
                    KeyCode::Esc => {
                        request.cancel();
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(false);
                    }
                    KeyCode::Backspace => secret.backspace(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        request.cancel();
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(false);
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                    {
                        secret.push(character)
                    }
                    _ => {}
                }
            }
            Event::Paste(pasted) => secret.extend_paste(&pasted),
            Event::Resize(columns, rows) => shell.set_size(columns, rows),
            _ => {}
        }
        // Re-rendering is safe: only the fixed prompt and cursor are visible;
        // secret bytes never influence frame contents.
        shell.render();
    }
}

/// Give one extension command exclusive ownership of terminal input. Secret
/// answers never enter the ordinary editor or rendered frame; non-secret setup
/// values use the same temporary composer surface and are echoed while typed.
pub async fn extension_input_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    request: &ExtensionInputRequest,
) -> anyhow::Result<Option<String>>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    shell.set_tool_input_prompt(Some(request.prompt.clone()));
    shell.render();
    let mut value = SecretInputBuffer::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => None,
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                shell.set_tool_input_prompt(None);
                shell.render();
                return Err(error.into());
            }
            None => {
                shell.set_tool_input_prompt(None);
                shell.render();
                return Ok(None);
            }
        };
        if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
            shell.set_tool_input_prompt(None);
            shell.request_close();
            shell.render();
            return Ok(None);
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Enter => {
                        let bytes = value.take();
                        let answer = String::from_utf8(bytes)
                            .map_err(|_| anyhow::anyhow!("extension input was not valid UTF-8"))?;
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(Some(answer));
                    }
                    KeyCode::Esc => {
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(None);
                    }
                    KeyCode::Backspace => value.backspace(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(None);
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                    {
                        value.push(character)
                    }
                    _ => {}
                }
            }
            Event::Paste(pasted) => value.extend_paste(&pasted),
            Event::Resize(columns, rows) => shell.set_size(columns, rows),
            _ => {}
        }
        let shown = if request.secret {
            request.prompt.clone()
        } else {
            let entered = std::str::from_utf8(&value.0).unwrap_or_default();
            format!("{} {}", request.prompt, entered)
        };
        shell.set_tool_input_prompt(Some(shown));
        shell.render();
    }
}

/// Drive a panel-based selection list. Owns the event loop while the panel is open.
async fn pick_list<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    title: &str,
    items: Vec<String>,
    descriptions: Vec<Option<String>>,
    initial_selected: usize,
    action: PanelAction,
) -> anyhow::Result<Option<usize>>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    if items.is_empty() {
        shell.error("nothing is available to select".into());
        shell.render();
        return Ok(None);
    }

    let initial_selected = initial_selected.min(items.len().saturating_sub(1));
    shell.open_panel(Panel::SelectList {
        title: title.into(),
        items,
        descriptions,
        selected: initial_selected,
        filter: String::new(),
        action,
    });
    shell.render();

    loop {
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.close_panel();
                return Ok(None);
            }
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                shell.close_panel();
                return Err(error.into());
            }
            None => {
                shell.close_panel();
                return Ok(None);
            }
        };
        if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
            shell.close_panel();
            shell.request_close();
            shell.render();
            return Ok(None);
        }
        // Mouse events pass through to the shell for transcript scrolling.
        if matches!(event, Event::Mouse(_)) {
            continue;
        }
        if let Some((result, _action)) = shell.panel_input(&event) {
            shell.render();
            return Ok(match result {
                PanelResult::Confirm(index) => Some(index),
                PanelResult::Cancel => None,
                PanelResult::Select(_) => None,
            });
        }
        // Panel consumed the event; render updated state.
        shell.render();
    }
}

/// Select one installed executable extension. Enter confirms the highlighted
/// row; Escape closes the management view without changing configuration.
pub async fn extension_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    title: &str,
    items: Vec<String>,
    descriptions: Vec<Option<String>>,
    initial_selected: usize,
) -> anyhow::Result<Option<usize>>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    let action_items = items.clone();
    pick_list(
        shell,
        input,
        title,
        items,
        descriptions,
        initial_selected,
        PanelAction::SelectExtension(action_items),
    )
    .await
}

/// Complete live subagent list replacement supplied by the owning product loop.
pub struct SubagentPickerSnapshot {
    pub title: String,
    pub items: Vec<String>,
    pub descriptions: Vec<Option<String>>,
    pub node_ids: Vec<String>,
    pub notices: Vec<String>,
}

/// Select one subagent node while periodically refreshing presentation state.
/// Selection is preserved by stable node ID and the returned ID is revalidated
/// by the caller before opening a transcript.
pub async fn subagent_picker<S, C, F>(
    shell: &mut InteractiveShell,
    input: &mut S,
    initial: SubagentPickerSnapshot,
    initial_selected: usize,
    context: &mut C,
    mut refresh: F,
) -> anyhow::Result<Option<String>>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
    F: for<'a> FnMut(&'a mut C) -> Pin<Box<dyn Future<Output = SubagentPickerSnapshot> + 'a>>,
{
    if initial.items.is_empty() {
        return Ok(None);
    }
    let selected = initial_selected.min(initial.items.len().saturating_sub(1));
    shell.open_panel(Panel::SelectList {
        title: initial.title,
        items: initial.items,
        descriptions: initial.descriptions,
        selected,
        filter: String::new(),
        action: PanelAction::SelectSubagent(initial.node_ids),
    });
    for notice in initial.notices {
        shell.notice(notice);
    }
    shell.render();

    let mut refresh_tick = tokio::time::interval(SUBAGENT_REFRESH_INTERVAL);
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = refresh_tick.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.close_panel();
                shell.request_close();
                shell.render();
                return Ok(None);
            }
            next = input.next() => {
                let event = match next {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => {
                        shell.close_panel();
                        return Err(error.into());
                    }
                    None => {
                        shell.close_panel();
                        return Ok(None);
                    }
                };
                if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
                    shell.close_panel();
                    shell.request_close();
                    shell.render();
                    return Ok(None);
                }
                if matches!(event, Event::Mouse(_)) {
                    continue;
                }
                if let Some((result, action)) = shell.panel_input(&event) {
                    shell.render();
                    return Ok(match (result, action) {
                        (PanelResult::Confirm(index), PanelAction::SelectSubagent(node_ids)) => {
                            node_ids.get(index).cloned()
                        }
                        (PanelResult::Cancel, _) => None,
                        _ => None,
                    });
                }
                shell.render();
            }
            _ = refresh_tick.tick() => {
                let snapshot = refresh(context).await;
                for notice in snapshot.notices {
                    shell.notice(notice);
                }
                shell.refresh_subagent_panel(
                    snapshot.title,
                    snapshot.items,
                    snapshot.descriptions,
                    snapshot.node_ids,
                );
                shell.render();
            }
        }
    }
}

/// Show a bounded read-only document. Arrow and page keys scroll; Escape or
/// Left returns to the owning list instead of closing the Ygg session.
pub async fn read_only_document<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    title: impl Into<String>,
    text: String,
) -> anyhow::Result<()>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    shell.open_panel(Panel::ReadOnlyDocument {
        title: title.into(),
        text: crate::tui::view::sanitize_for_terminal(&text),
        styled: false,
        scroll_from_bottom: 0,
    });
    shell.render();
    loop {
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.close_panel();
                shell.request_close();
                return Ok(());
            }
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                shell.close_panel();
                return Err(error.into());
            }
            None => {
                shell.close_panel();
                shell.request_close();
                return Ok(());
            }
        };
        if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
            shell.close_panel();
            shell.request_close();
            shell.render();
            return Ok(());
        }
        if matches!(event, Event::Mouse(_)) {
            continue;
        }
        if shell.panel_input(&event).is_some() {
            shell.render();
            return Ok(());
        }
        shell.render();
    }
}

/// Styled variant of [`read_only_document`]: the producer sanitizes its
/// content once and applies trusted theme ANSI, which rendering preserves.
pub async fn read_only_document_live_styled<S, F, Fut>(
    shell: &mut InteractiveShell,
    input: &mut S,
    title: impl Into<String>,
    text: String,
    mut refresh: F,
) -> anyhow::Result<()>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Option<String>>>,
{
    shell.open_panel(Panel::ReadOnlyDocument {
        title: title.into(),
        text,
        styled: true,
        scroll_from_bottom: 0,
    });
    shell.render();
    let mut refresh_tick = tokio::time::interval(SUBAGENT_REFRESH_INTERVAL);
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.close_panel();
                shell.request_close();
                return Ok(());
            }
            next = input.next() => {
                let event = match next {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => {
                        shell.close_panel();
                        return Err(error.into());
                    }
                    None => {
                        shell.close_panel();
                        shell.request_close();
                        return Ok(());
                    }
                };
                if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
                    shell.close_panel();
                    shell.render();
                    return Ok(());
                }
                if matches!(event, Event::Mouse(_)) {
                    continue;
                }
                if shell.panel_input(&event).is_some() {
                    shell.render();
                    return Ok(());
                }
                shell.render();
            }
            _ = refresh_tick.tick() => {
                if let Ok(Some(text)) = refresh().await {
                    shell.update_read_only_document_styled(text);
                    shell.render();
                }
            }
        }
    }
}

/// Hide recoverably trashed sessions from resume/fork browsing.
fn picker_rows(sessions: impl Iterator<Item = SessionMeta>) -> Vec<SessionMeta> {
    sessions
        .filter(|session| session.trashed_at_ms.is_none())
        .collect()
}

/// Ask the user to select a stored session from a precomputed snapshot.
pub async fn session_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    sessions: &[SessionMeta],
    store: &SessionStore,
    current_session_path: Option<&Path>,
) -> anyhow::Result<Option<PathBuf>> {
    let mut rows = picker_rows(sessions.iter().cloned());
    if rows.is_empty() {
        shell.error(format!("no sessions in {}", store.dir().display()));
        shell.render();
        return Ok(None);
    }

    let current_session_path = current_session_path.map(Path::to_owned);
    let mut all_rows = None;
    shell.open_panel(Panel::SessionPicker {
        picker: PickerState::new(rows.clone(), current_session_path),
    });
    shell.render();

    loop {
        let requests = shell.drain_panel_requests();
        for request in requests {
            match request {
                PanelRequest::LoadAll => {
                    let discovery_store = store.clone();
                    let discovered = run_blocking_lifecycle(
                        shell,
                        input,
                        "discovering sessions in all workspaces…",
                        move || Ok(discovery_store.list_all()),
                    )
                    .await?;
                    all_rows = Some(picker_rows(discovered.into_iter()));
                    shell.refresh_panel_sessions(rows.clone(), all_rows.clone());
                    shell.render();
                }
                PanelRequest::TrashSession { path, .. } => {
                    let Some(id) = session_id_from_path(&path) else {
                        shell.set_picker_message(
                            "Failed to trash session: path has no valid id",
                            std::time::Duration::from_secs(3),
                        );
                        continue;
                    };
                    let target_store = store_for_session_path(store, &path);
                    let changed_at_ms = unix_now_ms();
                    let result = run_blocking_lifecycle(
                        shell,
                        input,
                        "moving session to trash…",
                        move || {
                            target_store
                                .set_lifecycle(&id, SessionStorageLifecycle::Trash, changed_at_ms)
                                .map(|_| ())
                        },
                    )
                    .await;
                    match result {
                        Ok(()) => {
                            let (next_rows, next_all) =
                                refresh_session_rows(shell, input, store, all_rows.is_some())
                                    .await?;
                            rows = next_rows;
                            all_rows = next_all;
                            shell.refresh_panel_sessions(rows.clone(), all_rows.clone());
                            shell.set_picker_message(
                                "Session moved to trash",
                                std::time::Duration::from_secs(2),
                            );
                        }
                        Err(error) => shell.set_picker_message(
                            format!("Failed to trash session: {error}"),
                            std::time::Duration::from_secs(3),
                        ),
                    }
                    shell.render();
                }
                PanelRequest::RenameSession { path, name, .. } => {
                    let Some(id) = session_id_from_path(&path) else {
                        shell.set_picker_message(
                            "Failed to rename session: path has no valid id",
                            std::time::Duration::from_secs(3),
                        );
                        continue;
                    };
                    let target_store = store_for_session_path(store, &path);
                    let result =
                        run_blocking_lifecycle(shell, input, "renaming session…", move || {
                            target_store.rename(&id, &name).map(|_| ())
                        })
                        .await;
                    match result {
                        Ok(()) => {
                            let (next_rows, next_all) =
                                refresh_session_rows(shell, input, store, all_rows.is_some())
                                    .await?;
                            rows = next_rows;
                            all_rows = next_all;
                            shell.refresh_panel_sessions(rows.clone(), all_rows.clone());
                            shell.set_picker_message(
                                "Session renamed",
                                std::time::Duration::from_secs(2),
                            );
                        }
                        Err(error) => shell.set_picker_message(
                            format!("Failed to rename session: {error}"),
                            std::time::Duration::from_secs(3),
                        ),
                    }
                    shell.render();
                }
            }
        }

        if shell.close_requested() {
            shell.close_panel();
            return Ok(None);
        }
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.close_panel();
                return Ok(None);
            }
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                shell.close_panel();
                return Err(error.into());
            }
            None => {
                shell.close_panel();
                shell.request_close();
                return Ok(None);
            }
        };
        if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
            shell.close_panel();
            shell.request_close();
            shell.render();
            return Ok(None);
        }
        if matches!(event, Event::Mouse(_)) {
            continue;
        }
        if let Some((result, _action)) = shell.panel_input(&event) {
            shell.render();
            match result {
                PanelResult::Cancel => return Ok(None),
                PanelResult::Select(_) => {
                    let Some((id, path)) = shell.take_picker_selection() else {
                        return Ok(None);
                    };
                    if !selection_is_in_current_workspace(store, &rows, &all_rows, &id, &path) {
                        shell.notice_error("cannot resume a session from another workspace");
                        shell.render();
                        return Ok(None);
                    }
                    return Ok(Some(path));
                }
                PanelResult::Confirm(_) => {}
            }
        }
        shell.render();
    }
}

fn session_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn store_for_session_path(base: &SessionStore, path: &Path) -> SessionStore {
    let Some(directory) = path.parent() else {
        return base.clone();
    };
    if directory == base.dir() {
        base.clone()
    } else {
        SessionStore::for_directory(directory, base.root())
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn refresh_session_rows(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    store: &SessionStore,
    include_all: bool,
) -> anyhow::Result<(Vec<SessionMeta>, Option<Vec<SessionMeta>>)> {
    let store = store.clone();
    run_blocking_lifecycle(shell, input, "refreshing sessions…", move || {
        let rows = picker_rows(store.list().into_iter());
        let all = include_all.then(|| picker_rows(store.list_all().into_iter()));
        Ok((rows, all))
    })
    .await
}

fn selection_is_in_current_workspace(
    store: &SessionStore,
    rows: &[SessionMeta],
    all_rows: &Option<Vec<SessionMeta>>,
    id: &str,
    path: &Path,
) -> bool {
    let meta = rows
        .iter()
        .chain(all_rows.as_deref().unwrap_or(&[]).iter())
        .find(|meta| meta.id == id && meta.path == path);
    match meta.and_then(|meta| meta.workspace.as_deref()) {
        Some(workspace) => store.workspace() == Some(workspace),
        None => path.parent() == Some(store.dir()),
    }
}

/// Ask the user to choose a message boundary for `/fork`.
pub async fn message_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    messages: Vec<ForkMessage>,
) -> anyhow::Result<Option<(String, String)>>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    if messages.is_empty() {
        return Ok(None);
    }
    shell.open_panel(Panel::MessagePicker {
        picker: MessagePicker::new(messages),
    });
    shell.render();
    loop {
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.close_panel();
                return Ok(None);
            }
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                shell.close_panel();
                return Err(error.into());
            }
            None => {
                shell.close_panel();
                shell.request_close();
                return Ok(None);
            }
        };
        if matches!(&event, Event::Key(key) if crate::tui::keymap::is_close_key(key)) {
            shell.close_panel();
            shell.request_close();
            shell.render();
            return Ok(None);
        }
        if matches!(event, Event::Mouse(_)) {
            continue;
        }
        if shell.close_requested() {
            shell.close_panel();
            return Ok(None);
        }
        if let Some((result, _action)) = shell.panel_input(&event) {
            shell.render();
            match result {
                PanelResult::Cancel => return Ok(None),
                PanelResult::Select(_) => return Ok(shell.take_message_picker_selection()),
                PanelResult::Confirm(_) => {}
            }
        }
        shell.render();
    }
}

/// Ask the user to select a capability-supported thinking level.
pub async fn thinking_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    levels: &[ThinkingLevel],
) -> anyhow::Result<Option<ThinkingLevel>> {
    let items: Vec<String> = levels.iter().map(|l| l.label().into()).collect();
    let action_levels = levels.to_vec();
    let Some(index) = pick_list(
        shell,
        input,
        "Select thinking level",
        items,
        vec![None; levels.len()],
        0,
        PanelAction::SelectThinking(action_levels),
    )
    .await?
    else {
        return Ok(None);
    };
    let selected = levels[index];
    if let Err(e) = crate::cli::persist_reasoning(selected.label()) {
        shell.error(format!("failed to save thinking preference: {e}"));
    }
    Ok(Some(selected))
}

/// Ask the user to approve a typed tool request. Escape and input
/// closure are denials; approval is never inferred from a missing frontend.
pub async fn confirmation_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    request: &ToolConfirmation,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    confirmation_prompt_picker(
        shell,
        input,
        &request.prompt,
        request.detail.as_deref(),
        request.destructive,
        request.default,
    )
    .await
}

pub async fn extension_confirmation_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    extension: &str,
    request: &ConfirmationRequest,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    let prompt = format!("{extension}: {}", request.prompt);
    confirmation_prompt_picker(
        shell,
        input,
        &prompt,
        request.detail.as_deref(),
        request.destructive,
        request.default,
    )
    .await
}

async fn confirmation_prompt_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    prompt: &str,
    _detail: Option<&str>,
    destructive: bool,
    default: bool,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    let (items, decisions) = if default {
        (vec!["Approve".to_owned(), "Deny".to_owned()], [true, false])
    } else {
        (vec!["Deny".to_owned(), "Approve".to_owned()], [false, true])
    };
    // The prompt already explains the requested effect. Keep hashes and other
    // wire-level detail out of the two-choice picker; they belong in logs and
    // diagnostics, not beside both answers.
    let descriptions = vec![None, None];
    let title = if destructive {
        format!("Action requires approval · {prompt}")
    } else {
        prompt.to_owned()
    };
    let selected = pick_list(
        shell,
        input,
        &title,
        items,
        descriptions,
        0,
        PanelAction::Confirmation,
    )
    .await?;
    Ok(selected.map(|index| decisions[index]).unwrap_or(false))
}

/// Build a concise human-facing label from the same cached metadata boundary
/// used by the footer. Canonical and wire-level IDs remain available in
/// `/status`; the endpoint label disambiguates models from different custom
/// providers.
fn model_label(model: &ygg_ai::ModelSpec) -> String {
    ModelDisplayMetadata::resolve(model).name
}

fn model_label_with_endpoint(catalog: &ModelCatalog, model: &ygg_ai::ModelSpec) -> String {
    let model_name = model_label(model);
    catalog
        .endpoint_label(&model.endpoint)
        .map(|label| format!("{label} · {model_name}"))
        .unwrap_or(model_name)
}

#[cfg(test)]
fn model_description(model: &ygg_ai::ModelSpec) -> String {
    model_description_with_endpoint(model, None)
}

fn model_description_with_endpoint(
    model: &ygg_ai::ModelSpec,
    endpoint_label: Option<&str>,
) -> String {
    let context = match model.limits.context_window {
        value if value >= 1_000_000 => format!("{}M", value / 1_000_000),
        value if value >= 1_000 => format!("{}k", value / 1_000),
        value => value.to_string(),
    };
    let pricing = model
        .pricing
        .as_ref()
        .map(|pricing| {
            format!(
                "{}/{} per M · cache-read {} per M",
                format_token_rate_value(pricing.input),
                format_token_rate_value(pricing.output),
                format_token_rate_value(pricing.cache_read),
            )
        })
        .unwrap_or_else(|| "pricing unavailable ($?)".to_owned());
    let mut details = vec![format!(
        "{} · {pricing} · {context} context",
        endpoint_label.unwrap_or(&model.endpoint.0)
    )];
    if model.capabilities.tools {
        details.push("tools".into());
    }
    if model
        .capabilities
        .input_modalities
        .contains(ygg_ai::Modality::Image)
    {
        details.push("vision".into());
    }
    details.join(" · ")
}

/// Ask the user to select one model, preserving cancellation for workflows
/// such as `/logout` that must not mutate credentials until a replacement model
/// has been chosen.
pub async fn optional_model_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    catalog: &ModelCatalog,
) -> anyhow::Result<Option<ModelId>> {
    let mut models = catalog.models().collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    let model_ids: Vec<ModelId> = models.iter().map(|m| m.id.clone()).collect();
    let items: Vec<String> = models
        .iter()
        .map(|model| model_label_with_endpoint(catalog, model))
        .collect();
    let descs: Vec<Option<String>> = models
        .iter()
        .map(|model| {
            Some(model_description_with_endpoint(
                model,
                catalog.endpoint_label(&model.endpoint),
            ))
        })
        .collect();

    let Some(index) = pick_list(
        shell,
        input,
        "Select model",
        items,
        descs,
        0,
        PanelAction::SelectModel(model_ids),
    )
    .await?
    else {
        return Ok(None);
    };
    let selected_id = models[index].id.0.clone();
    if let Err(e) = crate::cli::persist_model(&selected_id) {
        shell.error(format!("failed to save model preference: {e}"));
    }
    Ok(Some(ModelId(selected_id)))
}

/// Ask the user to select one model from the active catalog.
pub async fn model_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    catalog: &ModelCatalog,
) -> anyhow::Result<ModelId> {
    optional_model_picker(shell, input, catalog)
        .await?
        .ok_or_else(|| anyhow::anyhow!("model selection cancelled"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use tokio_stream::wrappers::ReceiverStream;

    #[tokio::test]
    async fn ctrl_d_closes_a_picker_and_propagates_the_close_request() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            ))))
            .await
            .unwrap();
        drop(sender);
        let mut input = ReceiverStream::new(receiver);
        let mut shell = InteractiveShell::test_shell();

        let selected = pick_list(
            &mut shell,
            &mut input,
            "Choose",
            vec!["one".into()],
            vec![None],
            0,
            PanelAction::SelectModel(vec![ModelId("one".into())]),
        )
        .await
        .unwrap();

        assert_eq!(selected, None);
        assert!(!shell.has_panel());
        assert!(shell.close_requested());
    }

    #[tokio::test]
    async fn message_picker_driver_returns_the_selected_message() {
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            ))))
            .await
            .unwrap();
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))))
            .await
            .unwrap();
        drop(sender);
        let mut input = ReceiverStream::new(receiver);
        let mut shell = InteractiveShell::test_shell();
        let selected = message_picker(
            &mut shell,
            &mut input,
            vec![
                ForkMessage {
                    entry_id: "entry-a".into(),
                    text: "first".into(),
                    whole_conversation: false,
                },
                ForkMessage {
                    entry_id: "entry-b".into(),
                    text: "second".into(),
                    whole_conversation: false,
                },
            ],
        )
        .await
        .unwrap();

        assert_eq!(selected, Some(("entry-a".into(), "first".into())));
        assert!(!shell.has_panel());
    }

    struct LivePickerRefresh {
        calls: usize,
    }

    fn refresh_live_picker(
        context: &mut LivePickerRefresh,
    ) -> Pin<Box<dyn Future<Output = SubagentPickerSnapshot> + '_>> {
        Box::pin(async move {
            context.calls += 1;
            SubagentPickerSnapshot {
                title: "Subagents · refreshed".into(),
                items: vec!["beta".into(), "gamma".into()],
                descriptions: vec![Some("done".into()), Some("running".into())],
                node_ids: vec!["node-b".into(), "node-c".into()],
                notices: Vec::new(),
            }
        })
    }

    #[tokio::test]
    async fn live_subagent_picker_refreshes_and_keeps_the_stable_selection() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            sender
                .send(Ok(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                ))))
                .await
                .unwrap();
        });
        let mut input = ReceiverStream::new(receiver);
        let mut shell = InteractiveShell::test_shell();
        let mut refresh = LivePickerRefresh { calls: 0 };
        let selected = subagent_picker(
            &mut shell,
            &mut input,
            SubagentPickerSnapshot {
                title: "Subagents".into(),
                items: vec!["alpha".into(), "beta".into()],
                descriptions: vec![Some("running".into()), Some("running".into())],
                node_ids: vec!["node-a".into(), "node-b".into()],
                notices: Vec::new(),
            },
            1,
            &mut refresh,
            refresh_live_picker,
        )
        .await
        .unwrap();

        assert_eq!(selected.as_deref(), Some("node-b"));
        assert!(refresh.calls >= 1);
    }

    #[test]
    fn model_label_uses_friendly_metadata_without_wire_id_noise() {
        let spec = ygg_ai::ModelSpec {
            id: ModelId("my-custom".into()),
            endpoint: ygg_ai::EndpointId("local".into()),
            api_name: "llama-3.1-8b-instruct".into(),
            display_name: Some("Llama 3.1 8B".into()),
            protocol: ygg_ai::Protocol::OpenAiChat,
            capabilities: ygg_ai::Capabilities {
                input_modalities: ygg_ai::ModalitySet::none(),
                output_modalities: ygg_ai::ModalitySet::none(),
                tools: true,
                parallel_tool_calls: false,
                reasoning: None,
                responses_lite: false,
                agent_delegation: None,
                structured_output: false,
                deferred_tool_loading: false,
            },
            limits: ygg_ai::ModelLimits {
                context_window: 131072,
                max_output_tokens: 8192,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        };
        assert_eq!(model_label(&spec), "Llama 3.1 8B");
        assert!(model_description(&spec).contains("$?"));

        let mut priced = spec.clone();
        priced.pricing = Some(ygg_ai::Pricing {
            input: ygg_ai::TokenRate(1_000_000),
            output: ygg_ai::TokenRate(6_000_000),
            cache_read: ygg_ai::TokenRate(100_000),
            cache_write_5m: ygg_ai::TokenRate(1_250_000),
            cache_write_1h: None,
            reasoning: None,
            tiers: Vec::new(),
        });
        let description = model_description(&priced);
        assert!(description.contains("$1.00/$6.00"), "{description}");
        assert!(description.contains("cache-read $0.1"), "{description}");
    }

    #[test]
    fn custom_model_label_removes_provider_repository_and_quantization_noise() {
        let spec = ygg_ai::ModelSpec {
            id: ModelId("custom/Intel/Qwen3.6-27B-int4-AutoRound".into()),
            endpoint: ygg_ai::EndpointId("custom-openai".into()),
            api_name: "Intel/Qwen3.6-27B-int4-AutoRound".into(),
            display_name: None,
            protocol: ygg_ai::Protocol::OpenAiChat,
            capabilities: ygg_ai::Capabilities {
                input_modalities: ygg_ai::ModalitySet::none(),
                output_modalities: ygg_ai::ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: None,
                responses_lite: false,
                agent_delegation: None,
                structured_output: true,
                deferred_tool_loading: false,
            },
            limits: ygg_ai::ModelLimits {
                context_window: 128000,
                max_output_tokens: 16384,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        };
        assert_eq!(model_label(&spec), "Qwen3.6 27B");

        let mut catalog = ModelCatalog::default();
        let endpoint_id = ygg_ai::EndpointId("custom-openai".into());
        catalog
            .register_endpoint(ygg_ai::Endpoint {
                id: endpoint_id.clone(),
                base_url: url::Url::parse("http://127.0.0.1:1234/v1/").unwrap(),
                auth: ygg_ai::Auth::None,
                default_headers: http::HeaderMap::new(),
                transport: ygg_ai::EndpointTransport::Http,
                timeout: std::time::Duration::from_secs(300),
            })
            .unwrap();
        catalog
            .set_endpoint_label(endpoint_id, "Apple Foundation Models")
            .unwrap();
        assert_eq!(
            model_label_with_endpoint(&catalog, &spec),
            "Apple Foundation Models · Qwen3.6 27B"
        );
    }
}
