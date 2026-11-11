//! Index NAME extension associations; resolutions atomically update the index only.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
pub struct RenameConflict {
    pub paths: [Option<PathBuf>; 3],
    entries: [Option<Entry>; 3],
    token: Token,
    index: std::sync::Arc<Vec<u8>>,
    git_dir: PathBuf,
}
impl RenameConflict {
    pub fn summary(&self) -> String {
        ["Base", "Ours", "Theirs"]
            .into_iter()
            .zip(&self.paths)
            .map(|(label, path)| {
                format!(
                    "{label}: {}",
                    path.as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "deleted".into())
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn names(bytes: &[u8]) -> Result<Vec<[Option<Vec<u8>>; 3]>> {
    ensure!(
        bytes.len() >= 32 && &bytes[..4] == b"DIRC",
        "Invalid index header"
    );
    let version = u32::from_be_bytes(bytes[4..8].try_into()?);
    ensure!((2..=4).contains(&version), "Unsupported index format");
    let count = u32::from_be_bytes(bytes[8..12].try_into()?) as usize;
    let end = bytes.len() - 20; // libgit2 SHA-1 indexes have already validated the checksum.
    let mut offset = 12usize;
    for _ in 0..count {
        let start = offset;
        ensure!(
            offset.checked_add(62).is_some_and(|n| n <= end),
            "Truncated index entry"
        );
        let flags = u16::from_be_bytes(bytes[offset + 60..offset + 62].try_into()?);
        offset += 62;
        if flags & 0x4000 != 0 {
            offset += 2;
        }
        ensure!(offset < end, "Truncated extended index entry");
        if version == 4 {
            let mut length = 0;
            loop {
                let value = *bytes
                    .get(offset)
                    .filter(|_| offset < end)
                    .context("Invalid index path compression")?;
                offset += 1;
                length += 1;
                ensure!(length <= 10, "Invalid index path compression");
                if value & 128 == 0 {
                    break;
                }
            }
        }
        let nul = bytes
            .get(offset..end)
            .context("Truncated index path")?
            .iter()
            .position(|b| *b == 0)
            .context("Missing index path terminator")?;
        offset += nul + 1;
        if version < 4 {
            offset = start + (offset - start).div_ceil(8) * 8;
        }
        ensure!(offset <= end, "Invalid index padding");
    }
    let mut result = Vec::new();
    while offset < end {
        ensure!(offset + 8 <= end, "Truncated index extension");
        let signature = &bytes[offset..offset + 4];
        let size = u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        offset += 8;
        let limit = offset
            .checked_add(size)
            .filter(|n| *n <= end)
            .context("Invalid index extension size")?;
        if signature == b"NAME" {
            while offset < limit {
                let mut paths: [Option<Vec<u8>>; 3] = Default::default();
                for path in &mut paths {
                    let n = bytes[offset..limit]
                        .iter()
                        .position(|b| *b == 0)
                        .context("Invalid rename name")?;
                    if n > 0 {
                        *path = Some(bytes[offset..offset + n].to_vec());
                    }
                    offset += n + 1;
                }
                result.push(paths);
                ensure!(result.len() <= 10000, "Too many rename associations");
            }
        }
        offset = limit;
    }
    Ok(result)
}

pub(super) fn groups(
    repo: &git2::Repository,
    index: &Index,
) -> Result<Vec<std::sync::Arc<RenameConflict>>> {
    let bytes = std::sync::Arc::new(index_bytes(index)?);
    let mut used = BTreeSet::new();
    let mut result = Vec::new();
    for paths in names(&bytes)? {
        let unique: BTreeSet<_> = paths.iter().flatten().cloned().collect();
        if unique.len() < 2 || unique.iter().any(|p| used.contains(p)) {
            continue;
        }
        let mut entries: [Option<Entry>; 3] = Default::default();
        let mut complete = true;
        for (i, path) in paths.iter().enumerate() {
            if let Some(path) = path {
                entries[i] = index
                    .get_path(&super::super::path_from_bytes(path), (i + 1) as i32)
                    .map(Entry::from);
                if entries[i].is_none() {
                    complete = false;
                }
            }
        }
        if !complete {
            continue;
        }
        // Reject associations which overlap other stage entries at the same paths.
        for path in &unique {
            let p = super::super::path_from_bytes(path);
            if index.get_path(&p, 0).is_some() {
                complete = false;
            }
            for stage in 1..=3 {
                if let Some(entry) = index.get_path(&p, stage)
                    && entries[(stage - 1) as usize].as_ref().is_none_or(|e| {
                        e.bytes != entry.path || e.id != entry.id || e.mode != entry.mode
                    })
                {
                    complete = false;
                }
            }
        }
        if !complete {
            continue;
        }
        used.extend(unique);
        result.push(std::sync::Arc::new(RenameConflict {
            paths: paths.map(|p| p.map(|p| super::super::path_from_bytes(&p))),
            entries,
            token: token(repo)?,
            index: bytes.clone(),
            git_dir: repo.path().into(),
        }));
    }
    Ok(result)
}

impl Repository {
    pub(in crate::git) fn resolve_rename_conflict(
        &self,
        context: &RenameConflict,
        ours: bool,
    ) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == context.git_dir,
            "Rename group belongs to another repository"
        );
        let mut refs = repo.transaction()?;
        refs.lock_ref("HEAD")?;
        if context.token.head.reference != "HEAD" {
            refs.lock_ref(&context.token.head.reference)?;
        }
        let index = repo.index()?;
        let index_path = index.path().context("Missing index")?;
        let mut lock = IndexLock::acquire(index_path)?;
        ensure!(
            token(&repo)? == context.token
                && index_bytes(&index)?.as_slice() == context.index.as_slice(),
            "Rename group or index changed; refresh before resolving"
        );
        let scratch = tempfile::NamedTempFile::new_in(repo.path())?;
        fs::copy(index_path, scratch.path())?;
        let mut next = Index::open(scratch.path())?;
        let paths: BTreeSet<_> = context.paths.iter().flatten().collect();
        for path in paths {
            next.conflict_remove(path)?;
        }
        if let Some(entry) = &context.entries[if ours { 1 } else { 2 }] {
            ensure!(
                matches!(entry.mode, 0o100644 | 0o100755 | 0o120000 | 0o160000),
                "Unsupported rename entry type"
            );
            next.add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: entry.mode,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: entry.id,
                flags: 0,
                flags_extended: 0,
                path: entry.bytes.clone(),
            })?;
        }
        next.write()?;
        lock.write_and_install(&fs::read(scratch.path())?, index_path)?;
        Ok("Rename group resolved atomically in index; working files retained for review".into())
    }
}
