use gitbuddy::session::{
    AmendDraft, EditorView, Inspection, LineSelection, Offset, PatchKey, Selection, Session, Store,
    Tab,
};
use std::{collections::BTreeMap, fs};

fn state() -> Session {
    Session {
        tabs: vec![
            Tab {
                path: "/tmp/仓库 A".into(),
                message: "提交草稿\n\nDescription with + and −".into(),
                message_view: EditorView {
                    start: 3,
                    end: 12,
                    scroll: Offset { x: 0., y: -40. },
                },
                amend: Some(AmendDraft {
                    head: "abc123".into(),
                    reference: "refs/heads/main".into(),
                    message: "Amend 草稿".into(),
                    editor: EditorView::default(),
                }),
                selection: Selection::Commit("abc123".into()),
                selected_commits: vec!["abc123".into(), "def456".into()],
                commit_anchor: Some("abc123".into()),
                query: "author".into(),
                expanded: vec![PatchKey::Commit("abc123".into(), "src/main.rs".into())],
                show_commit_body: true,
                history_limit: 900,
                scroll: BTreeMap::from([
                    ("commit-list".into(), Offset { x: 0., y: -825. }),
                    ("commit-files-scroll".into(), Offset { x: -12., y: -240. }),
                ]),
                ..Tab::default()
            },
            Tab {
                path: "/missing/repo B".into(),
                message: "Unavailable repo draft".into(),
                selection: Selection::Inspect,
                history_tab: 1,
                inspection: Some(Inspection::Compare("base-oid".into(), "target-oid".into())),
                comparison_base: Some("base-oid".into()),
                selected_commits: vec!["other-tab-oid".into()],
                lines: vec![LineSelection {
                    key: PatchKey::Work("file with spaces.rs".into(), false),
                    fingerprint: 42,
                    rows: vec![8, 9],
                    anchor: Some(8),
                }],
                ..Tab::default()
            },
        ],
        active: 1,
        tab_scroll: Offset { x: -80., y: 0. },
        ..Session::default()
    }
}
#[test]
fn full_state_survives_an_actual_store_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/session.json");
    let store = Store::new(path.clone());
    assert!(store.load().unwrap().is_none());
    store.save(1, &state()).unwrap();
    drop(store);
    assert_eq!(Store::new(path).load().unwrap(), Some(state()));
}
#[test]
fn closing_all_tabs_is_distinct_from_first_launch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    Store::new(path.clone())
        .save(1, &Session::default())
        .unwrap();
    let restored = Store::new(path).load().unwrap().unwrap();
    assert!(restored.tabs.is_empty());
}
#[test]
fn stale_background_save_cannot_overwrite_exit_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let store = Store::new(path.clone());
    let older = store.clone();
    let mut newest = state();
    newest.tabs[1].message = "Typed immediately before quitting".into();
    store.save(8, &newest).unwrap();
    std::thread::spawn(move || older.save(7, &state()).unwrap())
        .join()
        .unwrap();
    assert_eq!(Store::new(path).load().unwrap(), Some(newest));
}
#[test]
fn unchanged_state_does_not_replace_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let store = Store::new(path.clone());
    store.save(1, &state()).unwrap();
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    store.save(2, &state()).unwrap();
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), before);
}
#[test]
fn corrupt_session_is_preserved_before_a_new_session_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let broken = b"{ partially written session";
    fs::write(&path, broken).unwrap();
    let store = Store::new(path.clone());
    assert!(store.load().is_err());
    assert!(!path.exists());
    let backup = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(fs::read(backup).unwrap(), broken);
    store.save(1, &state()).unwrap();
    assert_eq!(Store::new(path).load().unwrap(), Some(state()));
}
#[test]
fn unsupported_session_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let future = br#"{"version":99,"tabs":[{"path":"/repo","selection":"FutureView"}],"active":0}"#;
    fs::write(&path, future).unwrap();
    let store = Store::new(path.clone());
    assert!(
        store
            .load()
            .unwrap_err()
            .to_string()
            .contains("Unsupported session version")
    );
    store.save(1, &state()).unwrap();
    assert_eq!(fs::read(path).unwrap(), future);
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_do_not_block_saving_other_drafts() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let raw = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"invalid-\xff-file".to_vec()));
    let mut saved = state();
    saved.tabs[0]
        .expanded
        .push(PatchKey::Work(raw.clone(), false));
    saved.tabs[0].inspection = Some(Inspection::Blame(raw.clone(), "abc123".into()));
    saved.tabs[0].conflict_file = Some(raw);
    Store::new(path.clone()).save(1, &saved).unwrap();
    assert_eq!(Store::new(path).load().unwrap(), Some(saved));
}
#[test]
fn failed_atomic_write_preserves_previous_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let store = Store::new(path.clone());
    store.save(1, &state()).unwrap();
    fs::rename(&path, dir.path().join("previous.json")).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(store.save(2, &Session::default()).is_err());
    assert_eq!(
        Store::new(dir.path().join("previous.json")).load().unwrap(),
        Some(state())
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}
#[test]
fn older_schema_fields_receive_safe_defaults() {
    let saved: Session = serde_json::from_str(
        r#"{"version":1,"active":0,"tabs":[{"path":"/tmp/repo","message":"keep me"}]}"#,
    )
    .unwrap();
    assert_eq!(saved.tabs[0].selection, Selection::Work);
    assert_eq!(saved.tabs[0].message, "keep me");
    assert!(saved.tabs[0].scroll.is_empty());
}

#[test]
fn changed_diff_does_not_restore_selection_to_shifted_lines() {
    let lines = gitbuddy::git::diff_lines("@@ -1,1 +1,1 @@\n-old\n+new");
    let saved = LineSelection {
        key: PatchKey::Work("file.txt".into(), false),
        fingerprint: gitbuddy::session::patch_fingerprint(&lines),
        rows: vec![1, 2],
        anchor: Some(1),
    };
    let (rows, anchor) = saved.restore(&lines).unwrap();
    assert_eq!(rows.into_iter().collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(anchor, Some(1));
    let changed = gitbuddy::git::diff_lines("@@ -1,1 +1,1 @@\n-old\n+externally changed");
    assert!(saved.restore(&changed).is_none());
}

#[test]
fn restored_selection_rejects_context_and_out_of_range_rows() {
    let lines = gitbuddy::git::diff_lines("@@ -1,2 +1,2 @@\n context\n-old\n+new");
    let saved = LineSelection {
        key: PatchKey::Work("file.txt".into(), false),
        fingerprint: gitbuddy::session::patch_fingerprint(&lines),
        rows: vec![0, 1, 2, 3, usize::MAX],
        anchor: Some(usize::MAX),
    };
    let (rows, anchor) = saved.restore(&lines).unwrap();
    assert_eq!(rows.into_iter().collect::<Vec<_>>(), vec![2, 3]);
    assert_eq!(anchor, None);
}
