use gitbuddy::git::{FileChange, FilePatch, Operation, PatchSelection, Repository};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

const FILE: &str = "file.txt";
const RULES: &str = "file.txt text eol=lf working-tree-encoding=UTF-16LE\n";

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn setup(rules: &str, data: &[u8]) -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let native = git2::Repository::open(&repo.root).unwrap();
    let mut config = native.config().unwrap();
    config.set_bool("core.autocrlf", false).unwrap();
    config.set_bool("core.safecrlf", false).unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    config.set_str("core.attributesfile", "").unwrap();
    repo.execute(Operation::SetIdentity(
        "Encoding fixture".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    fs::write(repo.root.join(".gitattributes"), rules).unwrap();
    fs::write(repo.root.join(FILE), data).unwrap();
    fs::write(repo.root.join("other.txt"), "other base\n").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("base".into())).unwrap();
    (dir, repo)
}

fn index_content(repo: &Repository, path: &str) -> Vec<u8> {
    let native = git2::Repository::open(&repo.root).unwrap();
    let entry = native
        .index()
        .unwrap()
        .get_path(Path::new(path), 0)
        .unwrap();
    native.find_blob(entry.id).unwrap().content().to_vec()
}

fn changed_file(repo: &Repository, path: &str) -> FileChange {
    repo.snapshot(20)
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.path == Path::new(path))
        .unwrap()
}

fn stage(repo: &Repository, path: &str) {
    repo.execute(Operation::Stage(vec![path.into()])).unwrap();
}

fn git(repo: &Repository, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(&repo.root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn hunk(patch: &FilePatch, index: usize) -> PatchSelection {
    PatchSelection::Hunk(
        patch
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind == '@')
            .nth(index)
            .unwrap()
            .0,
    )
}

fn changed_patch(repo: &Repository, staged: bool) -> FilePatch {
    let file = changed_file(repo, FILE);
    let patch = repo.worktree_patch(&file, staged).unwrap();
    assert!(patch.partial.is_some(), "{:?}", patch.unavailable);
    patch
}

#[test]
fn utf16_import_then_stage_stores_utf8_and_status_tracks_edits() {
    let (_dir, repo) = setup(RULES, &utf16("before 中文 😀\n"));
    let patch = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before 中文 😀\n+after 日本語 😀\n";
    let plan = repo.prepare_patch_file(patch.as_bytes().to_vec()).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    let raw = utf16("after 日本語 😀\n");
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), raw);
    assert_eq!(changed_file(&repo, FILE).worktree, 'M');
    stage(&repo, FILE);
    assert_eq!(index_content(&repo, FILE), "after 日本語 😀\n".as_bytes());
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), raw);
    let staged = changed_file(&repo, FILE);
    assert_eq!((staged.index, staged.worktree), ('M', ' '));
    repo.execute(Operation::Commit("after".into())).unwrap();
    assert!(repo.snapshot(20).unwrap().files.is_empty());
    fs::write(repo.root.join(FILE), utf16("edited again 😀\n")).unwrap();
    assert_eq!(changed_file(&repo, FILE).worktree, 'M');
}

#[test]
fn invalid_encoding_preserves_index_and_worktree() {
    let (_dir, repo) = setup(RULES, &utf16("before\n"));
    let invalid = [0x00, 0xd8];
    fs::write(repo.root.join(FILE), invalid).unwrap();
    let file = changed_file(&repo, FILE);
    assert_eq!(file.worktree, 'M');
    assert!(repo.execute(Operation::Stage(vec![FILE.into()])).is_err());
    assert!(repo.diff(&file, false).is_err());
    assert!(repo.export_patch(false).is_err());
    assert_eq!(index_content(&repo, FILE), b"before\n");
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), invalid);
}

#[test]
fn canonical_diff_and_mixed_export_match_git_without_publishing_objects() {
    let (_dir, repo) = setup(RULES, &utf16("before 中文 😀\n"));
    let edited = "after 日本語 😀\n";
    fs::write(repo.root.join(FILE), utf16(edited)).unwrap();
    fs::write(repo.root.join("other.txt"), "other edited\n").unwrap();
    fs::write(repo.root.join("binary.dat"), [0, 1, 2, 3, 4]).unwrap();
    let file = changed_file(&repo, FILE);
    let native = git(&repo, &["diff", "--", FILE], None);
    let without_index = |bytes: &[u8]| -> Vec<Vec<u8>> {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.starts_with(b"index "))
            .map(<[u8]>::to_vec)
            .collect()
    };
    let actual = repo.diff(&file, false).unwrap();
    assert_eq!(without_index(actual.as_bytes()), without_index(&native));
    let patch = repo.export_patch(false).unwrap();
    git(&repo, &["apply", "--cached", "--check"], Some(&patch));
    assert!(
        patch
            .windows(b"GIT binary patch".len())
            .any(|window| window == b"GIT binary patch")
    );
    assert_eq!(index_content(&repo, FILE), "before 中文 😀\n".as_bytes());
    let native = git2::Repository::open(&repo.root).unwrap();
    let canonical_oid = git2::Oid::hash_object(git2::ObjectType::Blob, edited.as_bytes()).unwrap();
    assert!(!native.odb().unwrap().exists(canonical_oid));
}

#[test]
fn encoded_partial_stage_then_unstage_preserves_other_staged_and_worktree_changes() {
    let base = (1..=32)
        .map(|i| format!("line {i} 中文\n"))
        .collect::<String>();
    let (_dir, repo) = setup(RULES, &utf16(&base));
    let staged = base.replace("line 15 中文\n", "separately staged 😀\n");
    fs::write(repo.root.join(FILE), utf16(&staged)).unwrap();
    stage(&repo, FILE);
    fs::write(repo.root.join("other.txt"), "unrelated staged\n").unwrap();
    stage(&repo, "other.txt");
    let edited = staged
        .replace("line 2 中文\n", "first 😀\n")
        .replace("line 30 中文\n", "second 😀\n");
    let raw = utf16(&edited);
    fs::write(repo.root.join(FILE), &raw).unwrap();
    let unstaged = changed_patch(&repo, false);
    repo.execute(Operation::ApplyPartial {
        patch: unstaged.partial.clone().unwrap(),
        selection: hunk(&unstaged, 0),
    })
    .unwrap();
    assert_eq!(
        index_content(&repo, FILE),
        staged.replace("line 2 中文\n", "first 😀\n").as_bytes()
    );
    let staged_patch = changed_patch(&repo, true);
    repo.execute(Operation::ApplyPartial {
        patch: staged_patch.partial.clone().unwrap(),
        selection: hunk(&staged_patch, 0),
    })
    .unwrap();
    assert_eq!(index_content(&repo, FILE), staged.as_bytes());
    assert_eq!(index_content(&repo, "other.txt"), b"unrelated staged\n");
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), raw);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn encoded_partial_discard_preserves_bom_and_unselected_raw_lines() {
    let base = (1..=20)
        .map(|i| format!("line {i} 中文\n"))
        .collect::<String>();
    let encode = |text: &str| {
        let mut raw = vec![0xfe, 0xff];
        raw.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        raw
    };
    let (_dir, repo) = setup(
        "file.txt text eol=lf ident working-tree-encoding=UTF-16\n",
        &encode(&base),
    );
    let staged = base.replace("line 10 中文\n", "separately staged 😀\n");
    fs::write(repo.root.join(FILE), encode(&staged)).unwrap();
    stage(&repo, FILE);
    let tail = staged.replace("line 18 中文\n", "unselected 😀\r\n");
    let raw = encode(&tail.replace("line 2 中文\n", "discard this 😀\n"));
    fs::write(repo.root.join(FILE), raw).unwrap();
    let patch = changed_patch(&repo, false);
    assert!(patch.partial.as_ref().unwrap().can_discard());
    repo.execute(Operation::DiscardPartial {
        patch: patch.partial.clone().unwrap(),
        selection: hunk(&patch, 0),
    })
    .unwrap();
    assert_eq!(index_content(&repo, FILE), staged.as_bytes());
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), encode(&tail));
}

#[test]
fn encoded_partial_stage_rejects_stale_raw_or_attributes_even_if_diff_is_identical() {
    let (_dir, repo) = setup(RULES, &utf16("before\n"));
    fs::write(repo.root.join(FILE), utf16("after\n")).unwrap();
    let patch = changed_patch(&repo, false);
    let op = Operation::ApplyPartial {
        patch: patch.partial.clone().unwrap(),
        selection: hunk(&patch, 0),
    };
    let external = utf16("after\r\n");
    fs::write(repo.root.join(FILE), &external).unwrap();
    assert!(repo.execute(op).is_err());
    let patch = changed_patch(&repo, false);
    fs::write(
        repo.root.join(".gitattributes"),
        format!("# edited provenance\n{RULES}"),
    )
    .unwrap();
    assert!(
        repo.execute(Operation::ApplyPartial {
            patch: patch.partial.clone().unwrap(),
            selection: hunk(&patch, 0)
        })
        .is_err()
    );
    assert_eq!(index_content(&repo, FILE), b"before\n");
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), external);
}

#[test]
fn custom_filter_cannot_fall_back_to_raw_stage_or_diff() {
    for driver in ["custom", ""] {
        let (_dir, repo) = setup("", b"before\n");
        fs::write(
            repo.root.join(".gitattributes"),
            format!("file.txt filter={driver}\n"),
        )
        .unwrap();
        fs::write(repo.root.join(FILE), "after\n").unwrap();
        let native = git2::Repository::open(&repo.root).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(native.path().join("config"))
            .unwrap()
            .write_all(
                format!("\n[filter \"{driver}\"]\n\tclean = touch custom-ran && cat\n").as_bytes(),
            )
            .unwrap();
        let file = changed_file(&repo, FILE);
        assert!(repo.execute(Operation::Stage(vec![FILE.into()])).is_err());
        assert!(repo.worktree_patch(&file, false).is_err());
        assert!(repo.export_patch(false).is_err());
        assert_eq!(index_content(&repo, FILE), b"before\n");
        assert!(!repo.root.join("custom-ran").exists());
    }
}

#[test]
fn encoded_partial_stage_uses_patch_policy_for_safecrlf() {
    let (_dir, repo) = setup(RULES, &utf16("before\n"));
    git2::Repository::open(&repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.safecrlf", true)
        .unwrap();
    let raw = utf16("after\r\n");
    fs::write(repo.root.join(FILE), &raw).unwrap();
    assert!(repo.execute(Operation::Stage(vec![FILE.into()])).is_err());
    let patch = changed_patch(&repo, false);
    repo.execute(Operation::ApplyPartial {
        patch: patch.partial.clone().unwrap(),
        selection: hunk(&patch, 0),
    })
    .unwrap();
    assert_eq!(index_content(&repo, FILE), b"after\n");
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), raw);
}

#[test]
fn implicit_required_filter_invalidates_an_unconfigured_filter_selection() {
    let rules = "file.txt text eol=lf working-tree-encoding=UTF-16LE filter=custom\n";
    let (_dir, repo) = setup(rules, &utf16("before\n"));
    assert_eq!(index_content(&repo, FILE), b"before\n");
    let raw = utf16("after\n");
    fs::write(repo.root.join(FILE), &raw).unwrap();
    let patch = changed_patch(&repo, false);
    let native = git2::Repository::open(&repo.root).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(native.path().join("config"))
        .unwrap()
        .write_all(b"\n[filter \"custom\"]\n\trequired\n")
        .unwrap();
    assert!(
        repo.execute(Operation::ApplyPartial {
            patch: patch.partial.clone().unwrap(),
            selection: hunk(&patch, 0),
        })
        .is_err()
    );
    assert!(repo.execute(Operation::Stage(vec![FILE.into()])).is_err());
    assert_eq!(index_content(&repo, FILE), b"before\n");
    assert_eq!(fs::read(repo.root.join(FILE)).unwrap(), raw);
}
