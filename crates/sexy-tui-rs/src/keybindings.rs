/// Keybinding management. Port of src/keybindings.ts (244 lines).
use crate::keys::{matches_key, KeyId};

/// Type alias for a keybinding (maps action name to key).
pub type Keybinding = (String, KeyId);

/// Map of named actions to their bound key sequences.
pub type KeybindingsConfig = std::collections::HashMap<String, Vec<KeyId>>;

/// A single keybinding definition.
#[derive(Debug, Clone)]
pub struct KeybindingDefinition {
    pub description: String,
    pub keys: Vec<KeyId>,
}

/// Collection of keybinding definitions.
pub type KeybindingDefinitions = std::collections::HashMap<String, KeybindingDefinition>;

/// Resolved keybinding (ready to match against input).
pub type Keybindings = std::collections::HashMap<String, Vec<KeyId>>;

/// Info about a keybinding conflict.
#[derive(Debug)]
pub struct KeybindingConflict {
    pub action: String,
    pub key: KeyId,
    pub conflicts_with: String,
}

/// Default TUI keybindings.
pub static TUI_KEYBINDINGS: std::sync::LazyLock<KeybindingDefinitions> =
    std::sync::LazyLock::new(|| {
        let mut defs = std::collections::HashMap::new();
        defs.insert(
            "submit".into(),
            KeybindingDefinition {
                description: "Submit editor content".into(),
                keys: vec!["enter"],
            },
        );
        defs.insert(
            "newline".into(),
            KeybindingDefinition {
                description: "Insert newline".into(),
                keys: vec!["shift+enter", "alt+enter"],
            },
        );
        defs.insert(
            "cancel".into(),
            KeybindingDefinition {
                description: "Cancel current action".into(),
                keys: vec!["escape"],
            },
        );
        defs.insert(
            "quit".into(),
            KeybindingDefinition {
                description: "Quit application".into(),
                keys: vec!["ctrl+c"],
            },
        );
        defs
    });

/// Manages keybinding resolution and conflict detection.
#[derive(Clone)]
pub struct KeybindingsManager {
    bindings: Keybindings,
    #[allow(dead_code)]
    definitions: KeybindingDefinitions,
}

impl KeybindingsManager {
    pub fn new(definitions: KeybindingDefinitions) -> Self {
        let mut bindings = Keybindings::new();
        for (action, def) in &definitions {
            bindings.insert(action.clone(), def.keys.clone());
        }
        KeybindingsManager {
            bindings,
            definitions,
        }
    }

    /// Check if input data matches a specific action.
    pub fn matches(&self, data: &str, action: &str) -> bool {
        if let Some(keys) = self.bindings.get(action) {
            keys.iter().any(|k| matches_key(data, k))
        } else {
            false
        }
    }

    /// Find which action (if any) matches the input data.
    pub fn find_action(&self, data: &str) -> Option<&str> {
        for (action, keys) in &self.bindings {
            if keys.iter().any(|k| matches_key(data, k)) {
                return Some(action);
            }
        }
        None
    }

    /// Update the keybinding for an action.
    pub fn set_binding(&mut self, action: &str, keys: Vec<KeyId>) {
        self.bindings.insert(action.to_string(), keys);
    }

    /// Get all current bindings.
    pub fn get_bindings(&self) -> &Keybindings {
        &self.bindings
    }

    /// Check for conflicts between bindings.
    pub fn check_conflicts(&self) -> Vec<KeybindingConflict> {
        let mut conflicts = Vec::new();
        let actions: Vec<&String> = self.bindings.keys().collect();
        for i in 0..actions.len() {
            for j in (i + 1)..actions.len() {
                let a1 = actions[i];
                let a2 = actions[j];
                if let (Some(k1), Some(k2)) = (self.bindings.get(a1), self.bindings.get(a2)) {
                    for key in k1 {
                        if k2.contains(key) {
                            conflicts.push(KeybindingConflict {
                                action: a1.clone(),
                                key,
                                conflicts_with: a2.clone(),
                            });
                        }
                    }
                }
            }
        }
        conflicts
    }
}

use std::sync::Mutex;

static KEYBINDINGS_INSTANCE: Mutex<Option<KeybindingsManager>> = Mutex::new(None);

/// Recover from a poisoned mutex.
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set the global keybindings manager.
pub fn set_keybindings(manager: KeybindingsManager) {
    *lock_or_recover(&KEYBINDINGS_INSTANCE) = Some(manager);
}

/// Get the global keybindings manager.
pub fn get_keybindings() -> Option<KeybindingsManager> {
    lock_or_recover(&KEYBINDINGS_INSTANCE).clone()
}
