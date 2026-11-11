//! Canonical worktree diffs without replacing the real index or writing files.
use super::{Inspector, Limits};
use anyhow::{Context, Result, ensure};
use git2::{Diff, DiffFindOptions, DiffFormat, DiffOptions, IndexEntry, IndexTime};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub(in crate::git) fn working_diff<'a>(
    repo: &'a git2::Repository,
    options: &mut DiffOptions,
    paths: &[&Path],
    output_limit: Option<usize>,
) -> Result<Diff<'a>> {
    ensure!(
        !crate::git::worktree_conversion::pending(repo),
        "Recover the interrupted worktree conversion before reading a canonical diff"
    );
    let (mut inspector, _config) = Inspector::capture(repo)?;
    let index = repo.index()?;
    let native = repo.diff_index_to_workdir(Some(&index), Some(options))?;
    let mut candidates = paths
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<BTreeSet<_>>();
    if paths.is_empty() {
        for entry in index.iter() {
            candidates.insert(crate::git::path_from_bytes(&entry.path));
        }
    }
    for delta in native.deltas() {
        for path in [delta.old_file().path(), delta.new_file().path()]
            .into_iter()
            .flatten()
        {
            candidates.insert(path.to_path_buf());
        }
    }
    let root = repo
        .workdir()
        .context("Canonical diff needs a working tree")?;
    let mut managed = BTreeSet::new();
    let mut conversions = BTreeMap::new();
    for path in candidates {
        let previous = index.get_path(&path, 0);
        if previous
            .as_ref()
            .is_some_and(|entry| entry.flags & 0x8000 != 0 || entry.flags_extended & 0x4000 != 0)
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(root.join(&path)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        if previous
            .as_ref()
            .is_some_and(|entry| entry.mode == 0o120000)
            && repo
                .config()?
                .get_bool("core.symlinks")
                .is_ok_and(|enabled| !enabled)
        {
            continue;
        }
        let conversion = inspector.for_path(repo, &path)?;
        if !conversion.native() {
            ensure!(
                (1..=3).all(|stage| index.get_path(&path, stage).is_none()),
                "Canonical encoded conflicts require whole-file conflict resolution: {path:?}"
            );
            managed.insert(path.clone());
            conversions.insert(path, conversion);
        }
    }
    if managed.is_empty() {
        inspector.validate(repo)?;
        return Ok(native);
    }
    // A Diff may read its blobs lazily. Keep the temporary canonical content
    // on this read-only repository handle until the caller drops the Diff.
    let odb = repo.odb()?;
    let _memory = odb.add_new_mempack_backend(1000)?;
    // Reframe both halves with complete IDs. A parsed combined patch must not
    // infer exact renames from abbreviated object IDs without a repository.
    options.id_abbrev(40);
    let mut native = repo.diff_index_to_workdir(Some(&index), Some(options))?;
    native.find_similar(Some(
        DiffFindOptions::new().renames(true).for_untracked(true),
    ))?;
    for delta in native.deltas() {
        let names = [delta.old_file().path(), delta.new_file().path()];
        if names.iter().flatten().any(|path| managed.contains(*path)) {
            for path in names.into_iter().flatten() {
                managed.insert(path.to_path_buf());
            }
        }
    }
    let mut working = git2::Index::new()?;
    for entry in index.iter() {
        working.add(&entry)?;
    }
    for path in &managed {
        let metadata = match fs::symlink_metadata(root.join(path)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if working.get_path(path, 0).is_some() {
                    working.remove_path(path)?;
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "A mixed canonical rename changed its file type; refresh: {path:?}"
        );
        let previous = index.get_path(path, 0);
        let indexed = if let Some(entry) = &previous {
            ensure!(
                repo.odb()?.read_header(entry.id)?.0 <= Limits::ENCODED.max_bytes,
                "Indexed canonical diff base exceeds the conversion resource limit"
            );
            Some(repo.find_blob(entry.id)?)
        } else {
            None
        };
        let conversion = match conversions.remove(path) {
            Some(conversion) => conversion,
            None => inspector.for_path(repo, path)?,
        };
        let (bytes, mode) = conversion.canonical_file(
            repo,
            path,
            indexed.as_ref().map_or(&[], |blob| blob.content()),
            previous.as_ref().map(|entry| entry.mode),
            Limits::ENCODED,
        )?;
        let entry = IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: repo.blob(&bytes)?,
            flags: 0,
            flags_extended: 0,
            path: crate::git::libgit::path_bytes(path),
        };
        working.add(&entry)?;
    }
    let mut canonical = repo.diff_index_to_index(&index, &working, Some(options))?;
    canonical.find_similar(Some(DiffFindOptions::new().renames(true)))?;
    inspector.validate(repo)?;
    let native_remains = native.deltas().any(|delta| {
        ![delta.old_file().path(), delta.new_file().path()]
            .into_iter()
            .flatten()
            .any(|path| managed.contains(path))
    });
    if !native_remains {
        return Ok(canonical);
    }
    let mut bytes = Vec::new();
    append(&native, &managed, false, &mut bytes, output_limit)?;
    append(&canonical, &managed, true, &mut bytes, output_limit)?;
    if bytes.is_empty() {
        return Ok(repo.diff_index_to_index(&index, &index, Some(options))?);
    }
    Ok(Diff::from_buffer(&bytes)?)
}

fn append(
    diff: &Diff<'_>,
    managed: &BTreeSet<PathBuf>,
    include: bool,
    output: &mut Vec<u8>,
    limit: Option<usize>,
) -> Result<()> {
    let mut exceeded = false;
    let result = diff.print(DiffFormat::Patch, |delta, _, line| {
        let matched = [delta.old_file().path(), delta.new_file().path()]
            .into_iter()
            .flatten()
            .any(|path| managed.contains(path));
        if matched != include {
            return true;
        }
        let prefix = matches!(line.origin(), '+' | '-' | ' ');
        if limit.is_some_and(|limit| {
            output
                .len()
                .saturating_add(line.content().len())
                .saturating_add(usize::from(prefix))
                > limit
        }) {
            exceeded = true;
            return false;
        }
        if prefix {
            output.push(line.origin() as u8);
        }
        output.extend_from_slice(line.content());
        true
    });
    ensure!(!exceeded, "Canonical patch exceeds its output byte limit");
    result?;
    Ok(())
}
