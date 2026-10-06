//! Read-only, revision-pinned repository inspection.
use super::libgit::{oid, raw};
use super::{Commit, CommitFile, DIFF_LIMIT_LINES, Repository};
use anyhow::{Context, Result, ensure};
use chrono::{Local, TimeZone};
use git2::{BlameOptions, Delta, Diff, DiffFindOptions, DiffOptions, Oid, Patch, Sort, Tree};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Revision {
    pub id: String,
    pub label: String,
    pub subject: String,
}
#[derive(Clone, Debug)]
pub struct Comparison {
    pub base: Revision,
    pub target: Revision,
    pub files: Vec<CommitFile>,
    pub insertions: usize,
    pub deletions: usize,
}
#[derive(Clone, Debug)]
pub struct FileHistoryEntry {
    pub commit: Commit,
    pub file: CommitFile,
}
#[derive(Clone, Debug)]
pub struct FileHistory {
    pub revision: Revision,
    pub path: PathBuf,
    pub entries: Vec<FileHistoryEntry>,
    pub has_more: bool,
    pub scan_limited: bool,
}
#[derive(Clone, Debug)]
pub struct BlameLine {
    pub number: usize,
    pub text: String,
    pub commit_id: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub original_path: PathBuf,
    pub original_line: usize,
}
#[derive(Clone, Debug)]
pub struct BlameView {
    pub revision: Revision,
    pub path: PathBuf,
    pub lines: Vec<BlameLine>,
    pub truncated: bool,
}
#[derive(Clone, Debug)]
pub struct TreeFiles {
    pub revision: Revision,
    pub paths: Vec<PathBuf>,
    pub truncated: bool,
}
fn collect_files(
    repo: &git2::Repository,
    tree: &Tree<'_>,
    prefix: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in tree {
        if paths.len() > 5000 {
            break;
        }
        let path = prefix.join(super::path_from_bytes(entry.name_bytes()));
        if entry.kind() == Some(git2::ObjectType::Tree) {
            collect_files(repo, &repo.find_tree(entry.id())?, &path, paths)?;
        } else {
            paths.push(path);
        }
    }
    Ok(())
}

fn date(seconds: i64) -> String {
    Local
        .timestamp_opt(seconds, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}
fn relative_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "Choose a repository-relative file path without '..'."
    );
    Ok(())
}
fn revision(repo: &git2::Repository, spec: &str) -> Result<Revision> {
    ensure!(!spec.trim().is_empty(), "Enter a branch, tag or commit ID.");
    let commit = repo
        .revparse_single(spec)?
        .peel_to_commit()
        .context("The revision must identify a commit.")?;
    Ok(Revision {
        id: commit.id().to_string(),
        label: spec.into(),
        subject: commit.summary().ok().flatten().unwrap_or_default().into(),
    })
}
pub(super) fn tree_diff<'a>(
    repo: &'a git2::Repository,
    old: Option<&Tree<'a>>,
    new: &Tree<'a>,
) -> Result<Diff<'a>> {
    tree_diff_context(repo, old, new, false, &[])
}
pub(super) fn tree_diff_context<'a>(
    repo: &'a git2::Repository,
    old: Option<&Tree<'a>>,
    new: &Tree<'a>,
    full: bool,
    paths: &[&Path],
) -> Result<Diff<'a>> {
    let mut options = DiffOptions::new();
    options.context_lines(if full { u32::MAX } else { 3 });
    options.include_typechange(true);
    if full {
        options.disable_pathspec_match(true);
        for path in paths {
            options.pathspec(*path);
        }
    }
    let mut diff = repo.diff_tree_to_tree(old, Some(new), Some(&mut options))?;
    diff.find_similar(Some(
        DiffFindOptions::new()
            .renames(true)
            .renames_from_rewrites(true)
            .break_rewrites(true)
            .break_rewrites_for_renames_only(true),
    ))?;
    Ok(diff)
}
fn changed_file(delta: git2::DiffDelta<'_>) -> CommitFile {
    CommitFile {
        path: delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .unwrap_or(Path::new(""))
            .to_path_buf(),
        original: (delta.status() == Delta::Renamed)
            .then(|| delta.old_file().path().map(Path::to_path_buf))
            .flatten(),
        status: match delta.status() {
            Delta::Added => 'A',
            Delta::Deleted => 'D',
            Delta::Renamed => 'R',
            Delta::Copied => 'C',
            Delta::Typechange => 'T',
            _ => 'M',
        },
    }
}

/// Match a delta's new path and rename source together, rather than a pathspec
/// that can also select a different file reusing the rename's old name.
pub(super) fn single_file_patch(diff: &Diff<'_>, file: &CommitFile) -> Result<String> {
    for (i, delta) in diff.deltas().enumerate() {
        let actual = changed_file(delta);
        if actual.path != file.path
            || actual.original != file.original
            || actual.status != file.status
        {
            continue;
        }
        return if let Some(mut patch) = Patch::from_diff(diff, i)? {
            Ok(String::from_utf8_lossy(patch.to_buf()?.as_ref()).into_owned())
        } else {
            Ok(format!("Binary files differ: {}\n", file.path.display()))
        };
    }
    anyhow::bail!("The file is not part of this comparison.")
}
fn entry(tree: &Tree<'_>, path: &Path) -> Result<Option<(Oid, i32)>> {
    match tree.get_path(path) {
        Ok(e) => Ok(Some((e.id(), e.filemode()))),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn metadata(commit: &git2::Commit<'_>) -> Commit {
    Commit {
        id: commit.id().to_string(),
        parents: commit.parent_ids().map(|id| id.to_string()).collect(),
        subject: commit.summary().ok().flatten().unwrap_or_default().into(),
        author: commit.author().name().unwrap_or_default().into(),
        date: date(commit.time().seconds()),
        refs: String::new(),
    }
}

impl Repository {
    pub fn resolve_revision(&self, spec: &str) -> Result<Revision> {
        revision(&raw(self)?, spec)
    }
    pub fn revision_files(&self, spec: &str) -> Result<TreeFiles> {
        let repo = raw(self)?;
        let revision = revision(&repo, spec)?;
        let tree = repo.find_commit(oid(&revision.id)?)?.tree()?;
        let mut paths = Vec::new();
        collect_files(&repo, &tree, Path::new(""), &mut paths)?;
        let truncated = paths.len() > 5000;
        paths.truncate(5000);
        Ok(TreeFiles {
            revision,
            paths,
            truncated,
        })
    }

    pub fn compare(&self, base: &str, target: &str) -> Result<Comparison> {
        let repo = raw(self)?;
        let base = revision(&repo, base)?;
        let target = revision(&repo, target)?;
        let old = repo.find_commit(oid(&base.id)?)?.tree()?;
        let new = repo.find_commit(oid(&target.id)?)?.tree()?;
        let diff = tree_diff(&repo, Some(&old), &new)?;
        let stats = diff.stats()?;
        Ok(Comparison {
            base,
            target,
            insertions: stats.insertions(),
            deletions: stats.deletions(),
            files: diff.deltas().map(changed_file).collect(),
        })
    }

    pub fn comparison_file_diff(
        &self,
        comparison: &Comparison,
        file: &CommitFile,
    ) -> Result<String> {
        self.comparison_file_diff_context(comparison, file, false)
    }
    pub fn comparison_file_diff_context(
        &self,
        comparison: &Comparison,
        file: &CommitFile,
        full: bool,
    ) -> Result<String> {
        let repo = raw(self)?;
        let old = repo.find_commit(oid(&comparison.base.id)?)?.tree()?;
        let new = repo.find_commit(oid(&comparison.target.id)?)?.tree()?;
        if full {
            let mut paths = vec![file.path.as_path()];
            if let Some(old) = &file.original {
                paths.push(old);
            }
            super::diff_view::check_full_tree(&repo, Some(&old), &paths)?;
            super::diff_view::check_full_tree(&repo, Some(&new), &paths)?;
        }
        let paths = std::iter::once(file.path.as_path())
            .chain(file.original.as_deref())
            .collect::<Vec<_>>();
        single_file_patch(
            &tree_diff_context(&repo, Some(&old), &new, full, &paths)?,
            file,
        )
    }

    pub fn file_history(&self, path: &Path, spec: &str, limit: usize) -> Result<FileHistory> {
        relative_path(path)?;
        ensure!(
            (1..=2000).contains(&limit),
            "History page size must be between 1 and 2000."
        );
        let repo = raw(self)?;
        let revision = revision(&repo, spec)?;
        let start = oid(&revision.id)?;
        let mut walk = repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        walk.push(start)?;
        // A merge can have different historical names on each parent. Carry
        // paths along each edge of the DAG; never apply one rename globally.
        let mut paths: HashMap<Oid, BTreeSet<PathBuf>> =
            HashMap::from([(start, BTreeSet::from([path.to_path_buf()]))]);
        let mut entries = Vec::new();
        let mut scan_limited = false;
        for (scanned, id) in walk.enumerate() {
            if scanned >= 50_000 {
                scan_limited = true;
                break;
            }
            let id = id?;
            let Some(names) = paths.remove(&id) else {
                continue;
            };
            let commit = repo.find_commit(id)?;
            let tree = commit.tree()?;
            let parents = commit
                .parents()
                .map(|parent| parent.tree().map(|tree| (parent.id(), tree)))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut diffs: HashMap<usize, Diff<'_>> = HashMap::new();
            for name in names {
                let current = entry(&tree, &name)?;
                let first = parents
                    .first()
                    .map(|(_, t)| entry(t, &name))
                    .transpose()?
                    .flatten();
                if current != first {
                    let diff = match diffs.entry(0) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(tree_diff(&repo, parents.first().map(|(_, t)| t), &tree)?)
                        }
                    };
                    if let Some(file) = diff.deltas().map(changed_file).find(|f| {
                        f.path == name || (current.is_none() && f.original.as_ref() == Some(&name))
                    }) {
                        entries.push(FileHistoryEntry {
                            commit: metadata(&commit),
                            file,
                        });
                        if entries.len() > limit {
                            break;
                        }
                    }
                }
                for (i, (parent_id, parent_tree)) in parents.iter().enumerate() {
                    let previous = entry(parent_tree, &name)?;
                    let mut old_name = name.clone();
                    if current != previous {
                        let diff = match diffs.entry(i) {
                            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                            std::collections::hash_map::Entry::Vacant(e) => {
                                e.insert(tree_diff(&repo, Some(parent_tree), &tree)?)
                            }
                        };
                        if let Some(file) = diff.deltas().map(changed_file).find(|f| {
                            f.path == name
                                || (current.is_none() && f.original.as_ref() == Some(&name))
                        }) {
                            // An add starts a new incarnation of this name.
                            if file.status == 'A' {
                                continue;
                            }
                            if let Some(original) = file.original {
                                old_name = original;
                            }
                        }
                    }
                    paths.entry(*parent_id).or_default().insert(old_name);
                }
            }
            if entries.len() > limit {
                break;
            }
        }
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(FileHistory {
            revision,
            path: path.to_path_buf(),
            entries,
            has_more,
            scan_limited,
        })
    }

    /// Blame the committed blob at a pinned revision, not the current checkout.
    pub fn blame(&self, path: &Path, spec: &str) -> Result<BlameView> {
        relative_path(path)?;
        let repo = raw(self)?;
        let revision = revision(&repo, spec)?;
        let id = oid(&revision.id)?;
        let tree = repo.find_commit(id)?.tree()?;
        let file = tree
            .get_path(path)
            .context("The file does not exist at this revision.")?;
        ensure!(
            matches!(file.filemode(), 0o100644 | 0o100755),
            "Blame is available for regular text files only."
        );
        let blob = repo.find_blob(file.id())?;
        ensure!(
            blob.size() <= 2 * 1024 * 1024,
            "Blame preview is limited to files up to 2 MB."
        );
        ensure!(
            !blob.is_binary(),
            "Binary files do not have a text blame preview."
        );
        let content = blob.content();
        // split_inclusive avoids a phantom last line and keeps no-newline files.
        let rows: Vec<_> = content
            .split_inclusive(|b| *b == b'\n')
            .take(DIFF_LIMIT_LINES + 1)
            .collect();
        let truncated = rows.len() > DIFF_LIMIT_LINES;
        let count = rows.len().min(DIFF_LIMIT_LINES);
        let mut options = BlameOptions::new();
        options.newest_commit(id).max_line(count.max(1));
        let blame = if count > 0 {
            Some(repo.blame_file(path, Some(&mut options))?)
        } else {
            None
        };
        let mut lines = Vec::with_capacity(count);
        for (i, row) in rows.into_iter().take(count).enumerate() {
            let hunk = blame
                .as_ref()
                .and_then(|b| b.get_line(i + 1))
                .context("Missing blame attribution.")?;
            let signature = hunk.final_signature();
            let row = row.strip_suffix(b"\n").unwrap_or(row);
            let row = row.strip_suffix(b"\r").unwrap_or(row);
            lines.push(BlameLine {
                number: i + 1,
                text: String::from_utf8_lossy(row).into_owned(),
                commit_id: hunk.final_commit_id().to_string(),
                author: signature
                    .as_ref()
                    .and_then(|s| s.name().ok())
                    .unwrap_or_default()
                    .into(),
                date: signature
                    .as_ref()
                    .map(|s| date(s.when().seconds()))
                    .unwrap_or_default(),
                subject: hunk.summary().ok().flatten().unwrap_or_default().into(),
                original_path: hunk.path().unwrap_or(path).to_path_buf(),
                original_line: hunk.orig_start_line() + i + 1 - hunk.final_start_line(),
            });
        }
        Ok(BlameView {
            revision,
            path: path.to_path_buf(),
            lines,
            truncated,
        })
    }
}
