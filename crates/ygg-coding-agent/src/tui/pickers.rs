#![allow(missing_docs)]

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use sexy_tui_rs::widgets::{SelectItem, SelectList};
use sexy_tui_rs::Component;
use ygg_ai::{ModelCatalog, ModelId};

use crate::config::ThinkingLevel;
use crate::session_store::SessionStore;
use crate::tui::keymap::encode;
use crate::tui::theme::select_list_theme;
use crate::tui::view::InteractiveShell;

fn render_picker(shell: &mut InteractiveShell, list: &SelectList, filter: &str) {
    let mut lines = vec!["Select an item".to_owned()];
    if !filter.is_empty() {
        lines.push(format!("filter: {filter}"));
    }
    lines.extend(list.render(shell.columns().saturating_sub(4)));
    lines.push("↑/↓ select · Enter confirm · Esc cancel".to_owned());
    shell.show_overlay_text(lines.join("\n"));
    shell.render();
}

/// Drive an owned sexy-tui `SelectList` with crossterm events. The shell owns
/// the visual overlay; this function owns selection and filtering state.
pub async fn pick_from(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    items: Vec<SelectItem>,
) -> anyhow::Result<Option<usize>> {
    if items.is_empty() {
        shell.error("nothing is available to select".into());
        shell.render();
        return Ok(None);
    }

    let theme = select_list_theme(&shell.theme());
    let mut list = SelectList::new(items.clone(), 12, theme);
    let mut filter = String::new();
    render_picker(shell, &list, &filter);

    loop {
        let event = match input.next().await {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                shell.close_overlay();
                return Err(error.into());
            }
            None => {
                shell.close_overlay();
                return Ok(None);
            }
        };
        match event {
            Event::Resize(columns, rows) => {
                shell.set_size(columns, rows);
                render_picker(shell, &list, &filter);
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    shell.close_overlay();
                    shell.render();
                    return Ok(None);
                }
                if key.code == KeyCode::Enter && key.modifiers.is_empty() {
                    let selected = list.selected_item().and_then(|selected| {
                        items.iter().position(|item| item.value == selected.value)
                    });
                    shell.close_overlay();
                    shell.render();
                    return Ok(selected);
                }
                match key.code {
                    KeyCode::Char(character)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                    {
                        filter.push(character);
                        list.set_filter(&filter);
                    }
                    KeyCode::Backspace if key.modifiers.is_empty() => {
                        filter.pop();
                        list.set_filter(&filter);
                    }
                    _ => list.handle_input(&encode(&key)),
                }
                render_picker(shell, &list, &filter);
            }
            _ => {}
        }
    }
}

/// Convert persistent session metadata to select-list items.
pub fn session_items(store: &SessionStore) -> Vec<SelectItem> {
    store
        .list()
        .into_iter()
        .map(|session| SelectItem {
            value: session.id,
            label: session.title,
            description: Some(format!("{}", session.path.display())),
        })
        .collect()
}

/// Ask the user to select a stored session, returning its durable path.
pub async fn session_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    store: &SessionStore,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let items = session_items(store);
    if items.is_empty() {
        shell.error(format!("no sessions in {}", store.dir().display()));
        shell.render();
        return Ok(None);
    }
    let selected = pick_from(shell, input, items.clone()).await?;
    match selected {
        Some(index) => Ok(Some(store.by_id(&items[index].value)?.path)),
        None => Ok(None),
    }
}

/// Ask the user to select an installed theme name.
pub async fn theme_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    names: &[String],
) -> anyhow::Result<String> {
    let items = names
        .iter()
        .map(|name| SelectItem {
            value: name.clone(),
            label: name.clone(),
            description: None,
        })
        .collect::<Vec<_>>();
    let index = pick_from(shell, input, items)
        .await?
        .ok_or_else(|| anyhow::anyhow!("theme selection cancelled"))?;
    Ok(names[index].clone())
}

/// Ask the user to select a capability-supported thinking level.
pub async fn thinking_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    levels: &[ThinkingLevel],
) -> anyhow::Result<ThinkingLevel> {
    let items = levels
        .iter()
        .map(|level| SelectItem {
            value: level.label().into(),
            label: level.label().into(),
            description: None,
        })
        .collect::<Vec<_>>();
    let index = pick_from(shell, input, items)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thinking selection cancelled"))?;
    Ok(levels[index])
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
    let items = models
        .iter()
        .map(|model| SelectItem {
            value: model.id.0.clone(),
            label: model.id.0.clone(),
            description: Some(model.api_name.clone()),
        })
        .collect::<Vec<_>>();
    let Some(index) = pick_from(shell, input, items).await? else {
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

    #[test]
    fn session_items_map_ids_and_titles() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.dir().join("one.jsonl"), b"").unwrap();
        let items = session_items(&store);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "one");
        assert_eq!(items[0].label, "(empty session)");
    }
}
