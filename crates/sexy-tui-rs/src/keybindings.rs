//! Pi-compatible keybinding registry.
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use crate::keys::{matches_key, KeyId};

/// One resolved `(action, key)` pair.
pub type Keybinding = (String, KeyId);
/// User-provided action overrides. An empty vector explicitly unbinds an action.
pub type KeybindingsConfig = HashMap<String, Vec<KeyId>>;

/// Definition of an action and its defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub description: String,
    pub keys: Vec<KeyId>,
}

pub type KeybindingDefinitions = HashMap<String, KeybindingDefinition>;
pub type Keybindings = HashMap<String, Vec<KeyId>>;

/// A key explicitly claimed by more than one user override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: KeyId,
    pub keybindings: Vec<String>,
}

fn definition(description: &str, keys: &[KeyId]) -> KeybindingDefinition {
    KeybindingDefinition {
        description: description.into(),
        keys: keys.to_vec(),
    }
}

/// Defaults from pinned Pi TUI `src/keybindings.ts`.
pub static TUI_KEYBINDINGS: LazyLock<KeybindingDefinitions> = LazyLock::new(|| {
    HashMap::from([
        (
            "tui.editor.cursorUp".into(),
            definition("Move cursor up", &["up"]),
        ),
        (
            "tui.editor.cursorDown".into(),
            definition("Move cursor down", &["down"]),
        ),
        (
            "tui.editor.cursorLeft".into(),
            definition("Move cursor left", &["left", "ctrl+b"]),
        ),
        (
            "tui.editor.cursorRight".into(),
            definition("Move cursor right", &["right", "ctrl+f"]),
        ),
        (
            "tui.editor.cursorWordLeft".into(),
            definition("Move cursor word left", &["alt+left", "ctrl+left", "alt+b"]),
        ),
        (
            "tui.editor.cursorWordRight".into(),
            definition(
                "Move cursor word right",
                &["alt+right", "ctrl+right", "alt+f"],
            ),
        ),
        (
            "tui.editor.cursorLineStart".into(),
            definition("Move to line start", &["home", "ctrl+a"]),
        ),
        (
            "tui.editor.cursorLineEnd".into(),
            definition("Move to line end", &["end", "ctrl+e"]),
        ),
        (
            "tui.editor.jumpForward".into(),
            definition("Jump forward to character", &["ctrl+]"]),
        ),
        (
            "tui.editor.jumpBackward".into(),
            definition("Jump backward to character", &["ctrl+alt+]"]),
        ),
        (
            "tui.editor.pageUp".into(),
            definition("Page up", &["pageUp"]),
        ),
        (
            "tui.editor.pageDown".into(),
            definition("Page down", &["pageDown"]),
        ),
        (
            "tui.editor.deleteCharBackward".into(),
            definition("Delete character backward", &["backspace"]),
        ),
        (
            "tui.editor.deleteCharForward".into(),
            definition("Delete character forward", &["delete", "ctrl+d"]),
        ),
        (
            "tui.editor.deleteWordBackward".into(),
            definition("Delete word backward", &["ctrl+w", "alt+backspace"]),
        ),
        (
            "tui.editor.deleteWordForward".into(),
            definition("Delete word forward", &["alt+d", "alt+delete"]),
        ),
        (
            "tui.editor.deleteToLineStart".into(),
            definition("Delete to line start", &["ctrl+u"]),
        ),
        (
            "tui.editor.deleteToLineEnd".into(),
            definition("Delete to line end", &["ctrl+k"]),
        ),
        ("tui.editor.yank".into(), definition("Yank", &["ctrl+y"])),
        (
            "tui.editor.yankPop".into(),
            definition("Yank pop", &["alt+y"]),
        ),
        ("tui.editor.undo".into(), definition("Undo", &["ctrl+-"])),
        (
            "tui.input.newLine".into(),
            definition("Insert newline", &["shift+enter", "ctrl+j"]),
        ),
        (
            "tui.input.submit".into(),
            definition("Submit input", &["enter"]),
        ),
        (
            "tui.input.tab".into(),
            definition("Tab / autocomplete", &["tab"]),
        ),
        (
            "tui.input.copy".into(),
            definition("Copy selection", &["ctrl+c"]),
        ),
        (
            "tui.select.up".into(),
            definition("Move selection up", &["up"]),
        ),
        (
            "tui.select.down".into(),
            definition("Move selection down", &["down"]),
        ),
        (
            "tui.select.pageUp".into(),
            definition("Selection page up", &["pageUp"]),
        ),
        (
            "tui.select.pageDown".into(),
            definition("Selection page down", &["pageDown"]),
        ),
        (
            "tui.select.confirm".into(),
            definition("Confirm selection", &["enter"]),
        ),
        (
            "tui.select.cancel".into(),
            definition("Cancel selection", &["escape", "ctrl+c"]),
        ),
    ])
});

#[derive(Clone)]
pub struct KeybindingsManager {
    definitions: KeybindingDefinitions,
    user_bindings: KeybindingsConfig,
    bindings: Keybindings,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    pub fn new(definitions: KeybindingDefinitions) -> Self {
        Self::with_user_bindings(definitions, HashMap::new())
    }

    pub fn with_user_bindings(
        definitions: KeybindingDefinitions,
        user_bindings: KeybindingsConfig,
    ) -> Self {
        let mut manager = Self {
            definitions,
            user_bindings,
            bindings: HashMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    fn rebuild(&mut self) {
        self.bindings.clear();
        self.conflicts.clear();

        let mut claims: HashMap<KeyId, Vec<String>> = HashMap::new();
        for (action, keys) in &self.user_bindings {
            if !self.definitions.contains_key(action) {
                continue;
            }
            let mut seen = HashSet::new();
            for &key in keys {
                if seen.insert(key) {
                    claims.entry(key).or_default().push(action.clone());
                }
            }
        }
        for (key, keybindings) in claims {
            if keybindings.len() > 1 {
                self.conflicts.push(KeybindingConflict { key, keybindings });
            }
        }
        self.conflicts.sort_by_key(|conflict| conflict.key);

        for (action, definition) in &self.definitions {
            let source = self.user_bindings.get(action).unwrap_or(&definition.keys);
            let mut seen = HashSet::new();
            let keys = source
                .iter()
                .copied()
                .filter(|key| seen.insert(*key))
                .collect();
            self.bindings.insert(action.clone(), keys);
        }
    }

    pub fn matches(&self, data: &str, action: &str) -> bool {
        self.bindings
            .get(action)
            .is_some_and(|keys| keys.iter().any(|key| matches_key(data, key)))
    }

    pub fn get_keys(&self, action: &str) -> Vec<KeyId> {
        self.bindings.get(action).cloned().unwrap_or_default()
    }

    pub fn get_definition(&self, action: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(action)
    }

    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    pub fn set_user_bindings(&mut self, bindings: KeybindingsConfig) {
        self.user_bindings = bindings;
        self.rebuild();
    }

    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        self.bindings.clone()
    }

    /// Compatibility accessor retained for existing Rust callers.
    pub fn get_bindings(&self) -> &Keybindings {
        &self.bindings
    }

    pub fn find_action(&self, data: &str) -> Option<&str> {
        self.bindings.iter().find_map(|(action, keys)| {
            keys.iter()
                .any(|key| matches_key(data, key))
                .then_some(action.as_str())
        })
    }

    pub fn set_binding(&mut self, action: &str, keys: Vec<KeyId>) {
        self.user_bindings.insert(action.into(), keys);
        self.rebuild();
    }

    pub fn check_conflicts(&self) -> Vec<KeybindingConflict> {
        self.get_conflicts()
    }
}

static KEYBINDINGS_INSTANCE: Mutex<Option<KeybindingsManager>> = Mutex::new(None);

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn set_keybindings(manager: KeybindingsManager) {
    *lock_or_recover(&KEYBINDINGS_INSTANCE) = Some(manager);
}

pub fn get_keybindings() -> KeybindingsManager {
    let mut instance = lock_or_recover(&KEYBINDINGS_INSTANCE);
    instance
        .get_or_insert_with(|| KeybindingsManager::new(TUI_KEYBINDINGS.clone()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_binds_ctrl_j_as_default_newline_alias() {
        let manager = KeybindingsManager::new(TUI_KEYBINDINGS.clone());
        assert_eq!(
            manager.get_keys("tui.input.newLine"),
            ["shift+enter", "ctrl+j"]
        );
        assert!(manager.matches("\n", "tui.input.newLine"));
        assert!(manager.matches("\x1b[106;5u", "tui.input.newLine"));
    }

    #[test]
    fn pi_rebinding_submit_does_not_evict_selector_confirm() {
        let manager = KeybindingsManager::with_user_bindings(
            TUI_KEYBINDINGS.clone(),
            HashMap::from([("tui.input.submit".into(), vec!["enter", "ctrl+enter"])]),
        );
        assert_eq!(
            manager.get_keys("tui.input.submit"),
            ["enter", "ctrl+enter"]
        );
        assert_eq!(manager.get_keys("tui.select.confirm"), ["enter"]);
    }

    #[test]
    fn pi_reusing_key_does_not_evict_cursor_defaults() {
        let manager = KeybindingsManager::with_user_bindings(
            TUI_KEYBINDINGS.clone(),
            HashMap::from([("tui.select.up".into(), vec!["up", "ctrl+p"])]),
        );
        assert_eq!(manager.get_keys("tui.select.up"), ["up", "ctrl+p"]);
        assert_eq!(manager.get_keys("tui.editor.cursorUp"), ["up"]);
    }

    #[test]
    fn pi_reports_only_direct_user_conflicts() {
        let manager = KeybindingsManager::with_user_bindings(
            TUI_KEYBINDINGS.clone(),
            HashMap::from([
                ("tui.input.submit".into(), vec!["ctrl+x"]),
                ("tui.select.confirm".into(), vec!["ctrl+x"]),
            ]),
        );
        let conflicts = manager.get_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].key, "ctrl+x");
        let actions = &conflicts[0].keybindings;
        assert!(actions.contains(&"tui.input.submit".into()));
        assert!(actions.contains(&"tui.select.confirm".into()));
        assert_eq!(
            manager.get_keys("tui.editor.cursorLeft"),
            ["left", "ctrl+b"]
        );
    }
}
