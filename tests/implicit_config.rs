use gitbuddy::git::{Operation, Repository};
use sha2::{Digest, Sha256};
use std::{fs, sync::Arc};

#[test]
fn implicit_git_booleans_work_through_lfs_attributes_and_patch_conversion() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.execute(Operation::SetIdentity(
        "Fixture".into(),
        "fixture@example.invalid".into(),
    ))
    .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    config.set_str("core.attributesfile", "").unwrap();
    drop(config);
    let config_path = raw.path().join("config");
    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[core]\n\tignorecase\n[filter \"lfs\"]\n\trequired\n");
    fs::write(&config_path, text).unwrap();
    fs::write(
        repo.root.join(".gitattributes"),
        "ASSET.BIN filter=lfs -text\n",
    )
    .unwrap();
    let before = b"before\0body";
    let after = b"after\0body";
    fs::write(repo.root.join("asset.bin"), before).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("LFS base".into())).unwrap();
    fs::write(repo.root.join("asset.bin"), after).unwrap();
    repo.execute(Operation::StageAll).unwrap(); // Populate the new native cache.
    repo.execute(Operation::UnstageAll).unwrap();
    repo.execute(Operation::Discard("asset.bin".into()))
        .unwrap();
    assert_eq!(fs::read(repo.root.join("asset.bin")).unwrap(), before);
    assert!(repo.snapshot(10).unwrap().files.is_empty());
    let original_index = fs::read(raw.path().join("index")).unwrap();
    let original_config = fs::read(&config_path).unwrap();
    let patch = format!(
        "diff --git a/asset.bin b/asset.bin\n--- a/asset.bin\n+++ b/asset.bin\n@@ -1,3 +1,3 @@\n version https://git-lfs.github.com/spec/v1\n-oid sha256:{:x}\n-size {}\n+oid sha256:{:x}\n+size {}\n",
        Sha256::digest(before),
        before.len(),
        Sha256::digest(after),
        after.len()
    );
    let plan = repo.prepare_patch_file(patch.into_bytes()).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(fs::read(repo.root.join("asset.bin")).unwrap(), after);
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), original_index);
    assert_eq!(fs::read(&config_path).unwrap(), original_config);
}
