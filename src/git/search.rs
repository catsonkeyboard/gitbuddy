//! Bounded, cancellable search across commits reachable from repository references.
use super::{
    CancellationToken, Commit, Repository,
    libgit::{raw, refs_by_oid, relative_time},
};
use anyhow::{Context, Result, ensure};
use chrono::{Duration, NaiveDate};
use git2::Sort;

#[derive(Clone, Debug)]
pub struct HistorySearchResult {
    pub commits: Vec<Commit>,
    pub scanned: usize,
    pub truncated: bool,
}

#[derive(Default)]
struct SearchQuery {
    text: Vec<String>,
    author: Option<String>,
    path: Option<String>,
    reference: Option<String>,
    after: Option<i64>,
    before: Option<i64>,
}
impl SearchQuery {
    fn parse(input: &str) -> Result<Self> {
        ensure!(!input.trim().is_empty(), "Enter search text");
        ensure!(input.len() <= 1024, "Search text is too long");
        let mut result = Self::default();
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quoted = false;
        for ch in input.chars() {
            if ch == '"' {
                quoted = !quoted;
            } else if ch.is_whitespace() && !quoted {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            } else {
                word.push(ch);
            }
        }
        ensure!(!quoted, "Unclosed quote in search query");
        if !word.is_empty() {
            words.push(word);
        }
        ensure!(!words.is_empty(), "Enter search text");
        for word in &words {
            let Some((key, value)) = word.split_once(':') else {
                result.text.push(word.to_lowercase());
                continue;
            };
            match key.to_ascii_lowercase().as_str() {
                "author" | "path" | "ref" => {
                    ensure!(!value.is_empty(), "Empty {key} filter");
                    let slot = match key.to_ascii_lowercase().as_str() {
                        "author" => &mut result.author,
                        "path" => &mut result.path,
                        _ => &mut result.reference,
                    };
                    ensure!(slot.is_none(), "Duplicate {key} filter");
                    *slot = Some(if key.eq_ignore_ascii_case("ref") {
                        value.to_owned()
                    } else {
                        value.to_lowercase()
                    });
                }
                "after" | "before" => {
                    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
                        .context("Dates must use YYYY-MM-DD")?;
                    let (slot, boundary) = if key.eq_ignore_ascii_case("after") {
                        (&mut result.after, date)
                    } else {
                        (
                            &mut result.before,
                            date.checked_add_signed(Duration::days(1))
                                .context("Date out of range")?,
                        )
                    };
                    ensure!(slot.is_none(), "Duplicate {key} filter");
                    *slot = Some(
                        boundary
                            .and_hms_opt(0, 0, 0)
                            .context("Invalid date")?
                            .and_utc()
                            .timestamp(),
                    );
                }
                _ => result.text.push(word.to_lowercase()),
            }
        }
        if let (Some(after), Some(before)) = (result.after, result.before) {
            ensure!(after < before, "Date range is empty");
        }
        Ok(result)
    }
}

fn touches_path(
    repo: &git2::Repository,
    commit: &git2::Commit<'_>,
    pattern: &str,
    cancel: &CancellationToken,
) -> Result<bool> {
    let tree = commit.tree()?;
    let count = commit.parent_count().max(1);
    for i in 0..count {
        cancel.check()?;
        let parent_tree = if commit.parent_count() == 0 {
            None
        } else {
            Some(commit.parent(i)?.tree()?)
        };
        let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
        for delta in diff.deltas() {
            cancel.check()?;
            if [delta.old_file().path(), delta.new_file().path()]
                .into_iter()
                .flatten()
                .any(|p| p.to_string_lossy().to_lowercase().contains(pattern))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

impl Repository {
    pub fn search_history(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> Result<HistorySearchResult> {
        let query = SearchQuery::parse(query)?;
        let repo = raw(self)?;
        let refs = refs_by_oid(&repo)?;
        let mut walk = repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        if let Some(reference) = &query.reference {
            let commit = repo
                .revparse_single(reference)
                .context("Search reference not found")?
                .peel_to_commit()
                .context("Search reference does not resolve to a commit")?;
            walk.push(commit.id())?;
        } else {
            for id in refs.keys() {
                walk.push(*id)?;
            }
            if let Ok(head) = repo.head()
                && let Some(id) = head.target()
            {
                walk.push(id)?;
            }
        }
        let mut result = HistorySearchResult {
            commits: Vec::new(),
            scanned: 0,
            truncated: false,
        };
        for entry in walk {
            cancel.check()?;
            if result.scanned >= 100_000 || result.commits.len() >= 1_000 {
                result.truncated = true;
                break;
            }
            let commit = repo.find_commit(entry?)?;
            result.scanned += 1;
            let time = commit.time().seconds();
            if query.after.is_some_and(|n| time < n) || query.before.is_some_and(|n| time >= n) {
                continue;
            }
            let signature = commit.author();
            let author = signature.name().unwrap_or_default().to_owned();
            if query.author.as_ref().is_some_and(|q| {
                !format!("{} {}", author, signature.email().unwrap_or_default())
                    .to_lowercase()
                    .contains(q)
            }) {
                continue;
            }
            let names = refs
                .get(&commit.id())
                .cloned()
                .unwrap_or_default()
                .join(", ");
            let id = commit.id().to_string();
            let subject = commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_owned();
            let fields = [&id, &subject, &author, &names].map(|s| s.to_lowercase());
            if !query
                .text
                .iter()
                .all(|word| fields.iter().any(|field| field.contains(word)))
            {
                continue;
            }
            if let Some(path) = &query.path
                && !touches_path(&repo, &commit, path, cancel)?
            {
                continue;
            }
            result.commits.push(Commit {
                id,
                parents: commit.parent_ids().map(|p| p.to_string()).collect(),
                subject,
                author,
                date: relative_time(time),
                refs: names,
            });
        }
        Ok(result)
    }
}
