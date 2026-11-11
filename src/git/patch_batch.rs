//! Multi-file patch plans and a durable rollback journal.
#[path = "patch_conversion.rs"]
mod conversion;
use super::{Repository, libgit::raw};
use anyhow::{Context, Result, ensure};
use git2::{Delta, Diff, Oid, RepositoryState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

const JOURNAL: &str = "gitbuddy-patch-recovery";
const MAX_FILE: usize = 16 * 1024 * 1024;
pub(super) fn binary_limit(diff: &Diff<'_>) -> Result<()> {
    use std::io::Read;
    let mut error = None;
    let mut binary_count = 0usize;
    let mut total_inflated = 0usize;
    let mut total_result = 0usize;
    let mut binary = |_: git2::DiffDelta<'_>, binary: git2::DiffBinary<'_>| {
        binary_count += 1;
        let check = (|| -> Result<()> {
            ensure!(
                binary.contains_data(),
                "Binary patch does not contain file data"
            );
            for (side, file) in [binary.old_file(), binary.new_file()]
                .into_iter()
                .enumerate()
            {
                if matches!(file.kind(), git2::DiffBinaryKind::None) {
                    continue;
                }
                ensure!(
                    file.inflated_len() <= MAX_FILE,
                    "Binary patch exceeds the size limit"
                );
                total_inflated = total_inflated.saturating_add(file.inflated_len());
                ensure!(
                    total_inflated <= 64 * 1024 * 1024,
                    "Binary patch exceeds the total size limit"
                );
                let mut decoded = Vec::new();
                flate2::read::ZlibDecoder::new(file.data())
                    .take(MAX_FILE as u64 + 1)
                    .read_to_end(&mut decoded)?;
                ensure!(
                    decoded.len() == file.inflated_len() && decoded.len() <= MAX_FILE,
                    "Invalid or oversized binary patch data"
                );
                let result_size = if matches!(file.kind(), git2::DiffBinaryKind::Delta) {
                    let mut cursor = 0;
                    let mut read_varint = || -> Result<usize> {
                        let mut value = 0usize;
                        for shift in (0..usize::BITS).step_by(7) {
                            let byte =
                                *decoded.get(cursor).context("Invalid binary delta header")?;
                            cursor += 1;
                            ensure!(
                                (byte & 127) as usize <= (usize::MAX >> shift),
                                "Oversized binary delta"
                            );
                            value = value
                                .checked_add(
                                    ((byte & 127) as usize)
                                        .checked_shl(shift)
                                        .context("Oversized binary delta")?,
                                )
                                .context("Oversized binary delta")?;
                            if byte & 128 == 0 {
                                return Ok(value);
                            }
                        }
                        anyhow::bail!("Invalid binary delta header")
                    };
                    let source = read_varint()?;
                    let result = read_varint()?;
                    ensure!(
                        source <= MAX_FILE && result <= MAX_FILE,
                        "Binary delta result exceeds the size limit"
                    );
                    result
                } else {
                    file.inflated_len()
                };
                if side == 1 {
                    total_result = total_result.saturating_add(result_size);
                    ensure!(
                        total_result <= 64 * 1024 * 1024,
                        "Binary patch results exceed the total size limit"
                    );
                }
            }
            Ok(())
        })();
        if let Err(e) = check {
            error = Some(e);
            false
        } else {
            true
        }
    };
    let result = diff.foreach(&mut |_, _| true, Some(&mut binary), None, None);
    if let Some(error) = error {
        return Err(error);
    }
    result?;
    let expected = diff
        .deltas()
        .filter(|d| d.flags().contains(git2::DiffFlags::BINARY))
        .count();
    ensure!(
        binary_count >= expected,
        "Could not validate binary patch payload"
    );
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Version {
    /// Canonical Git blob used to validate/apply the incoming diff.
    oid: String,
    /// Pinned raw payload; old journals use the canonical blob verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree_oid: Option<String>,
    mode: u32,
    /// Actual worktree bytes, including CRLF and expanded ident text.
    hash: [u8; 32],
}
impl Version {
    fn payload_oid(&self) -> Result<Oid> {
        Ok(Oid::from_str(
            self.worktree_oid.as_deref().unwrap_or(&self.oid),
        )?)
    }
    fn pin_worktree(&mut self, repo: &git2::Repository, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_FILE,
            "Converted working file exceeds 16 MiB"
        );
        self.hash = Sha256::digest(bytes).into();
        let canonical = repo.find_blob(Oid::from_str(&self.oid)?)?;
        if canonical.content() != bytes {
            self.worktree_oid = Some(repo.blob(bytes)?.to_string());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Change {
    path: String,
    /// A basename-only ASCII case rename is one logical change, even when both
    /// spellings address the same directory entry on the current filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rename_from: Option<String>,
    before: Option<Version>,
    after: Option<Version>,
    metadata_source: Option<String>,
    temp: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    version: u32,
    id: String,
    head_ref: String,
    head_oid: Option<String>,
    changes: Vec<Change>,
}
#[derive(Clone, Debug)]
pub(super) struct BatchPlan {
    git_dir: PathBuf,
    fingerprint: u64,
    changes: Vec<Change>,
    patch: Vec<u8>,
    conversion: conversion::Snapshot,
}
impl BatchPlan {
    pub(super) fn summary(&self) -> String {
        let files = self
            .changes
            .iter()
            .map(|c| {
                if let Some(source) = &c.rename_from {
                    return format!("Rename {source:?} → {:?}", c.path);
                }
                format!(
                    "{} {:?}",
                    if c.before.is_none() {
                        "Add"
                    } else if c.after.is_none() {
                        "Delete"
                    } else {
                        "Modify"
                    },
                    c.path
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let raw = String::from_utf8_lossy(&self.patch);
        let preview = raw
            .chars()
            .take(2000)
            .map(|c| {
                if c.is_control() && !matches!(c, '\n' | '\t') {
                    '�'
                } else {
                    c
                }
            })
            .collect::<String>();
        format!(
            "Apply {} working-file changes? The index remains unchanged. A recovery journal protects interrupted writes.\n\n{}\n\nPatch preview:\n{}{}",
            self.changes.len(),
            files,
            preview,
            if raw.chars().count() > 2000 {
                "\n… preview truncated"
            } else {
                ""
            }
        )
    }
}
pub(super) fn recovery_pending(repo: &git2::Repository) -> bool {
    match fs::symlink_metadata(repo.path().join(JOURNAL)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}
fn safe_path(root: &Path, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path);
    ensure!(
        !path.is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "Invalid patch path"
    );
    ensure!(
        !relative
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")),
        "Patch path cannot enter .git"
    );
    let mut result = root.to_path_buf();
    for part in relative.components() {
        result.push(part);
        match fs::symlink_metadata(&result) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink() || result == root.join(relative),
                "Patch path contains a symbolic link"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(result)
}
fn version(repo: &git2::Repository, entry: Option<(Oid, u32)>) -> Result<Option<Version>> {
    entry
        .map(|(id, mode)| {
            ensure!(
                matches!(mode, 0o100644 | 0o100755 | 0o120000),
                "Only ordinary files are supported"
            );
            let (size, kind) = repo.odb()?.read_header(id)?;
            ensure!(
                kind == git2::ObjectType::Blob && size <= MAX_FILE,
                "Patch file exceeds 16 MiB"
            );
            Ok(Version {
                oid: id.to_string(),
                worktree_oid: None,
                mode,
                hash: Sha256::digest(repo.find_blob(id)?.content()).into(),
            })
        })
        .transpose()
}
fn tree_entry(tree: &git2::Tree<'_>, path: &Path) -> Result<Option<(Oid, u32)>> {
    match tree.get_path(path) {
        Ok(e) => Ok(Some((e.id(), e.filemode() as u32))),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn read_current(root: &Path, change: &Change) -> Result<Option<([u8; 32], u32)>> {
    read_path_current(root, &change.path)
}
fn read_path_current(root: &Path, name: &str) -> Result<Option<([u8; 32], u32)>> {
    let path = safe_path(root, name)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        (meta.is_file() || meta.file_type().is_symlink()) && meta.len() <= MAX_FILE as u64,
        "Patch target is not an ordinary file or is too large"
    );
    #[cfg(unix)]
    let mode = if meta.file_type().is_symlink() {
        0o120000
    } else {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        }
    };
    #[cfg(not(unix))]
    let mode = 0o100644;
    Ok(Some((
        Sha256::digest(read_content(&path, &meta)?).into(),
        mode,
    )))
}
fn read_content(path: &Path, meta: &fs::Metadata) -> Result<Vec<u8>> {
    if meta.file_type().is_symlink() {
        Ok(super::libgit::path_bytes(&fs::read_link(path)?))
    } else {
        let file = open_regular(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let actual = file.metadata()?;
            ensure!(
                actual.dev() == meta.dev() && actual.ino() == meta.ino(),
                "File changed while opening patch content"
            );
        }
        read_open_file(file)
    }
}
fn open_regular(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_FILE as u64,
        "Patch content must be a regular file up to 16 MiB"
    );
    Ok(file)
}
fn read_open_file(file: fs::File) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut data = Vec::new();
    file.take(MAX_FILE as u64 + 1).read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_FILE, "Patch content grew beyond 16 MiB");
    Ok(data)
}
fn create_link(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(super::path_from_bytes(bytes), path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes);
        anyhow::bail!("Symlink patch application requires Unix")
    }
}
fn hash_of(v: &Option<Version>) -> Option<([u8; 32], u32)> {
    v.as_ref().map(|v| (v.hash, v.mode))
}
fn case_basename_rename(old: &str, new: &str) -> bool {
    let old_path = Path::new(old);
    let new_path = Path::new(new);
    old != new
        && old_path.parent() == new_path.parent()
        && old_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.is_ascii())
        && new_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.is_ascii())
        && old.eq_ignore_ascii_case(new)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenameState {
    Before,
    Removed,
    After,
}
/// Check directory entries, rather than path lookups, so an alias on a
/// case-insensitive volume cannot hide a distinct or unexpectedly cased file.
fn rename_state(root: &Path, change: &Change) -> Result<RenameState> {
    let source = change
        .rename_from
        .as_deref()
        .context("Missing rename source")?;
    ensure!(
        case_basename_rename(source, &change.path),
        "Unsupported case rename"
    );
    safe_path(root, source)?;
    safe_path(root, &change.path)?;
    let source_path = Path::new(source);
    let mut parent = root.to_path_buf();
    for component in source_path
        .parent()
        .context("Missing rename parent")?
        .components()
    {
        ensure!(
            fs::read_dir(&parent)?
                .any(|entry| entry.is_ok_and(|e| e.file_name() == component.as_os_str())),
            "Rename directory spelling changed"
        );
        parent.push(component);
    }
    let old_name = source_path.file_name().context("Missing source name")?;
    let new_name = Path::new(&change.path)
        .file_name()
        .context("Missing destination name")?;
    let mut old_exists = false;
    let mut new_exists = false;
    for entry in fs::read_dir(&parent)? {
        let name = entry?.file_name();
        if name == old_name {
            old_exists = true;
        } else if name == new_name {
            new_exists = true;
        } else {
            ensure!(
                !name
                    .to_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(old_name.to_str().unwrap_or_default())),
                "Unexpected case-rename spelling: {}",
                name.to_string_lossy()
            );
        }
    }
    ensure!(
        !(old_exists && new_exists),
        "Case-rename destination already exists"
    );
    if old_exists {
        ensure!(
            read_path_current(root, source)? == hash_of(&change.before),
            "Case-rename source changed: {source}"
        );
        Ok(RenameState::Before)
    } else if new_exists {
        ensure!(
            read_current(root, change)? == hash_of(&change.after),
            "Case-rename destination changed: {}",
            change.path
        );
        Ok(RenameState::After)
    } else {
        ensure!(
            read_path_current(root, source)?.is_none() && read_current(root, change)?.is_none(),
            "Case-rename paths alias an unexpected directory entry"
        );
        Ok(RenameState::Removed)
    }
}
fn unchanged(root: &Path, change: &Change) -> Result<bool> {
    if change.rename_from.is_some() {
        Ok(rename_state(root, change)? == RenameState::Before)
    } else {
        Ok(read_current(root, change)? == hash_of(&change.before))
    }
}
impl Repository {
    pub(super) fn prepare_patch_batch(&self, bytes: Vec<u8>) -> Result<BatchPlan> {
        let repo = raw(self)?;
        ensure!(
            !recovery_pending(&repo)
                && repo.state() == RepositoryState::Clean
                && !super::integration::squash_active(&repo)
                && !super::rebase::active(&repo),
            "Finish the current operation or recover the interrupted patch first"
        );
        let diff = Diff::from_buffer(&bytes)?;
        binary_limit(&diff)?;
        ensure!(
            diff.deltas().len() != 0 && diff.deltas().len() <= 128,
            "Patch must contain 1–128 files"
        );
        let tree = match super::libgit::head_tree(&repo)? {
            Some(tree) => tree,
            None => repo.find_tree(repo.treebuilder(None)?.write()?)?,
        };
        let index = repo.index()?;
        let mut paths = BTreeSet::new();
        let mut renames = std::collections::HashMap::new();
        for d in diff.deltas() {
            ensure!(
                matches!(
                    d.status(),
                    Delta::Added | Delta::Deleted | Delta::Modified | Delta::Renamed
                ),
                "Unsupported patch operation"
            );
            for path in [d.old_file().path(), d.new_file().path()]
                .into_iter()
                .flatten()
            {
                let s = path
                    .to_str()
                    .context("Patch paths must be UTF-8")?
                    .to_owned();
                safe_path(&self.root, &s)?;
                paths.insert(s);
            }
            if d.status() == Delta::Renamed {
                let old = d
                    .old_file()
                    .path()
                    .context("Missing rename source")?
                    .to_str()
                    .context("Invalid source")?;
                let new = d
                    .new_file()
                    .path()
                    .context("Missing rename destination")?
                    .to_str()
                    .context("Invalid destination")?;
                // A file move cannot rename an aliased directory. Refuse the
                // ambiguity even if its basename is also changing.
                let mut directories = std::collections::BTreeMap::new();
                for path in [old, new] {
                    for parent in Path::new(path)
                        .parent()
                        .into_iter()
                        .flat_map(Path::ancestors)
                    {
                        let spelling = parent.to_str().context("Invalid rename directory")?;
                        if let Some(previous) =
                            directories.insert(spelling.to_lowercase(), spelling)
                        {
                            ensure!(
                                previous == spelling,
                                "Directory-component case renames are not supported"
                            );
                        }
                    }
                }
                renames.insert(new.to_owned(), old.to_owned());
            }
        }
        ensure!(
            !paths.is_empty() && paths.len() <= 256,
            "Too many patch paths"
        );
        let case_renames = renames
            .iter()
            .filter(|(new, old)| case_basename_rename(old, new))
            .map(|(new, old)| (new.clone(), old.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        let names = paths.iter().collect::<Vec<_>>();
        for (i, a) in names.iter().enumerate() {
            for b in names.iter().skip(i + 1) {
                ensure!(
                    !Path::new(a).starts_with(b)
                        && !Path::new(b).starts_with(a)
                        && (a.to_lowercase() != b.to_lowercase()
                            || case_renames.get(a.as_str()) == Some(*b)
                            || case_renames.get(b.as_str()) == Some(*a)),
                    "Overlapping paths or unsupported case renames (only ASCII basenames are supported)"
                );
            }
        }
        let mut base_total = 0usize;
        for name in &paths {
            if let Some((id, mode)) = tree_entry(&tree, Path::new(name))? {
                ensure!(
                    matches!(mode, 0o100644 | 0o100755 | 0o120000),
                    "Only ordinary files are supported"
                );
                let (size, kind) = repo.odb()?.read_header(id)?;
                ensure!(
                    kind == git2::ObjectType::Blob && size <= MAX_FILE,
                    "Patch file exceeds 16 MiB"
                );
                base_total = base_total.saturating_add(size);
                ensure!(
                    base_total <= 64 * 1024 * 1024,
                    "Patch source files exceed the total limit"
                );
            }
        }
        let result = repo
            .apply_to_tree(&tree, &diff, None)
            .context("Patch does not apply to HEAD (binary patches must contain data)")?;
        for attributes in paths.iter().filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| name == ".gitattributes")
        }) {
            let directory = Path::new(attributes)
                .parent()
                .context("Missing attributes parent")?;
            ensure!(
                !paths
                    .iter()
                    .any(|path| path != attributes && Path::new(path).starts_with(directory)),
                "Apply controlling .gitattributes changes separately from affected files"
            );
        }
        let mut converted_paths = BTreeSet::new();
        for path in &paths {
            let old = tree_entry(&tree, Path::new(path))?;
            let new = result.get_path(Path::new(path), 0);
            if old.is_some_and(|(_, mode)| mode != 0o120000)
                || new.is_some_and(|entry| entry.mode != 0o120000)
            {
                converted_paths.insert(path.clone());
            }
        }
        let conversion = conversion::Snapshot::capture(&repo, &converted_paths)?;
        let mut changes = Vec::new();
        let mut total = 0usize;
        let mut working_total = 0usize;
        for path in paths {
            let p = Path::new(&path);
            let old = tree_entry(&tree, p)?;
            let staged = index
                .get_path(p, 0)
                .filter(|e| e.path == path.as_bytes())
                .map(|e| (e.id, e.mode));
            ensure!(
                staged == old
                    && (1..=3).all(|stage| index
                        .get_path(p, stage)
                        .is_none_or(|e| e.path != path.as_bytes())),
                "Affected file has staged changes or conflicts: {path}"
            );
            if case_renames.values().any(|source| source == &path) {
                continue;
            }
            let rename_from = case_renames.get(&path).cloned();
            let mut before = version(
                &repo,
                match &rename_from {
                    Some(source) => tree_entry(&tree, Path::new(source))?,
                    None => old,
                },
            )?;
            let mut after = version(&repo, result.get_path(p, 0).map(|e| (e.id, e.mode)))?;
            if let Some(before) = &mut before {
                let source_name = rename_from.as_deref().unwrap_or(&path);
                let source = safe_path(&self.root, source_name)?;
                let bytes = read_content(&source, &fs::symlink_metadata(&source)?)?;
                let canonical = repo.find_blob(Oid::from_str(&before.oid)?)?;
                let clean = if before.mode == 0o120000 {
                    bytes.clone()
                } else {
                    conversion.clean(&repo, source_name, &bytes, canonical.content())?
                };
                ensure!(
                    clean == canonical.content(),
                    "Affected file has local changes or unsupported conversion: {source_name}"
                );
                before.pin_worktree(&repo, &bytes)?;
            }
            if let Some(after) = &mut after {
                let id = Oid::from_str(&after.oid)?;
                let canonical = repo.find_blob(id)?;
                let bytes = if after.mode == 0o120000 {
                    canonical.content().to_vec()
                } else {
                    conversion.smudge(&repo, &path, canonical.content(), id)?
                };
                if after.mode != 0o120000 {
                    let indexed = old.map(|(id, _)| repo.find_blob(id)).transpose()?;
                    ensure!(
                        conversion.clean(
                            &repo,
                            &path,
                            &bytes,
                            indexed.as_ref().map_or(&[], |blob| blob.content())
                        )? == canonical.content(),
                        "Patched blob cannot round-trip through content conversions: {path}"
                    );
                }
                after.pin_worktree(&repo, &bytes)?;
            }
            if before == after && rename_from.is_none() {
                continue;
            }
            let mut change = Change {
                metadata_source: renames
                    .get(&path)
                    .cloned()
                    .or_else(|| before.as_ref().map(|_| path.clone())),
                path,
                rename_from,
                before,
                after,
                temp: None,
            };
            if let Some(source) = &change.metadata_source
                && tree_entry(&tree, Path::new(source))?.is_some_and(|(_, mode)| mode == 0o120000)
            {
                change.metadata_source = None;
            }
            ensure!(
                unchanged(&self.root, &change)?,
                "Affected file has local changes or an untracked collision: {}",
                change.path
            );
            for v in [&change.before, &change.after].into_iter().flatten() {
                total = total.saturating_add(repo.odb()?.read_header(Oid::from_str(&v.oid)?)?.0);
                working_total =
                    working_total.saturating_add(repo.odb()?.read_header(v.payload_oid()?)?.0);
            }
            ensure!(
                total <= 64 * 1024 * 1024,
                "Patch exceeds the 64 MiB total limit"
            );
            ensure!(
                working_total <= 64 * 1024 * 1024,
                "Converted working files exceed the 64 MiB total limit"
            );
            changes.push(change);
        }
        ensure!(!changes.is_empty(), "Patch has no effective changes");
        conversion.validate(&repo)?;
        Ok(BatchPlan {
            git_dir: repo.path().into(),
            fingerprint: self.fingerprint()?,
            changes,
            patch: bytes,
            conversion,
        })
    }
}

pub(super) fn copy_metadata(source: &Path, destination: &fs::File) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let src = open_regular(source)?;
        ensure!(
            unsafe {
                libc::fcopyfile(
                    src.as_raw_fd(),
                    destination.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            } == 0,
            "Cannot preserve metadata: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) } == 0,
            "Cannot preserve extended attributes on this platform"
        );
        destination.set_permissions(fs::metadata(source)?.permissions())?;
    }
    Ok(())
}
fn make_parent(root: &Path, path: &str) -> Result<()> {
    let relative = Path::new(path).parent().context("Missing parent")?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) => ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "Unsafe patch directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                sync_dir(current.parent().context("Missing directory parent")?)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn backup_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i}.backup"))
}
fn sync_dir(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}
fn remove_journal(dir: &Path, journal: &Journal) -> Result<()> {
    let mut allowed = BTreeSet::from(["manifest.json".to_owned()]);
    for (i, change) in journal.changes.iter().enumerate() {
        if change.before.is_some() {
            allowed.insert(format!("{i}.backup"));
        }
    }
    for entry in fs::read_dir(dir)? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Unexpected recovery file"))?;
        ensure!(
            allowed.contains(&name),
            "Unexpected file in recovery directory: {name}"
        );
    }
    for name in allowed {
        fs::remove_file(dir.join(name))?;
    }
    fs::remove_dir(dir)?;
    if let Some(parent) = dir.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}
fn finish_journal(repo: &git2::Repository, journal: &Journal) -> Result<()> {
    let active = repo.path().join(JOURNAL);
    let complete = repo.path().join(format!("{}-completed", journal.id));
    ensure!(
        !complete.exists(),
        "Completed patch journal path already exists"
    );
    fs::rename(&active, &complete)?;
    sync_dir(repo.path())?;
    remove_journal(&complete, journal)
}
fn validate_journal(j: &Journal) -> Result<()> {
    ensure!(
        matches!(j.version, 1..=3) && !j.changes.is_empty() && j.changes.len() <= 256,
        "Invalid patch recovery journal"
    );
    ensure!(
        j.id.starts_with("gitbuddy-patch-pending-")
            && j.id.len() <= 80
            && j.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid recovery identifier"
    );
    let mut seen = BTreeSet::new();
    for (i, c) in j.changes.iter().enumerate() {
        ensure!(seen.insert(&c.path), "Duplicate recovery path");
        if let Some(source) = &c.rename_from {
            ensure!(
                j.version >= 2
                    && case_basename_rename(source, &c.path)
                    && c.before.is_some()
                    && c.after.is_some()
                    && seen.insert(source),
                "Invalid case-rename recovery change"
            );
        }
        ensure!(
            c.before.is_some() || c.after.is_some(),
            "Empty recovery change"
        );
        ensure!(
            j.version == 3
                || [&c.before, &c.after]
                    .into_iter()
                    .flatten()
                    .all(|v| v.worktree_oid.is_none()),
            "Converted payload requires a v3 journal"
        );
        if c.after.is_some() {
            let expected = Path::new(&c.path)
                .parent()
                .unwrap_or(Path::new(""))
                .join(format!(".{}-{i}.tmp", j.id));
            ensure!(
                c.temp.as_deref() == expected.to_str(),
                "Invalid recovery temporary path"
            );
        } else {
            ensure!(c.temp.is_none(), "Unexpected recovery temporary path");
        }
    }
    Ok(())
}
fn restore(repo: &git2::Repository, root: &Path) -> Result<()> {
    let dir = repo.path().join(JOURNAL);
    let metadata = fs::symlink_metadata(&dir)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Invalid patch recovery directory"
    );
    let manifest_path = dir.join("manifest.json");
    let meta = fs::symlink_metadata(&manifest_path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 1024 * 1024,
        "Invalid patch recovery manifest"
    );
    let journal: Journal = serde_json::from_slice(&fs::read(manifest_path)?)?;
    validate_journal(&journal)?;
    let head = super::recovery::head_state(repo)?;
    ensure!(
        repo.state() == RepositoryState::Clean
            && head.reference == journal.head_ref
            && head.id == journal.head_oid,
        "HEAD or repository operation changed since the interruption; inspect the recovery data before restoring"
    );
    for (i, c) in journal.changes.iter().enumerate() {
        if c.rename_from.is_some() {
            rename_state(root, c)?;
        } else {
            let actual = read_current(root, c)?;
            ensure!(
                actual == hash_of(&c.before) || actual == hash_of(&c.after),
                "File changed after the patch was interrupted; preserve it and inspect recovery data: {}",
                c.path
            );
        }
        if let Some(before) = &c.before {
            let backup = backup_path(&dir, i);
            let m = fs::symlink_metadata(&backup)?;
            ensure!(
                m.is_file()
                    && !m.file_type().is_symlink()
                    && m.len() <= MAX_FILE as u64
                    && <[u8; 32]>::from(Sha256::digest(read_content(&backup, &m)?)) == before.hash,
                "Patch backup is missing or corrupt: {}",
                c.path
            );
        }
        if let Some(temp) = &c.temp {
            safe_path(root, temp)?;
        }
    }
    for (i, c) in journal.changes.iter().enumerate().rev() {
        if let Some(source) = &c.rename_from {
            let state = rename_state(root, c)?;
            if state == RenameState::Before {
                continue;
            }
            let source_path = safe_path(root, source)?;
            let parent = source_path.parent().context("Missing rename parent")?;
            let backup = backup_path(&dir, i);
            let mut restored = tempfile::NamedTempFile::new_in(parent)?;
            if c.before.as_ref().is_some_and(|v| v.mode == 0o120000) {
                let restored = restored.into_temp_path();
                fs::remove_file(&restored)?;
                create_link(&restored, &read_open_file(open_regular(&backup)?)?)?;
                ensure!(
                    rename_state(root, c)? == state,
                    "Case-rename paths changed during recovery"
                );
                if state == RenameState::After {
                    fs::remove_file(safe_path(root, &c.path)?)?;
                    sync_dir(parent)?;
                }
                ensure!(
                    rename_state(root, c)? == RenameState::Removed,
                    "Case-rename destination changed during recovery"
                );
                create_link(
                    &source_path,
                    &fs::read_link(&restored).map(|p| super::libgit::path_bytes(&p))?,
                )?;
            } else {
                copy_metadata(&backup, restored.as_file())?;
                std::io::copy(&mut open_regular(&backup)?, &mut restored)?;
                restored.as_file().sync_all()?;
                ensure!(
                    rename_state(root, c)? == state,
                    "Case-rename paths changed during recovery"
                );
                if state == RenameState::After {
                    fs::remove_file(safe_path(root, &c.path)?)?;
                    sync_dir(parent)?;
                }
                ensure!(
                    rename_state(root, c)? == RenameState::Removed,
                    "Case-rename destination changed during recovery"
                );
                fs::hard_link(restored.path(), &source_path)?;
            }
            sync_dir(parent)?;
            continue;
        }
        let actual = read_current(root, c)?;
        if actual == hash_of(&c.before) {
            continue;
        }
        let path = safe_path(root, &c.path)?;
        match &c.before {
            None => fs::remove_file(&path)?,
            Some(_) => {
                make_parent(root, &c.path)?;
                let backup = backup_path(&dir, i);
                if c.before.as_ref().is_some_and(|v| v.mode == 0o120000) {
                    let temporary =
                        tempfile::NamedTempFile::new_in(path.parent().context("Missing parent")?)?
                            .into_temp_path();
                    fs::remove_file(&temporary)?;
                    create_link(&temporary, &read_open_file(open_regular(&backup)?)?)?;
                    ensure!(
                        read_current(root, c)? == actual,
                        "File changed during recovery: {}",
                        c.path
                    );
                    temporary.persist(&path).map_err(|e| e.error)?;
                    sync_dir(path.parent().context("Missing parent")?)?;
                    continue;
                }
                let mut temp =
                    tempfile::NamedTempFile::new_in(path.parent().context("Missing parent")?)?;
                copy_metadata(&backup, temp.as_file())?;
                std::io::copy(&mut open_regular(&backup)?, &mut temp)?;
                temp.as_file().sync_all()?;
                ensure!(
                    read_current(root, c)? == actual,
                    "File changed during recovery: {}",
                    c.path
                );
                temp.persist(&path).map_err(|e| e.error)?;
            }
        }
        sync_dir(path.parent().context("Missing parent")?)?;
    }
    for c in &journal.changes {
        if let Some(temp) = &c.temp {
            let path = safe_path(root, temp)?;
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => Some(meta),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(meta) = meta {
                ensure!(
                    (meta.is_file() || meta.file_type().is_symlink())
                        && meta.len() <= MAX_FILE as u64,
                    "Invalid patch temporary file: {}",
                    path.display()
                );
                let bytes = read_content(&path, &meta)?;
                let expected = c.after.as_ref().context("Missing temporary file version")?;
                let blob = repo
                    .find_blob(expected.payload_oid()?)
                    .context("Temporary content object is unavailable")?;
                ensure!(
                    blob.content().starts_with(&bytes),
                    "Patch temporary file differs from expected content and was retained: {}",
                    path.display()
                );
                fs::remove_file(path)?;
            }
        }
    }
    finish_journal(repo, &journal)
        .context("Files restored, but completed journal cleanup failed")?;
    Ok(())
}

impl Repository {
    pub fn has_patch_recovery(&self) -> Result<bool> {
        Ok(recovery_pending(&raw(self)?))
    }
    pub(super) fn recover_patch(&self) -> Result<String> {
        let repo = raw(self)?;
        let index = repo.index()?;
        let _lock = super::conflicts::IndexLock::acquire(index.path().context("Missing index")?)?;
        restore(&repo, &self.root)?;
        Ok("Recovered files from the interrupted patch operation".into())
    }
    pub(super) fn apply_patch_batch(&self, plan: &BatchPlan) -> Result<String> {
        self.apply_patch_batch_inner(plan, &mut |_| Ok(()))
    }
    fn apply_patch_batch_inner(
        &self,
        plan: &BatchPlan,
        after_write: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<String> {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        anyhow::bail!("Multi-file patch application supports macOS and Linux only");
        let repo = raw(self)?;
        let index = repo.index()?;
        let _lock = super::conflicts::IndexLock::acquire(index.path().context("Missing index")?)?;
        ensure!(
            repo.path() == plan.git_dir
                && self.fingerprint()? == plan.fingerprint
                && !recovery_pending(&repo),
            "Repository changed; review the patch again"
        );
        let head = super::recovery::head_state(&repo)?;
        plan.conversion.validate(&repo)?;
        for c in &plan.changes {
            ensure!(
                unchanged(&self.root, c)?,
                "File changed; review the patch again: {}",
                c.path
            );
        }
        let tempdir = tempfile::Builder::new()
            .prefix("gitbuddy-patch-pending-")
            .tempdir_in(repo.path())?;
        let label = tempdir
            .path()
            .file_name()
            .context("Missing journal name")?
            .to_string_lossy()
            .into_owned();
        let mut journal = Journal {
            version: if plan.changes.iter().any(|c| {
                [&c.before, &c.after]
                    .into_iter()
                    .flatten()
                    .any(|v| v.worktree_oid.is_some())
            }) {
                3
            } else if plan.changes.iter().any(|c| c.rename_from.is_some()) {
                2
            } else {
                1
            },
            id: label.clone(),
            head_ref: head.reference.clone(),
            head_oid: head.id.clone(),
            changes: plan.changes.clone(),
        };
        for (i, c) in journal.changes.iter_mut().enumerate() {
            if c.before.is_some() {
                let source = safe_path(&self.root, c.rename_from.as_deref().unwrap_or(&c.path))?;
                let mut backup = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(backup_path(tempdir.path(), i))?;
                if c.before.as_ref().is_some_and(|v| v.mode == 0o120000) {
                    backup.write_all(&read_content(&source, &fs::symlink_metadata(&source)?)?)?;
                } else {
                    copy_metadata(&source, &backup)?;
                    std::io::copy(&mut open_regular(&source)?, &mut backup)?;
                }
                backup.sync_all()?;
                ensure!(
                    <[u8; 32]>::from(Sha256::digest(fs::read(backup_path(tempdir.path(), i))?))
                        == c.before.as_ref().context("Missing original version")?.hash,
                    "File changed while creating backup: {}",
                    c.path
                );
            }
            if c.after.is_some() {
                let p = Path::new(&c.path);
                c.temp = Some(
                    p.parent()
                        .unwrap_or(Path::new(""))
                        .join(format!(".{label}-{i}.tmp"))
                        .to_str()
                        .context("Temporary path is not UTF-8")?
                        .into(),
                );
            }
        }
        let mut manifest = fs::File::create(tempdir.path().join("manifest.json"))?;
        manifest.write_all(&serde_json::to_vec(&journal)?)?;
        manifest.sync_all()?;
        sync_dir(tempdir.path())?;
        let persisted = tempdir.keep();
        if let Err(error) = fs::rename(&persisted, repo.path().join(JOURNAL)) {
            let _ = fs::remove_dir_all(&persisted);
            return Err(error.into());
        }
        sync_dir(repo.path())?;
        let work = (|| -> Result<()> {
            plan.conversion.validate(&repo)?;
            for c in &journal.changes {
                let Some(after) = &c.after else {
                    continue;
                };
                make_parent(&self.root, &c.path)?;
                let temp = safe_path(&self.root, c.temp.as_deref().context("Missing temp path")?)?;
                if after.mode == 0o120000 {
                    create_link(&temp, repo.find_blob(after.payload_oid()?)?.content())?;
                    sync_dir(temp.parent().context("Missing parent")?)?;
                    continue;
                }
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(temp)?;
                if let Some(source) = &c.metadata_source {
                    copy_metadata(&safe_path(&self.root, source)?, &file)?;
                }
                #[cfg(unix)]
                if c.metadata_source.is_none() {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if after.mode == 0o100755 { 0o755 } else { 0o644 };
                    file.set_permissions(fs::Permissions::from_mode(mode))?;
                }
                #[cfg(unix)]
                if c.metadata_source.is_some() {
                    use std::os::unix::fs::PermissionsExt;
                    let old = file.metadata()?.permissions().mode();
                    if (old & 0o111 != 0) != (after.mode == 0o100755) {
                        file.set_permissions(fs::Permissions::from_mode(
                            if after.mode == 0o100755 {
                                old | 0o111
                            } else {
                                old & !0o111
                            },
                        ))?;
                    }
                }
                file.write_all(repo.find_blob(after.payload_oid()?)?.content())?;
                file.sync_all()?;
            }
            ensure!(
                super::recovery::head_state(&repo)? == head
                    && repo.state() == RepositoryState::Clean,
                "Repository operation changed during patch preparation"
            );
            plan.conversion.validate(&repo)?;
            for c in &journal.changes {
                ensure!(
                    unchanged(&self.root, c)?,
                    "File changed during patch preparation: {}",
                    c.path
                );
            }
            for (i, c) in journal.changes.iter().enumerate() {
                plan.conversion.validate_path(&repo, &c.path)?;
                if let Some(source) = &c.rename_from {
                    plan.conversion.validate_path(&repo, source)?;
                }
                ensure!(
                    unchanged(&self.root, c)?,
                    "File changed while applying patch: {}",
                    c.path
                );
                let target = safe_path(&self.root, &c.path)?;
                if let Some(source) = &c.rename_from {
                    fs::remove_file(safe_path(&self.root, source)?)?;
                    sync_dir(target.parent().context("Missing rename parent")?)?;
                    after_write(i)?;
                    plan.conversion.validate_path(&repo, &c.path)?;
                    plan.conversion.validate_path(&repo, source)?;
                    ensure!(
                        rename_state(&self.root, c)? == RenameState::Removed,
                        "Case-rename destination changed during patch application"
                    );
                    let temp =
                        safe_path(&self.root, c.temp.as_deref().context("Missing temp path")?)?;
                    if c.after.as_ref().is_some_and(|v| v.mode == 0o120000) {
                        create_link(
                            &target,
                            &read_content(&temp, &fs::symlink_metadata(&temp)?)?,
                        )?;
                    } else {
                        fs::hard_link(&temp, &target)?;
                    }
                    fs::remove_file(temp)?;
                } else if c.after.is_none() {
                    fs::remove_file(target)?;
                } else {
                    let temp =
                        safe_path(&self.root, c.temp.as_deref().context("Missing temp path")?)?;
                    if c.before.is_none() {
                        if c.after.as_ref().is_some_and(|v| v.mode == 0o120000) {
                            create_link(
                                &target,
                                &read_content(&temp, &fs::symlink_metadata(&temp)?)?,
                            )?;
                        } else {
                            fs::hard_link(&temp, &target)?;
                        }
                        fs::remove_file(temp)?;
                    } else {
                        fs::rename(temp, target)?;
                    }
                }
                sync_dir(
                    safe_path(&self.root, &c.path)?
                        .parent()
                        .context("Missing parent")?,
                )?;
                after_write(i)?;
            }
            plan.conversion.validate(&repo)?;
            Ok(())
        })();
        if let Err(error) = work {
            return match restore(&repo, &self.root) {
                Ok(()) => Err(error.context("Patch failed; affected files were restored")),
                Err(recovery) => Err(error.context(format!(
                    "Patch failed; recovery data retained at {}: {recovery:#}",
                    repo.path().join(JOURNAL).display()
                ))),
            };
        }
        match finish_journal(&repo, &journal) {
            Ok(()) => Ok(format!(
                "Applied patch to {} files; changes are unstaged",
                journal.changes.len()
            )),
            Err(error) => Ok(format!(
                "Applied patch to {} files; changes are unstaged. Journal cleanup needs attention: {error:#}",
                journal.changes.len()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Operation;
    use std::process::Command;

    fn case_rename_fixture() -> (tempfile::TempDir, Repository, BatchPlan) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let raw = raw(&repo).unwrap();
        raw.config()
            .unwrap()
            .set_bool("commit.gpgsign", false)
            .unwrap();
        raw.config()
            .unwrap()
            .set_str("core.hooksPath", "/dev/null")
            .unwrap();
        repo.execute(Operation::SetIdentity(
            "Test".into(),
            "test@example.invalid".into(),
        ))
        .unwrap();
        fs::write(
            repo.root.join("Readme.txt"),
            "one\ntwo\nthree\nfour\nfive\nsix\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                repo.root.join("Readme.txt"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        fs::write(repo.root.join("unrelated.txt"), "before\n").unwrap();
        #[cfg(target_os = "macos")]
        assert!(
            Command::new("xattr")
                .args(["-w", "org.gitbuddy.case-rename", "preserve"])
                .arg(repo.root.join("Readme.txt"))
                .status()
                .unwrap()
                .success()
        );
        repo.execute(Operation::StageAll).unwrap();
        repo.execute(Operation::Commit("base".into())).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo.root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["mv", "Readme.txt", "rename-intermediate"]);
        git(&["mv", "rename-intermediate", "README.txt"]);
        fs::write(
            repo.root.join("README.txt"),
            "one\ntwo\nTHREE\nfour\nfive\nsix\n",
        )
        .unwrap();
        fs::write(repo.root.join("unrelated.txt"), "after\n").unwrap();
        git(&["add", "-A"]);
        let patch = git(&["diff", "--cached", "--binary", "-M"]);
        assert!(String::from_utf8_lossy(&patch).contains("rename from Readme.txt"));
        fs::rename(
            repo.root.join("README.txt"),
            repo.root.join("rename-intermediate"),
        )
        .unwrap();
        fs::rename(
            repo.root.join("rename-intermediate"),
            repo.root.join("Readme.txt"),
        )
        .unwrap();
        fs::write(
            repo.root.join("Readme.txt"),
            "one\ntwo\nthree\nfour\nfive\nsix\n",
        )
        .unwrap();
        fs::write(repo.root.join("unrelated.txt"), "before\n").unwrap();
        git(&["reset", "--mixed", "HEAD"]);
        let plan = repo.prepare_patch_batch(patch).unwrap();
        (dir, repo, plan)
    }

    #[test]
    fn interrupted_case_rename_restores_original_spelling_from_both_mutation_points() {
        for crash_point in [1, 2, 3] {
            let (_dir, repo, plan) = case_rename_fixture();
            let raw = raw(&repo).unwrap();
            let index = fs::read(raw.path().join("index")).unwrap();
            let mut writes = 0;
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repo.apply_patch_batch_inner(&plan, &mut |_| {
                    writes += 1;
                    assert_ne!(writes, crash_point, "simulated process interruption");
                    Ok(())
                })
            }));
            assert!(interrupted.is_err());
            assert!(repo.has_patch_recovery().unwrap());
            #[cfg(unix)]
            if crash_point >= 2 {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(repo.root.join("README.txt"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
            }
            repo.execute(Operation::RecoverPatch).unwrap();
            let names = fs::read_dir(&repo.root)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>();
            assert!(names.contains(&"Readme.txt".into()));
            assert!(!names.contains(&"README.txt".into()));
            assert_eq!(
                fs::read_to_string(repo.root.join("Readme.txt")).unwrap(),
                "one\ntwo\nthree\nfour\nfive\nsix\n"
            );
            assert_eq!(
                fs::read_to_string(repo.root.join("unrelated.txt")).unwrap(),
                "before\n"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(repo.root.join("Readme.txt"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
            }
            assert_eq!(fs::read(raw.path().join("index")).unwrap(), index);
            #[cfg(target_os = "macos")]
            {
                let attributes = Command::new("xattr")
                    .args(["-p", "org.gitbuddy.case-rename"])
                    .arg(repo.root.join("Readme.txt"))
                    .output()
                    .unwrap();
                assert!(attributes.status.success());
                assert_eq!(
                    String::from_utf8_lossy(&attributes.stdout).trim(),
                    "preserve"
                );
            }
            assert!(!repo.has_patch_recovery().unwrap());
        }
    }

    #[test]
    fn interrupted_case_rename_refuses_external_destination_and_unknown_spelling() {
        for unknown_spelling in [false, true] {
            let (_dir, repo, plan) = case_rename_fixture();
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repo.apply_patch_batch_inner(&plan, &mut |_| -> Result<()> {
                    panic!("simulated interruption")
                })
            }));
            assert!(interrupted.is_err());
            let external = repo.root.join(if unknown_spelling {
                "readme.txt"
            } else {
                "README.txt"
            });
            fs::write(&external, "external edit\n").unwrap();
            assert!(repo.execute(Operation::RecoverPatch).is_err());
            assert_eq!(fs::read_to_string(&external).unwrap(), "external edit\n");
            assert!(repo.has_patch_recovery().unwrap());
            fs::remove_file(external).unwrap();
            repo.execute(Operation::RecoverPatch).unwrap();
        }
    }

    #[test]
    fn converted_case_batch_recovery_restores_raw_preimages_after_attributes_change() {
        for crash_point in [1, 2, 3] {
            let (_dir, repo, original_plan) = case_rename_fixture();
            fs::write(repo.root.join(".gitattributes"), "* text eol=crlf\n").unwrap();
            let before = b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n";
            fs::write(repo.root.join("Readme.txt"), before).unwrap();
            let plan = repo.prepare_patch_batch(original_plan.patch).unwrap();
            let mut writes = 0;
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repo.apply_patch_batch_inner(&plan, &mut |_| {
                    writes += 1;
                    assert_ne!(
                        writes, crash_point,
                        "simulated converted-batch interruption"
                    );
                    Ok(())
                })
            }));
            assert!(interrupted.is_err());
            fs::write(repo.root.join(".gitattributes"), "* text eol=lf\n").unwrap();
            repo.execute(Operation::RecoverPatch).unwrap();
            assert_eq!(fs::read(repo.root.join("Readme.txt")).unwrap(), before);
            assert_eq!(
                fs::read(repo.root.join("unrelated.txt")).unwrap(),
                b"before\n"
            );
            assert!(!repo.has_patch_recovery().unwrap());
        }
    }

    #[test]
    fn lfs_case_batch_recovery_uses_pinned_raw_bytes_after_filters_and_cache_disappear() {
        for crash_point in [1, 2, 3] {
            let (_dir, repo, _original_plan) = case_rename_fixture();
            let raw = raw(&repo).unwrap();
            raw.config().unwrap().set_str("lfs.storage", "lfs").unwrap();
            let before = fs::read(repo.root.join("Readme.txt")).unwrap();
            let after = b"one\ntwo\nTHREE\nfour\nfive\nsix\n";
            let pointer = |bytes: &[u8]| {
                format!(
                    "version https://git-lfs.github.com/spec/v1\noid sha256:{:x}\nsize {}\n",
                    Sha256::digest(bytes),
                    bytes.len()
                )
                .into_bytes()
            };
            fs::write(
                repo.root.join(".gitattributes"),
                "Readme.txt filter=lfs -text\nREADME.txt filter=lfs -text\n",
            )
            .unwrap();
            let mut index = raw.index().unwrap();
            index.add_path(Path::new(".gitattributes")).unwrap();
            let entry = index.get_path(Path::new("Readme.txt"), 0).unwrap();
            index.add_frombuffer(&entry, &pointer(&before)).unwrap();
            index.write().unwrap();
            repo.execute(Operation::Commit("LFS source".into()))
                .unwrap();
            for bytes in [before.as_slice(), after.as_slice()] {
                let oid = format!("{:x}", Sha256::digest(bytes));
                let path = raw
                    .commondir()
                    .join("lfs/objects")
                    .join(&oid[..2])
                    .join(&oid[2..4])
                    .join(&oid);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, bytes).unwrap();
            }
            let mut index = raw.index().unwrap();
            let mut entry = index.get_path(Path::new("Readme.txt"), 0).unwrap();
            index.remove_path(Path::new("Readme.txt")).unwrap();
            entry.path = b"README.txt".to_vec();
            index.add_frombuffer(&entry, &pointer(after)).unwrap();
            let entry = index.get_path(Path::new("unrelated.txt"), 0).unwrap();
            index.add_frombuffer(&entry, b"after\n").unwrap();
            index.write().unwrap();
            let patch = Command::new("git")
                .arg("-C")
                .arg(&repo.root)
                .args(["diff", "--cached", "--binary", "--find-renames=1%"])
                .output()
                .unwrap();
            assert!(patch.status.success());
            assert!(String::from_utf8_lossy(&patch.stdout).contains("rename from Readme.txt"));
            index
                .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
                .unwrap();
            index.write().unwrap();
            let index_before = fs::read(raw.path().join("index")).unwrap();
            let plan = repo.prepare_patch_batch(patch.stdout).unwrap();
            let mut writes = 0;
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repo.apply_patch_batch_inner(&plan, &mut |_| {
                    writes += 1;
                    assert_ne!(writes, crash_point, "simulated LFS patch interruption");
                    Ok(())
                })
            }));
            assert!(interrupted.is_err());
            fs::write(repo.root.join(".gitattributes"), "* -filter\n").unwrap();
            raw.config()
                .unwrap()
                .set_str("lfs.storage", "missing-cache")
                .unwrap();
            raw.config()
                .unwrap()
                .set_str("gitbuddy.lfsProgram", "/missing/git-lfs")
                .unwrap();
            fs::remove_dir_all(raw.commondir().join("lfs")).unwrap();
            repo.execute(Operation::RecoverPatch).unwrap();
            assert_eq!(fs::read(repo.root.join("Readme.txt")).unwrap(), before);
            assert_eq!(
                fs::read(repo.root.join("unrelated.txt")).unwrap(),
                b"before\n"
            );
            assert_eq!(fs::read(raw.path().join("index")).unwrap(), index_before);
            assert!(!repo.has_patch_recovery().unwrap());
        }
    }

    #[test]
    fn encoded_case_batch_recovers_raw_bom_and_bytes_without_reloading_encoding_rules() {
        for crash_point in [1, 2, 3] {
            let (_dir, repo, original_plan) = case_rename_fixture();
            let raw = raw(&repo).unwrap();
            fs::write(repo.root.join(".gitattributes"), "Readme.txt text eol=crlf working-tree-encoding=UTF-16LE\nREADME.txt text eol=crlf working-tree-encoding=UTF-16LE\n").unwrap();
            let id = raw
                .head()
                .unwrap()
                .peel_to_tree()
                .unwrap()
                .get_path(Path::new("Readme.txt"))
                .unwrap()
                .id();
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo.root)
                .args([
                    "cat-file",
                    "--filters",
                    "--path=Readme.txt",
                    &id.to_string(),
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            let before = output.stdout;
            fs::write(repo.root.join("Readme.txt"), &before).unwrap();
            let index = fs::read(raw.path().join("index")).unwrap();
            let plan = repo.prepare_patch_batch(original_plan.patch).unwrap();
            let mut writes = 0;
            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repo.apply_patch_batch_inner(&plan, &mut |_| {
                    writes += 1;
                    assert_ne!(writes, crash_point, "simulated encoded patch interruption");
                    Ok(())
                })
            }));
            assert!(interrupted.is_err());
            fs::write(
                repo.root.join(".gitattributes"),
                "* working-tree-encoding=UNSUPPORTED-AFTER-CRASH\n",
            )
            .unwrap();
            raw.config()
                .unwrap()
                .set_str("core.checkRoundtripEncoding", "UTF-16LE")
                .unwrap();
            repo.execute(Operation::RecoverPatch).unwrap();
            assert_eq!(fs::read(repo.root.join("Readme.txt")).unwrap(), before);
            assert_eq!(
                fs::read(repo.root.join("unrelated.txt")).unwrap(),
                b"before\n"
            );
            assert_eq!(fs::read(raw.path().join("index")).unwrap(), index);
            assert!(!repo.has_patch_recovery().unwrap());
        }
    }

    #[test]
    fn interrupted_batch_recovery_refuses_external_edits_and_restores_originals() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let raw = raw(&repo).unwrap();
        let mut config = raw.config().unwrap();
        config.set_bool("commit.gpgsign", false).unwrap();
        config.set_str("core.hooksPath", "/dev/null").unwrap();
        repo.execute(Operation::SetIdentity(
            "Test".into(),
            "test@example.invalid".into(),
        ))
        .unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(repo.root.join(name), "before\n").unwrap();
        }
        repo.execute(Operation::StageAll).unwrap();
        repo.execute(Operation::Commit("base".into())).unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(repo.root.join(name), "after\n").unwrap();
        }
        let patch = Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["diff", "--binary"])
            .output()
            .unwrap();
        assert!(patch.status.success());
        for name in ["a.txt", "b.txt"] {
            fs::write(repo.root.join(name), "before\n").unwrap();
        }
        let plan = repo.prepare_patch_batch(patch.stdout).unwrap();
        let mut fail_after_first = |index| -> Result<()> {
            if index == 0 {
                anyhow::bail!("injected write failure");
            }
            Ok(())
        };
        assert!(
            repo.apply_patch_batch_inner(&plan, &mut fail_after_first)
                .is_err()
        );
        for name in ["a.txt", "b.txt"] {
            assert_eq!(
                fs::read_to_string(repo.root.join(name)).unwrap(),
                "before\n"
            );
        }
        assert!(!repo.has_patch_recovery().unwrap());
        let journal_dir = raw.path().join(JOURNAL);
        fs::create_dir(&journal_dir).unwrap();
        let id = "gitbuddy-patch-pending-test123".to_owned();
        let head = crate::git::recovery::head_state(&raw).unwrap();
        let mut journal = Journal {
            version: 1,
            id: id.clone(),
            head_ref: head.reference,
            head_oid: head.id,
            changes: plan.changes.clone(),
        };
        for (i, c) in journal.changes.iter_mut().enumerate() {
            if c.before.is_some() {
                fs::copy(repo.root.join(&c.path), backup_path(&journal_dir, i)).unwrap();
            }
            if c.after.is_some() {
                c.temp = Some(
                    Path::new(&c.path)
                        .parent()
                        .unwrap()
                        .join(format!(".{id}-{i}.tmp"))
                        .to_str()
                        .unwrap()
                        .into(),
                );
            }
        }
        fs::write(
            journal_dir.join("manifest.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        fs::write(repo.root.join("a.txt"), "after\n").unwrap();
        fs::write(repo.root.join("b.txt"), "external\n").unwrap();
        assert!(repo.execute(Operation::StageAll).is_err());
        assert!(repo.execute(Operation::RecoverPatch).is_err());
        assert_eq!(
            fs::read_to_string(repo.root.join("a.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("b.txt")).unwrap(),
            "external\n"
        );
        fs::write(repo.root.join("b.txt"), "before\n").unwrap();
        repo.execute(Operation::RecoverPatch).unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("a.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("b.txt")).unwrap(),
            "before\n"
        );
        assert!(!repo.has_patch_recovery().unwrap());
    }
}

#[cfg(all(test, unix))]
mod path_race_tests {
    use super::*;
    #[test]
    fn stale_regular_file_metadata_does_not_follow_replacement_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let outside = dir.path().join("outside");
        fs::write(&path, b"original").unwrap();
        fs::write(&outside, b"outside bytes").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(read_content(&path, &metadata).is_err());
    }
}

#[cfg(all(test, unix))]
mod rollback_kind_tests {
    use super::*;
    use crate::git::Operation;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn mid_batch_failure_restores_link_text_and_executable_mode() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.execute(Operation::SetIdentity(
            "Test".into(),
            "test@example.invalid".into(),
        ))
        .unwrap();
        let r = raw(&repo).unwrap();
        r.config()
            .unwrap()
            .set_str("core.hooksPath", "/dev/null")
            .unwrap();
        r.config()
            .unwrap()
            .set_bool("commit.gpgsign", false)
            .unwrap();
        fs::write(repo.root.join("a-file"), "before\n").unwrap();
        symlink("outside-old", repo.root.join("z-link")).unwrap();
        repo.execute(Operation::StageAll).unwrap();
        repo.execute(Operation::Commit("base".into())).unwrap();
        fs::set_permissions(repo.root.join("a-file"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(repo.root.join("z-link")).unwrap();
        symlink("outside-new", repo.root.join("z-link")).unwrap();
        let patch = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["diff", "--binary"])
            .output()
            .unwrap();
        assert!(patch.status.success());
        fs::set_permissions(repo.root.join("a-file"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_file(repo.root.join("z-link")).unwrap();
        symlink("outside-old", repo.root.join("z-link")).unwrap();
        let plan = repo.prepare_patch_batch(patch.stdout).unwrap();
        let result = repo.apply_patch_batch_inner(&plan, &mut |index| {
            if index == 1 {
                anyhow::bail!("injected after second install");
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read_link(repo.root.join("z-link")).unwrap(),
            Path::new("outside-old")
        );
        assert_eq!(
            fs::metadata(repo.root.join("a-file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert!(!repo.has_patch_recovery().unwrap());
        assert!(fs::read_dir(&repo.root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("gitbuddy-patch-pending")
        }));
    }
}
