#![allow(missing_docs)]

#[cfg(test)]
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use ygg_agent::session::SkillActivatedSnapshot;

use crate::resource_resolver::{ResourceKind, ResourceResolver, ResourceSnapshot, ResourceTrust};

const MAX_TEMPLATE_BYTES: usize = 256 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 32 * 1024;
const MAX_INCLUDED_FILE_BYTES: usize = 256 * 1024;
const MAX_EXPANDED_PROMPT_BYTES: usize = 512 * 1024;
const MAX_ARGUMENTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptTrust {
    UserInstalled,
    Workspace,
    ExplicitExternal,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSource {
    pub path: PathBuf,
    pub trust: PromptTrust,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplateDescriptor {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub path: PathBuf,
    pub trust: PromptTrust,
    pub content_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug)]
struct PromptTemplate {
    descriptor: PromptTemplateDescriptor,
    body: String,
}

#[derive(Clone, Debug, Default)]
pub struct PromptRegistry {
    templates: Arc<[PromptTemplate]>,
    descriptors: Arc<[PromptTemplateDescriptor]>,
    diagnostics: Arc<[PromptDiagnostic]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub name: String,
    pub content_hash: String,
    pub text: String,
}

pub fn debug_expansion(rendered: &RenderedPrompt) -> String {
    format!(
        "Expanded prompt template {:?} (sha256 {})\n--- expanded prompt ---\n{}\n--- end expanded prompt ---",
        rendered.name, rendered.content_hash, rendered.text
    )
}

pub struct PromptRenderContext<'a> {
    pub workspace: &'a Path,
    pub selection: Option<&'a str>,
    pub active_skills: &'a [SkillActivatedSnapshot],
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("prompt template not found: {0}")]
    NotFound(String),
    #[error("invalid prompt template {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("invalid prompt arguments: {0}")]
    InvalidArguments(String),
    #[error("prompt expansion failed: {0}")]
    Expansion(String),
    #[error("expanded prompt exceeds the {MAX_EXPANDED_PROMPT_BYTES}-byte limit")]
    ExpandedTooLarge,
}

#[derive(Default, serde::Deserialize)]
struct MarkdownHeader {
    description: Option<String>,
    #[serde(rename = "argument-hint", alias = "argument_hint")]
    argument_hint: Option<String>,
}

#[derive(serde::Deserialize)]
struct TomlTemplate {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "argument-hint", alias = "argument_hint")]
    argument_hint: Option<String>,
    #[serde(alias = "template", alias = "content")]
    prompt: String,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn read_bounded_utf8(path: &Path, limit: usize) -> Result<String, PromptError> {
    let bytes = ygg_agent::secure_fs::read_regular_file_bounded(path, limit).map_err(|error| {
        PromptError::Invalid {
            path: path.to_owned(),
            message: error.to_string(),
        }
    })?;
    String::from_utf8(bytes).map_err(|_| PromptError::Invalid {
        path: path.to_owned(),
        message: "file is not valid UTF-8".to_owned(),
    })
}

fn content_hash(content: &[u8]) -> String {
    Sha256::digest(content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fallback_description(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Reusable prompt template")
        .trim_start_matches('#')
        .trim();
    let mut description = line.chars().take(120).collect::<String>();
    if line.chars().count() > 120 {
        description.push('…');
    }
    description
}

fn parse_markdown(path: &Path, raw: &str) -> Result<(MarkdownHeader, String), PromptError> {
    if !raw.starts_with("---\n") && !raw.starts_with("---\r\n") {
        return Ok((MarkdownHeader::default(), raw.to_owned()));
    }
    let first_line_end = raw.find('\n').expect("frontmatter prefix contains newline") + 1;
    let mut offset = first_line_end;
    for line in raw[first_line_end..].split_inclusive('\n') {
        if offset > MAX_FRONTMATTER_BYTES {
            return Err(PromptError::Invalid {
                path: path.to_owned(),
                message: format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"),
            });
        }
        offset += line.len();
        if line.trim() == "---" {
            let header = serde_yaml::from_str(&raw[first_line_end..offset - line.len()]).map_err(
                |error| PromptError::Invalid {
                    path: path.to_owned(),
                    message: format!("invalid YAML frontmatter: {error}"),
                },
            )?;
            return Ok((header, raw[offset..].to_owned()));
        }
    }
    Err(PromptError::Invalid {
        path: path.to_owned(),
        message: "frontmatter is missing its closing --- delimiter".to_owned(),
    })
}

fn load_template(path: &Path, trust: PromptTrust) -> Result<PromptTemplate, PromptError> {
    let raw = read_bounded_utf8(path, MAX_TEMPLATE_BYTES)?;
    let hash = content_hash(raw.as_bytes());
    let extension = path.extension().and_then(|value| value.to_str());
    let file_name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| PromptError::Invalid {
            path: path.to_owned(),
            message: "file name is not valid UTF-8".to_owned(),
        })?;
    let (name, description, argument_hint, body) = match extension {
        Some("md") => {
            let (header, body) = parse_markdown(path, &raw)?;
            let description = header
                .description
                .unwrap_or_else(|| fallback_description(&body));
            (
                file_name.to_owned(),
                description,
                header.argument_hint,
                body,
            )
        }
        Some("toml") => {
            let parsed: TomlTemplate =
                toml::from_str(&raw).map_err(|error| PromptError::Invalid {
                    path: path.to_owned(),
                    message: format!("invalid TOML template: {error}"),
                })?;
            let name = parsed.name.unwrap_or_else(|| file_name.to_owned());
            let description = parsed
                .description
                .unwrap_or_else(|| fallback_description(&parsed.prompt));
            (name, description, parsed.argument_hint, parsed.prompt)
        }
        _ => {
            return Err(PromptError::Invalid {
                path: path.to_owned(),
                message: "prompt templates must use .md or .toml".to_owned(),
            });
        }
    };
    if !valid_name(&name) {
        return Err(PromptError::Invalid {
            path: path.to_owned(),
            message: format!(
                "invalid template name {name:?}; use ASCII letters, digits, '-' or '_'"
            ),
        });
    }
    if body.trim().is_empty() {
        return Err(PromptError::Invalid {
            path: path.to_owned(),
            message: "template body is empty".to_owned(),
        });
    }
    Ok(PromptTemplate {
        descriptor: PromptTemplateDescriptor {
            name,
            description,
            argument_hint,
            path: path.to_owned(),
            trust,
            content_hash: hash,
        },
        body,
    })
}

#[cfg(test)]
fn source_files(source: &PromptSource) -> Result<Vec<PathBuf>, PromptError> {
    let metadata = match std::fs::symlink_metadata(&source.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(PromptError::Invalid {
                path: source.path.clone(),
                message: error.to_string(),
            })
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(PromptError::Invalid {
            path: source.path.clone(),
            message: "prompt source must not be a symlink".to_owned(),
        });
    }
    if metadata.is_file() {
        return source
            .path
            .canonicalize()
            .map(|path| vec![path])
            .map_err(|error| PromptError::Invalid {
                path: source.path.clone(),
                message: error.to_string(),
            });
    }
    if !metadata.is_dir() {
        return Err(PromptError::Invalid {
            path: source.path.clone(),
            message: "prompt source is neither a regular file nor a directory".to_owned(),
        });
    }
    let root = source
        .path
        .canonicalize()
        .map_err(|error| PromptError::Invalid {
            path: source.path.clone(),
            message: error.to_string(),
        })?;
    let mut files = std::fs::read_dir(&root)
        .map_err(|error| PromptError::Invalid {
            path: root.clone(),
            message: error.to_string(),
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                && matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("md" | "toml")
                )
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

impl PromptRegistry {
    #[cfg(test)]
    /// Build a registry from low-to-high precedence sources. Later sources
    /// replace earlier templates with the same name.
    pub fn from_sources(sources: &[PromptSource]) -> Self {
        let mut templates = BTreeMap::<String, PromptTemplate>::new();
        let mut diagnostics = Vec::new();
        for source in sources {
            let files = match source_files(source) {
                Ok(files) => files,
                Err(error) => {
                    diagnostics.push(PromptDiagnostic {
                        path: source.path.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            for path in files {
                match load_template(&path, source.trust) {
                    Ok(template) => {
                        templates.insert(template.descriptor.name.clone(), template);
                    }
                    Err(error) => diagnostics.push(PromptDiagnostic {
                        path,
                        message: error.to_string(),
                    }),
                }
            }
        }
        let templates = templates.into_values().collect::<Vec<_>>();
        let descriptors = templates
            .iter()
            .map(|template| template.descriptor.clone())
            .collect::<Vec<_>>();
        Self {
            templates: Arc::from(templates),
            descriptors: Arc::from(descriptors),
            diagnostics: Arc::from(diagnostics),
        }
    }

    /// Parse an already-resolved prompt snapshot. Discovery, trust, and
    /// precedence belong to the shared resolver; this type owns prompt formats
    /// and deterministic expansion only.
    pub fn from_snapshot(snapshot: &ResourceSnapshot) -> Self {
        debug_assert_eq!(snapshot.kind, ResourceKind::Prompt);
        let mut templates = Vec::new();
        let mut diagnostics = snapshot
            .diagnostics()
            .iter()
            .map(|diagnostic| PromptDiagnostic {
                path: diagnostic.path.clone(),
                message: diagnostic.message.clone(),
            })
            .collect::<Vec<_>>();
        for resource in snapshot.resources() {
            let trust = match resource.trust {
                ResourceTrust::User => PromptTrust::UserInstalled,
                ResourceTrust::TrustedWorkspace => PromptTrust::Workspace,
                ResourceTrust::Explicit => PromptTrust::ExplicitExternal,
            };
            // Resolver paths already have canonical, symlink-free parents.
            // Keep the final component unresolved so the bounded loader's
            // no-follow open remains authoritative if a file is swapped
            // between discovery and parsing.
            let path = resource.path.clone();
            match load_template(&path, trust) {
                Ok(template) if template.descriptor.name == resource.name => {
                    templates.push(template);
                }
                Ok(template) => diagnostics.push(PromptDiagnostic {
                    path: resource.path.clone(),
                    message: format!(
                        "declared name {:?} does not match resolved name {:?}",
                        template.descriptor.name, resource.name
                    ),
                }),
                Err(error) => diagnostics.push(PromptDiagnostic {
                    path: resource.path.clone(),
                    message: error.to_string(),
                }),
            }
        }
        templates.sort_by(|left, right| left.descriptor.name.cmp(&right.descriptor.name));
        let descriptors = templates
            .iter()
            .map(|template| template.descriptor.clone())
            .collect::<Vec<_>>();
        Self {
            templates: Arc::from(templates),
            descriptors: Arc::from(descriptors),
            diagnostics: Arc::from(diagnostics),
        }
    }

    /// Discover global, trusted-project, and explicit prompt templates through
    /// the shared product resource resolver.
    pub fn discover(workspace: &Path, explicit: &[PathBuf], workspace_trusted: bool) -> Self {
        let resolver = ResourceResolver::new(workspace.to_owned(), workspace_trusted);
        Self::from_snapshot(&resolver.discover(ResourceKind::Prompt, explicit))
    }

    pub fn descriptors(&self) -> Arc<[PromptTemplateDescriptor]> {
        self.descriptors.clone()
    }

    pub fn diagnostics(&self) -> Arc<[PromptDiagnostic]> {
        self.diagnostics.clone()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.templates
            .binary_search_by(|template| template.descriptor.name.as_str().cmp(name))
            .is_ok()
    }

    pub fn render(
        &self,
        name: &str,
        arguments: &str,
        context: &PromptRenderContext<'_>,
    ) -> Result<RenderedPrompt, PromptError> {
        let template = self
            .templates
            .binary_search_by(|template| template.descriptor.name.as_str().cmp(name))
            .ok()
            .and_then(|index| self.templates.get(index))
            .ok_or_else(|| PromptError::NotFound(name.to_owned()))?;
        let parsed_arguments = parse_arguments(arguments)?;
        let pi_expanded = expand_pi_arguments(&template.body, &parsed_arguments)?;
        let text = expand_ygg_variables(&pi_expanded, arguments.trim(), context)?;
        if text.len() > MAX_EXPANDED_PROMPT_BYTES {
            return Err(PromptError::ExpandedTooLarge);
        }
        Ok(RenderedPrompt {
            name: template.descriptor.name.clone(),
            content_hash: template.descriptor.content_hash.clone(),
            text,
        })
    }
}

/// Expand a template against the branch-local active skills, then persist its
/// exact name/hash provenance before the resulting user message is submitted.
pub fn render_and_record(
    registry: &PromptRegistry,
    session: &mut ygg_agent::Session,
    workspace: &Path,
    name: &str,
    arguments: &str,
    selection: Option<&str>,
) -> Result<RenderedPrompt, PromptError> {
    let active_skills = match session.head() {
        Some(head) => {
            session
                .resolve_active_skills(&head)
                .map_err(|error| PromptError::Expansion(error.to_string()))?
                .active_skills
        }
        None => Vec::new(),
    };
    let rendered = registry.render(
        name,
        arguments,
        &PromptRenderContext {
            workspace,
            selection,
            active_skills: &active_skills,
        },
    )?;
    session
        .append(ygg_agent::EntryValue::PromptTemplateSelected {
            name: rendered.name.clone(),
            content_hash: rendered.content_hash.clone(),
        })
        .map_err(|error| PromptError::Expansion(format!("cannot record template: {error}")))?;
    Ok(rendered)
}

/// Apply the process-level `--prompt <name>` selection, if present.
pub fn render_configured(
    app: &mut crate::app::App,
    arguments: &str,
) -> Result<Option<RenderedPrompt>, PromptError> {
    let Some(name) = app.config.prompt_template.clone() else {
        return Ok(None);
    };
    let registry = app.prompts.clone();
    let workspace = app.config.workspace.clone();
    render_and_record(
        &registry,
        app.agent.session_mut(),
        &workspace,
        &name,
        arguments,
        None,
    )
    .map(Some)
}

fn parse_arguments(input: &str) -> Result<Vec<String>, PromptError> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars();
    let mut quote = None;
    let mut escaped = false;
    let mut in_word = false;
    for character in chars.by_ref() {
        if escaped {
            current.push(character);
            escaped = false;
            in_word = true;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '\\') | (None, '\\') => escaped = true,
            (Some(_), value) => {
                current.push(value);
                in_word = true;
            }
            (None, '\'' | '"') => {
                quote = Some(character);
                in_word = true;
            }
            (None, value) if value.is_whitespace() => {
                if in_word {
                    arguments.push(std::mem::take(&mut current));
                    in_word = false;
                    if arguments.len() > MAX_ARGUMENTS {
                        return Err(PromptError::InvalidArguments(format!(
                            "more than {MAX_ARGUMENTS} arguments"
                        )));
                    }
                }
            }
            (None, value) => {
                current.push(value);
                in_word = true;
            }
        }
    }
    if escaped {
        return Err(PromptError::InvalidArguments(
            "trailing escape character".to_owned(),
        ));
    }
    if let Some(quote) = quote {
        return Err(PromptError::InvalidArguments(format!(
            "unterminated {quote} quote"
        )));
    }
    if in_word {
        arguments.push(current);
    }
    if arguments.len() > MAX_ARGUMENTS {
        return Err(PromptError::InvalidArguments(format!(
            "more than {MAX_ARGUMENTS} arguments"
        )));
    }
    Ok(arguments)
}

fn positional(arguments: &[String], index: usize) -> &str {
    index
        .checked_sub(1)
        .and_then(|index| arguments.get(index))
        .map(String::as_str)
        .unwrap_or_default()
}

fn braced_argument(expression: &str, arguments: &[String]) -> Option<String> {
    let (variable, default) = expression
        .split_once(":-")
        .map_or((expression, None), |(variable, default)| {
            (variable, Some(default))
        });
    let value = if matches!(variable, "@" | "ARGUMENTS") {
        arguments.join(" ")
    } else if let Some(slice) = variable.strip_prefix("@:") {
        let mut pieces = slice.split(':');
        let start = pieces.next()?.parse::<usize>().ok()?.max(1) - 1;
        let length = pieces
            .next()
            .map(str::parse::<usize>)
            .transpose()
            .ok()?
            .unwrap_or(usize::MAX);
        if pieces.next().is_some() {
            return None;
        }
        arguments
            .iter()
            .skip(start)
            .take(length)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        positional(arguments, variable.parse::<usize>().ok()?).to_owned()
    };
    Some(if value.is_empty() {
        default.unwrap_or_default().to_owned()
    } else {
        value
    })
}

fn push_bounded(output: &mut String, text: &str) -> Result<(), PromptError> {
    if output.len().saturating_add(text.len()) > MAX_EXPANDED_PROMPT_BYTES {
        return Err(PromptError::ExpandedTooLarge);
    }
    output.push_str(text);
    Ok(())
}

fn expand_pi_arguments(template: &str, arguments: &[String]) -> Result<String, PromptError> {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(dollar) = rest.find('$') {
        push_bounded(&mut output, &rest[..dollar])?;
        rest = &rest[dollar..];
        if let Some(expression) = rest.strip_prefix("${") {
            let Some(end) = expression.find('}') else {
                push_bounded(&mut output, "$")?;
                rest = &rest[1..];
                continue;
            };
            let expression = &expression[..end];
            if let Some(value) = braced_argument(expression, arguments) {
                push_bounded(&mut output, &value)?;
                rest = &rest[3 + end..];
                continue;
            }
        } else if let Some(after) = rest.strip_prefix("$ARGUMENTS") {
            push_bounded(&mut output, &arguments.join(" "))?;
            rest = after;
            continue;
        } else if let Some(after) = rest.strip_prefix("$@") {
            push_bounded(&mut output, &arguments.join(" "))?;
            rest = after;
            continue;
        } else {
            let digits = rest[1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            if !digits.is_empty() {
                let index = digits.parse::<usize>().unwrap_or(usize::MAX);
                push_bounded(&mut output, positional(arguments, index))?;
                rest = &rest[1 + digits.len()..];
                continue;
            }
        }
        push_bounded(&mut output, "$")?;
        rest = &rest[1..];
    }
    push_bounded(&mut output, rest)?;
    Ok(output)
}

fn contained_workspace_path(workspace: &Path, value: &str) -> Result<PathBuf, PromptError> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(PromptError::Expansion(format!(
            "file include {value:?} must be a workspace-relative path without '..'"
        )));
    }
    let workspace = workspace.canonicalize().map_err(|error| {
        PromptError::Expansion(format!(
            "cannot resolve workspace {}: {error}",
            workspace.display()
        ))
    })?;
    Ok(workspace.join(path))
}

fn ygg_variable(
    variable: &str,
    raw_prompt: &str,
    context: &PromptRenderContext<'_>,
) -> Result<String, PromptError> {
    match variable {
        "prompt" => Ok(raw_prompt.to_owned()),
        "workspace" => Ok(context.workspace.display().to_string()),
        "selection" => Ok(context.selection.unwrap_or_default().to_owned()),
        _ if variable.starts_with("file:") => {
            let relative = variable["file:".len()..].trim();
            if relative.is_empty() {
                return Err(PromptError::Expansion("empty file include".to_owned()));
            }
            let path = contained_workspace_path(context.workspace, relative)?;
            read_bounded_utf8(&path, MAX_INCLUDED_FILE_BYTES)
                .map_err(|error| PromptError::Expansion(error.to_string()))
        }
        _ if variable.starts_with("skill:") => {
            let name = variable["skill:".len()..].trim();
            context
                .active_skills
                .iter()
                .find(|skill| {
                    skill.descriptor.id == name || skill.descriptor.name.eq_ignore_ascii_case(name)
                })
                .map(|skill| skill.instructions.clone())
                .ok_or_else(|| {
                    PromptError::Expansion(format!(
                        "skill {name:?} is not active; load it explicitly first"
                    ))
                })
        }
        _ => Err(PromptError::Expansion(format!(
            "unknown template variable {{{{{variable}}}}}"
        ))),
    }
}

fn expand_ygg_variables(
    template: &str,
    raw_prompt: &str,
    context: &PromptRenderContext<'_>,
) -> Result<String, PromptError> {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        push_bounded(&mut output, &rest[..start])?;
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(PromptError::Expansion(
                "unclosed {{ template variable".to_owned(),
            ));
        };
        let variable = after[..end].trim();
        let value = ygg_variable(variable, raw_prompt, context)?;
        push_bounded(&mut output, &value)?;
        rest = &after[end + 2..];
    }
    push_bounded(&mut output, rest)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_agent::skills::{SkillDescriptor, SkillSource, SkillTrust};
    use ygg_agent::EntryId;

    fn source(path: PathBuf, trust: PromptTrust) -> PromptSource {
        PromptSource { path, trust }
    }

    #[test]
    fn precedence_is_deterministic_and_invalid_templates_are_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let global = directory.path().join("global");
        let project = directory.path().join("project");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(global.join("review.md"), "global").unwrap();
        std::fs::write(
            project.join("review.md"),
            "---\ndescription: Project review\nargument-hint: '[focus]'\n---\nproject $@",
        )
        .unwrap();
        std::fs::write(project.join("BAD name.md"), "ignored").unwrap();

        let registry = PromptRegistry::from_sources(&[
            source(global, PromptTrust::UserInstalled),
            source(project, PromptTrust::Workspace),
        ]);
        assert_eq!(registry.descriptors().len(), 1);
        assert_eq!(registry.descriptors()[0].description, "Project review");
        assert_eq!(
            registry.descriptors()[0].argument_hint.as_deref(),
            Some("[focus]")
        );
        assert_eq!(registry.diagnostics().len(), 1);
    }

    #[test]
    fn renders_pi_arguments_and_ygg_variables_deterministically() {
        let directory = tempfile::tempdir().unwrap();
        let prompts = directory.path().join("prompts");
        std::fs::create_dir(&prompts).unwrap();
        std::fs::write(directory.path().join("notes.txt"), "bounded notes").unwrap();
        std::fs::write(
            prompts.join("review.md"),
            "first=$1 all=$@ default=${3:-safe} slice=${@:2:2}\n{{prompt}}\n{{workspace}}\n{{selection}}\n{{file:notes.txt}}\n{{skill:audit}}",
        )
        .unwrap();
        let registry = PromptRegistry::from_sources(&[source(prompts, PromptTrust::Workspace)]);
        let active = vec![SkillActivatedSnapshot {
            activation_id: EntryId("activation".into()),
            descriptor: SkillDescriptor {
                id: "audit".into(),
                name: "Audit".into(),
                description: "test".into(),
                version: None,
                source: SkillSource::BuiltIn,
                trust: SkillTrust::BuiltIn,
                required_tools: vec![],
                tags: vec![],
            },
            instructions_hash: "hash".into(),
            instructions: "skill instructions".into(),
        }];
        let rendered = registry
            .render(
                "review",
                "one \"two words\"",
                &PromptRenderContext {
                    workspace: directory.path(),
                    selection: Some("fn selected() {}"),
                    active_skills: &active,
                },
            )
            .unwrap();
        assert!(rendered
            .text
            .contains("first=one all=one two words default=safe slice=two words"));
        assert!(rendered.text.contains("one \"two words\""));
        assert!(rendered.text.contains("fn selected() {}"));
        assert!(rendered.text.contains("bounded notes"));
        assert!(rendered.text.contains("skill instructions"));
        assert_eq!(rendered.content_hash.len(), 64);
    }

    #[test]
    fn supports_small_toml_templates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("implement.toml");
        std::fs::write(
            &path,
            "description = \"Implement locally\"\nargument_hint = \"<task>\"\nprompt = \"Implement {{prompt}} in {{workspace}}\"",
        )
        .unwrap();
        let registry = PromptRegistry::from_sources(&[source(path, PromptTrust::ExplicitExternal)]);
        assert_eq!(registry.descriptors()[0].name, "implement");
        let rendered = registry
            .render(
                "implement",
                "the feature",
                &PromptRenderContext {
                    workspace: directory.path(),
                    selection: None,
                    active_skills: &[],
                },
            )
            .unwrap();
        assert!(rendered.text.contains("Implement the feature in"));
    }

    #[test]
    fn production_discovery_accepts_an_explicit_pi_prompt_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("direct-file-regression.md");
        std::fs::write(
            &path,
            "---\ndescription: Direct Pi prompt\nargument-hint: \"[focus]\"\n---\nReview $@",
        )
        .unwrap();

        let registry =
            PromptRegistry::discover(directory.path(), std::slice::from_ref(&path), false);
        let descriptor = registry
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.name == "direct-file-regression")
            .cloned()
            .expect("explicit prompt file should be discovered");
        assert_eq!(descriptor.description, "Direct Pi prompt");
        assert_eq!(descriptor.argument_hint.as_deref(), Some("[focus]"));
        assert_eq!(descriptor.trust, PromptTrust::ExplicitExternal);
        assert_eq!(
            registry
                .render(
                    "direct-file-regression",
                    "the agent loop",
                    &PromptRenderContext {
                        workspace: directory.path(),
                        selection: None,
                        active_skills: &[],
                    },
                )
                .unwrap()
                .text,
            "Review the agent loop"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prompt_snapshot_does_not_follow_a_file_swapped_to_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let explicit = directory.path().join("stable.md");
        let target = directory.path().join("target.md");
        std::fs::write(&explicit, "original").unwrap();
        std::fs::write(&target, "must not load").unwrap();
        let resolver = ResourceResolver::with_global_ygg_dir(
            directory.path().join("workspace"),
            false,
            directory.path().join("empty-global"),
        );
        let snapshot = resolver.discover(ResourceKind::Prompt, std::slice::from_ref(&explicit));
        std::fs::remove_file(&explicit).unwrap();
        symlink(&target, &explicit).unwrap();

        let registry = PromptRegistry::from_snapshot(&snapshot);
        assert!(!registry.contains("stable"));
        assert!(registry
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.path.ends_with("stable.md")));
    }

    #[test]
    fn rejects_traversal_and_unterminated_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsafe.md");
        std::fs::write(&path, "{{file:../secret}}").unwrap();
        let registry = PromptRegistry::from_sources(&[source(path, PromptTrust::ExplicitExternal)]);
        let context = PromptRenderContext {
            workspace: directory.path(),
            selection: None,
            active_skills: &[],
        };
        assert!(registry.render("unsafe", "", &context).is_err());
        assert!(registry
            .render("unsafe", "'unterminated", &context)
            .is_err());
    }

    #[test]
    fn selected_template_name_and_hash_are_durable_session_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let template = directory.path().join("review.md");
        std::fs::write(&template, "Review $@").unwrap();
        let registry =
            PromptRegistry::from_sources(&[source(template, PromptTrust::ExplicitExternal)]);
        let session_path = directory.path().join("session.jsonl");
        let mut session = ygg_agent::Session::create(&session_path).unwrap();
        let rendered = render_and_record(
            &registry,
            &mut session,
            directory.path(),
            "review",
            "this",
            None,
        )
        .unwrap();
        assert_eq!(rendered.text, "Review this");
        drop(session);

        let session = ygg_agent::Session::open(session_path).unwrap();
        assert!(matches!(
            &session.entries()[0].value,
            ygg_agent::EntryValue::PromptTemplateSelected { name, content_hash }
                if name == "review" && content_hash == &rendered.content_hash
        ));
        assert!(session.context().unwrap().is_empty());
    }

    #[test]
    fn debug_output_identifies_and_shows_the_exact_expansion() {
        let rendered = RenderedPrompt {
            name: "review".into(),
            content_hash: "abc123".into(),
            text: "Inspect the expanded request".into(),
        };
        let debug = debug_expansion(&rendered);
        assert!(debug.contains("review"));
        assert!(debug.contains("abc123"));
        assert!(debug.contains("Inspect the expanded request"));
    }
}
