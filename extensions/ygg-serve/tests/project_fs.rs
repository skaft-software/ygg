//! Standalone integration coverage for root-confined project filesystem operations.

#[allow(dead_code)]
#[path = "../src/fs.rs"]
mod project_fs;
#[allow(dead_code)]
#[path = "../src/project_registry.rs"]
mod project_registry;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(dead_code)]
#[path = "../src/repository_context.rs"]
mod repository_context;

use project_fs::{
    ProjectFileEntryKind, ProjectFileSystem, ProjectFileSystemError, MAX_PROJECT_FILE_READ_BYTES,
};
use project_registry::{ProjectId, ProjectRegistry};
use repository_context::GitFileStatusKind;

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    registry: ProjectRegistry,
    project_id: ProjectId,
}

impl Fixture {
    fn new(populate: impl FnOnce(&Path)) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("project");
        fs::create_dir(&root).unwrap();
        populate(&root);
        let state = temporary.path().join("state");
        let mut registry = ProjectRegistry::open(&state).unwrap();
        let project_id = registry.import(&root, Some("Trusted project")).unwrap().id;
        registry.grant_trust(&project_id).unwrap();
        Self {
            _temporary: temporary,
            root,
            registry,
            project_id,
        }
    }
}

fn git(root: &Path, args: &[&str]) {
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
}

fn initialize_git(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Ygg Test"]);
    git(root, &["config", "user.email", "ygg@example.invalid"]);
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", message]);
}

fn status_kinds(entry: &project_fs::ProjectFileEntry) -> Vec<GitFileStatusKind> {
    entry.git_status.iter().map(|status| status.kind).collect()
}

#[test]
fn tree_exposes_file_statuses_folder_aggregates_and_deleted_paths() {
    let fixture = Fixture::new(|root| {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("removed")).unwrap();
        fs::write(root.join("modified.txt"), "base\n").unwrap();
        fs::write(root.join("deleted.txt"), "base\n").unwrap();
        fs::write(root.join("renamed-before.txt"), "base\n").unwrap();
        fs::write(root.join("src/nested.txt"), "base\n").unwrap();
        fs::write(root.join("removed/lost.txt"), "base\n").unwrap();
    });
    initialize_git(&fixture.root);
    commit_all(&fixture.root, "base");
    fs::write(fixture.root.join("modified.txt"), "changed\n").unwrap();
    fs::write(fixture.root.join("untracked.txt"), "new\n").unwrap();
    fs::create_dir_all(fixture.root.join("untracked-dir/nested")).unwrap();
    fs::write(fixture.root.join("untracked-dir/nested/file.txt"), "new\n").unwrap();
    fs::write(fixture.root.join("src/added.txt"), "new\n").unwrap();
    fs::write(fixture.root.join("src/nested.txt"), "changed\n").unwrap();
    fs::remove_file(fixture.root.join("deleted.txt")).unwrap();
    fs::remove_dir_all(fixture.root.join("removed")).unwrap();
    git(&fixture.root, &["add", "src/added.txt"]);
    git(
        &fixture.root,
        &["mv", "renamed-before.txt", "renamed-after.txt"],
    );

    let tree = ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, "").unwrap();
    assert!(!tree.git_status_truncated);
    let entry = |name: &str| {
        tree.entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
    };
    assert_eq!(
        status_kinds(entry("modified.txt")),
        vec![GitFileStatusKind::Modified]
    );
    assert_eq!(
        status_kinds(entry("deleted.txt")),
        vec![GitFileStatusKind::Deleted]
    );
    assert_eq!(
        status_kinds(entry("renamed-after.txt")),
        vec![GitFileStatusKind::Renamed]
    );
    assert_eq!(
        entry("renamed-after.txt").git_status[0].old_path.as_deref(),
        Some("renamed-before.txt")
    );
    assert_eq!(
        status_kinds(entry("untracked-dir")),
        vec![GitFileStatusKind::Untracked]
    );
    let untracked =
        ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, "untracked-dir").unwrap();
    assert_eq!(untracked.entries[0].name, "nested");
    assert_eq!(
        status_kinds(&untracked.entries[0]),
        vec![GitFileStatusKind::Untracked]
    );
    let nested = ProjectFileSystem::tree(
        &fixture.registry,
        &fixture.project_id,
        "untracked-dir/nested",
    )
    .unwrap();
    assert_eq!(nested.entries[0].name, "file.txt");
    assert_eq!(
        status_kinds(&nested.entries[0]),
        vec![GitFileStatusKind::Untracked]
    );
    assert_eq!(
        status_kinds(entry("untracked.txt")),
        vec![GitFileStatusKind::Untracked]
    );
    assert_eq!(
        status_kinds(entry("src")),
        vec![GitFileStatusKind::Modified, GitFileStatusKind::Added]
    );
    assert_eq!(
        status_kinds(entry("removed")),
        vec![GitFileStatusKind::Deleted]
    );

    let removed =
        ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, "removed").unwrap();
    assert_eq!(removed.entries[0].name, "lost.txt");
    assert_eq!(
        status_kinds(&removed.entries[0]),
        vec![GitFileStatusKind::Deleted]
    );
}

#[test]
fn lists_only_safe_immediate_entries_without_exposing_the_host_root() {
    let fixture = Fixture::new(|root| {
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("docs")).unwrap();
        fs::write(root.join("README.md"), "# Project\n").unwrap();
        fs::write(
            root.join("src").join("lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .unwrap();
    });

    let tree = ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, "").unwrap();
    assert_eq!(tree.path, "");
    assert!(!tree.truncated);
    assert_eq!(
        tree.entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind))
            .collect::<Vec<_>>(),
        vec![
            ("docs", ProjectFileEntryKind::Directory),
            ("src", ProjectFileEntryKind::Directory),
            ("README.md", ProjectFileEntryKind::File),
        ]
    );
    assert!(tree
        .entries
        .iter()
        .all(|entry| entry.modified_at_ms.is_some()));
    let public = serde_json::to_string(&tree).unwrap();
    assert!(!public.contains(fixture.root.to_str().unwrap()));

    let source = ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, "src").unwrap();
    assert_eq!(source.path, "src");
    assert_eq!(source.entries.len(), 1);
    assert_eq!(source.entries[0].name, "lib.rs");
}

#[test]
fn traversal_and_links_are_rejected_before_they_can_escape_the_root() {
    let fixture = Fixture::new(|root| {
        fs::write(root.join("safe.txt"), "safe\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = root.parent().unwrap().join("outside.txt");
            fs::write(&outside, "outside\n").unwrap();
            symlink(&outside, root.join("outside.txt")).unwrap();
            fs::hard_link(&outside, root.join("linked-outside.txt")).unwrap();
        }
    });

    assert_eq!(
        ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, "../"),
        Err(ProjectFileSystemError::InvalidPath)
    );
    assert_eq!(
        ProjectFileSystem::read(
            &fixture.registry,
            &fixture.project_id,
            "safe.txt/../outside.txt",
            None,
            None,
        ),
        Err(ProjectFileSystemError::InvalidPath)
    );
    assert_eq!(
        ProjectFileSystem::read(
            &fixture.registry,
            &fixture.project_id,
            "/etc/passwd",
            None,
            None,
        ),
        Err(ProjectFileSystemError::InvalidPath)
    );

    #[cfg(unix)]
    {
        let tree = ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, "").unwrap();
        assert!(tree.entries.iter().all(|entry| entry.name != "outside.txt"));
        assert!(tree
            .entries
            .iter()
            .all(|entry| entry.name != "linked-outside.txt"));
        assert_eq!(
            ProjectFileSystem::read(
                &fixture.registry,
                &fixture.project_id,
                "outside.txt",
                None,
                None,
            ),
            Err(ProjectFileSystemError::InvalidPath)
        );
        assert_eq!(
            ProjectFileSystem::read(
                &fixture.registry,
                &fixture.project_id,
                "linked-outside.txt",
                None,
                None,
            ),
            Err(ProjectFileSystemError::NotFile)
        );
    }
}

#[test]
fn reads_bounded_text_with_line_ranges_and_a_clear_truncation_state() {
    let fixture = Fixture::new(|root| {
        fs::write(root.join("notes.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        fs::write(
            root.join("large.txt"),
            vec![b'x'; MAX_PROJECT_FILE_READ_BYTES as usize + 1],
        )
        .unwrap();
    });

    let range = ProjectFileSystem::read(
        &fixture.registry,
        &fixture.project_id,
        "notes.txt",
        Some(2),
        Some(3),
    )
    .unwrap();
    assert_eq!(range.content, "two\nthree\n");
    assert_eq!(
        (range.start_line, range.end_line, range.line_count),
        (2, 3, 4)
    );
    assert!(range.truncated);
    assert_eq!(range.sha256, None);

    let complete = ProjectFileSystem::read(
        &fixture.registry,
        &fixture.project_id,
        "notes.txt",
        None,
        None,
    )
    .unwrap();
    assert_eq!(complete.start_line, 1);
    assert_eq!(complete.end_line, 4);
    assert!(!complete.truncated);
    assert_eq!(complete.sha256.as_deref().map(str::len), Some(64));

    let large = ProjectFileSystem::read(
        &fixture.registry,
        &fixture.project_id,
        "large.txt",
        None,
        None,
    )
    .unwrap();
    assert!(large.truncated);
    assert_eq!(large.sha256, None);
    assert!(large.content.len() <= MAX_PROJECT_FILE_READ_BYTES as usize);
    assert_eq!(
        ProjectFileSystem::read(
            &fixture.registry,
            &fixture.project_id,
            "notes.txt",
            Some(3),
            Some(2),
        ),
        Err(ProjectFileSystemError::InvalidRange)
    );
}

#[test]
fn full_text_search_is_project_relative_and_bounded_to_safe_text_files() {
    let fixture = Fixture::new(|root| {
        fs::create_dir(root.join("src")).unwrap();
        fs::write(
            root.join("src").join("lib.rs"),
            "pub fn visible_answer() {}\n",
        )
        .unwrap();
        fs::write(root.join("release-notes.md"), "Ready for the Release\n").unwrap();
        fs::write(root.join("binary.txt"), b"release\0secret").unwrap();
    });

    let result =
        ProjectFileSystem::search(&fixture.registry, &fixture.project_id, "RELEASE").unwrap();
    assert_eq!(
        result
            .hits
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["release-notes.md"]
    );
    assert_eq!(result.hits[0].line, Some(1));
    assert_eq!(result.hits[0].snippet, "Ready for the Release");
    assert!(result.scanned_bytes > 0);
    assert!(result.scanned_bytes < 1_000);
    assert!(!result.truncated);

    let by_path =
        ProjectFileSystem::search(&fixture.registry, &fixture.project_id, "lib.rs").unwrap();
    assert_eq!(by_path.hits.len(), 1);
    assert_eq!(by_path.hits[0].path, "src/lib.rs");
}

#[test]
fn writes_require_the_read_version_and_an_explicit_force_after_conflict() {
    let fixture = Fixture::new(|root| {
        fs::write(root.join("editable.txt"), "before\n").unwrap();
    });
    let original = ProjectFileSystem::read(
        &fixture.registry,
        &fixture.project_id,
        "editable.txt",
        None,
        None,
    )
    .unwrap();
    let original_sha256 = original.sha256.unwrap();

    let saved = ProjectFileSystem::write(
        &fixture.registry,
        &fixture.project_id,
        "editable.txt",
        "after\n",
        &original_sha256,
        false,
    )
    .unwrap();
    assert_eq!(saved.sha256.len(), 64);
    assert_eq!(
        fs::read_to_string(fixture.root.join("editable.txt")).unwrap(),
        "after\n"
    );

    fs::write(fixture.root.join("editable.txt"), "external\n").unwrap();
    assert_eq!(
        ProjectFileSystem::write(
            &fixture.registry,
            &fixture.project_id,
            "editable.txt",
            "mine\n",
            &saved.sha256,
            false,
        ),
        Err(ProjectFileSystemError::Conflict)
    );
    ProjectFileSystem::write(
        &fixture.registry,
        &fixture.project_id,
        "editable.txt",
        "mine\n",
        &saved.sha256,
        true,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.root.join("editable.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn trust_revocation_is_effective_for_existing_project_file_requests() {
    let mut fixture = Fixture::new(|root| {
        fs::write(root.join("safe.txt"), "safe\n").unwrap();
    });
    fixture.registry.revoke_trust(&fixture.project_id).unwrap();

    assert_eq!(
        ProjectFileSystem::tree(&fixture.registry, &fixture.project_id, ""),
        Err(ProjectFileSystemError::TrustRequired)
    );
}
