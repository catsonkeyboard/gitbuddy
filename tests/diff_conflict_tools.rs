use gitbuddy::git::{
    BlockChoice, FilePatch, MergeTool, NetworkControl, Operation, PatchSelection, Repository,
    choose_conflict_block, conflict_blocks, word_changes,
};
use std::{path::Path, sync::Arc};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.execute(Operation::SetIdentity(
        "Diff Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    git2::Repository::open(&repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.autocrlf", false)
        .unwrap();
    (dir, repo)
}
fn commit(repo: &Repository) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("snapshot".into())).unwrap();
    repo.snapshot(20).unwrap().head_id.unwrap()
}
fn index_content(repo: &Repository, path: &str) -> Vec<u8> {
    let raw = git2::Repository::open(&repo.root).unwrap();
    let entry = raw.index().unwrap().get_path(Path::new(path), 0).unwrap();
    raw.find_blob(entry.id).unwrap().content().to_vec()
}
#[test]
fn word_highlights_preserve_unicode_boundaries_and_unchanged_tokens() {
    let old = "let 城市 = old_name + 10; // same";
    let new = "let 城市 = new_name + 20; // same";
    let (a, b) = word_changes(old, new);
    assert_eq!(
        a.iter().map(|r| &old[r.clone()]).collect::<Vec<_>>(),
        vec!["old_name", "10"]
    );
    assert_eq!(
        b.iter().map(|r| &new[r.clone()]).collect::<Vec<_>>(),
        vec!["new_name", "20"]
    );
    let (a, b) = word_changes("城市=昆明", "城市=北京");
    assert_eq!(&"城市=昆明"[a[0].clone()], "昆明");
    assert_eq!(&"城市=北京"[b[0].clone()], "北京");
    assert!(word_changes(old, old).0.is_empty());
    let long = "x + ".repeat(20_000);
    let (a, _) = word_changes(&long, "short");
    assert_eq!(a, vec![0..long.len()]);
}
#[test]
fn split_rows_keep_each_change_index_once_and_context_on_both_sides() {
    let p = FilePatch::read_only(
        "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,4 +1,3 @@\n common\n-old a\n-old b\n-old c\n+new a\n+new b\n",
    );
    let mut changed = Vec::new();
    for row in &p.display.rows {
        for i in [row.left, row.right].into_iter().flatten() {
            if matches!(p.lines[i].kind, '+' | '-') {
                changed.push(i);
            }
        }
        if row.left == row.right && !row.header {
            assert_eq!(p.lines[row.left.unwrap()].kind, ' ');
        }
    }
    changed.sort_unstable();
    assert_eq!(
        changed,
        p.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| matches!(l.kind, '+' | '-'))
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    );
    assert!(
        p.display
            .rows
            .iter()
            .any(|r| r.left.is_some() && r.right.is_none())
    );
    assert!(p.display.highlights.iter().any(|r| !r.is_empty()));
}
#[test]
fn full_context_above_preview_cap_still_stages_and_unstages_canonical_rows() {
    let (_dir, repo) = repo();
    let base = (1..=25_000)
        .map(|n| format!("line {n}\r\n"))
        .collect::<String>();
    std::fs::write(repo.root.join("file"), &base).unwrap();
    commit(&repo);
    let edited = base
        .replace("line 3\r\n", "changed three\r\n")
        .replace("line 24990\r\n", "changed last\r\n");
    std::fs::write(repo.root.join("file"), &edited).unwrap();
    let file = repo.snapshot(20).unwrap().files.remove(0);
    let compact = repo.worktree_patch(&file, false).unwrap();
    assert!(compact.lines.len() < 40);
    let full = repo.worktree_patch_context(&file, false, true).unwrap();
    assert!(full.lines.len() > 25_000);
    assert!(full.lines.iter().any(|l| l.text == " line 12500"));
    assert!(full.partial.is_some());
    let rows = full
        .display
        .rows
        .iter()
        .flat_map(|r| [r.left, r.right])
        .flatten()
        .filter(|&i| full.lines[i].text == "-line 3" || full.lines[i].text == "+changed three")
        .collect::<Vec<_>>();
    repo.execute(Operation::ApplyPartial {
        patch: full.partial.unwrap(),
        selection: PatchSelection::Lines(rows),
    })
    .unwrap();
    assert_eq!(
        index_content(&repo, "file"),
        base.replace("line 3\r\n", "changed three\r\n").as_bytes()
    );
    let staged = repo
        .worktree_patch_context(&repo.snapshot(20).unwrap().files.remove(0), true, true)
        .unwrap();
    let rows = staged
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| matches!(l.kind, '+' | '-'))
        .map(|(i, _)| i)
        .collect();
    repo.execute(Operation::ApplyPartial {
        patch: staged.partial.unwrap(),
        selection: PatchSelection::Lines(rows),
    })
    .unwrap();
    assert_eq!(index_content(&repo, "file"), base.as_bytes());
    assert_eq!(
        std::fs::read(repo.root.join("file")).unwrap(),
        edited.as_bytes()
    );
}

#[test]
fn full_context_uses_literal_paths_and_keeps_renamed_file_sides() {
    let (_dir, repo) = repo();
    let old = "[old]*?.txt";
    let new = "[new]*?.txt";
    let base = (1..=80)
        .map(|n| format!("base line {n}\n"))
        .collect::<String>();
    std::fs::write(repo.root.join(old), &base).unwrap();
    let before = commit(&repo);
    std::fs::rename(repo.root.join(old), repo.root.join(new)).unwrap();
    std::fs::write(
        repo.root.join(new),
        base.replace("base line 3\n", "changed three\n"),
    )
    .unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let file = repo
        .snapshot(20)
        .unwrap()
        .files
        .into_iter()
        .find(|f| f.path == Path::new(new))
        .unwrap();
    assert_eq!(file.original.as_deref(), Some(Path::new(old)));
    let patch = repo.worktree_patch_context(&file, true, true).unwrap();
    assert!(patch.lines.iter().any(|l| l.text.contains("base line 80")));
    let after = commit(&repo);
    let file = repo.commit_detail(&after).unwrap().files.remove(0);
    let text = repo.commit_file_diff_context(&after, &file, true).unwrap();
    assert!(text.contains("base line 1\n") && text.contains("base line 80\n"));
    assert!(text.contains("rename from [old]*?.txt"));
    let comparison = repo.compare(&before, &after).unwrap();
    let text = repo
        .comparison_file_diff_context(&comparison, &comparison.files[0], true)
        .unwrap();
    assert!(text.contains("base line 80\n"));
    std::fs::write(
        repo.root.join(new),
        base.replace("base line 80\n", "changed last\n"),
    )
    .unwrap();
    let file = repo
        .snapshot(20)
        .unwrap()
        .files
        .into_iter()
        .find(|f| f.path == Path::new(new))
        .unwrap();
    assert!(
        repo.worktree_patch_context(&file, false, true)
            .unwrap()
            .lines
            .iter()
            .any(|l| l.text.contains("changed last"))
    );
}
#[test]
fn full_context_history_and_comparison_include_both_ends_and_explicitly_reject_limits() {
    let (_dir, repo) = repo();
    let base = (1..=40).map(|n| format!("line {n}\n")).collect::<String>();
    std::fs::write(repo.root.join("file"), &base).unwrap();
    let first = commit(&repo);
    std::fs::write(
        repo.root.join("file"),
        base.replace("line 20\n", "middle changed\n"),
    )
    .unwrap();
    let second = commit(&repo);
    let compare = repo.compare(&first, &second).unwrap();
    let file = &compare.files[0];
    for text in [
        repo.commit_file_diff_context(&second, file, true).unwrap(),
        repo.comparison_file_diff_context(&compare, file, true)
            .unwrap(),
    ] {
        let patch = FilePatch::read_only_context(&text, true).unwrap();
        assert!(patch.lines.iter().any(|l| l.text == " line 1"));
        assert!(patch.lines.iter().any(|l| l.text == " line 40"));
    }
    let compact = repo.commit_file_diff(&second, file).unwrap();
    assert!(!compact.lines().any(|l| l == " line 1"));
    assert!(
        FilePatch::read_only_context(&"x\n".repeat(100_001), true)
            .unwrap_err()
            .to_string()
            .contains("safety limit")
    );
}
#[test]
fn blocks_keep_clean_edits_diff3_base_crlf_and_other_blocks() {
    let text = "clean local edit\r\n<<<<<<< ours\r\nlocal α\r\n||||||| base\r\nancestor\r\n=======\r\nremote β\r\n>>>>>>> theirs\r\nclean middle\r\n<<<<<<<< ours\r\nsecond local\r\n========\r\nsecond remote\r\n>>>>>>>> theirs\r\nend without newline";
    let blocks = conflict_blocks(text).unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].line, 2);
    assert_eq!(&text[blocks[0].base.clone().unwrap()], "ancestor\r\n");
    let first = choose_conflict_block(text, &blocks[0], BlockChoice::Both).unwrap();
    assert!(first.starts_with("clean local edit\r\nlocal α\r\nremote β\r\nclean middle\r\n"));
    assert!(first.ends_with("end without newline"));
    assert!(choose_conflict_block(&first, &blocks[1], BlockChoice::Theirs).is_err());
    let remaining = conflict_blocks(&first).unwrap();
    let final_text = choose_conflict_block(&first, &remaining[0], BlockChoice::Theirs).unwrap();
    assert_eq!(
        final_text,
        "clean local edit\r\nlocal α\r\nremote β\r\nclean middle\r\nsecond remote\r\nend without newline"
    );
}
#[test]
fn malformed_nested_markers_and_same_length_draft_edits_are_rejected() {
    for text in [
        "<<<<<<< ours\na\n",
        "=======\n",
        "<<<<<<< ours\n<<<<<<< nested\n=======\n>>>>>>> theirs\n",
        "<<<<<<< ours\na\n========\nb\n>>>>>>> theirs\n",
    ] {
        assert!(conflict_blocks(text).is_err());
    }
    let text = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
    let blocks = conflict_blocks(text).unwrap();
    assert!(
        choose_conflict_block(
            &text.replace("\na\n", "\nx\n"),
            &blocks[0],
            BlockChoice::Ours
        )
        .is_err()
    );
    assert_eq!(
        choose_conflict_block(text, &blocks[0], BlockChoice::Ours).unwrap(),
        "a\n"
    );
    assert!(
        conflict_blocks("ordinary >>> text\n<<<<<<<not-a-marker\n")
            .unwrap()
            .is_empty()
    );
}
fn conflict() -> (
    tempfile::TempDir,
    Repository,
    Arc<gitbuddy::git::ConflictContext>,
) {
    let (dir, repo) = repo();
    let name = "résult ; $(echo literal).txt";
    std::fs::write(repo.root.join(name), "base\n").unwrap();
    commit(&repo);
    repo.execute(Operation::CreateBranch("incoming".into()))
        .unwrap();
    std::fs::write(repo.root.join(name), "remote\n").unwrap();
    commit(&repo);
    repo.execute(Operation::Checkout("main".into())).unwrap();
    std::fs::write(repo.root.join(name), "local\n").unwrap();
    commit(&repo);
    assert!(repo.execute(Operation::Merge("incoming".into())).is_err());
    let context = Arc::new(repo.conflict_context(Path::new(name)).unwrap());
    (dir, repo, context)
}
#[cfg(unix)]
fn tool(dir: &Path, script: &str) -> MergeTool {
    use std::os::unix::fs::PermissionsExt;
    let program = dir.join("fake merge executable");
    std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    MergeTool {
        program: program.to_string_lossy().into_owned(),
        args: ["$BASE", "$LOCAL", "$REMOTE", "$MERGED"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    }
}
#[cfg(unix)]
#[test]
fn external_tool_receives_four_sides_and_returns_draft_without_git_writes() {
    let (dir, repo, context) = conflict();
    let before = std::fs::read(repo.root.join(&context.file.path)).unwrap();
    let index = std::fs::read(repo.root.join(".git/index")).unwrap();
    let tool = tool(
        dir.path(),
        "test \"$(cat \"$1\")\" = base || exit 1\ntest \"$(cat \"$2\")\" = local || exit 2\ntest \"$(cat \"$3\")\" = remote || exit 3\nprintf 'combined\\n' > \"$4\"",
    );
    let text = repo
        .run_merge_tool(&context, "custom draft\n", &tool, NetworkControl::default())
        .unwrap();
    assert_eq!(text, "combined\n");
    assert_eq!(
        std::fs::read(repo.root.join(&context.file.path)).unwrap(),
        before
    );
    assert_eq!(std::fs::read(repo.root.join(".git/index")).unwrap(), index);
    repo.execute(Operation::ResolveConflict {
        context,
        resolution: gitbuddy::git::ConflictResolution::Edited(text),
    })
    .unwrap();
    assert!(repo.conflict_session().unwrap().files.is_empty());
}
#[cfg(unix)]
#[test]
fn tool_failure_missing_executable_and_stale_context_preserve_conflicts() {
    let (dir, repo, context) = conflict();
    let tool = tool(dir.path(), "printf 'unsaved recovery\\n' > \"$4\"\nexit 9");
    let error = repo
        .run_merge_tool(&context, "draft", &tool, NetworkControl::default())
        .unwrap_err();
    assert!(format!("{error:#}").contains("kept at"));
    // Remove this test's recovery copy after checking that it was retained.
    let message = error.to_string();
    let recovery = message
        .split("Tool result kept at ")
        .nth(1)
        .unwrap()
        .split(". Reload")
        .next()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(recovery).unwrap(),
        "unsaved recovery\n"
    );
    std::fs::remove_dir_all(Path::new(recovery).parent().unwrap().parent().unwrap()).unwrap();
    let missing = MergeTool {
        program: "/gitbuddy-nonexistent-tool".into(),
        args: vec!["$MERGED".into()],
    };
    assert!(
        repo.run_merge_tool(&context, "draft", &missing, NetworkControl::default())
            .is_err()
    );
    std::fs::write(repo.root.join(&context.file.path), "externally changed\n").unwrap();
    assert!(
        repo.run_merge_tool(&context, "draft", &tool, NetworkControl::default())
            .is_err()
    );
    assert_eq!(repo.conflict_session().unwrap().files.len(), 1);
}
#[cfg(unix)]
#[test]
fn invalid_tool_outputs_are_not_imported_and_leave_git_unchanged() {
    let (dir, repo, context) = conflict();
    let original = std::fs::read(repo.root.join(&context.file.path)).unwrap();
    let index = std::fs::read(repo.root.join(".git/index")).unwrap();
    for script in [
        "printf '\\000' > \"$4\"",
        "printf '\\377' > \"$4\"",
        "rm \"$4\"; ln -s \"$2\" \"$4\"",
        "dd if=/dev/zero of=\"$4\" bs=1048576 count=3 2>/dev/null",
    ] {
        let tool = tool(dir.path(), script);
        let error = repo
            .run_merge_tool(&context, "draft\n", &tool, NetworkControl::default())
            .unwrap_err();
        let message = error.to_string();
        let recovery = message
            .split("Tool result kept at ")
            .nth(1)
            .unwrap()
            .split(". Reload")
            .next()
            .unwrap();
        assert!(std::fs::symlink_metadata(recovery).is_ok());
        std::fs::remove_dir_all(Path::new(recovery).parent().unwrap().parent().unwrap()).unwrap();
        assert_eq!(
            std::fs::read(repo.root.join(&context.file.path)).unwrap(),
            original
        );
        assert_eq!(std::fs::read(repo.root.join(".git/index")).unwrap(), index);
    }
}

#[cfg(unix)]
#[test]
fn external_tool_cancellation_stops_process_group_and_keeps_draft_and_index() {
    use std::time::{Duration, Instant};
    let (dir, repo, context) = conflict();
    let ready = dir.path().join("tool.started");
    let mut tool = tool(
        dir.path(),
        "touch \"$5\"\nsleep 30\nprintf 'late\\n' > \"$4\"",
    );
    tool.args.push(ready.to_string_lossy().into_owned());
    let index = std::fs::read(repo.root.join(".git/index")).unwrap();
    let cancellation = gitbuddy::git::CancellationToken::default();
    let control = NetworkControl::new(None, cancellation.clone());
    let worker = repo.clone();
    let context2 = context.clone();
    let thread =
        std::thread::spawn(move || worker.run_merge_tool(&context2, "draft\n", &tool, control));
    let started = Instant::now();
    while !ready.exists() {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    cancellation.cancel();
    let error = thread.join().unwrap().unwrap_err();
    assert!(gitbuddy::git::is_cancelled(&error));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(std::fs::read(repo.root.join(".git/index")).unwrap(), index);
    assert_eq!(repo.conflict_session().unwrap().files.len(), 1);
}
#[test]
fn tool_arguments_are_structured_and_require_an_output_placeholder() {
    assert!(MergeTool::default().validate().is_ok());
    for args in [
        vec!["$LOCAL"],
        vec!["--output=$MERGED"],
        vec!["$MERGED", "x\0"],
    ] {
        assert!(
            MergeTool {
                program: "tool".into(),
                args: args.into_iter().map(str::to_owned).collect()
            }
            .validate()
            .is_err()
        );
    }
}

#[cfg(unix)]
#[test]
fn tool_result_after_external_file_change_is_kept_but_not_applied() {
    let (dir, repo, context) = conflict();
    let before = std::fs::read(repo.root.join(".git/index")).unwrap();
    let mut tool = tool(
        dir.path(),
        "printf 'tool result\\n' > \"$4\"\nprintf 'outside edit\\n' > \"$5\"",
    );
    tool.args.push(
        repo.root
            .join(&context.file.path)
            .to_string_lossy()
            .into_owned(),
    );
    let error = repo
        .run_merge_tool(&context, "draft", &tool, NetworkControl::default())
        .unwrap_err();
    assert!(format!("{error:#}").contains("working file changed"));
    assert_eq!(std::fs::read(repo.root.join(".git/index")).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(repo.root.join(&context.file.path)).unwrap(),
        "outside edit\n"
    );
    let message = error.to_string();
    let recovery = message
        .split("Tool result kept at ")
        .nth(1)
        .unwrap()
        .split(". Reload")
        .next()
        .unwrap();
    assert_eq!(std::fs::read_to_string(recovery).unwrap(), "tool result\n");
    std::fs::remove_dir_all(Path::new(recovery).parent().unwrap().parent().unwrap()).unwrap();
}

#[test]
fn full_context_rejects_oversize_worktree_and_index_inputs_before_generation() {
    let (_dir, repo) = repo();
    std::fs::write(repo.root.join("file"), "small\n").unwrap();
    commit(&repo);
    let file = std::fs::File::create(repo.root.join("file")).unwrap();
    file.set_len(17 * 1024 * 1024).unwrap();
    let file = repo.snapshot(20).unwrap().files.remove(0);
    assert!(
        repo.worktree_patch_context(&file, false, true)
            .unwrap_err()
            .to_string()
            .contains("safety limit")
    );
    repo.execute(Operation::Stage(vec!["file".into()])).unwrap();
    assert!(
        repo.worktree_patch_context(&repo.snapshot(20).unwrap().files.remove(0), true, true)
            .unwrap_err()
            .to_string()
            .contains("safety limit")
    );
}

#[test]
fn external_merge_rejects_missing_text_sides_and_cross_repository_contexts() {
    let (_dir, repo, context) = conflict();
    let mut binary = (*context).clone();
    binary.ours.as_mut().unwrap().text = None;
    assert!(!binary.can_external_merge());
    assert!(
        repo.run_merge_tool(
            &binary,
            "draft",
            &MergeTool::default(),
            NetworkControl::default()
        )
        .is_err()
    );
    let (_otherdir, other) = self::repo();
    assert!(
        other
            .run_merge_tool(
                &context,
                "draft",
                &MergeTool::default(),
                NetworkControl::default()
            )
            .unwrap_err()
            .to_string()
            .contains("different repository")
    );
}
