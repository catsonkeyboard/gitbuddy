//! Remote configuration and pinned push plans, entirely through libgit2.
use super::{NetworkControl, Repository};
use anyhow::{Context, Result, bail, ensure};
use git2::{BranchType, Direction, ErrorCode, Oid};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
    pub push_url: Option<String>,
    pub fetch_refspecs: Vec<String>,
}
impl RemoteInfo {
    pub fn destination(&self) -> &str {
        self.push_url.as_deref().unwrap_or(&self.url)
    }
}
#[derive(Clone, Debug, Default)]
pub struct PushSelection {
    pub remote: String,
    /// None pushes only the explicitly selected tags.
    pub branch: Option<String>,
    pub target: String,
    pub set_upstream: bool,
    pub force_with_lease: bool,
    pub tags: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct RemoteState {
    pub remotes: Vec<RemoteInfo>,
    pub upstreams: Vec<(String, String)>,
    pub default_push: PushSelection,
}
#[derive(Clone, Debug)]
struct Update {
    source: String,
    target: String,
    id: Oid,
    /// Some(zero) leases creation of a branch that must not exist remotely.
    lease: Option<Oid>,
}
#[derive(Clone, Debug)]
pub struct PushPlan {
    remote: RemoteInfo,
    updates: Vec<Update>,
    upstream: Option<(String, String)>,
}
impl PushPlan {
    pub fn summary(&self) -> String {
        self.updates
            .iter()
            .map(|u| {
                let lease = u
                    .lease
                    .map(|id| {
                        format!(
                            " · lease {}",
                            if id.is_zero() {
                                "must not exist".into()
                            } else {
                                id.to_string()
                            }
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "{} @{} → {} / {}{lease}",
                    u.source,
                    &u.id.to_string()[..8],
                    self.remote.name,
                    u.target
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn has_lease(&self) -> bool {
        self.updates.iter().any(|u| u.lease.is_some())
    }
}

fn remote_info(repo: &git2::Repository, name: &str) -> Result<RemoteInfo> {
    let remote = repo.find_remote(name)?;
    Ok(RemoteInfo {
        name: name.into(),
        url: remote
            .url()
            .context("Remote URL is not valid UTF-8")?
            .into(),
        push_url: remote.pushurl()?.map(str::to_owned),
        fetch_refspecs: remote
            .fetch_refspecs()?
            .iter()
            .map(|s| Ok(s?.context("Invalid fetch refspec")?.to_owned()))
            .collect::<Result<_>>()?,
    })
}
fn check_remote(repo: &git2::Repository, expected: &RemoteInfo) -> Result<()> {
    ensure!(
        remote_info(repo, &expected.name)? == *expected,
        "Remote configuration changed; reopen the operation"
    );
    Ok(())
}
fn branch_ref(name: &str) -> Result<String> {
    let full = format!("refs/heads/{name}");
    ensure!(
        !name.is_empty() && !name.starts_with('-') && git2::Reference::is_valid_name(&full),
        "Enter a valid branch name"
    );
    Ok(full)
}
fn tracking_ref(repo: &git2::Repository, remote: &str, target: &str) -> Result<String> {
    let remote = repo.find_remote(remote)?;
    let mapped: Vec<_> = remote
        .refspecs()
        .filter(|spec| spec.direction() == Direction::Fetch && spec.src_matches(target))
        .map(|spec| {
            spec.transform(target)
                .map(|b| b.as_str().map(str::to_owned))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    ensure!(
        mapped.len() == 1,
        "Target must match exactly one fetch refspec; check the remote configuration"
    );
    ensure!(
        mapped[0].starts_with("refs/remotes/"),
        "Tracking ref must be under refs/remotes/"
    );
    Ok(mapped[0].clone())
}

fn local_destination(root: &std::path::Path, url: &str) -> Result<Option<std::path::PathBuf>> {
    if let Some(path) = url.strip_prefix("file://") {
        let path = path
            .strip_prefix("localhost/")
            .map(|p| format!("/{p}"))
            .unwrap_or_else(|| path.to_string());
        ensure!(
            path.starts_with('/'),
            "Local file URL must use an absolute path or localhost"
        );
        let mut bytes = Vec::new();
        let mut remaining = path.as_bytes();
        while let Some((&first, tail)) = remaining.split_first() {
            if first == b'%' {
                ensure!(tail.len() >= 2, "Invalid file URL escape");
                let value = u8::from_str_radix(std::str::from_utf8(&tail[..2])?, 16)?;
                ensure!(value != 0, "Invalid file URL path");
                bytes.push(value);
                remaining = &tail[2..];
            } else {
                bytes.push(first);
                remaining = tail;
            }
        }
        return Ok(Some(super::path_from_bytes(&bytes)));
    }
    let path = std::path::Path::new(url);
    if path.is_absolute() {
        return Ok(Some(path.to_owned()));
    }
    if url.contains(':') {
        return Ok(None);
    }
    Ok(Some(root.join(path)))
}

fn push_local_leased(
    source: &git2::Repository,
    path: &std::path::Path,
    plan: &PushPlan,
    control: &NetworkControl,
) -> Result<()> {
    use std::io::Write;
    let destination = git2::Repository::open(path)?;
    ensure!(
        destination.is_bare(),
        "Force-with-lease to a local path requires a bare repository"
    );
    let mut transaction = destination.transaction()?;
    let mut ordered: Vec<_> = plan.updates.iter().collect();
    ordered.sort_by(|a, b| a.target.cmp(&b.target));
    for update in &ordered {
        transaction.lock_ref(&update.target)?;
    }
    let mut old_ids = Vec::new();
    for update in &ordered {
        let old = match destination.find_reference(&update.target) {
            Ok(reference) => reference
                .target()
                .context("Destination ref must be direct")?,
            Err(e) if e.code() == ErrorCode::NotFound => Oid::ZERO_SHA1,
            Err(e) => return Err(e.into()),
        };
        if let Some(expected) = update.lease {
            ensure!(
                old == expected,
                "Force-with-lease rejected for {}: expected {}, remote is {}. Fetch and review before retrying.",
                update.target,
                expected,
                old
            );
        } else if update.target.starts_with("refs/tags/") {
            ensure!(
                old.is_zero() || old == update.id,
                "Push rejected: tag {} already exists",
                update.target
            );
        }
        old_ids.push(old);
    }
    control.phase("Push: preparing upload…");
    control.check()?;
    let mut pack = source.packbuilder()?;
    let mut walk = source.revwalk()?;
    for update in &ordered {
        control.check()?;
        pack.insert_recursive(update.id, None)?;
        if let Ok(commit) = source.find_object(update.id, None)?.peel_to_commit() {
            walk.push(commit.id())?;
        }
    }
    pack.insert_walk(&mut walk)?;
    pack.set_progress_callback(|_, done, total| {
        control.progress(&format!("Push: preparing {done} / {total} objects"));
        !control.cancellation.is_requested()
    })?;
    let odb = destination.odb()?;
    let mut writer = odb.packwriter()?;
    writer.progress(|stats| {
        control.progress(&format!(
            "Push: transferring {} / {} objects",
            stats.received_objects(),
            stats.total_objects()
        ));
        !control.cancellation.is_requested()
    });
    let mut write_error = None;
    let result = pack.foreach(|chunk| {
        if control.cancellation.is_requested() {
            return false;
        }
        match writer.write_all(chunk) {
            Ok(()) => true,
            Err(e) => {
                write_error = Some(e);
                false
            }
        }
    });
    control.check()?;
    if let Some(e) = write_error {
        return Err(e.into());
    }
    result?;
    writer.commit()?;
    for (update, old) in ordered.iter().zip(old_ids) {
        if update.lease.is_none() && update.target.starts_with("refs/heads/") {
            ensure!(
                old.is_zero()
                    || old == update.id
                    || destination.graph_descendant_of(update.id, old)?,
                "Push rejected: non-fast-forward for {}",
                update.target
            );
        }
        transaction.set_target(&update.target, update.id, None, "GitBuddy push")?;
    }
    control.cancellation.finish()?;
    control.phase("Push: updating locked destination refs…");
    transaction.commit()?;
    Ok(())
}

impl Repository {
    pub fn remote_state(&self) -> Result<RemoteState> {
        let repo = super::libgit::raw(self)?;
        let remotes = self.remote_info()?;
        let mut upstreams = Vec::new();
        for branch in repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let name = branch.name()?.context("Invalid branch name")?.to_string();
            let upstream = match branch.upstream() {
                Ok(upstream) => upstream.name()?.unwrap_or_default().to_string(),
                Err(e) if e.code() == ErrorCode::NotFound => String::new(),
                Err(e) => return Err(e.into()),
            };
            upstreams.push((name, upstream));
        }
        let head = repo.head().ok().filter(|h| h.is_branch());
        let branch = head
            .as_ref()
            .and_then(|h| h.shorthand().ok())
            .map(str::to_owned);
        let configured_remote = head
            .as_ref()
            .and_then(|h| repo.branch_upstream_remote(h.name().ok()?).ok())
            .and_then(|b| b.as_str().ok().map(str::to_owned));
        let configured_target = head
            .as_ref()
            .and_then(|h| repo.branch_upstream_merge(h.name().ok()?).ok())
            .and_then(|b| b.as_str().ok().map(str::to_owned));
        let remote = configured_remote
            .filter(|name| remotes.iter().any(|r| r.name == *name))
            .or_else(|| {
                remotes
                    .iter()
                    .find(|r| r.name == "origin")
                    .map(|r| r.name.clone())
            })
            .or_else(|| remotes.first().map(|r| r.name.clone()))
            .unwrap_or_default();
        let target = configured_target
            .and_then(|name| name.strip_prefix("refs/heads/").map(str::to_owned))
            .or_else(|| branch.clone())
            .unwrap_or_default();
        Ok(RemoteState {
            remotes,
            upstreams,
            default_push: PushSelection {
                remote,
                branch,
                target,
                ..PushSelection::default()
            },
        })
    }
    pub fn remote_info(&self) -> Result<Vec<RemoteInfo>> {
        let repo = super::libgit::raw(self)?;
        repo.remotes()?
            .iter()
            .map(|name| remote_info(&repo, name?.context("Invalid remote name")?))
            .collect()
    }
    pub(super) fn edit_remote(
        &self,
        expected: &RemoteInfo,
        url: &str,
        push_url: Option<&str>,
    ) -> Result<String> {
        ensure!(
            !url.trim().is_empty() && !url.contains(['\0', '\n', '\r']),
            "Enter a valid fetch URL"
        );
        ensure!(
            push_url.is_none_or(|u| !u.trim().is_empty() && !u.contains(['\0', '\n', '\r'])),
            "Enter a valid push URL or leave it blank"
        );
        let repo = super::libgit::raw(self)?;
        check_remote(&repo, expected)?;
        repo.remote_set_url(&expected.name, url)?;
        let push_result = repo.remote_set_pushurl(&expected.name, push_url);
        if let Err(error) = push_result
            && !(push_url.is_none() && error.code() == ErrorCode::NotFound)
        {
            repo.remote_set_url(&expected.name, &expected.url)
                .context("Cannot restore the old fetch URL")?;
            return Err(error.into());
        }
        Ok(format!("Updated remote {}", expected.name))
    }
    pub(super) fn rename_remote(&self, expected: &RemoteInfo, name: &str) -> Result<String> {
        ensure!(
            git2::Remote::is_valid_name(name),
            "Enter a valid remote name"
        );
        let repo = super::libgit::raw(self)?;
        check_remote(&repo, expected)?;
        let warnings = repo.remote_rename(&expected.name, name)?;
        let warnings: Vec<_> = warnings.iter().filter_map(|v| v.ok().flatten()).collect();
        Ok(if warnings.is_empty() {
            format!("Renamed {} to {name}", expected.name)
        } else {
            format!(
                "Renamed {} to {name}; manually update these custom refspecs: {}",
                expected.name,
                warnings.join(", ")
            )
        })
    }
    pub(super) fn delete_remote(&self, expected: &RemoteInfo) -> Result<String> {
        let repo = super::libgit::raw(self)?;
        check_remote(&repo, expected)?;
        repo.remote_delete(&expected.name)?;
        Ok(format!(
            "Deleted remote {} (local branches and remote server are unchanged)",
            expected.name
        ))
    }
    /// libgit2 validates and configures both local and fetched remote upstreams.
    pub(super) fn set_upstream(&self, branch: &str, upstream: Option<&str>) -> Result<String> {
        branch_ref(branch)?;
        let repo = super::libgit::raw(self)?;
        let mut local = repo.find_branch(branch, BranchType::Local)?;
        if let Some(upstream) = upstream {
            let full =
                if upstream.starts_with("refs/heads/") || upstream.starts_with("refs/remotes/") {
                    upstream.to_owned()
                } else {
                    let local = repo.find_branch(upstream, BranchType::Local).is_ok();
                    let remote = repo.find_branch(upstream, BranchType::Remote).is_ok();
                    ensure!(
                        !(local && remote),
                        "Upstream name is ambiguous; use refs/heads/ or refs/remotes/"
                    );
                    format!(
                        "refs/{}/{upstream}",
                        if local { "heads" } else { "remotes" }
                    )
                };
            let reference = repo.find_reference(&full)?;
            ensure!(
                reference.target().is_some(),
                "Upstream must be a direct branch reference"
            );
            ensure!(full != branch_ref(branch)?, "A branch cannot track itself");
            let (remote_name, merge) = if full.starts_with("refs/heads/") {
                (".".to_string(), full)
            } else {
                let remote_name = repo.branch_remote_name(&full)?.as_str()?.to_string();
                let remote = repo.find_remote(&remote_name)?;
                let mapped: Vec<_> = remote
                    .refspecs()
                    .filter(|s| s.direction() == Direction::Fetch && s.dst_matches(&full))
                    .map(|s| {
                        s.rtransform(&full)
                            .and_then(|b| b.as_str().map(str::to_owned))
                    })
                    .collect::<std::result::Result<_, _>>()?;
                ensure!(
                    mapped.len() == 1 && mapped[0].starts_with("refs/heads/"),
                    "Upstream must map to exactly one remote branch"
                );
                (remote_name, mapped[0].clone())
            };
            // Resolve and validate both values before changing configuration.
            // Keep local/remote names distinct even when their shorthands match.
            let mut config = repo.config()?.open_level(git2::ConfigLevel::Local)?;
            let remote_key = format!("branch.{branch}.remote");
            let merge_key = format!("branch.{branch}.merge");
            let previous = match config.get_string(&remote_key) {
                Ok(value) => Some(value),
                Err(e) if e.code() == ErrorCode::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            config.set_str(&remote_key, &remote_name)?;
            if let Err(e) = config.set_str(&merge_key, &merge) {
                match previous {
                    Some(value) => config.set_str(&remote_key, &value)?,
                    None => config.remove(&remote_key)?,
                }
                return Err(e.into());
            }
        } else {
            local.set_upstream(None)?;
        }
        Ok(format!(
            "Upstream for {branch}: {}",
            upstream.unwrap_or("none")
        ))
    }
    /// Capture objects, endpoint and lease before the confirmation dialog.
    /// Never fetch implicitly: fetching would silently replace the user's lease.
    pub fn prepare_push(&self, selection: PushSelection) -> Result<PushPlan> {
        let repo = super::libgit::raw(self)?;
        let remote = remote_info(&repo, &selection.remote)?;
        let mut updates = Vec::new();
        let mut upstream = None;
        if let Some(branch) = selection.branch {
            let source = branch_ref(&branch)?;
            let target = branch_ref(&selection.target)?;
            let id = repo
                .find_branch(&branch, BranchType::Local)?
                .get()
                .target()
                .context("Branch has no commit")?;
            let mapped = if selection.force_with_lease || selection.set_upstream {
                Some(tracking_ref(&repo, &selection.remote, &target)?)
            } else {
                None
            };
            let lease = if selection.force_with_lease {
                ensure!(
                    remote.destination() == remote.url,
                    "Force-with-lease requires the push URL to match the fetch URL"
                );
                let tracking = mapped.as_ref().unwrap();
                Some(match repo.find_reference(tracking) {
                    Ok(reference) => reference.target().context("Tracking ref must be direct")?,
                    Err(e) if e.code() == ErrorCode::NotFound => Oid::ZERO_SHA1,
                    Err(e) => return Err(e.into()),
                })
            } else {
                None
            };
            if selection.set_upstream {
                upstream = Some((branch, mapped.unwrap()));
            }
            updates.push(Update {
                source,
                target,
                id,
                lease,
            });
        } else {
            ensure!(
                !selection.set_upstream && !selection.force_with_lease,
                "Select a branch to set upstream or use force-with-lease"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for tag in selection.tags {
            let source = format!("refs/tags/{tag}");
            ensure!(git2::Reference::is_valid_name(&source), "Invalid tag name");
            if !seen.insert(source.clone()) {
                continue;
            }
            let id = repo
                .find_reference(&source)?
                .target()
                .context("Tag must be a direct reference")?;
            updates.push(Update {
                target: source.clone(),
                source,
                id,
                lease: None,
            });
        }
        ensure!(!updates.is_empty(), "Select a branch or at least one tag");
        Ok(PushPlan {
            remote,
            updates,
            upstream,
        })
    }
    pub(super) fn default_push(&self, control: NetworkControl) -> Result<String> {
        let repo = super::libgit::raw(self)?;
        let head = repo.head()?;
        ensure!(head.is_branch(), "Switch to a local branch before pushing");
        let source = head.name()?;
        let (remote, target, set_upstream) = match (
            repo.branch_upstream_remote(source),
            repo.branch_upstream_merge(source),
        ) {
            (Ok(remote), Ok(target)) => (
                remote.as_str().context("Invalid remote name")?.to_string(),
                target.as_str().context("Invalid upstream ref")?.to_string(),
                false,
            ),
            (Err(a), Err(b))
                if a.code() == ErrorCode::NotFound && b.code() == ErrorCode::NotFound =>
            {
                ("origin".into(), source.into(), true)
            }
            (Err(e), _) | (_, Err(e)) => {
                return Err(e).context("Upstream configuration is incomplete");
            }
        };
        ensure!(
            target.starts_with("refs/heads/"),
            "Upstream target is not a branch"
        );
        let plan = self.prepare_push(PushSelection {
            remote,
            branch: Some(head.shorthand().context("Invalid branch")?.into()),
            target: target.trim_start_matches("refs/heads/").into(),
            set_upstream,
            ..PushSelection::default()
        })?;
        self.push_plan(&plan, control)
    }
    pub(super) fn push_plan(&self, plan: &PushPlan, control: NetworkControl) -> Result<String> {
        control.check()?;
        let repo = super::libgit::raw(self)?;
        check_remote(&repo, &plan.remote)?;
        for update in &plan.updates {
            ensure!(
                repo.find_reference(&update.source)?.target() == Some(update.id),
                "Source {} changed; reopen the push dialog",
                update.source
            );
            // Pinned commits also cover annotated tags and older LFS objects.
            if let Ok(commit) = repo.find_object(update.id, None)?.peel_to_commit() {
                super::lfs::upload_before_push(
                    &repo,
                    &plan.remote.name,
                    &commit.id().to_string(),
                    &control,
                )?;
            }
        }
        if plan.has_lease()
            && let Some(path) = local_destination(&self.root, plan.remote.destination())?
        {
            // libgit2's local transport uses force-create without a CAS check.
            // Hold destination ref locks through validation and object transfer.
            push_local_leased(&repo, &path, plan, &control)?;
            for update in &plan.updates {
                if update.target.starts_with("refs/heads/")
                    && let Ok(tracking) = tracking_ref(&repo, &plan.remote.name, &update.target)
                {
                    repo.reference(&tracking, update.id, true, "GitBuddy push")
                        .context("Remote push succeeded, but local tracking ref update failed")?;
                }
            }
            if let Some((branch, upstream)) = &plan.upstream {
                self.set_upstream(branch, Some(upstream))
                    .context("Remote push succeeded, but upstream could not be saved")?;
            }
            return Ok("Push completed".into());
        }
        let mut callbacks =
            super::libgit::network_callbacks(control.clone(), repo.config()?.snapshot()?);
        let updates = plan.updates.clone();
        let negotiation = control.clone();
        callbacks.push_negotiation(move |proposed| {
            negotiation.check_git()?;
            for update in &updates {
                if update.target.starts_with("refs/tags/") {
                    let actual = proposed.iter().find(|p| p.dst_refname_bytes() == update.target.as_bytes())
                        .ok_or_else(|| git2::Error::from_str("Tag missing from push negotiation"))?;
                    if !actual.src().is_zero() && actual.src() != update.id {
                        return Err(git2::Error::from_str(&format!("Push rejected: tag {} already exists", update.target)));
                    }
                }
                if let Some(expected) = update.lease {
                    let actual = proposed.iter().find(|p| p.dst_refname_bytes() == update.target.as_bytes())
                        .ok_or_else(|| git2::Error::from_str("Lease target missing from push negotiation"))?;
                    if actual.src() != expected {
                        return Err(git2::Error::from_str(&format!(
                            "Force-with-lease rejected for {}: expected {}, remote is {}. Fetch and review the changes before retrying.",
                            update.target, expected, actual.src())));
                    }
                }
            }
            negotiation.phase("Push: preparing upload…");
            negotiation.cancellation.finish().map_err(|_| git2::Error::from_str("Operation cancelled"))?;
            negotiation.phase("Push: uploading; waiting for remote confirmation…");
            Ok(())
        });
        let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let slot = errors.clone();
        callbacks.push_update_reference(move |name, status| {
            if let Some(status) = status {
                slot.lock().unwrap().push(format!("{name}: {status}"));
            }
            Ok(())
        });
        let specs: Vec<_> = plan
            .updates
            .iter()
            .map(|u| {
                format!(
                    "{}{}:{}",
                    if u.lease.is_some() { "+" } else { "" },
                    u.id,
                    u.target
                )
            })
            .collect();
        let mut options = git2::PushOptions::new();
        options.remote_callbacks(callbacks);
        // Pin the actual push endpoint. libgit2's local transport can reopen
        // the remote's fetch URL when applying refs despite advertising pushurl.
        let anonymous = plan.remote.destination() != plan.remote.url
            && local_destination(&self.root, plan.remote.destination())?.is_some();
        let mut remote = if anonymous {
            repo.remote_anonymous(plan.remote.destination())?
        } else {
            repo.find_remote(&plan.remote.name)?
        };
        remote.push(&specs, Some(&mut options))?;
        control.cancellation.finish()?;
        let errors = errors.lock().unwrap();
        if !errors.is_empty() {
            bail!(
                "Push rejected (other refs may have succeeded): {}",
                errors.join("; ")
            );
        }
        if anonymous {
            for update in &plan.updates {
                if update.target.starts_with("refs/heads/")
                    && let Ok(tracking) = tracking_ref(&repo, &plan.remote.name, &update.target)
                {
                    repo.reference(&tracking, update.id, true, "GitBuddy push")
                        .context("Remote push succeeded, but local tracking ref update failed")?;
                }
            }
        }
        if let Some((branch, upstream)) = &plan.upstream {
            self.set_upstream(branch, Some(upstream))
                .context("Remote push succeeded, but upstream could not be saved")?;
        }
        Ok("Push completed".into())
    }
}
