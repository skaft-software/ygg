#![allow(missing_docs)]

//! Shared discovery and bounded-read policy for user-customizable resources.
//!
//! Resource-specific parsers remain in their owning modules. This module owns
//! the cross-cutting filesystem contract: global/project/explicit precedence,
//! workspace trust, deterministic scans, diagnostics, immutable reload
//! snapshots, and safe size-limited text reads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const MAX_RESOURCE_ENTRIES_PER_ROOT: usize = 4096;

/// A filesystem resource family understood by Ygg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Theme,
    Prompt,
    Skill,
    Extension,
}

impl ResourceKind {
    pub fn directory_name(self) -> &'static str {
        match self {
            Self::Theme => "themes",
            Self::Prompt => "prompts",
            Self::Skill => "skills",
            Self::Extension => "extensions",
        }
    }

    pub fn max_file_bytes(self) -> usize {
        match self {
            Self::Theme => 256 * 1024,
            Self::Prompt => 512 * 1024,
            Self::Skill => 256 * 1024,
            Self::Extension => 256 * 1024,
        }
    }

    fn accepts_file(self, path: &Path) -> bool {
        let extension = path.extension().and_then(|value| value.to_str());
        match self {
            Self::Theme => extension == Some("toml"),
            Self::Prompt => matches!(extension, Some("md" | "toml")),
            Self::Skill | Self::Extension => false,
        }
    }

    fn directory_entrypoint(self) -> Option<&'static str> {
        match self {
            Self::Skill => Some("SKILL.md"),
            Self::Extension => Some("extension.toml"),
            Self::Theme | Self::Prompt => None,
        }
    }
}

/// Where a resource was discovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceScope {
    Global,
    Project,
    Explicit,
}

/// Trust provenance kept visible after discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceTrust {
    User,
    TrustedWorkspace,
    Explicit,
}

/// One selected resource after precedence has been applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedResource {
    pub kind: ResourceKind,
    pub name: String,
    /// File consumed by the resource-specific parser. For directory resources
    /// this is the manifest/entrypoint, not merely the containing directory.
    pub path: PathBuf,
    pub root: PathBuf,
    pub scope: ResourceScope,
    pub trust: ResourceTrust,
}

/// Diagnostic severity. Discovery is intentionally best-effort: one broken
/// tinkerer resource should not prevent the core binary from starting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceDiagnosticLevel {
    Info,
    Warning,
}

/// Inspectable discovery or precedence diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceDiagnostic {
    pub level: ResourceDiagnosticLevel,
    pub kind: ResourceKind,
    pub path: PathBuf,
    pub message: String,
}

/// Immutable result of one discovery/reload pass.
#[derive(Clone, Debug)]
pub struct ResourceSnapshot {
    #[allow(dead_code)]
    pub generation: u64,
    pub kind: ResourceKind,
    resources: Arc<[ResolvedResource]>,
    diagnostics: Arc<[ResourceDiagnostic]>,
}

impl ResourceSnapshot {
    pub fn resources(&self) -> &[ResolvedResource] {
        &self.resources
    }

    pub fn diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.diagnostics
    }

    pub fn get(&self, name: &str) -> Option<&ResolvedResource> {
        self.resources.iter().find(|resource| resource.name == name)
    }
}

/// Resolver shared by themes, prompts, skills, and executable extensions.
pub struct ResourceResolver {
    workspace: PathBuf,
    global_ygg_dir: Option<PathBuf>,
    workspace_trusted: bool,
    next_generation: AtomicU64,
}

impl ResourceResolver {
    pub fn new(workspace: PathBuf, workspace_trusted: bool) -> Self {
        if let Some(global_ygg_dir) = global_ygg_dir_from_home(dirs::home_dir()) {
            return Self::with_global_ygg_dir(workspace, workspace_trusted, global_ygg_dir);
        }
        Self {
            workspace,
            global_ygg_dir: None,
            workspace_trusted,
            next_generation: AtomicU64::new(1),
        }
    }

    /// Injectable constructor used by tests and embedders that deliberately
    /// keep configuration outside the current user's home directory.
    pub fn with_global_ygg_dir(
        workspace: PathBuf,
        workspace_trusted: bool,
        global_ygg_dir: PathBuf,
    ) -> Self {
        Self {
            workspace,
            global_ygg_dir: Some(global_ygg_dir),
            workspace_trusted,
            next_generation: AtomicU64::new(1),
        }
    }

    /// Discover a resource family. Roots are visited from lowest to highest
    /// precedence: global, trusted project, then explicit paths in CLI order.
    /// A later resource with the same name wins and records a diagnostic.
    pub fn discover(&self, kind: ResourceKind, explicit_roots: &[PathBuf]) -> ResourceSnapshot {
        let mut selected = BTreeMap::<String, ResolvedResource>::new();
        let mut diagnostics = Vec::new();

        if let Some(global_ygg_dir) = &self.global_ygg_dir {
            self.scan_root(
                kind,
                &global_ygg_dir.join(kind.directory_name()),
                ResourceScope::Global,
                ResourceTrust::User,
                &mut selected,
                &mut diagnostics,
            );
        } else {
            diagnostics.push(ResourceDiagnostic {
                level: ResourceDiagnosticLevel::Info,
                kind,
                path: PathBuf::from("~/.ygg").join(kind.directory_name()),
                message: "global resources disabled because the user home directory is unavailable"
                    .into(),
            });
        }

        let project_root = self.workspace.join(".ygg").join(kind.directory_name());
        if self.workspace_trusted {
            self.scan_root(
                kind,
                &project_root,
                ResourceScope::Project,
                ResourceTrust::TrustedWorkspace,
                &mut selected,
                &mut diagnostics,
            );
        } else if project_root.exists() {
            diagnostics.push(ResourceDiagnostic {
                level: ResourceDiagnosticLevel::Info,
                kind,
                path: project_root,
                message: "ignored project resources because the workspace is not trusted".into(),
            });
        }

        for root in explicit_roots {
            self.scan_root(
                kind,
                root,
                ResourceScope::Explicit,
                ResourceTrust::Explicit,
                &mut selected,
                &mut diagnostics,
            );
        }

        ResourceSnapshot {
            generation: self.next_generation.fetch_add(1, Ordering::Relaxed),
            kind,
            resources: selected.into_values().collect::<Vec<_>>().into(),
            diagnostics: diagnostics.into(),
        }
    }

    /// Reload is a fresh immutable snapshot. Existing consumers can finish a
    /// prompt/run against the prior generation without observing half-reloads.
    #[allow(dead_code)]
    pub fn reload(&self, kind: ResourceKind, explicit_roots: &[PathBuf]) -> ResourceSnapshot {
        self.discover(kind, explicit_roots)
    }

    /// Safely read a selected parser entrypoint with the resource family's
    /// documented bound. Symlinks and non-regular files are rejected by the
    /// shared secure filesystem boundary.
    pub fn read_text(&self, resource: &ResolvedResource) -> anyhow::Result<String> {
        let parent = resource
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("resource {} has no parent", resource.path.display()))?;
        let name = resource.path.file_name().ok_or_else(|| {
            anyhow::anyhow!("resource {} has no file name", resource.path.display())
        })?;
        // Secure descriptor traversal requires a symlink-free absolute path.
        // Canonicalize only the parent so the final resource itself is still
        // opened with no-follow semantics and cannot be swapped for a symlink.
        let opened_path = parent.canonicalize()?.join(name);
        let bytes = ygg_agent::secure_fs::read_regular_file_bounded(
            &opened_path,
            resource.kind.max_file_bytes(),
        )?;
        String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("resource {} is not valid UTF-8", resource.path.display()))
    }

    fn scan_root(
        &self,
        kind: ResourceKind,
        root: &Path,
        scope: ResourceScope,
        trust: ResourceTrust,
        selected: &mut BTreeMap<String, ResolvedResource>,
        diagnostics: &mut Vec<ResourceDiagnostic>,
    ) {
        let metadata = match std::fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: format!("cannot inspect resource root: {error}"),
                });
                return;
            }
        };
        if metadata.file_type().is_symlink() {
            diagnostics.push(ResourceDiagnostic {
                level: ResourceDiagnosticLevel::Warning,
                kind,
                path: root.to_owned(),
                message: "resource root must not be a symlink".into(),
            });
            return;
        }

        // Pi-compatible prompt configuration accepts either a directory or a
        // single prompt file. Keep the same precedence, trust, validation,
        // bounded-read, and no-follow contract for that explicit file instead
        // of routing it through a separate loader.
        if metadata.is_file() {
            if !kind.accepts_file(root) {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: format!(
                        "{} resources require a directory{}",
                        kind.directory_name(),
                        match kind {
                            ResourceKind::Theme => " or a .toml file",
                            ResourceKind::Prompt => " or a .md/.toml file",
                            ResourceKind::Skill | ResourceKind::Extension => "",
                        }
                    ),
                });
                return;
            }
            let Some(parent) = root.parent() else {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: "resource file has no parent directory".into(),
                });
                return;
            };
            let Some(file_name) = root.file_name() else {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: "resource file has no file name".into(),
                });
                return;
            };
            let canonical_parent = match parent.canonicalize() {
                Ok(parent) => parent,
                Err(error) => {
                    diagnostics.push(ResourceDiagnostic {
                        level: ResourceDiagnosticLevel::Warning,
                        kind,
                        path: root.to_owned(),
                        message: format!("cannot canonicalize resource parent: {error}"),
                    });
                    return;
                }
            };
            let parser_path = canonical_parent.join(file_name);
            let Some(name) = parser_path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
            else {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: "resource file name is not valid UTF-8".into(),
                });
                return;
            };
            insert_resource(
                kind,
                name,
                parser_path,
                canonical_parent,
                scope,
                trust,
                selected,
                diagnostics,
            );
            return;
        }

        if !metadata.is_dir() {
            diagnostics.push(ResourceDiagnostic {
                level: ResourceDiagnosticLevel::Warning,
                kind,
                path: root.to_owned(),
                message: "resource root must be a regular file or directory".into(),
            });
            return;
        }

        // Normalize accepted roots after rejecting a symlink at the resource
        // boundary. This keeps secure descriptor traversal working on macOS,
        // where temporary directories are commonly reached through `/var`,
        // while still refusing a root that is itself a symlink.
        let canonical_root = match root.canonicalize() {
            Ok(root) => root,
            Err(error) => {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: format!("cannot canonicalize resource root: {error}"),
                });
                return;
            }
        };

        let entries = match std::fs::read_dir(&canonical_root) {
            Ok(entries) => entries,
            Err(error) => {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: format!("cannot scan resource root: {error}"),
                });
                return;
            }
        };
        let mut paths = Vec::with_capacity(MAX_RESOURCE_ENTRIES_PER_ROOT.min(256));
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(error) => {
                    diagnostics.push(ResourceDiagnostic {
                        level: ResourceDiagnosticLevel::Warning,
                        kind,
                        path: root.to_owned(),
                        message: format!("cannot inspect directory entry: {error}"),
                    });
                    continue;
                }
            };
            if paths.len() == MAX_RESOURCE_ENTRIES_PER_ROOT {
                diagnostics.push(ResourceDiagnostic {
                    level: ResourceDiagnosticLevel::Warning,
                    kind,
                    path: root.to_owned(),
                    message: format!(
                        "resource root exceeds the {MAX_RESOURCE_ENTRIES_PER_ROOT}-entry scan limit; ignored"
                    ),
                });
                return;
            }
            paths.push(path);
        }
        paths.sort();

        for candidate in paths {
            let (name, parser_path) = match resource_candidate(kind, &candidate) {
                Ok(Some(candidate)) => candidate,
                Ok(None) => continue,
                Err(message) => {
                    diagnostics.push(ResourceDiagnostic {
                        level: ResourceDiagnosticLevel::Warning,
                        kind,
                        path: candidate,
                        message,
                    });
                    continue;
                }
            };
            insert_resource(
                kind,
                name,
                parser_path,
                canonical_root.clone(),
                scope,
                trust,
                selected,
                diagnostics,
            );
        }
    }
}

fn global_ygg_dir_from_home(home: Option<PathBuf>) -> Option<PathBuf> {
    home.filter(|home| home.is_absolute())
        .map(|home| home.join(".ygg"))
}

#[allow(clippy::too_many_arguments)]
fn insert_resource(
    kind: ResourceKind,
    name: String,
    parser_path: PathBuf,
    root: PathBuf,
    scope: ResourceScope,
    trust: ResourceTrust,
    selected: &mut BTreeMap<String, ResolvedResource>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    if !valid_resource_name(&name) {
        diagnostics.push(ResourceDiagnostic {
            level: ResourceDiagnosticLevel::Warning,
            kind,
            path: parser_path,
            message: "resource name must use letters, digits, '.', '-' or '_'".into(),
        });
        return;
    }
    let resource = ResolvedResource {
        kind,
        name: name.clone(),
        path: parser_path,
        root,
        scope,
        trust,
    };
    if let Some(shadowed) = selected.insert(name.clone(), resource) {
        diagnostics.push(ResourceDiagnostic {
            level: ResourceDiagnosticLevel::Info,
            kind,
            path: shadowed.path,
            message: format!("resource {name:?} was shadowed by a higher-precedence definition"),
        });
    }
}

fn resource_candidate(
    kind: ResourceKind,
    path: &Path,
) -> Result<Option<(String, PathBuf)>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect resource candidate: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("resource candidate must not be a symlink".into());
    }
    if kind.accepts_file(path) && metadata.is_file() {
        let name = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "resource file name is not valid UTF-8".to_owned())?
            .to_owned();
        return Ok(Some((name, path.to_owned())));
    }
    let Some(entrypoint) = kind.directory_entrypoint() else {
        return Ok(None);
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "resource directory name is not valid UTF-8".to_owned())?
        .to_owned();
    let entrypoint = path.join(entrypoint);
    let entry_metadata = std::fs::symlink_metadata(&entrypoint)
        .map_err(|error| format!("cannot inspect resource entrypoint: {error}"))?;
    if entry_metadata.file_type().is_symlink() {
        return Err("resource entrypoint must not be a symlink".into());
    }
    if !entry_metadata.is_file() {
        return Err("resource entrypoint must be a regular file".into());
    }
    Ok(Some((name, entrypoint)))
}

fn valid_resource_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(root: &tempfile::TempDir, trusted: bool) -> (ResourceResolver, PathBuf, PathBuf) {
        let workspace = root.path().join("workspace");
        let global = root.path().join("global/.ygg");
        std::fs::create_dir_all(&workspace).unwrap();
        (
            ResourceResolver::with_global_ygg_dir(workspace.clone(), trusted, global.clone()),
            workspace,
            global,
        )
    }

    #[test]
    fn unavailable_home_never_reclassifies_project_files_as_global() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let project = workspace.join(".ygg/extensions/local-tool");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("extension.toml"), "name='local-tool'").unwrap();
        let resolver = ResourceResolver {
            workspace,
            global_ygg_dir: None,
            workspace_trusted: false,
            next_generation: AtomicU64::new(1),
        };

        let snapshot = resolver.discover(ResourceKind::Extension, &[]);

        assert!(snapshot.resources().is_empty());
        assert!(snapshot.diagnostics().iter().any(|diagnostic| diagnostic
            .message
            .contains("user home directory is unavailable")));
        assert!(snapshot
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("workspace is not trusted")));
    }

    #[test]
    fn relative_home_is_not_a_global_resource_root() {
        assert_eq!(global_ygg_dir_from_home(None), None);
        assert_eq!(global_ygg_dir_from_home(Some(".".into())), None);
        let absolute_home = std::env::temp_dir().join("ygg-home");
        assert_eq!(
            global_ygg_dir_from_home(Some(absolute_home.clone())),
            Some(absolute_home.join(".ygg"))
        );
    }

    #[test]
    fn precedence_is_global_then_trusted_project_then_explicit() {
        let root = tempfile::tempdir().unwrap();
        let (resolver, workspace, global) = resolver(&root, true);
        let project = workspace.join(".ygg/themes");
        let explicit = root.path().join("explicit-themes");
        for directory in [global.join("themes"), project.clone(), explicit.clone()] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(global.join("themes/shared.toml"), "accent='global'").unwrap();
        std::fs::write(project.join("shared.toml"), "accent='project'").unwrap();
        std::fs::write(explicit.join("shared.toml"), "accent='explicit'").unwrap();

        let snapshot = resolver.discover(ResourceKind::Theme, std::slice::from_ref(&explicit));
        let shared = snapshot.get("shared").unwrap();
        assert_eq!(
            shared.path,
            explicit.canonicalize().unwrap().join("shared.toml")
        );
        assert_eq!(shared.scope, ResourceScope::Explicit);
        assert_eq!(snapshot.diagnostics().len(), 2);
    }

    #[test]
    fn untrusted_project_is_absent_and_diagnosed() {
        let root = tempfile::tempdir().unwrap();
        let (resolver, workspace, global) = resolver(&root, false);
        std::fs::create_dir_all(global.join("prompts")).unwrap();
        std::fs::create_dir_all(workspace.join(".ygg/prompts")).unwrap();
        std::fs::write(global.join("prompts/review.md"), "global").unwrap();
        std::fs::write(workspace.join(".ygg/prompts/review.md"), "project").unwrap();

        let snapshot = resolver.discover(ResourceKind::Prompt, &[]);
        assert_eq!(
            resolver.read_text(snapshot.get("review").unwrap()).unwrap(),
            "global"
        );
        assert!(snapshot
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("workspace is not trusted")));
    }

    #[test]
    fn directory_resources_resolve_their_parser_entrypoints() {
        let root = tempfile::tempdir().unwrap();
        let (resolver, _, global) = resolver(&root, true);
        let skill = global.join("skills/refactor");
        let extension = global.join("extensions/git-tools");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::create_dir_all(&extension).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: refactor\n---\n").unwrap();
        std::fs::write(extension.join("extension.toml"), "name='git-tools'").unwrap();

        assert_eq!(
            resolver
                .discover(ResourceKind::Skill, &[])
                .get("refactor")
                .unwrap()
                .path,
            skill.canonicalize().unwrap().join("SKILL.md")
        );
        assert_eq!(
            resolver
                .discover(ResourceKind::Extension, &[])
                .get("git-tools")
                .unwrap()
                .path,
            extension.canonicalize().unwrap().join("extension.toml")
        );
    }

    #[test]
    fn reload_generations_are_monotonic_and_snapshots_are_immutable() {
        let root = tempfile::tempdir().unwrap();
        let (resolver, _, global) = resolver(&root, true);
        std::fs::create_dir_all(global.join("themes")).unwrap();
        std::fs::write(global.join("themes/one.toml"), "accent='red'").unwrap();
        let first = resolver.discover(ResourceKind::Theme, &[]);
        std::fs::write(global.join("themes/two.toml"), "accent='blue'").unwrap();
        let second = resolver.reload(ResourceKind::Theme, &[]);

        assert!(second.generation > first.generation);
        assert_eq!(first.resources().len(), 1);
        assert_eq!(second.resources().len(), 2);
    }

    #[test]
    fn explicit_prompt_file_uses_normal_precedence_and_secure_reads() {
        let root = tempfile::tempdir().unwrap();
        let (resolver, _, global) = resolver(&root, true);
        std::fs::create_dir_all(global.join("prompts")).unwrap();
        std::fs::write(global.join("prompts/review.md"), "global").unwrap();
        let explicit = root.path().join("review.md");
        std::fs::write(&explicit, "explicit").unwrap();

        let snapshot = resolver.discover(ResourceKind::Prompt, std::slice::from_ref(&explicit));
        let review = snapshot.get("review").expect("explicit prompt file");
        assert_eq!(review.scope, ResourceScope::Explicit);
        assert_eq!(review.trust, ResourceTrust::Explicit);
        assert_eq!(resolver.read_text(review).unwrap(), "explicit");
        assert!(snapshot.diagnostics().iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("shadowed by a higher-precedence definition")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_roots_candidates_and_entrypoints_are_diagnosed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let (resolver, _, global) = resolver(&root, true);
        let prompts = global.join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        let target = root.path().join("target.md");
        std::fs::write(&target, "target").unwrap();
        let linked_candidate = prompts.join("linked.md");
        symlink(&target, &linked_candidate).unwrap();

        let linked_root = root.path().join("explicit.md");
        symlink(&target, &linked_root).unwrap();
        let explicit = resolver.discover(ResourceKind::Prompt, &[linked_root]);
        assert!(explicit
            .diagnostics()
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("root must not be a symlink") }));

        let discovered = resolver.discover(ResourceKind::Prompt, &[]);
        assert!(discovered.diagnostics().iter().any(|diagnostic| {
            diagnostic.path.ends_with("linked.md")
                && diagnostic
                    .message
                    .contains("candidate must not be a symlink")
        }));

        let skill = global.join("skills/broken");
        std::fs::create_dir_all(&skill).unwrap();
        symlink(&target, skill.join("SKILL.md")).unwrap();
        let skills = resolver.discover(ResourceKind::Skill, &[]);
        assert!(skills.diagnostics().iter().any(|diagnostic| {
            diagnostic.path.ends_with("broken")
                && diagnostic
                    .message
                    .contains("entrypoint must not be a symlink")
        }));
    }

    #[test]
    fn bounded_reads_reject_oversized_resources() {
        let root = tempfile::tempdir().unwrap();
        let (resolver, _, global) = resolver(&root, true);
        std::fs::create_dir_all(global.join("themes")).unwrap();
        std::fs::write(
            global.join("themes/huge.toml"),
            vec![b'x'; ResourceKind::Theme.max_file_bytes() + 1],
        )
        .unwrap();
        let snapshot = resolver.discover(ResourceKind::Theme, &[]);
        let error = resolver
            .read_text(snapshot.get("huge").unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("too large"), "{error:#}");
    }

    #[test]
    fn oversized_resource_roots_are_rejected_without_partial_discovery() {
        let root = tempfile::tempdir().unwrap();
        let (resolver, _, global) = resolver(&root, true);
        let prompts = global.join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        for index in 0..=MAX_RESOURCE_ENTRIES_PER_ROOT {
            std::fs::File::create(prompts.join(format!("prompt-{index:04}.md"))).unwrap();
        }

        let snapshot = resolver.discover(ResourceKind::Prompt, &[]);

        assert!(snapshot.resources().is_empty());
        assert!(snapshot
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("entry scan limit")));
    }
}
