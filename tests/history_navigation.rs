use gitbuddy::git::{Operation, Repository};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.execute(Operation::SetIdentity(
        "History Reader".into(),
        "history@example.invalid".into(),
    ))
    .unwrap();
    (dir, repo)
}

fn commit(repo: &Repository, path: &str, text: &str) -> String {
    std::fs::write(repo.root.join(path), text).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(text.into())).unwrap();
    repo.resolve_revision("HEAD").unwrap().id
}

#[test]
fn commit_details_expose_every_parent_in_git_order_outside_loaded_history() {
    let (_dir, repo) = setup();
    let root = commit(&repo, "root", "root");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    let feature = commit(&repo, "feature", "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    let main = commit(&repo, "main", "main");
    repo.execute(Operation::Merge("feature".into())).unwrap();
    let merge = repo.resolve_revision("HEAD").unwrap().id;
    let loaded = repo.snapshot(1).unwrap();
    assert_eq!(loaded.commits.len(), 1);
    assert_eq!(loaded.commits[0].id, merge);

    let detail = repo.commit_detail(&merge).unwrap();
    assert_eq!(detail.parents, [main.clone(), feature.clone()]);
    for parent in &detail.parents {
        let detail = repo.commit_detail(parent).unwrap();
        assert_eq!(detail.id, *parent);
        assert_eq!(detail.parents.as_slice(), std::slice::from_ref(&root));
    }
    assert!(repo.commit_detail(&root).unwrap().parents.is_empty());
}

#[test]
fn resolving_and_reading_pinned_revisions_preserves_head_index_and_dirty_files() {
    let (_dir, repo) = setup();
    let root = commit(&repo, "file", "root");
    let head = commit(&repo, "file", "head");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let root_commit = raw.find_commit(root.parse().unwrap()).unwrap();
    let signature = raw.signature().unwrap();
    raw.tag("saved", root_commit.as_object(), &signature, "saved", false)
        .unwrap();
    let unreachable = raw
        .commit(
            None,
            &signature,
            &signature,
            "unreferenced commit",
            &root_commit.tree().unwrap(),
            &[&root_commit],
        )
        .unwrap()
        .to_string();
    std::fs::write(repo.root.join("file"), "staged").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    std::fs::write(repo.root.join("file"), "unstaged").unwrap();
    std::fs::write(repo.root.join("untracked"), b"untracked\0bytes").unwrap();
    let index = std::fs::read(repo.root.join(".git/index")).unwrap();
    let head_file = std::fs::read(repo.root.join(".git/HEAD")).unwrap();

    for (spec, expected) in [
        ("main", head.as_str()),
        ("HEAD~1", root.as_str()),
        ("saved", root.as_str()),
        (&root[..10], root.as_str()),
        (unreachable.as_str(), unreachable.as_str()),
    ] {
        let revision = repo.resolve_revision(spec).unwrap();
        assert_eq!(revision.id, expected);
        let detail = repo.commit_detail(&revision.id).unwrap();
        assert_eq!(detail.id, expected);
    }
    let pinned = repo.resolve_revision("saved").unwrap();
    raw.reference("refs/tags/saved", head.parse().unwrap(), true, "move tag")
        .unwrap();
    assert_eq!(repo.commit_detail(&pinned.id).unwrap().id, root);
    for invalid in ["", "missing-reference", "HEAD^{tree}", "HEAD:file"] {
        assert!(repo.resolve_revision(invalid).is_err(), "{invalid:?}");
    }

    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, head);
    assert_eq!(
        std::fs::read(repo.root.join(".git/HEAD")).unwrap(),
        head_file
    );
    assert_eq!(std::fs::read(repo.root.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(repo.root.join("file")).unwrap(), b"unstaged");
    assert_eq!(
        std::fs::read(repo.root.join("untracked")).unwrap(),
        b"untracked\0bytes"
    );
}
