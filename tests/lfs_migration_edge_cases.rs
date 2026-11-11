use gitbuddy::git::{NetworkControl, Operation, Repository};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

fn tool() -> PathBuf {
    let path = std::env::var_os("GITBUDDY_TEST_GIT_LFS")
        .or_else(|| std::env::var_os("GITBUDDY_TEST_LFS_PROGRAM"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs")
        });
    assert!(
        path.is_file(),
        "Set GITBUDDY_TEST_GIT_LFS to the official git-lfs binary"
    );
    path
}

fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn setup() -> (tempfile::TempDir, Repository) {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(&directory.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Migration review".into(),
        "migration-review@example.invalid".into(),
    ))
    .unwrap();
    let mut config = raw(&repo).config().unwrap();
    config.set_str("core.attributesfile", "").unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config
        .set_str("gitbuddy.lfsProgram", tool().to_str().unwrap())
        .unwrap();
    (directory, repo)
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.resolve_revision("HEAD").unwrap().id
}
fn index(repo: &Repository) -> Vec<u8> {
    fs::read(raw(repo).path().join("index")).unwrap()
}
fn config(repo: &Repository) -> Vec<u8> {
    fs::read(raw(repo).commondir().join("config")).unwrap()
}

struct ExistingPointer {
    _directory: tempfile::TempDir,
    repo: Repository,
    pointer: Vec<u8>,
    canonical: Vec<u8>,
    object: PathBuf,
}
fn existing_pointer(cached: bool, processor: bool) -> ExistingPointer {
    let (directory, repo) = setup();
    let original = b"original";
    let canonical = b"EXT:lanigiro".to_vec();
    let oid = format!("{:x}", Sha256::digest(&canonical));
    let pointer = format!("version https://git-lfs.github.com/spec/v1\next-0-reverse sha256:{:x}\noid sha256:{oid}\nsize {}\n", Sha256::digest(original), canonical.len()).into_bytes();
    let object = raw(&repo)
        .commondir()
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid);
    if cached {
        fs::create_dir_all(object.parent().unwrap()).unwrap();
        fs::write(&object, &canonical).unwrap();
    }
    if processor {
        let script = directory.path().join("reverse.py");
        fs::write(&script, "import sys\nd=sys.stdin.buffer.read()\nsys.stdout.buffer.write(b'EXT:'+d[::-1] if sys.argv[1]=='clean' else d[4:][::-1])\n").unwrap();
        let mut config = raw(&repo).config().unwrap();
        config.set_i64("lfs.extension.reverse.priority", 0).unwrap();
        for operation in ["clean", "smudge"] {
            config
                .set_str(
                    &format!("lfs.extension.reverse.{operation}"),
                    &format!("python3 {} {operation}", script.display()),
                )
                .unwrap();
        }
    }
    fs::write(repo.root.join(".gitattributes"), b"*.bin filter=lfs\n").unwrap();
    fs::write(repo.root.join("asset.bin"), &pointer).unwrap();
    // The new candidate's extension canonical is five bytes and fits cap=8;
    // the preserved pointer's existing canonical is twelve bytes and does not.
    fs::write(repo.root.join("new.dat"), b"x").unwrap();
    commit(&repo, "existing pointer and new raw data");
    ExistingPointer {
        _directory: directory,
        repo,
        pointer,
        canonical,
        object,
    }
}

fn assert_preview_refusal_preserves_source(fixture: &ExistingPointer, expected: &str) {
    let before_index = index(&fixture.repo);
    let before_config = config(&fixture.repo);
    let before_head = fixture.repo.resolve_revision("HEAD").unwrap().id;
    let result = fixture
        .repo
        .prepare_lfs_migration("*.dat", false, NetworkControl::default());
    let error =
        result.expect_err("Preview must reject a preserved cached pointer that cannot be hydrated");
    assert!(
        format!("{error:#}").contains(expected),
        "wrong refusal: {error:#}"
    );
    assert_eq!(index(&fixture.repo), before_index);
    assert_eq!(config(&fixture.repo), before_config);
    assert_eq!(
        fixture.repo.resolve_revision("HEAD").unwrap().id,
        before_head
    );
    assert_eq!(
        fs::read(fixture.repo.root.join("asset.bin")).unwrap(),
        fixture.pointer
    );
    assert_eq!(fs::read(fixture.repo.root.join("new.dat")).unwrap(), b"x");
    assert_eq!(fs::read(&fixture.object).unwrap(), fixture.canonical);
    assert!(fixture.repo.lfs_migration_status().unwrap().is_none());
}

#[test]
fn preview_refuses_missing_processors_for_preserved_cached_head_pointers() {
    let fixture = existing_pointer(true, false);
    assert_preview_refusal_preserves_source(&fixture, "processor");
}

#[test]
fn preview_checks_current_filter_cap_for_preserved_cached_head_pointers() {
    let fixture = existing_pointer(true, true);
    raw(&fixture.repo)
        .config()
        .unwrap()
        .set_i64("gitbuddy.lfsFilterMaxBytes", 8)
        .unwrap();
    assert_preview_refusal_preserves_source(&fixture, "limit");
}

#[test]
fn missing_old_cache_keeps_the_placeholder_without_fetching_or_requiring_its_processor() {
    let fixture = existing_pointer(false, false);
    let plan = fixture
        .repo
        .prepare_lfs_migration("*.dat", false, NetworkControl::default())
        .unwrap();
    let target = plan.target.clone();
    fixture
        .repo
        .execute(Operation::LfsMigrate(Arc::new(plan)))
        .unwrap();
    assert_eq!(fixture.repo.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(
        fs::read(fixture.repo.root.join("asset.bin")).unwrap(),
        fixture.pointer
    );
    assert_eq!(fs::read(fixture.repo.root.join("new.dat")).unwrap(), b"x");
    assert!(!fixture.object.exists());
    assert!(fixture.repo.lfs_migration_status().unwrap().is_none());
}

#[test]
fn historical_cached_pointers_outside_candidate_head_do_not_require_a_processor() {
    let fixture = existing_pointer(true, false);
    let source = raw(&fixture.repo);
    let old_blob = source
        .head()
        .unwrap()
        .peel_to_tree()
        .unwrap()
        .get_name("asset.bin")
        .unwrap()
        .id();
    fs::remove_file(fixture.repo.root.join("asset.bin")).unwrap();
    commit(&fixture.repo, "remove historical pointer");
    let plan = fixture
        .repo
        .prepare_lfs_migration("*.dat", false, NetworkControl::default())
        .unwrap();
    fixture
        .repo
        .execute(Operation::LfsMigrate(Arc::new(plan)))
        .unwrap();
    let current = raw(&fixture.repo);
    let rewritten = current.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(
        rewritten
            .parent(0)
            .unwrap()
            .tree()
            .unwrap()
            .get_name("asset.bin")
            .unwrap()
            .id(),
        old_blob
    );
    assert!(!fixture.repo.root.join("asset.bin").exists());
    assert_eq!(fs::read(fixture.repo.root.join("new.dat")).unwrap(), b"x");
    assert_eq!(fs::read(&fixture.object).unwrap(), fixture.canonical);
}

#[test]
fn future_hydration_uses_source_info_overrides_instead_of_isolated_clone_defaults() {
    let fixture = existing_pointer(true, false);
    fs::write(
        raw(&fixture.repo).commondir().join("info/attributes"),
        b"asset.bin -filter\n",
    )
    .unwrap();
    // Source checkout itself proves this preserved pointer does not need its
    // processor. The target adds only a new.dat rule; info/attributes still wins.
    fixture.repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(
        fs::read(fixture.repo.root.join("asset.bin")).unwrap(),
        fixture.pointer
    );
    let before_index = index(&fixture.repo);
    let before_config = config(&fixture.repo);
    let plan = fixture
        .repo
        .prepare_lfs_migration("*.dat", false, NetworkControl::default())
        .unwrap();
    assert_eq!(index(&fixture.repo), before_index);
    assert_eq!(config(&fixture.repo), before_config);
    let target = plan.target.clone();
    fixture
        .repo
        .execute(Operation::LfsMigrate(Arc::new(plan)))
        .unwrap();
    assert_eq!(fixture.repo.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(
        fs::read(fixture.repo.root.join("asset.bin")).unwrap(),
        fixture.pointer
    );
    assert_eq!(fs::read(fixture.repo.root.join("new.dat")).unwrap(), b"x");
    assert_eq!(fs::read(&fixture.object).unwrap(), fixture.canonical);
}

#[test]
fn future_hydration_uses_candidate_tree_rules_that_enable_a_preserved_pointer() {
    let fixture = existing_pointer(true, false);
    fs::write(
        fixture.repo.root.join(".gitattributes"),
        b"# no current LFS rules\n",
    )
    .unwrap();
    commit(&fixture.repo, "disable the old rule before migration");
    let before_index = index(&fixture.repo);
    let before_config = config(&fixture.repo);
    let original = fixture.repo.resolve_revision("HEAD").unwrap().id;
    // Querying only the source's old worktree would miss the new *.bin rule.
    let error = fixture
        .repo
        .prepare_lfs_migration("*.bin,*.dat", false, NetworkControl::default())
        .unwrap_err();
    assert!(format!("{error:#}").contains("processor"));
    assert_eq!(index(&fixture.repo), before_index);
    assert_eq!(config(&fixture.repo), before_config);
    assert_eq!(fixture.repo.resolve_revision("HEAD").unwrap().id, original);
    assert_eq!(
        fs::read(fixture.repo.root.join("asset.bin")).unwrap(),
        fixture.pointer
    );
}

#[cfg(unix)]
#[test]
fn migration_tree_path_validation_ignores_unrelated_cwd_symlinks() {
    if let Some(root) = std::env::var_os("GITBUDDY_MIGRATION_CWD_CHILD") {
        let repo = Repository::open(Path::new(&root)).unwrap();
        let before_index = index(&repo);
        let before_config = config(&repo);
        let before_head = repo.resolve_revision("HEAD").unwrap().id;
        let plan = repo
            .prepare_lfs_migration("*.bin", false, NetworkControl::default())
            .unwrap();
        assert_ne!(plan.original, plan.target);
        assert_eq!(index(&repo), before_index);
        assert_eq!(config(&repo), before_config);
        assert_eq!(repo.resolve_revision("HEAD").unwrap().id, before_head);
        assert_eq!(
            fs::read(repo.root.join("nested/asset.bin")).unwrap(),
            b"payload"
        );
        return;
    }
    let (directory, repo) = setup();
    fs::create_dir(repo.root.join("nested")).unwrap();
    fs::write(repo.root.join("nested/asset.bin"), b"payload").unwrap();
    commit(&repo, "safe nested source path");
    let cwd = directory.path().join("unrelated-cwd");
    fs::create_dir(&cwd).unwrap();
    let target = directory.path().join("unrelated-target");
    fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(target, cwd.join("nested")).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "migration_tree_path_validation_ignores_unrelated_cwd_symlinks",
            "--test-threads=1",
            "--nocapture",
        ])
        .current_dir(cwd)
        .env("GITBUDDY_MIGRATION_CWD_CHILD", &repo.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
