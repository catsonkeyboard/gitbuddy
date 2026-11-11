//! Partial staging uses libgit2's structured diff, never the lossy display text.
//! Index edits use canonical blobs; discard keeps untouched working bytes.
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::conversion::WorkingLine;
use super::conversion::{CleanPolicy, FileStamp, Limits, Snapshot, WorkingFile};
use super::libgit::{diff_for_context, head_tree, raw};
use super::{DIFF_LIMIT_LINES, DiffLine, FileChange, Repository, diff_preview};
use anyhow::{Context, Result, ensure};
use git2::{Delta, DiffFindOptions, FileMode, IndexEntry, IndexTime, Oid, Patch};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct FilePatch {
    pub lines: Vec<DiffLine>,
    pub display: super::DiffDisplay,
    pub partial: Option<Arc<PartialPatch>>,
    pub unavailable: Option<String>,
}

impl FilePatch {
    /// Retained allocations, including the lossless partial-staging source.
    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.lines.capacity() * std::mem::size_of::<DiffLine>()
            + self
                .lines
                .iter()
                .map(|l| l.old.capacity() + l.new.capacity() + l.text.capacity())
                .sum::<usize>()
            + self.display.rows.capacity() * std::mem::size_of::<super::SplitRow>()
            + self.display.highlights.capacity()
                * std::mem::size_of::<Vec<std::ops::Range<usize>>>()
            + self
                .display
                .highlights
                .iter()
                .map(|r| r.capacity() * std::mem::size_of::<std::ops::Range<usize>>())
                .sum::<usize>()
            + self.unavailable.as_ref().map_or(0, String::capacity)
            + self.partial.as_ref().map_or(0, |p| {
                std::mem::size_of::<PartialPatch>()
                    + p.diff.capacity()
                    + p.path_bytes.capacity()
                    + p.encoded
                        .as_ref()
                        .map_or(0, |_| std::mem::size_of::<EncodedPatch>())
                    + p.hunks.capacity() * std::mem::size_of::<Hunk>()
                    + p.hunks
                        .iter()
                        .map(|h| {
                            h.lines.capacity() * std::mem::size_of::<PatchLine>()
                                + h.lines.iter().map(|l| l.bytes.capacity()).sum::<usize>()
                        })
                        .sum::<usize>()
            })
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkingVersion {
    stamp: FileStamp,
    hash: [u8; 32],
}
impl WorkingVersion {
    fn read(repo: &git2::Repository, path: &Path) -> Result<Option<WorkingFile>> {
        let absolute = repo.workdir().context("Missing worktree")?.join(path);
        match fs::symlink_metadata(absolute) {
            Ok(_) => Ok(Some(WorkingFile::read(
                repo,
                path,
                Limits::ENCODED.max_bytes,
            )?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn of(file: &WorkingFile) -> Self {
        Self {
            stamp: file.stamp.clone(),
            hash: Sha256::digest(&file.bytes).into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EncodedPatch {
    snapshot: Snapshot,
    working: Option<WorkingVersion>,
}
impl EncodedPatch {
    fn validate(
        &self,
        repo: &git2::Repository,
        path: &Path,
        staged: bool,
    ) -> Result<Option<WorkingFile>> {
        self.snapshot.validate_path(repo, path)?;
        if staged {
            return Ok(None);
        }
        let working = WorkingVersion::read(repo, path)?;
        ensure!(
            working.as_ref().map(WorkingVersion::of) == self.working,
            "Working bytes changed; refresh the diff"
        );
        Ok(working)
    }
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
    encoded: Option<Box<EncodedPatch>>,
}

impl Repository {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(super) fn discard_partial(
        &self,
        _patch: &PartialPatch,
        _selection: &PatchSelection,
    ) -> Result<String> {
        anyhow::bail!("Partial discard is currently supported on macOS and Linux only")
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) fn discard_partial(
        &self,
        patch: &PartialPatch,
        selection: &PatchSelection,
    ) -> Result<String> {
        use std::io::Write;
        ensure!(
            !patch.staged && patch.file.worktree != 'D',
            "Partial discard requires an existing working file"
        );
        let repo = raw(self)?;
        ensure!(
            repo.path() == patch.git_dir,
            "This diff belongs to another repository"
        );
        let mut index = repo.index()?;
        let _index_lock =
            super::conflicts::IndexLock::acquire(index.path().context("Missing index")?)?;
        index.read(true)?;
        ensure!(
            entry_version(index.get_path(&patch.file.path, 0)) == patch.index
                && (1..=3).all(|s| index.get_path(&patch.file.path, s).is_none()),
            "Index changed; refresh the diff"
        );
        ensure!(
            self.worktree_patch_context(&patch.file, false, patch.full_context)?
                .partial
                .as_deref()
                == Some(patch),
            "Diff changed; refresh and select again"
        );
        let mut path = self.root.clone();
        for component in patch.file.path.components() {
            let std::path::Component::Normal(part) = component else {
                anyhow::bail!("Invalid repository path")
            };
            path.push(part);
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "Partial discard does not follow symbolic links"
            );
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.len() <= super::diff_view::FULL_DIFF_BYTES as u64,
            "Partial discard requires a regular file no larger than 16 MiB"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.nlink() == 1,
                "Partial discard is unavailable for hard-linked files"
            );
            #[cfg(target_os = "linux")]
            let attributes = {
                use std::os::unix::ffi::OsStrExt;
                let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())?;
                unsafe { libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) }
            };
            #[cfg(target_os = "linux")]
            ensure!(
                attributes == 0,
                "Partial discard is unavailable when extended attributes cannot be safely preserved"
            );
        }
        let selected = patch.selected_rows(selection)?;
        let all = patch.changed_rows();
        let remaining = all.difference(&selected).copied().collect::<BTreeSet<_>>();
        let base = match &patch.index {
            Some(entry) => repo.find_blob(entry.id)?.content().to_vec(),
            None => Vec::new(),
        };
        let expected = patch.selected_content(&base, &all)?;
        let (actual, content) = if let Some(encoded) = &patch.encoded {
            let working = encoded
                .validate(&repo, &patch.file.path, false)?
                .context("Working file was removed; refresh the diff")?;
            ensure!(
                encoded.snapshot.clean(
                    &repo,
                    &patch.file.path,
                    &working.bytes,
                    &base,
                    CleanPolicy::PatchApply,
                    Limits::ENCODED,
                )? == expected,
                "Working bytes differ from the displayed diff"
            );
            let mut lines = Vec::new();
            let canonical =
                patch.selected_content_with_lines(&base, &remaining, |bytes, working| {
                    if let Some(line) = working {
                        if let Some(WorkingLine::Keep(range)) = lines.last_mut()
                            && range.end == line
                        {
                            range.end += 1;
                        } else {
                            lines.push(WorkingLine::Keep(line..line + 1));
                        }
                    } else {
                        lines.push(WorkingLine::Restore(bytes));
                    }
                })?;
            let raw = encoded.snapshot.splice_working_lines(
                &patch.file.path,
                &working.bytes,
                &expected,
                &canonical,
                &lines,
            )?;
            ensure!(
                encoded.snapshot.clean(
                    &repo,
                    &patch.file.path,
                    &raw,
                    &base,
                    CleanPolicy::PatchApply,
                    Limits::ENCODED,
                )? == canonical,
                "Selected edits cannot be written without changing other working lines"
            );
            (working.bytes, raw)
        } else {
            let actual = fs::read(&path)?;
            ensure!(
                actual == expected,
                "Working file differs from the displayed diff or uses a content filter; refresh or use whole-file discard"
            );
            (actual, patch.selected_content(&base, &remaining)?)
        };
        let mut temporary =
            tempfile::NamedTempFile::new_in(path.parent().context("Missing parent directory")?)?;
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let source = std::fs::File::open(&path)?;
            let copied = unsafe {
                libc::fcopyfile(
                    source.as_raw_fd(),
                    temporary.as_file().as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            };
            ensure!(
                copied == 0,
                "Could not preserve file metadata: {}",
                std::io::Error::last_os_error()
            );
        }
        temporary.write_all(&content)?;
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
        temporary.as_file().sync_all()?;
        ensure!(
            self.worktree_patch_context(&patch.file, false, patch.full_context)?
                .partial
                .as_deref()
                == Some(patch)
                && WorkingFile::read(&repo, &patch.file.path, Limits::ENCODED.max_bytes)?.bytes
                    == actual,
            "Working file changed; discard cancelled"
        );
        temporary.persist(&path).map_err(|e| e.error)?;
        Ok(format!(
            "Discarded {} selected changed lines",
            selected.len()
        ))
    }

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
        let version = entry_version(index.get_path(&file.path, 0));
        let working_metadata = fs::symlink_metadata(self.root.join(&file.path));
        let regular_working = match &working_metadata {
            Ok(meta) => meta.is_file() && !meta.file_type().is_symlink(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        let mut encoded = None;
        if version
            .as_ref()
            .is_none_or(|entry| matches!(entry.mode, 0o100644 | 0o100755))
            && (staged || regular_working)
        {
            let snapshot = super::conversion::Snapshot::capture(
                &repo,
                &BTreeSet::from([file.path.clone()]),
                super::lfs::AttributeLookup::WorktreeThenIndex,
            )?;
            if snapshot.has_encoding(&file.path) {
                let working = if staged {
                    None
                } else {
                    WorkingVersion::read(&repo, &file.path)?
                        .as_ref()
                        .map(WorkingVersion::of)
                };
                encoded = Some(Box::new(EncodedPatch { snapshot, working }));
            }
        }
        if super::lfs::tracked(&repo, &file.path)? {
            return Ok(FilePatch {
                unavailable: Some("LFS files must be staged or unstaged as a whole file.".into()),
                ..FilePatch::read_only_context(
                    &self.diff_context(file, staged, full_context)?,
                    full_context,
                )?
            });
        }
        if full_context || encoded.is_some() {
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
            } else if let Ok(meta) = working_metadata {
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
        if let Some(encoded) = &encoded {
            encoded.validate(&repo, &file.path, staged)?;
        }
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
            encoded,
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
        let mut index = repo.index()?;
        let index_path = index.path().context("Missing index")?.to_path_buf();
        let mut lock = super::conflicts::IndexLock::acquire(&index_path)?;
        let current = self.worktree_patch_context(&patch.file, patch.staged, patch.full_context)?;
        ensure!(
            current.partial.as_deref() == Some(patch),
            "The diff has changed. Refresh and select the changes again."
        );
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
        if let Some(encoded) = &patch.encoded {
            ensure!(
                content.len() <= Limits::ENCODED.max_bytes,
                "Partial index result exceeds the conversion byte limit"
            );
            if let Some(working) = encoded.validate(&repo, &patch.file.path, patch.staged)? {
                // Like git add -p, stage an already-cleaned canonical patch.
                // The whole-file Add-only round-trip rules do not apply.
                ensure!(
                    encoded.snapshot.clean(
                        &repo,
                        &patch.file.path,
                        &working.bytes,
                        &base,
                        CleanPolicy::PatchApply,
                        Limits::ENCODED,
                    )? == patch.selected_content(&base, &patch.changed_rows())?,
                    "Working bytes differ from the displayed diff"
                );
            }
        }
        // A separate index preserves all unrelated entries while the standard
        // lock excludes external Git writers between review and publication.
        let scratch = tempfile::tempdir_in(repo.path())?;
        let next_path = scratch.path().join("index");
        match fs::copy(&index_path, &next_path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !index_path.exists() => {}
            Err(error) => return Err(error.into()),
        }
        let mut next = git2::Index::open(&next_path)?;
        if content.is_empty() && patch.remove_when_empty {
            next.remove_path(&patch.file.path)?;
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
                id: repo.blob(&content)?,
                flags: 0,
                flags_extended: 0,
                path: patch.path_bytes.clone(),
            };
            next.add(&entry)?;
        }
        next.write()?;
        let next_bytes = fs::read(&next_path)?;
        if patch.staged {
            ensure!(
                head_tree(&repo)?.map(|tree| tree.id()) == patch.head,
                "HEAD changed; refresh the diff"
            );
        }
        if let Some(encoded) = &patch.encoded {
            encoded.validate(&repo, &patch.file.path, patch.staged)?;
        }
        lock.write_and_install(&next_bytes, &index_path)?;
        Ok(format!(
            "{} changed lines {}",
            selected.len(),
            if patch.staged { "unstaged" } else { "staged" }
        ))
    }
}

impl PartialPatch {
    pub fn patch_bytes(&self) -> &[u8] {
        &self.diff
    }
    pub fn can_discard(&self) -> bool {
        !self.staged && self.file.worktree != 'D'
    }
    fn changed_rows(&self) -> BTreeSet<usize> {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| matches!(l.kind, '+' | '-'))
            .map(|l| l.row)
            .collect()
    }
    fn selected_rows(&self, selection: &PatchSelection) -> Result<BTreeSet<usize>> {
        let changes = self.changed_rows();
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
        self.selected_content_with_lines(base, selected, |_, _| ())
    }

    fn selected_content_with_lines<'a>(
        &'a self,
        base: &'a [u8],
        selected: &BTreeSet<usize>,
        mut record_source: impl FnMut(&'a [u8], Option<usize>),
    ) -> Result<Vec<u8>> {
        let mut base_lines = base.split_inclusive(|&b| b == b'\n');
        let mut cursor = 0;
        let mut working_line = 0;
        let mut output = Vec::with_capacity(base.len());
        let mut append = |bytes: &'a [u8], working: Option<usize>| {
            output.extend_from_slice(bytes);
            record_source(bytes, working);
        };
        for hunk in &self.hunks {
            ensure!(
                hunk.base_start >= cursor,
                "Diff no longer matches the index."
            );
            for _ in cursor..hunk.base_start {
                let line = base_lines
                    .next()
                    .context("Diff no longer matches the index.")?;
                append(line, Some(working_line));
                working_line += 1;
            }
            cursor = hunk.base_start;
            for line in &hunk.lines {
                let (remove, add) = if self.staged { ('+', '-') } else { ('-', '+') };
                if line.kind == ' ' || line.kind == remove {
                    ensure!(
                        base_lines.next() == Some(line.bytes.as_slice()),
                        "Diff no longer matches the index."
                    );
                    cursor += 1;
                    if line.kind == ' ' || !selected.contains(&line.row) {
                        append(&line.bytes, (line.kind == ' ').then_some(working_line));
                    }
                    if line.kind == ' ' {
                        working_line += 1;
                    }
                } else if line.kind == add {
                    if selected.contains(&line.row) {
                        append(&line.bytes, Some(working_line));
                    }
                    working_line += 1;
                }
            }
        }
        for line in base_lines {
            append(line, Some(working_line));
            working_line += 1;
        }
        Ok(output)
    }
}
