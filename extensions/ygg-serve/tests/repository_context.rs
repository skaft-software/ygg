#![allow(missing_docs)]

mod project_registry {
    pub use ygg_serve_backend::{
        ProjectRegistry, ProjectRegistryError, ProjectRoot, RegistryProjectId as ProjectId,
    };
}

#[path = "../src/repository_context.rs"]
mod repository_context;

use std::fs;
use std::path::Path;
use std::process::Command;

use repository_context::{
    refresh_repository_context, ContextRefreshState, GitBranchState, GitWorktreeState,
    InstructionLoadErrorCode, RepositoryContextError, RepositoryTrust, MAX_INSTRUCTION_FILE_BYTES,
    MAX_INSTRUCTION_TOTAL_BYTES, MAX_VISIBLE_INSTRUCTION_FILE_BYTES,
    MAX_VISIBLE_INSTRUCTION_TOTAL_BYTES,
};
use tempfile::TempDir;
use ygg_serve_backend::{ProjectRegistry, RegistryProjectId};

fn private_directory(parent: &Path, name: &str) -> std::path::PathBuf {
    let path = parent.join(name);
    fs::create_dir(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

fn trusted_project() -> (TempDir, ProjectRegistry, RegistryProjectId) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let state = private_directory(temporary.path(), "state");
    let mut registry = ProjectRegistry::open(&state).unwrap();
    let project = registry.import(&root, Some("Workspace")).unwrap();
    registry.grant_trust(&project.id).unwrap();
    (temporary, registry, project.id)
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn initialize_git(root: &Path) {
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.name", "Ygg Test"]);
    git(root, &["config", "user.email", "ygg@example.invalid"]);
}

fn commit_all(root: &Path, message: &str) -> String {
    git(root, &["add", "."]);
    git(root, &["commit", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn trust_is_required_before_git_or_instruction_reads() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("AGENTS.md"), "private sentinel").unwrap();
    let state = private_directory(temporary.path(), "state");
    let mut registry = ProjectRegistry::open(&state).unwrap();
    let project = registry.import(&root, None).unwrap();

    assert_eq!(
        refresh_repository_context(&registry, &project.id),
        Err(RepositoryContextError::TrustRequired)
    );
    registry.grant_trust(&project.id).unwrap();
    registry.archive(&project.id).unwrap();
    assert_eq!(
        refresh_repository_context(&registry, &project.id),
        Err(RepositoryContextError::TrustRequired)
    );
}

#[test]
fn replaced_project_root_fails_identity_revalidation() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    fs::remove_dir(&root).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("AGENTS.md"), "replacement sentinel").unwrap();

    assert_eq!(
        refresh_repository_context(&registry, &project_id),
        Err(RepositoryContextError::RootChanged)
    );
}

#[test]
fn non_repository_is_current_and_path_free() {
    let (temporary, registry, project_id) = trusted_project();
    fs::write(
        temporary.path().join("workspace/AGENTS.md"),
        "# Project rules\nStay focused.",
    )
    .unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert_eq!(snapshot.trust, RepositoryTrust::Verified);
    assert_eq!(
        snapshot.repository.refresh.state,
        ContextRefreshState::NotApplicable
    );
    assert_eq!(
        snapshot.repository.worktree,
        GitWorktreeState::NotRepository
    );
    assert_eq!(snapshot.repository.dirty, None);
    assert_eq!(
        snapshot.instructions.refresh.state,
        ContextRefreshState::Current
    );
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains(temporary.path().to_str().unwrap()));
    assert!(!json.contains("/workspace"));
    assert!(json.contains("\"relativePath\":\"AGENTS.md\""));
}

#[test]
fn git_head_branch_dirty_and_divergence_are_reported() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    initialize_git(&root);
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    let base = commit_all(&root, "base");
    fs::write(root.join("tracked.txt"), "remote\n").unwrap();
    let remote = commit_all(&root, "remote");
    git(&root, &["reset", "--hard", &base]);
    fs::write(root.join("local.txt"), "local\n").unwrap();
    let local = commit_all(&root, "local");
    git(&root, &["remote", "add", "origin", "."]);
    git(&root, &["update-ref", "refs/remotes/origin/main", &remote]);
    git(&root, &["branch", "--set-upstream-to=origin/main", "main"]);
    fs::write(root.join("untracked.txt"), "dirty\n").unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert_eq!(
        snapshot.repository.refresh.state,
        ContextRefreshState::Current
    );
    assert_eq!(snapshot.repository.worktree, GitWorktreeState::Present);
    assert_eq!(snapshot.repository.head.as_deref(), Some(local.as_str()));
    assert_eq!(snapshot.repository.branch_state, GitBranchState::Named);
    assert_eq!(snapshot.repository.branch.as_deref(), Some("main"));
    assert_eq!(snapshot.repository.dirty, Some(true));
    assert_eq!(snapshot.repository.ahead, Some(1));
    assert_eq!(snapshot.repository.behind, Some(1));
}

#[test]
fn clean_detached_head_is_explicit() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    initialize_git(&root);
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    let head = commit_all(&root, "base");
    git(&root, &["checkout", "--detach"]);

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert_eq!(snapshot.repository.head.as_deref(), Some(head.as_str()));
    assert_eq!(snapshot.repository.branch_state, GitBranchState::Detached);
    assert_eq!(snapshot.repository.branch, None);
    assert_eq!(snapshot.repository.dirty, Some(false));
    assert_eq!(snapshot.repository.ahead, None);
    assert_eq!(snapshot.repository.behind, None);
}

#[test]
fn unborn_branch_has_no_head() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    initialize_git(&root);

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert_eq!(snapshot.repository.head, None);
    assert_eq!(snapshot.repository.branch_state, GitBranchState::Unborn);
    assert_eq!(snapshot.repository.branch.as_deref(), Some("main"));
    assert_eq!(snapshot.repository.dirty, Some(false));
}

#[test]
fn nested_parent_repository_is_not_crossed() {
    let temporary = tempfile::tempdir().unwrap();
    let parent = temporary.path().join("parent");
    let root = parent.join("nested-project");
    fs::create_dir_all(&root).unwrap();
    initialize_git(&parent);
    fs::write(parent.join("outside.txt"), "outside\n").unwrap();
    commit_all(&parent, "base");
    let state = private_directory(temporary.path(), "state");
    let mut registry = ProjectRegistry::open(&state).unwrap();
    let project = registry.import(&root, None).unwrap();
    registry.grant_trust(&project.id).unwrap();

    let snapshot = refresh_repository_context(&registry, &project.id).unwrap();
    assert_eq!(
        snapshot.repository.worktree,
        GitWorktreeState::NotRepository
    );
    assert_eq!(
        snapshot.repository.refresh.state,
        ContextRefreshState::NotApplicable
    );
}

#[cfg(unix)]
#[test]
fn symlinked_git_metadata_fails_closed() {
    use std::os::unix::fs::symlink;

    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    let outside = temporary.path().join("outside-git");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, root.join(".git")).unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert_eq!(snapshot.repository.worktree, GitWorktreeState::Unknown);
    assert_eq!(
        snapshot.repository.refresh.state,
        ContextRefreshState::Unavailable
    );
}

#[test]
fn instructions_are_root_first_with_safe_origins_and_summaries() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("AGENTS.md"), "\n# Root rules\nroot body").unwrap();
    fs::write(root.join("src/AGENTS.md"), "# Source rules\nsource body").unwrap();
    fs::write(
        root.join("src/nested/AGENTS.md"),
        "# Nested rules\nnested body",
    )
    .unwrap();
    fs::write(root.join("docs/AGENTS.md"), "# Docs rules\ndocs body").unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    let files = &snapshot.instructions.files;
    assert_eq!(
        files
            .iter()
            .map(|file| file.origin.relative_path.as_str())
            .collect::<Vec<_>>(),
        [
            "AGENTS.md",
            "docs/AGENTS.md",
            "src/AGENTS.md",
            "src/nested/AGENTS.md"
        ]
    );
    assert_eq!(
        files
            .iter()
            .map(|file| file.origin.scope.as_str())
            .collect::<Vec<_>>(),
        [".", "docs", "src", "src/nested"]
    );
    assert_eq!(
        files.iter().map(|file| file.precedence).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(files[0].summary, "# Root rules");
    assert_eq!(files[3].summary, "# Nested rules");
    assert!(files.iter().all(|file| file.sha256.len() == 64));
}

#[test]
fn visible_instruction_content_is_bounded_at_utf8_boundaries() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    let content = "é".repeat(MAX_VISIBLE_INSTRUCTION_FILE_BYTES);
    fs::write(root.join("AGENTS.md"), &content).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/AGENTS.md"), &content).unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert_eq!(snapshot.instructions.files.len(), 2);
    assert!(snapshot
        .instructions
        .files
        .iter()
        .all(|file| file.visible_content.len() <= MAX_VISIBLE_INSTRUCTION_FILE_BYTES));
    assert!(
        snapshot
            .instructions
            .files
            .iter()
            .map(|file| file.visible_content.len())
            .sum::<usize>()
            <= MAX_VISIBLE_INSTRUCTION_TOTAL_BYTES
    );
    assert!(snapshot
        .instructions
        .files
        .iter()
        .all(|file| file.content_truncated));
    assert_eq!(
        snapshot.instructions.refresh.state,
        ContextRefreshState::Partial
    );
    assert!(snapshot.instructions.refresh.truncated);
}

#[test]
fn aggregate_instruction_source_limit_is_fail_closed_and_ordered() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    let chunk = vec![b'x'; (MAX_INSTRUCTION_TOTAL_BYTES / 3) as usize];
    fs::write(root.join("AGENTS.md"), &chunk).unwrap();
    for name in ["a", "b", "c"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("AGENTS.md"), &chunk).unwrap();
    }

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert!(snapshot.instructions.loaded_bytes <= MAX_INSTRUCTION_TOTAL_BYTES);
    assert_eq!(
        snapshot.instructions.refresh.state,
        ContextRefreshState::Partial
    );
    assert!(snapshot
        .instructions
        .errors
        .iter()
        .any(|error| { error.code == InstructionLoadErrorCode::AggregateLimitReached }));
    assert_eq!(
        snapshot.instructions.files[0].origin.relative_path,
        "AGENTS.md"
    );
}

#[test]
fn oversized_invalid_utf8_and_binary_instructions_report_safe_errors() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    fs::write(
        root.join("AGENTS.md"),
        vec![b'x'; MAX_INSTRUCTION_FILE_BYTES as usize + 1],
    )
    .unwrap();
    fs::create_dir(root.join("invalid")).unwrap();
    fs::write(root.join("invalid/AGENTS.md"), [0xff, 0xfe]).unwrap();
    fs::create_dir(root.join("binary")).unwrap();
    fs::write(root.join("binary/AGENTS.md"), b"safe\0unsafe").unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert!(snapshot.instructions.files.is_empty());
    let codes = snapshot
        .instructions
        .errors
        .iter()
        .map(|error| error.code)
        .collect::<Vec<_>>();
    assert!(codes.contains(&InstructionLoadErrorCode::FileTooLarge));
    assert!(codes.contains(&InstructionLoadErrorCode::InvalidUtf8));
    assert!(codes.contains(&InstructionLoadErrorCode::BinaryContent));
    let json = serde_json::to_string(&snapshot.instructions.errors).unwrap();
    assert!(!json.contains(temporary.path().to_str().unwrap()));
}

#[cfg(unix)]
#[test]
fn instruction_symlinks_and_hardlinks_are_rejected_without_reading_targets() {
    use std::os::unix::fs::symlink;

    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    let outside = temporary.path().join("outside-secret");
    fs::write(&outside, "outside secret sentinel").unwrap();
    symlink(&outside, root.join("AGENTS.md")).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::hard_link(&outside, root.join("nested/AGENTS.md")).unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert!(snapshot.instructions.files.is_empty());
    assert!(snapshot.instructions.errors.iter().any(|error| {
        error
            .origin
            .as_ref()
            .is_some_and(|origin| origin.relative_path == "AGENTS.md")
            && error.code == InstructionLoadErrorCode::SymlinkRejected
    }));
    assert!(snapshot.instructions.errors.iter().any(|error| {
        error
            .origin
            .as_ref()
            .is_some_and(|origin| origin.relative_path == "nested/AGENTS.md")
            && error.code == InstructionLoadErrorCode::HardLinkRejected
    }));
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("outside secret sentinel"));
}

#[test]
fn ignored_dependency_trees_are_not_scanned() {
    let (temporary, registry, project_id) = trusted_project();
    let root = temporary.path().join("workspace");
    for ignored in [".git", "node_modules", "target", ".next"] {
        fs::create_dir_all(root.join(ignored)).unwrap();
        fs::write(root.join(ignored).join("AGENTS.md"), "ignored sentinel").unwrap();
    }
    fs::write(root.join("AGENTS.md"), "visible").unwrap();

    let snapshot = refresh_repository_context(&registry, &project_id).unwrap();
    assert_eq!(snapshot.instructions.files.len(), 1);
    assert_eq!(
        snapshot.instructions.files[0].origin.relative_path,
        "AGENTS.md"
    );
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("ignored sentinel"));
}
