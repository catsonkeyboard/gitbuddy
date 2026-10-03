use gitbuddy::git::{FilePatch, Operation, PatchSelection, Repository};
use std::{path::Path, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    git2::Repository::open(dir.path())
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.autocrlf", false)
        .unwrap();
    repo.execute(Operation::SetIdentity(
        "Partial Test".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    (dir, repo)
}

fn write(repo: &Repository, path: &str, data: impl AsRef<[u8]>) {
    std::fs::write(repo.root.join(path), data).unwrap();
}

fn commit(repo: &Repository) {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("base".into())).unwrap();
}

fn patch(repo: &Repository, path: &str, staged: bool) -> FilePatch {
    let snapshot = repo.snapshot(20).unwrap();
    let file = snapshot
        .files
        .iter()
        .find(|f| f.path == Path::new(path))
        .unwrap();
    repo.worktree_patch(file, staged).unwrap()
}

fn apply(repo: &Repository, patch: &FilePatch, selection: PatchSelection) {
    repo.execute(Operation::ApplyPartial {
        patch: patch.partial.clone().expect("editable patch"),
        selection,
    })
    .unwrap();
}

fn index_content(repo: &Repository, path: &str) -> Option<Vec<u8>> {
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.index()
        .unwrap()
        .get_path(Path::new(path), 0)
        .map(|e| raw.find_blob(e.id).unwrap().content().to_vec())
}

fn changed_rows(patch: &FilePatch) -> Vec<usize> {
    patch
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| matches!(l.kind, '+' | '-'))
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn hunk_stage_and_unstage_preserve_other_hunks_and_worktree() {
    let (_dir, repo) = setup();
    let base = (1..=30).map(|i| format!("line {i}\n")).collect::<String>();
    write(&repo, "file.txt", &base);
    write(&repo, "other.txt", "other\n");
    commit(&repo);
    let edited = base
        .replace("line 2\n", "first change\n")
        .replace("line 28\n", "last change\n");
    write(&repo, "file.txt", &edited);
    write(&repo, "other.txt", "staged other\n");
    repo.execute(Operation::Stage(vec!["other.txt".into()]))
        .unwrap();
    let p = patch(&repo, "file.txt", false);
    let hunks: Vec<_> = p
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind == '@')
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hunks.len(), 2);
    apply(&repo, &p, PatchSelection::Hunk(hunks[1]));
    assert_eq!(
        index_content(&repo, "file.txt").unwrap(),
        base.replace("line 28\n", "last change\n").as_bytes()
    );
    let p = patch(&repo, "file.txt", true);
    let hunk = p.lines.iter().position(|l| l.kind == '@').unwrap();
    apply(&repo, &p, PatchSelection::Hunk(hunk));
    assert_eq!(index_content(&repo, "file.txt").unwrap(), base.as_bytes());
    assert_eq!(
        index_content(&repo, "other.txt").unwrap(),
        b"staged other\n"
    );
    assert_eq!(
        std::fs::read(repo.root.join("file.txt")).unwrap(),
        edited.as_bytes()
    );
}

#[test]
fn every_subset_of_replacement_lines_is_independent_in_both_directions() {
    // Four independent changed lines, including deletion-only and addition-only selections.
    for staged in [false, true] {
        for mask in 1..16 {
            let (_dir, repo) = setup();
            write(&repo, "file.txt", "before\nold a\nold b\nafter\n");
            commit(&repo);
            write(&repo, "file.txt", "before\nnew a\nnew b\nafter\n");
            if staged {
                repo.execute(Operation::StageAll).unwrap();
            }
            let p = patch(&repo, "file.txt", staged);
            let rows = changed_rows(&p);
            assert_eq!(rows.len(), 4);
            let selected = rows
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, r)| *r)
                .collect();
            apply(&repo, &p, PatchSelection::Lines(selected));
            let mut expected = String::from("before\n");
            for (i, text) in ["old a\n", "old b\n", "new a\n", "new b\n"]
                .into_iter()
                .enumerate()
            {
                let selected = mask & (1 << i) != 0;
                let keep = if staged {
                    if i < 2 { selected } else { !selected }
                } else if i < 2 {
                    !selected
                } else {
                    selected
                };
                if keep {
                    expected.push_str(text);
                }
            }
            expected.push_str("after\n");
            assert_eq!(
                index_content(&repo, "file.txt").unwrap(),
                expected.as_bytes(),
                "staged={staged} mask={mask}"
            );
            assert_eq!(
                std::fs::read(repo.root.join("file.txt")).unwrap(),
                b"before\nnew a\nnew b\nafter\n"
            );
        }
    }
}

#[test]
fn new_file_partial_stage_and_unstage_work_without_head() {
    let (_dir, repo) = setup();
    let name = "[new]\nwith space.txt";
    write(&repo, name, "first\nsecond\nthird");
    let p = patch(&repo, name, false);
    apply(&repo, &p, PatchSelection::Lines(vec![changed_rows(&p)[1]]));
    assert_eq!(index_content(&repo, name).unwrap(), b"second\n");
    let p = patch(&repo, name, true);
    apply(&repo, &p, PatchSelection::Lines(changed_rows(&p)));
    assert!(index_content(&repo, name).is_none());
    assert_eq!(
        std::fs::read(repo.root.join(name)).unwrap(),
        b"first\nsecond\nthird"
    );
}

#[test]
fn deleted_file_can_be_partially_staged_and_restored_to_index() {
    let (_dir, repo) = setup();
    write(&repo, "file.txt", "first\nsecond\nthird\n");
    commit(&repo);
    std::fs::remove_file(repo.root.join("file.txt")).unwrap();
    let p = patch(&repo, "file.txt", false);
    apply(&repo, &p, PatchSelection::Lines(vec![changed_rows(&p)[1]]));
    assert_eq!(index_content(&repo, "file.txt").unwrap(), b"first\nthird\n");
    let p = patch(&repo, "file.txt", false);
    apply(&repo, &p, PatchSelection::Lines(changed_rows(&p)));
    assert!(index_content(&repo, "file.txt").is_none());
    let p = patch(&repo, "file.txt", true);
    apply(&repo, &p, PatchSelection::Lines(vec![changed_rows(&p)[1]]));
    assert_eq!(index_content(&repo, "file.txt").unwrap(), b"second\n");
    assert!(!repo.root.join("file.txt").exists());
}

#[test]
fn exact_bytes_survive_crlf_non_utf8_and_missing_final_newline() {
    let (_dir, repo) = setup();
    let before = b"start\r\nold \xff\r\nlast";
    let after = b"start\r\nnew \xfe\r\nlast changed";
    write(&repo, "file.txt", before);
    commit(&repo);
    write(&repo, "file.txt", after);
    let p = patch(&repo, "file.txt", false);
    apply(&repo, &p, PatchSelection::Lines(changed_rows(&p)));
    assert_eq!(index_content(&repo, "file.txt").unwrap(), after);
    let p = patch(&repo, "file.txt", true);
    apply(&repo, &p, PatchSelection::Lines(changed_rows(&p)));
    assert_eq!(index_content(&repo, "file.txt").unwrap(), before);
    assert_eq!(std::fs::read(repo.root.join("file.txt")).unwrap(), after);
}

#[test]
fn stale_worktree_or_index_patch_and_invalid_selection_are_rejected_without_writes() {
    let (_dir, repo) = setup();
    write(&repo, "file.txt", "before\n");
    commit(&repo);
    write(&repo, "file.txt", "after\n");
    let p = patch(&repo, "file.txt", false);
    let rows = changed_rows(&p);
    write(&repo, "file.txt", "later\n");
    assert!(
        repo.execute(Operation::ApplyPartial {
            patch: p.partial.unwrap(),
            selection: PatchSelection::Lines(rows)
        })
        .is_err()
    );
    assert_eq!(index_content(&repo, "file.txt").unwrap(), b"before\n");
    let p = patch(&repo, "file.txt", false);
    for selection in [
        PatchSelection::Lines(vec![]),
        PatchSelection::Lines(vec![0]),
        PatchSelection::Hunk(usize::MAX),
    ] {
        assert!(
            repo.execute(Operation::ApplyPartial {
                patch: Arc::clone(p.partial.as_ref().unwrap()),
                selection
            })
            .is_err()
        );
    }
    repo.execute(Operation::StageAll).unwrap();
    let p = patch(&repo, "file.txt", true);
    write(&repo, "file.txt", "latest\n");
    repo.execute(Operation::StageAll).unwrap();
    assert!(
        repo.execute(Operation::ApplyPartial {
            patch: p.partial.clone().unwrap(),
            selection: PatchSelection::Lines(changed_rows(&p))
        })
        .is_err()
    );
    assert_eq!(index_content(&repo, "file.txt").unwrap(), b"latest\n");
}

#[test]
fn binary_and_rename_patches_are_read_only() {
    let (_dir, repo) = setup();
    write(&repo, "binary", b"\0old");
    write(&repo, "old.txt", "original\n");
    commit(&repo);
    write(&repo, "binary", b"\0new");
    assert!(patch(&repo, "binary", false).partial.is_none());
    std::fs::rename(repo.root.join("old.txt"), repo.root.join("new.txt")).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let p = patch(&repo, "new.txt", true);
    assert!(p.partial.is_none());
    assert!(p.unavailable.unwrap().contains("Renames"));
}

#[test]
fn conflicts_and_truncated_previews_do_not_offer_partial_actions() {
    let (_dir, repo) = setup();
    write(&repo, "file.txt", "base\n");
    commit(&repo);
    let branch = repo.snapshot(10).unwrap().branch;
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "file.txt", "feature\n");
    commit(&repo);
    repo.execute(Operation::Checkout(branch)).unwrap();
    write(&repo, "file.txt", "main\n");
    commit(&repo);
    let _ = repo.execute(Operation::Merge("feature".into()));
    assert!(
        repo.snapshot(10)
            .unwrap()
            .files
            .iter()
            .any(|f| f.conflict())
    );
    assert!(patch(&repo, "file.txt", false).partial.is_none());
    write(&repo, "large.txt", "text\n".repeat(20_001));
    let p = patch(&repo, "large.txt", false);
    assert!(p.partial.is_none());
    assert!(p.unavailable.unwrap().contains("truncated"));
}

#[test]
fn stale_head_and_cross_repository_selections_are_rejected() {
    let (_dir, repo) = setup();
    write(&repo, "file.txt", "base\n");
    commit(&repo);
    write(&repo, "file.txt", "next\n");
    repo.execute(Operation::StageAll).unwrap();
    let p = patch(&repo, "file.txt", true);
    let (_other_dir, other) = setup();
    assert!(
        other
            .execute(Operation::ApplyPartial {
                patch: p.partial.clone().unwrap(),
                selection: PatchSelection::Lines(changed_rows(&p))
            })
            .is_err()
    );
    repo.execute(Operation::Commit("next".into())).unwrap();
    assert!(
        repo.execute(Operation::ApplyPartial {
            patch: p.partial.clone().unwrap(),
            selection: PatchSelection::Lines(changed_rows(&p))
        })
        .is_err()
    );
    assert_eq!(index_content(&repo, "file.txt").unwrap(), b"next\n");
}

#[cfg(unix)]
#[test]
fn content_selection_preserves_executable_mode_and_rejects_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (_dir, repo) = setup();
    write(&repo, "script", "old\n");
    std::fs::set_permissions(
        repo.root.join("script"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("script", repo.root.join("link")).unwrap();
    commit(&repo);
    write(&repo, "script", "new\n");
    let p = patch(&repo, "script", false);
    apply(&repo, &p, PatchSelection::Lines(changed_rows(&p)));
    let raw = git2::Repository::open(&repo.root).unwrap();
    assert_eq!(
        raw.index()
            .unwrap()
            .get_path(Path::new("script"), 0)
            .unwrap()
            .mode,
        0o100755
    );
    std::fs::remove_file(repo.root.join("link")).unwrap();
    symlink("elsewhere", repo.root.join("link")).unwrap();
    assert!(patch(&repo, "link", false).partial.is_none());
}
