#![allow(missing_docs)]

use std::path::{Path, PathBuf};

use crate::config::Config;

/// Stable base instruction applied before all user/project instructions.
pub const BASE_PERSONA: &str = "You are ygg, a careful coding agent. Work directly in the workspace, explain important changes concisely, and use tools when they improve accuracy.";

fn global_agents_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("AGENTS.md")
}

fn read_if_exists(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Produce the inclusive root-to-leaf workspace path. It never walks above the
/// workspace, even if an invocation path is malformed or outside it.
pub fn dirs_from_workspace_to_cwd(workspace: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut directories = vec![workspace.to_owned()];
    let Ok(relative) = cwd.strip_prefix(workspace) else {
        return directories;
    };
    let mut current = workspace.to_owned();
    for component in relative.components() {
        if let std::path::Component::Normal(component) = component {
            current.push(component);
            directories.push(current.clone());
        }
    }
    directories
}

fn compose_instructions_at(config: &Config, global: &Path) -> anyhow::Result<String> {
    let mut parts = vec![BASE_PERSONA.to_owned()];
    if let Some(contents) = read_if_exists(global)? {
        parts.push(contents);
    }
    for directory in dirs_from_workspace_to_cwd(&config.workspace, &config.invocation_cwd) {
        if let Some(contents) = read_if_exists(&directory.join("AGENTS.md"))? {
            parts.push(contents);
        }
    }
    Ok(parts.join("\n\n"))
}

/// Compose global then workspace-root-to-leaf AGENTS.md instructions.
pub fn compose_instructions(config: &Config) -> anyhow::Result<String> {
    compose_instructions_at(config, &global_agents_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompactionPolicy, Mode, ResumeSelector, SandboxPolicy};

    fn config(workspace: PathBuf, cwd: PathBuf) -> Config {
        Config {
            workspace,
            invocation_cwd: cwd,
            model: None,
            reasoning: ygg_ai::ReasoningConfig::Off,
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
    fn composition_is_global_root_to_leaf_and_never_ascends() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let global = outside.path().join("AGENTS.md");
        std::fs::write(&global, "global instructions").unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root instructions").unwrap();
        std::fs::write(root.path().join("a/AGENTS.md"), "a instructions").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "leaf instructions").unwrap();
        std::fs::write(outside.path().join("parent-AGENTS.md"), "excluded").unwrap();

        let output =
            compose_instructions_at(&config(root.path().to_owned(), nested.clone()), &global)
                .unwrap();
        let positions = [
            output.find("global instructions").unwrap(),
            output.find("root instructions").unwrap(),
            output.find("a instructions").unwrap(),
            output.find("leaf instructions").unwrap(),
        ];
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!output.contains("excluded"));
    }

    #[test]
    fn dirs_are_workspace_first_and_cwd_last() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("one/two");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            dirs_from_workspace_to_cwd(root.path(), &nested),
            vec![
                root.path().to_owned(),
                root.path().join("one"),
                root.path().join("one/two"),
            ]
        );
    }
}
