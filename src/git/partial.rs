//! Partial staging uses libgit2's structured diff, never the lossy display text.
//! Reconstruct just the index blob; the working tree is never written.
use super::libgit::{diff_for_context, head_tree, raw};
use super::{DIFF_LIMIT_LINES, DiffLine, FileChange, Repository, diff_preview};
use anyhow::{Context, Result, ensure};
use git2::{Delta, DiffFindOptions, FileMode, IndexEntry, IndexTime, Oid, Patch};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

#[derive(Clone, Debug)]
pub struct FilePatch {
    pub lines: Vec<DiffLine>,
    pub display: super::DiffDisplay,
    pub partial: Option<Arc<PartialPatch>>,
    pub unavailable: Option<String>,
}

impl FilePatch {
    pub fn read_only(text: &str) -> Self {
        let lines = diff_preview(text);
        Self {
            display: super::DiffDisplay::new(&lines),
            lines,
            partial: None,
            unavailable: None,
        }
    }
    pub fn read_only_context(text: &str, full: bool) -> Result<Self> {
        if !full {
            return Ok(Self::read_only(text));
        }
        super::diff_view::check_full_diff(text.as_bytes())?;
        let lines = super::diff_lines_with_limit(text, super::diff_view::FULL_DIFF_LINES);
        Ok(Self {
            display: super::DiffDisplay::new(&lines),
            lines,
            partial: None,
            unavailable: None,
        })
    }
}

#[derive(Clone, Debug)]
pub enum PatchSelection {
    /// Row of the hunk header in FilePatch::lines.
    Hunk(usize),
    /// Rows of additions/deletions in FilePatch::lines (context is not selectable).
    Lines(Vec<usize>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EntryVersion {
    id: Oid,
    mode: u32,
    flags: u16,
    extended: u16,
}

fn entry_version(entry: Option<IndexEntry>) -> Option<EntryVersion> {
    entry.map(|e| EntryVersion {
        id: e.id,
        mode: e.mode,
        flags: e.flags,
        extended: e.flags_extended,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PatchLine {
    row: usize,
    kind: char,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Hunk {
    row: usize,
    base_start: usize,
    lines: Vec<PatchLine>,
}

/// An immutable selection source, tied to the repository and index version
/// shown to the user. Fields stay private so callers cannot forge a patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialPatch {
    git_dir: PathBuf,
    file: FileChange,
    staged: bool,
    full_context: bool,
    index: Option<EntryVersion>,
    head: Option<Oid>,
    diff: Vec<u8>,
    path_bytes: Vec<u8>,
    mode: u32,
    remove_when_empty: bool,
    hunks: Vec<Hunk>,
}

impl Repository {
    pub fn worktree_patch(&self, file: &FileChange, staged: bool) -> Result<FilePatch> {
        self.worktree_patch_context(file, staged, false)
    }
    pub fn worktree_patch_context(
        &self,
        file: &FileChange,
        staged: bool,
        full_context: bool,
    ) -> Result<FilePatch> {
        if file.index == '?' && !staged {
            ensure!(
                std::fs::symlink_metadata(self.root.join(&file.path))?.len() <= 2 * 1024 * 1024,
                "Untracked file is larger than 2 MB"
            );
        }
        let repo = raw(self)?;
        let index = repo.index()?;
        if super::lfs::tracked(&repo, &file.path)? {
            return Ok(FilePatch {
                unavailable: Some("LFS files must be staged or unstaged as a whole file.".into()),
                ..FilePatch::read_only(
                    "LFS content is stored separately; use the file's Stage / Unstage action.",
                )
            });
        }
        let version = entry_version(index.get_path(&file.path, 0));
        if full_context {
            if let Some(entry) = &version
                && entry.mode != 0o160000
            {
                super::diff_view::check_full_blob(&repo, entry.id)?;
            }
            if staged {
                super::diff_view::check_full_tree(
                    &repo,
                    head_tree(&repo)?.as_ref(),
                    &[&file.path],
                )?;
            } else if let Ok(meta) = std::fs::symlink_metadata(self.root.join(&file.path)) {
                ensure!(
                    meta.len() <= super::diff_view::FULL_DIFF_BYTES as u64,
                    "Full context exceeds the safety limit (16 MB per file). Use compact context or an external editor."
                );
            }
        }
        let readonly = |reason: &str| -> Result<FilePatch> {
            let mut patch = FilePatch::read_only_context(
                &self.diff_context(file, staged, full_context)?,
                full_context,
            )?;
            patch.unavailable = Some(reason.into());
            Ok(patch)
        };
        if file.conflict() || (1..=3).any(|stage| index.get_path(&file.path, stage).is_some()) {
            return readonly("Resolve conflicts first; use the file's Stage action.");
        }
        if file.original.is_some() {
            return readonly("Renames must be staged or unstaged as a whole file.");
        }
        let mut diff = diff_for_context(&repo, staged, full_context, &[file.path.as_path()])?;
        diff.find_similar(Some(DiffFindOptions::new().renames(true)))?;
        let Some((delta_index, delta)) = diff.deltas().enumerate().find(|(_, d)| {
            d.new_file().path() == Some(file.path.as_path())
                || d.old_file().path() == Some(file.path.as_path())
        }) else {
            return readonly("No text changes to select.");
        };
        let ordinary_mode = |mode| {
            matches!(
                mode,
                FileMode::Unreadable | FileMode::Blob | FileMode::BlobExecutable
            )
        };
        if !matches!(
            delta.status(),
            Delta::Modified | Delta::Added | Delta::Deleted | Delta::Untracked
        ) || !ordinary_mode(delta.old_file().mode())
            || !ordinary_mode(delta.new_file().mode())
        {
            return readonly("Renames, symlinks and submodules require whole-file operations.");
        }
        let Some(mut patch) = Patch::from_diff(&diff, delta_index)? else {
            return readonly("Binary files require whole-file operations.");
        };
        if patch.num_hunks() == 0 {
            return readonly(
                "No text hunks; use the whole-file action for binary or metadata changes.",
            );
        }
        let bytes = patch.to_buf()?.to_vec();
        if full_context {
            super::diff_view::check_full_diff(&bytes)?;
        }
        if !full_context && bytes.iter().filter(|&&b| b == b'\n').count() > DIFF_LIMIT_LINES {
            return readonly("Partial actions are unavailable for a truncated diff preview.");
        }
        // Keep file headers for orientation, then construct rows directly from
        // libgit2 so CRLF, non-UTF8 and missing final newlines remain lossless.
        let header: String = String::from_utf8_lossy(&bytes)
            .lines()
            .take_while(|l| !l.starts_with("@@"))
            .map(|l| format!("{l}\n"))
            .collect();
        let mut lines = diff_preview(&header);
        let mut hunks = Vec::new();
        for h in 0..patch.num_hunks() {
            let (info, count) = patch.hunk(h)?;
            let row = lines.len();
            lines.push(DiffLine {
                old: String::new(),
                new: String::new(),
                kind: '@',
                text: String::from_utf8_lossy(info.header()).trim_end().into(),
            });
            let (start, length) = if staged {
                (info.new_start(), info.new_lines())
            } else {
                (info.old_start(), info.old_lines())
            };
            let mut hunk = Hunk {
                row,
                base_start: if length == 0 {
                    start
                } else {
                    start.saturating_sub(1)
                } as usize,
                lines: Vec::new(),
            };
            for l in 0..count {
                let line = patch.line_in_hunk(h, l)?;
                let kind = line.origin();
                let text = String::from_utf8_lossy(line.content());
                if matches!(kind, '+' | '-' | ' ') {
                    hunk.lines.push(PatchLine {
                        row: lines.len(),
                        kind,
                        bytes: line.content().to_vec(),
                    });
                    lines.push(DiffLine {
                        old: line.old_lineno().map(|n| n.to_string()).unwrap_or_default(),
                        new: line.new_lineno().map(|n| n.to_string()).unwrap_or_default(),
                        text: format!("{kind}{}", text.trim_end_matches(['\r', '\n'])),
                        kind,
                    });
                } else {
                    lines.push(DiffLine {
                        old: String::new(),
                        new: String::new(),
                        kind: 'h',
                        text: text.trim().into(),
                    });
                }
            }
            hunks.push(hunk);
        }
        let mode = version.as_ref().map(|e| e.mode).unwrap_or_else(|| {
            if staged {
                delta.old_file().mode().into()
            } else {
                delta.new_file().mode().into()
            }
        });
        let partial = PartialPatch {
            git_dir: repo.path().to_path_buf(),
            file: file.clone(),
            staged,
            full_context,
            index: version,
            head: if staged {
                head_tree(&repo)?.map(|tree| tree.id())
            } else {
                None
            },
            diff: bytes,
            path_bytes: delta
                .new_file()
                .path_bytes()
                .or_else(|| delta.old_file().path_bytes())
                .context("Missing file path")?
                .to_vec(),
            mode,
            remove_when_empty: if staged {
                delta.status() == Delta::Added
            } else {
                delta.status() == Delta::Deleted
            },
            hunks,
        };
        Ok(FilePatch {
            display: super::DiffDisplay::new(&lines),
            lines,
            partial: Some(Arc::new(partial)),
            unavailable: None,
        })
    }

    pub(super) fn apply_partial(
        &self,
        patch: &PartialPatch,
        selection: &PatchSelection,
    ) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == patch.git_dir,
            "This diff belongs to a different repository. Refresh and select again."
        );
        let current = self.worktree_patch_context(&patch.file, patch.staged, patch.full_context)?;
        ensure!(
            current.partial.as_deref() == Some(patch),
            "The diff has changed. Refresh and select the changes again."
        );
        let mut index = repo.index()?;
        index.read(true)?;
        ensure!(
            entry_version(index.get_path(&patch.file.path, 0)) == patch.index
                && (1..=3).all(|stage| index.get_path(&patch.file.path, stage).is_none()),
            "The index has changed. Refresh and select the changes again."
        );
        let base = patch
            .index
            .as_ref()
            .map(|e| repo.find_blob(e.id).map(|b| b.content().to_vec()))
            .transpose()?
            .unwrap_or_default();
        let selected = patch.selected_rows(selection)?;
        let content = patch.selected_content(&base, &selected)?;
        if content.is_empty() && patch.remove_when_empty {
            index.remove_path(&patch.file.path)?;
        } else {
            // Invalidate stat data: it describes the worktree, not this partial
            // blob. Retaining it can hide the remaining unstaged changes.
            let entry = IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: patch.mode,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: Oid::ZERO_SHA1,
                flags: 0,
                flags_extended: 0,
                path: patch.path_bytes.clone(),
            };
            index.add_frombuffer(&entry, &content)?;
        }
        index.write()?;
        Ok(format!(
            "{} changed lines {}",
            selected.len(),
            if patch.staged { "unstaged" } else { "staged" }
        ))
    }
}

impl PartialPatch {
    fn selected_rows(&self, selection: &PatchSelection) -> Result<BTreeSet<usize>> {
        let changes: BTreeSet<_> = self
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| matches!(l.kind, '+' | '-'))
            .map(|l| l.row)
            .collect();
        let selected: BTreeSet<_> = match selection {
            PatchSelection::Hunk(row) => self
                .hunks
                .iter()
                .find(|h| h.row == *row)
                .context("Invalid hunk selection")?
                .lines
                .iter()
                .filter(|l| matches!(l.kind, '+' | '-'))
                .map(|l| l.row)
                .collect(),
            PatchSelection::Lines(rows) => rows.iter().copied().collect(),
        };
        ensure!(
            !selected.is_empty() && selected.is_subset(&changes),
            "Select added or deleted lines first."
        );
        Ok(selected)
    }

    fn selected_content(&self, base: &[u8], selected: &BTreeSet<usize>) -> Result<Vec<u8>> {
        let base_lines: Vec<_> = base.split_inclusive(|&b| b == b'\n').collect();
        let mut cursor = 0;
        let mut output = Vec::with_capacity(base.len());
        for hunk in &self.hunks {
            ensure!(
                hunk.base_start >= cursor && hunk.base_start <= base_lines.len(),
                "Diff no longer matches the index."
            );
            for line in &base_lines[cursor..hunk.base_start] {
                output.extend_from_slice(line);
            }
            cursor = hunk.base_start;
            for line in &hunk.lines {
                let (remove, add) = if self.staged { ('+', '-') } else { ('-', '+') };
                if line.kind == ' ' || line.kind == remove {
                    ensure!(
                        base_lines.get(cursor).copied() == Some(line.bytes.as_slice()),
                        "Diff no longer matches the index."
                    );
                    cursor += 1;
                    if line.kind == ' ' || !selected.contains(&line.row) {
                        output.extend_from_slice(&line.bytes);
                    }
                } else if line.kind == add && selected.contains(&line.row) {
                    output.extend_from_slice(&line.bytes);
                }
            }
        }
        for line in &base_lines[cursor..] {
            output.extend_from_slice(line);
        }
        Ok(output)
    }
}
