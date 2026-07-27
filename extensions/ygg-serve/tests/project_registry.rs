//! Standalone integration coverage for the isolated project registry module.

#[path = "../src/project_registry.rs"]
mod project_registry;

use std::fs;
use std::path::Path;

use project_registry::{
    ProjectRegistry, ProjectRegistryError, ProjectState, MAX_REGISTRY_STATE_BYTES,
};

fn state_file(state_directory: &Path) -> std::path::PathBuf {
    state_directory.join("projects.json")
}

#[test]
fn lifecycle_and_trust_decisions_survive_restart_without_public_paths() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let root = fixture.path().join("workspace-secret-name");
    fs::create_dir(&root).unwrap();

    let id = {
        let mut registry = ProjectRegistry::open(&state_directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(
                fs::metadata(&state_directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(state_file(&state_directory))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let imported = registry.import(&root, Some("Visible project")).unwrap();
        assert_eq!(imported.state, ProjectState::Untrusted);
        assert!(imported.available);
        assert!(!imported.is_default);
        assert!(imported.id.as_str().starts_with("prj_"));
        assert!(!imported.id.as_str().contains("workspace"));
        assert_eq!(
            registry.find_by_root(&root).unwrap().unwrap().id,
            imported.id
        );

        let public_json = serde_json::to_string(&imported).unwrap();
        assert!(!public_json.contains(root.to_str().unwrap()));
        assert!(!public_json.contains("canonicalRoot"));
        assert!(!public_json.contains("workspace-secret-name"));
        assert_eq!(registry.list(), vec![imported.clone()]);

        let id = imported.id;
        let renamed = registry
            .update_display_name(&id, "Renamed project")
            .unwrap();
        assert_eq!(renamed.display_name, "Renamed project");
        assert_eq!(
            registry.grant_trust(&id).unwrap().state,
            ProjectState::Trusted
        );
        assert!(registry.set_default(&id).unwrap().is_default);
        assert_eq!(
            registry.resolve_root(&id).unwrap().as_path(),
            root.canonicalize().unwrap()
        );
        id
    };

    let mut reopened = ProjectRegistry::open(&state_directory).unwrap();
    let restored = reopened.get(&id).unwrap();
    assert_eq!(restored.display_name, "Renamed project");
    assert_eq!(restored.state, ProjectState::Trusted);
    assert!(restored.is_default);
    assert_eq!(reopened.default_project().unwrap().id, id);

    assert_eq!(
        reopened.revoke_trust(&id).unwrap().state,
        ProjectState::Untrusted
    );
    let archived = reopened.archive(&id).unwrap();
    assert_eq!(archived.state, ProjectState::Archived);
    assert!(!archived.is_default);
    assert!(matches!(
        reopened.resolve_root(&id),
        Err(ProjectRegistryError::ProjectArchived)
    ));
    drop(reopened);

    let reopened = ProjectRegistry::open(&state_directory).unwrap();
    assert_eq!(reopened.get(&id).unwrap().state, ProjectState::Archived);
    assert!(reopened.default_project().is_none());
}

#[test]
fn durable_session_bindings_are_atomic_private_and_survive_restart() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let first_root = fixture.path().join("first-private-root");
    let second_root = fixture.path().join("second-private-root");
    fs::create_dir(&first_root).unwrap();
    fs::create_dir(&second_root).unwrap();

    let (first_id, second_id) = {
        let mut registry = ProjectRegistry::open(&state_directory).unwrap();
        let first = registry
            .import_with_sessions(&first_root, Some("First"), ["session-one", "session-two"])
            .unwrap();
        let second = registry.import(&second_root, Some("Second")).unwrap();
        assert_eq!(
            registry.project_for_session("session-one"),
            Some(first.id.clone())
        );
        assert_eq!(
            registry.sessions_for_project(&first.id),
            vec!["session-one".to_owned(), "session-two".to_owned()]
        );
        assert!(matches!(
            registry.bind_session("session-one", &second.id),
            Err(ProjectRegistryError::SessionAlreadyBound)
        ));
        assert_eq!(
            registry.project_for_session("session-one"),
            Some(first.id.clone()),
            "a rejected collision must not partially mutate memory"
        );
        registry.bind_session("session-three", &second.id).unwrap();
        (first.id, second.id)
    };

    let registry = ProjectRegistry::open(&state_directory).unwrap();
    assert_eq!(registry.project_for_session("session-two"), Some(first_id));
    assert_eq!(
        registry.project_for_session("session-three"),
        Some(second_id)
    );
    let public_json = serde_json::to_string(&registry.list()).unwrap();
    assert!(!public_json.contains("session-one"));
    assert!(!public_json.contains("first-private-root"));
}

#[test]
fn trusted_resolution_is_a_distinct_execution_boundary() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let root = fixture.path().join("project");
    fs::create_dir(&root).unwrap();
    let mut registry = ProjectRegistry::open(&state_directory).unwrap();
    let project = registry.import(&root, None).unwrap();

    assert!(registry.resolve_root(&project.id).is_ok());
    assert!(matches!(
        registry.resolve_trusted_root(&project.id),
        Err(ProjectRegistryError::ProjectUntrusted)
    ));
    registry.grant_trust(&project.id).unwrap();
    assert_eq!(
        registry
            .resolve_trusted_root(&project.id)
            .unwrap()
            .as_path(),
        root.canonicalize().unwrap()
    );
    registry.revoke_trust(&project.id).unwrap();
    assert!(matches!(
        registry.resolve_trusted_root(&project.id),
        Err(ProjectRegistryError::ProjectUntrusted)
    ));
}

#[test]
fn binding_validation_rejects_invalid_or_unknown_session_authority() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let root = fixture.path().join("project");
    fs::create_dir(&root).unwrap();
    let mut registry = ProjectRegistry::open(&state_directory).unwrap();
    let project = registry.import(&root, None).unwrap();

    for invalid in ["", "../escape", "slash/name", &"x".repeat(129)] {
        assert!(matches!(
            registry.bind_session(invalid, &project.id),
            Err(ProjectRegistryError::InvalidSessionId)
        ));
    }
    let missing =
        project_registry::ProjectId::parse("prj_11111111111111111111111111111111").unwrap();
    assert!(matches!(
        registry.bind_session("session", &missing),
        Err(ProjectRegistryError::ProjectNotFound)
    ));
}

#[test]
fn rejects_duplicate_file_relative_traversal_and_symlink_roots() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let root = fixture.path().join("project");
    fs::create_dir(&root).unwrap();
    let file = fixture.path().join("not-a-directory");
    fs::write(&file, b"x").unwrap();
    let mut registry = ProjectRegistry::open(&state_directory).unwrap();
    registry.import(&root, None).unwrap();

    assert!(matches!(
        registry.import(root.join("."), None),
        Err(ProjectRegistryError::DuplicateRoot)
    ));
    assert!(matches!(
        registry.import(Path::new("relative/project"), None),
        Err(ProjectRegistryError::RelativePath)
    ));
    assert!(matches!(
        registry.import(root.join("child").join(".."), None),
        Err(ProjectRegistryError::PathTraversal)
    ));
    assert!(matches!(
        registry.import(&file, None),
        Err(ProjectRegistryError::RootNotDirectory)
    ));
    assert!(matches!(
        registry.import(fixture.path().join("missing"), None),
        Err(ProjectRegistryError::RootUnavailable)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let root_link = fixture.path().join("project-link");
        symlink(&root, &root_link).unwrap();
        assert!(matches!(
            registry.import(&root_link, None),
            Err(ProjectRegistryError::RootSymlink)
        ));

        let parent_link = fixture.path().join("parent-link");
        symlink(fixture.path(), &parent_link).unwrap();
        assert!(matches!(
            registry.import(parent_link.join("project"), None),
            Err(ProjectRegistryError::DuplicateRoot)
        ));
    }
}

#[test]
fn private_state_and_project_roots_cannot_overlap() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    fs::create_dir(&project).unwrap();
    let state_directory = project.join("private-state");
    let mut registry = ProjectRegistry::open(&state_directory).unwrap();

    assert!(matches!(
        registry.import(&project, None),
        Err(ProjectRegistryError::RootOverlapsState)
    ));
    let nested = state_directory.join("nested-project");
    fs::create_dir(&nested).unwrap();
    assert!(matches!(
        registry.import(&nested, None),
        Err(ProjectRegistryError::RootOverlapsState)
    ));
}

#[test]
fn replaced_directory_identity_never_inherits_existing_trust() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let root = fixture.path().join("project");
    fs::create_dir(&root).unwrap();
    let mut registry = ProjectRegistry::open(&state_directory).unwrap();
    let id = registry.import(&root, None).unwrap().id;
    registry.grant_trust(&id).unwrap();

    fs::remove_dir(&root).unwrap();
    fs::create_dir(&root).unwrap();

    let summary = registry.get(&id).unwrap();
    assert_eq!(summary.state, ProjectState::Trusted);
    assert!(!summary.available);
    assert!(matches!(
        registry.resolve_root(&id),
        Err(ProjectRegistryError::RootIdentityChanged)
    ));
    registry.revoke_trust(&id).unwrap();
    assert!(matches!(
        registry.grant_trust(&id),
        Err(ProjectRegistryError::RootIdentityChanged)
    ));
}

#[test]
fn invalid_names_and_archived_selection_are_rejected() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let root = fixture.path().join("project");
    fs::create_dir(&root).unwrap();
    let mut registry = ProjectRegistry::open(&state_directory).unwrap();

    assert!(matches!(
        registry.import(&root, Some("../secret")),
        Err(ProjectRegistryError::InvalidDisplayName)
    ));
    let id = registry.import(&root, Some("  Project  ")).unwrap().id;
    assert_eq!(registry.get(&id).unwrap().display_name, "Project");
    registry.set_default(&id).unwrap();
    registry.grant_trust(&id).unwrap();
    registry.archive(&id).unwrap();

    assert!(matches!(
        registry.set_default(&id),
        Err(ProjectRegistryError::ProjectArchived)
    ));
    assert!(matches!(
        registry.grant_trust(&id),
        Err(ProjectRegistryError::ProjectArchived)
    ));
    registry.clear_default().unwrap();
}

#[test]
fn corrupt_oversized_duplicate_and_unknown_state_are_rejected() {
    let fixture = tempfile::tempdir().unwrap();

    let corrupt_directory = fixture.path().join("corrupt");
    ProjectRegistry::open(&corrupt_directory).unwrap();
    fs::write(state_file(&corrupt_directory), b"{not-json").unwrap();
    assert!(matches!(
        ProjectRegistry::open(&corrupt_directory),
        Err(ProjectRegistryError::CorruptState)
    ));

    let oversized_directory = fixture.path().join("oversized");
    ProjectRegistry::open(&oversized_directory).unwrap();
    fs::write(
        state_file(&oversized_directory),
        vec![b'x'; MAX_REGISTRY_STATE_BYTES as usize + 1],
    )
    .unwrap();
    assert!(matches!(
        ProjectRegistry::open(&oversized_directory),
        Err(ProjectRegistryError::StateTooLarge)
    ));

    let unknown_directory = fixture.path().join("unknown");
    ProjectRegistry::open(&unknown_directory).unwrap();
    let path = state_file(&unknown_directory);
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("unexpected".into(), true.into());
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(matches!(
        ProjectRegistry::open(&unknown_directory),
        Err(ProjectRegistryError::CorruptState)
    ));

    let duplicate_directory = fixture.path().join("duplicate");
    let root = fixture.path().join("duplicate-root");
    fs::create_dir(&root).unwrap();
    let mut registry = ProjectRegistry::open(&duplicate_directory).unwrap();
    registry.import(&root, None).unwrap();
    drop(registry);
    let path = state_file(&duplicate_directory);
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let projects = json["projects"].as_array_mut().unwrap();
    let mut duplicate = projects[0].clone();
    duplicate["id"] = serde_json::Value::String("prj_11111111111111111111111111111111".into());
    projects.push(duplicate);
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(matches!(
        ProjectRegistry::open(&duplicate_directory),
        Err(ProjectRegistryError::CorruptState)
    ));

    let overlap_directory = fixture.path().join("overlap");
    let overlap_root = fixture.path().join("overlap-root");
    fs::create_dir(&overlap_root).unwrap();
    let mut registry = ProjectRegistry::open(&overlap_directory).unwrap();
    registry.import(&overlap_root, None).unwrap();
    drop(registry);
    let path = state_file(&overlap_directory);
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json["projects"][0]["canonicalRoot"] =
        serde_json::Value::String(overlap_directory.to_str().unwrap().into());
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(matches!(
        ProjectRegistry::open(&overlap_directory),
        Err(ProjectRegistryError::CorruptState)
    ));
}

#[cfg(unix)]
#[test]
fn rejects_unsafe_directory_file_and_symlink_state() {
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    let fixture = tempfile::tempdir().unwrap();

    let unsafe_directory = fixture.path().join("unsafe-directory");
    ProjectRegistry::open(&unsafe_directory).unwrap();
    fs::set_permissions(&unsafe_directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        ProjectRegistry::open(&unsafe_directory),
        Err(ProjectRegistryError::UnsafePermissions)
    ));

    let unsafe_file_directory = fixture.path().join("unsafe-file");
    ProjectRegistry::open(&unsafe_file_directory).unwrap();
    let unsafe_file = state_file(&unsafe_file_directory);
    fs::set_permissions(&unsafe_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        ProjectRegistry::open(&unsafe_file_directory),
        Err(ProjectRegistryError::UnsafePermissions)
    ));

    let symlink_directory = fixture.path().join("symlink-state");
    fs::create_dir(&symlink_directory).unwrap();
    fs::set_permissions(&symlink_directory, fs::Permissions::from_mode(0o700)).unwrap();
    let elsewhere = fixture.path().join("elsewhere.json");
    fs::write(&elsewhere, b"{}").unwrap();
    symlink(&elsewhere, state_file(&symlink_directory)).unwrap();
    assert!(matches!(
        ProjectRegistry::open(&symlink_directory),
        Err(ProjectRegistryError::UnsafeStatePath)
    ));
}

#[cfg(unix)]
#[test]
fn failed_persist_keeps_memory_and_restart_state_unchanged() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    let root = fixture.path().join("project");
    fs::create_dir(&root).unwrap();
    let mut registry = ProjectRegistry::open(&state_directory).unwrap();
    let id = registry.import(&root, Some("Before")).unwrap().id;

    fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o500)).unwrap();
    let result = registry.update_display_name(&id, "After");
    assert!(result.is_err(), "read-only state unexpectedly mutated");
    assert_eq!(registry.get(&id).unwrap().display_name, "Before");

    fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o700)).unwrap();
    drop(registry);
    let reopened = ProjectRegistry::open(&state_directory).unwrap();
    assert_eq!(reopened.get(&id).unwrap().display_name, "Before");
}

#[test]
fn stale_atomic_temporary_files_are_removed_on_restart() {
    let fixture = tempfile::tempdir().unwrap();
    let state_directory = fixture.path().join("state");
    ProjectRegistry::open(&state_directory).unwrap();
    let stale = state_directory.join(".projects.tmp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    fs::write(&stale, b"partial").unwrap();
    assert!(stale.exists());

    ProjectRegistry::open(&state_directory).unwrap();
    assert!(!stale.exists());
}
