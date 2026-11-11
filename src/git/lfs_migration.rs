//! Reviewed, single-branch LFS import. The official rewriter runs only in a
//! disposable repository; native publication has its own durable recovery state.
use super::{
    NetworkControl, Repository, lfs,
    libgit::{WriterLock, oid, raw},
};
use anyhow::{Context, Result, bail, ensure};
use git2::{
    Oid, Repository as Raw, RepositoryState,
    build::{CheckoutBuilder, CloneLocal, RepoBuilder},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const JOURNAL: &str = "gitbuddy-lfs-migration.json";
const MIGRATION_REF: &str = "refs/heads/gitbuddy-lfs-migrate";

#[derive(Debug)]
pub struct LfsMigrationPlan {
    pub original: String,
    pub target: String,
    pub branch: String,
    pub patterns: String,
    pub commits: usize,
    pub objects: usize,
    pub bytes: u64,
    pub unsigned: bool,
    pub signed_commits: usize,
    workspace: tempfile::TempDir,
    state: State,
}
impl LfsMigrationPlan {
    pub fn report(&self) -> String {
        format!(
            "{}\n{} → {}\nPatterns: {}\n{} rewritten commits · {} new LFS objects · {} bytes\nOther branches and tags retain their original history. Original and candidate tips are kept in backup references. No remote is contacted or pushed.{}",
            self.branch,
            self.original,
            self.target,
            self.patterns,
            self.commits,
            self.objects,
            self.bytes,
            if self.unsigned {
                format!(
                    "\nUnsigned rewrite explicitly allowed; {} existing signed commits lose their signatures.",
                    self.signed_commits
                )
            } else {
                String::new()
            }
        )
    }
}
#[derive(Clone, Debug)]
pub struct LfsMigrationStatus {
    pub expected: String,
    pub branch: String,
    pub original: String,
    pub target: String,
    pub owner: PathBuf,
    pub phase: String,
    pub can_continue: bool,
    pub recovery: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Prepared,
    CheckingOut,
    CheckedOut,
    Published,
    Aborting,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    path: String,
    kind: String,
    hash: String,
    mode: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    id: String,
    owner: PathBuf,
    common: PathBuf,
    branch: String,
    original: String,
    target: String,
    config: String,
    index: String,
    before: Vec<Entry>,
    after: Vec<Entry>,
    after_index: String,
    phase: Phase,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(super) fn pending(repo: &Raw) -> bool {
    match fs::symlink_metadata(repo.commondir().join(JOURNAL)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}
fn no_symlink(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            !meta.file_type().is_symlink(),
            "Migration path must not traverse a symlink: {}",
            path.display()
        );
    }
    Ok(())
}
fn relative_path(relative: &str) -> Result<&Path> {
    let path = Path::new(relative);
    ensure!(!relative.is_empty() && path.components().all(|c| matches!(c, Component::Normal(p) if !p.eq_ignore_ascii_case(std::ffi::OsStr::new(".git")))), "Unsafe migration path");
    Ok(path)
}
fn checked_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = relative_path(relative)?;
    let mut full = root.to_path_buf();
    no_symlink(&full)?;
    let parts: Vec<_> = path.components().collect();
    for (i, part) in parts.iter().enumerate() {
        full.push(part.as_os_str());
        if i + 1 < parts.len() {
            no_symlink(&full)?;
        }
    }
    Ok(full)
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
fn entry(root: &Path, path: &str) -> Result<Entry> {
    let full = checked_path(root, path)?;
    let meta = match fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Entry {
                path: path.into(),
                kind: "missing".into(),
                hash: String::new(),
                mode: 0,
            });
        }
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = 0;
    let (kind, hash) = if meta.file_type().is_symlink() {
        (
            "symlink",
            digest(super::libgit::path_bytes(&fs::read_link(&full)?).as_slice()),
        )
    } else {
        ensure!(
            meta.is_file(),
            "Migration needs a regular file: {}",
            full.display()
        );
        ("file", file_hash(&full)?)
    };
    Ok(Entry {
        path: path.into(),
        kind: kind.into(),
        hash,
        mode,
    })
}
fn manifest(root: &Path, paths: impl Iterator<Item = String>) -> Result<Vec<Entry>> {
    paths.map(|p| entry(root, &p)).collect()
}
fn index_bytes(repo: &Raw) -> Result<Vec<u8>> {
    fs::read(repo.path().join("index")).context("Migration requires an existing index")
}
fn config_hash(repo: &Raw) -> Result<String> {
    let fresh = Raw::open(repo.path())?;
    let mut config = fresh.config()?;
    let config = config.snapshot()?;
    let mut entries = config.entries(None)?;
    let mut values = Vec::new();
    let mut names = BTreeSet::new();
    while let Some(e) = entries.next() {
        let e = e?;
        names.insert(e.name()?.to_owned());
        values.push((
            e.name_bytes().to_vec(),
            e.has_value().then(|| e.value_bytes().to_vec()),
            e.has_value(),
            e.level() as i32,
        ));
    }
    values.sort();
    // A raw multiset cannot distinguish duplicate-key precedence, and absent
    // values are implicit true while explicit empty boolean values are false.
    let mut effective = Vec::new();
    for name in names {
        let entry = config.get_entry(&name)?;
        effective.push((
            name,
            entry.has_value().then(|| entry.value_bytes().to_vec()),
            entry.has_value(),
            entry.level() as i32,
        ));
    }
    let mut data = serde_json::to_vec(&(values, effective))?;
    for path in [
        repo.path().join("info/attributes"),
        repo.commondir().join("info/attributes"),
    ] {
        no_symlink(&path)?;
        let content = if path.exists() {
            Some(fs::read(&path)?)
        } else {
            None
        };
        data.extend(serde_json::to_vec(&(path, content))?);
    }
    Ok(digest(&data))
}
type TreeEntries = BTreeMap<String, (i32, Oid)>;
fn tree_entries(
    repo: &Raw,
    tree: &git2::Tree<'_>,
    prefix: &str,
    result: &mut TreeEntries,
) -> Result<()> {
    for e in tree {
        let name = e
            .name()
            .context("LFS migration currently requires UTF-8 paths")?;
        let path = format!("{prefix}{name}");
        relative_path(&path)?;
        if e.kind() == Some(git2::ObjectType::Tree) {
            tree_entries(repo, &repo.find_tree(e.id())?, &format!("{path}/"), result)?;
        } else {
            result.insert(path, (e.filemode(), e.id()));
        }
    }
    Ok(())
}
fn commit_entries(repo: &Raw, id: &str) -> Result<TreeEntries> {
    let mut entries = BTreeMap::new();
    tree_entries(repo, &repo.find_commit(oid(id)?)?.tree()?, "", &mut entries)?;
    Ok(entries)
}
fn head(repo: &Raw) -> Result<(String, String)> {
    let h = repo.head()?;
    ensure!(
        h.is_branch(),
        "Choose a current local branch for LFS migration"
    );
    ensure!(
        repo.find_reference("HEAD")?.symbolic_target()? == Some(h.name()?),
        "LFS migration requires HEAD to name its local branch directly"
    );
    Ok((
        h.name()?.into(),
        h.target().context("Unborn branch")?.to_string(),
    ))
}
fn available(repo: &Raw) -> Result<()> {
    ensure!(
        !repo.is_bare() && !repo.is_shallow(),
        "LFS migration requires a nonshallow working repository"
    );
    let (branch, original) = head(repo)?;
    ensure_branch_ownership(repo, &branch)?;
    ensure!(
        repo.state() == RepositoryState::Clean
            && !super::rebase::active(repo)
            && !super::integration::squash_active(repo)
            && !super::patch_batch::recovery_pending(repo),
        "Finish the current repository operation before LFS migration"
    );
    ensure!(
        !pending(repo),
        "Recover the interrupted LFS migration first"
    );
    super::worktree_conversion::guard_native_history_write(repo, Some(oid(&original)?))?;
    ensure!(
        super::libgit::file_changes(repo)?.is_empty(),
        "Commit or stash all changes before LFS migration"
    );
    lfs::validate_extension_configuration(repo)?;
    Ok(())
}
// Only this branch and this checkout are rewritten. Other worktrees may have
// unrelated edits, but cannot also own the branch whose ref is about to move.
fn ensure_branch_ownership(repo: &Raw, branch: &str) -> Result<()> {
    for other in other_worktrees(repo)? {
        ensure!(
            other.find_reference("HEAD")?.symbolic_target()? != Some(branch),
            "Migration branch is checked out in another worktree"
        );
    }
    Ok(())
}
fn other_worktrees(repo: &Raw) -> Result<Vec<Raw>> {
    let current = repo.path().canonicalize()?;
    let mut others = BTreeMap::new();
    let main = Raw::open(repo.commondir())?;
    others.insert(main.path().canonicalize()?, main);
    for name in repo.worktrees()?.iter() {
        let name = name?.context("Non UTF-8 worktree name")?;
        let tree = repo.find_worktree(name)?;
        let other = Raw::open_from_worktree(&tree)
            .context("Repair or prune an unavailable worktree before LFS migration")?;
        others.insert(other.path().canonicalize()?, other);
    }
    others.remove(&current);
    Ok(others.into_values().collect())
}
fn lock_other_heads(repos: &[Raw]) -> Result<Vec<git2::Transaction<'_>>> {
    let mut locks = Vec::with_capacity(repos.len());
    for repo in repos {
        let mut transaction = repo.transaction()?;
        transaction
            .lock_ref("HEAD")
            .context("Another worktree is updating HEAD; retry the migration later")?;
        locks.push(transaction);
    }
    Ok(locks)
}
fn is_signed(commit: &git2::Commit<'_>) -> bool {
    commit.raw_header_bytes().split(|b| *b == b'\n').any(|l| {
        l.starts_with(b"gpgsig ") || l.starts_with(b"gpgsig-sha256 ") || l.starts_with(b"mergetag ")
    })
}
fn ensure_source(repo: &Raw, state: &State) -> Result<()> {
    ensure_branch_ownership(repo, &state.branch)?;
    ensure!(
        repo.workdir().context("Missing worktree")?.canonicalize()? == state.owner
            && repo.commondir().canonicalize()? == state.common,
        "Migration belongs to another worktree"
    );
    ensure!(
        head(repo)? == (state.branch.clone(), state.original.clone()),
        "HEAD changed; prepare a new migration preview"
    );
    ensure!(
        config_hash(repo)? == state.config
            && digest(&index_bytes(repo)?) == state.index
            && manifest(&state.owner, state.before.iter().map(|e| e.path.clone()))? == state.before,
        "Index, attributes, configuration or worktree changed; prepare a new migration preview"
    );
    ensure!(
        super::libgit::file_changes(repo)?.is_empty(),
        "The worktree changed; prepare a new migration preview"
    );
    Ok(())
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn rewrite(
    program: &std::ffi::OsStr,
    root: &Path,
    workspace: &Path,
    patterns: &str,
    control: &NetworkControl,
) -> Result<()> {
    let empty = workspace.join("empty-config");
    fs::write(&empty, "")?;

    let storage = workspace.join("lfs");
    let map = workspace.join("object-map.csv");
    let output = tempfile::tempfile()?;
    let error = tempfile::tempfile()?;
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &empty)
        .env("GIT_CONFIG_SYSTEM", &empty)
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_CONFIG_COUNT", "7");
    for (i, (key, value)) in [
        ("core.hooksPath", std::ffi::OsStr::new("/dev/null")),
        ("lfs.storage", storage.as_os_str()),
        ("filter.lfs.process", std::ffi::OsStr::new("")),
        ("filter.lfs.smudge", std::ffi::OsStr::new("")),
        ("filter.lfs.clean", std::ffi::OsStr::new("")),
        ("filter.lfs.required", std::ffi::OsStr::new("false")),
        ("core.autocrlf", std::ffi::OsStr::new("false")),
    ]
    .iter()
    .enumerate()
    {
        command
            .env(format!("GIT_CONFIG_KEY_{i}"), key)
            .env(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    command
        .args(["migrate", "import", "--skip-fetch", "--yes"])
        .arg(format!("--include-ref={MIGRATION_REF}"))
        .arg(format!("--include={patterns}"))
        .arg(format!("--object-map={}", map.display()))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(error.try_clone()?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = Child(
        command
            .spawn()
            .context("Install the official git-lfs tool for history migration")?,
    );
    let started = Instant::now();
    loop {
        control.check()?;
        ensure!(
            started.elapsed() < Duration::from_secs(1800),
            "LFS migration exceeded 30 minutes; source repository unchanged"
        );
        ensure!(
            output.metadata()?.len() + error.metadata()?.len() <= 4 * 1024 * 1024,
            "LFS migration output exceeds its limit"
        );
        if let Some(status) = child.0.try_wait()? {
            let mut text = String::new();
            let mut error = error;
            error.rewind()?;
            error.read_to_string(&mut text)?;
            ensure!(status.success(), "Official LFS migration failed: {text}");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn object_files(workspace: &Path) -> Result<Vec<(PathBuf, lfs::Pointer)>> {
    let root = workspace.join("lfs/objects");
    let mut result = Vec::new();
    if !root.exists() {
        return Ok(result);
    }
    for first in fs::read_dir(root)? {
        let first = first?;
        ensure!(first.file_type()?.is_dir(), "Invalid migration LFS cache");
        for second in fs::read_dir(first.path())? {
            let second = second?;
            ensure!(second.file_type()?.is_dir(), "Invalid migration LFS cache");
            for file in fs::read_dir(second.path())? {
                let file = file?;
                ensure!(file.file_type()?.is_file(), "Invalid migration LFS object");
                let pointer = lfs::hash(&file.path())?;
                ensure!(
                    file.file_name().to_str() == Some(pointer.oid.as_str()),
                    "Corrupt migration LFS object"
                );
                result.push((file.path(), pointer));
            }
        }
    }
    Ok(result)
}

fn required_objects(
    workspace: &Path,
    repo: &Raw,
    tip: &str,
    control: &NetworkControl,
) -> Result<Vec<(PathBuf, lfs::Pointer)>> {
    let mut required = BTreeSet::new();
    let mut inspected = BTreeSet::new();
    let odb = repo.odb()?;
    let mut walk = repo.revwalk()?;
    walk.push(oid(tip)?)?;
    for id in walk {
        control.check()?;
        for (_, (mode, id)) in commit_entries(repo, &id?.to_string())? {
            if mode == 0o160000 || !inspected.insert(id) || odb.read_header(id)?.0 > 1024 {
                continue;
            }
            if let Some(pointer) = lfs::Pointer::parse(repo.find_blob(id)?.content()) {
                required.insert(pointer.oid);
            }
        }
    }
    Ok(object_files(workspace)?
        .into_iter()
        .filter(|(_, p)| required.contains(&p.oid))
        .collect())
}

// Official migrate can re-clean an already extended pointer as literal data.
// Preserve every existing pointer blob, then remap the affected descendants.
// The tool's unused double-wrapped objects never enter the source cache.
fn preserve_existing_pointers(
    source: &Raw,
    target: &Raw,
    original: &str,
    mapping: &mut BTreeMap<String, String>,
    control: &NetworkControl,
) -> Result<String> {
    let mut walk = source.revwalk()?;
    walk.push(oid(original)?)?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    let odb = target.odb()?;
    let source_odb = source.odb()?;
    let mut rewritten = BTreeMap::<Oid, Oid>::new();
    for old in walk {
        control.check()?;
        let old = old?;
        let intermediate = mapping
            .get(&old.to_string())
            .cloned()
            .unwrap_or_else(|| old.to_string());
        let intermediate = oid(&intermediate)?;
        let commit = target.find_commit(intermediate)?;
        let mut index = git2::Index::new()?;
        index.read_tree(&commit.tree()?)?;
        for (path, (mode, blob)) in commit_entries(source, &old.to_string())? {
            control.check()?;
            if mode == 0o160000 || source_odb.read_header(blob)?.0 > 1024 {
                continue;
            }
            let source_blob = source.find_blob(blob)?;
            let existing = index.get_path(Path::new(&path), 0);
            let pointer = if existing.as_ref().is_some_and(|entry| entry.id != blob) {
                lfs::Pointer::parse_checked(source_blob.content())?
            } else {
                lfs::Pointer::parse(source_blob.content())
            };
            if pointer.is_none() {
                continue;
            }
            let mut entry = index
                .get_path(Path::new(&path), 0)
                .context("Migration deleted an existing LFS path")?;
            ensure!(
                entry.mode == mode as u32,
                "Migration changed an existing LFS mode"
            );
            if entry.id != blob {
                let restored = odb.write(git2::ObjectType::Blob, source_blob.content())?;
                ensure!(restored == blob, "Cannot preserve existing LFS blob");
                entry.id = restored;
                index.add(&entry)?;
            }
        }
        let tree = index.write_tree_to(target)?;
        let object = odb.read(intermediate)?;
        let bytes = object.data();
        let split = bytes
            .windows(2)
            .position(|p| p == b"\n\n")
            .context("Invalid candidate commit")?;
        let mut result = Vec::new();
        for line in bytes[..split].split(|b| *b == b'\n') {
            if line.starts_with(b"tree ") {
                result.extend_from_slice(format!("tree {tree}").as_bytes());
            } else if let Some(parent) = line.strip_prefix(b"parent ") {
                let parent = oid(std::str::from_utf8(parent)?)?;
                let parent = rewritten.get(&parent).copied().unwrap_or(parent);
                result.extend_from_slice(format!("parent {parent}").as_bytes());
            } else {
                result.extend_from_slice(line);
            }
            result.push(b'\n');
        }
        result.push(b'\n');
        result.extend_from_slice(&bytes[split + 2..]);
        let final_id = if result == bytes {
            intermediate
        } else {
            odb.write(git2::ObjectType::Commit, &result)?
        };
        rewritten.insert(intermediate, final_id);
        mapping.insert(old.to_string(), final_id.to_string());
    }
    mapping
        .get(original)
        .cloned()
        .context("Missing normalized migration tip")
}
fn validate_rewrite(
    source: &Raw,
    target: &Raw,
    original: &str,
    mapping: &BTreeMap<String, String>,
    control: &NetworkControl,
) -> Result<()> {
    let mut walk = source.revwalk()?;
    walk.push(oid(original)?)?;
    let mut visited = BTreeSet::new();
    for old in walk {
        control.check()?;
        let old = old?;
        visited.insert(old.to_string());
        let old_commit = source.find_commit(old)?;
        let new_id = mapping
            .get(&old.to_string())
            .cloned()
            .unwrap_or_else(|| old.to_string());
        let new_commit = target.find_commit(oid(&new_id)?)?;
        let mut normalized_message = old_commit.message_bytes().to_vec();
        if !normalized_message.ends_with(b"\n") {
            normalized_message.push(b'\n');
        }
        ensure!(
            old_commit.message_bytes() == new_commit.message_bytes()
                || normalized_message == new_commit.message_bytes(),
            "Migration unexpectedly changed a commit message"
        );
        ensure!(
            old_commit.author().name_bytes() == new_commit.author().name_bytes()
                && old_commit.author().email_bytes() == new_commit.author().email_bytes()
                && old_commit.author().when() == new_commit.author().when(),
            "Migration changed author metadata"
        );
        ensure!(
            old_commit.committer().name_bytes() == new_commit.committer().name_bytes()
                && old_commit.committer().email_bytes() == new_commit.committer().email_bytes()
                && old_commit.committer().when() == new_commit.committer().when(),
            "Migration changed committer metadata"
        );
        let parents: Vec<_> = old_commit
            .parent_ids()
            .map(|p| {
                mapping
                    .get(&p.to_string())
                    .cloned()
                    .unwrap_or_else(|| p.to_string())
            })
            .collect();
        ensure!(
            parents
                == new_commit
                    .parent_ids()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>(),
            "Migration changed merge topology"
        );
        let before = commit_entries(source, &old.to_string())?;
        let after = commit_entries(target, &new_id)?;
        for (path, (mode, id)) in &before {
            let Some((new_mode, new_id)) = after.get(path) else {
                bail!("Migration deleted {path}");
            };
            ensure!(mode == new_mode, "Migration changed the mode of {path}");
            if *mode == 0o160000 {
                ensure!(id == new_id, "Migration changed a gitlink");
                continue;
            }
            let old_blob = source.find_blob(*id)?;
            let previous_pointer = if id != new_id {
                lfs::Pointer::parse_checked(old_blob.content())?
            } else {
                lfs::Pointer::parse(old_blob.content())
            };
            if previous_pointer.is_some() {
                ensure!(
                    id == new_id,
                    "Migration rewrote an existing LFS pointer: {path}"
                );
                continue;
            }
            if id == new_id
                || Path::new(path).file_name() == Some(std::ffi::OsStr::new(".gitattributes"))
            {
                continue;
            }
            let new_blob = target.find_blob(*new_id)?;
            let pointer = lfs::Pointer::parse(new_blob.content())
                .context("Migration produced a non-LFS change")?;
            if new_blob
                .content()
                .split(|b| *b == b'\n')
                .any(|l| l.starts_with(b"ext-"))
            {
                let restored = lfs::local_smudge_content_at(
                    target,
                    new_blob.content(),
                    Path::new(path),
                    old_blob.size().max(1) as u64,
                )?;
                ensure!(
                    restored == old_blob.content(),
                    "Migration extension does not restore original content: {path}"
                );
                continue;
            }
            ensure!(
                pointer.oid == digest(old_blob.content()) && pointer.size == old_blob.size() as u64,
                "Migration pointer does not match original content"
            );
        }
        for (path, (mode, _)) in &after {
            if !before.contains_key(path) {
                ensure!(
                    Path::new(path).file_name() == Some(std::ffi::OsStr::new(".gitattributes"))
                        && *mode == 0o100644,
                    "Migration unexpectedly added {path}"
                );
            }
        }
    }
    ensure!(
        mapping.keys().all(|k| visited.contains(k)),
        "Object map contains commits outside the reviewed branch"
    );
    Ok(())
}

// git-lfs can retain a now-invalid gpgsig header. Explicit unsigned migration
// removes complete signature blocks, then remaps descendants in topological
// order. Preserve every other raw header and message byte.
fn unsigned_graph(repo: &Raw, tip: &str) -> Result<BTreeMap<String, String>> {
    let mut walk = repo.revwalk()?;
    walk.push(oid(tip)?)?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    let odb = repo.odb()?;
    let mut mapping: BTreeMap<String, String> = BTreeMap::new();
    for id in walk {
        let id = id?;
        let object = odb.read(id)?;
        let bytes = object.data();
        let split = bytes
            .windows(2)
            .position(|p| p == b"\n\n")
            .context("Invalid commit headers")?;
        let mut result = Vec::new();
        let mut skipping = false;
        for line in bytes[..split].split(|b| *b == b'\n') {
            if !line.starts_with(b" ") {
                skipping = line.starts_with(b"gpgsig ")
                    || line.starts_with(b"gpgsig-sha256 ")
                    || line.starts_with(b"mergetag ");
            }
            if skipping {
                continue;
            }
            if let Some(parent) = line.strip_prefix(b"parent ") {
                let parent = std::str::from_utf8(parent)?;
                result.extend_from_slice(b"parent ");
                result.extend_from_slice(
                    mapping
                        .get(parent)
                        .map(String::as_bytes)
                        .unwrap_or(parent.as_bytes()),
                );
            } else {
                result.extend_from_slice(line);
            }
            result.push(b'\n');
        }
        result.push(b'\n');
        result.extend_from_slice(&bytes[split + 2..]);
        let new = if result == bytes {
            id
        } else {
            odb.write(git2::ObjectType::Commit, &result)?
        };
        mapping.insert(id.to_string(), new.to_string());
    }
    Ok(mapping)
}

impl Repository {
    pub fn prepare_lfs_migration(
        &self,
        patterns: &str,
        allow_unsigned: bool,
        control: NetworkControl,
    ) -> Result<LfsMigrationPlan> {
        let repo = raw(self)?;
        let _writer = WriterLock::acquire(&repo)?;
        available(&repo)?;
        control.check()?;
        ensure!(
            !patterns.trim().is_empty()
                && !patterns.contains(['\0', '\n', '\r'])
                && patterns.split(',').all(|p| !p.trim().is_empty()),
            "Enter comma-separated LFS filename patterns"
        );
        let (branch, original) = head(&repo)?;
        let owner = repo.workdir().context("No worktree")?.canonicalize()?;
        let entries = commit_entries(&repo, &original)?;
        let mut before = manifest(
            &owner,
            entries
                .iter()
                .filter(|(_, (mode, _))| *mode != 0o160000)
                .map(|(p, _)| p.clone()),
        )?;
        let config = config_hash(&repo)?;
        let index = digest(&index_bytes(&repo)?);
        let mut walk = repo.revwalk()?;
        walk.push(oid(&original)?)?;
        let mut signed_commits = 0;
        for id in walk {
            control.check()?;
            if is_signed(&repo.find_commit(id?)?) {
                signed_commits += 1;
            }
        }
        let configured_signing = match repo.config()?.get_bool("commit.gpgsign") {
            Ok(v) => v,
            Err(e) if e.code() == git2::ErrorCode::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            allow_unsigned || (!configured_signing && signed_commits == 0),
            "Rewriting signed history requires explicit approval for unsigned commits"
        );
        let workspace = tempfile::tempdir()?;
        let clone_path = workspace.path().join("repo");
        control.phase("Cloning reviewed history into an isolated local repository…");
        let mut builder = RepoBuilder::new();
        builder.clone_local(CloneLocal::Local).with_checkout({
            let mut c = CheckoutBuilder::new();
            c.disable_filters(true);
            c
        });
        let cloned = builder.clone(
            self.root
                .to_str()
                .context("Repository path must be UTF-8")?,
            &clone_path,
        )?;
        let remotes: Vec<_> = cloned
            .remotes()?
            .iter()
            .filter_map(|s| s.ok().flatten().map(str::to_owned))
            .collect();
        for name in remotes {
            cloned.remote_delete(&name)?;
        }
        let refs: Vec<_> = cloned
            .references()?
            .map(|r| Ok(r?.name()?.to_owned()))
            .collect::<Result<_>>()?;
        cloned.set_head_detached(oid(&original)?)?;
        for name in refs {
            cloned.find_reference(&name)?.delete()?;
        }
        cloned.reference(
            MIGRATION_REF,
            oid(&original)?,
            false,
            "isolated LFS migration",
        )?;
        cloned.set_head(MIGRATION_REF)?;
        let program = match repo.config()?.get_path("gitbuddy.lfsProgram") {
            Ok(p) => p.into_os_string(),
            Err(e) if e.code() == git2::ErrorCode::NotFound => "git-lfs".into(),
            Err(e) => return Err(e.into()),
        };
        // Copy only the extension transformation contract. Credentials,
        // remotes, source hooks and arbitrary source Git filters stay absent.
        let source_config = repo.config()?;
        let mut names = BTreeSet::new();
        let mut config_entries = source_config.entries(Some("^lfs\\.extension\\."))?;
        while let Some(entry) = config_entries.next() {
            names.insert(entry?.name().context("Non UTF-8 extension key")?.to_owned());
        }
        let mut candidate_config = cloned.config()?;
        for name in names {
            candidate_config.set_str(&name, &source_config.get_string(&name)?)?;
        }
        if let Ok(limit) = source_config.get_i64("gitbuddy.lfsFilterMaxBytes") {
            candidate_config.set_i64("gitbuddy.lfsFilterMaxBytes", limit)?;
        }
        candidate_config.set_str(
            "lfs.storage",
            workspace
                .path()
                .join("lfs")
                .to_str()
                .context("Non UTF-8 migration storage")?,
        )?;
        candidate_config.set_str(
            "gitbuddy.lfsProgram",
            program.to_str().context("Non UTF-8 LFS program")?,
        )?;
        rewrite(&program, &clone_path, workspace.path(), patterns, &control)?;
        let mut target = cloned
            .find_reference(MIGRATION_REF)?
            .target()
            .context("Missing migrated tip")?
            .to_string();
        let mut mapping = BTreeMap::new();
        for line in fs::read_to_string(workspace.path().join("object-map.csv"))?.lines() {
            let (old, new) = line.split_once(',').context("Invalid LFS object map")?;
            oid(old)?;
            oid(new)?;
            ensure!(
                mapping.insert(old.into(), new.into()).is_none(),
                "Duplicated object map entry"
            );
        }
        ensure!(
            mapping.get(&original).unwrap_or(&original) == &target,
            "Migration tip and object map disagree"
        );
        target = preserve_existing_pointers(&repo, &cloned, &original, &mut mapping, &control)?;
        cloned.reference(
            MIGRATION_REF,
            oid(&target)?,
            true,
            "preserve existing LFS pointers",
        )?;
        if allow_unsigned {
            let unsigned = unsigned_graph(&cloned, &target)?;
            let mut walk = repo.revwalk()?;
            walk.push(oid(&original)?)?;
            for old in walk {
                let old = old?.to_string();
                let intermediate = mapping.get(&old).cloned().unwrap_or_else(|| old.clone());
                if let Some(final_id) = unsigned.get(&intermediate) {
                    mapping.insert(old, final_id.clone());
                }
            }
            target = unsigned
                .get(&target)
                .context("Missing unsigned candidate")?
                .clone();
            cloned.reference(
                MIGRATION_REF,
                oid(&target)?,
                true,
                "explicit unsigned LFS candidate",
            )?;
        }
        validate_rewrite(&repo, &cloned, &original, &mapping, &control)?;
        let target_entries = commit_entries(&cloned, &target)?;
        let target_tree = cloned.find_commit(oid(&target)?)?.tree_id();
        for (path, (mode, id)) in &target_entries {
            control.check()?;
            // New pointers were validated against their raw source blob above.
            // Existing HEAD pointers must also be usable by the coming checkout.
            if matches!(*mode, 0o100644 | 0o100755)
                && entries.get(path) == Some(&(*mode, *id))
                && cloned.odb()?.read_header(*id)?.0 <= 1024
            {
                let blob = cloned.find_blob(*id)?;
                let query = lfs::query_attributes_from_tree(
                    &repo,
                    &cloned,
                    target_tree,
                    Path::new(path),
                    &["filter"],
                )?;
                if matches!(query.values.get("filter"), Some(lfs::AttributeState::Value(value)) if value == b"lfs")
                    && lfs::Pointer::parse(blob.content()).is_some()
                {
                    lfs::validate_local_checkout(&repo, blob.content(), Path::new(path))?;
                }
            }
        }
        for (path, (mode, _)) in &target_entries {
            if !entries.contains_key(path) && *mode != 0o160000 {
                let item = entry(&owner, path)?;
                ensure!(
                    item.kind == "missing",
                    "Migration would overwrite an untracked or ignored path: {path}"
                );
                before.push(item);
            }
        }
        before.sort_by(|a, b| a.path.cmp(&b.path));
        let objects = required_objects(workspace.path(), &cloned, &target, &control)?;
        let bytes = objects.iter().map(|(_, p)| p.size).sum();
        let state = State {
            version: 1,
            id: String::new(),
            owner,
            common: repo.commondir().canonicalize()?,
            branch: branch.clone(),
            original: original.clone(),
            target: target.clone(),
            config,
            index,
            before,
            after: vec![],
            after_index: String::new(),
            phase: Phase::Prepared,
        };
        ensure_source(&repo, &state)?;
        Ok(LfsMigrationPlan {
            original,
            target,
            branch,
            patterns: patterns.into(),
            commits: mapping.iter().filter(|(a, b)| a != b).count(),
            objects: objects.len(),
            bytes,
            unsigned: allow_unsigned,
            signed_commits,
            workspace,
            state,
        })
    }
    pub fn lfs_migration_status(&self) -> Result<Option<LfsMigrationStatus>> {
        let repo = raw(self)?;
        if !pending(&repo) {
            return Ok(None);
        }
        let (state, expected) = load(&repo)?;
        Ok(Some(LfsMigrationStatus {
            expected,
            branch: state.branch.clone(),
            original: state.original.clone(),
            target: state.target.clone(),
            owner: state.owner.clone(),
            phase: format!("{:?}", state.phase),
            can_continue: matches!(
                state.phase,
                Phase::Prepared | Phase::CheckedOut | Phase::Published
            ),
            recovery: directory(&repo, &state)?,
        }))
    }
}

fn directory(repo: &Raw, state: &State) -> Result<PathBuf> {
    ensure!(
        state.id.starts_with("migration-")
            && state
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid migration recovery directory"
    );
    let base = repo.commondir().join("gitbuddy-lfs-migrations");
    no_symlink(&base)?;
    let dir = base.join(&state.id);
    no_symlink(&dir)?;
    Ok(dir)
}
fn save(repo: &Raw, state: &State) -> Result<()> {
    let path = repo.commondir().join(JOURNAL);
    no_symlink(&path)?;
    atomic(&path, &serde_json::to_vec_pretty(state)?)
}
fn load(repo: &Raw) -> Result<(State, String)> {
    let path = repo.commondir().join(JOURNAL);
    no_symlink(&path)?;
    let bytes = fs::read(path)?;
    let state: State = serde_json::from_slice(&bytes)
        .context("Invalid LFS migration journal; preserve it for manual recovery")?;
    ensure!(
        state.version == 1
            && state.common == repo.commondir().canonicalize()?
            && state.branch.starts_with("refs/heads/")
            && git2::Reference::is_valid_name(&state.branch),
        "Invalid LFS migration journal"
    );
    ensure!(
        state.owner.is_absolute()
            && state.owner.canonicalize()? == state.owner
            && Raw::open(&state.owner)?.commondir().canonicalize()? == state.common,
        "Invalid migration worktree identity"
    );
    oid(&state.original)?;
    oid(&state.target)?;
    directory(repo, &state)?;
    let valid_hash = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    ensure!(
        valid_hash(&state.config) && valid_hash(&state.index),
        "Invalid migration fingerprints"
    );
    ensure!(
        state.before.windows(2).all(|w| w[0].path < w[1].path),
        "Invalid migration path order"
    );
    if matches!(state.phase, Phase::CheckedOut | Phase::Published) {
        ensure!(
            valid_hash(&state.after_index)
                && state
                    .before
                    .iter()
                    .map(|e| &e.path)
                    .eq(state.after.iter().map(|e| &e.path)),
            "Incomplete verified checkout journal"
        );
    }
    for e in state.before.iter().chain(&state.after) {
        checked_path(&state.owner, &e.path)?;
        ensure!(
            matches!(e.kind.as_str(), "file" | "symlink" | "missing"),
            "Invalid migration manifest"
        );
        ensure!(
            e.mode <= 0o777
                && if e.kind == "missing" {
                    e.hash.is_empty() && e.mode == 0
                } else {
                    valid_hash(&e.hash)
                },
            "Invalid migration file fingerprint"
        );
    }
    Ok((state, digest(&bytes)))
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("No parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
// Git writers reserve this exact pathname. Keep it reserved until Abort has
// installed the original index and published its final ref/journal state.
// A durable session marker also lets a reopened Abort reclaim only its own
// abandoned reservation; an unrelated Git index.lock is never removed.
struct AbortIndexLock {
    file: fs::File,
    path: PathBuf,
    marker: Vec<u8>,
}
impl AbortIndexLock {
    fn acquire(repo: &Raw, state: &State) -> Result<Self> {
        let path = repo.path().join("index.lock");
        let marker = format!("GitBuddy LFS Abort index reservation {}\n", state.id).into_bytes();
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let (mut file, created) = match options.create_new(true).open(&path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                options.create_new(false);
                (
                    options
                        .open(&path)
                        .context("Cannot inspect the existing index lock; it was not removed")?,
                    false,
                )
            }
            Err(error) => {
                return Err(error).context("Cannot reserve index.lock for LFS migration recovery");
            }
        };
        file.try_lock()
            .context("The index lock is held by another recovery process")?;
        if created {
            file.write_all(&marker)?;
            file.sync_all()?;
            fs::File::open(repo.path())?.sync_all()?;
        }
        let lock = Self { file, path, marker };
        if let Err(error) = lock.verify() {
            // Drop only removes our exact marker, never an unrelated Git lock.
            return Err(error.context(
                "An unrelated or changed index lock exists; verify its owner before recovery",
            ));
        }
        Ok(lock)
    }
    fn verify(&self) -> Result<()> {
        let path_metadata = fs::symlink_metadata(&self.path)?;
        ensure!(path_metadata.is_file(), "Index reservation was replaced");
        ensure!(
            path_metadata.len() == self.marker.len() as u64,
            "Index reservation contents changed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let held = self.file.metadata()?;
            ensure!(
                held.dev() == path_metadata.dev() && held.ino() == path_metadata.ino(),
                "Index reservation was replaced"
            );
        }
        ensure!(
            fs::read(&self.path)? == self.marker,
            "Index reservation contents changed"
        );
        Ok(())
    }
}
impl Drop for AbortIndexLock {
    fn drop(&mut self) {
        if self.verify().is_ok() {
            let _ = fs::remove_file(&self.path);
            if let Some(parent) = self.path.parent() {
                let _ = fs::File::open(parent).and_then(|file| file.sync_all());
            }
        }
        let _ = self.file.unlock();
    }
}
fn unchanged_destination(root: &Path, preserved: &Entry) -> Result<()> {
    ensure!(
        entry(root, &preserved.path)? == *preserved,
        "{} changed after Abort preserved it; the newer content was kept. Reopen Abort to preserve the new state before retrying",
        preserved.path
    );
    Ok(())
}
fn copy_entries(source: &Path, destination: &Path, entries: &[Entry]) -> Result<()> {
    copy_entries_with_sync(source, destination, entries, |path| {
        fs::File::open(path)?.sync_all().map_err(Into::into)
    })
}
fn make_backup_directories(path: &Path, directories: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut ancestor = Some(path);
    while let Some(current) = ancestor {
        directories.insert(current.to_path_buf());
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Recovery directory must not traverse a symlink: {}",
                    current.display()
                );
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = current.parent()
            }
            Err(error) => return Err(error.into()),
        }
    }
    fs::create_dir_all(path)?;
    Ok(())
}
fn copy_entries_with_sync(
    source: &Path,
    destination: &Path,
    entries: &[Entry],
    mut sync_directory: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let mut directories = BTreeSet::new();
    make_backup_directories(destination, &mut directories)?;
    for e in entries {
        if e.kind == "missing" {
            continue;
        }
        let from = checked_path(source, &e.path)?;
        let to = checked_path(destination, &e.path)?;
        make_backup_directories(to.parent().unwrap(), &mut directories)?;
        if e.kind == "symlink" {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(from)?, &to)?;
            #[cfg(not(unix))]
            bail!("Symlink migration recovery requires Unix");
        } else {
            fs::copy(from, &to)?;
            fs::File::open(&to)?.sync_all()?;
        }
    }
    ensure!(
        manifest(destination, entries.iter().map(|entry| entry.path.clone()))? == entries,
        "Recovery snapshot changed while being copied; destructive recovery was not started"
    );
    // Sync every newly linked directory entry, from the deepest child back to
    // its pre-existing ancestor, before the caller advances its durable phase.
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| a.cmp(b))
    });
    for directory in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

fn refs(state: &State) -> (String, String) {
    (
        format!("refs/gitbuddy/lfs-migration/{}/original", state.id),
        format!("refs/gitbuddy/lfs-migration/{}/candidate", state.id),
    )
}
fn pin(repo: &Raw, name: &str, id: &str) -> Result<()> {
    repo.reference(
        name,
        oid(id)?,
        false,
        "gitbuddy: preserve LFS migration history",
    )?;
    sync_reference(repo, name)
}
fn sync_reference(repo: &Raw, name: &str) -> Result<()> {
    let path = repo.commondir().join(name);
    fs::File::open(&path)?.sync_all()?;
    let mut parent = path.parent();
    while let Some(p) = parent {
        fs::File::open(p)?.sync_all()?;
        if p == repo.commondir() {
            break;
        }
        parent = p.parent();
    }
    for log in [
        repo.commondir().join("logs").join(name),
        repo.path().join("logs/HEAD"),
    ] {
        if log.exists() {
            fs::File::open(&log)?.sync_all()?;
            fs::File::open(log.parent().unwrap())?.sync_all()?;
        }
    }
    Ok(())
}
fn verify_owner(repo: &Raw, state: &State) -> Result<()> {
    ensure_branch_ownership(repo, &state.branch)?;
    ensure!(
        repo.workdir().context("No worktree")?.canonicalize()? == state.owner,
        "Recover LFS migration from its owning worktree: {}",
        state.owner.display()
    );
    let (branch, id) = head(repo)?;
    ensure!(
        branch == state.branch && (id == state.original || id == state.target),
        "HEAD moved outside the LFS migration; preserve recovery files and restore the recorded branch before recovering"
    );
    let (old, new) = refs(state);
    ensure!(
        repo.refname_to_id(&old)? == oid(&state.original)?
            && repo.refname_to_id(&new)? == oid(&state.target)?,
        "Migration backup references changed"
    );
    Ok(())
}
fn remove_journal(repo: &Raw) -> Result<()> {
    fs::remove_file(repo.commondir().join(JOURNAL))?;
    fs::File::open(repo.commondir())?.sync_all()?;
    Ok(())
}

impl Repository {
    pub(super) fn migrate_lfs(
        &self,
        plan: &LfsMigrationPlan,
        control: NetworkControl,
    ) -> Result<String> {
        let repo = raw(self)?;
        available(&repo)?;
        ensure_source(&repo, &plan.state)?;
        ensure!(
            plan.original == plan.state.original
                && plan.target == plan.state.target
                && plan.original != plan.target,
            "Migration plan changed or has no changes"
        );
        control.check()?;
        control.phase("Importing verified Git and LFS objects…");
        let base = repo.commondir().join("gitbuddy-lfs-migrations");
        no_symlink(&base)?;
        fs::create_dir_all(&base)?;
        let dir = tempfile::Builder::new()
            .prefix("migration-")
            .tempdir_in(&base)?;
        let mut state = plan.state.clone();
        state.id = dir.path().file_name().unwrap().to_str().unwrap().into();
        let cloned = Raw::open(plan.workspace.path().join("repo"))?;
        let mut walk = cloned.revwalk()?;
        walk.push(oid(&plan.target)?)?;
        let mut pack = cloned.packbuilder()?;
        pack.insert_walk(&mut walk)?;
        let odb = repo.odb()?;
        let mut writer = odb.packwriter()?;
        let mut failure = None;
        let hash_length = oid(&state.target)?.as_bytes().len();
        let mut trailer = Vec::with_capacity(hash_length);
        pack.foreach(|data| {
            if let Err(e) = control
                .check()
                .and_then(|_| writer.write_all(data).map_err(Into::into))
            {
                failure = Some(e);
                false
            } else {
                if data.len() >= hash_length {
                    trailer.clear();
                    trailer.extend_from_slice(&data[data.len() - hash_length..]);
                } else {
                    let excess = (trailer.len() + data.len()).saturating_sub(hash_length);
                    trailer.drain(..excess);
                    trailer.extend_from_slice(data);
                }
                true
            }
        })
        .map_err(|e| failure.take().unwrap_or_else(|| e.into()))?;
        // Protect an installed but not-yet-referenced pack from concurrent GC.
        let pack_dir = repo.commondir().join("objects/pack");
        fs::create_dir_all(&pack_dir)?;
        ensure!(
            trailer.len() == hash_length,
            "Missing migrated pack checksum"
        );
        let pack_id: String = trailer.iter().map(|b| format!("{b:02x}")).collect();
        let keep = pack_dir.join(format!("pack-{pack_id}.keep"));
        let own_keep = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&keep)
        {
            Ok(mut file) => {
                file.write_all(b"GitBuddy LFS migration candidate\n")?;
                file.sync_all()?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e.into()),
        };
        fs::File::open(&pack_dir)?.sync_all()?;
        writer.commit()?;
        for e in fs::read_dir(&pack_dir)? {
            let e = e?;
            if e.file_type()?.is_file() {
                fs::File::open(e.path())?.sync_all()?;
            }
        }
        fs::File::open(&pack_dir)?.sync_all()?;
        let (original, candidate) = refs(&state);
        pin(&repo, &original, &state.original)?;
        pin(&repo, &candidate, &state.target)?;
        if own_keep {
            fs::remove_file(keep)?;
            fs::File::open(&pack_dir)?.sync_all()?;
        }
        for (source, pointer) in
            required_objects(plan.workspace.path(), &cloned, &plan.target, &control)?
        {
            control.check()?;
            let destination = lfs::object(&repo, &pointer)?;
            if destination.exists() {
                let actual = lfs::hash(&destination)?;
                ensure!(
                    actual.oid == pointer.oid && actual.size == pointer.size,
                    "Existing canonical LFS object is corrupt"
                );
                continue;
            }
            fs::create_dir_all(destination.parent().unwrap())?;
            lfs::object(&repo, &pointer)?;
            let mut temp = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
            std::io::copy(&mut fs::File::open(source)?, &mut temp)?;
            temp.as_file().sync_all()?;
            let actual = lfs::hash(temp.path())?;
            ensure!(
                actual.oid == pointer.oid && actual.size == pointer.size,
                "LFS object changed during import"
            );
            temp.persist_noclobber(&destination)?;
            fs::File::open(destination.parent().unwrap())?.sync_all()?;
        }
        ensure_source(&repo, &plan.state)?;
        copy_entries(
            &state.owner,
            &dir.path().join("original/worktree"),
            &state.before,
        )?;
        atomic(&dir.path().join("original/index"), &index_bytes(&repo)?)?;
        ensure_source(&repo, &state)?;
        control.cancellation.finish()?;
        fs::File::open(dir.path())?.sync_all()?;
        fs::File::open(&base)?.sync_all()?;
        let _kept = dir.keep();
        save(&repo, &state)?;
        self.finish_lfs_migration(state, &control)
    }
    fn finish_lfs_migration(&self, mut state: State, control: &NetworkControl) -> Result<String> {
        let repo = raw(self)?;
        let other_repositories = other_worktrees(&repo)?;
        let _other_head_locks = lock_other_heads(&other_repositories)?;
        verify_owner(&repo, &state)?;
        let mut transaction = repo.transaction()?;
        transaction.lock_ref("HEAD")?;
        transaction.lock_ref(&state.branch)?;
        verify_owner(&repo, &state)?;
        if state.phase == Phase::Prepared {
            ensure_source(&repo, &state)?;
            super::worktree_conversion::guard_native_history_write(
                &repo,
                Some(oid(&state.target)?),
            )?;
            state.phase = Phase::CheckingOut;
            save(&repo, &state)?;
            control.phase("LFS migration: checkout pending");
            lfs::dehydrate(&repo)?;
            let mut checkout = CheckoutBuilder::new();
            checkout
                .safe()
                .disable_filters(true)
                .overwrite_ignored(false)
                .remove_untracked(false)
                .remove_ignored(false);
            repo.checkout_tree(
                repo.find_commit(oid(&state.target)?)?.as_object(),
                Some(&mut checkout),
            )?;
            lfs::hydrate(&repo)?;
            ensure!(
                !super::libgit::file_changes(&repo)?
                    .iter()
                    .any(|f| f.unstaged()),
                "Migration checkout is incomplete; use Abort after preserving work"
            );
            state.after = manifest(&state.owner, state.before.iter().map(|e| e.path.clone()))?;
            state.after_index = digest(&index_bytes(&repo)?);
            state.phase = Phase::CheckedOut;
            save(&repo, &state)?;
            control.phase("LFS migration: checkout verified");
        }
        ensure!(
            matches!(state.phase, Phase::CheckedOut | Phase::Published),
            "Partial checkout cannot be verified; use Abort migration"
        );
        ensure!(
            config_hash(&repo)? == state.config,
            "Migration configuration changed; preserve edits and use Abort migration"
        );
        ensure!(
            digest(&index_bytes(&repo)?) == state.after_index,
            "Migration index changed; preserve edits and use Abort migration"
        );
        ensure!(
            manifest(&state.owner, state.after.iter().map(|e| e.path.clone()))? == state.after,
            "Migration working files changed; preserve edits and use Abort migration"
        );
        let mut index = repo.index()?;
        ensure!(
            index.write_tree()? == repo.find_commit(oid(&state.target)?)?.tree_id(),
            "Migration index does not match the candidate"
        );
        verify_owner(&repo, &state)?;
        if head(&repo)?.1 == state.original {
            let signature = git2::Signature::now("GitBuddy", "gitbuddy@localhost")?;
            repo.reference_ensure_log("HEAD")?;
            repo.reference_ensure_log(&state.branch)?;
            transaction.set_target(
                &state.branch,
                oid(&state.target)?,
                Some(&signature),
                "gitbuddy: migrate current branch to LFS",
            )?;
            transaction.commit()?;
            sync_reference(&repo, &state.branch)?;
            control.phase("LFS migration: branch published");
        }
        state.phase = Phase::Published;
        save(&repo, &state)?;
        remove_journal(&repo)?;
        Ok(format!(
            "LFS migration complete; original history and recovery files retained at {}",
            directory(&repo, &state)?.display()
        ))
    }
    pub(super) fn continue_lfs_migration(&self, expected: &str) -> Result<String> {
        let repo = raw(self)?;
        if !pending(&repo) {
            return Ok("No LFS migration remains in progress".into());
        }
        let (state, current) = load(&repo)?;
        ensure!(current == expected, "Migration recovery changed; reopen it");
        self.finish_lfs_migration(state, &NetworkControl::default())
    }
    pub(super) fn abort_lfs_migration(
        &self,
        expected: &str,
        control: &NetworkControl,
    ) -> Result<String> {
        let repo = raw(self)?;
        if !pending(&repo) {
            return Ok("No LFS migration remains in progress".into());
        }
        let (mut state, current) = load(&repo)?;
        ensure!(current == expected, "Migration recovery changed; reopen it");
        verify_owner(&repo, &state)?;
        let other_repositories = other_worktrees(&repo)?;
        let _other_head_locks = lock_other_heads(&other_repositories)?;
        let mut transaction = repo.transaction()?;
        transaction.lock_ref("HEAD")?;
        transaction.lock_ref(&state.branch)?;
        verify_owner(&repo, &state)?;
        let index_lock = AbortIndexLock::acquire(&repo, &state)?;
        let dir = directory(&repo, &state)?;
        let original = dir.join("original/worktree");
        ensure!(
            manifest(&original, state.before.iter().map(|e| e.path.clone()))? == state.before,
            "Original recovery files changed; refusing restoration"
        );
        let saved_index = fs::read(dir.join("original/index"))?;
        ensure!(
            digest(&saved_index) == state.index,
            "Original recovery index changed"
        );
        let backup = tempfile::Builder::new()
            .prefix("aborted-")
            .tempdir_in(&dir)?;
        let partial = manifest(&state.owner, state.before.iter().map(|e| e.path.clone()))?;
        let partial_index = index_bytes(&repo)?;
        copy_entries(&state.owner, &backup.path().join("worktree"), &partial)?;
        atomic(&backup.path().join("index"), &partial_index)?;
        atomic(
            &backup.path().join("journal.json"),
            &fs::read(repo.commondir().join(JOURNAL))?,
        )?;
        atomic(
            &backup.path().join("head.json"),
            &serde_json::to_vec(&head(&repo)?)?,
        )?;
        let backup = backup.keep();
        fs::File::open(&backup)?.sync_all()?;
        fs::File::open(&dir)?.sync_all()?;
        state.phase = Phase::Aborting;
        save(&repo, &state)?;
        for (e, preserved) in state.before.iter().zip(&partial) {
            control.phase(&format!("LFS migration: restoring {}", e.path));
            let destination = checked_path(&state.owner, &e.path)?;
            if let Ok(meta) = fs::symlink_metadata(&destination) {
                ensure!(
                    !meta.is_dir(),
                    "Preserve the directory at {} manually before retrying Abort",
                    destination.display()
                );
            }
            if e.kind == "missing" {
                unchanged_destination(&state.owner, preserved)?;
                if fs::symlink_metadata(&destination).is_ok() {
                    fs::remove_file(&destination)?;
                    fs::File::open(destination.parent().unwrap())?.sync_all()?;
                }
                continue;
            }
            fs::create_dir_all(destination.parent().unwrap())?;
            if e.kind == "symlink" {
                #[cfg(unix)]
                {
                    let temporary = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
                    let path = temporary.into_temp_path();
                    fs::remove_file(&path)?;
                    std::os::unix::fs::symlink(
                        fs::read_link(checked_path(&original, &e.path)?)?,
                        &path,
                    )?;
                    unchanged_destination(&state.owner, preserved)?;
                    fs::rename(&path, &destination)?;
                }
                #[cfg(not(unix))]
                bail!("Symlink recovery requires Unix");
            } else {
                let mut temporary = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
                std::io::copy(
                    &mut fs::File::open(checked_path(&original, &e.path)?)?,
                    &mut temporary,
                )?;
                ensure!(
                    file_hash(temporary.path())? == e.hash,
                    "Original recovery content changed during restoration"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    temporary
                        .as_file()
                        .set_permissions(fs::Permissions::from_mode(e.mode))?;
                }
                temporary.as_file().sync_all()?;
                unchanged_destination(&state.owner, preserved)?;
                temporary.persist(&destination)?;
            }
            fs::File::open(destination.parent().unwrap())?.sync_all()?;
            control.phase(&format!("LFS migration: restored {}", e.path));
        }
        ensure!(
            manifest(&state.owner, state.before.iter().map(|e| e.path.clone()))? == state.before,
            "Restoration is incomplete; recovery journal retained"
        );
        index_lock.verify()?;
        ensure!(
            index_bytes(&repo)? == partial_index,
            "The index changed after Abort preserved it; the newer index was kept. Reopen Abort before retrying"
        );
        verify_owner(&repo, &state)?;
        atomic(&repo.path().join("index"), &saved_index)?;
        transaction.set_target(
            &state.branch,
            oid(&state.original)?,
            Some(&git2::Signature::now("GitBuddy", "gitbuddy@localhost")?),
            "gitbuddy: abort LFS migration",
        )?;
        transaction.commit()?;
        sync_reference(&repo, &state.branch)?;
        remove_journal(&repo)?;
        Ok(format!(
            "Original history and worktree restored; interrupted files retained at {}",
            backup.display()
        ))
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;

    #[test]
    fn nested_recovery_copies_sync_each_created_ancestor_children_first() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        fs::create_dir_all(source.join("a/b")).unwrap();
        fs::write(
            source.join("a/b/file"),
            "must survive before destructive recovery",
        )
        .unwrap();
        let entries = vec![entry(&source, "a/b/file").unwrap()];
        let destination = fixture.path().join("backup/original/worktree");
        let mut synced = Vec::new();
        copy_entries_with_sync(&source, &destination, &entries, |path| {
            fs::File::open(path)?.sync_all()?;
            synced.push(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        let required = [
            destination.join("a/b"),
            destination.join("a"),
            destination.clone(),
            fixture.path().join("backup/original"),
            fixture.path().join("backup"),
            fixture.path().to_path_buf(),
        ];
        let positions: Vec<_> = required
            .iter()
            .map(|path| {
                synced
                    .iter()
                    .position(|synced| synced == path)
                    .unwrap_or_else(|| {
                        panic!(
                            "Created backup ancestor was never synced: {}",
                            path.display()
                        )
                    })
            })
            .collect();
        assert!(positions.windows(2).all(|p| p[0] < p[1]));
    }

    #[test]
    fn an_intermediate_backup_directory_sync_failure_stops_preservation() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        fs::create_dir_all(source.join("a/b")).unwrap();
        fs::write(source.join("a/b/file"), "preserve me").unwrap();
        let entries = vec![entry(&source, "a/b/file").unwrap()];
        let destination = fixture.path().join("backup/worktree");
        let result = copy_entries_with_sync(&source, &destination, &entries, |path| {
            ensure!(
                path != destination.join("a"),
                "injected intermediate directory fsync failure"
            );
            fs::File::open(path)?.sync_all()?;
            Ok(())
        });
        assert!(
            result.is_err(),
            "Missing ancestor fsync must prevent destructive recovery"
        );
    }
}
