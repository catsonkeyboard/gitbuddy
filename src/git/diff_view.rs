//! Presentation data is computed on the worker, never during window layout.
use super::DiffLine;
use std::ops::Range;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DiffPreferences {
    pub side_by_side: bool,
    pub word_highlight: bool,
    pub full_context: bool,
}
impl Default for DiffPreferences {
    fn default() -> Self {
        Self {
            side_by_side: false,
            word_highlight: true,
            full_context: false,
        }
    }
}
pub const FULL_DIFF_LINES: usize = 100_000;
pub const FULL_DIFF_BYTES: usize = 16 * 1024 * 1024;
pub(super) fn check_full_diff(bytes: &[u8]) -> anyhow::Result<()> {
    let lines = bytes.iter().filter(|&&b| b == b'\n').count()
        + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    anyhow::ensure!(
        bytes.len() <= FULL_DIFF_BYTES && lines <= FULL_DIFF_LINES,
        "Full context exceeds the safety limit (100,000 lines / 16 MB). Use compact context or an external editor."
    );
    Ok(())
}
pub(super) fn check_full_blob(repo: &git2::Repository, id: git2::Oid) -> anyhow::Result<()> {
    let (size, kind) = repo.odb()?.read_header(id)?;
    anyhow::ensure!(
        kind != git2::ObjectType::Blob || size <= FULL_DIFF_BYTES,
        "Full context exceeds the safety limit (16 MB per file). Use compact context or an external editor."
    );
    Ok(())
}
pub(super) fn check_full_tree(
    repo: &git2::Repository,
    tree: Option<&git2::Tree<'_>>,
    paths: &[&std::path::Path],
) -> anyhow::Result<()> {
    if let Some(tree) = tree {
        for path in paths {
            match tree.get_path(path) {
                Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => {
                    check_full_blob(repo, entry.id())?
                }
                Ok(_) => {}
                Err(e) if e.code() == git2::ErrorCode::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SplitRow {
    /// Canonical patch row indices; selections always use these, not display rows.
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub header: bool,
}
#[derive(Clone, Debug, Default)]
pub struct DiffDisplay {
    pub rows: Vec<SplitRow>,
    /// UTF-8 byte ranges in the original DiffLine::text, including its prefix.
    pub highlights: Vec<Vec<Range<usize>>>,
    pub columns: usize,
    pub measure_row: usize,
    pub split_measure_row: usize,
}
impl DiffDisplay {
    pub fn new(lines: &[DiffLine]) -> Self {
        let mut view = Self {
            rows: Vec::new(),
            highlights: vec![Vec::new(); lines.len()],
            columns: lines
                .iter()
                .filter(|l| matches!(l.kind, '+' | '-' | ' '))
                .map(|l| {
                    l.text
                        .chars()
                        .map(|c| {
                            if c == '\t' {
                                4
                            } else if c.is_ascii() {
                                1
                            } else {
                                2
                            }
                        })
                        .sum()
                })
                .max()
                .unwrap_or(0),
            measure_row: lines
                .iter()
                .enumerate()
                .max_by_key(|(_, line)| line.text.len())
                .map(|(i, _)| i)
                .unwrap_or(0),
            split_measure_row: 0,
        };
        let mut i = 0;
        let mut budget = 2_000_000usize;
        while i < lines.len() {
            if matches!(lines[i].kind, '-' | '+') {
                let start = i;
                while i < lines.len() && matches!(lines[i].kind, '-' | '+') {
                    i += 1;
                }
                let old: Vec<_> = (start..i).filter(|&r| lines[r].kind == '-').collect();
                let new: Vec<_> = (start..i).filter(|&r| lines[r].kind == '+').collect();
                for n in 0..old.len().max(new.len()) {
                    let left = old.get(n).copied();
                    let right = new.get(n).copied();
                    if let (Some(a), Some(b)) = (left, right) {
                        let old = &lines[a].text[1..];
                        let new = &lines[b].text[1..];
                        let cost = old.len().saturating_mul(new.len()).min(65_536);
                        let (deleted, added) = if cost <= budget {
                            budget -= cost;
                            word_changes(old, new)
                        } else {
                            whole_changes(old, new)
                        };
                        view.highlights[a] = deleted
                            .into_iter()
                            .map(|r| r.start + 1..r.end + 1)
                            .collect();
                        view.highlights[b] =
                            added.into_iter().map(|r| r.start + 1..r.end + 1).collect();
                    }
                    view.rows.push(SplitRow {
                        left,
                        right,
                        header: false,
                    });
                }
            } else {
                view.rows.push(SplitRow {
                    left: Some(i),
                    right: Some(i),
                    header: lines[i].kind != ' ',
                });
                i += 1;
            }
        }
        view.split_measure_row = view.rows.iter().position(|row| !row.header).unwrap_or(0);
        view
    }
}

fn tokens(text: &str) -> Vec<Range<usize>> {
    let mut result: Vec<Range<usize>> = Vec::new();
    let mut previous = None;
    for (start, c) in text.char_indices() {
        let kind = if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        };
        if kind != 2 && previous == Some(kind) {
            result.last_mut().unwrap().end = start + c.len_utf8();
        } else {
            result.push(start..start + c.len_utf8());
        }
        previous = Some(kind);
        if result.len() > 512 {
            break;
        }
    }
    result
}
fn ranges(tokens: &[Range<usize>], matched: &[bool]) -> Vec<Range<usize>> {
    let mut result: Vec<Range<usize>> = Vec::new();
    for (range, keep) in tokens.iter().zip(matched) {
        if *keep {
            continue;
        }
        if let Some(last) = result.last_mut()
            && last.end == range.start
        {
            last.end = range.end;
        } else {
            result.push(range.clone());
        }
    }
    result
}
/// Bounded token LCS: long/minified lines fall back to a single changed range.
pub fn word_changes(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old == new {
        return (Vec::new(), Vec::new());
    }
    let a = tokens(old);
    let b = tokens(new);
    if a.len() > 512 || b.len() > 512 || a.len().saturating_mul(b.len()) > 65_536 {
        return whole_changes(old, new);
    }
    let width = b.len() + 1;
    let mut lcs = vec![0u16; (a.len() + 1) * width];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * width + j] = if old[a[i].clone()] == new[b[j].clone()] {
                1 + lcs[(i + 1) * width + j + 1]
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let mut am = vec![false; a.len()];
    let mut bm = vec![false; b.len()];
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if old[a[i].clone()] == new[b[j].clone()] {
            am[i] = true;
            bm[j] = true;
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (ranges(&a, &am), ranges(&b, &bm))
}
fn whole_changes(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    (
        std::iter::once(0..old.len())
            .filter(|r| !r.is_empty())
            .collect(),
        std::iter::once(0..new.len())
            .filter(|r| !r.is_empty())
            .collect(),
    )
}
