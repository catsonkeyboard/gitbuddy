use gitbuddy::git::{ConflictResolution, Operation, Repository};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

const VERSION: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";
const BODY: &[u8] = b"attribute fixture content\0binary\xff";

fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn setup() -> (tempfile::TempDir, Repository) {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(&directory.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    let mut config = raw(&repo).config().unwrap();
    config.set_str("core.attributesfile", "").unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_bool("core.ignorecase", false).unwrap();
    config
        .set_str("gitbuddy.lfsProgram", "/unavailable/git-lfs")
        .unwrap();
    fs::write(repo.root.join("base"), b"base").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("initial".into())).unwrap();
    (directory, repo)
}
fn staged(repo: &Repository, path: &Path) -> Vec<u8> {
    let raw = raw(repo);
    let entry = raw.index().unwrap().get_path(path, 0).unwrap();
    raw.find_blob(entry.id).unwrap().content().to_vec()
}
fn index_bytes(repo: &Repository) -> Vec<u8> {
    fs::read(raw(repo).path().join("index")).unwrap()
}
fn write(repo: &Repository, path: &Path, bytes: &[u8]) {
    let path = repo.root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
fn stage_body(repo: &Repository, path: &Path, is_lfs: bool) -> Vec<u8> {
    write(repo, path, BODY);
    repo.execute(Operation::Stage(vec![path.into()])).unwrap();
    let bytes = staged(repo, path);
    if is_lfs {
        assert!(
            bytes.starts_with(VERSION),
            "expected LFS pointer for {}",
            path.display()
        );
        let oid = format!("{:x}", Sha256::digest(BODY));
        let expected = format!(
            "{}oid sha256:{oid}\nsize {}\n",
            std::str::from_utf8(VERSION).unwrap(),
            BODY.len()
        );
        assert_eq!(bytes, expected.as_bytes());
        assert_eq!(
            fs::read(
                raw(repo)
                    .commondir()
                    .join("lfs/objects")
                    .join(&oid[..2])
                    .join(&oid[2..4])
                    .join(oid)
            )
            .unwrap(),
            BODY
        );
    } else {
        assert_eq!(bytes, BODY, "unexpected filtering for {}", path.display());
    }
    assert_eq!(fs::read(repo.root.join(path)).unwrap(), BODY);
    bytes
}
fn oracle(repo: &Repository, path: &Path) -> bool {
    let result = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["check-attr", "-z", "filter", "--"])
        .arg(path)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    result.stdout.split(|byte| *byte == 0).nth(2) == Some(b"lfs")
}
fn indexed_attributes(repo: &Repository, path: &Path, bytes: &[u8]) {
    write(repo, path, bytes);
    let raw = raw(repo);
    let mut index = raw.index().unwrap();
    index.add_path(path).unwrap();
    index.write().unwrap();
}

#[test]
fn quoted_patterns_stage_and_hydrate_natively_while_preserving_other_data() {
    let (_directory, repo) = setup();
    fs::write(repo.root.join("unrelated"), b"staged sentinel").unwrap();
    repo.execute(Operation::Stage(vec!["unrelated".into()]))
        .unwrap();
    fs::write(repo.root.join("unrelated"), b"dirty sentinel").unwrap();
    fs::write(
        repo.root.join(".gitattributes"),
        b"\"my file.bin\" filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    let pointer = stage_body(&repo, Path::new("my file.bin"), true);
    assert_eq!(staged(&repo, Path::new("unrelated")), b"staged sentinel");
    fs::write(repo.root.join("my file.bin"), &pointer).unwrap();
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(fs::read(repo.root.join("my file.bin")).unwrap(), BODY);
    assert_eq!(
        fs::read(repo.root.join("unrelated")).unwrap(),
        b"dirty sentinel"
    );
    assert_eq!(staged(&repo, Path::new("my file.bin")), pointer);
}

#[test]
fn c_quoted_patterns_and_globs_match_the_git_oracle() {
    let fixtures: &[(&[u8], &str)] = &[
        (b"\"my file.bin\" filter=lfs\n", "my file.bin"),
        (b"\"my\\tfile.bin\" filter=lfs\n", "my\tfile.bin"),
        (b"\"my\\nfile.bin\" filter=lfs\n", "my\nfile.bin"),
        (b"\"my\\\"file.bin\" filter=lfs\n", "my\"file.bin"),
        (b"\"caf\\303\\251 file.bin\" filter=lfs\n", "café file.bin"),
        (
            b"\"assets/**/*.bin\" filter=lfs\n",
            "assets/deep/my file.bin",
        ),
        (b"\"/my file.bin\" filter=lfs\n", "nested/my file.bin"),
        (b"\"literal[[]file.bin\" filter=lfs\n", "literal[file.bin"),
        (
            b"\xef\xbb\xbf# comment\r\n\"my file.bin\" filter=lfs\r\n",
            "my file.bin",
        ),
        (b"\"bad\\qfile.bin\" filter=lfs\n", "badqfile.bin"),
    ];
    for (rules, path) in fixtures {
        let (_directory, repo) = setup();
        fs::write(repo.root.join(".gitattributes"), rules).unwrap();
        stage_body(&repo, Path::new(path), oracle(&repo, Path::new(path)));
    }
}

#[test]
fn macros_and_filter_states_match_git_including_cycles_and_same_line_order() {
    let cases: &[&[u8]] = &[
        b"[attr]large filter=lfs\n[attr]media large\n\"my file.bin\" media\n",
        b"[attr]a b filter=lfs\n[attr]b a\n\"my file.bin\" a\n",
        b"[attr]large filter=lfs\n\"my file.bin\" large -filter\n",
        b"[attr]large filter=lfs\n\"my file.bin\" -filter large\n",
        b"[attr]large filter=lfs\n\"my file.bin\" -large\n",
        b"[attr]large filter=lfs\n\"my file.bin\" large=value\n",
        b"[attr]binary filter=lfs\n\"my file.bin\" binary\n",
        b"\"my file.bin\" filter=lfs\n\"my file.bin\" !filter\n",
        b"\"my file.bin\" filter=lfs\n\"my file.bin\" filter=other\n",
        b"\"my file.bin\" filter=lfs\n\"my file.bin\" filter=\n",
        b"\"my file.bin\" filter\n",
    ];
    for rules in cases {
        let (_directory, repo) = setup();
        fs::write(repo.root.join(".gitattributes"), rules).unwrap();
        stage_body(
            &repo,
            Path::new("my file.bin"),
            oracle(&repo, Path::new("my file.bin")),
        );
    }
}

#[test]
fn nested_root_global_and_info_rules_resolve_per_attribute_and_macro_precedence() {
    type Sources<'a> = (&'a [u8], &'a [u8], &'a [u8], &'a [u8]);
    let cases: &[Sources<'_>] = &[
        (
            b"*.bin filter=lfs\n",
            b"\"my file.bin\" -filter\n",
            b"",
            b"",
        ),
        (b"*.bin filter=lfs\n", b"\"my file.bin\" -text\n", b"", b""),
        (
            b"*.bin filter=lfs\n",
            b"*.bin -filter\n",
            b"",
            b"\"nested/my file.bin\" filter=lfs\n",
        ),
        (
            b"\"nested/my file.bin\" -filter\n",
            b"",
            b"*.bin filter=lfs\n",
            b"",
        ),
        (
            b"\"nested/my file.bin\" large\n",
            b"",
            b"[attr]large filter=lfs\n",
            b"",
        ),
        (
            b"[attr]large filter=lfs\n*.bin large\n",
            b"[attr]large -filter\n",
            b"",
            b"",
        ),
        (
            b"[attr]large -filter\n*.bin large\n",
            b"",
            b"",
            b"[attr]large filter=lfs\n",
        ),
        (
            b"\"nested/my file.bin\" !filter\n",
            b"",
            b"*.bin filter=lfs\n",
            b"",
        ),
    ];
    for (root, nested, global, info) in cases {
        let (directory, repo) = setup();
        let global_path = directory.path().join("global-attributes");
        fs::write(&global_path, global).unwrap();
        raw(&repo)
            .config()
            .unwrap()
            .set_str("core.attributesfile", global_path.to_str().unwrap())
            .unwrap();
        fs::write(repo.root.join(".gitattributes"), root).unwrap();
        write(&repo, Path::new("nested/.gitattributes"), nested);
        fs::write(raw(&repo).commondir().join("info/attributes"), info).unwrap();
        stage_body(
            &repo,
            Path::new("nested/my file.bin"),
            oracle(&repo, Path::new("nested/my file.bin")),
        );
    }
}

#[test]
fn worktree_attributes_only_fall_back_to_index_when_the_file_is_unavailable() {
    for working in [
        None,
        Some(b"".as_slice()),
        Some(b"*.txt filter=other\n"),
        Some(b"\"my file.bin\" -filter\n"),
    ] {
        let (_directory, repo) = setup();
        indexed_attributes(
            &repo,
            Path::new(".gitattributes"),
            b"\"my file.bin\" filter=lfs\n",
        );
        match working {
            Some(bytes) => fs::write(repo.root.join(".gitattributes"), bytes).unwrap(),
            None => fs::remove_file(repo.root.join(".gitattributes")).unwrap(),
        }
        stage_body(
            &repo,
            Path::new("my file.bin"),
            oracle(&repo, Path::new("my file.bin")),
        );
    }
}

#[test]
fn missing_nested_attributes_read_indexed_regular_or_symlink_blob_bytes() {
    for symlink_mode in [false, true] {
        let (_directory, repo) = setup();
        fs::write(repo.root.join(".gitattributes"), b"*.bin filter=lfs\n").unwrap();
        indexed_attributes(
            &repo,
            Path::new("nested/.gitattributes"),
            b"\"my file.bin\" -filter\n",
        );
        if symlink_mode {
            let mut index = raw(&repo).index().unwrap();
            let mut entry = index
                .get_path(Path::new("nested/.gitattributes"), 0)
                .unwrap();
            entry.mode = 0o120000;
            index.add(&entry).unwrap();
            index.write().unwrap();
        }
        fs::remove_file(repo.root.join("nested/.gitattributes")).unwrap();
        stage_body(
            &repo,
            Path::new("nested/my file.bin"),
            oracle(&repo, Path::new("nested/my file.bin")),
        );
    }
}

#[test]
fn checkout_hydrates_indexed_pointers_using_index_preferred_attributes() {
    let (_directory, repo) = setup();
    indexed_attributes(&repo, Path::new(".gitattributes"), b"*.bin filter=lfs\n");
    let pointer = stage_body(&repo, Path::new("data.bin"), true);
    fs::write(repo.root.join(".gitattributes"), b"*.bin -filter\n").unwrap();
    fs::write(repo.root.join("data.bin"), &pointer).unwrap();
    let before = index_bytes(&repo);
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), BODY);
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        b"*.bin -filter\n"
    );
    assert_eq!(index_bytes(&repo), before);
}

#[test]
fn worktree_only_pointer_hydration_keeps_worktree_preferred_attributes() {
    let (_directory, repo) = setup();
    fs::write(repo.root.join(".gitattributes"), b"*.bin filter=lfs\n").unwrap();
    let pointer = stage_body(&repo, Path::new("held.bin"), true);
    let mut index = raw(&repo).index().unwrap();
    index.remove_path(Path::new("held.bin")).unwrap();
    index.write().unwrap();
    indexed_attributes(&repo, Path::new(".gitattributes"), b"*.bin -filter\n");
    fs::write(
        repo.root.join(".gitattributes"),
        b"\"restored file.bin\" filter=lfs\n",
    )
    .unwrap();
    fs::write(repo.root.join("restored file.bin"), pointer).unwrap();
    let before = index_bytes(&repo);
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(fs::read(repo.root.join("restored file.bin")).unwrap(), BODY);
    assert_eq!(index_bytes(&repo), before);
}

#[test]
fn quoted_attribute_changes_are_observed_between_staging_operations() {
    let (_directory, repo) = setup();
    fs::write(
        repo.root.join(".gitattributes"),
        b"\"my file.bin\" filter=lfs\n",
    )
    .unwrap();
    stage_body(&repo, Path::new("my file.bin"), true);
    fs::write(
        repo.root.join(".gitattributes"),
        b"\"my file.bin\" -filter\n",
    )
    .unwrap();
    stage_body(&repo, Path::new("my file.bin"), false);
}

#[test]
fn quoted_macro_rules_apply_to_public_conflict_resolution() {
    let (_directory, repo) = setup();
    fs::write(
        repo.root.join(".gitattributes"),
        b"[attr]large filter=lfs\n\"my file.bin\" large\n",
    )
    .unwrap();
    fs::write(repo.root.join("my file.bin"), b"base").unwrap();
    // Construct an actual conflicted index using independent pointer blobs.
    let raw = raw(&repo);
    let mut index = raw.index().unwrap();
    let template = index.get_path(Path::new("base"), 0).unwrap();
    for stage in 1..=3 {
        index
            .add(&git2::IndexEntry {
                ctime: template.ctime,
                mtime: template.mtime,
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: 4,
                id: raw.blob(b"side").unwrap(),
                flags: stage << 12,
                flags_extended: 0,
                path: b"my file.bin".to_vec(),
            })
            .unwrap();
    }
    index.write().unwrap();
    let context = Arc::new(repo.conflict_context(Path::new("my file.bin")).unwrap());
    repo.execute(Operation::ResolveConflict {
        context,
        resolution: ConflictResolution::Edited("combined original".into()),
    })
    .unwrap();
    assert!(staged(&repo, Path::new("my file.bin")).starts_with(VERSION));
    assert_eq!(
        fs::read(repo.root.join("my file.bin")).unwrap(),
        b"combined original"
    );
}

#[test]
fn attributes_case_matching_respects_the_repository_setting() {
    for fold in [false, true] {
        let (_directory, repo) = setup();
        raw(&repo)
            .config()
            .unwrap()
            .set_bool("core.ignorecase", fold)
            .unwrap();
        fs::write(
            repo.root.join(".gitattributes"),
            b"\"MY FILE.BIN\" filter=lfs\n",
        )
        .unwrap();
        stage_body(
            &repo,
            Path::new("my file.bin"),
            oracle(&repo, Path::new("my file.bin")),
        );
    }
}

#[cfg(unix)]
#[test]
fn worktree_attribute_symlinks_use_index_fallback_without_reading_external_rules() {
    let (directory, repo) = setup();
    indexed_attributes(
        &repo,
        Path::new(".gitattributes"),
        b"\"my file.bin\" filter=lfs\n",
    );
    fs::remove_file(repo.root.join(".gitattributes")).unwrap();
    let external = directory.path().join("external");
    fs::write(&external, b"\"my file.bin\" -filter\n").unwrap();
    std::os::unix::fs::symlink(&external, repo.root.join(".gitattributes")).unwrap();
    stage_body(&repo, Path::new("my file.bin"), true);
    assert_eq!(fs::read(external).unwrap(), b"\"my file.bin\" -filter\n");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn quoted_octal_patterns_preserve_non_utf8_git_paths() {
    use std::os::unix::ffi::OsStrExt;
    let (_directory, repo) = setup();
    fs::write(
        repo.root.join(".gitattributes"),
        b"\"nonutf8-\\377.bin\" filter=lfs\n",
    )
    .unwrap();
    let path = Path::new(std::ffi::OsStr::from_bytes(b"nonutf8-\xff.bin"));
    stage_body(&repo, path, oracle(&repo, path));
}

#[test]
fn relative_global_attribute_paths_are_resolved_from_the_repository_root() {
    let (_directory, repo) = setup();
    fs::write(
        repo.root.join("global-attributes"),
        b"\"my file.bin\" filter=lfs\n",
    )
    .unwrap();
    raw(&repo)
        .config()
        .unwrap()
        .set_str("core.attributesfile", "global-attributes")
        .unwrap();
    stage_body(&repo, Path::new("my file.bin"), true);
}

#[test]
fn attribute_io_errors_preserve_the_index_and_working_content() {
    let (_directory, repo) = setup();
    fs::write(
        repo.root.join(".gitattributes"),
        b"\"my file.bin\" filter=lfs\n",
    )
    .unwrap();
    raw(&repo)
        .config()
        .unwrap()
        .set_str("core.attributesfile", "/dev/null/invalid-attribute-source")
        .unwrap();
    write(&repo, Path::new("my file.bin"), BODY);
    let before = index_bytes(&repo);
    assert!(
        repo.execute(Operation::Stage(vec!["my file.bin".into()]))
            .is_err()
    );
    assert_eq!(index_bytes(&repo), before);
    assert_eq!(fs::read(repo.root.join("my file.bin")).unwrap(), BODY);
}

#[test]
fn default_global_sources_and_lookup_work_without_a_git_process() {
    if let Some(home) = std::env::var_os("GITBUDDY_LFS_ATTRIBUTES_CHILD") {
        let (_directory, repo) = setup();
        raw(&repo)
            .config()
            .unwrap()
            .remove("core.attributesfile")
            .unwrap();
        stage_body(&repo, Path::new("my file.bin"), true);
        raw(&repo)
            .config()
            .unwrap()
            .set_str("core.attributesfile", "")
            .unwrap();
        stage_body(&repo, Path::new("my file.bin"), false);
        assert!(!PathBuf::from(home).join("git-was-run").exists());
        return;
    }
    for xdg in [None, Some(""), Some("xdg")] {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let source = if xdg == Some("xdg") {
            home.join("xdg/git/attributes")
        } else {
            home.join(".config/git/attributes")
        };
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(source, b"\"my file.bin\" filter=lfs\n").unwrap();
        let bin = home.join("bin");
        fs::create_dir(&bin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let git = bin.join("git");
            fs::write(
                &git,
                b"#!/bin/sh\n: > \"$GITBUDDY_LFS_ATTRIBUTES_CHILD/git-was-run\"\nexit 99\n",
            )
            .unwrap();
            fs::set_permissions(git, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "default_global_sources_and_lookup_work_without_a_git_process",
                "--nocapture",
            ])
            .env("GITBUDDY_LFS_ATTRIBUTES_CHILD", home)
            .env("HOME", home)
            .env("PATH", &bin)
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        match xdg {
            None => {
                command.env_remove("XDG_CONFIG_HOME");
            }
            Some("") => {
                command.env("XDG_CONFIG_HOME", "");
            }
            Some(_) => {
                command.env("XDG_CONFIG_HOME", home.join("xdg"));
            }
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!home.join("git-was-run").exists());
    }
}

#[test]
fn system_attribute_search_paths_use_native_order_and_honor_nosystem() {
    if let Some(paths) = std::env::var_os("GITBUDDY_LFS_SYSTEM_PATHS") {
        // This isolated child runs only this test, and owns its process-local
        // libgit2 settings before opening a repository. No system file changes.
        unsafe {
            git2::opts::set_search_path(git2::ConfigLevel::System, Path::new(&paths)).unwrap();
        }
        let (_directory, repo) = setup();
        let expected = std::env::var("GITBUDDY_LFS_SYSTEM_EXPECT").unwrap() == "true";
        stage_body(&repo, Path::new("my file.bin"), expected);
        return;
    }
    for (empty_first, disable, expected) in [
        (false, false, true),
        (true, false, false),
        (false, true, false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        if empty_first {
            fs::write(first.join("gitattributes"), b"").unwrap();
        }
        fs::write(
            second.join("gitattributes"),
            b"\"my file.bin\" filter=lfs\n",
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "system_attribute_search_paths_use_native_order_and_honor_nosystem",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(
                "GITBUDDY_LFS_SYSTEM_PATHS",
                std::env::join_paths([first, second]).unwrap(),
            )
            .env(
                "GITBUDDY_LFS_SYSTEM_EXPECT",
                if expected { "true" } else { "false" },
            )
            .env("GIT_ATTR_NOSYSTEM", if disable { "1" } else { "0" })
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn quoted_macros_apply_the_official_extension_pipeline_before_staging_and_hydration() {
    let downloaded = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs");
    let tool = std::env::var_os("GITBUDDY_TEST_LFS_PROGRAM")
        .map(PathBuf::from)
        .or_else(|| downloaded.is_file().then_some(downloaded))
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|directory| directory.join("git-lfs"))
                .find(|candidate| candidate.is_file())
        });
    let Some(tool) = tool else {
        eprintln!("official git-lfs unavailable; skipping local extension fixture");
        return;
    };
    let (_directory, repo) = setup();
    let raw = raw(&repo);
    let mut config = raw.config().unwrap();
    config
        .set_str("gitbuddy.lfsProgram", tool.to_str().unwrap())
        .unwrap();
    config
        .set_str("lfs.extension.upper.clean", "/usr/bin/tr a-z A-Z")
        .unwrap();
    config
        .set_str("lfs.extension.upper.smudge", "/usr/bin/tr A-Z a-z")
        .unwrap();
    config.set_i64("lfs.extension.upper.priority", 0).unwrap();
    fs::write(
        repo.root.join(".gitattributes"),
        b"[attr]large filter=lfs -text\n\"my file.bin\" large\n",
    )
    .unwrap();
    let original = b"lower case content";
    let canonical = b"LOWER CASE CONTENT";
    fs::write(repo.root.join("my file.bin"), original).unwrap();
    repo.execute(Operation::Stage(vec!["my file.bin".into()]))
        .unwrap();
    let oid = format!("{:x}", Sha256::digest(canonical));
    let original_oid = format!("{:x}", Sha256::digest(original));
    let text = format!(
        "{}ext-0-upper sha256:{original_oid}\noid sha256:{oid}\nsize {}\n",
        std::str::from_utf8(VERSION).unwrap(),
        canonical.len()
    );
    assert_eq!(staged(&repo, Path::new("my file.bin")), text.as_bytes());
    assert_eq!(
        fs::read(
            raw.commondir()
                .join("lfs/objects")
                .join(&oid[..2])
                .join(&oid[2..4])
                .join(oid)
        )
        .unwrap(),
        canonical
    );
    fs::write(repo.root.join("my file.bin"), text).unwrap();
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(fs::read(repo.root.join("my file.bin")).unwrap(), original);
}

#[test]
fn track_authors_git_c_quoted_globs_and_untrack_preserves_indexed_content() {
    let fixtures = [
        ("*.bin", "*.bin", "data.bin"),
        ("my file.bin", "\"my file.bin\"", "my file.bin"),
        ("my\"*.bin", "\"my\\\"*.bin\"", "my\"file.bin"),
        (r"back\\*.bin", r#""back\\\\*.bin""#, r"back\file.bin"),
        ("tab\t*.bin", "\"tab\\t*.bin\"", "tab\tfile.bin"),
        ("line\n*.bin", "\"line\\n*.bin\"", "line\nfile.bin"),
        (
            "carriage\r*.bin",
            "\"carriage\\r*.bin\"",
            "carriage\rfile.bin",
        ),
        (
            "control\u{1}*.bin",
            "\"control\\001*.bin\"",
            "control\u{1}file.bin",
        ),
        ("café *.bin", "\"caf\\303\\251 *.bin\"", "café file.bin"),
        (
            " leading *.bin ",
            "\" leading *.bin \"",
            " leading file.bin ",
        ),
        ("#my*.bin", "\"#my*.bin\"", "#myfile.bin"),
        ("-my*.bin", "-my*.bin", "-myfile.bin"),
        (r"\!my*.bin", r#""\\!my*.bin""#, "!myfile.bin"),
    ];
    for (pattern, token, path) in fixtures {
        let (_directory, repo) = setup();
        let original = b"# unrelated comment\r\n*.txt text\r\n";
        fs::write(repo.root.join(".gitattributes"), original).unwrap();
        let head = raw(&repo).head().unwrap().target();
        let before = index_bytes(&repo);
        for _ in 0..2 {
            repo.execute(Operation::LfsTrack {
                pattern: pattern.into(),
                track: true,
            })
            .unwrap();
        }
        let authored = [
            original.as_slice(),
            format!("{token} filter=lfs diff=lfs merge=lfs -text\n").as_bytes(),
        ]
        .concat();
        assert_eq!(
            fs::read(repo.root.join(".gitattributes")).unwrap(),
            authored
        );
        assert_eq!(
            index_bytes(&repo),
            before,
            "authoring must not stage anything"
        );
        assert!(oracle(&repo, Path::new(path)));
        stage_body(&repo, Path::new(path), true);
        let pointer_index = index_bytes(&repo);
        repo.execute(Operation::LfsTrack {
            pattern: pattern.into(),
            track: false,
        })
        .unwrap();
        assert_eq!(
            fs::read(repo.root.join(".gitattributes")).unwrap(),
            original
        );
        assert_eq!(index_bytes(&repo), pointer_index);
        assert_eq!(raw(&repo).head().unwrap().target(), head);
        assert_eq!(fs::read(repo.root.join(path)).unwrap(), BODY);
        stage_body(&repo, Path::new(path), false);
    }
}

#[test]
fn authoring_preserves_opaque_custom_rules_and_untrack_only_removes_generated_assignments() {
    let (_directory, repo) = setup();
    let custom = b"# opaque \xff\r\n\"my file.bin\" filter=lfs diff=lfs merge=lfs -text custom=value\r\n\"my file.bin\" diff=lfs filter=lfs merge=lfs -text\r\n\"my file.bin\" filter=lfs\r\n*.txt filter=lfs diff=lfs merge=lfs -text\r\n# no final newline";
    fs::write(repo.root.join(".gitattributes"), custom).unwrap();
    repo.execute(Operation::LfsTrack {
        pattern: "my file.bin".into(),
        track: true,
    })
    .unwrap();
    let authored = [
        custom.as_slice(),
        b"\n\"my file.bin\" filter=lfs diff=lfs merge=lfs -text\n",
    ]
    .concat();
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        authored
    );
    repo.execute(Operation::LfsTrack {
        pattern: "my file.bin".into(),
        track: false,
    })
    .unwrap();
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        [custom.as_slice(), b"\n"].concat()
    );
    // Custom user rules still request LFS after the generated rule is removed.
    stage_body(&repo, Path::new("my file.bin"), true);
}

#[test]
fn invalid_or_oversized_authoring_patterns_preserve_all_repository_data() {
    let (_directory, repo) = setup();
    let original = b"# attributes sentinel\r\n*.txt text\r\n";
    fs::write(repo.root.join(".gitattributes"), original).unwrap();
    fs::write(repo.root.join("working sentinel"), BODY).unwrap();
    let before = index_bytes(&repo);
    let head = raw(&repo).head().unwrap().target();
    for pattern in [
        "".into(),
        "   ".into(),
        "!*.bin".into(),
        "[attr]large".into(),
        "a\0b".into(),
        "*".repeat(2048),
        "\u{1}".repeat(600),
    ] {
        for track in [false, true] {
            assert!(
                repo.execute(Operation::LfsTrack {
                    pattern: pattern.clone(),
                    track
                })
                .is_err()
            );
            assert_eq!(
                fs::read(repo.root.join(".gitattributes")).unwrap(),
                original
            );
            assert_eq!(index_bytes(&repo), before);
            assert_eq!(raw(&repo).head().unwrap().target(), head);
            assert_eq!(fs::read(repo.root.join("working sentinel")).unwrap(), BODY);
        }
    }
}

#[test]
fn authored_rule_length_respects_gits_2048_byte_line_limit() {
    let (_directory, repo) = setup();
    let suffix = " filter=lfs diff=lfs merge=lfs -text";
    let pattern = "*".repeat(2047 - suffix.len());
    repo.execute(Operation::LfsTrack {
        pattern: pattern.clone(),
        track: true,
    })
    .unwrap();
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap().len(),
        2048
    );
    assert!(oracle(&repo, Path::new("data.bin")));
    stage_body(&repo, Path::new("data.bin"), true);
    let before = fs::read(repo.root.join(".gitattributes")).unwrap();
    assert!(
        repo.execute(Operation::LfsTrack {
            pattern: format!("{pattern}*"),
            track: true
        })
        .is_err()
    );
    assert_eq!(fs::read(repo.root.join(".gitattributes")).unwrap(), before);
}

#[test]
fn untrack_preserves_unmatched_files_and_removes_legacy_quoted_simple_rules() {
    let (_directory, repo) = setup();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: false,
    })
    .unwrap();
    assert!(!repo.root.join(".gitattributes").exists());
    let original = b"# unchanged\r\n*.txt text";
    fs::write(repo.root.join(".gitattributes"), original).unwrap();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: false,
    })
    .unwrap();
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        original
    );
    fs::write(repo.root.join(".gitattributes"), b"# keep\r\n*.bin filter=lfs diff=lfs merge=lfs -text\r\n\"*.bin\" filter=lfs diff=lfs merge=lfs -text\n").unwrap();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: false,
    })
    .unwrap();
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        b"# keep\r\n"
    );
}

#[cfg(unix)]
fn routing_probe(mode: &str, test_name: &str) {
    if let Some(root) = std::env::var_os("GITBUDDY_LFS_ROUTE_CHILD") {
        let repo = Repository::open(Path::new(&root)).unwrap();
        let before = index_bytes(&repo);
        if mode == "fetch" {
            repo.execute(Operation::LfsFetch("origin".into())).unwrap();
        } else {
            assert!(
                repo.execute(Operation::Stage(vec!["probe.bin".into()]))
                    .is_err()
            );
            assert_eq!(fs::read(repo.root.join("probe.bin")).unwrap(), BODY);
        }
        assert_eq!(index_bytes(&repo), before);
        let report = fs::read(std::env::var_os("GITBUDDY_LFS_ROUTE_REPORT").unwrap()).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&report).unwrap();
        let correct = raw(&repo);
        assert_eq!(
            fs::canonicalize(report["actual_top"].as_str().unwrap()).unwrap(),
            repo.root
        );
        if mode == "fetch" {
            assert_eq!(
                fs::canonicalize(report["GIT_DIR"].as_str().unwrap()).unwrap(),
                fs::canonicalize(correct.path()).unwrap()
            );
        } else {
            let isolated = Path::new(report["GIT_DIR"].as_str().unwrap());
            assert!(isolated.starts_with(correct.commondir()));
            assert_ne!(isolated, correct.path());
        }
        assert_eq!(
            fs::canonicalize(report["GIT_WORK_TREE"].as_str().unwrap()).unwrap(),
            repo.root
        );
        for key in [
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
        ] {
            assert!(
                report[key].is_null(),
                "inherited routing override survived: {key}"
            );
        }
        assert_eq!(report["GIT_CONFIG_KEY_0"], "gitbuddy.routeProbe");
        assert_eq!(report["GIT_CONFIG_VALUE_0"], "preserved");
        assert_eq!(
            report["GIT_CONFIG_COUNT"],
            if mode == "fetch" { "1" } else { "5" }
        );
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let (directory, repo) = setup();
    let (_other_directory, other) = setup();
    let other_raw = raw(&other);
    let other_index = index_bytes(&other);
    let program = directory.path().join("fake-lfs.py");
    fs::write(&program, r#"#!/usr/bin/env python3
import json,os,subprocess,sys
keys=['GIT_DIR','GIT_WORK_TREE','GIT_INDEX_FILE','GIT_COMMON_DIR','GIT_OBJECT_DIRECTORY','GIT_ALTERNATE_OBJECT_DIRECTORIES','GIT_NAMESPACE','GIT_CONFIG_COUNT','GIT_CONFIG_KEY_0','GIT_CONFIG_VALUE_0']
report={key:os.environ.get(key) for key in keys}
report['actual_top']=subprocess.check_output(['git','rev-parse','--show-toplevel'],text=True).strip()
with open(os.environ['GITBUDDY_LFS_ROUTE_REPORT'],'w') as out: json.dump(report,out)
if sys.argv[1]=='clean':
 sys.stderr.write('intentional local filter probe refusal')
 sys.exit(19)
"#).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let correct = raw(&repo);
    correct
        .config()
        .unwrap()
        .set_str("gitbuddy.lfsProgram", program.to_str().unwrap())
        .unwrap();
    correct
        .remote("origin", "file:///unused-lfs-routing-fixture")
        .unwrap();
    if mode == "clean" {
        let mut config = correct.config().unwrap();
        config
            .set_str("lfs.extension.probe.clean", "unused-clean")
            .unwrap();
        config
            .set_str("lfs.extension.probe.smudge", "unused-smudge")
            .unwrap();
        config.set_i64("lfs.extension.probe.priority", 0).unwrap();
        fs::write(repo.root.join(".gitattributes"), b"*.bin filter=lfs\n").unwrap();
        fs::write(repo.root.join("probe.bin"), BODY).unwrap();
    }
    let before = index_bytes(&repo);
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env("GITBUDDY_LFS_ROUTE_CHILD", &repo.root)
        .env(
            "GITBUDDY_LFS_ROUTE_REPORT",
            directory.path().join("routing-report.json"),
        )
        .env("GIT_DIR", other_raw.path())
        .env("GIT_WORK_TREE", &other.root)
        .env("GIT_COMMON_DIR", other_raw.commondir())
        .env("GIT_INDEX_FILE", other_raw.path().join("index"))
        .env(
            "GIT_OBJECT_DIRECTORY",
            other_raw.commondir().join("objects"),
        )
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            other_raw.commondir().join("objects"),
        )
        .env("GIT_NAMESPACE", "poison-namespace")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "gitbuddy.routeProbe")
        .env("GIT_CONFIG_VALUE_0", "preserved")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(index_bytes(&repo), before);
    assert_eq!(index_bytes(&other), other_index);
}

#[cfg(unix)]
#[test]
fn lfs_transport_children_do_not_inherit_another_repositorys_routing() {
    routing_probe(
        "fetch",
        "lfs_transport_children_do_not_inherit_another_repositorys_routing",
    );
}

#[cfg(unix)]
#[test]
fn lfs_filter_children_do_not_inherit_another_repositorys_routing() {
    routing_probe(
        "clean",
        "lfs_filter_children_do_not_inherit_another_repositorys_routing",
    );
}
