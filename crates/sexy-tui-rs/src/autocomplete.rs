/// Autocomplete support for slash commands and file paths.
/// Port of src/autocomplete.ts (786 lines).
use crate::fuzzy::{fuzzy_filter, fuzzy_match};

/// An autocomplete suggestion item.
#[derive(Debug, Clone)]
pub struct AutocompleteItem {
    pub value: String,
    pub display: String,
    pub description: Option<String>,
}

/// A slash command definition.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

/// Result of an autocomplete query.
#[derive(Debug)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    pub query: String,
    pub offset: usize, // character offset from cursor for the text being completed
}

/// Trait for autocomplete providers.
pub trait AutocompleteProvider {
    fn get_suggestions(&self, text: &str, cursor_pos: usize) -> Option<AutocompleteSuggestions>;
}

/// Combined autocomplete provider supporting slash commands and file paths.
pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommand>,
    base_path: String,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<SlashCommand>, base_path: String) -> Self {
        CombinedAutocompleteProvider {
            commands,
            base_path,
        }
    }

    fn complete_slash_commands(&self, query: &str) -> Vec<AutocompleteItem> {
        let cmd_refs: Vec<&SlashCommand> = self.commands.iter().collect();
        let filtered: Vec<&SlashCommand> =
            fuzzy_filter(&cmd_refs, query, |cmd: &&SlashCommand| cmd.name.clone());
        filtered
            .into_iter()
            .map(|cmd| AutocompleteItem {
                value: format!("/{}", cmd.name),
                display: format!("/{}", cmd.name),
                description: Some(cmd.description.clone()),
            })
            .collect()
    }

    fn complete_file_paths(&self, query: &str) -> Vec<AutocompleteItem> {
        // Simple file path completion — read directory entries
        let search_path = if query.is_empty() {
            self.base_path.clone()
        } else if query.starts_with('/') {
            query.to_string()
        } else if let Some(relative) = query.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                format!("{home}/{relative}")
            } else {
                query.to_string()
            }
        } else {
            format!("{}/{}", self.base_path, query)
        };

        let dir = std::path::Path::new(&search_path);
        let parent = if query.ends_with('/') || query.is_empty() {
            dir.to_path_buf()
        } else {
            dir.parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf()
        };

        let file_filter = if query.ends_with('/') || query.is_empty() {
            String::new()
        } else {
            dir.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        };

        let mut items: Vec<AutocompleteItem> = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&parent) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !file_filter.is_empty() && !fuzzy_match(&file_filter, &name).matches {
                    continue;
                }
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let display = if is_dir {
                    format!("{}/", name)
                } else {
                    name.clone()
                };
                items.push(AutocompleteItem {
                    value: display.clone(),
                    display,
                    description: if is_dir {
                        Some("directory".into())
                    } else {
                        None
                    },
                });
            }
        }

        items
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn get_suggestions(&self, text: &str, cursor_pos: usize) -> Option<AutocompleteSuggestions> {
        let text_before = &text[..cursor_pos];

        // Check for slash command
        if let Some(slash_pos) = text_before.rfind('/') {
            // Check if we're in a slash command (not a file path)
            let before_slash = &text_before[..slash_pos];
            let is_slash_cmd = before_slash.is_empty()
                || before_slash.ends_with(' ')
                || before_slash.ends_with('\n');

            if is_slash_cmd {
                let query = &text_before[slash_pos + 1..];
                let items = self.complete_slash_commands(query);
                if !items.is_empty() {
                    return Some(AutocompleteSuggestions {
                        items,
                        query: query.to_string(),
                        offset: query.len(),
                    });
                }
            }
        }

        // File path completion (triggered by Tab)
        // Find the path being typed before the cursor
        if let Some(last_space) = text_before.rfind(' ') {
            let path_query = &text_before[last_space + 1..];
            let items = self.complete_file_paths(path_query);
            if !items.is_empty() {
                return Some(AutocompleteSuggestions {
                    items,
                    query: path_query.to_string(),
                    offset: path_query.len(),
                });
            }
        }

        None
    }
}
