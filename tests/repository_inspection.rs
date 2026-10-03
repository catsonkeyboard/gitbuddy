use gitbuddy::git::{Operation, Repository};
use std::path::Path;

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.execute(Operation::SetIdentity(
        "Alice".into(),
        "alice@example.invalid".into(),
    ))
    .unwrap();
    (dir, repo)
}
fn write(repo: &Repository, path: &str, text: &str) {
    std::fs::write(repo.root.join(path), text).unwrap();
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.snapshot(20).unwrap().head_id.unwrap()
}
fn ids(history: &gitbuddy::git::FileHistory) -> Vec<String> {
    history
        .entries
        .iter()
        .map(|e| e.commit.id.clone())
        .collect()
}

#[test]
fn history_filters_files_paginates_and_pins_the_revision() {
    let (_dir, repo) = setup();
    write(&repo, "file", "first\n");
    let root = commit(&repo, "root");
    write(&repo, "other", "other");
    commit(&repo, "unrelated");
    write(&repo, "file", "second\n");
    let second = commit(&repo, "second");
    let page = repo.file_history(Path::new("file"), "HEAD", 1).unwrap();
    assert_eq!(ids(&page), std::slice::from_ref(&second));
    assert!(page.has_more);
    write(&repo, "file", "third\n");
    commit(&repo, "third");
    let more = repo
        .file_history(Path::new("file"), &page.revision.id, 100)
        .unwrap();
    assert_eq!(ids(&more), [second, root]);
    assert!(!more.has_more);
    for e in &more.entries {
        let patch = repo.commit_file_diff(&e.commit.id, &e.file).unwrap();
        assert!(patch.contains("file"));
    }
    assert!(
        repo.file_history(Path::new("missing"), "HEAD", 100)
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(repo.file_history(Path::new("file"), "HEAD", 0).is_err());
}

#[test]
fn history_follows_renames_without_conflating_reused_names() {
    let (_dir, repo) = setup();
    write(&repo, "old", "one\ntwo\nthree\nfour\nfive\nsix\n");
    let root = commit(&repo, "root");
    std::fs::rename(repo.root.join("old"), repo.root.join("new")).unwrap();
    write(&repo, "old", "different replacement file\n");
    let renamed = commit(&repo, "rename and reuse");
    let history = repo.file_history(Path::new("new"), "HEAD", 100).unwrap();
    assert_eq!(ids(&history), [renamed.clone(), root.clone()]);
    assert_eq!(
        history.entries[0].file.original.as_deref(),
        Some(Path::new("old"))
    );
    assert_eq!(history.entries[1].file.path, Path::new("old"));
    let comparison = repo.compare(&root, &renamed).unwrap();
    let rename = comparison
        .files
        .iter()
        .find(|f| f.path == Path::new("new"))
        .unwrap();
    let patch = repo.comparison_file_diff(&comparison, rename).unwrap();
    assert!(patch.contains("rename from old"));
    assert!(!patch.contains("different replacement"));
    let commit_patch = repo.commit_file_diff(&renamed, rename).unwrap();
    assert_eq!(patch, commit_patch);
    assert_eq!(
        ids(&repo.file_history(Path::new("old"), "HEAD", 100).unwrap()),
        [renamed]
    );
}

#[test]
fn file_history_traverses_merge_parents_and_tracks_each_name() {
    let (_dir, repo) = setup();
    write(&repo, "old", "one\ntwo\nthree\nfour\nfive\nsix\n");
    let root = commit(&repo, "root");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    std::fs::rename(repo.root.join("old"), repo.root.join("new")).unwrap();
    let rename = commit(&repo, "rename on feature");
    write(&repo, "new", "one\ntwo\nthree\nfour\nfive\nfeature\n");
    let feature = commit(&repo, "feature edit");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "unrelated", "main");
    commit(&repo, "main change");
    repo.execute(Operation::Merge("feature".into())).unwrap();
    let history = repo.file_history(Path::new("new"), "HEAD", 100).unwrap();
    let all = ids(&history);
    assert!(all.contains(&root));
    assert!(all.contains(&rename));
    assert!(all.contains(&feature));
    assert_eq!(all.iter().filter(|id| *id == &root).count(), 1);
    for e in &history.entries {
        repo.commit_file_diff(&e.commit.id, &e.file).unwrap();
    }
}

#[test]
fn history_includes_deletion_and_stops_at_a_new_incarnation() {
    let (_dir, repo) = setup();
    write(&repo, "file", "original");
    let root = commit(&repo, "root");
    std::fs::remove_file(repo.root.join("file")).unwrap();
    let deleted = commit(&repo, "delete");
    let h = repo.file_history(Path::new("file"), "HEAD", 100).unwrap();
    assert_eq!(ids(&h), [deleted, root]);
    assert_eq!(h.entries[0].file.status, 'D');
    write(&repo, "file", "new incarnation");
    let new = commit(&repo, "recreate");
    assert_eq!(
        ids(&repo.file_history(Path::new("file"), "HEAD", 100).unwrap()),
        [new]
    );
}

#[test]
fn blame_attributes_committed_lines_and_ignores_checkout_and_dirty_content() {
    let (_dir, repo) = setup();
    write(&repo, "file", "one\r\ntwo\r\nthree");
    let root = commit(&repo, "root");
    repo.execute(Operation::SetIdentity(
        "Bob".into(),
        "bob@example.invalid".into(),
    ))
    .unwrap();
    write(&repo, "file", "one\r\nTWO\r\nthree");
    let edited = commit(&repo, "edit second line");
    write(&repo, "file", "dirty\n");
    let index = std::fs::read(repo.root.join(".git/index")).unwrap();
    let view = repo.blame(Path::new("file"), &edited).unwrap();
    assert_eq!(view.lines.len(), 3);
    assert_eq!(view.lines[0].commit_id, root);
    assert_eq!(view.lines[0].author, "Alice");
    assert_eq!(view.lines[0].text, "one");
    assert_eq!(view.lines[1].commit_id, edited);
    assert_eq!(view.lines[1].author, "Bob");
    assert_eq!(view.lines[1].original_line, 2);
    assert_eq!(view.lines[2].text, "three");
    assert_eq!(std::fs::read(repo.root.join(".git/index")).unwrap(), index);
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file")).unwrap(),
        "dirty\n"
    );
    let old = repo.blame(Path::new("file"), &root).unwrap();
    assert!(old.lines.iter().all(|l| l.commit_id == root));
    assert_eq!(old.lines[1].text, "two");
}

#[test]
fn blame_follows_rename_and_handles_empty_and_non_utf8_content() {
    let (_dir, repo) = setup();
    write(&repo, "old", "one\ntwo\n");
    write(&repo, "empty", "");
    std::fs::write(repo.root.join("bytes"), b"valid\n\xff\n").unwrap();
    let root = commit(&repo, "root");
    std::fs::rename(repo.root.join("old"), repo.root.join("new")).unwrap();
    commit(&repo, "rename");
    let view = repo.blame(Path::new("new"), "HEAD").unwrap();
    assert!(view.lines.iter().all(|l| l.commit_id == root));
    assert_eq!(view.lines[0].original_path, Path::new("old"));
    assert!(
        repo.blame(Path::new("empty"), "HEAD")
            .unwrap()
            .lines
            .is_empty()
    );
    assert_eq!(
        repo.blame(Path::new("bytes"), "HEAD").unwrap().lines.len(),
        2
    );
}

#[test]
fn blame_caps_previews_and_rejects_binary_large_and_non_regular_files() {
    let (_dir, repo) = setup();
    write(&repo, "long", &"x\n".repeat(20_001));
    std::fs::write(repo.root.join("binary"), b"a\0b").unwrap();
    write(&repo, "large", &"x".repeat(2 * 1024 * 1024 + 1));
    #[cfg(unix)]
    std::os::unix::fs::symlink("long", repo.root.join("link")).unwrap();
    commit(&repo, "root");
    let view = repo.blame(Path::new("long"), "HEAD").unwrap();
    assert!(view.truncated);
    assert_eq!(view.lines.len(), 20_000);
    assert!(repo.blame(Path::new("binary"), "HEAD").is_err());
    assert!(repo.blame(Path::new("large"), "HEAD").is_err());
    #[cfg(unix)]
    assert!(repo.blame(Path::new("link"), "HEAD").is_err());
}

#[test]
fn comparisons_pin_refs_support_reverse_and_literal_paths() {
    let (_dir, repo) = setup();
    write(&repo, "[file]*", "base\n");
    write(&repo, "other", "other\n");
    let base = commit(&repo, "base");
    repo.execute(Operation::Tag("start".into())).unwrap();
    repo.execute(Operation::CreateBranch("topic/slash".into()))
        .unwrap();
    write(&repo, "[file]*", "target\n");
    write(&repo, "new", "new\n");
    std::fs::remove_file(repo.root.join("other")).unwrap();
    let target = commit(&repo, "target");
    let comparison = repo
        .compare("refs/tags/start", "refs/heads/topic/slash")
        .unwrap();
    assert_eq!(comparison.base.id, base);
    assert_eq!(comparison.target.id, target);
    let file = comparison
        .files
        .iter()
        .find(|f| f.path == Path::new("[file]*"))
        .unwrap();
    let patch = repo.comparison_file_diff(&comparison, file).unwrap();
    assert!(patch.contains("+target"));
    assert!(!patch.contains("+new"));
    let reverse = repo.compare(&target, &base).unwrap();
    assert_eq!(comparison.insertions, reverse.deletions);
    assert_eq!(comparison.deletions, reverse.insertions);
    assert!(repo.compare("HEAD", "HEAD").unwrap().files.is_empty());
    write(&repo, "[file]*", "later\n");
    commit(&repo, "later");
    assert_eq!(repo.comparison_file_diff(&comparison, file).unwrap(), patch);
    assert!(repo.compare("missing-ref", "HEAD").is_err());
    assert!(repo.compare("HEAD:file", "HEAD").is_err());
    let tree = repo.revision_files(&base).unwrap();
    assert!(tree.paths.contains(&Path::new("other").to_path_buf()));
}

#[test]
fn inspection_rejects_paths_outside_the_repo_and_supports_non_utf8_names() {
    let (_dir, repo) = setup();
    write(&repo, "file", "text\n");
    commit(&repo, "root");
    for path in ["../file", "/tmp/file", ""] {
        assert!(repo.file_history(Path::new(path), "HEAD", 100).is_err());
        assert!(repo.blame(Path::new(path), "HEAD").is_err());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let path =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec()));
        // APFS rejects this filename; create the committed entry directly.
        let raw = git2::Repository::open(&repo.root).unwrap();
        let mut index = raw.index().unwrap();
        let mut entry = index.get_path(Path::new("file"), 0).unwrap();
        entry.path = b"non-utf8-\xff".to_vec();
        entry.id = raw.blob(b"line\n").unwrap();
        entry.flags = 0;
        entry.file_size = 5;
        index.add(&entry).unwrap();
        index.write().unwrap();
        repo.execute(Operation::Commit("special path".into()))
            .unwrap();
        let id = repo.resolve_revision("HEAD").unwrap().id;
        assert!(repo.revision_files("HEAD").unwrap().paths.contains(&path));
        assert_eq!(
            repo.file_history(&path, "HEAD", 100).unwrap().entries[0]
                .commit
                .id,
            id
        );
        assert_eq!(repo.blame(&path, "HEAD").unwrap().lines[0].text, "line");
    }
}
