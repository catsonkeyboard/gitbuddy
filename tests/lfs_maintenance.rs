use gitbuddy::git::{CancellationToken, NetworkControl, Operation, Repository};
use std::{fs, sync::Arc};
fn control() -> NetworkControl {
    NetworkControl::default()
}
fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    fs::write(repo.root.join("data.bin"), "committed content").unwrap();
    commit(&repo, "initial");
    (dir, repo)
}
fn commit(repo: &Repository, message: &str) {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
}
fn cache(repo: &Repository, oid: &str) -> std::path::PathBuf {
    git2::Repository::open(&repo.root)
        .unwrap()
        .commondir()
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}
fn orphan(repo: &Repository, text: &str) -> String {
    fs::write(repo.root.join("data.bin"), text).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let oid = repo.lfs_status().unwrap().files[0].oid.clone();
    repo.execute(Operation::UnstageAll).unwrap();
    repo.execute(Operation::Discard("data.bin".into())).unwrap();
    oid
}
#[test]
fn preview_quarantine_restore_and_permanent_delete_are_separate() {
    let (_dir, repo) = setup();
    let oid = orphan(&repo, "orphan content");
    let preview = repo.lfs_maintenance(control()).unwrap();
    assert_eq!(preview.checked, 2);
    assert_eq!(preview.protected, 1);
    assert_eq!(preview.candidates, vec![oid.clone()]);
    assert!(cache(&repo, &oid).exists());
    repo.execute(Operation::LfsQuarantine(Arc::new(preview)))
        .unwrap();
    assert!(!cache(&repo, &oid).exists());
    let preview = repo.lfs_maintenance(control()).unwrap();
    assert!(preview.candidates.is_empty());
    assert_eq!(preview.purge.len(), 1);
    repo.execute(Operation::LfsRestoreQuarantine).unwrap();
    assert!(cache(&repo, &oid).exists());
    let preview = repo.lfs_maintenance(control()).unwrap();
    repo.execute(Operation::LfsQuarantine(Arc::new(preview)))
        .unwrap();
    let preview = repo.lfs_maintenance(control()).unwrap();
    let bytes = preview.purge_bytes;
    assert!(bytes > 0);
    repo.execute(Operation::LfsPurgeQuarantine(Arc::new(preview)))
        .unwrap();
    assert!(repo.lfs_maintenance(control()).unwrap().purge.is_empty());
    assert_eq!(
        fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "committed content"
    );
}
#[test]
fn stale_cache_preview_cannot_move_or_delete_objects() {
    let (_dir, repo) = setup();
    let one = orphan(&repo, "one");
    let preview = repo.lfs_maintenance(control()).unwrap();
    let two = orphan(&repo, "two");
    assert!(
        repo.execute(Operation::LfsQuarantine(Arc::new(preview)))
            .is_err()
    );
    assert!(cache(&repo, &one).exists() && cache(&repo, &two).exists());
}
#[test]
fn reflog_only_objects_and_backup_refs_are_retained() {
    let (_dir, repo) = setup();
    let original = repo.snapshot(10).unwrap().head_id.unwrap();
    fs::write(repo.root.join("data.bin"), "new committed content").unwrap();
    commit(&repo, "second");
    let second = repo.lfs_status().unwrap().files[0].oid.clone();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.reset(
        raw.find_commit(git2::Oid::from_str(&original).unwrap())
            .unwrap()
            .as_object(),
        git2::ResetType::Hard,
        None,
    )
    .unwrap();
    let report = repo.lfs_maintenance(control()).unwrap();
    assert!(report.candidates.is_empty());
    assert!(cache(&repo, &second).exists());
}
#[test]
fn linked_worktree_index_is_a_protection_root_and_dirty_tree_blocks_cleanup() {
    let (dir, repo) = setup();
    let oid = orphan(&repo, "unused");
    let linked = dir.path().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "linked".into(),
        path: linked.clone(),
    })
    .unwrap();
    let child = Repository::open(&linked).unwrap();
    fs::write(child.root.join("data.bin"), "unique staged content").unwrap();
    child.execute(Operation::StageAll).unwrap();
    let staged = child.lfs_status().unwrap().files[0].oid.clone();
    let report = repo.lfs_maintenance(control()).unwrap();
    assert!(!report.candidates.contains(&staged));
    assert!(report.candidates.contains(&oid));
    assert!(
        repo.execute(Operation::LfsQuarantine(Arc::new(report)))
            .is_err()
    );
    assert!(cache(&repo, &oid).exists());
}
#[test]
fn corrupt_objects_are_reported_and_never_pruned() {
    let (_dir, repo) = setup();
    let oid = orphan(&repo, "unused");
    fs::write(cache(&repo, &oid), "corrupt").unwrap();
    let report = repo.lfs_maintenance(control()).unwrap();
    assert_eq!(report.corrupt, vec![oid.clone()]);
    assert!(report.candidates.is_empty());
    assert!(
        repo.execute(Operation::LfsQuarantine(Arc::new(report)))
            .is_err()
    );
    assert_eq!(fs::read_to_string(cache(&repo, &oid)).unwrap(), "corrupt");
}
#[test]
fn object_referenced_after_quarantine_cannot_be_permanently_deleted() {
    let (_dir, repo) = setup();
    let oid = orphan(&repo, "orphan");
    repo.execute(Operation::LfsQuarantine(Arc::new(
        repo.lfs_maintenance(control()).unwrap(),
    )))
    .unwrap();
    let old = repo.lfs_maintenance(control()).unwrap();
    assert_eq!(old.purge.len(), 1);
    let raw = git2::Repository::open(&repo.root).unwrap();
    let pointer = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 6\n");
    fs::write(repo.root.join("recovery.bin"), &pointer).unwrap();
    let mut index = raw.index().unwrap();
    index
        .add_path(std::path::Path::new("recovery.bin"))
        .unwrap();
    index.write().unwrap();
    let tree = raw.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = raw.head().unwrap().peel_to_commit().unwrap();
    let sig = raw.signature().unwrap();
    raw.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "reference recovery",
        &tree,
        &[&parent],
    )
    .unwrap();
    assert!(repo.lfs_maintenance(control()).unwrap().purge.is_empty());
    assert!(
        repo.execute(Operation::LfsPurgeQuarantine(Arc::new(old)))
            .is_err()
    );
    repo.execute(Operation::LfsRestoreQuarantine).unwrap();
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(
        fs::read_to_string(repo.root.join("recovery.bin")).unwrap(),
        "orphan"
    );
}
#[test]
fn missing_linked_worktree_and_custom_storage_fail_closed() {
    let (dir, repo) = setup();
    let linked = dir.path().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "linked".into(),
        path: linked.clone(),
    })
    .unwrap();
    fs::remove_dir_all(linked).unwrap();
    assert!(repo.lfs_maintenance(control()).is_err());
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("lfs.storage", dir.path().join("shared").to_str().unwrap())
        .unwrap();
    assert!(repo.lfs_maintenance(control()).is_err());
}
#[test]
fn cancelled_inspection_never_changes_cache() {
    let (_dir, repo) = setup();
    let oid = orphan(&repo, "unused");
    let token = CancellationToken::default();
    token.cancel();
    assert!(
        repo.lfs_maintenance(NetworkControl::new(None, token))
            .is_err()
    );
    assert!(cache(&repo, &oid).exists());
}
#[cfg(unix)]
#[test]
fn symlinked_quarantine_is_not_followed_or_written() {
    let (dir, repo) = setup();
    let oid = orphan(&repo, "unused");
    let external = dir.path().join("external");
    fs::create_dir(&external).unwrap();
    let root = git2::Repository::open(&repo.root)
        .unwrap()
        .commondir()
        .join("lfs/gitbuddy-quarantine");
    std::os::unix::fs::symlink(&external, root).unwrap();
    assert!(repo.lfs_maintenance(control()).is_err());
    assert!(repo.execute(Operation::LfsRestoreQuarantine).is_err());
    assert!(cache(&repo, &oid).exists());
    assert_eq!(fs::read_dir(external).unwrap().count(), 0);
}
#[test]
fn unsupported_pointer_in_history_blocks_pruning_instead_of_hiding_its_object() {
    let (_dir, repo) = setup();
    let oid = orphan(&repo, "orphan");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let text = format!(
        "version https://git-lfs.github.com/spec/v1\next-0-test sha256:{}\noid sha256:{oid}\nsize 6\n",
        "1".repeat(64)
    );
    fs::write(repo.root.join("extension.bin"), text).unwrap();
    let mut index = raw.index().unwrap();
    index
        .add_path(std::path::Path::new("extension.bin"))
        .unwrap();
    index.write().unwrap();
    let tree = raw.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = raw.head().unwrap().peel_to_commit().unwrap();
    let sig = raw.signature().unwrap();
    raw.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "external extension pointer",
        &tree,
        &[&parent],
    )
    .unwrap();
    let error = repo.lfs_maintenance(control()).unwrap_err();
    assert!(format!("{error:#}").contains("Unsupported or malformed LFS pointer"));
    assert!(cache(&repo, &oid).exists());
}
