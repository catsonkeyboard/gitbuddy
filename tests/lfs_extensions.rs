use gitbuddy::git::{ConflictResolution, NetworkControl, Operation, Repository};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn setup() -> (tempfile::TempDir, Repository) {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(&directory.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    fs::write(repo.root.join("base"), "base").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("initial".into())).unwrap();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    (directory, repo)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn pointer(original: &[u8], canonical: &[u8]) -> String {
    format!(
        "version https://git-lfs.github.com/spec/v1\next-0-reverse sha256:{}\noid sha256:{}\nsize {}\n",
        digest(original),
        digest(canonical),
        canonical.len()
    )
}

fn cache(repo: &Repository, canonical: &[u8]) -> PathBuf {
    let oid = digest(canonical);
    git2::Repository::open(&repo.root)
        .unwrap()
        .commondir()
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}

fn staged(repo: &Repository, path: &str) -> Vec<u8> {
    let raw = git2::Repository::open(&repo.root).unwrap();
    let entry = raw.index().unwrap().get_path(path.as_ref(), 0).unwrap();
    raw.find_blob(entry.id).unwrap().content().to_vec()
}

fn official_tool() -> Option<PathBuf> {
    if let Some(tool) = std::env::var_os("GITBUDDY_TEST_LFS_PROGRAM") {
        let tool = PathBuf::from(tool);
        assert!(
            tool.is_file(),
            "GITBUDDY_TEST_LFS_PROGRAM must identify the official git-lfs binary"
        );
        return Some(tool);
    }
    let downloaded = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs");
    if downloaded.is_file() {
        return Some(downloaded);
    }
    let installed = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| {
            directory.join(if cfg!(windows) {
                "git-lfs.exe"
            } else {
                "git-lfs"
            })
        })
        .find(|candidate| candidate.is_file());
    if installed.is_none() {
        eprintln!(
            "official git-lfs unavailable; set GITBUDDY_TEST_LFS_PROGRAM to run the local extension fixture"
        );
    }
    installed
}

fn configure_extension(repo: &Repository, directory: &std::path::Path) -> Option<PathBuf> {
    let tool = official_tool()?;
    let processor = directory.join("local-transform.py");
    fs::write(
        &processor,
        "import sys\ndata=sys.stdin.buffer.read()\nif sys.argv[1]=='clean':\n sys.stdout.buffer.write(b'EXT:'+data[::-1])\nelse:\n assert data.startswith(b'EXT:')\n sys.stdout.buffer.write(data[4:][::-1])\n",
    )
    .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config
        .set_str("gitbuddy.lfsProgram", tool.to_str().unwrap())
        .unwrap();
    config.set_i64("lfs.extension.reverse.priority", 0).unwrap();
    for operation in ["clean", "smudge"] {
        config
            .set_str(
                &format!("lfs.extension.reverse.{operation}"),
                &format!("python3 {} {operation}", processor.display()),
            )
            .unwrap();
    }
    Some(processor)
}

#[test]
fn existing_extension_pointer_preserves_metadata_and_protects_canonical_object_without_tool() {
    let (_directory, repo) = setup();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("gitbuddy.lfsProgram", "/unavailable/git-lfs")
        .unwrap();
    let canonical = b"EXT:lanigiro";
    let text = pointer(b"original", canonical);
    let object = cache(&repo, canonical);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(&object, canonical).unwrap();
    fs::write(repo.root.join("existing.bin"), &text).unwrap();
    repo.execute(Operation::Stage(vec!["existing.bin".into()]))
        .unwrap();
    assert_eq!(staged(&repo, "existing.bin"), text.as_bytes());
    let preview = repo.lfs_maintenance(NetworkControl::default()).unwrap();
    assert_eq!(preview.protected, 1);
    assert!(preview.candidates.is_empty() && preview.corrupt.is_empty());
    assert!(repo.execute(Operation::LfsCheckout).is_err());
    assert_eq!(
        fs::read(repo.root.join("existing.bin")).unwrap(),
        text.as_bytes()
    );
    assert_eq!(fs::read(object).unwrap(), canonical);
}

#[test]
fn official_extension_round_trip_streams_canonical_cache_and_original_content() {
    let (directory, repo) = setup();
    let Some(_) = configure_extension(&repo, directory.path()) else {
        return;
    };
    // Larger than the bounded diagnostic capture; smudge must stream to disk.
    let original = b"original binary content\0".repeat(100_000);
    let canonical = [
        b"EXT:".as_slice(),
        original
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>()
            .as_slice(),
    ]
    .concat();
    let text = pointer(&original, &canonical);
    fs::write(repo.root.join("unrelated"), "keep staged bytes").unwrap();
    repo.execute(Operation::Stage(vec!["unrelated".into()]))
        .unwrap();
    let unrelated = staged(&repo, "unrelated");
    fs::write(repo.root.join("data.bin"), &original).unwrap();
    repo.execute(Operation::Stage(vec!["data.bin".into()]))
        .unwrap();
    assert_eq!(staged(&repo, "data.bin"), text.as_bytes());
    assert_eq!(staged(&repo, "unrelated"), unrelated);
    assert_eq!(fs::read(cache(&repo, &canonical)).unwrap(), canonical);
    assert!(!cache(&repo, &original).exists());
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), original);
    assert!(
        !repo
            .snapshot(10)
            .unwrap()
            .files
            .iter()
            .any(|change| change.path == Path::new("data.bin") && change.unstaged())
    );
    fs::write(repo.root.join("data.bin"), &text).unwrap();
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), original);
    assert_eq!(staged(&repo, "data.bin"), text.as_bytes());
    let preview = repo.lfs_maintenance(NetworkControl::default()).unwrap();
    assert_eq!(preview.protected, 1);
    assert!(preview.candidates.is_empty() && preview.corrupt.is_empty());
}

#[test]
fn unavailable_extension_processor_refuses_raw_staging_without_index_or_worktree_loss() {
    let (directory, repo) = setup();
    let Some(processor) = configure_extension(&repo, directory.path()) else {
        return;
    };
    fs::remove_file(processor).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let before = fs::read(raw.path().join("index")).unwrap();
    let original = b"must not be silently stored without transformation";
    fs::write(repo.root.join("data.bin"), original).unwrap();
    assert!(
        repo.execute(Operation::Stage(vec!["data.bin".into()]))
            .is_err()
    );
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), before);
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), original);
    assert!(!cache(&repo, original).exists());
}

#[test]
fn malformed_extension_metadata_never_becomes_cached_file_content() {
    let (_directory, repo) = setup();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let before = fs::read(raw.path().join("index")).unwrap();
    let valid = pointer(b"original", b"canonical");
    let extension = valid.lines().nth(1).unwrap();
    for malformed in [
        valid.replacen("sha256:", "sha1:", 1),
        valid.replacen(extension, &format!("{extension}\n{extension}"), 1),
        valid.replacen("ext-0-reverse", "ext-10-reverse", 1),
        valid.replacen("ext-0-reverse", "ext-0-", 1),
        valid.replacen("ext-0-reverse", "ext-0-.", 1),
    ] {
        fs::write(repo.root.join("bad.bin"), &malformed).unwrap();
        assert!(
            repo.execute(Operation::Stage(vec!["bad.bin".into()]))
                .is_err()
        );
        assert_eq!(fs::read(raw.path().join("index")).unwrap(), before);
        assert_eq!(
            fs::read(repo.root.join("bad.bin")).unwrap(),
            malformed.as_bytes()
        );
    }
    assert!(!raw.commondir().join("lfs/objects").exists());
}

#[test]
fn editing_an_extension_file_without_its_processor_never_falls_back_to_native_clean() {
    let (_directory, repo) = setup();
    let text = pointer(b"old original", b"old canonical");
    fs::write(repo.root.join("data.bin"), &text).unwrap();
    repo.execute(Operation::Stage(vec!["data.bin".into()]))
        .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let before = fs::read(raw.path().join("index")).unwrap();
    let edited = b"edited original content";
    fs::write(repo.root.join("data.bin"), edited).unwrap();
    assert!(
        repo.execute(Operation::Stage(vec!["data.bin".into()]))
            .is_err()
    );
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), before);
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), edited);
    assert!(!cache(&repo, edited).exists());
}

#[test]
fn edited_conflict_result_requires_processors_from_every_extension_index_stage() {
    let (_directory, repo) = setup();
    let text = pointer(b"old original", b"old canonical");
    fs::write(repo.root.join("data.bin"), &text).unwrap();
    repo.execute(Operation::Stage(vec!["data.bin".into()]))
        .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut index = raw.index().unwrap();
    let mut entry = index.get_path("data.bin".as_ref(), 0).unwrap();
    index.remove_path("data.bin".as_ref()).unwrap();
    for stage in 1..=3 {
        entry.flags = (entry.flags & !(3 << 12)) | (stage << 12);
        index.add(&entry).unwrap();
    }
    index.write().unwrap();
    let before = fs::read(raw.path().join("index")).unwrap();
    let context = std::sync::Arc::new(repo.conflict_context("data.bin".as_ref()).unwrap());
    let result = repo.execute(Operation::ResolveConflict {
        context,
        resolution: ConflictResolution::Edited("combined original content".into()),
    });
    assert!(
        result.is_err(),
        "extension processors must not be lost when stage 0 is absent"
    );
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), before);
    assert_eq!(
        fs::read(repo.root.join("data.bin")).unwrap(),
        text.as_bytes()
    );
    assert!(!cache(&repo, b"combined original content").exists());
}

#[test]
fn multiple_extension_original_oid_chain_and_custom_storage_round_trip() {
    let (directory, repo) = setup();
    let Some(processor) = configure_extension(&repo, directory.path()) else {
        return;
    };
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    let storage = directory.path().join("custom-lfs");
    config
        .set_str("lfs.storage", storage.to_str().unwrap())
        .unwrap();
    config.set_i64("lfs.extension.reverse.priority", 2).unwrap();
    config.set_i64("lfs.extension.second.priority", 5).unwrap();
    for operation in ["clean", "smudge"] {
        config
            .set_str(
                &format!("lfs.extension.second.{operation}"),
                &format!("python3 {} {operation}", processor.display()),
            )
            .unwrap();
    }
    let original = b"original";
    let first = b"EXT:lanigiro";
    let canonical = b"EXT:original:TXE";
    let text = format!(
        "version https://git-lfs.github.com/spec/v1\next-0-reverse sha256:{}\next-1-second sha256:{}\noid sha256:{}\nsize 16\n",
        digest(original),
        digest(first),
        digest(canonical)
    );
    fs::write(repo.root.join("file with spaces.bin"), original).unwrap();
    repo.execute(Operation::Stage(vec!["file with spaces.bin".into()]))
        .unwrap();
    assert_eq!(staged(&repo, "file with spaces.bin"), text.as_bytes());
    let oid = digest(canonical);
    assert_eq!(
        fs::read(
            storage
                .join("objects")
                .join(&oid[..2])
                .join(&oid[2..4])
                .join(oid)
        )
        .unwrap(),
        canonical
    );
    assert!(!raw.commondir().join("lfs/objects").exists());
    fs::write(repo.root.join("file with spaces.bin"), &text).unwrap();
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(
        fs::read(repo.root.join("file with spaces.bin")).unwrap(),
        original
    );
    assert!(
        !repo
            .snapshot(10)
            .unwrap()
            .files
            .iter()
            .any(|change| change.path == Path::new("file with spaces.bin") && change.unstaged())
    );
}

#[test]
fn wrong_extension_original_oid_never_overwrites_the_working_pointer() {
    let (directory, repo) = setup();
    let Some(_) = configure_extension(&repo, directory.path()) else {
        return;
    };
    let original = b"original";
    let canonical = b"EXT:lanigiro";
    let text = pointer(original, canonical);
    fs::write(repo.root.join("data.bin"), original).unwrap();
    repo.execute(Operation::Stage(vec!["data.bin".into()]))
        .unwrap();
    let wrong = text.replacen(&digest(original), &digest(b"different original"), 1);
    fs::write(repo.root.join("data.bin"), &wrong).unwrap();
    repo.execute(Operation::Stage(vec!["data.bin".into()]))
        .unwrap();
    assert!(repo.execute(Operation::LfsCheckout).is_err());
    assert_eq!(
        fs::read(repo.root.join("data.bin")).unwrap(),
        wrong.as_bytes()
    );
    assert_eq!(fs::read(cache(&repo, canonical)).unwrap(), canonical);
}

#[test]
fn smudge_preserves_a_working_pointer_edited_while_the_processor_runs() {
    let (directory, repo) = setup();
    let Some(processor) = configure_extension(&repo, directory.path()) else {
        return;
    };
    let original = b"original";
    let text = pointer(original, b"EXT:lanigiro");
    fs::write(repo.root.join("data.bin"), original).unwrap();
    repo.execute(Operation::Stage(vec!["data.bin".into()]))
        .unwrap();
    fs::write(repo.root.join("data.bin"), &text).unwrap();
    fs::write(processor,
        "import sys,pathlib\ndata=sys.stdin.buffer.read()\npathlib.Path('data.bin').write_bytes(b'concurrent edit')\nsys.stdout.buffer.write(data[4:][::-1])\n"
    ).unwrap();
    assert!(repo.execute(Operation::LfsCheckout).is_err());
    assert_eq!(
        fs::read(repo.root.join("data.bin")).unwrap(),
        b"concurrent edit"
    );
    assert_eq!(staged(&repo, "data.bin"), text.as_bytes());
}

#[test]
fn delegated_canonical_output_obeys_the_configured_byte_limit() {
    let (directory, repo) = setup();
    let Some(_) = configure_extension(&repo, directory.path()) else {
        return;
    };
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_i64("gitbuddy.lfsFilterMaxBytes", 8)
        .unwrap();
    let before = fs::read(raw.path().join("index")).unwrap();
    fs::write(repo.root.join("data.bin"), b"original").unwrap();
    assert!(
        repo.execute(Operation::Stage(vec!["data.bin".into()]))
            .is_err()
    );
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), before);
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), b"original");
    assert!(!cache(&repo, b"EXT:lanigiro").exists());
}

#[cfg(unix)]
#[test]
fn official_clean_cannot_write_through_a_symlink_in_the_real_cache() {
    let (directory, repo) = setup();
    let Some(_) = configure_extension(&repo, directory.path()) else {
        return;
    };
    let original = b"original";
    let canonical = b"EXT:lanigiro";
    let object = cache(&repo, canonical);
    let objects = object.parent().unwrap().parent().unwrap().parent().unwrap();
    fs::create_dir_all(objects).unwrap();
    let external = directory.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("keep"), b"preserved").unwrap();
    std::os::unix::fs::symlink(&external, object.parent().unwrap().parent().unwrap()).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let before = fs::read(raw.path().join("index")).unwrap();
    fs::write(repo.root.join("data.bin"), original).unwrap();
    assert!(
        repo.execute(Operation::Stage(vec!["data.bin".into()]))
            .is_err()
    );
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), before);
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), original);
    assert_eq!(fs::read(external.join("keep")).unwrap(), b"preserved");
    assert_eq!(fs::read_dir(external).unwrap().count(), 1);
}
