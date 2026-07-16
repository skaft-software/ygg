#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sexy_tui_rs::theme::{capability::CapabilityTier, Theme};
use sexy_tui_rs::widgets::SelectListTheme;

use crate::config::Config;

fn foreground(theme: &Theme, token: &'static str) -> Box<dyn Fn(&str) -> String> {
    let theme = theme.clone();
    Box::new(move |text| theme.fg(token, text))
}

fn bold_foreground(theme: &Theme, token: &'static str) -> Box<dyn Fn(&str) -> String> {
    let theme = theme.clone();
    Box::new(move |text| theme.bold(&theme.fg(token, text)))
}

fn global_theme_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("themes")
}

fn project_theme_dir(config: &Config) -> PathBuf {
    config.workspace.join(".ygg").join("themes")
}

fn theme_file_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || Path::new(name).components().count() != 1
        || name.contains(std::path::MAIN_SEPARATOR)
    {
        return None;
    }
    Some(if name.ends_with(".toml") {
        name.to_owned()
    } else {
        format!("{name}.toml")
    })
}

/// Resolve a theme by name, preferring the workspace theme directory.
pub fn theme_path(name: &str, config: &Config) -> Option<PathBuf> {
    let file_name = theme_file_name(name)?;
    let project = project_theme_dir(config).join(&file_name);
    if project.is_file() {
        return Some(project);
    }
    let global = global_theme_dir().join(file_name);
    global.is_file().then_some(global)
}

fn valid_theme_file(path: &Path) -> anyhow::Result<()> {
    let source = std::fs::read_to_string(path)?;
    let _: toml::Value = toml::from_str(&source)
        .map_err(|error| anyhow::anyhow!("invalid theme {}: {error}", path.display()))?;
    Ok(())
}

/// Load a named theme or return an error without altering the current theme.
pub fn load_named_theme(name: &str, config: &Config) -> anyhow::Result<Theme> {
    let path =
        theme_path(name, config).ok_or_else(|| anyhow::anyhow!("theme {name:?} was not found"))?;
    valid_theme_file(&path)?;
    let path = path.to_string_lossy();
    Ok(Theme::load(Some(&path), CapabilityTier::Baseline))
}

/// Load the startup theme. Missing or malformed files intentionally fall back
/// to the framework's default token set instead of affecting launch/print mode.
pub fn load_theme(config: &Config) -> Theme {
    match config
        .theme
        .as_deref()
        .map(|name| load_named_theme(name, config))
    {
        Some(Ok(theme)) => theme,
        _ => Theme::load(None, CapabilityTier::Baseline),
    }
}

fn available_themes_from_dirs(global: &Path, project: &Path) -> Vec<String> {
    let mut names = BTreeSet::new();
    for directory in [global, project] {
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
                    if let Some(name) = path.file_stem().and_then(|name| name.to_str()) {
                        names.insert(name.to_owned());
                    }
                }
            }
        }
    }
    names.into_iter().collect()
}

/// List global and project theme names, deduplicated with project precedence.
pub fn available_themes(config: &Config) -> Vec<String> {
    available_themes_from_dirs(&global_theme_dir(), &project_theme_dir(config))
}

/// Build sexy-tui's select-list closures from a resolved theme.
pub fn select_list_theme(theme: &Theme) -> SelectListTheme {
    SelectListTheme {
        selected_prefix: foreground(theme, "accent"),
        selected_text: bold_foreground(theme, "accent"),
        description: foreground(theme, "muted"),
        scroll_info: foreground(theme, "muted"),
        no_match: foreground(theme, "error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompactionPolicy, Mode, ResumeSelector, SandboxPolicy};

    fn config(workspace: PathBuf) -> Config {
        Config {
            workspace: workspace.clone(),
            invocation_cwd: workspace,
            model: None,
            reasoning: ygg_ai::ReasoningConfig::Off,
            cache_retention: ygg_ai::CacheRetention::Short,
            sandbox: SandboxPolicy::default(),
            theme: None,
            session_dir: PathBuf::from("sessions"),
            compaction: CompactionPolicy::default(),
            max_turns: 40,
            show_reasoning_in_print: false,
            initial_prompt: None,
            mode: Mode::Interactive,
            resume: ResumeSelector::New,
        }
    }

    #[test]
    fn select_list_theme_builds_and_preserves_text() {
        let theme = select_list_theme(&Theme::load(None, CapabilityTier::Baseline));
        assert!((theme.selected_text)("x").contains('x'));
        assert!((theme.no_match)("x").contains('x'));
    }

    #[test]
    fn project_theme_wins_and_available_themes_deduplicate() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path().to_owned());
        let project = project_theme_dir(&config);
        let global = directory.path().join("global-themes");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(project.join("project.toml"), "accent = 'blue'").unwrap();
        std::fs::write(project.join("shared.toml"), "accent = 'green'").unwrap();
        std::fs::write(global.join("global.toml"), "accent = 'red'").unwrap();
        std::fs::write(global.join("shared.toml"), "accent = 'red'").unwrap();
        assert_eq!(
            theme_path("shared", &config),
            Some(project.join("shared.toml"))
        );
        assert_eq!(
            available_themes_from_dirs(&global, &project),
            vec![
                "global".to_owned(),
                "project".to_owned(),
                "shared".to_owned()
            ]
        );
    }
}
