//! Journaled encoded-worktree checkout. Native Git objects remain canonical;
//! only verified-equivalent current files are normalized for libgit2's safety
//! checks. The caller acknowledges the declared ref transition afterwards.
use super::{
    NetworkControl, Repository,
    conversion::{CleanPolicy, FilterDrivers, Limits, Snapshot},
    lfs::{AttributeDirectoryView, AttributeLookup, AttributeState, query_attributes},
    libgit::{WriterLock, raw},
};
use anyhow::{Context, Result, bail, ensure};
use git2::{Oid, Repository as Raw, build::CheckoutBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const JOURNAL: &str = "gitbuddy-worktree-conversion.json";
const RECOVERY: &str = "gitbuddy-worktree-conversions";
const PRIVATE_REFS: &str = "refs/gitbuddy/worktree-conversions/";
const MAX_JOURNAL: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CheckoutMode {
    #[default]
    Safe,
    Force,
    /// Finish a native initial checkout after proving its files are unchanged.
    Initialize,
}
#[derive(Clone, Debug)]
pub struct CheckoutHead {
    pub reference: Option<String>,
    pub id: Oid,
}
#[derive(Clone, Debug)]
pub struct CheckoutOptions {
    /// Empty means the full native checkout; otherwise literal paths/directories.
    pub paths: Vec<PathBuf>,
    pub mode: CheckoutMode,
    pub update_index: bool,
    /// None preserves HEAD. Declare ordinary checkout/reset transitions before
    /// writing so a restart can finish publication rather than guess intent.
    pub publication: Option<CheckoutHead>,
}
impl Default for CheckoutOptions {
    fn default() -> Self {
        Self {
            paths: vec![],
            mode: CheckoutMode::Safe,
            update_index: true,
            publication: None,
        }
    }
}
#[must_use = "publish the declared refs, then acknowledge this checkout receipt"]
#[derive(Clone, Debug)]
pub struct CheckoutReceipt {
    expected: Option<String>,
    native: Option<NativeReceipt>,
}
#[derive(Clone, Debug)]
struct NativeReceipt {
    git_dir: PathBuf,
    publication: Option<CheckoutHead>,
    paths: Vec<PathBuf>,
}
#[derive(Clone, Debug)]
pub struct WorktreeRecoveryStatus {
    pub expected: String,
    pub owner: PathBuf,
    pub phase: String,
    pub original_head: String,
    pub target_head: String,
    pub can_continue: bool,
    pub recovery: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Kind {
    Missing,
    File,
    Symlink,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Version {
    kind: Kind,
    hash: String,
    size: u64,
    mode: u32,
    stamp: Vec<u64>,
}
impl Version {
    fn same_content(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.hash == other.hash
            && self.size == other.size
            && self.mode == other.mode
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Data {
    hash: String,
    size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    #[serde(with = "crate::session::path_encoding")]
    path: PathBuf,
    before: Version,
    current: Version,
    normalized: Option<Data>,
    #[serde(default)]
    native_expected: Option<Version>,
    #[serde(default)]
    native_delete: bool,
    target: Option<Data>,
    target_mode: u32,
    materialized: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Reference {
    Direct(String),
    Symbolic(String),
}
type Refs = BTreeMap<String, Reference>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    NativePending,
    NativeComplete,
    Ready,
    Aborting,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Policy {
    #[serde(with = "crate::session::path_encoding")]
    path: PathBuf,
    fingerprint: [u8; 32],
    #[serde(default)]
    working_fingerprint: Option<[u8; 32]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    id: String,
    #[serde(with = "crate::session::path_encoding")]
    owner: PathBuf,
    #[serde(with = "crate::session::path_encoding")]
    git_dir: PathBuf,
    #[serde(with = "crate::session::path_encoding")]
    common: PathBuf,
    owner_identity: Vec<u64>,
    git_identity: Vec<u64>,
    common_identity: Vec<u64>,
    tree: String,
    config: String,
    policy: Vec<Policy>,
    refs_before: Refs,
    refs_after: Refs,
    #[serde(default)]
    track_orig_head: bool,
    index_before: Option<Data>,
    index_after: Option<Data>,
    #[serde(default)]
    refresh_index: bool,
    #[serde(default)]
    refreshing_index: bool,
    #[serde(default)]
    index_inflight: Option<Data>,
    #[serde(default)]
    pins: BTreeMap<String, String>,
    #[serde(default)]
    lfs_policy: Option<[u8; 32]>,
    #[serde(default)]
    missing_lfs_objects: usize,
    records: Vec<Record>,
    phase: Phase,
    inflight: Option<usize>,
}
type Entries = BTreeMap<PathBuf, (Oid, u32)>;
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn data(bytes: &[u8]) -> Data {
    Data {
        hash: digest(bytes),
        size: bytes.len() as u64,
    }
}
fn io_data(path: &Path) -> Result<Data> {
    let mut file = open_regular(path)?;
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok(Data {
        hash: format!("{:x}", hash.finalize()),
        size,
    })
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
    ensure!(
        file.metadata()?.is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    Ok(file)
}
pub(in crate::git) fn pending(repo: &Raw) -> bool {
    match fs::symlink_metadata(repo.commondir().join(JOURNAL)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}
fn no_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(
            !m.file_type().is_symlink(),
            "Checkout must not traverse a symlink: {}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty() && !path.as_os_str().as_encoded_bytes().contains(&0)
        && path.components().all(|p|matches!(p,Component::Normal(s) if !s.eq_ignore_ascii_case(std::ffi::OsStr::new(".git")))), "Unsafe checkout path");
    Ok(())
}
fn safe(root: &Path, path: &Path) -> Result<PathBuf> {
    validate_relative_path(path)?;
    let parts: Vec<_> = path.components().collect();
    let mut full = root.to_path_buf();
    no_symlink(&full)?;
    for (i, part) in parts.iter().enumerate() {
        full.push(part.as_os_str());
        if i + 1 < parts.len() {
            no_symlink(&full)?;
        }
    }
    Ok(full)
}
fn stamp(metadata: &fs::Metadata) -> Vec<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        vec![
            metadata.dev(),
            metadata.ino(),
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
            metadata.uid() as u64,
            metadata.gid() as u64,
        ]
    }
    #[cfg(not(unix))]
    {
        vec![
            metadata.len(),
            metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |t| t.as_nanos() as u64),
        ]
    }
}
fn directory_identity(path: &Path) -> Result<Vec<u64>> {
    let metadata = fs::metadata(path)?;
    ensure!(metadata.is_dir(), "Checkout owner is no longer a directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(vec![metadata.dev(), metadata.ino()])
    }
    #[cfg(not(unix))]
    {
        Ok(vec![
            metadata
                .created()?
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos() as u64,
        ])
    }
}
fn version(root: &Path, path: &Path) -> Result<Version> {
    let full = safe(root, path)?;
    let metadata = match fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Version {
                kind: Kind::Missing,
                hash: String::new(),
                size: 0,
                mode: 0,
                stamp: vec![],
            });
        }
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    };
    #[cfg(not(unix))]
    let mode = if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    let (kind, bytes) = if metadata.file_type().is_symlink() {
        (
            Kind::Symlink,
            Some(super::libgit::path_bytes(&fs::read_link(&full)?)),
        )
    } else {
        ensure!(
            metadata.is_file(),
            "Checkout cannot preserve a directory/special file at {}",
            full.display()
        );
        (Kind::File, None)
    };
    let measured = match bytes {
        Some(bytes) => data(&bytes),
        None => io_data(&full)?,
    };
    let after = fs::symlink_metadata(&full)?;
    ensure!(
        stamp(&metadata) == stamp(&after) && metadata.len() == after.len(),
        "Working file changed while inspecting {}",
        path.display()
    );
    Ok(Version {
        kind,
        hash: measured.hash,
        size: measured.size,
        mode,
        stamp: stamp(&after),
    })
}
fn config(repo: &Raw) -> Result<String> {
    let fresh = Raw::open(repo.path())?;
    let config = fresh.config()?.snapshot()?;
    let mut entries = config.entries(None)?;
    let mut values = vec![];
    let mut names = BTreeSet::new();
    while let Some(e) = entries.next() {
        let e = e?;
        names.insert(e.name()?.to_owned());
        values.push((
            e.name_bytes().to_vec(),
            e.has_value().then(|| e.value_bytes().to_vec()),
            e.level() as i32,
        ));
    }
    values.sort();
    let mut effective = Vec::with_capacity(names.len());
    for name in names {
        let entry = config.get_entry(&name)?;
        effective.push((
            name,
            entry.has_value().then(|| entry.value_bytes().to_vec()),
            entry.level() as i32,
        ));
    }
    Ok(digest(&serde_json::to_vec(&(values, effective))?))
}
fn references(repo: &Raw, track_orig_head: bool) -> Result<Refs> {
    let mut refs = BTreeMap::new();
    for item in repo.references()? {
        let item = item?;
        let name = item.name()?;
        if name.starts_with(PRIVATE_REFS) {
            continue;
        }
        refs.insert(name.to_owned(), reference_value(&item)?);
    }
    refs.insert(
        "HEAD".into(),
        reference_value(&repo.find_reference("HEAD")?)?,
    );
    if track_orig_head {
        match repo.find_reference("ORIG_HEAD") {
            Ok(reference) => {
                refs.insert("ORIG_HEAD".into(), reference_value(&reference)?);
            }
            Err(error) if error.code() == git2::ErrorCode::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(refs)
}
fn reference_value(reference: &git2::Reference<'_>) -> Result<Reference> {
    if let Some(name) = reference.symbolic_target()? {
        Ok(Reference::Symbolic(name.into()))
    } else {
        Ok(Reference::Direct(
            reference
                .target()
                .context("Reference has no target")?
                .to_string(),
        ))
    }
}
fn head_id(refs: &Refs) -> Option<&str> {
    match refs.get("HEAD")? {
        Reference::Direct(id) => Some(id),
        Reference::Symbolic(name) => match refs.get(name) {
            Some(Reference::Direct(id)) => Some(id),
            _ => None,
        },
    }
}
fn describe_head(refs: &Refs) -> String {
    match refs.get("HEAD") {
        Some(Reference::Direct(id)) => format!("detached HEAD @ {id}"),
        Some(Reference::Symbolic(name)) => match head_id(refs) {
            Some(id) => format!("{name} @ {id}"),
            None => format!("unborn {name}"),
        },
        None => "unknown HEAD".into(),
    }
}
fn owner(repo: &Raw, state: &State) -> Result<()> {
    ensure!(
        repo.workdir()
            .context("Checkout requires a worktree")?
            .canonicalize()?
            == state.owner
            && repo.path().canonicalize()? == state.git_dir
            && repo.commondir().canonicalize()? == state.common,
        "Checkout recovery belongs to another worktree: {}",
        state.owner.display()
    );
    ensure!(
        directory_identity(&state.owner)? == state.owner_identity
            && directory_identity(&state.git_dir)? == state.git_identity
            && directory_identity(&state.common)? == state.common_identity,
        "Checkout repository identity changed"
    );
    Ok(())
}
fn read_index(repo: &Raw) -> Result<Option<Vec<u8>>> {
    match fs::read(repo.path().join("index")) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn index_version(repo: &Raw) -> Result<Option<Data>> {
    Ok(read_index(repo)?.as_deref().map(data))
}
fn entries(repo: &Raw, tree: &git2::Tree<'_>, prefix: &Path, result: &mut Entries) -> Result<()> {
    for entry in tree {
        let path = prefix.join(super::path_from_bytes(entry.name_bytes()));
        validate_relative_path(&path)?;
        if entry.kind() == Some(git2::ObjectType::Tree) {
            entries(repo, &repo.find_tree(entry.id())?, &path, result)?;
        } else {
            result.insert(path, (entry.id(), entry.filemode() as u32));
        }
    }
    Ok(())
}
fn indexed(repo: &Raw) -> Result<(git2::Index, Entries)> {
    let fresh = Raw::open(repo.path())?;
    let index = fresh.index()?;
    let values = index
        .iter()
        .filter(|e| e.flags & 0x3000 == 0)
        .map(|e| (super::path_from_bytes(&e.path), (e.id, e.mode)))
        .collect();
    Ok((index, values))
}
fn encoded_blob(repo: &Raw, oid: Oid) -> Result<git2::Blob<'_>> {
    ensure!(
        repo.odb()?.read_header(oid)?.0 <= Limits::ENCODED.max_bytes,
        "Encoded canonical blob exceeds conversion limit"
    );
    Ok(repo.find_blob(oid)?)
}
fn selected(path: &Path, paths: Option<&[PathBuf]>) -> bool {
    paths.is_none_or(|paths| {
        paths.is_empty() || paths.iter().any(|p| path == p || path.starts_with(p))
    })
}
fn candidate_paths(
    repo: &Raw,
    target: Option<&git2::Tree<'_>>,
    paths: Option<&[PathBuf]>,
) -> Result<(Entries, Entries, Entries, BTreeSet<PathBuf>)> {
    let (_, before) = indexed(repo)?;
    let mut baseline = Entries::new();
    let mut after = Entries::new();
    if let Some(tree) = target {
        entries(repo, tree, Path::new(""), &mut after)?;
        if read_index(repo)?.is_some() {
            match repo.head() {
                Ok(head) => entries(repo, &head.peel_to_tree()?, Path::new(""), &mut baseline)?,
                Err(e) if e.code() == git2::ErrorCode::UnbornBranch => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    let mut all: BTreeSet<_> = before
        .keys()
        .chain(after.keys())
        .chain(baseline.keys())
        .filter(|p| selected(p, paths))
        .cloned()
        .collect();
    if target.is_none() {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        for entry in repo.statuses(Some(&mut opts))?.iter() {
            let path = super::path_from_bytes(entry.path_bytes());
            if selected(&path, paths) {
                all.insert(path);
            }
        }
    }
    Ok((before, after, baseline, all))
}
#[derive(Default)]
struct ConversionPaths {
    encoded: BTreeSet<PathBuf>,
    current_lfs: BTreeMap<PathBuf, Vec<AttributeState>>,
    target_lfs: BTreeMap<PathBuf, Vec<AttributeState>>,
    current_filters: BTreeMap<PathBuf, Vec<u8>>,
    target_filters: BTreeMap<PathBuf, Vec<u8>>,
    views: Vec<AttributeDirectoryView>,
}
impl ConversionPaths {
    fn validate(&self, repo: &Raw) -> Result<()> {
        for view in &self.views {
            view.verify(repo)?;
        }
        Ok(())
    }
    fn validate_filters(&self, drivers: &FilterDrivers, path: &Path) -> Result<()> {
        for driver in [
            self.current_filters.get(path),
            self.target_filters.get(path),
        ]
        .into_iter()
        .flatten()
        {
            drivers.validate(driver, path)?;
        }
        Ok(())
    }
}
fn conversion_paths(
    repo: &Raw,
    tree: Option<Oid>,
    paths: &BTreeSet<PathBuf>,
) -> Result<ConversionPaths> {
    let mut result = ConversionPaths::default();
    let mut directories: BTreeMap<PathBuf, Vec<&PathBuf>> = BTreeMap::new();
    for path in paths {
        directories
            .entry(path.parent().unwrap_or(Path::new("")).to_path_buf())
            .or_default()
            .push(path);
    }
    for lookup in std::iter::once(AttributeLookup::WorktreeThenIndex)
        .chain(tree.map(AttributeLookup::TargetTree))
    {
        for siblings in directories.values() {
            let view = AttributeDirectoryView::capture(
                repo,
                siblings[0],
                lookup,
                &[
                    "working-tree-encoding",
                    "filter",
                    "text",
                    "crlf",
                    "eol",
                    "ident",
                ],
            )?;
            for path in siblings {
                let values = view.values(path)?;
                if matches!(
                    values.get("working-tree-encoding"),
                    Some(AttributeState::Set | AttributeState::Value(_))
                ) {
                    result.encoded.insert((*path).clone());
                }
                if let Some(AttributeState::Value(filter)) = values.get("filter") {
                    let target = matches!(lookup, AttributeLookup::TargetTree(_));
                    if filter == b"lfs" {
                        let set = if target {
                            &mut result.target_lfs
                        } else {
                            &mut result.current_lfs
                        };
                        set.insert(
                            (*path).clone(),
                            ["text", "crlf", "eol", "ident"]
                                .into_iter()
                                .map(|name| {
                                    values
                                        .get(name)
                                        .cloned()
                                        .unwrap_or(AttributeState::Unspecified)
                                })
                                .collect(),
                        );
                    } else {
                        let map = if target {
                            &mut result.target_filters
                        } else {
                            &mut result.current_filters
                        };
                        map.insert((*path).clone(), filter.clone());
                    }
                }
            }
            view.verify(repo)?;
            result.views.push(view);
        }
    }
    Ok(result)
}
fn regular_paths(
    all: &BTreeSet<PathBuf>,
    before: &Entries,
    target: &Entries,
    baseline: &Entries,
) -> BTreeSet<PathBuf> {
    all.iter()
        .filter(|p| {
            [before, target, baseline].iter().any(|entries| {
                entries
                    .get(*p)
                    .is_some_and(|(_, mode)| matches!(mode, 0o100644 | 0o100755))
            })
        })
        .cloned()
        .collect()
}
pub(in crate::git) fn needs_encoded_checkout(
    repo: &Raw,
    target: Option<Oid>,
    paths: Option<&[PathBuf]>,
) -> Result<bool> {
    let tree = target
        .map(|id| repo.find_object(id, None)?.peel_to_tree())
        .transpose()?;
    let (before, after, baseline, all) = candidate_paths(repo, tree.as_ref(), paths)?;
    let paths = if tree.is_some() {
        regular_paths(&all, &before, &after, &baseline)
    } else {
        all
    };
    Ok(
        !conversion_paths(repo, tree.as_ref().map(|t| t.id()), &paths)?
            .encoded
            .is_empty(),
    )
}
/// Call this before an unadapted native writer. It never pretends raw encoded
/// bytes are supported; a common-dir pending journal also blocks sibling tabs.
pub(in crate::git) fn guard_native_write(
    repo: &Raw,
    target: Option<Oid>,
    paths: Option<&[PathBuf]>,
) -> Result<()> {
    ensure!(
        !pending(repo),
        "Recover the interrupted encoded checkout before writing this repository"
    );
    ensure!(
        !needs_encoded_checkout(repo, target, paths)?,
        "This native write does not yet support working-tree-encoding; use the guarded checkout path"
    );
    Ok(())
}
pub(in crate::git) fn guard_native_history_write(repo: &Raw, target: Option<Oid>) -> Result<()> {
    ensure!(
        !history_needs_encoded_checkout(repo, target)?,
        "This native write does not yet support working-tree-encoding; use the guarded checkout path"
    );
    Ok(())
}
pub(in crate::git) fn history_needs_encoded_checkout(
    repo: &Raw,
    target: Option<Oid>,
) -> Result<bool> {
    ensure!(
        !pending(repo),
        "Recover the interrupted encoded checkout before writing this repository"
    );
    let tree = target
        .map(|id| repo.find_object(id, None)?.peel_to_tree())
        .transpose()?;
    let (_, _, _, paths) = candidate_paths(repo, tree.as_ref(), None)?;
    // Native merges can combine a macro declaration from one tree with a
    // previously dormant macro use from the other tree.
    let mut directories = BTreeMap::new();
    for path in &paths {
        directories
            .entry(path.parent().unwrap_or(Path::new("")))
            .or_insert(path);
    }
    for lookup in std::iter::once(AttributeLookup::WorktreeThenIndex).chain(
        tree.as_ref()
            .map(|tree| AttributeLookup::TargetTree(tree.id())),
    ) {
        for path in directories.values() {
            let view =
                AttributeDirectoryView::capture(repo, path, lookup, &["working-tree-encoding"])?;
            if view.declares_worktree_encoding() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn artifact(dir: &Path, index: usize, suffix: &str) -> PathBuf {
    dir.join(format!("{index}.{suffix}"))
}
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("No parent directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    sync_directory(parent)
}
fn store(path: &Path, bytes: &[u8]) -> Result<Data> {
    atomic(path, bytes)?;
    Ok(data(bytes))
}
fn copy_metadata(source: &Path, target: &fs::File) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let source = fs::File::open(source)?;
        ensure!(
            unsafe {
                libc::fcopyfile(
                    source.as_raw_fd(),
                    target.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            } == 0,
            "Cannot preserve file metadata"
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) } == 0,
            "Encoded checkout cannot safely preserve extended attributes on this platform"
        );
        target.set_permissions(fs::metadata(source)?.permissions())?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    target.set_permissions(fs::metadata(source)?.permissions())?;
    Ok(())
}
fn save_raw(root: &Path, path: &Path, expected: &Version, destination: &Path) -> Result<()> {
    match expected.kind {
        Kind::Missing => (),
        Kind::Symlink => {
            store(
                destination,
                &super::libgit::path_bytes(&fs::read_link(safe(root, path)?)?),
            )?;
        }
        Kind::File => {
            let full = safe(root, path)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            std::io::copy(&mut open_regular(&full)?, &mut output)?;
            copy_metadata(&full, &output)?;
            output.sync_all()?;
        }
    }
    if expected.kind != Kind::Missing {
        ensure!(
            io_data(destination)?
                == Data {
                    hash: expected.hash.clone(),
                    size: expected.size
                },
            "Raw recovery copy changed"
        );
    }
    ensure!(
        version(root, path)? == *expected,
        "Working file changed during preservation"
    );
    Ok(())
}
fn session(repo: &Raw, state: &State) -> Result<PathBuf> {
    ensure!(
        state.id.starts_with("checkout-")
            && state
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid checkout recovery identity"
    );
    let base = repo.commondir().join(RECOVERY);
    no_symlink(&base)?;
    let path = base.join(&state.id);
    no_symlink(&path)?;
    Ok(path)
}
fn save(repo: &Raw, state: &State) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    ensure!(
        bytes.len() as u64 <= MAX_JOURNAL,
        "Checkout journal exceeds safety limit"
    );
    let path = repo.commondir().join(JOURNAL);
    no_symlink(&path)?;
    atomic(&path, &bytes)
}
fn load(repo: &Raw) -> Result<(State, String)> {
    let path = repo.commondir().join(JOURNAL);
    no_symlink(&path)?;
    ensure!(
        fs::metadata(&path)?.len() <= MAX_JOURNAL,
        "Checkout journal exceeds safety limit"
    );
    let bytes = fs::read(path)?;
    let state: State = serde_json::from_slice(&bytes)
        .context("Invalid checkout conversion journal; preserve it for manual recovery")?;
    ensure!(
        state.version == 1 && state.common == repo.commondir().canonicalize()?,
        "Invalid checkout journal owner"
    );
    session(repo, &state)?;
    Oid::from_str(&state.tree)?;
    ensure!(
        state.records.windows(2).all(|w| w[0].path < w[1].path),
        "Invalid checkout path order"
    );
    for record in &state.records {
        safe(&state.owner, &record.path)?;
    }
    for refs in [&state.refs_before, &state.refs_after] {
        for (name, value) in refs {
            ensure!(
                matches!(name.as_str(), "HEAD" | "ORIG_HEAD")
                    || git2::Reference::is_valid_name(name),
                "Invalid recorded refname"
            );
            match value {
                Reference::Direct(id) => {
                    Oid::from_str(id)?;
                }
                Reference::Symbolic(name) => {
                    ensure!(git2::Reference::is_valid_name(name), "Invalid symbolic ref")
                }
            }
        }
    }
    ensure!(
        state.inflight.is_none_or(|n| n < state.records.len()),
        "Invalid checkout in-flight path"
    );
    Ok((state, digest(&bytes)))
}
fn remove_journal(repo: &Raw) -> Result<()> {
    fs::remove_file(repo.commondir().join(JOURNAL))?;
    sync_directory(repo.commondir())
}
fn finish_checkout(repo: &Raw, state: &State) -> Result<()> {
    let dir = session(repo, state)?;
    // Publication is durable. Remove the active pointer before disposing of
    // the recovery data, so a crash during cleanup cannot strand a journal
    // that still offers to restore deleted data.
    remove_journal(repo)?;
    let mut transaction = repo.transaction()?;
    for (name, id) in &state.pins {
        ensure!(
            name.starts_with(&format!("{PRIVATE_REFS}{}/", state.id)),
            "Invalid checkout recovery ref"
        );
        transaction.lock_ref(name)?;
        let current = repo.find_reference(name)?;
        ensure!(
            current.target() == Some(Oid::from_str(id)?),
            "Checkout recovery ref changed: {name}"
        );
        transaction.remove(name)?;
    }
    transaction.commit()?;
    fs::remove_dir_all(&dir)?;
    sync_directory(dir.parent().context("Missing checkout recovery parent")?)
}
fn refs_allowed(current: &Refs, before: &Refs, after: &Refs) -> bool {
    current
        .keys()
        .chain(before.keys())
        .chain(after.keys())
        .all(|name| current.get(name) == before.get(name) || current.get(name) == after.get(name))
}
fn verify_refs(repo: &Raw, state: &State) -> Result<()> {
    ensure!(
        refs_allowed(
            &references(repo, state.track_orig_head)?,
            &state.refs_before,
            &state.refs_after
        ),
        "References changed outside this checkout; preserve recovery data and review before continuing"
    );
    Ok(())
}
fn apply_refs(repo: &Raw, expected: &Refs, desired: &Refs, track_orig_head: bool) -> Result<()> {
    let mut tx = repo.transaction()?;
    let changed: BTreeSet<_> = expected
        .keys()
        .chain(desired.keys())
        .filter(|key| expected.get(*key) != desired.get(*key))
        .cloned()
        .collect();
    for name in &changed {
        tx.lock_ref(name)?;
    }
    ensure!(
        references(repo, track_orig_head)? == *expected,
        "References changed while locking checkout publication"
    );
    let signature = git2::Signature::now("GitBuddy", "gitbuddy@localhost")?;
    for name in changed {
        match desired.get(&name) {
            Some(Reference::Direct(id)) => tx.set_target(
                &name,
                Oid::from_str(id)?,
                Some(&signature),
                "gitbuddy: encoded checkout recovery",
            )?,
            Some(Reference::Symbolic(target)) => tx.set_symbolic_target(
                &name,
                target,
                Some(&signature),
                "gitbuddy: encoded checkout recovery",
            )?,
            None => tx.remove(&name)?,
        }
    }
    tx.commit()?;
    sync_publication(repo, expected, desired)?;
    Ok(())
}
fn sync_publication(repo: &Raw, before: &Refs, after: &Refs) -> Result<()> {
    let changed: BTreeSet<_> = before
        .keys()
        .chain(after.keys())
        .filter(|name| before.get(*name) != after.get(*name))
        .cloned()
        .collect();
    for name in changed {
        let worktree_ref = matches!(name.as_str(), "HEAD" | "ORIG_HEAD");
        let path = if worktree_ref {
            repo.path().join(&name)
        } else {
            repo.commondir().join(&name)
        };
        if path.exists() {
            fs::File::open(&path)?.sync_all()?;
        }
        for ancestor in path.parent().into_iter().flat_map(Path::ancestors) {
            sync_directory(ancestor)?;
            if ancestor == repo.commondir() {
                break;
            }
        }
        let log = if worktree_ref {
            repo.path().join("logs").join(&name)
        } else {
            repo.commondir().join("logs").join(&name)
        };
        if log.exists() {
            fs::File::open(&log)?.sync_all()?;
            sync_directory(log.parent().context("Missing reflog parent")?)?;
        }
    }
    Ok(())
}

fn expected_refs(
    repo: &Raw,
    before: &Refs,
    head: Option<&CheckoutHead>,
    tree: Oid,
) -> Result<Refs> {
    let mut after = before.clone();
    if let Some(head) = head {
        ensure!(
            repo.find_commit(head.id)?.tree_id() == tree,
            "Declared publication does not match checkout tree"
        );
        if let Some(name) = &head.reference {
            ensure!(
                name.starts_with("refs/heads/") && git2::Reference::is_valid_name(name),
                "Invalid checkout branch"
            );
            super::workspaces::branch_available(repo, name)?;
            ensure!(
                before.get("HEAD") == Some(&Reference::Symbolic(name.clone()))
                    || before.get(name) == Some(&Reference::Direct(head.id.to_string())),
                "Prepare the target branch before checking it out"
            );
            after.insert(name.clone(), Reference::Direct(head.id.to_string()));
            after.insert("HEAD".into(), Reference::Symbolic(name.clone()));
        } else {
            after.insert("HEAD".into(), Reference::Direct(head.id.to_string()));
        }
    }
    Ok(after)
}
fn native(
    repo: &Raw,
    target: &git2::Object<'_>,
    options: &CheckoutOptions,
    control: &NetworkControl,
) -> Result<()> {
    native_at(repo, target, options, None, control)
}

fn native_with_index_reservation(
    repo: &Raw,
    target: &git2::Object<'_>,
    options: &CheckoutOptions,
    control: &NetworkControl,
) -> Result<()> {
    // libgit2 can write working files before its index writer discovers a
    // foreign lock. Reserve the real index first and give checkout a private
    // copy so the reservation remains held until the new index is published.
    let directory = tempfile::Builder::new()
        .prefix("plain-checkout-")
        .tempdir_in(repo.path())?;
    let id = directory
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid temporary checkout identity")?;
    let lock = IndexReservation::acquire(repo, id)?;
    let before = read_index(repo)?;
    let scratch_path = directory.path().join("index");
    if let Some(bytes) = &before {
        atomic(&scratch_path, bytes)?;
    }
    let result = (|| -> Result<()> {
        super::lfs::dehydrate_paths(repo, &options.paths)?;
        let native_repo = Raw::open(repo.path())?;
        let mut scratch = git2::Index::open(&scratch_path)?;
        native_repo.set_index(&mut scratch)?;
        let target = native_repo.find_object(target.id(), None)?;
        native(&native_repo, &target, options, control)?;
        if options.update_index {
            lock.verify()?;
            ensure!(
                read_index(repo)? == before,
                "Index changed during checkout; newer index was kept"
            );
            let after = fs::read(&scratch_path)?;
            if Some(after.as_slice()) != before.as_deref() {
                atomic(&repo.path().join("index"), &after)?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = super::lfs::hydrate_paths(repo, &options.paths);
    }
    result
}
fn native_at(
    repo: &Raw,
    target: &git2::Object<'_>,
    options: &CheckoutOptions,
    directory: Option<&Path>,
    control: &NetworkControl,
) -> Result<()> {
    let mut checkout = CheckoutBuilder::new();
    match options.mode {
        CheckoutMode::Safe => {
            checkout.safe();
        }
        CheckoutMode::Force | CheckoutMode::Initialize => {
            checkout.force();
        }
    }
    if let Some(directory) = directory {
        checkout.target_dir(directory);
    }
    checkout
        .update_index(options.update_index)
        .overwrite_ignored(false)
        .remove_ignored(false)
        .remove_untracked(false)
        .disable_pathspec_match(true);
    for path in &options.paths {
        checkout.path(path);
    }
    let token = control.cancellation.clone();
    checkout
        .notify_on(git2::CheckoutNotificationType::all())
        .notify(move |_, _, _, _, _| !token.is_requested());
    repo.checkout_tree(target, Some(&mut checkout))?;
    Ok(())
}
fn native_preview(
    repo: &Raw,
    tree: Oid,
    paths: Vec<PathBuf>,
    parent: &Path,
    control: &NetworkControl,
) -> Result<BTreeMap<PathBuf, Version>> {
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let directory = tempfile::tempdir_in(parent)?;
    let preview_repo = Raw::open(repo.path())?;
    let target = preview_repo.find_object(tree, None)?;
    let options = CheckoutOptions {
        mode: CheckoutMode::Force,
        paths,
        update_index: false,
        publication: None,
    };
    native_at(
        &preview_repo,
        &target,
        &options,
        Some(directory.path()),
        control,
    )?;
    options
        .paths
        .into_iter()
        .map(|path| Ok((path.clone(), version(directory.path(), &path)?)))
        .collect()
}
fn matches_native(observed: &Version, pinned: &Version, respect_filemode: bool) -> bool {
    observed.kind == pinned.kind
        && observed.hash == pinned.hash
        && observed.size == pinned.size
        && (!respect_filemode
            || observed.kind != Kind::File
            || observed.mode & 0o111 == pinned.mode & 0o111)
}
fn sync_native_output(
    root: &Path,
    record: &Record,
    directories: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if record.current != record.before {
        let path = safe(root, &record.path)?;
        if record.current.kind == Kind::File {
            open_regular(&path)?.sync_all()?;
        }
        for dir in path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take_while(|dir| dir.starts_with(root))
        {
            match fs::symlink_metadata(dir) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_dir(),
                        "Checkout parent changed: {}",
                        dir.display()
                    );
                    directories.insert(dir.to_owned());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}
fn preserve_untracked_rules(
    repo: &Raw,
    tree: &Entries,
    current: &Entries,
    paths: &BTreeSet<PathBuf>,
) -> Result<()> {
    let root = repo.workdir().context("No worktree")?;
    for path in paths {
        for parent in path.parent().into_iter().flat_map(Path::ancestors) {
            let attrs = parent.join(".gitattributes");
            if !tree.contains_key(&attrs)
                && !current.contains_key(&attrs)
                && fs::symlink_metadata(safe(root, &attrs)?).is_ok()
            {
                bail!(
                    "Stage or remove the preserved untracked/ignored controlling attributes before encoded checkout: {}",
                    attrs.display()
                );
            }
        }
    }
    Ok(())
}
fn verify_preserved_rules(
    repo: &Raw,
    root: &Path,
    target: &Entries,
    index: &Entries,
    affected: &BTreeSet<PathBuf>,
    encoded: &BTreeSet<PathBuf>,
) -> Result<()> {
    let mut checked = BTreeSet::new();
    for path in affected
        .intersection(encoded)
        .filter(|path| target.contains_key(*path))
    {
        for parent in path.parent().into_iter().flat_map(Path::ancestors) {
            let attrs = parent.join(".gitattributes");
            if affected.contains(&attrs) || !checked.insert(attrs.clone()) {
                continue;
            }
            let indexed = index
                .get(&attrs)
                .filter(|(_, mode)| matches!(mode, 0o100644 | 0o100755));
            let expected = target
                .get(&attrs)
                .filter(|(_, mode)| matches!(mode, 0o100644 | 0o100755));
            let actual = version(root, &attrs)?;
            let matches = if actual.kind == Kind::File {
                expected.is_some_and(|(id, _)| {
                    repo.find_blob(*id)
                        .is_ok_and(|blob| actual.hash == digest(blob.content()))
                })
            } else {
                indexed.map(|entry| entry.0) == expected.map(|entry| entry.0)
            };
            ensure!(
                matches,
                "Preserved local attributes would disagree with encoded checkout: {}",
                attrs.display()
            );
        }
    }
    Ok(())
}
fn write_raw(
    root: &Path,
    path: &Path,
    expected: &Version,
    content: &Path,
    desired: &Data,
    mode: u32,
    restore_metadata: bool,
) -> Result<Version> {
    let destination = safe(root, path)?;
    let parent = destination.parent().context("No file parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut open_regular(content)?, &mut temp)?;
    ensure!(
        io_data(temp.path())? == *desired,
        "Prepared checkout bytes changed"
    );
    if restore_metadata {
        copy_metadata(content, temp.as_file())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
        }
    } else if expected.kind == Kind::File {
        copy_metadata(&destination, temp.as_file())?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    temp.as_file().sync_all()?;
    ensure!(
        version(root, path)? == *expected,
        "{} changed outside checkout; newer data was kept",
        path.display()
    );
    temp.persist(&destination)?;
    sync_directory(parent)?;
    version(root, path)
}
fn must_detach(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(metadata.nlink() > 1)
    }
    #[cfg(not(unix))]
    {
        // Where the standard library does not expose link counts, replace
        // the selected regular file before libgit2 can truncate its inode.
        Ok(metadata.is_file())
    }
}
fn pins(
    repo: &Raw,
    state: &State,
    index: &mut git2::Index,
    target: &git2::Tree<'_>,
) -> Result<BTreeMap<String, String>> {
    let mut pins = BTreeMap::new();
    let signature = git2::Signature::now("GitBuddy", "gitbuddy@localhost")?;
    let before_tree = repo.find_tree(index.write_tree_to(repo)?)?;
    for (name, tree, refs) in [
        ("before", &before_tree, &state.refs_before),
        ("target", target, &state.refs_after),
    ] {
        let parent = head_id(refs)
            .map(|id| repo.find_commit(Oid::from_str(id)?))
            .transpose()?;
        let commit = repo.commit(
            None,
            &signature,
            &signature,
            "GitBuddy encoded checkout recovery",
            tree,
            &parent.iter().collect::<Vec<_>>(),
        )?;
        let name = format!("{PRIVATE_REFS}{}/{name}", state.id);
        repo.reference(&name, commit, false, "gitbuddy: preserve encoded checkout")?;
        let path = repo.commondir().join(&name);
        fs::File::open(&path)?.sync_all()?;
        for ancestor in path.parent().into_iter().flat_map(Path::ancestors) {
            sync_directory(ancestor)?;
            if ancestor == repo.commondir() {
                break;
            }
        }
        pins.insert(name, commit.to_string());
    }
    Ok(pins)
}

// A real Git-compatible index reservation. Reopen only this session's durable
// marker after a crash; never unlink a foreign lock or an active reservation.
struct IndexReservation {
    file: fs::File,
    path: PathBuf,
    marker: Vec<u8>,
}
impl IndexReservation {
    fn acquire(repo: &Raw, id: &str) -> Result<Self> {
        let path = repo.path().join("index.lock");
        let marker = format!("GitBuddy checkout reservation {id}\n").into_bytes();
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let (mut file, created) = match opts.create_new(true).open(&path) {
            Ok(file) => (file, true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                opts.create_new(false);
                (opts.open(&path)?, false)
            }
            Err(e) => {
                return Err(e).context("Index is locked; finish its owner before encoded checkout");
            }
        };
        file.try_lock()?;
        if created {
            file.write_all(&marker)?;
            file.sync_all()?;
            sync_directory(repo.path())?;
        }
        let lock = Self { file, path, marker };
        lock.verify()?;
        Ok(lock)
    }
    fn verify(&self) -> Result<()> {
        let m = fs::symlink_metadata(&self.path)?;
        ensure!(
            m.is_file() && m.len() == self.marker.len() as u64,
            "Checkout index reservation changed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let held = self.file.metadata()?;
            ensure!(
                held.dev() == m.dev() && held.ino() == m.ino(),
                "Checkout index reservation replaced"
            );
        }
        ensure!(
            fs::read(&self.path)? == self.marker,
            "Foreign index lock exists; it was not removed"
        );
        Ok(())
    }
}
impl Drop for IndexReservation {
    fn drop(&mut self) {
        if self.verify().is_ok() {
            let _ = fs::remove_file(&self.path);
            if let Some(parent) = self.path.parent() {
                let _ = sync_directory(parent);
            }
        }
        let _ = self.file.unlock();
    }
}

/// The caller owns the shared WriterLock. No HEAD/ref is moved here; encoded
/// completion remains pending until acknowledge_checkout verifies publication.
pub(in crate::git) fn checkout_tree(
    repo: &Raw,
    target: &git2::Object<'_>,
    options: CheckoutOptions,
    control: &NetworkControl,
) -> Result<CheckoutReceipt> {
    checkout_tree_with_publication(repo, target, options, control, None)
}
pub(in crate::git) fn checkout_tree_with_orig_head(
    repo: &Raw,
    target: &git2::Object<'_>,
    options: CheckoutOptions,
    control: &NetworkControl,
    original: Oid,
) -> Result<CheckoutReceipt> {
    ensure!(
        options.publication.is_some(),
        "ORIG_HEAD requires a declared checkout HEAD"
    );
    checkout_tree_with_publication(repo, target, options, control, Some(original))
}
fn checkout_tree_with_publication(
    repo: &Raw,
    target: &git2::Object<'_>,
    options: CheckoutOptions,
    control: &NetworkControl,
    orig_head: Option<Oid>,
) -> Result<CheckoutReceipt> {
    ensure!(
        !pending(repo),
        "Recover the interrupted encoded checkout first"
    );
    control.check()?;
    for path in &options.paths {
        validate_relative_path(path)?;
    }
    ensure!(
        options.publication.is_none() || options.paths.is_empty(),
        "HEAD publication requires a whole-tree checkout"
    );
    let initializing = options.mode == CheckoutMode::Initialize;
    ensure!(
        !initializing
            || (options.publication.is_none() && options.paths.is_empty() && options.update_index),
        "Initialize only materializes the current complete checkout"
    );
    let tree = target.peel_to_tree()?;
    let (before_entries, target_entries, baseline, all) =
        candidate_paths(repo, Some(&tree), Some(&options.paths))?;
    let regular = regular_paths(&all, &before_entries, &target_entries, &baseline);
    let conversion = conversion_paths(repo, Some(tree.id()), &regular)?;
    let encoded = &conversion.encoded;
    let filters = FilterDrivers::capture(repo)?;
    if encoded.is_empty() {
        if initializing {
            return Ok(CheckoutReceipt {
                expected: None,
                native: None,
            });
        }
        for path in &regular {
            if options.mode == CheckoutMode::Force
                || baseline.get(path) != target_entries.get(path)
                || conversion.current_filters.get(path) != conversion.target_filters.get(path)
            {
                conversion.validate_filters(&filters, path)?;
            }
        }
        conversion.validate(repo)?;
        ensure!(
            filters == FilterDrivers::capture(repo)?,
            "Checkout filter configuration changed"
        );
        let receipt = NativeReceipt {
            git_dir: repo.path().canonicalize()?,
            publication: options.publication.clone(),
            paths: options.paths.clone(),
        };
        native_with_index_reservation(repo, target, &options, control)?;
        return Ok(CheckoutReceipt {
            expected: None,
            native: Some(receipt),
        });
    }
    let (mut index, _) = indexed(repo)?;
    ensure!(
        !index.has_conflicts(),
        "Resolve index conflicts before encoded checkout"
    );
    if initializing {
        ensure!(
            baseline == target_entries && before_entries == target_entries,
            "Initial checkout HEAD/index no longer matches the target tree"
        );
    }
    preserve_untracked_rules(repo, &target_entries, &before_entries, encoded)?;
    let current = Snapshot::capture(repo, encoded, AttributeLookup::WorktreeThenIndex)?;
    let desired = Snapshot::capture(repo, encoded, AttributeLookup::TargetTree(tree.id()))?;
    let mut affected = BTreeSet::new();
    for path in &all {
        let before = before_entries.get(path);
        let target = target_entries.get(path);
        let head = baseline.get(path);
        if [before, target, head]
            .into_iter()
            .flatten()
            .any(|(_, mode)| *mode == 0o160000)
        {
            ensure!(
                before == target && head == target,
                "Encoded checkout with changed gitlinks requires a separate submodule step"
            );
            continue;
        }
        let old_lfs = conversion.current_lfs.get(path);
        let new_lfs = conversion.target_lfs.get(path);
        let has_lfs = old_lfs.is_some() || new_lfs.is_some();
        if initializing {
            if !encoded.contains(path) {
                continue;
            }
            ensure!(
                !has_lfs,
                "Initial checkout cannot combine encoding with LFS: {}",
                path.display()
            );
            conversion.validate_filters(&filters, path)?;
            affected.insert(path.clone());
            continue;
        }
        let same_rules = old_lfs == new_lfs
            && conversion.current_filters.get(path) == conversion.target_filters.get(path)
            && (!encoded.contains(path) || current.same_conversion_rules(&desired, path)?);
        // libgit2 uses HEAD as its baseline. A staged/unstaged edit on a
        // path unchanged between branches is carry, not a checkout write.
        if options.mode == CheckoutMode::Safe && head == target && same_rules {
            continue;
        }
        conversion.validate_filters(&filters, path)?;
        if has_lfs {
            // A force checkout can also exclude an already-matching pointer
            // or payload without consulting the mutable LFS object cache.
            if options.mode == CheckoutMode::Force
                && before == target
                && target.is_some()
                && same_rules
                && !encoded.contains(path)
                && super::lfs::matches_index(repo, path)?
            {
                continue;
            }
            ensure!(
                !encoded.contains(path),
                "Checkout cannot combine encoding with LFS on the same path: {}",
                path.display()
            );
        }
        if options.mode == CheckoutMode::Safe && (encoded.contains(path) || has_lfs) {
            ensure!(
                before == head || before == target,
                "Encoded checkout would overwrite a staged change: {}",
                path.display()
            );
        }
        affected.insert(path.clone());
    }
    let lfs_affected: BTreeSet<_> = affected
        .iter()
        .filter(|path| {
            conversion.current_lfs.contains_key(*path) || conversion.target_lfs.contains_key(*path)
        })
        .cloned()
        .collect();
    let lfs_policy = (!lfs_affected.is_empty())
        .then(|| super::lfs::conversion_fingerprint(repo))
        .transpose()?;
    let lfs_current = Snapshot::capture(repo, &lfs_affected, AttributeLookup::WorktreeThenIndex)?;
    let lfs_target =
        Snapshot::capture(repo, &lfs_affected, AttributeLookup::TargetTree(tree.id()))?;
    for path in &lfs_affected {
        ensure!(
            (!lfs_current.has_lfs(path) || lfs_current.plain_lfs(path)?)
                && (!lfs_target.has_lfs(path) || lfs_target.plain_lfs(path)?),
            "Checkout cannot combine LFS with another worktree conversion: {}",
            path.display()
        );
    }
    let converted: BTreeSet<_> = encoded.union(&lfs_affected).cloned().collect();
    let root = repo.workdir().context("No worktree")?.canonicalize()?;
    verify_preserved_rules(
        repo,
        &root,
        &target_entries,
        &before_entries,
        &affected,
        &converted,
    )?;
    let common = repo.commondir().canonicalize()?;
    let git_dir = repo.path().canonicalize()?;
    let track_orig_head = orig_head.is_some();
    let refs_before = references(repo, track_orig_head)?;
    let mut refs_after =
        expected_refs(repo, &refs_before, options.publication.as_ref(), tree.id())?;
    if let Some(original) = orig_head {
        ensure!(
            head_id(&refs_before) == Some(original.to_string().as_str()),
            "Declared ORIG_HEAD does not match current HEAD"
        );
        refs_after.insert("ORIG_HEAD".into(), Reference::Direct(original.to_string()));
    }
    let index_before_bytes = read_index(repo)?;
    let index_before = index_before_bytes.as_deref().map(data);
    let configuration = config(repo)?;
    let base = repo.commondir().join(RECOVERY);
    no_symlink(&base)?;
    fs::create_dir_all(&base)?;
    let directory = tempfile::Builder::new()
        .prefix("checkout-")
        .tempdir_in(&base)?;
    let dir = directory.path();
    let initial_preview = if initializing {
        for path in &affected {
            let (id, _) = target_entries
                .get(path)
                .context("Missing initial checkout target")?;
            ensure!(
                repo.odb()?.read_header(*id)?.0 <= Limits::ENCODED.max_bytes,
                "Encoded index blob exceeds conversion limit"
            );
        }
        let preview = tempfile::tempdir_in(dir)?;
        let preview_options = CheckoutOptions {
            mode: CheckoutMode::Force,
            paths: affected.iter().cloned().collect(),
            update_index: false,
            publication: None,
        };
        native_at(
            repo,
            target,
            &preview_options,
            Some(preview.path()),
            control,
        )?;
        Some(preview)
    } else {
        None
    };
    let mut records = vec![];
    let mut policies = vec![];
    let mut missing_lfs_objects = 0;
    for path in &affected {
        let before = version(&root, path)?;
        if !before_entries.contains_key(path) && target_entries.contains_key(path) {
            ensure!(
                before.kind == Kind::Missing,
                "Checkout would overwrite an untracked or ignored path: {}",
                path.display()
            );
        }
        let n = records.len();
        save_raw(&root, path, &before, &artifact(dir, n, "before"))?;
        let mut record = Record {
            path: path.clone(),
            before: before.clone(),
            current: before.clone(),
            normalized: None,
            native_expected: None,
            native_delete: false,
            target: None,
            target_mode: target_entries.get(path).map_or(0o100644, |(_, m)| *m),
            materialized: false,
        };
        if converted.contains(path) {
            let policy = query_attributes(repo, path, AttributeLookup::TargetTree(tree.id()), &[])?;
            policies.push(Policy {
                path: path.clone(),
                fingerprint: policy.fingerprint,
                working_fingerprint: None,
            });
        }
        if lfs_affected.contains(path) {
            if options.mode == CheckoutMode::Safe
                && let Some((id, mode)) = before_entries.get(path)
            {
                if lfs_current.has_lfs(path) && matches!(mode, 0o100644 | 0o100755) {
                    let deleting_missing =
                        before.kind == Kind::Missing && !target_entries.contains_key(path);
                    ensure!(
                        deleting_missing
                            || (before.kind == Kind::File
                                && super::lfs::matches_index(repo, path)?),
                        "LFS working file has local changes: {}",
                        path.display()
                    );
                    if !deleting_missing {
                        let canonical = repo.find_blob(*id)?;
                        if before.hash != digest(canonical.content()) {
                            record.normalized =
                                Some(store(&artifact(dir, n, "normalized"), canonical.content())?);
                        }
                    }
                } else if before_entries.get(path) == target_entries.get(path) {
                    // A filter-only transition does not make libgit2 check
                    // for an overwrite. Verify its actual source ourselves.
                    let canonical = repo.find_blob(*id)?;
                    let mode_matches = !repo.config()?.get_bool("core.filemode").unwrap_or(true)
                        || (before.mode & 0o111 != 0) == (*mode == 0o100755);
                    ensure!(
                        before.kind == Kind::File
                            && mode_matches
                            && before.hash == digest(canonical.content()),
                        "LFS checkout would overwrite a local edit: {}",
                        path.display()
                    );
                }
            }
            if lfs_target.has_lfs(path)
                && let Some((id, 0o100644 | 0o100755)) = target_entries.get(path)
            {
                let (size, _) = repo.odb()?.read_header(*id)?;
                ensure!(
                    size <= 1024,
                    "LFS target is not a supported canonical pointer: {}",
                    path.display()
                );
                let canonical = repo.find_blob(*id)?;
                let destination = artifact(dir, n, "target");
                record.target = Some(
                    match super::lfs::prepare_checkout_payload(
                        repo,
                        path,
                        canonical.content(),
                        dir,
                    )? {
                        Some(prepared) => {
                            prepared.as_file().sync_all()?;
                            prepared.persist(&destination)?;
                            io_data(&destination)?
                        }
                        None => {
                            missing_lfs_objects += 1;
                            store(&destination, canonical.content())?
                        }
                    },
                );
            }
        }
        if encoded.contains(path) {
            if let Some((id, mode @ (0o100644 | 0o100755))) = before_entries.get(path) {
                let canonical = encoded_blob(repo, *id)?;
                let equivalent = if before.kind == Kind::File
                    && before.size <= Limits::ENCODED.max_bytes as u64
                {
                    let bytes = fs::read(safe(&root, path)?)?;
                    current
                        .clean(
                            repo,
                            path,
                            &bytes,
                            canonical.content(),
                            CleanPolicy::PatchApply,
                            Limits::ENCODED,
                        )
                        .map(|v| v == canonical.content())
                        .unwrap_or(false)
                } else {
                    false
                };
                let mode_matches = if repo.config()?.get_bool("core.filemode").unwrap_or(true) {
                    (before.mode & 0o111 != 0) == (*mode == 0o100755)
                } else {
                    true
                };
                ensure!(
                    initializing
                        || options.mode == CheckoutMode::Force
                        || (equivalent && mode_matches)
                        || (before.kind == Kind::Missing && !target_entries.contains_key(path)),
                    "Encoded working file has local changes or invalid bytes: {}",
                    path.display()
                );
                if options.mode == CheckoutMode::Safe
                    && equivalent
                    && mode_matches
                    && before.hash != digest(canonical.content())
                {
                    record.normalized =
                        Some(store(&artifact(dir, n, "normalized"), canonical.content())?);
                }
            }
            if let Some((id, mode @ (0o100644 | 0o100755))) = target_entries.get(path) {
                let canonical = encoded_blob(repo, *id)?;
                let bytes =
                    desired.smudge(repo, path, canonical.content(), *id, Limits::ENCODED)?;
                if let Some(preview) = &initial_preview {
                    let native = version(preview.path(), path)?;
                    let mode_matches = !repo.config()?.get_bool("core.filemode").unwrap_or(true)
                        || (before.mode & 0o111 != 0) == (*mode == 0o100755);
                    ensure!(
                        before.kind == Kind::File
                            && mode_matches
                            && (before.hash == native.hash
                                || before.hash == digest(canonical.content())
                                || before.hash == digest(&bytes)),
                        "Initial checkout file has local changes: {}",
                        path.display()
                    );
                }
                record.target = Some(store(&artifact(dir, n, "target"), &bytes)?);
            }
        }
        records.push(record);
    }
    drop(initial_preview);
    let mut state = State {
        version: 1,
        id: dir
            .file_name()
            .unwrap()
            .to_str()
            .context("Invalid session name")?
            .into(),
        owner_identity: directory_identity(&root)?,
        git_identity: directory_identity(&git_dir)?,
        common_identity: directory_identity(&common)?,
        owner: root,
        git_dir,
        common,
        tree: tree.id().to_string(),
        config: configuration,
        policy: policies,
        refs_before,
        refs_after,
        track_orig_head,
        index_before,
        index_after: None,
        refresh_index: options.update_index,
        refreshing_index: false,
        index_inflight: None,
        pins: BTreeMap::new(),
        lfs_policy,
        missing_lfs_objects,
        records,
        phase: Phase::NativePending,
        inflight: None,
    };
    if let Some(bytes) = index_before_bytes {
        store(&dir.join("index.before"), &bytes)?;
    }
    control.phase("Worktree conversion: prepared");
    conversion.validate(repo)?;
    ensure!(
        filters == FilterDrivers::capture(repo)?,
        "Checkout filter configuration changed"
    );
    current.validate(repo)?;
    desired.validate(repo)?;
    lfs_current.validate(repo)?;
    lfs_target.validate(repo)?;
    owner(repo, &state)?;
    ensure!(
        config(repo)? == state.config
            && references(repo, state.track_orig_head)? == state.refs_before
            && index_version(repo)? == state.index_before,
        "Repository identity/config/index/refs changed during preparation"
    );
    for record in &state.records {
        ensure!(
            version(&state.owner, &record.path)? == record.before,
            "Working file changed during preparation"
        );
    }
    state.pins = pins(repo, &state, &mut index, &tree)?;
    sync_directory(dir)?;
    sync_directory(&base)?;
    sync_directory(repo.commondir())?;
    let kept = directory.keep();
    save(repo, &state)?;
    let lock = IndexReservation::acquire(repo, &state.id)?;
    ensure!(
        index_version(repo)? == state.index_before
            && references(repo, state.track_orig_head)? == state.refs_before,
        "Index/refs changed before native checkout"
    );
    for n in 0..state.records.len() {
        if initializing {
            break;
        }
        let record = &state.records[n];
        if options.mode == CheckoutMode::Force
            && record.current.kind == Kind::Symlink
            && matches!(
                target_entries.get(&record.path),
                Some((_, 0o100644 | 0o100755))
            )
        {
            state.inflight = Some(n);
            save(repo, &state)?;
            let record = &mut state.records[n];
            ensure!(
                version(&state.owner, &record.path)? == record.current,
                "Working symlink changed before checkout: {}",
                record.path.display()
            );
            let destination = safe(&state.owner, &record.path)?;
            fs::remove_file(&destination)?;
            sync_directory(
                destination
                    .parent()
                    .context("Missing checkout path parent")?,
            )?;
            record.current = version(&state.owner, &record.path)?;
            state.inflight = None;
            save(repo, &state)?;
            continue;
        }
        let mut replacement = record.normalized.clone().map(|bytes| ("normalized", bytes));
        if replacement.is_none()
            && record.current.kind == Kind::File
            && must_detach(&safe(&state.owner, &record.path)?)?
        {
            replacement = Some((
                "before",
                Data {
                    hash: record.before.hash.clone(),
                    size: record.before.size,
                },
            ));
        }
        if let Some((suffix, bytes)) = replacement {
            state.inflight = Some(n);
            save(repo, &state)?;
            let next = write_raw(
                &state.owner,
                &state.records[n].path,
                &state.records[n].current,
                &artifact(&kept, n, suffix),
                &bytes,
                state.records[n].target_mode,
                false,
            )?;
            state.records[n].current = next;
            state.inflight = None;
            save(repo, &state)?;
        }
    }
    current.validate(repo)?;
    desired.validate(repo)?;
    lfs_current.validate(repo)?;
    lfs_target.validate(repo)?;
    control.phase("Worktree conversion: native pending");
    control.check()?;
    conversion.validate(repo)?;
    ensure!(
        filters == FilterDrivers::capture(repo)?,
        "Checkout filter configuration changed"
    );
    for record in &state.records {
        ensure!(
            version(&state.owner, &record.path)? == record.current,
            "Working file changed before native checkout: {}",
            record.path.display()
        );
    }
    let mut native_index = read_index(repo)?;
    let mut verified_native = BTreeMap::new();
    if !affected.is_empty() && !initializing {
        let preview_paths: Vec<_> = affected
            .iter()
            .filter(|path| target_entries.contains_key(*path))
            .cloned()
            .collect();
        let before_preview =
            native_preview(repo, tree.id(), preview_paths.clone(), &kept, control)?;
        for record in &mut state.records {
            record.native_expected = before_preview.get(&record.path).cloned();
            record.native_delete = !target_entries.contains_key(&record.path);
        }
        save(repo, &state)?;
        // Keep the real index.lock for the whole write. libgit2's own index
        // writes go to a private index with the same preimage and extensions.
        let scratch_path = kept.join("index.native");
        if let Some(bytes) = &native_index {
            atomic(&scratch_path, bytes)?;
        }
        let native_repo = Raw::open(repo.path())?;
        let mut scratch = git2::Index::open(&scratch_path)?;
        native_repo.set_index(&mut scratch)?;
        let target = native_repo.find_object(tree.id(), None)?;
        let mut adjusted = options.clone();
        adjusted.paths = affected.iter().cloned().collect();
        lock.verify()?;
        ensure!(
            index_version(repo)? == state.index_before
                && references(repo, state.track_orig_head)? == state.refs_before
                && config(repo)? == state.config,
            "Repository changed before native checkout"
        );
        conversion.validate(repo)?;
        for record in &state.records {
            ensure!(
                version(&state.owner, &record.path)? == record.current,
                "Working file changed before native checkout: {}",
                record.path.display()
            );
        }
        native(&native_repo, &target, &adjusted, control)?;
        control.phase("Worktree conversion: verifying native output");
        verify_preserved_rules(
            repo,
            &state.owner,
            &target_entries,
            &before_entries,
            &affected,
            &converted,
        )?;
        // libgit2 may resolve attributes before or after writing the target
        // .gitattributes. Compute both bounded path images independently of
        // the bytes in the real worktree before adopting the native output.
        let after_preview = native_preview(repo, tree.id(), preview_paths, &kept, control)?;
        let respect_filemode = repo.config()?.get_bool("core.filemode").unwrap_or(true);
        for record in &state.records {
            let observed = version(&state.owner, &record.path)?;
            let matches_preview = [
                before_preview.get(&record.path),
                after_preview.get(&record.path),
            ]
            .into_iter()
            .flatten()
            .any(|pinned| matches_native(&observed, pinned, respect_filemode));
            let known = matches_preview
                || (!target_entries.contains_key(&record.path) && observed.kind == Kind::Missing)
                || (baseline.get(&record.path) == target_entries.get(&record.path)
                    && observed == record.current);
            ensure!(
                known,
                "Native checkout did not produce the expected bytes; newer content was kept: {}",
                record.path.display()
            );
            verified_native.insert(record.path.clone(), observed);
        }
        if options.update_index {
            native_index = Some(fs::read(&scratch_path)?);
        }
    }
    ensure!(
        references(repo, state.track_orig_head)? == state.refs_before
            && config(repo)? == state.config,
        "Refs or configuration changed during native checkout"
    );
    desired.validate(repo)?;
    lfs_target.validate(repo)?;
    let mut native_directories = BTreeSet::new();
    for record in &mut state.records {
        if let Some(observed) = verified_native.remove(&record.path) {
            record.current = observed;
        }
        ensure!(
            version(&state.owner, &record.path)? == record.current,
            "Working file changed after native checkout: {}",
            record.path.display()
        );
        sync_native_output(&state.owner, record, &mut native_directories)?;
    }
    for dir in native_directories.iter().rev() {
        sync_directory(dir)?;
    }
    ensure!(
        index_version(repo)? == state.index_before,
        "Index changed during native checkout"
    );
    state.index_after = native_index.as_deref().map(data);
    if let Some(bytes) = native_index {
        store(&kept.join("index.after"), &bytes)?;
        if state.index_before != state.index_after {
            lock.verify()?;
            save(repo, &state)?;
            atomic(&repo.path().join("index"), &bytes)?;
        }
    }
    for policy in &mut state.policy {
        policy.working_fingerprint = Some(
            query_attributes(repo, &policy.path, AttributeLookup::WorktreeThenIndex, &[])?
                .fingerprint,
        );
    }
    state.phase = Phase::NativeComplete;
    save(repo, &state)?;
    control.phase("Worktree conversion: native completed");
    materialize(repo, &mut state, &lock, control)?;
    lock.verify()?;
    Ok(CheckoutReceipt {
        expected: Some(load(repo)?.1),
        native: None,
    })
}
/// Finish the libgit2 checkout of a newly created worktree without writing
/// any other working path or publishing refs. The caller owns its WriterLock.
pub(in crate::git) fn initialize_worktree(repo: &Raw, control: &NetworkControl) -> Result<()> {
    let commit = repo.head()?.peel_to_commit()?;
    let receipt = checkout_tree(
        repo,
        commit.as_object(),
        CheckoutOptions {
            mode: CheckoutMode::Initialize,
            ..Default::default()
        },
        control,
    )?;
    acknowledge_checkout(repo, &receipt).map(|_| ())
}
fn verify_current(repo: &Raw, state: &State) -> Result<()> {
    owner(repo, state)?;
    verify_refs(repo, state)?;
    ensure!(
        index_version(repo)? == state.index_after,
        "Checkout index changed; newer index was kept"
    );
    for record in &state.records {
        ensure!(
            version(&state.owner, &record.path)? == record.current,
            "{} changed after checkout; newer content was kept",
            record.path.display()
        );
    }
    Ok(())
}
fn verify_policy(repo: &Raw, state: &State) -> Result<()> {
    ensure!(
        config(repo)? == state.config,
        "Configuration changed; abort or restore the reviewed configuration"
    );
    if let Some(expected) = state.lfs_policy {
        ensure!(
            super::lfs::conversion_fingerprint(repo)? == expected,
            "LFS checkout configuration changed; prepared output was not reinterpreted"
        );
    }
    for policy in &state.policy {
        let observed = query_attributes(
            repo,
            &policy.path,
            AttributeLookup::TargetTree(Oid::from_str(&state.tree)?),
            &[],
        )?;
        ensure!(
            observed.fingerprint == policy.fingerprint,
            "Attribute source bytes changed; prepared output was not reinterpreted"
        );
        if let Some(expected) = policy.working_fingerprint {
            ensure!(
                query_attributes(repo, &policy.path, AttributeLookup::WorktreeThenIndex, &[])?
                    .fingerprint
                    == expected,
                "Working attributes changed; prepared output was not reinterpreted"
            );
        }
    }
    Ok(())
}
#[cfg(unix)]
fn set_raw_stat(entry: &mut git2::IndexEntry, metadata: &fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    entry.ctime = git2::IndexTime::new(metadata.ctime() as i32, metadata.ctime_nsec() as u32);
    entry.mtime = git2::IndexTime::new(metadata.mtime() as i32, metadata.mtime_nsec() as u32);
    entry.dev = metadata.dev() as u32;
    entry.ino = metadata.ino() as u32;
    entry.uid = metadata.uid();
    entry.gid = metadata.gid();
    entry.file_size = metadata.len() as u32;
}
#[cfg(not(unix))]
fn set_raw_stat(entry: &mut git2::IndexEntry, _metadata: &fs::Metadata) {
    // Without Unix stat fields, force the client to recheck canonical content.
    entry.ctime = git2::IndexTime::new(0, 0);
    entry.mtime = git2::IndexTime::new(0, 0);
    entry.dev = 0;
    entry.ino = 0;
    entry.uid = 0;
    entry.gid = 0;
    entry.file_size = 0;
}
fn refresh_materialized_index(
    repo: &Raw,
    state: &mut State,
    lock: &IndexReservation,
) -> Result<()> {
    if !state.refresh_index || !state.records.iter().any(|record| record.materialized) {
        return Ok(());
    }
    let scratch = session(repo, state)?.join("index.materialized");
    let bytes = read_index(repo)?.context("Encoded checkout did not produce an index")?;
    ensure!(
        Some(data(&bytes)) == state.index_after,
        "Index changed before stat refresh"
    );
    atomic(&scratch, &bytes)?;
    let mut index = git2::Index::open(&scratch)?;
    let target = repo.find_tree(Oid::from_str(&state.tree)?)?;
    for record in state.records.iter().filter(|record| record.materialized) {
        let mut entry = index
            .get_path(&record.path, 0)
            .context("Encoded target is missing in the index")?;
        let desired = target.get_path(&record.path)?;
        ensure!(
            entry.id == desired.id() && entry.mode == desired.filemode() as u32,
            "Encoded target differs from the canonical index: {}",
            record.path.display()
        );
        let path = safe(&state.owner, &record.path)?;
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file()
                && stamp(&metadata) == record.current.stamp
                && metadata.len() == record.current.size,
            "Encoded file changed before index stat refresh: {}",
            record.path.display()
        );
        // The index retains the canonical object and file mode. Its cached
        // stat data describes the actual, possibly larger working-tree file.
        set_raw_stat(&mut entry, &metadata);
        index.add(&entry)?;
    }
    index.write()?;
    let result = fs::read(scratch)?;
    if result != bytes {
        verify_current(repo, state)?;
        lock.verify()?;
        state.refreshing_index = true;
        state.index_inflight = Some(data(&result));
        save(repo, state)?;
        atomic(&repo.path().join("index"), &result)?;
        state.index_after = Some(data(&result));
        state.refreshing_index = false;
        state.index_inflight = None;
    }
    Ok(())
}
fn materialize(
    repo: &Raw,
    state: &mut State,
    lock: &IndexReservation,
    control: &NetworkControl,
) -> Result<()> {
    ensure!(
        state.phase == Phase::NativeComplete && state.inflight.is_none() && !state.refreshing_index,
        "Unverified partial checkout requires Abort"
    );
    verify_current(repo, state)?;
    verify_policy(repo, state)?;
    let dir = session(repo, state)?;
    for n in 0..state.records.len() {
        if state.records[n].materialized {
            continue;
        }
        if let Some(bytes) = state.records[n].target.clone() {
            control.check()?;
            ensure!(
                state.records[n].current.kind == Kind::File,
                "Native checkout did not materialize an encoded target"
            );
            state.inflight = Some(n);
            save(repo, state)?;
            let next = write_raw(
                &state.owner,
                &state.records[n].path,
                &state.records[n].current,
                &artifact(&dir, n, "target"),
                &bytes,
                state.records[n].target_mode,
                false,
            )?;
            state.records[n].current = next;
            state.records[n].materialized = true;
            state.inflight = None;
            save(repo, state)?;
        }
    }
    verify_current(repo, state)?;
    refresh_materialized_index(repo, state, lock)?;
    state.phase = Phase::Ready;
    save(repo, state)?;
    Ok(())
}
/// Validate the declared publication, not merely whatever refs happen to be
/// present. Ready stays durable until this acknowledgement succeeds.
pub(in crate::git) fn acknowledge_checkout(repo: &Raw, receipt: &CheckoutReceipt) -> Result<usize> {
    let Some(expected) = &receipt.expected else {
        let Some(native) = &receipt.native else {
            return Ok(0);
        };
        ensure!(
            repo.path().canonicalize()? == native.git_dir && !pending(repo),
            "Native checkout receipt belongs to another or interrupted worktree"
        );
        if let Some(publication) = &native.publication {
            let head = repo.find_reference("HEAD")?;
            ensure!(
                head.symbolic_target()? == publication.reference.as_deref()
                    && head.resolve()?.target() == Some(publication.id),
                "Publish the declared checkout HEAD before hydrating LFS files"
            );
        }
        return super::lfs::hydrate_paths(&Raw::open(repo.path())?, &native.paths)
            .context("Checkout completed, but LFS hydration failed; inspect the worktree and retry LFS Checkout");
    };
    let (state, actual) = load(repo)?;
    ensure!(
        &actual == expected,
        "Checkout receipt changed; reopen recovery"
    );
    ensure!(
        state.phase == Phase::Ready && state.inflight.is_none(),
        "Checkout is not ready for publication acknowledgement"
    );
    verify_current(repo, &state)?;
    verify_policy(repo, &state)?;
    ensure!(
        references(repo, state.track_orig_head)? == state.refs_after,
        "Publish the declared checkout HEAD/refs before acknowledging"
    );
    sync_publication(repo, &state.refs_before, &state.refs_after)?;
    verify_current(repo, &state)?;
    ensure!(
        references(repo, state.track_orig_head)? == state.refs_after,
        "Checkout publication changed during acknowledgement"
    );
    finish_checkout(repo, &state)
        .context("Checkout completed, but its recovery data could not be cleaned up")?;
    Ok(state.missing_lfs_objects)
}
pub(in crate::git) fn continue_checkout(repo: &Raw, expected: &str) -> Result<String> {
    if !pending(repo) {
        return Ok("No encoded checkout needs recovery".into());
    }
    let (mut state, actual) = load(repo)?;
    ensure!(actual == expected, "Checkout recovery changed; reopen it");
    owner(repo, &state)?;
    let lock = IndexReservation::acquire(repo, &state.id)?;
    if state.phase == Phase::NativeComplete && state.inflight.is_none() && !state.refreshing_index {
        materialize(repo, &mut state, &lock, &NetworkControl::default())?;
    }
    ensure!(
        state.phase == Phase::Ready && state.inflight.is_none(),
        "Unverified partial checkout cannot Continue; use Abort"
    );
    verify_current(repo, &state)?;
    verify_policy(repo, &state)?;
    let current = references(repo, state.track_orig_head)?;
    if current != state.refs_after {
        ensure!(
            refs_allowed(&current, &state.refs_before, &state.refs_after),
            "Unknown ref changes; publication refused"
        );
        if let Some(Reference::Symbolic(name)) = state.refs_after.get("HEAD") {
            super::workspaces::branch_available(repo, name)?;
        }
        apply_refs(repo, &current, &state.refs_after, state.track_orig_head)?;
    }
    lock.verify()?;
    acknowledge_checkout(
        repo,
        &CheckoutReceipt {
            expected: Some(load(repo)?.1),
            native: None,
        },
    )?;
    Ok(if state.missing_lfs_objects == 0 {
        "Encoded checkout completed and declared refs published".into()
    } else {
        format!(
            "Encoded checkout completed and declared refs published; {} LFS objects missing locally (use LFS Fetch)",
            state.missing_lfs_objects
        )
    })
}
fn restore_record(
    root: &Path,
    path: &Path,
    expected: &Version,
    original: &Version,
    source: &Path,
) -> Result<Version> {
    let destination = safe(root, path)?;
    match original.kind {
        Kind::Missing => {
            ensure!(
                version(root, path)? == *expected,
                "Newer file was kept during Abort"
            );
            if expected.kind != Kind::Missing {
                fs::remove_file(&destination)?;
                sync_directory(destination.parent().unwrap())?;
            }
        }
        Kind::File => {
            let desired = Data {
                hash: original.hash.clone(),
                size: original.size,
            };
            let _ = write_raw(root, path, expected, source, &desired, original.mode, true)?;
        }
        Kind::Symlink => {
            let bytes = fs::read(source)?;
            ensure!(
                data(&bytes)
                    == Data {
                        hash: original.hash.clone(),
                        size: original.size
                    },
                "Original symlink receipt changed"
            );
            let parent = destination.parent().context("No symlink parent")?;
            fs::create_dir_all(parent)?;
            let temp = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
            fs::remove_file(&temp)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(super::path_from_bytes(&bytes), &temp)?;
            #[cfg(not(unix))]
            bail!("Symlink recovery is unavailable on this platform");
            ensure!(
                version(root, path)? == *expected,
                "Newer symlink was kept during Abort"
            );
            fs::rename(&temp, &destination)?;
            sync_directory(parent)?;
        }
    }
    version(root, path)
}
pub(in crate::git) fn abort_checkout(
    repo: &Raw,
    expected: &str,
    control: &NetworkControl,
) -> Result<String> {
    if !pending(repo) {
        return Ok("No encoded checkout needs recovery".into());
    }
    let (mut state, actual) = load(repo)?;
    ensure!(actual == expected, "Checkout recovery changed; reopen it");
    owner(repo, &state)?;
    verify_refs(repo, &state)?;
    let lock = IndexReservation::acquire(repo, &state.id)?;
    let dir = session(repo, &state)?;
    for (n, record) in state.records.iter().enumerate() {
        if record.before.kind != Kind::Missing {
            ensure!(
                io_data(&artifact(&dir, n, "before"))?
                    == Data {
                        hash: record.before.hash.clone(),
                        size: record.before.size
                    },
                "Original raw recovery bytes changed"
            );
        }
    }
    let original_index = match &state.index_before {
        Some(expected) => {
            let bytes = fs::read(dir.join("index.before"))?;
            ensure!(data(&bytes) == *expected, "Original recovery index changed");
            Some(bytes)
        }
        None => None,
    };
    let partial_index = read_index(repo)?;
    let observed_index = partial_index.as_deref().map(data);
    ensure!(
        observed_index == state.index_before
            || observed_index.as_ref().is_some_and(|observed| {
                state.index_after.as_ref() == Some(observed)
                    || state.index_inflight.as_ref() == Some(observed)
            }),
        "The index changed outside checkout; the newer index and recovery data were kept"
    );
    let mut partial = Vec::with_capacity(state.records.len());
    for (n, record) in state.records.iter().enumerate() {
        let observed = version(&state.owner, &record.path)?;
        let matches_payload = |payload: &Data| {
            observed.kind == Kind::File
                && observed.hash == payload.hash
                && observed.size == payload.size
                && observed.mode == record.current.mode
        };
        let inflight_known = state.inflight == Some(n)
            && match state.phase {
                Phase::NativePending => {
                    record.normalized.as_ref().is_some_and(matches_payload)
                        || (record.current.kind == Kind::Symlink
                            && observed.kind == Kind::Missing
                            && matches!(record.target_mode, 0o100644 | 0o100755))
                }
                Phase::NativeComplete => record.target.as_ref().is_some_and(matches_payload),
                _ => false,
            };
        let pinned_native = state.phase == Phase::NativePending
            && (record
                .native_expected
                .as_ref()
                .is_some_and(|pinned| matches_native(&observed, pinned, true))
                || (record.native_delete && observed.kind == Kind::Missing));
        ensure!(
            observed.same_content(&record.before)
                || observed.same_content(&record.current)
                || inflight_known
                || pinned_native,
            "{} changed outside checkout; newer content and recovery data were kept",
            record.path.display()
        );
        partial.push(observed);
    }
    let backup = tempfile::Builder::new()
        .prefix("aborted-")
        .tempdir_in(&dir)?;
    for (n, record) in state.records.iter().enumerate() {
        save_raw(
            &state.owner,
            &record.path,
            &partial[n],
            &artifact(backup.path(), n, "raw"),
        )?;
    }
    if let Some(bytes) = &partial_index {
        store(&backup.path().join("index"), bytes)?;
    }
    store(
        &backup.path().join("journal.json"),
        &fs::read(repo.commondir().join(JOURNAL))?,
    )?;
    store(
        &backup.path().join("refs.json"),
        &serde_json::to_vec(&references(repo, state.track_orig_head)?)?,
    )?;
    sync_directory(backup.path())?;
    sync_directory(&dir)?;
    let retained = backup.keep();
    state.phase = Phase::Aborting;
    state.inflight = None;
    for (record, observed) in state.records.iter_mut().zip(&partial) {
        record.current = observed.clone();
    }
    save(repo, &state)?;
    for (n, record) in state.records.iter().enumerate() {
        control.phase(&format!(
            "Worktree conversion: restoring {}",
            record.path.display()
        ));
        if !partial[n].same_content(&record.before) {
            let _ = restore_record(
                &state.owner,
                &record.path,
                &partial[n],
                &record.before,
                &artifact(&dir, n, "before"),
            )?;
        }
    }
    lock.verify()?;
    ensure!(
        read_index(repo)? == partial_index,
        "Index changed after Abort preserved it; newer index was kept"
    );
    match original_index {
        Some(bytes) => atomic(&repo.path().join("index"), &bytes)?,
        None => {
            if repo.path().join("index").exists() {
                fs::remove_file(repo.path().join("index"))?;
                sync_directory(repo.path())?;
            }
        }
    }
    for record in &state.records {
        ensure!(
            version(&state.owner, &record.path)?.same_content(&record.before),
            "Raw restoration is incomplete; journal retained"
        );
    }
    let now = references(repo, state.track_orig_head)?;
    ensure!(
        refs_allowed(&now, &state.refs_before, &state.refs_after),
        "References changed during Abort"
    );
    apply_refs(repo, &now, &state.refs_before, state.track_orig_head)?;
    remove_journal(repo)?;
    Ok(format!(
        "Original raw files/index/refs restored; interrupted state retained at {}",
        retained.display()
    ))
}

impl Repository {
    pub fn checkout_worktree(
        &self,
        target: &str,
        options: CheckoutOptions,
        control: NetworkControl,
    ) -> Result<CheckoutReceipt> {
        let repo = raw(self)?;
        let _writer = WriterLock::acquire(&repo)?;
        ensure!(
            repo.state() == git2::RepositoryState::Clean
                && !super::integration::squash_active(&repo)
                && !super::rebase::active(&repo)
                && !super::patch_batch::recovery_pending(&repo)
                && !super::lfs_migration::pending(&repo),
            "Finish or recover the active repository operation before checking out the worktree"
        );
        let object = repo.revparse_single(target)?;
        control.normalize(checkout_tree(&repo, &object, options, &control))
    }
    pub fn acknowledge_worktree_checkout(&self, receipt: &CheckoutReceipt) -> Result<()> {
        let repo = raw(self)?;
        let _writer = WriterLock::acquire(&repo)?;
        acknowledge_checkout(&repo, receipt).map(|_| ())
    }
    pub fn worktree_recovery_status(&self) -> Result<Option<WorktreeRecoveryStatus>> {
        let repo = raw(self)?;
        if !pending(&repo) {
            return Ok(None);
        }
        let (state, expected) = load(&repo)?;
        Ok(Some(WorktreeRecoveryStatus {
            expected,
            owner: state.owner.clone(),
            phase: format!("{:?}", state.phase),
            original_head: describe_head(&state.refs_before),
            target_head: describe_head(&state.refs_after),
            can_continue: state.inflight.is_none()
                && !state.refreshing_index
                && matches!(state.phase, Phase::NativeComplete | Phase::Ready),
            recovery: session(&repo, &state)?,
        }))
    }
    pub fn continue_worktree_checkout(&self, expected: &str) -> Result<String> {
        let repo = raw(self)?;
        let _writer = WriterLock::acquire(&repo)?;
        continue_checkout(&repo, expected)
    }
    pub fn abort_worktree_checkout(&self, expected: &str) -> Result<String> {
        let repo = raw(self)?;
        let _writer = WriterLock::acquire(&repo)?;
        abort_checkout(&repo, expected, &NetworkControl::default())
    }
    pub fn guard_worktree_write(&self, target: Option<&str>, paths: &[PathBuf]) -> Result<()> {
        let repo = raw(self)?;
        let target = target
            .map(|spec| repo.revparse_single(spec).map(|o| o.id()))
            .transpose()?;
        guard_native_write(&repo, target, Some(paths))
    }
}
