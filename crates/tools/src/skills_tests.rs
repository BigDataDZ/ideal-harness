use super::*;

fn workspace(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-skills-{}-{name}", std::process::id()))
}

fn write_skill(root: &Path, directory: &str, name: &str, description: &str, body: &str) {
    let directory = root.join(".harness/skills").join(directory);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

#[test]
fn add_modify_delete_refreshes_deterministic_catalog() {
    let root = workspace("refresh");
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    write_skill(&root, "z-dir", "zeta", "Z skill", "first");
    write_skill(&root, "a-dir", "alpha", "A skill", "read only");
    let mut catalog = SkillCatalog::discover(&root).unwrap();
    assert_eq!(
        catalog
            .skills()
            .map(VerifiedSkill::name)
            .collect::<Vec<_>>(),
        vec!["alpha", "zeta"]
    );
    let alpha_fingerprint = catalog.get("alpha").unwrap().fingerprint();

    write_skill(&root, "a-dir", "alpha", "A skill", "changed");
    write_skill(&root, "b-dir", "beta", "B skill", "new");
    fs::remove_dir_all(root.join(".harness/skills/z-dir")).unwrap();
    let changes = catalog.refresh().unwrap();
    assert_eq!(changes.added, ["beta"]);
    assert_eq!(changes.modified, ["alpha"]);
    assert_eq!(changes.removed, ["zeta"]);
    assert_ne!(
        catalog.get("alpha").unwrap().fingerprint(),
        alpha_fingerprint
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn traversal_unknown_yaml_and_duplicate_names_fail_closed() {
    for (case, name, extra) in [
        ("traversal", "../escape", ""),
        ("unknown", "safe", "owner: attacker\n"),
    ] {
        let root = workspace(case);
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(root.join(".harness/skills/a")).unwrap();
        fs::write(
            root.join(".harness/skills/a/SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n{extra}---\nbody\n"),
        )
        .unwrap();
        let error = SkillCatalog::discover(&root).unwrap_err();
        assert!(matches!(
            error.code,
            ErrorCode::SandboxDenied | ErrorCode::ToolArgsInvalid
        ));
        fs::remove_dir_all(root).ok();
    }

    let root = workspace("duplicate");
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    write_skill(&root, "one", "same", "one", "body");
    write_skill(&root, "two", "same", "two", "body");
    assert_eq!(
        SkillCatalog::discover(&root).unwrap_err().code,
        ErrorCode::ToolArgsInvalid
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn child_scope_is_subset_of_current_parent_verification() {
    let root = workspace("scope");
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    write_skill(&root, "read", "read", "read", "body");
    write_skill(&root, "review", "review", "review", "body");
    let mut catalog = SkillCatalog::discover(&root).unwrap();
    let parent = catalog.verified_scope(["read", "review"]).unwrap();
    let child = catalog.inherit_scope(&parent, ["read"]).unwrap();
    assert_eq!(child.names().collect::<Vec<_>>(), vec!["read"]);
    assert_eq!(
        catalog.inherit_scope(&child, ["review"]).unwrap_err().code,
        ErrorCode::SandboxDenied
    );
    write_skill(&root, "read", "read", "read", "changed");
    catalog.refresh().unwrap();
    assert_eq!(
        catalog.inherit_scope(&parent, ["read"]).unwrap_err().code,
        ErrorCode::SandboxDenied
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn symlinked_skill_directory_is_rejected_when_platform_allows_creation() {
    let root = workspace("symlink");
    let outside = workspace("outside");
    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&outside).ok();
    fs::create_dir_all(root.join(".harness/skills")).unwrap();
    write_skill(&outside, "real", "escaped", "escaped", "body");
    let source = outside.join(".harness/skills/real");
    let target = root.join(".harness/skills/linked");
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(&source, &target).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(&source, &target).is_ok();
    if created {
        assert_eq!(
            SkillCatalog::discover(&root).unwrap_err().code,
            ErrorCode::SandboxDenied
        );
    }
    fs::remove_dir_all(root).ok();
    fs::remove_dir_all(outside).ok();
}

#[test]
fn missing_skill_root_is_an_empty_catalog_and_bad_workspace_fails() {
    let root = workspace("empty");
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let catalog = SkillCatalog::discover(&root).unwrap();
    assert_eq!(catalog.skills().count(), 0);
    fs::remove_dir_all(&root).ok();
    assert_eq!(
        SkillCatalog::discover(&root).unwrap_err().code,
        ErrorCode::Internal
    );
}
