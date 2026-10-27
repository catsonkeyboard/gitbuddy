//! Explicit fetch sources and server-advertised references; no shell commands.
use super::libgit::{network_callbacks, oid, raw};
use super::{NetworkControl, RemoteInfo, Repository};
use anyhow::{Context, Result, ensure};
use git2::{AutotagOption, Direction, FetchOptions, FetchPrune};
use std::path::PathBuf;

fn advertised_name<'a>(head: &'a git2::RemoteHead<'_>) -> Result<&'a str> {
    // git2 0.21 exposes no byte-oriented RemoteHead name API and unwraps
    // UTF-8 internally. Refuse unsupported names instead of losing the task.
    std::panic::catch_unwind(|| head.name()).map_err(|_| {
        anyhow::anyhow!(
            "Remote reference name is not valid UTF-8; this remote cannot be listed safely"
        )
    })
}
#[derive(Clone, Debug)]
pub struct RemoteRef {
    pub name: String,
    /// The tag object itself for annotated tags, never its peeled commit.
    pub id: String,
    pub peeled: Option<String>,
}
#[derive(Clone, Debug)]
pub struct RemoteRefs {
    git_dir: PathBuf,
    pub remote: RemoteInfo,
    pub branches: Vec<RemoteRef>,
    pub tags: Vec<RemoteRef>,
}
#[derive(Clone, Debug)]
pub struct FetchPlan {
    git_dir: PathBuf,
    pub remote: RemoteInfo,
    /// Full source reference, or None for the remote's configured fetch specs.
    pub(super) branch: Option<String>,
    pub(super) tracking: Option<String>,
}
#[derive(Clone, Debug)]
pub struct RemoteTagDelete {
    git_dir: PathBuf,
    pub remote: RemoteInfo,
    pub name: String,
    pub id: String,
}
pub(super) fn check_remote(repo: &git2::Repository, expected: &RemoteInfo) -> Result<()> {
    ensure!(
        super::remotes::remote_info(repo, &expected.name)? == *expected,
        "Remote configuration changed; query or review the remote again"
    );
    Ok(())
}
impl Repository {
    /// Query the server without fetching objects or changing local refs/tags.
    pub fn remote_refs(
        &self,
        expected: &RemoteInfo,
        control: NetworkControl,
    ) -> Result<RemoteRefs> {
        let result = (|| {
            control.check()?;
            let repo = raw(self)?;
            check_remote(&repo, expected)?;
            control.phase(&format!(
                "Query remote refs: connecting to {}…",
                expected.name
            ));
            let mut remote = repo.remote_anonymous(&expected.url)?;
            let connection = remote.connect_auth(
                Direction::Fetch,
                Some(network_callbacks(
                    control.clone(),
                    repo.config()?.snapshot()?,
                )),
                None,
            )?;
            let heads = connection.list()?;
            ensure!(
                heads.len() <= 100_000,
                "Remote advertises more than 100,000 refs; use a smaller remote"
            );
            let mut branches = Vec::new();
            let mut tags = Vec::new();
            let mut peeled = std::collections::HashMap::new();
            for head in heads {
                control.check()?;
                let name = advertised_name(head)?;
                if let Some(base) = name.strip_suffix("^{}") {
                    peeled.insert(base.to_owned(), head.oid().to_string());
                    continue;
                }
                let (prefix, destination) = if name.starts_with("refs/heads/") {
                    ("refs/heads/", &mut branches)
                } else if name.starts_with("refs/tags/") {
                    ("refs/tags/", &mut tags)
                } else {
                    continue;
                };
                ensure!(
                    git2::Reference::is_valid_name(name) && !head.oid().is_zero(),
                    "Remote advertised an invalid reference"
                );
                destination.push(RemoteRef {
                    name: name[prefix.len()..].into(),
                    id: head.oid().to_string(),
                    peeled: None,
                });
            }
            // Use a map: annotated-tag peeling must not make listing quadratic.
            for tag in &mut tags {
                tag.peeled = peeled
                    .get(format!("refs/tags/{}", tag.name).as_str())
                    .cloned();
            }
            branches.sort_by(|a, b| a.name.cmp(&b.name));
            tags.sort_by(|a, b| a.name.cmp(&b.name));
            ensure!(
                branches.windows(2).all(|w| w[0].name != w[1].name)
                    && tags.windows(2).all(|w| w[0].name != w[1].name),
                "Remote returned duplicate refs"
            );
            drop(connection);
            check_remote(&repo, expected)?;
            control.check()?;
            Ok(RemoteRefs {
                git_dir: repo.path().into(),
                remote: expected.clone(),
                branches,
                tags,
            })
        })();
        control.normalize(result)
    }
    pub fn prepare_fetch(&self, expected: &RemoteInfo, branch: Option<&str>) -> Result<FetchPlan> {
        let repo = raw(self)?;
        check_remote(&repo, expected)?;
        let branch = branch.map(|b| {
            if b.starts_with("refs/heads/") {
                b.to_owned()
            } else {
                format!("refs/heads/{b}")
            }
        });
        let tracking = if let Some(source) = &branch {
            ensure!(
                source != "refs/heads/" && git2::Reference::is_valid_name(source),
                "Select a valid remote branch"
            );
            let remote = repo.find_remote(&expected.name)?;
            let mapped = remote
                .refspecs()
                .filter(|s| s.direction() == Direction::Fetch && s.src_matches(source))
                .map(|s| {
                    s.transform(source)
                        .and_then(|b| b.as_str().map(str::to_owned))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ensure!(
                mapped.len() <= 1,
                "Selected branch matches multiple fetch mappings; fix ambiguous refspecs"
            );
            let tracking = mapped.into_iter().next();
            ensure!(
                tracking.as_ref().is_none_or(
                    |r| r.starts_with("refs/remotes/") && git2::Reference::is_valid_name(r)
                ),
                "Fetch destination must be under refs/remotes/"
            );
            tracking
        } else {
            None
        };
        Ok(FetchPlan {
            git_dir: repo.path().into(),
            remote: expected.clone(),
            branch,
            tracking,
        })
    }
    pub(super) fn check_fetch(&self, plan: &FetchPlan) -> Result<()> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == plan.git_dir,
            "Fetch source belongs to another repository"
        );
        check_remote(&repo, &plan.remote)
    }
    /// Called under the outer repository writer lock. Returns the actual
    /// downloaded source ID, rather than rereading a possibly stale tracking ref.
    pub(super) fn fetch_selected(
        &self,
        plan: &FetchPlan,
        control: NetworkControl,
    ) -> Result<Option<String>> {
        control.check()?;
        self.check_fetch(plan)?;
        let repo = raw(self)?;
        control.phase(&format!("Fetch: connecting to {}…", plan.remote.name));
        self.check_fetch(plan)?;
        let mut options = FetchOptions::new();
        options.remote_callbacks(network_callbacks(
            control.clone(),
            repo.config()?.snapshot()?,
        ));
        let id = if let Some(branch) = &plan.branch {
            // Anonymous transport avoids opportunistic updates from configured
            // refspecs. Only this destination is allowed to move; no tag auto-fetch.
            let mut remote = repo.remote_anonymous(&plan.remote.url)?;
            options
                .prune(FetchPrune::Off)
                .download_tags(AutotagOption::None);
            let spec = match &plan.tracking {
                Some(dst) => format!("+{branch}:{dst}"),
                None => branch.clone(),
            };
            remote.fetch(
                &[spec],
                Some(&mut options),
                Some("gitbuddy: selected branch fetch"),
            )?;
            let mut downloaded = None;
            for head in remote.list()? {
                if advertised_name(head)? == branch {
                    downloaded = Some(head.oid());
                }
            }
            let id = downloaded.context("Selected branch was not advertised by the remote")?;
            // A Pull source must be a commit. Fetch itself also validates this,
            // so malformed server refs cannot yield a misleading success.
            repo.find_commit(id)?;
            Some(id.to_string())
        } else {
            let mut remote = repo.find_remote(&plan.remote.name)?;
            options.prune(FetchPrune::On);
            remote.fetch(&[] as &[&str], Some(&mut options), None)?;
            None
        };
        control.check()?;
        self.check_fetch(plan)?;
        Ok(id)
    }
    pub fn prepare_remote_tag_delete(
        &self,
        refs: &RemoteRefs,
        name: &str,
    ) -> Result<RemoteTagDelete> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == refs.git_dir,
            "Remote tag query belongs to another repository"
        );
        check_remote(&repo, &refs.remote)?;
        ensure!(
            refs.remote.destination() == refs.remote.url,
            "Deleting a queried remote tag requires matching fetch and push URLs"
        );
        let tag = refs
            .tags
            .iter()
            .find(|t| t.name == name)
            .context("Query the remote tag before deleting it")?;
        ensure!(
            git2::Reference::is_valid_name(&format!("refs/tags/{name}")),
            "Invalid tag name"
        );
        oid(&tag.id)?;
        Ok(RemoteTagDelete {
            git_dir: refs.git_dir.clone(),
            remote: refs.remote.clone(),
            name: name.into(),
            id: tag.id.clone(),
        })
    }
    pub(super) fn delete_remote_tag(
        &self,
        plan: &RemoteTagDelete,
        control: NetworkControl,
    ) -> Result<String> {
        use std::sync::{Arc, Mutex};
        control.check()?;
        let repo = raw(self)?;
        ensure!(
            repo.path() == plan.git_dir,
            "Tag deletion belongs to another repository"
        );
        check_remote(&repo, &plan.remote)?;
        let target = format!("refs/tags/{}", plan.name);
        let expected = oid(&plan.id)?;
        ensure!(
            git2::Reference::is_valid_name(&target) && !expected.is_zero(),
            "Invalid remote tag deletion target"
        );
        control.phase(&format!(
            "Delete remote tag: connecting to {}…",
            plan.remote.name
        ));
        control.check()?;
        check_remote(&repo, &plan.remote)?;
        if let Some(path) =
            super::remotes::local_destination(&self.root, plan.remote.destination())?
        {
            let destination = git2::Repository::open(&path)?;
            ensure!(
                destination.is_bare(),
                "Local remote tag deletion requires a bare repository"
            );
            let mut transaction = destination.transaction()?;
            transaction.lock_ref(&target)?;
            ensure!(
                destination.find_reference(&target)?.target() == Some(expected),
                "Remote tag moved since query; query and review it again"
            );
            transaction.remove(&target)?;
            control.check()?;
            control.cancellation.finish()?;
            control.phase("Delete remote tag: updating locked reference…");
            check_remote(&repo, &plan.remote)?;
            transaction.commit()?;
        } else {
            let mut remote = repo.remote_anonymous(plan.remote.destination())?;
            let mut callbacks = network_callbacks(control.clone(), repo.config()?.snapshot()?);
            let negotiation = control.clone();
            let destination = target.clone();
            let client = self.clone();
            let selected_remote = plan.remote.clone();
            callbacks.push_negotiation(move |updates| {
                negotiation.check_git()?;
                let update = updates
                    .iter()
                    .find(|u| u.dst_refname_bytes() == destination.as_bytes())
                    .ok_or_else(|| {
                        git2::Error::from_str("Tag missing from deletion negotiation")
                    })?;
                if update.src() != expected || !update.dst().is_zero() {
                    return Err(git2::Error::from_str(
                        "Remote tag moved since query; query and review it again",
                    ));
                }
                negotiation.phase("Delete remote tag: preparing deletion…");
                check_remote(
                    &raw(&client).map_err(|e| git2::Error::from_str(&format!("{e:#}")))?,
                    &selected_remote,
                )
                .map_err(|e| git2::Error::from_str(&format!("{e:#}")))?;
                negotiation
                    .cancellation
                    .finish()
                    .map_err(|_| git2::Error::from_str("Operation cancelled"))?;
                negotiation.phase("Delete remote tag: waiting for server confirmation…");
                Ok(())
            });
            let status = Arc::new(Mutex::new(None));
            let received = status.clone();
            let name = target.clone();
            callbacks.push_update_reference(move |reference, error| {
                if reference == name {
                    *received.lock().unwrap() = Some(error.map(str::to_owned));
                }
                Ok(())
            });
            let mut options = git2::PushOptions::new();
            options.remote_callbacks(callbacks);
            remote.push(&[format!(":{target}")], Some(&mut options))?;
            control.cancellation.finish()?;
            let result = status.lock().unwrap();
            ensure!(
                result.is_some(),
                "Server did not acknowledge tag deletion; query its current state"
            );
            if let Some(Some(error)) = result.as_ref() {
                anyhow::bail!("Remote tag deletion rejected: {error}");
            }
        }
        Ok(format!(
            "Deleted remote tag {} / {}; local tags preserved",
            plan.remote.name, plan.name
        ))
    }
}
