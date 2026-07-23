#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::resource_resolver::{ResourceKind, ResourceResolver, ResourceSnapshot, ResourceTrust};

/// Stable identity applied before the dynamic environment and tool contract.
pub const BASE_PERSONA: &str = "You are Ygg, an expert coding agent.";

const MAX_CONTEXT_FILE_BYTES: usize = 256 * 1024;
const MAX_CONTEXT_TOTAL_BYTES: usize = 512 * 1024;

fn global_agents_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.canonicalize()
        .unwrap_or(home)
        .join(".ygg")
        .join("AGENTS.md")
}

fn read_if_exists(path: &Path) -> anyhow::Result<Option<String>> {
    let Some(name) = path.file_name() else {
        anyhow::bail!("context path {} has no file name", path.display());
    };
    let Some(parent) = path.parent() else {
        anyhow::bail!("context path {} has no parent", path.display());
    };
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let opened_path = parent.join(name);
    match ygg_agent::secure_fs::read_regular_file_bounded(&opened_path, MAX_CONTEXT_FILE_BYTES) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("context file {} is not valid UTF-8", path.display())),
        Err(ygg_agent::secure_fs::SecureFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(anyhow::anyhow!(
            "refusing context file {}: {error}",
            path.display()
        )),
    }
}

fn prompt_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn base_prompt(config: &Config) -> String {
    // Full contracts already travel with each tool schema. Repeat only the
    // enabled names here so the scaffold stays useful to small local models.
    let mut prompt = format!("{BASE_PERSONA}\n\nCore tools: ");
    let tools = ["read", "edit", "write", "exec", "search"];
    let mut visible_tools = 0usize;
    for name in tools {
        if config.tool_available(name) {
            if visible_tools > 0 {
                prompt.push_str(", ");
            }
            visible_tools += 1;
            prompt.push_str(name);
        }
    }
    if visible_tools == 0 {
        prompt.push_str("none");
    }

    prompt.push_str(
        ". Project tools may also appear.\n\nResponse:\n\
- Direct, terse, conclusion-first. No preamble, prompt echo, needless recap, obvious reasoning/steps.\n\
- Prefer unambiguous fragments/symbols/equations/tables/code; only rationale needed for trust/action.\n\
- Expand only on request or for correctness, ambiguity, safety, or debugging. Keep essential qualifications, units, constraints, uncertainty.\n\
- Show file paths clearly. Never request/reveal private chain-of-thought.\n\nCWD: ",
    );
    prompt.push_str(&prompt_path(&config.invocation_cwd));
    prompt
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
    let base = base_prompt(config);
    if !config.context_files {
        return Ok(base);
    }
    let mut context = Vec::new();
    let mut total = 0usize;
    let mut add = |path: &Path| -> anyhow::Result<()> {
        if let Some(contents) = read_if_exists(path)? {
            total = total
                .checked_add(contents.len())
                .ok_or_else(|| anyhow::anyhow!("aggregate context-file byte count overflowed"))?;
            if total > MAX_CONTEXT_TOTAL_BYTES {
                anyhow::bail!(
                    "context files exceed the aggregate {}-byte limit",
                    MAX_CONTEXT_TOTAL_BYTES
                );
            }
            eprintln!("context: loaded {}", path.display());
            context.push(format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                xml_attribute(&prompt_path(path)),
                contents
            ));
        }
        Ok(())
    };
    add(global)?;
    if config.workspace_trusted {
        for directory in dirs_from_workspace_to_cwd(&config.workspace, &config.invocation_cwd) {
            add(&directory.join("AGENTS.md"))?;
        }
    }
    if context.is_empty() {
        Ok(base)
    } else {
        Ok(format!(
            "{base}\n\n<project_context>\n{}\n</project_context>",
            context.join("\n\n")
        ))
    }
}

/// Compose global then workspace-root-to-leaf AGENTS.md instructions.
pub fn compose_instructions(config: &Config) -> anyhow::Result<String> {
    compose_instructions_at(config, &global_agents_path())
}

use std::fs;
use std::io::{BufRead, BufReader, Read};
use ygg_agent::skills::{
    LoadedSkill, SkillDescriptor, SkillDiagnostic, SkillId, SkillLoadError, SkillQuery,
    SkillRegistry, SkillSearchResult, SkillSource, SkillTrust,
};
use ygg_agent::tool::{
    ErasedToolAdapter, ToolContext, ToolDescriptor, ToolError, ToolInputValidationIssue, TypedTool,
    TypedToolAdapter, ValidateToolInput,
};
use ygg_agent::{Extension, ExtensionHost};

/// Scanning and loading registry for filesystem-based skills.
pub struct FileSystemSkillRegistry {
    _workspace_root: PathBuf,
    _additional_paths: Vec<PathBuf>,
    descriptors: Arc<[SkillDescriptor]>,
    diagnostics: Arc<[SkillDiagnostic]>,
    workspace_trusted: bool,
}

#[derive(serde::Deserialize)]
struct ManifestHeader {
    #[serde(default)]
    id: Option<String>,
    name: String,
    description: String,
    version: Option<String>,
    #[serde(rename = "required-tools", default)]
    required_tools: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

fn validate_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn check_symlinks(root: &Path, target: &Path) -> Result<(), SkillLoadError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| SkillLoadError::SecurityViolation("Target path escapes skill root".into()))?;

    let mut current = root.to_path_buf();
    let meta = fs::symlink_metadata(&current).map_err(|e| SkillLoadError::Io(e.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(SkillLoadError::SymlinkRejected);
    }

    for component in relative.components() {
        if let std::path::Component::Normal(c) = component {
            current.push(c);
            let meta =
                fs::symlink_metadata(&current).map_err(|e| SkillLoadError::Io(e.to_string()))?;
            if meta.file_type().is_symlink() {
                return Err(SkillLoadError::SymlinkRejected);
            }
        } else {
            return Err(SkillLoadError::InvalidResourcePath);
        }
    }
    Ok(())
}

fn check_allowed_subdirs(root: &Path, target: &Path) -> Result<(), SkillLoadError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| SkillLoadError::SecurityViolation("Target path escapes skill root".into()))?;

    let mut components = relative.components();
    if let Some(std::path::Component::Normal(first)) = components.next() {
        let first_str = first.to_str().ok_or(SkillLoadError::InvalidResourcePath)?;
        if first_str == "references" || first_str == "templates" {
            return Ok(());
        }
    }
    Err(SkillLoadError::SecurityViolation(
        "Resources must reside under references/ or templates/".into(),
    ))
}

fn parse_manifest_header(
    skill_md: &Path,
    trust: SkillTrust,
    dir_path: &Path,
) -> Result<SkillDescriptor, SkillLoadError> {
    let file = fs::File::open(skill_md).map_err(|e| SkillLoadError::Io(e.to_string()))?;
    let limit = 32 * 1024; // 32 KiB
                           // `BufRead::read_line` normally grows until a newline. Wrap the file in a
                           // hard byte cap first so a hostile newline-free manifest cannot allocate
                           // an unbounded String during startup scanning.
    let mut reader = BufReader::new(file.take((limit + 1) as u64));

    let mut header_buf = Vec::new();
    let mut total_read = 0usize;
    let mut dash_count = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|e| SkillLoadError::Io(e.to_string()))?;
        if bytes_read == 0 {
            break;
        }
        if total_read.saturating_add(bytes_read) > limit {
            return Err(SkillLoadError::InvalidManifest(
                "YAML frontmatter exceeds the 32 KiB limit".into(),
            ));
        }
        total_read += bytes_read;

        if line.trim() == "---" {
            dash_count += 1;
            if dash_count == 2 {
                break;
            }
        } else if dash_count == 1 {
            header_buf.extend_from_slice(line.as_bytes());
        }
    }

    if dash_count < 2 {
        return Err(SkillLoadError::InvalidManifest(
            "Missing YAML frontmatter delimiters '---'".into(),
        ));
    }

    let header_str = std::str::from_utf8(&header_buf).map_err(|_| SkillLoadError::InvalidUtf8)?;
    let header: ManifestHeader = serde_yaml::from_str(header_str)
        .map_err(|e| SkillLoadError::InvalidManifest(e.to_string()))?;

    let id = header.id.unwrap_or_else(|| {
        dir_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned()
    });
    if !validate_id(&id) {
        return Err(SkillLoadError::InvalidManifest(format!(
            "Invalid ID grammar for '{}'. ID must be lowercase alphanumeric and hyphens.",
            id
        )));
    }

    Ok(SkillDescriptor {
        id,
        name: header.name,
        description: header.description,
        version: header.version,
        source: SkillSource::FileSystem {
            root: dir_path.to_path_buf(),
            entrypoint: skill_md.to_path_buf(),
        },
        trust,
        required_tools: header.required_tools,
        tags: header.tags,
    })
}

/// Return SKILL.md's markdown body, excluding its required YAML frontmatter.
fn strip_frontmatter(content: &str) -> Result<String, SkillLoadError> {
    let mut offset = 0;
    let mut delimiters = 0;
    for line in content.split_inclusive('\n') {
        offset += line.len();
        if line.trim() == "---" {
            delimiters += 1;
            if delimiters == 2 {
                return Ok(content[offset..].to_owned());
            }
        }
    }
    Err(SkillLoadError::InvalidManifest(
        "Missing YAML frontmatter delimiters '---'".into(),
    ))
}

#[cfg(test)]
fn scan_skills_dir(
    dir: &Path,
    trust: SkillTrust,
    map: &mut std::collections::HashMap<SkillId, SkillDescriptor>,
) -> Result<(), SkillLoadError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(dir).map_err(|e| SkillLoadError::Io(e.to_string()))?;
    if metadata.file_type().is_symlink() {
        return Err(SkillLoadError::SymlinkRejected);
    }
    // secure_fs requires an absolute path with no symlinked ancestors. macOS
    // temporary directories commonly arrive through `/var` -> `/private/var`,
    // so normalize the already-validated root before retaining descriptors.
    let dir = dir
        .canonicalize()
        .map_err(|e| SkillLoadError::Io(e.to_string()))?;

    let mut paths = fs::read_dir(&dir)
        .map_err(|e| SkillLoadError::Io(e.to_string()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.is_file() {
                match parse_manifest_header(&skill_md, trust, &path) {
                    Ok(descriptor) => {
                        if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                            if dir_name != descriptor.id {
                                eprintln!(
                                    "warning: ignoring skill {}: directory name does not match manifest ID {}",
                                    path.display(), descriptor.id
                                );
                                continue;
                            }
                        }
                        map.insert(descriptor.id.clone(), descriptor);
                    }
                    Err(error) => eprintln!(
                        "warning: ignoring malformed skill {}: {error}",
                        skill_md.display()
                    ),
                }
            }
        }
    }
    Ok(())
}

impl FileSystemSkillRegistry {
    /// Creates a new skills registry scanning the user config directory, workspace directory, and CLI paths.
    pub fn new(
        workspace_root: PathBuf,
        additional_paths: Vec<PathBuf>,
        workspace_trusted: bool,
    ) -> Result<Self, SkillLoadError> {
        let resolver = ResourceResolver::new(workspace_root.clone(), workspace_trusted);
        let snapshot = resolver.discover(ResourceKind::Skill, &additional_paths);
        Ok(Self::from_snapshot(
            workspace_root,
            additional_paths,
            workspace_trusted,
            &snapshot,
        ))
    }

    /// Parse an immutable skill discovery snapshot from the shared resolver.
    pub fn from_snapshot(
        workspace_root: PathBuf,
        additional_paths: Vec<PathBuf>,
        workspace_trusted: bool,
        snapshot: &ResourceSnapshot,
    ) -> Self {
        debug_assert_eq!(snapshot.kind, ResourceKind::Skill);
        let mut diagnostics = snapshot
            .diagnostics()
            .iter()
            .map(|diagnostic| SkillDiagnostic {
                path: diagnostic.path.clone(),
                message: diagnostic.message.clone(),
            })
            .collect::<Vec<_>>();
        for diagnostic in snapshot.diagnostics() {
            eprintln!(
                "resource: skill {}: {}",
                diagnostic.path.display(),
                diagnostic.message
            );
        }
        let mut descriptors = Vec::new();
        for resource in snapshot.resources() {
            let trust = match resource.trust {
                ResourceTrust::User => SkillTrust::UserInstalled,
                ResourceTrust::TrustedWorkspace => SkillTrust::Workspace,
                ResourceTrust::Explicit => SkillTrust::ExplicitExternal,
            };
            let entrypoint = match resource.path.canonicalize() {
                Ok(path) => path,
                Err(error) => {
                    eprintln!(
                        "warning: ignoring unreadable skill {}: {error}",
                        resource.path.display()
                    );
                    diagnostics.push(SkillDiagnostic {
                        path: resource.path.clone(),
                        message: format!("cannot read skill entrypoint: {error}"),
                    });
                    continue;
                }
            };
            let Some(skill_root) = entrypoint.parent() else {
                eprintln!(
                    "warning: ignoring malformed skill path {}",
                    resource.path.display()
                );
                diagnostics.push(SkillDiagnostic {
                    path: resource.path.clone(),
                    message: "skill entrypoint has no parent directory".into(),
                });
                continue;
            };
            match parse_manifest_header(&entrypoint, trust, skill_root) {
                Ok(descriptor) if descriptor.id == resource.name => descriptors.push(descriptor),
                Ok(descriptor) => {
                    let message = format!(
                        "manifest ID {} does not match directory name {}",
                        descriptor.id, resource.name
                    );
                    eprintln!(
                        "warning: ignoring skill {}: {message}",
                        resource.path.display()
                    );
                    diagnostics.push(SkillDiagnostic {
                        path: resource.path.clone(),
                        message,
                    });
                }
                Err(error) => {
                    eprintln!(
                        "warning: ignoring malformed skill {}: {error}",
                        resource.path.display()
                    );
                    diagnostics.push(SkillDiagnostic {
                        path: resource.path.clone(),
                        message: error.to_string(),
                    });
                }
            }
        }
        descriptors.sort_by(|left, right| left.id.cmp(&right.id));
        Self {
            _workspace_root: workspace_root,
            _additional_paths: additional_paths,
            descriptors: Arc::from(descriptors),
            diagnostics: Arc::from(diagnostics),
            workspace_trusted,
        }
    }

    #[cfg(test)]
    fn new_with_user_skills_dir(
        workspace_root: PathBuf,
        additional_paths: Vec<PathBuf>,
        workspace_trusted: bool,
        user_dir: Option<PathBuf>,
    ) -> Result<Self, SkillLoadError> {
        let mut map = std::collections::HashMap::new();
        let mut scan = |path: &Path, trust| {
            if let Err(error) = scan_skills_dir(path, trust, &mut map) {
                eprintln!(
                    "warning: failed to scan skills directory {}: {error}",
                    path.display()
                );
            }
        };

        if let Some(user_dir) = user_dir.as_deref() {
            scan(user_dir, SkillTrust::UserInstalled);
        }
        let workspace_dir = workspace_root.join(".ygg").join("skills");
        // An untrusted checkout must not publish metadata or shadow a trusted
        // user skill with the same ID. The load-time check remains a
        // defense-in-depth guard for any future descriptor source.
        if workspace_trusted {
            scan(&workspace_dir, SkillTrust::Workspace);
        }
        for path in &additional_paths {
            scan(path, SkillTrust::ExplicitExternal);
        }

        let mut descriptors: Vec<SkillDescriptor> = map.into_values().collect();
        descriptors.sort_by(|left, right| left.id.cmp(&right.id));

        Ok(Self {
            _workspace_root: workspace_root,
            _additional_paths: additional_paths,
            descriptors: Arc::from(descriptors),
            diagnostics: Arc::from([]),
            workspace_trusted,
        })
    }
}

impl SkillRegistry for FileSystemSkillRegistry {
    fn descriptors(&self) -> Arc<[SkillDescriptor]> {
        self.descriptors.clone()
    }

    fn diagnostics(&self) -> Arc<[SkillDiagnostic]> {
        self.diagnostics.clone()
    }

    fn find(&self, query: &SkillQuery) -> Vec<SkillSearchResult> {
        let q = query.text.to_ascii_lowercase();
        self.descriptors
            .iter()
            .filter(|d| {
                d.id.to_ascii_lowercase().contains(&q)
                    || d.name.to_ascii_lowercase().contains(&q)
                    || d.description.to_ascii_lowercase().contains(&q)
                    || d.tags.iter().any(|t| t.to_ascii_lowercase().contains(&q))
            })
            .map(|d| SkillSearchResult {
                descriptor: d.clone(),
            })
            .collect()
    }

    fn load(&self, id: &SkillId) -> Result<LoadedSkill, SkillLoadError> {
        let desc = self
            .descriptors
            .iter()
            .find(|d| &d.id == id)
            .ok_or_else(|| SkillLoadError::NotFound(id.clone()))?;

        if desc.trust == SkillTrust::Workspace && !self.workspace_trusted {
            return Err(SkillLoadError::UntrustedWorkspace);
        }

        let entrypoint = match &desc.source {
            SkillSource::BuiltIn => {
                return Err(SkillLoadError::UnsupportedSource("built-in".to_string()))
            }
            SkillSource::FileSystem { entrypoint, .. } => entrypoint,
        };

        let root = match &desc.source {
            SkillSource::FileSystem { root, .. } => root,
            _ => unreachable!(),
        };
        check_symlinks(root, entrypoint)?;

        let limit = 256 * 1024; // 256 KiB
        let content_bytes = ygg_agent::secure_fs::read_regular_file_bounded(entrypoint, limit)
            .map_err(|error| match error {
                ygg_agent::secure_fs::SecureFileError::TooLarge { actual, .. } => {
                    SkillLoadError::ResourceTooLarge(actual)
                }
                other => SkillLoadError::Io(other.to_string()),
            })?;
        let content_str =
            String::from_utf8(content_bytes).map_err(|_| SkillLoadError::InvalidUtf8)?;
        let hash = ygg_agent::content_hash(content_str.as_bytes());

        Ok(LoadedSkill {
            descriptor: desc.clone(),
            instructions: strip_frontmatter(&content_str)?,
            content_hash: hash,
        })
    }

    fn read_resource(&self, snapshot: &LoadedSkill, path: &str) -> Result<String, SkillLoadError> {
        let root_path = match &snapshot.descriptor.source {
            SkillSource::BuiltIn => {
                return Err(SkillLoadError::UnsupportedSource("built-in".to_string()))
            }
            SkillSource::FileSystem { root, .. } => root,
        };

        let path_obj = Path::new(path);
        if path_obj.is_absolute() || path.contains("..") {
            return Err(SkillLoadError::InvalidResourcePath);
        }

        let target = root_path.join(path_obj);
        check_allowed_subdirs(root_path, &target)?;
        check_symlinks(root_path, &target)?;

        let size_limit = 512 * 1024; // 512 KiB
        let content_bytes = ygg_agent::secure_fs::read_regular_file_bounded(&target, size_limit)
            .map_err(|error| match error {
                ygg_agent::secure_fs::SecureFileError::TooLarge { actual, .. } => {
                    SkillLoadError::ResourceTooLarge(actual)
                }
                other => SkillLoadError::Io(other.to_string()),
            })?;
        let content_str =
            String::from_utf8(content_bytes).map_err(|_| SkillLoadError::InvalidUtf8)?;

        Ok(content_str)
    }
}

/// Validates a skill's declared tool requirements against the exact final tool
/// registry supplied to a running Agent. The frontend has already applied
/// explicit allowlists, sandbox capability gates, and executable-extension
/// registration before these names reach a [`ToolContext`].
pub fn validate_skill_requirements(
    descriptor: &SkillDescriptor,
    registered_tools: &[String],
) -> Result<(), SkillLoadError> {
    let missing = descriptor
        .required_tools
        .iter()
        .filter(|required| !registered_tools.iter().any(|name| name == *required))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SkillLoadError::MissingRequiredTools(missing))
    }
}

/// Extension that exposes skill discovery, activation, and lazy resources to
/// the model through the same registration boundary as the core tools.
#[allow(dead_code)]
pub struct SkillToolsExtension {
    registry: Arc<dyn SkillRegistry>,
}

#[allow(dead_code)]
impl SkillToolsExtension {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
    }
}

impl Extension for SkillToolsExtension {
    fn register(&self, host: &mut ExtensionHost) {
        host.tool(ErasedToolAdapter::new(TypedToolAdapter::new(
            SearchSkillsTool {
                registry: self.registry.clone(),
            },
        )));
        host.tool(ErasedToolAdapter::new(TypedToolAdapter::new(
            LoadSkillTool {
                registry: self.registry.clone(),
            },
        )));
        host.tool(ErasedToolAdapter::new(TypedToolAdapter::new(
            ReadSkillResourceTool {
                registry: self.registry.clone(),
            },
        )));
    }
}

/// Input schema for `search_skills`.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct SearchSkillsInput {
    /// Search terms.
    #[schemars(length(min = 1))]
    pub query: String,
}

impl ValidateToolInput for SearchSkillsInput {
    fn validate_tool_input(&self) -> Result<(), Vec<ToolInputValidationIssue>> {
        validate_non_empty_fields(&[("$.query", &self.query)])
    }
}

/// Output schema for `search_skills`.
#[derive(serde::Serialize)]
#[allow(dead_code)]
pub struct SearchSkillsOutput {
    /// List of matching skills.
    pub skills: Vec<SkillSearchResultSnapshot>,
}

/// Metadata summary of a skill search result.
#[derive(serde::Serialize)]
#[allow(dead_code)]
pub struct SkillSearchResultSnapshot {
    /// Unique lowercase identifier.
    pub id: String,
    /// Human readable name.
    pub name: String,
    /// Purpose description.
    pub description: String,
    /// Trust classification level.
    pub trust: String,
    /// Required base tools.
    pub required_tools: Vec<String>,
    /// Categorization tags.
    pub tags: Vec<String>,
}

/// Search tool to discover applicable skills in workspace or config paths.
#[allow(dead_code)]
pub struct SearchSkillsTool {
    /// The skill registry to search.
    pub registry: Arc<dyn SkillRegistry>,
}

#[async_trait::async_trait]
impl TypedTool for SearchSkillsTool {
    type Input = SearchSkillsInput;
    type Output = SearchSkillsOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "search_skills".to_string(),
            description: "Search for applicable skills installed in the system or workspace using query terms. Returns metadata matching the search query.".to_string(),
        }
    }

    async fn execute(
        &self,
        input: Self::Input,
        _context: &ToolContext<'_>,
    ) -> Result<Self::Output, ToolError> {
        let results = self.registry.find(&SkillQuery { text: input.query });
        let skills = results
            .into_iter()
            .map(|r| SkillSearchResultSnapshot {
                id: r.descriptor.id,
                name: r.descriptor.name,
                description: r.descriptor.description,
                trust: format!("{:?}", r.descriptor.trust),
                required_tools: r.descriptor.required_tools,
                tags: r.descriptor.tags,
            })
            .collect();
        Ok(SearchSkillsOutput { skills })
    }
}

/// Input schema for `load_skill`.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct LoadSkillInput {
    /// Unique lowercase identifier.
    #[schemars(length(min = 1))]
    pub id: String,
}

impl ValidateToolInput for LoadSkillInput {
    fn validate_tool_input(&self) -> Result<(), Vec<ToolInputValidationIssue>> {
        validate_non_empty_fields(&[("$.id", &self.id)])
    }
}

/// Output schema for `load_skill`.
#[derive(serde::Serialize)]
#[allow(dead_code)]
pub struct LoadSkillOutput {
    /// Unique lowercase identifier.
    pub id: String,
    /// Human readable name.
    pub name: String,
    /// Content hash of the instructions.
    pub hash: String,
    /// The active session activation identifier.
    pub activation_id: String,
}

/// Tool to explicitly load and activate a skill.
#[allow(dead_code)]
pub struct LoadSkillTool {
    /// The skill registry to load from.
    pub registry: Arc<dyn SkillRegistry>,
}

#[async_trait::async_trait]
impl TypedTool for LoadSkillTool {
    type Input = LoadSkillInput;
    type Output = LoadSkillOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "load_skill".to_string(),
            description: "Load and activate a skill by its unique ID. Its instructions enter the system context on the next model turn; this result returns only activation metadata.".to_string(),
        }
    }

    async fn execute(
        &self,
        input: Self::Input,
        context: &ToolContext<'_>,
    ) -> Result<Self::Output, ToolError> {
        let loaded = self
            .registry
            .load(&input.id)
            .map_err(|e| ToolError::new(format!("Failed to load skill: {e}")))?;
        validate_skill_requirements(&loaded.descriptor, context.registered_tools)
            .map_err(|e| ToolError::new(format!("Failed to load skill: {e}")))?;

        let event = ygg_agent::session::EntryValue::SkillActivated {
            descriptor: loaded.descriptor.clone(),
            instructions_hash: loaded.content_hash.clone(),
            instructions: loaded.instructions.clone(),
        };

        let activation_id = context.append_session_entry(event).await?;

        Ok(LoadSkillOutput {
            id: loaded.descriptor.id,
            name: loaded.descriptor.name,
            hash: loaded.content_hash,
            activation_id: activation_id.0,
        })
    }
}

/// Input schema for `read_skill_resource`.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct ReadSkillResourceInput {
    /// The skill ID.
    #[schemars(length(min = 1))]
    pub skill_id: String,
    /// Relative path under references/ or templates/.
    #[schemars(length(min = 1))]
    pub resource_path: String,
    /// The activation ID returned by load_skill.
    #[schemars(length(min = 1))]
    pub activation_id: String,
}

impl ValidateToolInput for ReadSkillResourceInput {
    fn validate_tool_input(&self) -> Result<(), Vec<ToolInputValidationIssue>> {
        validate_non_empty_fields(&[
            ("$.skill_id", &self.skill_id),
            ("$.resource_path", &self.resource_path),
            ("$.activation_id", &self.activation_id),
        ])
    }
}

fn validate_non_empty_fields(
    fields: &[(&str, &String)],
) -> Result<(), Vec<ToolInputValidationIssue>> {
    let issues = fields
        .iter()
        .filter(|(_, value)| value.is_empty())
        .map(|(path, value)| ToolInputValidationIssue {
            path: (*path).to_string(),
            expected: "a string with at least one character".to_string(),
            received: format!("{}-character string", value.chars().count()),
        })
        .collect::<Vec<_>>();
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

/// Output schema for `read_skill_resource`.
#[derive(Debug, serde::Serialize)]
#[allow(dead_code)]
pub struct ReadSkillResourceOutput {
    /// Content of the loaded resource.
    pub content: String,
}

/// Tool to lazily read a reference or template file associated with an active skill.
#[allow(dead_code)]
pub struct ReadSkillResourceTool {
    /// The skill registry to read from.
    pub registry: Arc<dyn SkillRegistry>,
}

#[async_trait::async_trait]
impl TypedTool for ReadSkillResourceTool {
    type Input = ReadSkillResourceInput;
    type Output = ReadSkillResourceOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read_skill_resource".to_string(),
            description: "Read a reference or template file associated with an active skill. Appends a resource read event to the session.".to_string(),
        }
    }

    async fn execute(
        &self,
        input: Self::Input,
        context: &ToolContext<'_>,
    ) -> Result<Self::Output, ToolError> {
        let activation_id = ygg_agent::session::EntryId(input.activation_id.clone());
        let active = context.active_skills.iter().find(|skill| {
            skill.activation_id == activation_id && skill.descriptor.id == input.skill_id
        });
        let Some(active) = active else {
            return Err(ToolError::new(
                "activation_id does not identify an active activation of the requested skill",
            ));
        };

        let loaded = self
            .registry
            .load(&input.skill_id)
            .map_err(|e| ToolError::new(format!("Failed to load skill metadata: {e}")))?;
        if loaded.descriptor.source != active.descriptor.source
            || loaded.content_hash != active.instructions_hash
        {
            return Err(ToolError::new(format!(
                "Failed to read resource: {}",
                SkillLoadError::SourceChanged
            )));
        }

        let content = self
            .registry
            .read_resource(&loaded, &input.resource_path)
            .map_err(|e| ToolError::new(format!("Failed to read resource: {e}")))?;

        let hash = ygg_agent::content_hash(content.as_bytes());

        let event = ygg_agent::session::EntryValue::SkillResourceRead {
            activation_id,
            skill_id: input.skill_id.clone(),
            resource_path: input.resource_path.clone(),
            start_line: None,
            line_count: None,
            content_hash: hash,
            content: content.clone(),
        };

        context.append_session_entry(event).await?;

        Ok(ReadSkillResourceOutput { content })
    }
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
            model_explicit: false,
            reasoning: ygg_ai::ReasoningConfig::Off,
            reasoning_explicit: false,
            cache_retention: ygg_ai::CacheRetention::Short,
            sandbox: SandboxPolicy::default(),
            theme: None,
            theme_paths: vec![],
            color: crate::config::ColorMode::Auto,
            mouse: crate::config::MouseMode::Auto,
            plain: false,
            session_dir: PathBuf::from("sessions"),
            compaction: CompactionPolicy::default(),
            max_cost_microdollars: None,
            cost_warning_microdollars: None,
            show_turn_cost: false,
            max_turns: Some(40),
            show_reasoning_in_print: false,
            initial_prompt: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_paths: vec![],
            mode: Mode::Interactive,
            resume: ResumeSelector::New,
            skill_paths: vec![],
            extension_paths: vec![],
            enabled_extensions: vec![],
            trusted_extensions: vec![],
            invocation_trusted_extensions: vec![],
            tools: crate::config::ToolPolicy::default(),
            context_files: true,
            offline: true,
            workspace_trusted: true,
        }
    }

    #[test]
    fn typed_skill_inputs_report_every_empty_field_without_validator_macros() {
        let valid = SearchSkillsInput {
            query: "local model".to_string(),
        };
        assert!(valid.validate_tool_input().is_ok());

        let invalid = ReadSkillResourceInput {
            skill_id: String::new(),
            resource_path: String::new(),
            activation_id: String::new(),
        };
        let issues = invalid.validate_tool_input().unwrap_err();
        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0].path, "$.skill_id");
        assert_eq!(issues[1].path, "$.resource_path");
        assert_eq!(issues[2].path, "$.activation_id");
        assert!(issues
            .iter()
            .all(|issue| issue.received == "0-character string"));
    }

    #[test]
    fn base_prompt_is_grounded_dynamic_and_compact() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("src/agent");
        std::fs::create_dir_all(&nested).unwrap();
        let config = config(root.path().to_owned(), nested.clone());
        let prompt = base_prompt(&config);

        assert_eq!(BASE_PERSONA, "You are Ygg, an expert coding agent.");
        assert!(prompt.contains(&format!("CWD: {}", nested.display())));
        assert!(
            prompt.contains("Core tools: read, edit, write, exec."),
            "{prompt}"
        );
        assert!(
            !prompt.contains("Core tools: read, edit, write, exec, search"),
            "{prompt}"
        );
        assert!(prompt.contains("Project tools may also appear"));
        assert!(prompt.contains("Direct, terse, conclusion-first"));
        assert!(prompt.contains("No preamble, prompt echo, needless recap"));
        assert!(prompt.contains("fragments/symbols/equations/tables/code"));
        assert!(prompt.contains("only rationale needed for trust/action"));
        assert!(prompt.contains("Expand only on request or for correctness"));
        assert!(prompt.contains("essential qualifications, units, constraints, uncertainty"));
        assert!(prompt.contains("Show file paths clearly"));
        assert!(prompt.contains("Never request/reveal private chain-of-thought"));
        let cwd = prompt_path(&nested);
        let scaffold = prompt.strip_suffix(&cwd).expect("prompt ends with CWD");
        assert!(
            scaffold.len() <= 550,
            "base prompt scaffold grew to {} bytes",
            scaffold.len()
        );
    }

    #[test]
    fn base_prompt_only_advertises_tools_that_can_execute() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config(root.path().to_owned(), root.path().to_owned());
        config.sandbox.allow_edit = false;
        config.sandbox.allow_write = false;
        config.sandbox.allow_process = false;

        let prompt = base_prompt(&config);
        assert!(prompt.contains("Core tools: read."), "{prompt}");
        assert!(!prompt.contains("Core tools: read, edit"), "{prompt}");
    }

    #[test]
    fn context_paths_are_safe_xml_attributes() {
        assert_eq!(
            xml_attribute("one & \"two\" <three>"),
            "one &amp; &quot;two&quot; &lt;three&gt;"
        );
    }

    #[test]
    fn skill_activation_result_does_not_duplicate_instruction_bodies() {
        let value = serde_json::to_value(LoadSkillOutput {
            id: "focused-review".into(),
            name: "Focused review".into(),
            hash: "abc123".into(),
            activation_id: "entry-1".into(),
        })
        .unwrap();
        assert!(value.get("instructions").is_none());
        assert_eq!(value["hash"], "abc123");
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
        for path in [
            global,
            root.path().join("AGENTS.md"),
            root.path().join("a/AGENTS.md"),
            nested.join("AGENTS.md"),
        ] {
            assert!(
                output.contains(&format!(
                    "<project_instructions path=\"{}\">",
                    prompt_path(&path)
                )),
                "{output}"
            );
        }
        assert!(output.contains("<project_context>"));
        assert!(output.contains("</project_context>"));
    }

    #[test]
    fn untrusted_or_disabled_workspace_context_never_enters_the_system_prompt() {
        let root = tempfile::tempdir().unwrap();
        let global_dir = tempfile::tempdir().unwrap();
        let global = global_dir.path().join("AGENTS.md");
        std::fs::write(&global, "trusted global context").unwrap();
        std::fs::write(
            root.path().join("AGENTS.md"),
            "untrusted workspace sentinel",
        )
        .unwrap();
        let mut config = config(root.path().to_owned(), root.path().to_owned());
        config.workspace_trusted = false;

        let output = compose_instructions_at(&config, &global).unwrap();
        assert!(output.contains("trusted global context"));
        assert!(!output.contains("untrusted workspace sentinel"));

        config.context_files = false;
        let output = compose_instructions_at(&config, &global).unwrap();
        assert_eq!(output, base_prompt(&config));
    }

    #[cfg(unix)]
    #[test]
    fn context_symlinks_and_special_files_are_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside secret sentinel").unwrap();
        symlink(outside.path(), root.path().join("AGENTS.md")).unwrap();
        let config = config(root.path().to_owned(), root.path().to_owned());
        let missing_global = root.path().join("missing-global");

        let error = compose_instructions_at(&config, &missing_global).unwrap_err();
        assert!(
            error.to_string().contains("refusing context file"),
            "{error}"
        );
    }

    #[test]
    fn oversized_context_file_is_rejected_by_actual_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("AGENTS.md"),
            vec![b'x'; MAX_CONTEXT_FILE_BYTES + 1],
        )
        .unwrap();
        let config = config(root.path().to_owned(), root.path().to_owned());
        let error =
            compose_instructions_at(&config, &root.path().join("missing-global")).unwrap_err();
        assert!(error.to_string().contains("too large"), "{error}");
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

    #[test]
    fn oversized_newline_free_skill_header_is_rejected_at_the_byte_limit() {
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join("oversized");
        std::fs::create_dir(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(&skill_md, vec![b'a'; 1024 * 1024]).unwrap();

        let error =
            parse_manifest_header(&skill_md, SkillTrust::Workspace, &skill_dir).unwrap_err();
        assert!(error.to_string().contains("32 KiB"), "{error}");
    }

    #[test]
    fn test_skills_scanning_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let user_dir = temp.path().join("user/skills");
        let workspace_dir = temp.path().join("workspace/.ygg/skills");
        let cli_dir = temp.path().join("cli/skills");

        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::create_dir_all(&cli_dir).unwrap();

        // Skill in user_dir
        let user_skill_dir = user_dir.join("test-skill");
        std::fs::create_dir_all(&user_skill_dir).unwrap();
        std::fs::write(user_skill_dir.join("SKILL.md"), "---\nid: test-skill\nname: User Skill\ndescription: User skill desc\n---\nUser instructions").unwrap();

        // Skill in workspace_dir
        let ws_skill_dir = workspace_dir.join("test-skill");
        std::fs::create_dir_all(&ws_skill_dir).unwrap();
        std::fs::write(ws_skill_dir.join("SKILL.md"), "---\nid: test-skill\nname: Workspace Skill\ndescription: Workspace skill desc\n---\nWorkspace instructions").unwrap();

        // Skill in cli_dir
        let cli_skill_dir = cli_dir.join("test-skill");
        std::fs::create_dir_all(&cli_skill_dir).unwrap();
        std::fs::write(cli_skill_dir.join("SKILL.md"), "---\nid: test-skill\nname: CLI Skill\ndescription: CLI skill desc\n---\nCLI instructions").unwrap();

        // Instantiate registry with workspace and additional path
        let registry = FileSystemSkillRegistry::new_with_user_skills_dir(
            temp.path().join("workspace"),
            vec![cli_dir.clone()],
            true,
            Some(user_dir.clone()),
        )
        .unwrap();

        // CLI path has highest precedence, so it should win!
        let loaded = registry.load(&"test-skill".to_string()).unwrap();
        assert_eq!(loaded.descriptor.name, "CLI Skill");
        assert_eq!(loaded.instructions.trim(), "CLI instructions");

        // Now if we recreate without CLI path, Workspace should win
        let registry2 = FileSystemSkillRegistry::new_with_user_skills_dir(
            temp.path().join("workspace"),
            vec![],
            true,
            Some(user_dir.clone()),
        )
        .unwrap();
        let loaded2 = registry2.load(&"test-skill".to_string()).unwrap();
        assert_eq!(loaded2.descriptor.name, "Workspace Skill");

        // An untrusted workspace is omitted entirely, so it cannot shadow the
        // trusted user-installed descriptor with the same ID.
        let untrusted = FileSystemSkillRegistry::new_with_user_skills_dir(
            temp.path().join("workspace"),
            vec![],
            false,
            Some(user_dir.clone()),
        )
        .unwrap();
        let loaded_untrusted = untrusted.load(&"test-skill".to_string()).unwrap();
        assert_eq!(loaded_untrusted.descriptor.name, "User Skill");
        assert!(untrusted
            .descriptors()
            .iter()
            .all(|descriptor| descriptor.trust != SkillTrust::Workspace));

        // With no workspace override, the injected user directory is used.
        let registry3 = FileSystemSkillRegistry::new_with_user_skills_dir(
            temp.path().join("empty-workspace"),
            vec![],
            true,
            Some(user_dir),
        )
        .unwrap();
        let loaded3 = registry3.load(&"test-skill".to_string()).unwrap();
        assert_eq!(loaded3.descriptor.name, "User Skill");
    }

    #[test]
    fn shared_resolver_skill_snapshot_accepts_standard_name_only_frontmatter() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let global = temp.path().join("global/.ygg");
        let skill = global.join("skills/focused-review");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: Focused Review\ndescription: Review only relevant code.\n---\nStay focused.",
        )
        .unwrap();
        let resolver = ResourceResolver::with_global_ygg_dir(workspace.clone(), true, global);
        let snapshot = resolver.discover(ResourceKind::Skill, &[]);
        let registry = FileSystemSkillRegistry::from_snapshot(workspace, vec![], true, &snapshot);
        let loaded = registry.load(&"focused-review".to_owned()).unwrap();
        assert_eq!(loaded.descriptor.name, "Focused Review");
        assert_eq!(loaded.descriptor.id, "focused-review");
        assert_eq!(loaded.instructions.trim(), "Stay focused.");
    }

    #[test]
    fn malformed_and_mismatched_skills_remain_inspectable_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let global = temp.path().join("global/.ygg");
        let malformed = global.join("skills/malformed");
        let mismatched = global.join("skills/wrong-directory");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&malformed).unwrap();
        std::fs::create_dir_all(&mismatched).unwrap();
        std::fs::write(
            malformed.join("SKILL.md"),
            "---\nname: Missing description\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            mismatched.join("SKILL.md"),
            "---\nid: declared-id\nname: Declared\ndescription: mismatch\n---\nbody",
        )
        .unwrap();

        let resolver = ResourceResolver::with_global_ygg_dir(workspace.clone(), true, global);
        let snapshot = resolver.discover(ResourceKind::Skill, &[]);
        let registry = FileSystemSkillRegistry::from_snapshot(workspace, vec![], true, &snapshot);
        let diagnostics = registry.diagnostics();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.path.ends_with("malformed/SKILL.md")
                && diagnostic.message.contains("missing field")
        }));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.path.ends_with("wrong-directory/SKILL.md")
                && diagnostic.message.contains("does not match directory name")
        }));
        assert!(registry.descriptors().is_empty());
    }

    #[tokio::test]
    async fn resource_reads_require_a_matching_active_activation() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let sandbox = ygg_agent::SandboxConfig::new(&workspace);
        let tool = ReadSkillResourceTool {
            registry: Arc::new(
                FileSystemSkillRegistry::new(workspace.clone(), vec![], true).unwrap(),
            ),
        };
        let context = ToolContext {
            workspace: &workspace,
            sandbox: &sandbox,
            execution_scope: "resource-loader-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ygg_agent::ToolProgressSink::null(),
            cancellation: Default::default(),
        };

        let error = tool
            .execute(
                ReadSkillResourceInput {
                    skill_id: "fabricated-skill".to_string(),
                    resource_path: "references/secret.md".to_string(),
                    activation_id: "fabricated-activation".to_string(),
                },
                &context,
            )
            .await
            .unwrap_err();
        assert!(error
            .message
            .contains("does not identify an active activation"));
    }

    #[test]
    fn required_tools_must_be_registered_and_enabled() {
        let descriptor = SkillDescriptor {
            id: "needs-tools".to_string(),
            name: "Needs tools".to_string(),
            description: "test".to_string(),
            version: None,
            source: SkillSource::BuiltIn,
            trust: SkillTrust::BuiltIn,
            required_tools: vec!["edit".to_string(), "unknown-tool".to_string()],
            tags: vec![],
        };
        let registered_tools = vec!["read".to_string(), "extension-review".to_string()];
        assert!(matches!(
            validate_skill_requirements(&descriptor, &registered_tools),
            Err(SkillLoadError::MissingRequiredTools(missing))
                if missing == vec!["edit".to_string(), "unknown-tool".to_string()]
        ));

        let extension_descriptor = SkillDescriptor {
            required_tools: vec!["read".to_string(), "extension-review".to_string()],
            ..descriptor
        };
        assert!(validate_skill_requirements(&extension_descriptor, &registered_tools).is_ok());
    }

    #[tokio::test]
    async fn resource_read_rejects_a_changed_skill_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let skill_root = workspace.join(".ygg/skills/changeable");
        std::fs::create_dir_all(skill_root.join("references")).unwrap();
        let skill_file = skill_root.join("SKILL.md");
        std::fs::write(
            &skill_file,
            "---\nid: changeable\nname: Changeable\ndescription: test\n---\noriginal",
        )
        .unwrap();
        std::fs::write(skill_root.join("references/info.md"), "reference").unwrap();

        let registry: Arc<dyn SkillRegistry> =
            Arc::new(FileSystemSkillRegistry::new(workspace.clone(), vec![], true).unwrap());
        let loaded = registry.load(&"changeable".to_string()).unwrap();
        let active = vec![ygg_agent::session::SkillActivatedSnapshot {
            activation_id: ygg_agent::session::EntryId("activation".to_string()),
            descriptor: loaded.descriptor,
            instructions_hash: loaded.content_hash,
            instructions: loaded.instructions,
        }];
        std::fs::write(
            skill_file,
            "---\nid: changeable\nname: Changeable\ndescription: test\n---\nchanged",
        )
        .unwrap();

        let sandbox = ygg_agent::SandboxConfig::new(&workspace);
        let context = ToolContext {
            workspace: &workspace,
            sandbox: &sandbox,
            execution_scope: "changed-skill-test",
            active_skills: &active,
            registered_tools: &[],
            progress: ygg_agent::ToolProgressSink::null(),
            cancellation: Default::default(),
        };
        let error = ReadSkillResourceTool { registry }
            .execute(
                ReadSkillResourceInput {
                    skill_id: "changeable".to_string(),
                    resource_path: "references/info.md".to_string(),
                    activation_id: "activation".to_string(),
                },
                &context,
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("Skill source changed"), "{error}");
    }

    #[test]
    fn test_yaml_frontmatter_limits_and_validation() {
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join("workspace/.ygg/skills/invalid-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Invalid ID formatting (uppercase/unsupported chars)
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nid: Invalid-Skill\nname: Invalid\ndescription: Invalid desc\n---\nInstructions",
        )
        .unwrap();
        let registry =
            FileSystemSkillRegistry::new(temp.path().to_path_buf(), vec![], true).unwrap();
        assert!(registry.load(&"Invalid-Skill".to_string()).is_err());

        // Frontmatter exceeding 32 KiB
        let skill_dir2 = temp.path().join("workspace/.ygg/skills/large-frontmatter");
        std::fs::create_dir_all(&skill_dir2).unwrap();
        let mut large_yaml = String::from("---\nid: large-frontmatter\nname: Large\ndescription: ");
        large_yaml.push_str(&"a".repeat(33 * 1024)); // >32 KiB
        large_yaml.push_str("\n---\nInstructions");
        std::fs::write(skill_dir2.join("SKILL.md"), large_yaml).unwrap();

        let registry2 =
            FileSystemSkillRegistry::new(temp.path().to_path_buf(), vec![], true).unwrap();
        assert!(registry2.load(&"large-frontmatter".to_string()).is_err());
    }

    #[test]
    fn test_symlink_rejection() {
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join(".ygg/skills/test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Create references directory
        let ref_dir = skill_dir.join("references");
        std::fs::create_dir_all(&ref_dir).unwrap();

        // Create a symlink to outside directory inside references
        let secret_file = temp.path().join("secret.txt");
        std::fs::write(&secret_file, "secret data").unwrap();

        let symlink_target = ref_dir.join("symlink.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret_file, &symlink_target).unwrap();

        // Setup SKILL.md
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nid: test-skill\nname: Test\ndescription: Test\n---\nInstructions",
        )
        .unwrap();

        let registry =
            FileSystemSkillRegistry::new(temp.path().to_path_buf(), vec![], true).unwrap();
        let loaded = registry.load(&"test-skill".to_string()).unwrap();

        // Reading resource that is a symlink should fail!
        let resource_res = registry.read_resource(&loaded, "references/symlink.txt");
        assert!(matches!(resource_res, Err(SkillLoadError::SymlinkRejected)));
    }

    #[test]
    fn test_session_active_skills_and_deactivation() {
        let temp = tempfile::tempdir().unwrap();
        let session_path = temp.path().join("session.jsonl");
        let mut session = ygg_agent::session::Session::create(session_path).unwrap();

        let desc = SkillDescriptor {
            id: "my-skill".to_string(),
            name: "My Skill".to_string(),
            description: "Desc".to_string(),
            version: None,
            source: SkillSource::FileSystem {
                root: PathBuf::from("root"),
                entrypoint: PathBuf::from("root/SKILL.md"),
            },
            trust: SkillTrust::Workspace,
            required_tools: vec![],
            tags: vec![],
        };

        // Activate
        let act_event = ygg_agent::session::EntryValue::SkillActivated {
            descriptor: desc.clone(),
            instructions_hash: "hash".to_string(),
            instructions: "instructions".to_string(),
        };
        let act_id = session.append(act_event).unwrap();

        // Read resource
        let read_event = ygg_agent::session::EntryValue::SkillResourceRead {
            activation_id: act_id.clone(),
            skill_id: "my-skill".to_string(),
            resource_path: "references/ref.md".to_string(),
            start_line: None,
            line_count: None,
            content_hash: "res-hash".to_string(),
            content: "resource content".to_string(),
        };
        session.append(read_event).unwrap();

        // Resolve active skills at head
        let head_id = session.head().unwrap();
        let active_state = session.resolve_active_skills(&head_id).unwrap();
        assert_eq!(active_state.active_skills.len(), 1);
        assert_eq!(active_state.active_skills[0].descriptor.id, "my-skill");
        assert_eq!(active_state.skill_resources.len(), 1);
        assert_eq!(
            active_state.skill_resources[0].resource_path,
            "references/ref.md"
        );

        // Deactivate
        let deact_event = ygg_agent::session::EntryValue::SkillDeactivated {
            activation_id: act_id.clone(),
            skill_id: "my-skill".to_string(),
        };
        let deact_id = session.append(deact_event).unwrap();

        // Resolve active skills after deactivation
        let active_state2 = session.resolve_active_skills(&deact_id).unwrap();
        assert!(active_state2.active_skills.is_empty());
        assert!(active_state2.skill_resources.is_empty());
    }

    #[test]
    fn test_compaction_active_skills_serialization() {
        let temp = tempfile::tempdir().unwrap();
        let session_path = temp.path().join("session.jsonl");
        let mut session = ygg_agent::session::Session::create(session_path).unwrap();

        let desc = SkillDescriptor {
            id: "my-skill".to_string(),
            name: "My Skill".to_string(),
            description: "Desc".to_string(),
            version: None,
            source: SkillSource::FileSystem {
                root: PathBuf::from("root"),
                entrypoint: PathBuf::from("root/SKILL.md"),
            },
            trust: SkillTrust::Workspace,
            required_tools: vec![],
            tags: vec![],
        };

        // Activate
        let act_event = ygg_agent::session::EntryValue::SkillActivated {
            descriptor: desc.clone(),
            instructions_hash: "hash".to_string(),
            instructions: "instructions".to_string(),
        };
        let act_id = session.append(act_event).unwrap();

        // Read resource
        let read_event = ygg_agent::session::EntryValue::SkillResourceRead {
            activation_id: act_id.clone(),
            skill_id: "my-skill".to_string(),
            resource_path: "references/ref.md".to_string(),
            start_line: None,
            line_count: None,
            content_hash: "res-hash".to_string(),
            content: "resource content".to_string(),
        };
        let read_id = session.append(read_event).unwrap();

        // Compact history up to read_id (keeping read_id as first_kept)
        session.compact("summary", read_id.clone()).unwrap();

        // The compaction boundary will be the new head
        let head_id = session.head().unwrap();

        // Resolve active skills at head (after compaction)
        let active_state = session.resolve_active_skills(&head_id).unwrap();
        // Since act_id occurred before first_kept, its activation event has been pruned,
        // but it should still be resolved because it was cached inside the Compaction record!
        assert_eq!(active_state.active_skills.len(), 1);
        assert_eq!(active_state.active_skills[0].descriptor.id, "my-skill");
        assert_eq!(active_state.skill_resources.len(), 1);
        assert_eq!(
            active_state.skill_resources[0].resource_path,
            "references/ref.md"
        );
    }
}
