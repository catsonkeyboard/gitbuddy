//! Conservative local LFS maintenance. Reflogs and GitBuddy recovery refs count
//! as roots, unlike the standard prune command. Pruning is reversible quarantine.
use super::{
    NetworkControl, Repository,
    lfs::{Pointer, capture_lfs, hash, hash_with_control, object, safe_path, storage},
    libgit::raw,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize)]
pub struct LfsLockOwner {
    pub name: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct LfsLock {
    pub id: String,
    pub path: String,
    pub locked_at: String,
    pub owner: Option<LfsLockOwner>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct LfsLocks {
    pub ours: Vec<LfsLock>,
    pub theirs: Vec<LfsLock>,
}
#[derive(Clone, Debug)]
pub struct LfsMaintenance {
    pub checked: usize,
    pub protected: usize,
    pub corrupt: Vec<String>,
    pub candidates: Vec<String>,
    pub bytes: u64,
    pub fingerprint: String,
    pub purge: Vec<PathBuf>,
    pub purge_bytes: u64,
}
impl LfsMaintenance {
    pub fn report(&self) -> String {
        format!(
            "{} cached objects checked · {} protected · {} corrupt\n{} unreferenced objects · {} bytes eligible for reversible quarantine\n{} quarantined objects · {} bytes eligible for permanent deletion\nProtection includes every reference, reflog, index and linked worktree. Missing or unreadable Git history blocks maintenance. Quarantine does not free disk space.\n\n{}",
            self.checked,
            self.protected,
            self.corrupt.len(),
            self.candidates.len(),
            self.bytes,
            self.purge.len(),
            self.purge_bytes,
            if self.corrupt.is_empty() {
                "No corrupt cache objects found.".into()
            } else {
                format!(
                    "Corrupt objects (retained; first 50 shown):\n{}",
                    self.corrupt
                        .iter()
                        .take(50)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
        )
    }
}

fn remote(repo: &git2::Repository, name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.starts_with('-') && !name.contains(['\n', '\r', '\0']),
        "Invalid remote"
    );
    repo.find_remote(name)?;
    Ok(())
}
fn parse_locks(text: &str) -> Result<LfsLocks> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    ensure!(
        value
            .get("error")
            .is_none_or(|v| v.is_null() || v.as_str() == Some("")),
        "LFS lock server returned an error"
    );
    let locks: LfsLocks = serde_json::from_value(value)?;
    let mut ids = HashSet::new();
    for lock in locks.ours.iter().chain(&locks.theirs) {
        ensure!(
            !lock.id.is_empty() && ids.insert(lock.id.clone()),
            "Invalid or duplicated LFS lock ID"
        );
        ensure!(!lock.path.is_empty(), "LFS lock path is missing");
    }
    Ok(locks)
}
impl Repository {
    pub fn lfs_locks(&self, name: &str, control: NetworkControl) -> Result<LfsLocks> {
        let _writer = super::libgit::WriterLock::acquire(&raw(self)?)?;
        self.verified_lfs_locks(name, control)
    }
    fn verified_lfs_locks(&self, name: &str, control: NetworkControl) -> Result<LfsLocks> {
        remote(&raw(self)?, name)?;
        verified_locks(name, |args| capture_lfs(&self.root, args, &control))
    }
    pub(super) fn lfs_lock(
        &self,
        name: &str,
        path: &Path,
        control: NetworkControl,
    ) -> Result<String> {
        let repo = raw(self)?;
        remote(&repo, name)?;
        ensure!(
            super::lfs::tracked(&repo, path)?,
            "Choose a file tracked by LFS"
        );
        ensure!(
            safe_path(&repo, path)?.is_file(),
            "Choose an existing regular file"
        );
        let path = path.to_str().context("LFS lock path must be UTF-8")?;
        let arg = format!("--remote={name}");
        let text = capture_lfs(&self.root, &["lock", &arg, "--", path], &control)?;
        control.cancellation.finish()?;
        Ok(text)
    }
    pub(super) fn lfs_unlock(
        &self,
        name: &str,
        expected: &LfsLock,
        force: bool,
        control: NetworkControl,
    ) -> Result<String> {
        let locks = self.verified_lfs_locks(name, control.clone())?;
        check_unlock(&locks, expected, force)?;
        let arg = format!("--remote={name}");
        let id = format!("--id={}", expected.id);
        let mut args = vec!["unlock", arg.as_str(), id.as_str()];
        if force {
            args.push("--force");
        }
        let text = capture_lfs(&self.root, &args, &control)?;
        control.cancellation.finish()?;
        Ok(text)
    }
    pub fn lfs_maintenance(&self, control: NetworkControl) -> Result<LfsMaintenance> {
        let _writer = super::libgit::WriterLock::acquire(&raw(self)?)?;
        self.inspect_lfs_cache(control)
    }
    fn inspect_lfs_cache(&self, control: NetworkControl) -> Result<LfsMaintenance> {
        let repo = raw(self)?;
        let objects = storage(&repo)?;
        let roots = repositories(&repo)?;
        let mut protected = BTreeSet::new();
        for root in &roots {
            collect(root, &mut protected, &control)?;
        }
        let mut report = LfsMaintenance {
            checked: 0,
            protected: 0,
            corrupt: vec![],
            candidates: vec![],
            bytes: 0,
            fingerprint: String::new(),
            purge: vec![],
            purge_bytes: 0,
        };
        let mut digest = Sha256::new();
        for oid in &protected {
            digest.update(oid);
        }
        for path in cached_objects(&objects, &control)? {
            control.check()?;
            let oid = path
                .file_name()
                .context("Missing object name")?
                .to_string_lossy()
                .into_owned();
            control.phase(&format!("Checking LFS cache object {}…", &oid[..8]));
            let pointer = hash_with_control(&path, Some(&control))?;
            report.checked += 1;
            digest.update(&oid);
            digest.update(&pointer.oid);
            digest.update(pointer.size.to_le_bytes());
            if pointer.oid != oid {
                report.corrupt.push(oid);
            } else if protected.contains(&oid) {
                report.protected += 1;
            } else {
                report.bytes += pointer.size;
                report.candidates.push(oid);
            }
        }
        let quarantine = objects
            .parent()
            .context("No LFS directory")?
            .join("gitbuddy-quarantine");
        for path in quarantined_objects(&quarantine, &control)? {
            control.check()?;
            let relative = path.strip_prefix(&quarantine)?.to_path_buf();
            let oid = path
                .file_name()
                .context("Missing quarantine object name")?
                .to_string_lossy()
                .into_owned();
            let pointer = hash_with_control(&path, Some(&control))?;
            digest.update(relative.to_string_lossy().as_bytes());
            digest.update(&pointer.oid);
            digest.update(pointer.size.to_le_bytes());
            if oid != pointer.oid {
                report
                    .corrupt
                    .push(format!("quarantine/{}", relative.display()));
            } else if !protected.contains(&oid) {
                report.purge.push(relative);
                report.purge_bytes += pointer.size;
            }
        }
        report.fingerprint = format!("{:x}", digest.finalize());
        Ok(report)
    }
    pub(super) fn lfs_quarantine(
        &self,
        expected: &LfsMaintenance,
        control: NetworkControl,
    ) -> Result<String> {
        let repo = raw(self)?;
        for root in repositories(&repo)? {
            ensure!(
                root.state() == git2::RepositoryState::Clean
                    && !super::rebase::active(&root)
                    && super::libgit::file_changes(&root)?.is_empty(),
                "Finish operations and preserve changes in every worktree before pruning"
            );
        }
        let current = self.inspect_lfs_cache(control.clone())?;
        ensure!(
            current.fingerprint == expected.fingerprint
                && current.candidates == expected.candidates,
            "Git history or LFS cache changed; build a new preview"
        );
        ensure!(
            !current.candidates.is_empty(),
            "No unreferenced LFS objects"
        );
        control.cancellation.finish()?;
        let source = storage(&repo)?;
        let target = source
            .parent()
            .context("No LFS directory")?
            .join("gitbuddy-quarantine")
            .join(
                chrono::Utc::now()
                    .timestamp_nanos_opt()
                    .unwrap_or_default()
                    .to_string(),
            );
        ensure!(
            fs::symlink_metadata(target.parent().unwrap())
                .ok()
                .is_none_or(|m| !m.file_type().is_symlink()),
            "Quarantine must not be a symlink"
        );
        fs::create_dir_all(target.parent().unwrap())?;
        fs::create_dir(&target)?;
        for oid in &current.candidates {
            fs::rename(
                source.join(&oid[..2]).join(&oid[2..4]).join(oid),
                target.join(oid),
            )
            .with_context(|| {
                format!(
                    "Prune partly completed; recovery objects retained at {}",
                    target.display()
                )
            })?;
        }
        fs::File::open(&target)?.sync_all()?;
        Ok(format!(
            "{} objects moved to {}; disk space is retained until you remove the quarantine",
            current.candidates.len(),
            target.display()
        ))
    }
    pub(super) fn lfs_restore_quarantine(&self, control: NetworkControl) -> Result<String> {
        let repo = raw(self)?;
        let source = storage(&repo)?;
        let quarantine = source
            .parent()
            .context("No LFS directory")?
            .join("gitbuddy-quarantine");
        let mut count = 0;
        if quarantine.exists() {
            ensure!(
                !fs::symlink_metadata(&quarantine)?.file_type().is_symlink(),
                "Quarantine must not be a symlink"
            );
            for directory in fs::read_dir(&quarantine)? {
                let directory = directory?;
                ensure!(
                    directory.file_type()?.is_dir(),
                    "Unexpected quarantine entry"
                );
                for entry in fs::read_dir(directory.path())? {
                    control.check()?;
                    let entry = entry?;
                    ensure!(entry.file_type()?.is_file(), "Unexpected quarantine object");
                    let pointer = hash(&entry.path())?;
                    ensure!(
                        entry.file_name().to_str() == Some(&pointer.oid),
                        "Corrupt quarantine object; preserved for inspection"
                    );
                    let destination = object(&repo, &pointer)?;
                    fs::create_dir_all(destination.parent().unwrap())?;
                    if destination.exists() {
                        ensure!(
                            hash(&destination)?.oid == pointer.oid,
                            "Destination cache is corrupt"
                        );
                    } else {
                        fs::rename(entry.path(), &destination)?;
                        count += 1;
                    }
                }
            }
        }
        Ok(format!(
            "Restored {count} quarantined objects; use LFS checkout to expand pointers"
        ))
    }
    pub(super) fn lfs_purge_quarantine(
        &self,
        expected: &LfsMaintenance,
        control: NetworkControl,
    ) -> Result<String> {
        let repo = raw(self)?;
        for root in repositories(&repo)? {
            ensure!(
                root.state() == git2::RepositoryState::Clean
                    && !super::rebase::active(&root)
                    && super::libgit::file_changes(&root)?.is_empty(),
                "Preserve changes and finish operations in every worktree before deleting quarantine"
            );
        }
        let current = self.inspect_lfs_cache(control.clone())?;
        ensure!(
            current.fingerprint == expected.fingerprint && current.purge == expected.purge,
            "History or quarantine changed; build a new preview"
        );
        control.cancellation.finish()?;
        let root = storage(&repo)?
            .parent()
            .context("No LFS directory")?
            .join("gitbuddy-quarantine");
        for path in &current.purge {
            fs::remove_file(root.join(path)).with_context(|| {
                format!(
                    "Deletion partly completed; remaining objects are in {}",
                    root.display()
                )
            })?;
        }
        Ok(format!(
            "Permanently deleted {} unreferenced quarantine objects ({} bytes)",
            current.purge.len(),
            current.purge_bytes
        ))
    }
}
fn verified_locks(name: &str, mut run: impl FnMut(&[&str]) -> Result<String>) -> Result<LfsLocks> {
    let arg = format!("--remote={name}");
    // The JSON-only CLI path can report success on a failed search. Verify
    // without JSON first; only then read the just-written verified cache.
    run(&["locks", "--verify", &arg])?;
    parse_locks(&run(&["locks", "--verify", "--cached", "--json", &arg])?)
}
fn check_unlock(locks: &LfsLocks, expected: &LfsLock, force: bool) -> Result<()> {
    let current = locks
        .ours
        .iter()
        .chain(&locks.theirs)
        .find(|lock| lock.id == expected.id)
        .context("Lock no longer exists; refresh")?;
    ensure!(
        current.path == expected.path && current.locked_at == expected.locked_at,
        "Lock changed; refresh"
    );
    ensure!(
        force || locks.ours.iter().any(|lock| lock.id == expected.id),
        "This lock belongs to another user; explicit force unlock is required"
    );
    Ok(())
}
fn quarantined_objects(root: &Path, control: &NetworkControl) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    if !root.exists() {
        return Ok(result);
    }
    ensure!(
        !fs::symlink_metadata(root)?.file_type().is_symlink(),
        "Quarantine must not be a symlink"
    );
    for directory in fs::read_dir(root)? {
        let directory = directory?;
        ensure!(
            directory.file_type()?.is_dir(),
            "Unexpected quarantine directory"
        );
        for entry in fs::read_dir(directory.path())? {
            control.check()?;
            ensure!(
                result.len() < 500_000,
                "Quarantine exceeds maintenance limit"
            );
            let entry = entry?;
            ensure!(entry.file_type()?.is_file(), "Unexpected quarantine object");
            let name = entry.file_name();
            let Some(oid) = name.to_str() else {
                continue;
            };
            if oid.len() == 64
                && oid
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                result.push(entry.path());
            }
        }
    }
    result.sort();
    Ok(result)
}
fn repositories(repo: &git2::Repository) -> Result<Vec<git2::Repository>> {
    let mut roots = vec![git2::Repository::open(repo.commondir())?];
    for name in repo.worktrees()?.iter() {
        let name = name?.context("Worktree name is not UTF-8")?;
        let worktree = repo.find_worktree(name)?;
        ensure!(
            worktree.validate().is_ok(),
            "Repair or prune missing worktree {name} before LFS maintenance"
        );
        roots.push(git2::Repository::open_from_worktree(&worktree)?);
    }
    Ok(roots)
}
fn collect(
    repo: &git2::Repository,
    protected: &mut BTreeSet<String>,
    control: &NetworkControl,
) -> Result<()> {
    let mut walk = repo.revwalk()?;
    let mut pushed = false;
    for reference in repo.references()? {
        let reference = reference?;
        let resolved = reference.resolve()?;
        let object = repo.find_object(
            resolved
                .target()
                .context("Reference has no object target")?,
            None,
        )?;
        let object = object.peel(git2::ObjectType::Any)?;
        if let Some(commit) = object.as_commit() {
            walk.push(commit.id())?;
            pushed = true;
        } else if let Some(value) = object.as_tree() {
            tree(repo, value, protected, &mut HashSet::new(), control, 0)?;
        } else if let Some(value) = object.as_blob()
            && let Some(pointer) = maintenance_pointer(value.content())?
        {
            protected.insert(pointer.oid);
        }
        if repo.reference_has_log(reference.name()?)? {
            for entry in repo.reflog(reference.name()?)?.iter() {
                for id in [entry.id_old(), entry.id_new()] {
                    if !id.is_zero() {
                        walk.push(id)?;
                        pushed = true;
                    }
                }
            }
        }
    }
    for name in log_names(&repo.commondir().join("logs/refs"), "refs")? {
        for entry in repo.reflog(&name)?.iter() {
            for id in [entry.id_old(), entry.id_new()] {
                if !id.is_zero() {
                    walk.push(id)?;
                    pushed = true;
                }
            }
        }
    }
    match repo.head() {
        Ok(head) => {
            walk.push(head.peel_to_commit()?.id())?;
            pushed = true;
        }
        Err(e)
            if matches!(
                e.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) => {}
        Err(e) => return Err(e.into()),
    }
    if repo.reference_has_log("HEAD")? {
        for entry in repo.reflog("HEAD")?.iter() {
            for id in [entry.id_old(), entry.id_new()] {
                if !id.is_zero() {
                    walk.push(id)?;
                    pushed = true;
                }
            }
        }
    }
    let mut seen = HashSet::new();
    if pushed {
        for (n, id) in walk.enumerate() {
            control.check()?;
            ensure!(
                n < 100_000,
                "History exceeds maintenance limit; no objects were pruned"
            );
            tree(
                repo,
                &repo.find_commit(id?)?.tree()?,
                protected,
                &mut seen,
                control,
                0,
            )?;
        }
    }
    let index = repo.index()?;
    for entry in index.iter() {
        blob(repo, entry.id, entry.mode, protected)?;
    }
    for conflict in index.conflicts()? {
        let conflict = conflict?;
        for entry in [conflict.ancestor, conflict.our, conflict.their]
            .into_iter()
            .flatten()
        {
            blob(repo, entry.id, entry.mode, protected)?;
        }
    }
    Ok(())
}
fn tree(
    repo: &git2::Repository,
    value: &git2::Tree<'_>,
    protected: &mut BTreeSet<String>,
    seen: &mut HashSet<git2::Oid>,
    control: &NetworkControl,
    depth: usize,
) -> Result<()> {
    ensure!(
        depth < 128 && seen.len() < 1_000_000,
        "LFS maintenance tree limit reached"
    );
    if !seen.insert(value.id()) {
        return Ok(());
    }
    for entry in value {
        control.check()?;
        if entry.kind() == Some(git2::ObjectType::Tree) {
            tree(
                repo,
                &repo.find_tree(entry.id())?,
                protected,
                seen,
                control,
                depth + 1,
            )?;
        } else {
            blob(repo, entry.id(), entry.filemode() as u32, protected)?;
        }
    }
    Ok(())
}
fn blob(
    repo: &git2::Repository,
    id: git2::Oid,
    mode: u32,
    protected: &mut BTreeSet<String>,
) -> Result<()> {
    if mode & 0o170000 != 0o100000 {
        return Ok(());
    }
    if repo.odb()?.read_header(id)?.0 > 1024 {
        return Ok(());
    }
    let blob = repo.find_blob(id)?;
    if let Some(pointer) = maintenance_pointer(blob.content())? {
        protected.insert(pointer.oid);
    }
    Ok(())
}
fn maintenance_pointer(bytes: &[u8]) -> Result<Option<Pointer>> {
    let pointer = Pointer::parse(bytes);
    ensure!(
        pointer.is_some()
            || !(bytes.starts_with(b"version ")
                && bytes
                    .windows(b"oid sha256:".len())
                    .any(|part| part == b"oid sha256:")),
        "Unsupported or malformed LFS pointer in protected history; no objects were pruned"
    );
    Ok(pointer)
}
fn log_names(root: &Path, prefix: &str) -> Result<Vec<String>> {
    ensure!(
        prefix.matches('/').count() < 64,
        "Reflog directory exceeds maintenance depth limit"
    );
    let mut names = Vec::new();
    if !root.exists() {
        return Ok(names);
    }
    ensure!(
        !fs::symlink_metadata(root)?.file_type().is_symlink(),
        "Reflog directory is a symlink; no objects were pruned"
    );
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = format!(
            "{prefix}/{}",
            file_name.to_str().context("Reflog name is not UTF-8")?
        );
        if entry.file_type()?.is_dir() {
            names.extend(log_names(&entry.path(), &name)?);
        } else {
            ensure!(entry.file_type()?.is_file(), "Unsupported reflog entry");
            names.push(name);
        }
    }
    Ok(names)
}
fn cached_objects(root: &Path, control: &NetworkControl) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    if !root.exists() {
        return Ok(result);
    }
    ensure!(
        !fs::symlink_metadata(root)?.file_type().is_symlink(),
        "LFS cache must not be a symlink"
    );
    for a in fs::read_dir(root)? {
        let a = a?;
        if !a.file_type()?.is_dir() {
            continue;
        }
        for b in fs::read_dir(a.path())? {
            let b = b?;
            if !b.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(b.path())? {
                control.check()?;
                ensure!(
                    result.len() < 500_000,
                    "LFS cache exceeds maintenance limit"
                );
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let name = file.file_name();
                let Some(oid) = name.to_str() else {
                    continue;
                };
                if oid.len() == 64
                    && oid
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && a.file_name().to_str() == Some(&oid[..2])
                    && b.file_name().to_str() == Some(&oid[2..4])
                {
                    result.push(file.path());
                }
            }
        }
    }
    result.sort();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_payload_requires_complete_owned_groups_and_unique_ids() {
        assert!(parse_locks("{\"ours\":[],\"theirs\":[]}").is_ok());
        assert!(parse_locks("{\"ours\":[]}").is_err());
        assert!(parse_locks("{\"ours\":[],\"theirs\":[],\"error\":\"denied\"}").is_err());
        assert!(parse_locks("{\"ours\":[{\"id\":\"1\",\"path\":\"a\",\"locked_at\":\"x\"}],\"theirs\":[{\"id\":\"1\",\"path\":\"b\",\"locked_at\":\"y\"}]}").is_err());
    }
    #[test]
    fn failed_server_verification_never_reads_stale_lock_cache() {
        let mut calls = 0;
        assert!(
            verified_locks("origin", |_| {
                calls += 1;
                anyhow::bail!("HTTP 403")
            })
            .is_err()
        );
        assert_eq!(calls, 1);
    }
    #[test]
    fn verified_lock_query_uses_complete_ownership_and_the_selected_remote() {
        let mut calls = Vec::new();
        let locks=verified_locks("team/backup", |args| {
            calls.push(args.iter().map(|s|s.to_string()).collect::<Vec<_>>());
            Ok(if args.contains(&"--json") {"{\"ours\":[{\"id\":\"owned\",\"path\":\"a.bin\",\"locked_at\":\"now\",\"owner\":{\"name\":\"Tester\"}}],\"theirs\":[]}".into()} else {String::new()})
        }).unwrap();
        assert_eq!(locks.ours[0].id, "owned");
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].iter().any(|s| s == "--json"));
        assert!(calls[1].iter().any(|s| s == "--cached"));
        assert!(
            calls
                .iter()
                .all(|args| args.iter().any(|s| s == "--remote=team/backup"))
        );
    }
    #[test]
    fn unlock_checks_ownership_and_rejects_stale_identity_even_with_force() {
        let lock = LfsLock {
            id: "1".into(),
            path: "a.bin".into(),
            locked_at: "created".into(),
            owner: None,
        };
        let mut locks = LfsLocks {
            ours: vec![],
            theirs: vec![lock.clone()],
        };
        assert!(check_unlock(&locks, &lock, false).is_err());
        assert!(check_unlock(&locks, &lock, true).is_ok());
        locks.theirs[0].path = "other.bin".into();
        assert!(check_unlock(&locks, &lock, true).is_err());
        locks.theirs.clear();
        assert!(check_unlock(&locks, &lock, true).is_err());
        locks.ours.push(lock.clone());
        assert!(check_unlock(&locks, &lock, false).is_ok());
        locks.ours[0].locked_at = "later".into();
        assert!(check_unlock(&locks, &lock, true).is_err());
    }
}
