//! Native attribute snapshots. All gix objects remain local to a query; callers
//! receive owned states and a fingerprint of the complete source observations.
use anyhow::{Context, Result, bail, ensure};
use gix_attributes::{
    Search,
    glob::pattern::Case,
    search::{MetadataCollection, Outcome},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::git) enum Lookup {
    WorktreeThenIndex,
    IndexThenWorktree,
    /// Immutable repository attributes from this commit/tree, combined with
    /// this repository's own system/global/info sources and case policy.
    /// Obsolete worktree/index attributes are deliberately not a fallback.
    TargetTree(git2::Oid),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::git) enum AttributeState {
    Unspecified,
    Set,
    Unset,
    Value(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::git) struct AttributeQuery {
    pub values: BTreeMap<String, AttributeState>,
    pub fingerprint: [u8; 32],
}

pub(super) fn is_lfs(repo: &git2::Repository, path: &Path, lookup: Lookup) -> Result<bool> {
    Ok(
        matches!(query(repo, path, lookup, &["filter"])?.values.get("filter"), Some(AttributeState::Value(value)) if value == b"lfs"),
    )
}

pub(in crate::git) fn query(
    repo: &git2::Repository,
    path: &Path,
    lookup: Lookup,
    selected: &[&str],
) -> Result<AttributeQuery> {
    let snapshot = Snapshot::capture(repo, path, lookup, selected)?;
    let values = snapshot.evaluate()?;
    snapshot.verify(repo)?;
    Ok(query_result(&snapshot, values))
}

fn query_result(snapshot: &Snapshot, values: BTreeMap<String, AttributeState>) -> AttributeQuery {
    let mut digest = Sha256::new();
    field(&mut digest, b"domain", b"gitbuddy.attribute-query.v1");
    field(&mut digest, b"sources", &snapshot.fingerprint);
    hash_values(&mut digest, &values);
    AttributeQuery {
        values,
        fingerprint: digest.finalize().into(),
    }
}

struct Buffer {
    bytes: Vec<u8>,
    source: PathBuf,
    root: Option<PathBuf>,
    macros: bool,
}

struct Snapshot {
    path: PathBuf,
    lookup: Lookup,
    selected: Vec<String>,
    case: Case,
    buffers: Vec<Buffer>,
    fingerprint: [u8; 32],
}

/// A local, read-only directory view. Values for siblings reuse the same
/// parsed search; callers verify once after assembling status. This is not a
/// writable conversion receipt and must not replace query() at mutation time.
pub(in crate::git) struct DirectoryView {
    snapshot: Snapshot,
    search: Search,
    metadata: MetadataCollection,
}
impl DirectoryView {
    pub(in crate::git) fn capture(
        repo: &git2::Repository,
        representative: &Path,
        lookup: Lookup,
        selected: &[&str],
    ) -> Result<Self> {
        let snapshot = Snapshot::capture(repo, representative, lookup, selected)?;
        let (search, metadata) = snapshot.search()?;
        snapshot.verify(repo)?;
        Ok(Self {
            snapshot,
            search,
            metadata,
        })
    }
    pub(in crate::git) fn values(&self, path: &Path) -> Result<BTreeMap<String, AttributeState>> {
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                && path.parent() == self.snapshot.path.parent(),
            "Attribute directory view only accepts sibling paths"
        );
        evaluate_search(
            path,
            self.snapshot.case,
            &self.snapshot.selected,
            &self.search,
            &self.metadata,
        )
    }
    pub(in crate::git) fn verify(&self, repo: &git2::Repository) -> Result<()> {
        self.snapshot.verify(repo)
    }

    /// Merge and replay may combine independent attribute sources, activating
    /// a declaration that matched no file in either input tree on its own.
    pub(in crate::git) fn declares_worktree_encoding(&self) -> bool {
        self.snapshot.buffers.iter().any(|buffer| {
            gix_attributes::parse(&buffer.bytes)
                .filter_map(Result::ok)
                .filter(|(kind, _, _)| {
                    buffer.macros || !matches!(kind, gix_attributes::parse::Kind::Macro(_))
                })
                .flat_map(|(_, assignments, _)| assignments.filter_map(Result::ok))
                .any(|assignment| {
                    assignment.name.as_str() == "working-tree-encoding"
                        && matches!(
                            assignment.state,
                            gix_attributes::StateRef::Set | gix_attributes::StateRef::Value(_)
                        )
                })
        })
    }
}

impl Snapshot {
    fn capture(
        repo: &git2::Repository,
        path: &Path,
        lookup: Lookup,
        selected: &[&str],
    ) -> Result<Self> {
        Self::capture_from(repo, repo, path, lookup, selected)
    }
    fn capture_from(
        repo: &git2::Repository,
        object_provider: &git2::Repository,
        path: &Path,
        lookup: Lookup,
        selected: &[&str],
    ) -> Result<Self> {
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "Invalid attribute path"
        );
        // Independent handles see same-length/mtime external config rewrites,
        // and never reload/reset a caller's pending in-memory index changes.
        let fresh = git2::Repository::open(repo.path())?;
        let mut config = fresh.config()?;
        let config = config.snapshot()?;
        let index = fresh.index()?;
        let objects = matches!(lookup, Lookup::TargetTree(_))
            .then(|| git2::Repository::open(object_provider.path()))
            .transpose()?;
        let target_tree = match lookup {
            Lookup::TargetTree(id) => Some(
                objects
                    .as_ref()
                    .context("Missing target object provider")?
                    .find_object(id, None)?
                    .peel_to_tree()?,
            ),
            _ => None,
        };
        let root = repo.workdir().context("Attributes need a working tree")?;
        let root_trace = Trace::read(root, true)?;
        let fresh_root = Trace::read(
            fresh.workdir().context("Attributes need a working tree")?,
            true,
        )?;
        ensure!(
            root_trace.available
                && fresh_root.available
                && root_trace.resolved == fresh_root.resolved,
            "Repository working tree changed while reading attributes; retry"
        );
        let canonical_root = &root_trace.resolved;
        let selected = selected
            .iter()
            .map(|name| (*name).to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut digest = Sha256::new();
        field(&mut digest, b"domain", b"gitbuddy.attribute-sources.v1");
        field(&mut digest, b"query-path", &path_bytes(path));
        field(
            &mut digest,
            b"lookup",
            match lookup {
                Lookup::WorktreeThenIndex => b"worktree-first",
                Lookup::IndexThenWorktree => b"index-first",
                Lookup::TargetTree(_) => b"target-tree",
            },
        );
        if let Some(tree) = &target_tree {
            field(&mut digest, b"target-tree-oid", tree.id().as_bytes());
            hash_trace(
                &mut digest,
                b"tree-object-provider",
                &Trace::read(object_provider.path(), true)?,
            );
        }
        for name in &selected {
            field(&mut digest, b"selected", name.as_bytes());
        }
        hash_trace(&mut digest, b"worktree-root", &root_trace);
        hash_trace(&mut digest, b"fresh-worktree-root", &fresh_root);
        hash_trace(&mut digest, b"git-dir", &Trace::read(repo.path(), true)?);
        hash_trace(
            &mut digest,
            b"common-dir",
            &Trace::read(fresh.commondir(), true)?,
        );
        field(
            &mut digest,
            b"index-path",
            &path_bytes(index.path().context("Attribute index has no path")?),
        );
        let environment = Environment::capture();
        environment.hash(&mut digest);
        hash_config(&mut digest, &config, "core.attributesfile")?;
        hash_config(&mut digest, &config, "core.ignorecase")?;
        let case = match config.get_bool("core.ignorecase") {
            Ok(true) => Case::Fold,
            Ok(false) => Case::Sensitive,
            Err(error) if error.code() == git2::ErrorCode::NotFound => Case::Sensitive,
            Err(error) => return Err(error.into()),
        };
        field(
            &mut digest,
            b"case",
            match case {
                Case::Fold => b"fold",
                Case::Sensitive => b"sensitive",
            },
        );
        let builtin = b"[attr]binary -diff -merge -text\n";
        field(&mut digest, b"builtin", builtin);
        let mut buffers = vec![Buffer {
            bytes: builtin.to_vec(),
            source: "[builtin]".into(),
            root: None,
            macros: true,
        }];

        let system_paths = native_system_paths()?;
        field(&mut digest, b"native-system-paths", system_paths.as_bytes());
        if no_system(environment.no_system.clone())? {
            field(&mut digest, b"system-selection", b"disabled");
        } else {
            field(&mut digest, b"system-selection", b"enabled");
            for source in split_system_paths(system_paths.as_bytes()) {
                if let Some(bytes) = read_source(&mut digest, b"system-candidate", &source, true)? {
                    field(&mut digest, b"selected-system", &path_bytes(&source));
                    buffers.push(Buffer {
                        bytes,
                        source,
                        root: None,
                        macros: true,
                    });
                    break;
                }
            }
        }
        match global_path(&config, root, &environment)? {
            Some(source) => {
                field(&mut digest, b"selected-global", &path_bytes(&source));
                if let Some(bytes) = read_source(&mut digest, b"global", &source, true)? {
                    buffers.push(Buffer {
                        bytes,
                        source,
                        root: None,
                        macros: true,
                    });
                }
            }
            None => field(&mut digest, b"selected-global", b"disabled-or-unavailable"),
        }

        for relative in std::iter::once(PathBuf::new()).chain(
            path.parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .filter(|parent| !parent.as_os_str().is_empty())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(Path::to_path_buf),
        ) {
            let attributes = relative.join(".gitattributes");
            field(
                &mut digest,
                b"directory-attributes",
                &path_bytes(&attributes),
            );
            let (choice, bytes) = if let Some(tree) = &target_tree {
                (
                    b"target-tree".as_slice(),
                    tree_source(
                        objects.as_ref().context("Missing target object provider")?,
                        tree,
                        &attributes,
                        &mut digest,
                    )?,
                )
            } else {
                // Observe both mutable candidates, including a shadowed index
                // entry, so later fallback cannot acquire unreviewed rules.
                let working = read_source(
                    &mut digest,
                    b"worktree",
                    &canonical_root.join(&attributes),
                    false,
                )?;
                let indexed = index_source(&fresh, &index, &attributes, &mut digest)?;
                match lookup {
                    Lookup::WorktreeThenIndex if working.is_some() => {
                        (b"worktree".as_slice(), working)
                    }
                    Lookup::WorktreeThenIndex => (b"index".as_slice(), indexed),
                    Lookup::IndexThenWorktree if indexed.is_some() => {
                        (b"index".as_slice(), indexed)
                    }
                    Lookup::IndexThenWorktree => (b"worktree".as_slice(), working),
                    Lookup::TargetTree(_) => unreachable!("target tree was resolved above"),
                }
            };
            field(&mut digest, b"fallback-choice", choice);
            field(
                &mut digest,
                b"chosen-available",
                &[u8::from(bytes.is_some())],
            );
            if let Some(bytes) = bytes {
                buffers.push(Buffer {
                    bytes,
                    source: canonical_root.join(attributes),
                    root: Some(canonical_root.to_path_buf()),
                    macros: relative.as_os_str().is_empty(),
                });
            }
        }
        let info = fresh.commondir().join("info/attributes");
        field(&mut digest, b"selected-info", &path_bytes(&info));
        if let Some(bytes) = read_source(&mut digest, b"info", &info, true)? {
            buffers.push(Buffer {
                bytes,
                source: info,
                root: None,
                macros: true,
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            lookup,
            selected,
            case,
            buffers,
            fingerprint: digest.finalize().into(),
        })
    }

    fn evaluate(&self) -> Result<BTreeMap<String, AttributeState>> {
        if self.selected.is_empty() {
            return Ok(BTreeMap::new());
        }
        let (search, metadata) = self.search()?;
        evaluate_search(&self.path, self.case, &self.selected, &search, &metadata)
    }
    fn search(&self) -> Result<(Search, MetadataCollection)> {
        let mut search = Search::default();
        let mut metadata = MetadataCollection::default();
        for buffer in &self.buffers {
            search.add_patterns_buffer(
                &buffer.bytes,
                buffer.source.clone(),
                buffer.root.as_deref(),
                &mut metadata,
                buffer.macros,
            )?;
        }
        Ok((search, metadata))
    }

    fn verify(&self, repo: &git2::Repository) -> Result<()> {
        self.verify_from(repo, repo)
    }
    fn verify_from(&self, repo: &git2::Repository, objects: &git2::Repository) -> Result<()> {
        let selected = self.selected.iter().map(String::as_str).collect::<Vec<_>>();
        let current = Self::capture_from(repo, objects, &self.path, self.lookup, &selected)?;
        ensure!(
            self.fingerprint == current.fingerprint,
            "Attribute sources changed while assembling a snapshot; retry"
        );
        Ok(())
    }
}

fn evaluate_search(
    path: &Path,
    case: Case,
    selected: &[String],
    search: &Search,
    metadata: &MetadataCollection,
) -> Result<BTreeMap<String, AttributeState>> {
    let mut values = BTreeMap::new();
    if selected.is_empty() {
        return Ok(values);
    }
    let mut outcome = Outcome::default();
    outcome.initialize_with_selection(metadata, selected);
    let bytes = path_bytes(path);
    search.pattern_matching_relative_path(bytes.as_slice().into(), case, Some(false), &mut outcome);
    for (name, found) in selected.iter().zip(outcome.iter_selected()) {
        let state = match found.assignment.state {
            gix_attributes::StateRef::Unspecified => AttributeState::Unspecified,
            gix_attributes::StateRef::Set => AttributeState::Set,
            gix_attributes::StateRef::Unset => AttributeState::Unset,
            gix_attributes::StateRef::Value(value) => {
                AttributeState::Value(value.as_bstr().to_vec())
            }
        };
        values.insert(name.clone(), state);
    }
    Ok(values)
}

#[derive(Clone)]
struct Environment {
    home: Option<OsString>,
    xdg: Option<OsString>,
    no_system: Option<OsString>,
}
impl Environment {
    fn capture() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            xdg: std::env::var_os("XDG_CONFIG_HOME"),
            no_system: std::env::var_os("GIT_ATTR_NOSYSTEM"),
        }
    }
    fn hash(&self, digest: &mut Sha256) {
        for (name, value) in [
            (b"HOME".as_slice(), &self.home),
            (b"XDG_CONFIG_HOME".as_slice(), &self.xdg),
            (b"GIT_ATTR_NOSYSTEM".as_slice(), &self.no_system),
        ] {
            field(digest, b"environment-name", name);
            field(digest, b"environment-present", &[u8::from(value.is_some())]);
            if let Some(value) = value {
                field(digest, b"environment-value", &path_bytes(Path::new(value)));
            }
        }
    }
}

fn global_path(
    config: &git2::Config,
    root: &Path,
    environment: &Environment,
) -> Result<Option<PathBuf>> {
    let path = match config.get_path("core.attributesfile") {
        Ok(path) if path.as_os_str().is_empty() => return Ok(None),
        Ok(path) => path,
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            if let Some(xdg) = environment.xdg.as_ref().filter(|value| !value.is_empty()) {
                PathBuf::from(xdg).join("git/attributes")
            } else if let Some(home) = environment.home.as_ref().filter(|value| !value.is_empty()) {
                PathBuf::from(home).join(".config/git/attributes")
            } else {
                return Ok(None);
            }
        }
        Err(error) => return Err(error.into()),
    };
    Ok(Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    }))
}

fn no_system(value: Option<OsString>) -> Result<bool> {
    let Some(value) = value else { return Ok(false) };
    let value = value
        .to_str()
        .context("GIT_ATTR_NOSYSTEM is not a boolean")?;
    match value.to_ascii_lowercase().as_str() {
        "" | "false" | "no" | "off" => Ok(false),
        "true" | "yes" | "on" => Ok(true),
        value => Ok(value
            .parse::<i64>()
            .context("GIT_ATTR_NOSYSTEM is not a boolean")?
            != 0),
    }
}

fn native_system_paths() -> Result<std::ffi::CString> {
    // SAFETY: repository initialization completed first; GET_SEARCH_PATH only
    // copies libgit2's initialized buffer. Production never changes the global
    // paths; setters are confined to isolated test processes before lookup.
    Ok(unsafe { git2::opts::get_search_path(git2::ConfigLevel::System) }?)
}
fn split_system_paths(bytes: &[u8]) -> Vec<PathBuf> {
    let separator = if cfg!(windows) { b';' } else { b':' };
    let mut start = 0;
    let mut paths = Vec::new();
    for end in (0..bytes.len())
        .filter(|index| bytes[*index] == separator && (*index == 0 || bytes[*index - 1] != b'\\'))
        .chain(std::iter::once(bytes.len()))
    {
        if end > start {
            paths.push(super::super::path_from_bytes(&bytes[start..end]).join("gitattributes"));
        }
        start = end + 1;
    }
    paths
}

fn tree_source(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    path: &Path,
    digest: &mut Sha256,
) -> Result<Option<Vec<u8>>> {
    let entry = match tree.get_path(path) {
        Ok(entry) => entry,
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            field(digest, b"tree-entry", b"missing");
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    field(digest, b"tree-entry", b"present");
    field(digest, b"tree-mode", &entry.filemode().to_le_bytes());
    field(digest, b"tree-oid", entry.id().as_bytes());
    if matches!(entry.filemode(), 0o100644 | 0o100755 | 0o120000) {
        let bytes = repo.find_blob(entry.id())?.content().to_vec();
        field(digest, b"tree-content", &bytes);
        Ok(Some(bytes))
    } else {
        Ok(None)
    }
}

fn index_source(
    repo: &git2::Repository,
    index: &git2::Index,
    path: &Path,
    digest: &mut Sha256,
) -> Result<Option<Vec<u8>>> {
    let mut selected = None;
    for stage in 0i32..=3 {
        field(digest, b"index-stage", &stage.to_le_bytes());
        match index.get_path(path, stage) {
            None => field(digest, b"index-entry", b"missing"),
            Some(entry) => {
                field(digest, b"index-entry", b"present");
                field(digest, b"index-mode", &entry.mode.to_le_bytes());
                field(digest, b"index-flags", &entry.flags.to_le_bytes());
                field(
                    digest,
                    b"index-flags-extended",
                    &entry.flags_extended.to_le_bytes(),
                );
                field(digest, b"index-oid", entry.id.as_bytes());
                // Indexed symlink bytes are rules, not a target to traverse.
                if matches!(entry.mode, 0o100644 | 0o100755 | 0o120000) {
                    let bytes = repo.find_blob(entry.id)?.content().to_vec();
                    field(digest, b"index-content", &bytes);
                    if stage == 0 {
                        selected = Some(bytes);
                    }
                }
            }
        }
    }
    Ok(selected)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    path: PathBuf,
    stamp: Vec<u8>,
    target: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Trace {
    requested: PathBuf,
    resolved: PathBuf,
    nodes: Vec<Node>,
    available: bool,
    missing: bool,
}

#[derive(Clone)]
enum Step {
    Prefix(OsString),
    Root,
    Parent,
    Normal(OsString),
}
fn steps(path: &Path) -> Vec<Step> {
    path.components()
        .filter_map(|component| match component {
            Component::Prefix(prefix) => Some(Step::Prefix(prefix.as_os_str().to_os_string())),
            Component::RootDir => Some(Step::Root),
            Component::ParentDir => Some(Step::Parent),
            Component::Normal(name) => Some(Step::Normal(name.to_os_string())),
            Component::CurDir => None,
        })
        .collect()
}

impl Trace {
    fn read(path: &Path, follow: bool) -> Result<Self> {
        let requested = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut pending = VecDeque::from(steps(&requested));
        let mut trace = Self {
            requested,
            resolved: PathBuf::new(),
            nodes: Vec::new(),
            available: true,
            missing: false,
        };
        let mut links = 0;
        while let Some(step) = pending.pop_front() {
            match step {
                Step::Prefix(prefix) => trace.resolved = prefix.into(),
                Step::Root => trace.resolved.push(std::path::MAIN_SEPARATOR_STR),
                Step::Parent => {
                    trace.resolved.pop();
                }
                Step::Normal(name) => {
                    trace.resolved.push(name);
                    let metadata = match fs::symlink_metadata(&trace.resolved) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            trace.nodes.push(Node {
                                path: trace.resolved.clone(),
                                stamp: b"missing".to_vec(),
                                target: None,
                            });
                            trace.available = false;
                            trace.missing = true;
                            // A patch may create safe parent directories while
                            // the controlling attribute leaf remains absent.
                            // Keep the full intended leaf rather than pinning
                            // whichever parent happened to be missing first.
                            for step in pending {
                                match step {
                                    Step::Prefix(prefix) => trace.resolved = prefix.into(),
                                    Step::Root => {
                                        trace.resolved.push(std::path::MAIN_SEPARATOR_STR)
                                    }
                                    Step::Parent => {
                                        trace.resolved.pop();
                                    }
                                    Step::Normal(name) => trace.resolved.push(name),
                                }
                            }
                            return Ok(trace);
                        }
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!(
                                    "Cannot inspect attribute source {}",
                                    trace.resolved.display()
                                )
                            });
                        }
                    };
                    let stamp = metadata_stamp(&metadata);
                    let target = if metadata.file_type().is_symlink() {
                        Some(fs::read_link(&trace.resolved)?)
                    } else {
                        None
                    };
                    trace.nodes.push(Node {
                        path: trace.resolved.clone(),
                        stamp: stamp.clone(),
                        target: target.clone(),
                    });
                    if let Some(target) = target {
                        ensure!(
                            metadata_stamp(&fs::symlink_metadata(&trace.resolved)?) == stamp,
                            "Attribute symlink changed while reading; retry"
                        );
                        if !follow {
                            trace.available = false;
                            return Ok(trace);
                        }
                        links += 1;
                        ensure!(links <= 40, "Attribute symlink chain is too deep");
                        trace.resolved.pop();
                        for step in steps(&target).into_iter().rev() {
                            pending.push_front(step);
                        }
                    } else if !pending.is_empty() && !metadata.is_dir() {
                        if !follow {
                            trace.available = false;
                            return Ok(trace);
                        }
                        bail!(
                            "Attribute source parent is not a directory: {}",
                            trace.resolved.display()
                        );
                    } else if pending.is_empty() && !follow && !metadata.is_file() {
                        trace.available = false;
                        return Ok(trace);
                    }
                }
            }
        }
        Ok(trace)
    }
}

fn read_source(
    digest: &mut Sha256,
    label: &[u8],
    path: &Path,
    follow: bool,
) -> Result<Option<Vec<u8>>> {
    let trace = Trace::read(path, follow)?;
    hash_trace(digest, label, &trace);
    if !trace.available {
        return Ok(None);
    }
    let mut file = open_pinned(&trace.resolved)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "Attribute source must be a regular file: {}",
        path.display()
    );
    ensure!(
        trace
            .nodes
            .last()
            .is_some_and(|node| node.stamp == metadata_stamp(&before)),
        "Attribute source changed while opening; retry"
    );
    let mut bytes = Vec::new();
    file.by_ref()
        .take(
            before
                .len()
                .checked_add(1)
                .context("Attribute source length overflow")?,
        )
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == before.len()
            && metadata_stamp(&file.metadata()?) == metadata_stamp(&before),
        "Attribute source changed while reading; retry"
    );
    ensure!(
        Trace::read(path, follow)? == trace,
        "Attribute source route changed while reading; retry"
    );
    field(digest, b"source-content", &bytes);
    Ok(Some(bytes))
}

#[cfg(unix)]
fn open_pinned(path: &Path) -> Result<fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    ensure!(path.is_absolute(), "Attribute source path must be absolute");
    let mut directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open("/")?;
    let names = path
        .components()
        .filter_map(|component| {
            if let Component::Normal(name) = component {
                Some(name)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for (position, name) in names.iter().enumerate() {
        let name = CString::new(name.as_bytes())?;
        let last = position + 1 == names.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: directory owns a valid descriptor and name is NUL-terminated.
        // A successful new descriptor is transferred once to File ownership.
        let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error()).context(
                "Attribute source changed or could not be opened without following links; retry",
            );
        }
        let opened = unsafe { fs::File::from_raw_fd(descriptor) };
        if last {
            return Ok(opened);
        }
        directory = opened;
    }
    bail!("Attribute source must name a regular file")
}
#[cfg(not(unix))]
fn open_pinned(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    Ok(options.open(path)?)
}

fn metadata_stamp(metadata: &fs::Metadata) -> Vec<u8> {
    let mut digest = Sha256::new();
    let kind = if metadata.is_dir() {
        b"directory".as_slice()
    } else if metadata.is_file() {
        b"file".as_slice()
    } else if metadata.file_type().is_symlink() {
        b"symlink".as_slice()
    } else {
        b"special".as_slice()
    };
    field(&mut digest, b"kind", kind);
    field(
        &mut digest,
        b"readonly",
        &[u8::from(metadata.permissions().readonly())],
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        field(&mut digest, b"dev", &metadata.dev().to_le_bytes());
        field(&mut digest, b"ino", &metadata.ino().to_le_bytes());
        field(&mut digest, b"mode", &metadata.mode().to_le_bytes());
        field(&mut digest, b"uid", &metadata.uid().to_le_bytes());
        field(&mut digest, b"gid", &metadata.gid().to_le_bytes());
        if !metadata.is_dir() {
            field(&mut digest, b"ctime", &metadata.ctime().to_le_bytes());
            field(
                &mut digest,
                b"ctime-ns",
                &metadata.ctime_nsec().to_le_bytes(),
            );
        }
    }
    if let Ok(created) = metadata.created() {
        hash_time(&mut digest, b"created", created);
    }
    // Directory mtime/length can change for unrelated files. Identity is pinned
    // above, while only actual attribute files pin their content timestamps.
    if !metadata.is_dir() {
        field(&mut digest, b"length", &metadata.len().to_le_bytes());
        if let Ok(modified) = metadata.modified() {
            hash_time(&mut digest, b"modified", modified);
        }
    }
    digest.finalize().to_vec()
}
fn hash_time(digest: &mut Sha256, label: &[u8], time: SystemTime) {
    let (negative, duration) = match time.duration_since(UNIX_EPOCH) {
        Ok(value) => (false, value),
        Err(error) => (true, error.duration()),
    };
    field(digest, label, &[u8::from(negative)]);
    field(digest, b"seconds", &duration.as_secs().to_le_bytes());
    field(digest, b"nanos", &duration.subsec_nanos().to_le_bytes());
}
fn hash_trace(digest: &mut Sha256, label: &[u8], trace: &Trace) {
    field(digest, b"source-kind", label);
    field(digest, b"requested-path", &path_bytes(&trace.requested));
    field(digest, b"resolved-path", &path_bytes(&trace.resolved));
    field(digest, b"available", &[u8::from(trace.available)]);
    field(digest, b"missing-leaf", &[u8::from(trace.missing)]);
    for node in trace
        .nodes
        .iter()
        .filter(|node| !trace.missing || node.target.is_some())
    {
        // For genuinely missing leaves, normal parent creation does not alter
        // the attribute input. Symlink routes remain pinned; blocked links and
        // non-directory parents have missing=false and retain their full trace.
        field(digest, b"node-path", &path_bytes(&node.path));
        field(digest, b"node-stamp", &node.stamp);
        field(
            digest,
            b"symlink-present",
            &[u8::from(node.target.is_some())],
        );
        if let Some(target) = &node.target {
            field(digest, b"symlink-target", &path_bytes(target));
        }
    }
}
fn hash_config(digest: &mut Sha256, config: &git2::Config, key: &str) -> Result<()> {
    field(digest, b"config-key", key.as_bytes());
    match config.get_entry(key) {
        Ok(entry) => {
            field(digest, b"config-present", b"yes");
            field(digest, b"config-has-value", &[u8::from(entry.has_value())]);
            if entry.has_value() {
                field(digest, b"config-value", entry.value_bytes());
            }
            field(
                digest,
                b"config-level",
                &(entry.level() as i32).to_le_bytes(),
            );
            field(
                digest,
                b"config-include-depth",
                &entry.include_depth().to_le_bytes(),
            );
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            field(digest, b"config-present", b"no")
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
fn path_bytes(path: &Path) -> Vec<u8> {
    super::super::libgit::path_bytes(path)
}

pub(super) fn route_fingerprint(path: &Path) -> Result<[u8; 32]> {
    let trace = Trace::read(path, true)?;
    let mut digest = Sha256::new();
    field(
        &mut digest,
        b"route-requested",
        &path_bytes(&trace.requested),
    );
    field(&mut digest, b"route-resolved", &path_bytes(&trace.resolved));
    // Cache directories/objects may be added or removed after preview. Pin the
    // selected route, not inventory or ordinary directory timestamps/identity.
    for node in trace.nodes.iter().filter(|node| node.target.is_some()) {
        field(&mut digest, b"symlink-path", &path_bytes(&node.path));
        field(
            &mut digest,
            b"symlink-target",
            &path_bytes(node.target.as_ref().unwrap()),
        );
    }
    Ok(digest.finalize().into())
}
fn field(digest: &mut Sha256, label: &[u8], bytes: &[u8]) {
    digest.update((label.len() as u64).to_le_bytes());
    digest.update(label);
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}
fn hash_values(digest: &mut Sha256, values: &BTreeMap<String, AttributeState>) {
    for (name, state) in values {
        field(digest, b"attribute", name.as_bytes());
        match state {
            AttributeState::Unspecified => field(digest, b"state", b"unspecified"),
            AttributeState::Set => field(digest, b"state", b"set"),
            AttributeState::Unset => field(digest, b"state", b"unset"),
            AttributeState::Value(value) => {
                field(digest, b"state", b"value");
                field(digest, b"value", value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, git2::Repository) {
        let directory = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(directory.path()).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", "")
            .unwrap();
        (directory, repo)
    }

    fn inspect(repo: &git2::Repository, path: &str) -> AttributeQuery {
        query(
            repo,
            Path::new(path),
            Lookup::WorktreeThenIndex,
            &["filter", "text", "eol", "ident", "unknown"],
        )
        .unwrap()
    }

    fn index_rules(repo: &git2::Repository, bytes: &[u8]) {
        fs::write(repo.workdir().unwrap().join(".gitattributes"), bytes).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitattributes")).unwrap();
        index.write().unwrap();
    }

    #[test]
    fn selected_attribute_states_are_owned_and_repeatable() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<AttributeQuery>();
        let (directory, repo) = repository();
        fs::write(
            directory.path().join(".gitattributes"),
            b"*.bin filter=lfs text -ident eol=lf\n*.bin !eol\n",
        )
        .unwrap();
        let first = inspect(&repo, "data.bin");
        assert_eq!(
            first.values["filter"],
            AttributeState::Value(b"lfs".to_vec())
        );
        assert_eq!(first.values["text"], AttributeState::Set);
        assert_eq!(first.values["ident"], AttributeState::Unset);
        assert_eq!(first.values["eol"], AttributeState::Unspecified);
        assert_eq!(first.values["unknown"], AttributeState::Unspecified);
        assert_eq!(first, inspect(&repo, "data.bin"));
    }

    #[test]
    fn unchanged_outcomes_do_not_hide_attribute_comment_changes() {
        let (directory, repo) = repository();
        let path = directory.path().join(".gitattributes");
        fs::write(&path, b"*.bin text\n").unwrap();
        let before = inspect(&repo, "data.bin");
        fs::write(path, b"# changed provenance\n*.bin text\n").unwrap();
        let after = inspect(&repo, "data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[test]
    fn global_source_path_changes_are_pinned_even_with_identical_contents() {
        let (directory, repo) = repository();
        let first = directory.path().join("first-attributes");
        let second = directory.path().join("second-attributes");
        fs::write(&first, b"*.bin text\n").unwrap();
        fs::write(&second, b"*.bin text\n").unwrap();
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", first.to_str().unwrap())
            .unwrap();
        let before = inspect(&repo, "data.bin");
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", second.to_str().unwrap())
            .unwrap();
        let after = inspect(&repo, "data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[test]
    fn missing_and_empty_attribute_sources_have_distinct_provenance() {
        let (directory, repo) = repository();
        let global = directory.path().join("missing-global");
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", global.to_str().unwrap())
            .unwrap();
        let before = inspect(&repo, "nested/data.bin");
        fs::write(global, b"").unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(directory.path().join("nested/.gitattributes"), b"").unwrap();
        let after = inspect(&repo, "nested/data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[test]
    fn worktree_to_index_fallback_changes_fingerprint_without_changing_values() {
        let (directory, repo) = repository();
        index_rules(&repo, b"*.bin text\n");
        let before = inspect(&repo, "data.bin");
        fs::remove_file(directory.path().join(".gitattributes")).unwrap();
        let after = inspect(&repo, "data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[test]
    fn lookup_order_and_raw_query_paths_are_part_of_the_fingerprint() {
        let (_directory, repo) = repository();
        index_rules(&repo, b"*.bin text\n");
        let before = inspect(&repo, "data.bin");
        let ordered = query(
            &repo,
            Path::new("data.bin"),
            Lookup::IndexThenWorktree,
            &["filter", "text", "eol", "ident", "unknown"],
        )
        .unwrap();
        let renamed = inspect(&repo, "other.bin");
        assert_eq!(before.values, ordered.values);
        assert_eq!(before.values, renamed.values);
        assert_ne!(before.fingerprint, ordered.fingerprint);
        assert_ne!(before.fingerprint, renamed.fingerprint);
    }

    #[test]
    fn shadowed_index_attribute_content_is_still_pinned() {
        let (directory, repo) = repository();
        index_rules(&repo, b"*.bin text\n");
        let before = inspect(&repo, "data.bin");
        index_rules(&repo, b"# index changed\n*.bin text\n");
        fs::write(directory.path().join(".gitattributes"), b"*.bin text\n").unwrap();
        let after = inspect(&repo, "data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[test]
    fn empty_selection_returns_no_unrequested_attributes() {
        let (directory, repo) = repository();
        fs::write(
            directory.path().join(".gitattributes"),
            b"*.bin text filter=lfs\n",
        )
        .unwrap();
        assert!(
            query(&repo, Path::new("data.bin"), Lookup::WorktreeThenIndex, &[])
                .unwrap()
                .values
                .is_empty()
        );
    }

    #[test]
    fn snapshot_validation_rejects_attribute_changes_after_an_earlier_read() {
        let (directory, repo) = repository();
        let source = directory.path().join(".gitattributes");
        fs::write(&source, b"*.bin text\n").unwrap();
        let snapshot = Snapshot::capture(
            &repo,
            Path::new("data.bin"),
            Lookup::WorktreeThenIndex,
            &["text"],
        )
        .unwrap();
        fs::write(source, b"# changed during assembly\n*.bin text\n").unwrap();
        assert!(snapshot.verify(&repo).is_err());
    }

    #[test]
    fn config_selection_is_fresh_even_when_external_rewrite_preserves_length_and_mtime() {
        let (directory, repo) = repository();
        let first = directory.path().join("attrs-a");
        let second = directory.path().join("attrs-b");
        fs::write(&first, b"*.bin text\n").unwrap();
        fs::write(&second, b"*.bin text\n").unwrap();
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", first.to_str().unwrap())
            .unwrap();
        let before = inspect(&repo, "data.bin");
        let config_path = repo.path().join("config");
        let metadata = fs::metadata(&config_path).unwrap();
        let old = fs::read_to_string(&config_path).unwrap();
        let new = old.replace("attrs-a", "attrs-b");
        assert_ne!(old, new);
        assert_eq!(old.len(), new.len());
        fs::write(&config_path, new).unwrap();
        fs::File::options()
            .write(true)
            .open(config_path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(metadata.modified().unwrap())
                    .set_accessed(metadata.accessed().unwrap()),
            )
            .unwrap();
        let after = inspect(&repo, "data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn global_symlink_routes_are_pinned_but_worktree_symlinks_remain_unread() {
        let (directory, repo) = repository();
        let first = directory.path().join("target-a");
        let second = directory.path().join("target-b");
        let link = directory.path().join("global-link");
        fs::write(&first, b"*.bin text\n").unwrap();
        fs::write(&second, b"*.bin text\n").unwrap();
        std::os::unix::fs::symlink(&first, &link).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", link.to_str().unwrap())
            .unwrap();
        let before = inspect(&repo, "data.bin");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();
        let after = inspect(&repo, "data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
        std::os::unix::fs::symlink(&first, directory.path().join(".gitattributes")).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", "")
            .unwrap();
        assert_eq!(
            inspect(&repo, "data.bin").values["text"],
            AttributeState::Unspecified
        );
    }

    #[test]
    fn every_missing_source_is_distinct_from_a_present_empty_file() {
        for source in ["root", "nested", "global", "info"] {
            let (directory, repo) = repository();
            fs::create_dir_all(directory.path().join("nested")).unwrap();
            let path = match source {
                "root" => directory.path().join(".gitattributes"),
                "nested" => directory.path().join("nested/.gitattributes"),
                "info" => repo.commondir().join("info/attributes"),
                _ => {
                    let path = directory.path().join("missing-global");
                    repo.config()
                        .unwrap()
                        .set_str("core.attributesfile", path.to_str().unwrap())
                        .unwrap();
                    path
                }
            };
            let before = inspect(&repo, "nested/data.bin");
            fs::write(path, b"").unwrap();
            let after = inspect(&repo, "nested/data.bin");
            assert_eq!(before.values, after.values);
            assert_ne!(
                before.fingerprint, after.fingerprint,
                "missing {source} was not pinned"
            );
        }
    }

    #[test]
    fn unrelated_worktree_files_and_git_journals_do_not_change_source_fingerprints() {
        let (directory, repo) = repository();
        fs::write(directory.path().join(".gitattributes"), b"*.bin text\n").unwrap();
        let before = inspect(&repo, "data.bin");
        fs::write(directory.path().join("worktree-temporary"), b"unrelated").unwrap();
        fs::write(repo.path().join("journal-temporary"), b"unrelated").unwrap();
        assert_eq!(before, inspect(&repo, "data.bin"));
        fs::remove_file(directory.path().join("worktree-temporary")).unwrap();
        fs::remove_file(repo.path().join("journal-temporary")).unwrap();
        assert_eq!(before, inspect(&repo, "data.bin"));
    }

    #[test]
    fn safe_parent_creation_does_not_change_an_absent_attribute_leaf() {
        let (directory, repo) = repository();
        fs::write(directory.path().join(".gitattributes"), b"*.bin text\n").unwrap();
        let before = inspect(&repo, "nested/deep/data.bin");
        fs::create_dir_all(directory.path().join("nested/deep")).unwrap();
        assert_eq!(before, inspect(&repo, "nested/deep/data.bin"));
        fs::write(directory.path().join("nested/deep/.gitattributes"), b"").unwrap();
        assert_ne!(
            before.fingerprint,
            inspect(&repo, "nested/deep/data.bin").fingerprint
        );
    }

    #[test]
    fn attribute_query_never_resets_a_callers_pending_index_entries() {
        let (directory, repo) = repository();
        fs::write(directory.path().join(".gitattributes"), b"*.bin text\n").unwrap();
        fs::write(directory.path().join("pending"), b"unstored index content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("pending")).unwrap();
        let expected = index.get_path(Path::new("pending"), 0).unwrap().id;
        inspect(&repo, "data.bin");
        assert_eq!(
            index.get_path(Path::new("pending"), 0).unwrap().id,
            expected
        );
    }

    #[test]
    fn effective_case_configuration_is_pinned_even_for_identical_matches() {
        let (directory, repo) = repository();
        fs::write(directory.path().join(".gitattributes"), b"*.bin text\n").unwrap();
        repo.config()
            .unwrap()
            .set_bool("core.ignorecase", false)
            .unwrap();
        let before = inspect(&repo, "data.bin");
        repo.config()
            .unwrap()
            .set_bool("core.ignorecase", true)
            .unwrap();
        let after = inspect(&repo, "data.bin");
        assert_eq!(before.values, after.values);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn raw_query_path_and_attribute_value_bytes_are_not_lossily_decoded() {
        use std::os::unix::ffi::OsStrExt;
        let (directory, repo) = repository();
        fs::write(
            directory.path().join(".gitattributes"),
            b"*.bin working-tree-encoding=custom\xff\n",
        )
        .unwrap();
        let selected = &["working-tree-encoding"];
        let first = query(
            &repo,
            Path::new(std::ffi::OsStr::from_bytes(b"data-\xff.bin")),
            Lookup::WorktreeThenIndex,
            selected,
        )
        .unwrap();
        let second = query(
            &repo,
            Path::new(std::ffi::OsStr::from_bytes(b"data-\xfe.bin")),
            Lookup::WorktreeThenIndex,
            selected,
        )
        .unwrap();
        assert_eq!(
            first.values["working-tree-encoding"],
            AttributeState::Value(b"custom\xff".to_vec())
        );
        assert_eq!(first.values, second.values);
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn no_system_uses_git_boolean_semantics() {
        for value in ["", "false", "no", "OFF", "0"] {
            assert!(!no_system(Some(value.into())).unwrap());
        }
        for value in ["yes", "true", "ON", "1", "2"] {
            assert!(no_system(Some(value.into())).unwrap());
        }
        assert!(no_system(Some("invalid".into())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_pattern_matching_does_not_require_materializing_an_invalid_filesystem_name() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(directory.path()).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.attributesfile", "")
            .unwrap();
        fs::write(
            directory.path().join(".gitattributes"),
            b"\"nonutf8-\\377.bin\" filter=lfs\n",
        )
        .unwrap();
        let path = Path::new(std::ffi::OsStr::from_bytes(b"nonutf8-\xff.bin"));
        assert!(is_lfs(&repo, path, Lookup::WorktreeThenIndex).unwrap());
    }

    #[test]
    fn target_tree_uses_immutable_rules_and_source_overrides_not_old_index_or_worktree() {
        let (directory, repo) = repository();
        index_rules(&repo, b"*.bin -filter\n");
        let blob = repo
            .blob(b"ASSET.BIN filter=lfs working-tree-encoding=UTF-16LE\n")
            .unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(".gitattributes", blob, 0o100644).unwrap();
        let tree = tree.write().unwrap();
        repo.config()
            .unwrap()
            .set_bool("core.ignorecase", true)
            .unwrap();
        let query_target = || {
            query(
                &repo,
                Path::new("asset.bin"),
                Lookup::TargetTree(tree),
                &["filter", "working-tree-encoding"],
            )
            .unwrap()
        };
        let first = query_target();
        assert_eq!(
            first.values["filter"],
            AttributeState::Value(b"lfs".to_vec())
        );
        assert_eq!(
            first.values["working-tree-encoding"],
            AttributeState::Value(b"UTF-16LE".to_vec())
        );
        index_rules(
            &repo,
            b"*.bin filter=unrelated working-tree-encoding=UTF-32LE\n",
        );
        assert_eq!(
            query_target(),
            first,
            "target lookup was pinned to mutable old checkout attributes"
        );
        fs::create_dir_all(repo.commondir().join("info")).unwrap();
        fs::write(repo.commondir().join("info/attributes"), b"*.bin -filter\n").unwrap();
        let overridden = query_target();
        assert_eq!(overridden.values["filter"], AttributeState::Unset);
        assert_ne!(overridden.fingerprint, first.fingerprint);
        fs::remove_file(directory.path().join(".gitattributes")).unwrap();
        assert_eq!(query_target(), overridden);
    }

    #[test]
    fn target_tree_missing_rules_do_not_fall_back_to_obsolete_checkout_attributes() {
        let (_directory, repo) = repository();
        index_rules(&repo, b"*.bin filter=lfs\n");
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let result = query(
            &repo,
            Path::new("asset.bin"),
            Lookup::TargetTree(tree),
            &["filter"],
        )
        .unwrap();
        assert_eq!(result.values["filter"], AttributeState::Unspecified);
    }

    #[test]
    fn directory_view_reuses_frozen_rules_for_siblings_and_detects_source_changes() {
        let (directory, repo) = repository();
        fs::write(
            directory.path().join(".gitattributes"),
            b"*.txt working-tree-encoding=UTF-16LE\n*.bin filter=lfs\n",
        )
        .unwrap();
        let selected = &["filter", "working-tree-encoding"];
        let view = DirectoryView::capture(
            &repo,
            Path::new("one.txt"),
            Lookup::WorktreeThenIndex,
            selected,
        )
        .unwrap();
        for path in ["one.txt", "two.txt", "other.bin"] {
            assert_eq!(
                view.values(Path::new(path)).unwrap(),
                query(&repo, Path::new(path), Lookup::WorktreeThenIndex, selected)
                    .unwrap()
                    .values
            );
        }
        assert!(view.values(Path::new("nested/other.txt")).is_err());
        fs::write(
            directory.path().join("unrelated"),
            b"not an attribute source",
        )
        .unwrap();
        view.verify(&repo).unwrap();
        fs::write(
            directory.path().join(".gitattributes"),
            b"*.txt working-tree-encoding=UTF-32LE\n",
        )
        .unwrap();
        assert!(view.verify(&repo).is_err());
    }
}
