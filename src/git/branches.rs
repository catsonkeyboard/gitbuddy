//! Reviewed branch operations with repository identity and stale-target checks.
use super::libgit::{oid, raw};
use super::recovery::{editable, head_state};
use super::{HeadState, Repository};
use anyhow::{Context, Result, ensure};
use git2::{BranchType, Oid, build::CheckoutBuilder};
use std::{io::Write, path::PathBuf};

struct ConfigLock {
    path: PathBuf,
    lock: PathBuf,
    file: std::fs::File,
    owned: bool,
}
impl ConfigLock {
    fn acquire(repo: &git2::Repository) -> Result<Self> {
        let path = repo.commondir().join("config");
        ensure!(
            std::fs::symlink_metadata(&path)?.file_type().is_file(),
            "Branch rename requires a regular repository config file"
        );
        let lock = path.with_extension("lock");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .context("Cannot lock repository config; finish the other Git operation first")?;
        Ok(Self {
            path,
            lock,
            file,
            owned: true,
        })
    }
    fn prepare(&mut self, old: &str, new: &str) -> Result<()> {
        let bytes = std::fs::read(&self.path)?;
        let mut copy = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        copy.write_all(&bytes)?;
        copy.flush()?;
        let config = git2::Config::open(copy.path())?;
        let prefix = format!("branch.{old}.");
        let expected = config_values(&config)?
            .into_iter()
            .map(|(key, value, depth)| {
                let key = if depth == 0 && key.starts_with(prefix.as_bytes()) {
                    [format!("branch.{new}.").as_bytes(), &key[prefix.len()..]].concat()
                } else {
                    key
                };
                (key, value, depth)
            })
            .collect::<Vec<_>>();
        drop(config);
        // Preserve values and comments verbatim. libgit2 delete_multivar has a
        // NULL-value crash for implicit booleans; rewriting values also risks
        // changing multiline or duplicate entries. Only rewrite section headers,
        // then validate the complete parsed config against the expected mapping.
        let rewritten = renamed_sections(&bytes, old, new);
        std::fs::write(copy.path(), &rewritten)?;
        let actual = config_values(&git2::Config::open(copy.path())?)?;
        ensure!(
            actual == expected,
            "Cannot safely rename this configuration syntax; migrate the branch section manually"
        );
        self.file
            .set_permissions(std::fs::metadata(&self.path)?.permissions())?;
        self.file.write_all(&rewritten)?;
        self.file.sync_all()?;
        Ok(())
    }
    fn install(mut self) -> Result<()> {
        std::fs::rename(&self.lock, &self.path)?;
        self.owned = false;
        Ok(())
    }
}

type ConfigValue = (Vec<u8>, Option<Vec<u8>>, u32);
fn config_values(config: &git2::Config) -> Result<Vec<ConfigValue>> {
    let mut result = Vec::new();
    config.entries(None)?.for_each(|entry| {
        result.push((
            entry.name_bytes().to_vec(),
            entry.has_value().then(|| entry.value_bytes().to_vec()),
            entry.include_depth(),
        ));
    })?;
    Ok(result)
}
fn renamed_sections(bytes: &[u8], old: &str, new: &str) -> Vec<u8> {
    let mut result = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let trimmed = line.trim_ascii_start();
        let parsed = (|| {
            let rest = trimmed.strip_prefix(b"[")?.trim_ascii_start();
            let end = rest
                .iter()
                .position(|b| b.is_ascii_whitespace() || matches!(b, b'.' | b']'))?;
            if !rest[..end].eq_ignore_ascii_case(b"branch") {
                return None;
            }
            let tail = rest[end..].trim_ascii_start();
            let (name, suffix) = if let Some(tail) = tail.strip_prefix(b".") {
                let end = tail.iter().position(|b| *b == b']')?;
                (
                    tail[..end].trim_ascii().to_ascii_lowercase(),
                    &tail[end + 1..],
                )
            } else {
                let tail = tail.strip_prefix(b"\"")?;
                let mut name = Vec::new();
                let mut i = 0;
                loop {
                    match *tail.get(i)? {
                        b'"' => break,
                        b'\\' => {
                            i += 1;
                            let b = *tail.get(i)?;
                            if !matches!(b, b'\\' | b'"') {
                                return None;
                            }
                            name.push(b);
                        }
                        b => name.push(b),
                    }
                    i += 1;
                }
                (name, tail[i + 1..].trim_ascii_start().strip_prefix(b"]")?)
            };
            (name == old.as_bytes()).then_some(suffix)
        })();
        if let Some(suffix) = parsed {
            result.extend_from_slice(&line[..line.len() - trimmed.len()]);
            let escaped = new.replace('\\', "\\\\").replace('"', "\\\"");
            result.extend_from_slice(format!("[branch \"{escaped}\"]").as_bytes());
            result.extend_from_slice(suffix);
        } else {
            result.extend_from_slice(line);
        }
    }
    result
}
impl Drop for ConfigLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.lock);
        }
    }
}

#[derive(Clone, Debug)]
pub struct BranchRename {
    pub name: String,
    pub id: String,
    head: HeadState,
    git_dir: PathBuf,
}
#[derive(Clone, Debug)]
pub struct DetachedCheckout {
    pub label: String,
    pub id: String,
    head: HeadState,
    reference: Option<(String, Oid)>,
    fingerprint: u64,
    git_dir: PathBuf,
}
impl Repository {
    pub fn prepare_branch_rename(&self, name: &str) -> Result<BranchRename> {
        let repo = raw(self)?;
        editable(&repo)?;
        let branch = repo.find_branch(name, BranchType::Local)?;
        let reference = branch.get().name()?.to_owned();
        super::workspaces::branch_available(&repo, &reference)?;
        Ok(BranchRename {
            name: name.into(),
            id: branch
                .get()
                .target()
                .context("Branch must be a direct reference")?
                .to_string(),
            head: head_state(&repo)?,
            git_dir: repo.path().into(),
        })
    }
    pub(super) fn rename_branch(&self, expected: &BranchRename, name: &str) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == expected.git_dir,
            "Branch selection belongs to another repository"
        );
        editable(&repo)?;
        ensure!(
            head_state(&repo)? == expected.head,
            "HEAD changed; reopen Rename branch"
        );
        ensure!(
            git2::Branch::name_is_valid(name)? && !name.starts_with('-'),
            "Invalid branch name"
        );
        ensure!(name != expected.name, "Enter a different branch name");
        ensure!(
            !name.starts_with(&format!("{}/", expected.name))
                && !expected.name.starts_with(&format!("{name}/")),
            "Renaming between nested ref paths requires an intermediate branch name; use a separate name first"
        );
        ensure!(
            repo.find_reference(&format!("refs/heads/{name}"))
                .is_err_and(|e| e.code() == git2::ErrorCode::NotFound),
            "Destination branch already exists or cannot be read"
        );
        // An orphaned branch section may still contain an upstream or settings.
        // Never replace that configuration merely because the ref is absent.
        let prefix = format!("branch.{name}.");
        let config = repo.config()?.snapshot()?;
        let mut occupied = false;
        config.entries(None)?.for_each(|entry| {
            occupied |= entry.name().is_ok_and(|key| key.starts_with(&prefix));
        })?;
        ensure!(
            !occupied,
            "Destination branch configuration already exists; choose another name"
        );
        super::workspaces::branch_available(&repo, &format!("refs/heads/{}", expected.name))?;
        let branch = repo.find_branch(&expected.name, BranchType::Local)?;
        ensure!(
            branch.get().target() == Some(oid(&expected.id)?),
            "Branch moved; reopen Rename branch"
        );
        let source = format!("refs/heads/{}", expected.name);
        let target = format!("refs/heads/{name}");
        // libgit2's branch_move removes the source before acquiring all locks.
        // Keep the source alive until destination, config and HEAD are ready.
        let mut source_lock = repo.transaction()?;
        source_lock.lock_ref(&source)?;
        let mut destination_lock = repo.transaction()?;
        destination_lock.lock_ref(&target)?;
        let mut head_lock = repo.transaction()?;
        head_lock.lock_ref("HEAD")?;
        let mut config_lock = ConfigLock::acquire(&repo)?;
        let config = raw(self)?.config()?.snapshot()?;
        let destination_prefix = format!("branch.{name}.");
        let mut destination_config = false;
        config.entries(None)?.for_each(|entry| {
            destination_config |= entry
                .name()
                .is_ok_and(|key| key.starts_with(&destination_prefix));
        })?;
        ensure!(
            !destination_config,
            "Destination branch configuration appeared; reopen Rename branch"
        );
        ensure!(
            head_state(&repo)? == expected.head
                && repo.find_reference(&source)?.target() == Some(oid(&expected.id)?),
            "Branch or HEAD changed; reopen Rename branch"
        );
        let prefix = format!("branch.{}.", expected.name);
        let mut external_config = false;
        config.entries(None)?.for_each(|entry| {
            if entry.name().is_ok_and(|key| key.starts_with(&prefix)) {
                external_config |=
                    entry.level() != git2::ConfigLevel::Local || entry.include_depth() != 0;
            }
        })?;
        ensure!(
            !external_config,
            "Branch settings in included, global or worktree config require manual migration before renaming"
        );
        config_lock.prepare(&expected.name, name)?;
        let signature = repo
            .signature()
            .or_else(|_| git2::Signature::now("GitBuddy", "gitbuddy@localhost"))?;
        let message = format!("branch: renamed {source} to {target}");
        let original_log = repo.reflog(&source)?;
        let mut log = repo.reflog(&target)?;
        ensure!(
            log.is_empty(),
            "Destination reflog already exists; choose another name to preserve it"
        );
        for index in (0..original_log.len()).rev() {
            let entry = original_log.get(index).unwrap();
            if log.is_empty() && !entry.id_old().is_zero() {
                log.append(
                    entry.id_old(),
                    &entry.committer(),
                    Some("gitbuddy: preserved reflog boundary"),
                )?;
            }
            log.append(entry.id_new(), &entry.committer(), entry.message()?)?;
        }
        log.append(oid(&expected.id)?, &signature, Some(&message))?;
        destination_lock.set_target(&target, oid(&expected.id)?, Some(&signature), &message)?;
        destination_lock.set_reflog(&target, log)?;
        if expected.head.reference == source {
            head_lock.set_symbolic_target("HEAD", &target, Some(&signature), &message)?;
        }
        // Errors after this point retain both refs. They never silently delete
        // the only branch. Config/refs are separate filesystem resources.
        destination_lock.commit()?;
        config_lock.install().context("New branch exists and original branch is retained, but config installation failed; inspect both branches")?;
        head_lock.commit().context(
            "New branch and config exist; original branch retained because HEAD update failed",
        )?;
        source_lock.remove(&source)?;
        source_lock.commit().context(
            "New branch is ready, but old branch cleanup failed; both branches may remain",
        )?;
        Ok(format!(
            "Renamed {} to {name}; index and working files preserved",
            expected.name
        ))
    }
    pub fn prepare_detached_checkout(&self, revision: &str) -> Result<DetachedCheckout> {
        let repo = raw(self)?;
        editable(&repo)?;
        let (object, reference) = repo.revparse_ext(revision)?;
        let id = object.peel_to_commit()?.id().to_string();
        let reference = reference
            .map(|r| -> Result<_> {
                let r = r.resolve()?;
                Ok((
                    r.name()?.to_owned(),
                    r.target()
                        .context("Revision must resolve to a direct reference")?,
                ))
            })
            .transpose()?;
        Ok(DetachedCheckout {
            label: revision.into(),
            id,
            reference,
            head: head_state(&repo)?,
            fingerprint: self.fingerprint()?,
            git_dir: repo.path().into(),
        })
    }
    pub(super) fn checkout_detached(&self, expected: &DetachedCheckout) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == expected.git_dir,
            "Checkout selection belongs to another repository"
        );
        editable(&repo)?;
        let mut transaction = repo.transaction()?;
        transaction.lock_ref("HEAD")?;
        if expected.head.reference != "HEAD" {
            transaction.lock_ref(&expected.head.reference)?;
        }
        ensure!(
            head_state(&repo)? == expected.head && self.fingerprint()? == expected.fingerprint,
            "Repository changed; reopen Checkout and review again"
        );
        if let Some((name, id)) = &expected.reference {
            ensure!(
                repo.find_reference(name)?.resolve()?.target() == Some(*id),
                "Selected revision moved; reopen Checkout"
            );
        }
        let commit = repo.find_commit(oid(&expected.id)?)?;
        transaction.set_target(
            "HEAD",
            commit.id(),
            None,
            &format!("checkout: moving to {} (detached)", expected.label),
        )?;
        // Manage LFS inside this operation: stale checks must precede dehydrate.
        let result = (|| {
            super::lfs::dehydrate(&repo)?;
            repo.checkout_tree(
                commit.as_object(),
                Some(CheckoutBuilder::new().safe().dry_run()),
            )?;
            repo.reference_ensure_log("HEAD")?;
            repo.checkout_tree(commit.as_object(), Some(CheckoutBuilder::new().safe()))?;
            transaction.commit()?;
            anyhow::Ok(())
        })();
        let hydrated = super::lfs::hydrate(&repo);
        result?;
        let missing =
            hydrated.context("HEAD detached, but LFS checkout failed; inspect the working tree")?;
        Ok(format!(
            "Detached HEAD at {} · {}{}",
            &expected.id[..8],
            expected.label,
            if missing > 0 {
                format!("; {missing} LFS objects missing (use LFS Fetch)")
            } else {
                String::new()
            }
        ))
    }
}
