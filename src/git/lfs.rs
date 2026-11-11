//! Native standard LFS pointers; official git-lfs handles extensions and transport.
#[path = "lfs/attributes.rs"]
mod attributes;
use super::libgit::raw;
use super::{NetworkControl, Repository};
use anyhow::{Context, Result, ensure};
pub(in crate::git) use attributes::DirectoryView as AttributeDirectoryView;
pub(in crate::git) use attributes::{
    AttributeQuery, AttributeState, Lookup as AttributeLookup, query as query_attributes,
    query_from_tree as query_attributes_from_tree,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const VERSION: &str = "version https://git-lfs.github.com/spec/v1";
const LFS_ROUTING_ENV: [&str; 9] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_SUPER_PREFIX",
];
#[derive(Clone, Debug)]
pub struct LfsFile {
    pub path: PathBuf,
    pub oid: String,
    pub size: u64,
    pub available: bool,
    pub hydrated: bool,
}
#[derive(Clone, Debug, Default)]
pub struct LfsStatus {
    pub tool: Option<String>,
    pub patterns: Vec<String>,
    pub files: Vec<LfsFile>,
    pub auto_fetch: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pointer {
    pub(super) oid: String,
    pub(super) size: u64,
    extensions: Vec<Extension>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Extension {
    priority: u8,
    name: String,
    oid: String,
}
fn valid_oid(oid: &str) -> bool {
    oid.len() == 64
        && oid
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
impl Pointer {
    pub(super) fn parse_checked(bytes: &[u8]) -> Result<Option<Self>> {
        let pointer = Self::parse(bytes);
        ensure!(
            pointer.is_some() || !bytes.starts_with(VERSION.as_bytes()),
            "Unsupported or malformed LFS pointer"
        );
        Ok(pointer)
    }
    pub(super) fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > 1024 {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        if lines.next()? != VERSION {
            return None;
        }
        let mut extensions: Vec<Extension> = Vec::new();
        let mut line = lines.next()?;
        while let Some(extension) = line.strip_prefix("ext-") {
            let (key, oid) = extension.split_once(" sha256:")?;
            let (priority, name) = key.split_once('-')?;
            if priority.len() != 1
                || !priority.as_bytes()[0].is_ascii_digit()
                || name.is_empty()
                || !(name.as_bytes()[0].is_ascii_alphanumeric() || name.as_bytes()[0] == b'_')
                || name.chars().any(|c| c.is_whitespace() || c.is_control())
                || !valid_oid(oid)
            {
                return None;
            }
            let priority = priority.parse().ok()?;
            if extensions.iter().any(|ext| ext.name == name)
                || extensions
                    .last()
                    .is_some_and(|ext| ext.priority >= priority)
            {
                return None;
            }
            extensions.push(Extension {
                priority,
                name: name.into(),
                oid: oid.into(),
            });
            line = lines.next()?;
        }
        let oid = line.strip_prefix("oid sha256:")?;
        if !valid_oid(oid) {
            return None;
        }
        let size_text = lines.next()?.strip_prefix("size ")?;
        if size_text.is_empty() || !size_text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let size = size_text.parse().ok()?;
        if lines.next().is_some() {
            return None;
        }
        Some(Self {
            oid: oid.into(),
            size,
            extensions,
        })
    }
    fn text(&self) -> String {
        let mut text = format!("{VERSION}\n");
        for extension in &self.extensions {
            text.push_str(&format!(
                "ext-{}-{} sha256:{}\n",
                extension.priority, extension.name, extension.oid
            ));
        }
        text.push_str(&format!("oid sha256:{}\nsize {}\n", self.oid, self.size));
        text
    }
    fn matches_canonical(&self, actual: &Self) -> bool {
        self.oid == actual.oid && self.size == actual.size
    }
    fn matches_original(&self, actual: &Self) -> bool {
        match self.extensions.first() {
            Some(extension) => extension.oid == actual.oid,
            None => self.matches_canonical(actual),
        }
    }
}
pub(super) fn hash(path: &Path) -> Result<Pointer> {
    hash_with_control(path, None)
}
pub(super) fn hash_with_control(path: &Path, control: Option<&NetworkControl>) -> Result<Pointer> {
    let mut file = fs::File::open(path)?;
    let before = file.metadata()?;
    let mut digest = Sha256::new();
    let mut size = 0;
    let mut buffer = [0u8; 65536];
    loop {
        if let Some(control) = control {
            control.check()?;
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        size += count as u64;
    }
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "LFS content changed while hashing; retry inspection"
    );
    Ok(Pointer {
        oid: format!("{:x}", digest.finalize()),
        size,
        extensions: Vec::new(),
    })
}
pub(super) fn storage(repo: &git2::Repository) -> Result<PathBuf> {
    let root = match repo.config()?.get_path("lfs.storage") {
        Err(error) if error.code() == git2::ErrorCode::NotFound => repo.commondir().join("lfs"),
        Err(error) => return Err(error.into()),
        Ok(path) if path.as_os_str().is_empty() => repo.commondir().join("lfs"),
        Ok(path) if path.is_absolute() => path,
        Ok(path) => repo.commondir().join(path),
    };
    for path in [root.clone(), root.join("objects")] {
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "LFS cache must not traverse a symlink"
            );
        }
    }
    Ok(root.join("objects"))
}
pub(super) fn object(repo: &git2::Repository, pointer: &Pointer) -> Result<PathBuf> {
    let path = storage(repo)?
        .join(&pointer.oid[..2])
        .join(&pointer.oid[2..4])
        .join(&pointer.oid);
    for part in [
        path.parent().unwrap().parent().unwrap(),
        path.parent().unwrap(),
        path.as_path(),
    ] {
        if let Ok(metadata) = fs::symlink_metadata(part) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "LFS object path must not traverse a symlink"
            );
        }
    }
    Ok(path)
}
pub(super) fn safe_path(repo: &git2::Repository, path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "Invalid LFS path"
    );
    let root = repo.workdir().context("LFS needs a working tree")?;
    let mut cursor = root.to_path_buf();
    for part in path.components() {
        cursor.push(part);
        if let Ok(meta) = fs::symlink_metadata(&cursor) {
            ensure!(
                !meta.file_type().is_symlink(),
                "LFS does not follow symlinks"
            );
        }
    }
    Ok(cursor)
}
pub(super) fn tracked(repo: &git2::Repository, path: &Path) -> Result<bool> {
    attributes::is_lfs(repo, path, attributes::Lookup::WorktreeThenIndex)
}
fn index_pointer(repo: &git2::Repository, entry: &git2::IndexEntry) -> Result<Option<Pointer>> {
    if (entry.flags >> 12) & 3 != 0 {
        return Ok(None);
    }
    entry_pointer(repo, entry)
}
fn entry_pointer(repo: &git2::Repository, entry: &git2::IndexEntry) -> Result<Option<Pointer>> {
    if entry.mode != 0o100644 && entry.mode != 0o100755 {
        return Ok(None);
    }
    if repo.odb()?.read_header(entry.id)?.0 > 1024 {
        return Ok(None);
    }
    let blob = repo.find_blob(entry.id)?;
    Ok(Pointer::parse(blob.content()))
}
pub(super) fn matches_index(repo: &git2::Repository, path: &Path) -> Result<bool> {
    let index = repo.index()?;
    let Some(entry) = index.get_path(path, 0) else {
        return Ok(false);
    };
    let Some(pointer) = index_pointer(repo, &entry)? else {
        return Ok(false);
    };
    let file = safe_path(repo, path)?;
    if !file.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if repo.config()?.get_bool("core.filemode").unwrap_or(true)
            && ((fs::metadata(&file)?.permissions().mode() & 0o111 != 0)
                != (entry.mode == 0o100755))
        {
            return Ok(false);
        }
    }
    if fs::metadata(&file)?.len() <= 1024
        && Pointer::parse(&fs::read(&file)?).as_ref() == Some(&pointer)
    {
        return Ok(true);
    }
    Ok(pointer.matches_original(&hash(&file)?))
}
/// Clean through one implementation for ordinary staging and conflict results.
fn clean_reader(
    repo: &git2::Repository,
    mut source: impl Read,
    path: Option<&Path>,
    max_bytes: Option<u64>,
) -> Result<Vec<u8>> {
    let mut probe = Vec::new();
    source.by_ref().take(1025).read_to_end(&mut probe)?;
    let pointer = Pointer::parse_checked(&probe)?;
    let pointer = if let Some(pointer) = pointer {
        pointer
    } else if configured_extensions(repo)? || !indexed_extensions(repo, path)?.is_empty() {
        let expected = indexed_extensions(repo, path)?;
        if expected.is_empty() {
            require_extension_processors(repo, None)?;
        } else {
            for pointer in &expected {
                require_extension_processors(repo, Some(pointer))?;
            }
        }
        let limit = filter_limit(repo)?.min(max_bytes.unwrap_or(u64::MAX));
        let mut input = tempfile::NamedTempFile::new_in(repo.commondir())?;
        let copied = std::io::copy(
            &mut std::io::Cursor::new(probe).chain(source).take(limit + 1),
            &mut input,
        )?;
        ensure!(
            copied <= limit,
            "LFS extension input exceeds gitbuddy.lfsFilterMaxBytes"
        );
        let original = hash(input.path())?;
        let isolated = tempfile::tempdir_in(repo.commondir())?;
        let output = run_filter(
            repo,
            "clean",
            path,
            input.path(),
            isolated.path(),
            1024,
            limit,
        )?;
        let pointer = Pointer::parse(&fs::read(output.path())?)
            .context("git-lfs clean did not return a supported SHA-256 pointer")?;
        ensure!(
            pointer.size <= limit,
            "LFS canonical object exceeds the caller byte limit"
        );
        ensure!(
            pointer.matches_original(&original),
            "LFS extension clean output does not describe the original content"
        );
        let canonical = isolated_object(isolated.path(), &pointer);
        ensure!(
            pointer.matches_canonical(&hash(&canonical)?),
            "LFS extension produced a corrupt canonical object"
        );
        import_object(repo, &canonical, &pointer)?;
        pointer
    } else {
        let mut temp = tempfile::NamedTempFile::new_in(repo.commondir())?;
        let mut input = std::io::Cursor::new(probe).chain(source);
        let mut digest = Sha256::new();
        let mut size = 0;
        let mut buffer = [0u8; 65536];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            temp.write_all(&buffer[..count])?;
            digest.update(&buffer[..count]);
            size += count as u64;
        }
        let pointer = Pointer {
            oid: format!("{:x}", digest.finalize()),
            size,
            extensions: Vec::new(),
        };
        let target = object(repo, &pointer)?;
        fs::create_dir_all(target.parent().context("No object directory")?)?;
        temp.as_file().sync_all()?;
        if target.exists() {
            ensure!(
                pointer.matches_canonical(&hash(&target)?),
                "Cached LFS object is corrupt"
            );
        } else {
            temp.persist(&target)?;
        }
        pointer
    };
    if let Some(limit) = max_bytes {
        ensure!(
            pointer.size <= limit,
            "LFS canonical object exceeds the caller byte limit"
        );
    }
    Ok(pointer.text().into_bytes())
}

pub(super) fn clean_content_at(
    repo: &git2::Repository,
    bytes: &[u8],
    path: &Path,
) -> Result<Vec<u8>> {
    clean_reader(repo, std::io::Cursor::new(bytes), Some(path), None)
}

pub(super) fn clean_file_content_at(repo: &git2::Repository, path: &Path) -> Result<Vec<u8>> {
    let absolute = safe_path(repo, path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut source = options.open(&absolute)?;
    let metadata = source.metadata()?;
    ensure!(metadata.is_file(), "LFS conversion requires a regular file");
    let stamp = super::conversion::FileStamp::new(&metadata);
    let before = conversion_fingerprint(repo)?;
    let bytes = clean_reader(repo, &mut source, Some(path), None)?;
    ensure!(
        stamp == super::conversion::FileStamp::new(&source.metadata()?),
        "LFS input changed during conversion"
    );
    stamp.validate(repo, path)?;
    ensure!(
        before == conversion_fingerprint(repo)?,
        "LFS configuration changed during conversion"
    );
    Ok(bytes)
}

pub(super) fn clean_content_at_bounded(
    repo: &git2::Repository,
    bytes: &[u8],
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    ensure!(
        max_bytes > 0 && bytes.len() as u64 <= max_bytes,
        "LFS input exceeds the caller byte limit"
    );
    let fresh = git2::Repository::open(repo.path())?;
    safe_path(&fresh, path)?;
    let before = conversion_fingerprint(&fresh)?;
    let pointer = clean_reader(
        &fresh,
        std::io::Cursor::new(bytes),
        Some(path),
        Some(max_bytes),
    )?;
    ensure!(
        before == conversion_fingerprint(&fresh)?,
        "LFS conversion configuration changed; retry"
    );
    Ok(pointer)
}

pub(super) fn local_smudge_content_at(
    repo: &git2::Repository,
    pointer_bytes: &[u8],
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    ensure!(
        max_bytes > 0 && max_bytes < u64::MAX && max_bytes <= usize::MAX as u64,
        "Invalid LFS output byte limit"
    );
    let pointer = Pointer::parse(pointer_bytes).context("Unsupported or malformed LFS pointer")?;
    let fresh = git2::Repository::open(repo.path())?;
    safe_path(&fresh, path)?;
    let before = conversion_fingerprint(&fresh)?;
    let temporary = smudge_cached_file(
        &fresh,
        &pointer,
        path,
        max_bytes,
        filter_limit(&fresh)?,
        fresh.commondir(),
    )?;
    let mut bytes = Vec::new();
    fs::File::open(temporary.path())?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max_bytes,
        "LFS output exceeds the caller byte limit"
    );
    ensure!(
        before == conversion_fingerprint(&fresh)?,
        "LFS conversion configuration changed; retry"
    );
    Ok(bytes)
}

pub(super) fn validate_extension_configuration(repo: &git2::Repository) -> Result<()> {
    let fresh = git2::Repository::open(repo.path())?;
    validate_repository_format(&fresh.config()?)?;
    if configured_extensions(&fresh)? {
        require_extension_processors(&fresh, None)?;
    }
    Ok(())
}

/// Preflight a future checkout without materializing an unbounded Vec or
/// changing working files. Missing objects intentionally remain placeholders.
pub(super) fn validate_local_checkout(
    repo: &git2::Repository,
    bytes: &[u8],
    path: &Path,
) -> Result<bool> {
    Ok(prepare_checkout_payload(repo, path, bytes, repo.commondir())?.is_some())
}

/// Prepare an owned working payload on disk, without modifying the worktree
/// or fetching. The caller can pin it in a recovery journal without rerunning
/// the filter. Standard LFS objects are streamed without the patch byte cap.
pub(super) fn prepare_checkout_payload(
    repo: &git2::Repository,
    path: &Path,
    canonical: &[u8],
    temporary_directory: &Path,
) -> Result<Option<tempfile::NamedTempFile>> {
    let fresh = git2::Repository::open(repo.path())?;
    safe_path(&fresh, path)?;
    let pointer = Pointer::parse_checked(canonical)?.context("Expected an LFS pointer")?;
    let before = conversion_fingerprint(&fresh)?;
    let cached = object(&fresh, &pointer)?;
    match fs::symlink_metadata(cached) {
        Ok(meta) => ensure!(meta.file_type().is_file(), "Invalid LFS cache object type"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let limit = if pointer.extensions.is_empty() {
        u64::MAX - 1
    } else {
        filter_limit(&fresh)?
    };
    let payload = smudge_cached_file(&fresh, &pointer, path, limit, limit, temporary_directory)?;
    ensure!(
        before == conversion_fingerprint(&fresh)?,
        "LFS conversion configuration changed; retry"
    );
    Ok(Some(payload))
}

pub(super) fn conversion_fingerprint(repo: &git2::Repository) -> Result<[u8; 32]> {
    fn field(digest: &mut Sha256, label: &[u8], value: &[u8]) {
        digest.update((label.len() as u64).to_le_bytes());
        digest.update(label);
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value);
    }
    let fresh = git2::Repository::open(repo.path())?;
    let mut config = fresh.config()?;
    let config = config.snapshot()?;
    validate_repository_format(&config)?;
    let mut digest = Sha256::new();
    field(&mut digest, b"domain", b"gitbuddy.lfs-conversion.v1");
    field(&mut digest, b"repository-format", b"0");
    for path in [fresh.path(), fresh.commondir()] {
        field(&mut digest, b"repository", &super::libgit::path_bytes(path));
    }
    if let Some(path) = fresh.workdir() {
        field(&mut digest, b"worktree", &super::libgit::path_bytes(path));
    }
    let objects = storage(&fresh)?;
    field(
        &mut digest,
        b"storage-route",
        &attributes::route_fingerprint(&objects)?,
    );
    let mut names = std::collections::BTreeSet::new();
    let mut entries = config.entries(None)?;
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let name = entry.name()?;
        if name != "lfs.repositoryformatversion"
            && (name.starts_with("lfs.")
                || name.starts_with("filter.lfs.")
                || matches!(name, "gitbuddy.lfsprogram" | "gitbuddy.lfsfiltermaxbytes"))
        {
            names.insert(name.to_string());
        }
    }
    for name in names {
        let entry = config.get_entry(&name)?;
        field(&mut digest, b"config-name", name.as_bytes());
        field(
            &mut digest,
            b"config-has-value",
            &[u8::from(entry.has_value())],
        );
        if entry.has_value() {
            field(&mut digest, b"config-value", entry.value_bytes());
        }
        field(
            &mut digest,
            b"config-level",
            &(entry.level() as i32).to_le_bytes(),
        );
    }
    // User-configured processors can consume inherited environment variables.
    // Pin their effective inherited inputs, excluding only routing and control
    // values that this engine always replaces/removes for tool children.
    let environment = std::env::vars_os().collect::<std::collections::BTreeMap<_, _>>();
    for (name, value) in environment {
        if name.to_str().is_some_and(|name| {
            LFS_ROUTING_ENV.contains(&name)
                || matches!(
                    name,
                    "GIT_TERMINAL_PROMPT" | "GIT_LFS_SKIP_SMUDGE" | "GIT_LFS_SKIP_DOWNLOAD_ERRORS"
                )
        }) {
            continue;
        }
        field(
            &mut digest,
            b"environment-name",
            &super::libgit::path_bytes(Path::new(&name)),
        );
        field(
            &mut digest,
            b"environment-value",
            &super::libgit::path_bytes(Path::new(&value)),
        );
    }
    Ok(digest.finalize().into())
}

fn validate_repository_format(config: &git2::Config) -> Result<()> {
    let version = match config.get_i64("lfs.repositoryformatversion") {
        Ok(version) => version,
        Err(error) if error.code() == git2::ErrorCode::NotFound => 0,
        Err(error) => return Err(error).context("Invalid LFS repository format version"),
    };
    ensure!(
        version == 0,
        "Unsupported LFS repository format version {version}"
    );
    Ok(())
}

fn private_filter_metadata(
    repo: &git2::Repository,
) -> Result<(tempfile::TempDir, git2::Repository)> {
    let fresh = git2::Repository::open(repo.path())?;
    let mut source = fresh.config()?;
    let source = source.snapshot()?;
    validate_repository_format(&source)?;
    let directory = tempfile::tempdir_in(repo.commondir())?;
    let isolated = git2::Repository::init_bare(directory.path())?;
    let mut config = isolated.config()?;
    config.set_bool("core.bare", false)?;
    config.set_i64("lfs.repositoryformatversion", 0)?;
    let mut names = std::collections::BTreeSet::new();
    let mut entries = source.entries(Some("^lfs\\.extension\\."))?;
    while let Some(entry) = entries.next() {
        names.insert(entry?.name()?.to_string());
    }
    for name in names {
        config.set_str(&name, source.get_entry(&name)?.value()?)?;
    }
    // Do not copy includes, credential settings, HEAD, refs or the source index.
    // Processors retain their source cwd/relative paths, but Git metadata writes
    // and git-lfs's first-use format initialization remain private. Processors
    // requiring source HEAD/index metadata are unsupported in this context.
    Ok((directory, isolated))
}

fn indexed_extensions(repo: &git2::Repository, path: Option<&Path>) -> Result<Vec<Pointer>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let index = repo.index()?;
    let mut pointers = Vec::new();
    // Conflict resolution has no stage-zero entry. Each side can require a
    // different processor, so all conflict stages must retain their guard.
    for stage in 0..=3 {
        if let Some(entry) = index.get_path(path, stage)
            && let Some(pointer) = entry_pointer(repo, &entry)?
            && !pointer.extensions.is_empty()
        {
            pointers.push(pointer);
        }
    }
    Ok(pointers)
}

fn configured_extensions(repo: &git2::Repository) -> Result<bool> {
    let config = repo.config()?;
    let mut entries = config.entries(Some("^lfs\\.extension\\."))?;
    Ok(entries.next().transpose()?.is_some())
}

fn require_extension_processors(repo: &git2::Repository, pointer: Option<&Pointer>) -> Result<()> {
    let config = repo.config()?;
    let mut names = std::collections::BTreeSet::new();
    let mut entries = config.entries(Some("^lfs\\.extension\\."))?;
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let key = entry
            .name()
            .context("LFS extension config name is not UTF-8")?;
        if let Some((name, _)) = key
            .strip_prefix("lfs.extension.")
            .and_then(|s| s.rsplit_once('.'))
        {
            names.insert(name.to_string());
        }
    }
    if let Some(pointer) = pointer {
        names.extend(
            pointer
                .extensions
                .iter()
                .map(|extension| extension.name.clone()),
        );
    }
    ensure!(
        !names.is_empty(),
        "LFS extension processors are not configured"
    );
    for name in names {
        for operation in ["clean", "smudge"] {
            ensure!(
                config
                    .get_string(&format!("lfs.extension.{name}.{operation}"))
                    .is_ok_and(|s| !s.trim().is_empty()),
                "LFS extension {name} needs a configured {operation} processor; content was preserved"
            );
        }
    }
    Ok(())
}

// Applies only to delegated extension filters; native standard pointers retain
// their existing streaming behavior. The repo-local setting permits larger jobs.
fn filter_limit(repo: &git2::Repository) -> Result<u64> {
    let limit = match repo.config()?.get_i64("gitbuddy.lfsFilterMaxBytes") {
        Ok(value) => value,
        Err(error) if error.code() == git2::ErrorCode::NotFound => 64 * 1024 * 1024 * 1024,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        limit > 0 && limit < i64::MAX,
        "gitbuddy.lfsFilterMaxBytes must be a positive byte limit below i64::MAX"
    );
    Ok(limit as u64)
}

fn isolated_object(storage: &Path, pointer: &Pointer) -> PathBuf {
    storage
        .join("objects")
        .join(&pointer.oid[..2])
        .join(&pointer.oid[2..4])
        .join(&pointer.oid)
}

fn import_object(repo: &git2::Repository, source: &Path, pointer: &Pointer) -> Result<()> {
    let target = object(repo, pointer)?;
    fs::create_dir_all(target.parent().context("No object directory")?)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(target.parent().context("No object directory")?)?;
    std::io::copy(&mut fs::File::open(source)?, &mut temporary)?;
    ensure!(
        pointer.matches_canonical(&hash(temporary.path())?),
        "LFS canonical object changed during import"
    );
    temporary.as_file().sync_all()?;
    // Recheck existing directory components immediately before publication.
    object(repo, pointer)?;
    if target.exists() {
        ensure!(
            pointer.matches_canonical(&hash(&target)?),
            "Cached LFS object is corrupt"
        );
    } else if let Err(error) = temporary.persist_noclobber(&target) {
        ensure!(
            target.is_file(),
            "Cannot store canonical LFS object: {}",
            error.error
        );
        object(repo, pointer)?;
        ensure!(
            pointer.matches_canonical(&hash(&target)?),
            "Cached LFS object is corrupt"
        );
    }
    Ok(())
}

fn filter_storage_size(root: &Path, depth: usize) -> Result<u64> {
    ensure!(
        depth < 16,
        "LFS extension temporary cache exceeds directory limit"
    );
    let mut bytes = 0u64;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if depth > 0 && error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        let size = if kind.is_dir() {
            filter_storage_size(&entry.path(), depth + 1)?
        } else {
            ensure!(
                kind.is_file(),
                "LFS extension temporary cache contains an unsupported file"
            );
            // git-lfs atomically moves completed temporary objects into place.
            // A vanished intermediate file will be counted at its new path on
            // the next poll and in the final check after all writers exit.
            match entry.metadata() {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
        };
        bytes = bytes
            .checked_add(size)
            .context("LFS temporary cache size overflow")?;
    }
    Ok(bytes)
}

fn run_filter(
    repo: &git2::Repository,
    operation: &str,
    path: Option<&Path>,
    input: &Path,
    storage: &Path,
    stdout_limit: u64,
    storage_limit: u64,
) -> Result<tempfile::NamedTempFile> {
    let root = repo
        .workdir()
        .context("LFS extensions need a working tree")?;
    let output = tempfile::NamedTempFile::new_in(repo.commondir())?;
    let mut command = lfs_command(root, &lfs_program(root)?)?;
    let (_metadata_directory, metadata) = private_filter_metadata(repo)?;
    command
        .env("GIT_DIR", metadata.path())
        .env("GIT_WORK_TREE", root);
    command.arg(operation).arg("--");
    if let Some(path) = path {
        command.arg(path);
    }
    command
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_LFS_SKIP_SMUDGE")
        .env("GIT_LFS_SKIP_DOWNLOAD_ERRORS", "0")
        .stdin(fs::File::open(input)?)
        .stdout(output.as_file().try_clone()?)
        .stderr(Stdio::piped());
    // Append overrides, preserving any caller-provided Git config environment.
    let count = match std::env::var("GIT_CONFIG_COUNT") {
        Ok(value) => value.parse::<usize>().context("Invalid GIT_CONFIG_COUNT")?,
        Err(std::env::VarError::NotPresent) => 0,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        count <= 4096,
        "Git config environment exceeds LFS filter limit"
    );
    let settings = [
        ("lfs.storage", storage.as_os_str()),
        ("lfs.skipdownloaderrors", std::ffi::OsStr::new("false")),
        ("lfs.fetchinclude", std::ffi::OsStr::new("")),
        ("lfs.fetchexclude", std::ffi::OsStr::new("")),
    ];
    for (index, (key, value)) in settings.iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{}", count + index), key)
            .env(format!("GIT_CONFIG_VALUE_{}", count + index), value);
    }
    command.env("GIT_CONFIG_COUNT", (count + settings.len()).to_string());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = TransferChild {
        child: command.spawn().context(
            "LFS extensions require the official git-lfs executable; content was preserved",
        )?,
        cleaned: false,
    };
    let stderr = child
        .stderr
        .take()
        .context("Missing LFS filter error pipe")?;
    let reader = std::thread::spawn(move || drain(stderr));
    let limit = filter_limit(repo)?.min(storage_limit);
    let started = Instant::now();
    let result = (|| -> Result<()> {
        loop {
            ensure!(
                started.elapsed() < Duration::from_secs(300),
                "LFS extension filter exceeded the 300-second deadline; content was preserved"
            );
            ensure!(
                output.as_file().metadata()?.len() <= stdout_limit,
                "LFS extension output exceeds its byte limit; content was preserved"
            );
            ensure!(
                filter_storage_size(storage, 0)? <= limit,
                "LFS canonical object exceeds gitbuddy.lfsFilterMaxBytes; content was preserved"
            );
            if let Some(status) = child.try_wait()? {
                child.cleanup();
                ensure!(status.success(), "git-lfs {operation} failed");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    child.cleanup();
    let diagnostic = reader
        .join()
        .map_err(|_| anyhow::anyhow!("LFS filter error reader failed"))??;
    result.with_context(|| {
        format!(
            "LFS extension {operation}: {} (processors run with isolated Git metadata; source HEAD/index are unavailable)",
            String::from_utf8_lossy(&diagnostic)
        )
    })?;
    // Enforce limits once more after the final writer has exited.
    ensure!(
        output.as_file().metadata()?.len() <= stdout_limit,
        "LFS extension output exceeds its byte limit"
    );
    ensure!(
        filter_storage_size(storage, 0)? <= limit,
        "LFS canonical object exceeds gitbuddy.lfsFilterMaxBytes"
    );
    Ok(output)
}

pub(super) fn stage(repo: &git2::Repository, index: &mut git2::Index, path: &Path) -> Result<()> {
    use super::conversion::{CleanPolicy, FileStamp, Limits, Snapshot, WorkingFile};
    let absolute = repo.workdir().context("No working tree")?.join(path);
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if index.get_path(path, 0).is_some() {
                index.remove_path(path)?;
            }
            return Ok(());
        }
        Err(error) => return Err(error).context("Cannot inspect the working file before staging"),
    };
    // Git never applies content filters to symbolic links or gitlinks.
    let emulated_link = index
        .get_path(path, 0)
        .is_some_and(|entry| entry.mode == 0o120000)
        && repo
            .config()?
            .get_bool("core.symlinks")
            .is_ok_and(|enabled| !enabled);
    if metadata.file_type().is_symlink() || metadata.is_dir() || emulated_link {
        index.add_path(path)?;
        return Ok(());
    }
    ensure!(
        metadata.is_file(),
        "Staging requires a regular file, symlink, or gitlink"
    );
    let snapshot = Snapshot::capture(
        repo,
        &std::collections::BTreeSet::from([path.to_path_buf()]),
        AttributeLookup::WorktreeThenIndex,
    )?;
    let stamp = FileStamp::new(&metadata);
    if snapshot.try_native_stage(repo, index, path)? {
        stamp.validate(repo, path)?;
        return Ok(());
    }
    if snapshot.plain_lfs(path)? {
        let previous_mode = index.get_path(path, 0).map(|entry| entry.mode);
        stage_lfs_stream(repo, index, path)?;
        #[cfg(unix)]
        let actual_mode = {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            }
        };
        #[cfg(not(unix))]
        let actual_mode = 0o100644;
        let expected_mode = snapshot.canonical_mode(actual_mode, previous_mode);
        if let Some(mut entry) = index.get_path(path, 0)
            && entry.mode != expected_mode
        {
            entry.mode = expected_mode;
            index.add(&entry)?;
        }
        stamp.validate(repo, path)?;
        snapshot.validate(repo)?;
        return Ok(());
    }
    let working = WorkingFile::read(repo, path, Limits::ENCODED.max_bytes)?;
    let previous = index.get_path(path, 0);
    let indexed = match previous.as_ref() {
        Some(entry) if matches!(entry.mode, 0o100644 | 0o100755) => {
            ensure!(
                repo.odb()?.read_header(entry.id)?.0 <= Limits::ENCODED.max_bytes,
                "Indexed conversion base exceeds the managed resource limit"
            );
            Some(repo.find_blob(entry.id)?)
        }
        _ => None,
    };
    let canonical = snapshot.clean(
        repo,
        path,
        &working.bytes,
        indexed.as_ref().map_or(&[], |blob| blob.content()),
        CleanPolicy::Add,
        Limits::ENCODED,
    )?;
    working.validate(repo, path)?;
    snapshot.validate(repo)?;
    let entry = git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: snapshot.canonical_mode(working.actual_mode, previous.map(|entry| entry.mode)),
        uid: 0,
        gid: 0,
        file_size: 0,
        id: git2::Oid::ZERO_SHA1,
        flags: 0,
        flags_extended: 0,
        path: super::libgit::path_bytes(path),
    };
    index.add_frombuffer(&entry, &canonical)?;
    Ok(())
}

fn stage_lfs_stream(repo: &git2::Repository, index: &mut git2::Index, path: &Path) -> Result<()> {
    if !tracked(repo, path)? {
        let file = repo.workdir().context("No working tree")?.join(path);
        match fs::symlink_metadata(file) {
            Ok(_) => index.add_path(path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if index.get_path(path, 0).is_some() {
                    index.remove_path(path)?;
                }
            }
            Err(error) => {
                return Err(error).context("Cannot inspect the working file before staging");
            }
        }
        return Ok(());
    }
    let file = safe_path(repo, path)?;
    if !file.exists() {
        if index.get_path(path, 0).is_some() {
            index.remove_path(path)?;
        }
        return Ok(());
    }
    ensure!(file.is_file(), "An LFS path must be a regular file");
    let original = hash(&file)?;
    let text = clean_reader(repo, fs::File::open(&file)?, Some(path), None)?;
    safe_path(repo, path)?;
    ensure!(
        hash(&file)? == original,
        "LFS content changed while staging; retry"
    );
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&file)?.permissions().mode() & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        }
    };
    #[cfg(not(unix))]
    let mode = 0o100644;
    let entry = git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: text.len() as u32,
        id: git2::Oid::ZERO_SHA1,
        flags: 0,
        flags_extended: 0,
        path: super::libgit::path_bytes(path),
    };
    index.add_frombuffer(&entry, &text)?;
    Ok(())
}
fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("No parent")?)?;
    if let Ok(meta) = fs::metadata(path) {
        temp.as_file().set_permissions(meta.permissions())?;
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}
// libgit2 has no registered LFS filter here. Temporarily put clean files back
// into pointer form before its checkout safety checks, then hydrate afterwards.
pub(super) fn dehydrate(repo: &git2::Repository) -> Result<()> {
    dehydrate_paths(repo, &[])
}

fn selected_path(path: &Path, paths: &[PathBuf]) -> bool {
    paths.is_empty() || paths.iter().any(|selected| path.starts_with(selected))
}

pub(super) fn dehydrate_paths(repo: &git2::Repository, paths: &[PathBuf]) -> Result<()> {
    for entry in repo.index()?.iter() {
        let path = super::path_from_bytes(&entry.path);
        if selected_path(&path, paths)
            && tracked(repo, &path)?
            && let Some(pointer) = index_pointer(repo, &entry)?
            && matches_index(repo, &path)?
        {
            let file = safe_path(repo, &path)?;
            if fs::metadata(&file)?.len() > 1024 || Pointer::parse(&fs::read(&file)?).is_none() {
                // Reuse canonical objects independently of extension metadata.
                // Re-clean only when an external client removed the cache.
                let canonical = object(repo, &pointer)?;
                if canonical.is_file() {
                    ensure!(
                        pointer.matches_canonical(&hash(&canonical)?),
                        "Cached LFS object is corrupt"
                    );
                } else {
                    let mut index = repo.index()?;
                    stage(repo, &mut index, &path)?;
                    let entry = index.get_path(&path, 0).context("LFS entry disappeared")?;
                    ensure!(
                        index_pointer(repo, &entry)?.as_ref() == Some(&pointer),
                        "LFS extension configuration changed; restage before checkout"
                    );
                }
                ensure!(
                    matches_index(repo, &path)?,
                    "LFS content changed before checkout; retry"
                );
                replace(&file, pointer.text().as_bytes())?;
            }
        }
    }
    Ok(())
}
pub(super) fn hydrate(repo: &git2::Repository) -> Result<usize> {
    hydrate_paths(repo, &[])
}

pub(super) fn hydrate_paths(repo: &git2::Repository, paths: &[PathBuf]) -> Result<usize> {
    let mut missing = 0;
    let index = repo.index()?;
    for entry in index.iter() {
        let path = super::path_from_bytes(&entry.path);
        if !selected_path(&path, paths)
            || !attributes::is_lfs(repo, &path, attributes::Lookup::IndexThenWorktree)?
        {
            continue;
        }
        let Some(pointer) = index_pointer(repo, &entry)? else {
            continue;
        };
        let file = safe_path(repo, &path)?;
        if !file.is_file()
            || fs::metadata(&file)?.len() > 1024
            || Pointer::parse(&fs::read(&file)?).as_ref() != Some(&pointer)
        {
            continue;
        }
        missing += hydrate_file(repo, &file, &pointer)?;
    }
    // A stash can restore added / modified LFS files only to the working tree,
    // without restoring their index entries. Hydrate those exact pointers too.
    for changed in super::libgit::file_changes(repo)? {
        if !selected_path(&changed.path, paths)
            || changed.conflict()
            || !changed.unstaged()
            || !tracked(repo, &changed.path)?
        {
            continue;
        }
        let file = safe_path(repo, &changed.path)?;
        if file.is_file()
            && fs::metadata(&file)?.len() <= 1024
            && let Some(pointer) = Pointer::parse(&fs::read(&file)?)
        {
            missing += hydrate_file(repo, &file, &pointer)?;
        }
    }
    Ok(missing)
}
fn copy_cached(source: &Path, target: &mut impl Write, expected_size: u64) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(source)
        .context("LFS object is missing locally; fetch explicitly before conversion")?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink() && before.len() == expected_size,
        "Cached LFS object size/type does not match its pointer"
    );
    let count = std::io::copy(&mut Read::by_ref(&mut file).take(expected_size + 1), target)?;
    let after = file.metadata()?;
    ensure!(
        count == expected_size
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "Cached LFS object changed while reading; retry"
    );
    Ok(())
}

fn smudge_cached_file(
    repo: &git2::Repository,
    pointer: &Pointer,
    path: &Path,
    raw_limit: u64,
    canonical_limit: u64,
    temporary_directory: &Path,
) -> Result<tempfile::NamedTempFile> {
    let cached = object(repo, pointer)?;
    let metadata = fs::symlink_metadata(&cached)
        .context("LFS object is missing locally; fetch explicitly before conversion")?;
    // Check the actual size before hashing or copying. A corrupt huge object
    // must not turn a small declared pointer into unbounded conversion I/O.
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == pointer.size,
        "Cached LFS object size/type does not match its pointer"
    );
    ensure!(
        pointer.size <= canonical_limit,
        "LFS canonical object exceeds its configured byte limit"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(temporary_directory)?;
    if pointer.extensions.is_empty() {
        ensure!(
            pointer.size <= raw_limit,
            "LFS output exceeds the caller byte limit"
        );
        copy_cached(&cached, &mut temporary, pointer.size)?;
        ensure!(
            pointer.matches_canonical(&hash(temporary.path())?),
            "Cached LFS object is corrupt"
        );
    } else {
        require_extension_processors(repo, Some(pointer))?;
        let isolated = tempfile::tempdir_in(repo.commondir())?;
        let canonical = isolated_object(isolated.path(), pointer);
        fs::create_dir_all(canonical.parent().context("No object directory")?)?;
        copy_cached(&cached, &mut fs::File::create(&canonical)?, pointer.size)?;
        ensure!(
            pointer.matches_canonical(&hash(&canonical)?),
            "Cached LFS object is corrupt"
        );
        let mut input = tempfile::NamedTempFile::new_in(repo.commondir())?;
        input.write_all(pointer.text().as_bytes())?;
        let output = run_filter(
            repo,
            "smudge",
            Some(path),
            input.path(),
            isolated.path(),
            raw_limit,
            canonical_limit,
        )?;
        let count = std::io::copy(
            &mut fs::File::open(output.path())?.take(raw_limit + 1),
            &mut temporary,
        )?;
        ensure!(
            count <= raw_limit,
            "LFS output exceeds the caller byte limit"
        );
    }
    ensure!(
        pointer.matches_original(&hash(temporary.path())?),
        "LFS hydrated content does not match the original extension oid"
    );
    object(repo, pointer)?;
    Ok(temporary)
}

fn hydrate_file(repo: &git2::Repository, file: &Path, pointer: &Pointer) -> Result<usize> {
    let cached = object(repo, pointer)?;
    if !cached.is_file() {
        return Ok(1);
    }
    let path = file.strip_prefix(repo.workdir().context("No working tree")?)?;
    let before = fs::read(file)?;
    ensure!(
        Pointer::parse(&before).as_ref() == Some(pointer),
        "LFS working pointer changed; retry"
    );
    let limit = if pointer.extensions.is_empty() {
        u64::MAX - 1
    } else {
        filter_limit(repo)?
    };
    let temporary = smudge_cached_file(
        repo,
        pointer,
        path,
        limit,
        limit,
        file.parent().context("No parent")?,
    )?;
    temporary
        .as_file()
        .set_permissions(fs::metadata(file)?.permissions())?;
    temporary.as_file().sync_all()?;
    safe_path(repo, path)?;
    ensure!(
        fs::read(file)? == before,
        "LFS working pointer changed during hydration; content was preserved"
    );
    temporary.persist(file)?;
    Ok(0)
}
impl Repository {
    pub(super) fn auto_lfs_fetch(
        &self,
        remote: &str,
        revision: &str,
        control: &NetworkControl,
    ) -> Result<()> {
        let repo = raw(self)?;
        let enabled = match repo.config()?.get_bool("gitbuddy.lfsAutoFetch") {
            Ok(value) => value,
            Err(error) if error.code() == git2::ErrorCode::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !enabled {
            return Ok(());
        }
        if revision == "HEAD" && repo.head().is_err() {
            return Ok(());
        }
        let id = repo
            .revparse_single(revision)?
            .peel_to_commit()?
            .id()
            .to_string();
        control.cancellation.next_network_phase()?;
        control.phase("LFS: downloading objects for the fetched revision…");
        run_lfs(&self.root, &["fetch", remote, &id], control)?;
        control.cancellation.finish()?;
        hydrate(&repo)?;
        Ok(())
    }
    pub fn lfs_status(&self) -> Result<LfsStatus> {
        let repo = raw(self)?;
        let tool = lfs_command(&self.root, &lfs_program(&self.root)?)?
            .arg("version")
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let attrs = fs::read_to_string(self.root.join(".gitattributes")).unwrap_or_default();
        let patterns = attrs
            .lines()
            .filter(|l| l.split_whitespace().any(|w| w == "filter=lfs"))
            .map(str::to_string)
            .collect();
        let mut files = Vec::new();
        for entry in repo.index()?.iter() {
            if let Some(pointer) = index_pointer(&repo, &entry)? {
                let path = super::path_from_bytes(&entry.path);
                let file = safe_path(&repo, &path)?;
                let hydrated = file.is_file()
                    && (fs::metadata(&file)?.len() > 1024
                        || Pointer::parse(&fs::read(&file)?).is_none());
                files.push(LfsFile {
                    path,
                    available: object(&repo, &pointer)?.is_file(),
                    oid: pointer.oid,
                    size: pointer.size,
                    hydrated,
                });
            }
        }
        Ok(LfsStatus {
            tool,
            patterns,
            files,
            auto_fetch: match repo.config()?.get_bool("gitbuddy.lfsAutoFetch") {
                Ok(value) => value,
                Err(error) if error.code() == git2::ErrorCode::NotFound => false,
                Err(error) => return Err(error.into()),
            },
        })
    }
    pub(super) fn lfs_track(&self, pattern: &str, track: bool) -> Result<String> {
        const SUFFIX: &str = " filter=lfs diff=lfs merge=lfs -text";
        // Git rejects attribute lines whose body reaches ATTR_MAX_LINE_LENGTH.
        const MAX_LINE: usize = 2048;

        fn quote(pattern: &str) -> String {
            if !pattern.starts_with('#')
                && pattern
                    .bytes()
                    .all(|byte| (b'!'..=b'~').contains(&byte) && !matches!(byte, b'"' | b'\\'))
            {
                return pattern.to_string();
            }
            let mut out = String::from("\"");
            for byte in pattern.bytes() {
                match byte {
                    7 => out.push_str("\\a"),
                    8 => out.push_str("\\b"),
                    b'\t' => out.push_str("\\t"),
                    b'\n' => out.push_str("\\n"),
                    11 => out.push_str("\\v"),
                    12 => out.push_str("\\f"),
                    b'\r' => out.push_str("\\r"),
                    b'"' => out.push_str("\\\""),
                    b'\\' => out.push_str("\\\\"),
                    b' '..=b'~' => out.push(char::from(byte)),
                    byte => {
                        out.push('\\');
                        out.push(char::from(b'0' + (byte >> 6)));
                        out.push(char::from(b'0' + ((byte >> 3) & 7)));
                        out.push(char::from(b'0' + (byte & 7)));
                    }
                }
            }
            out.push('"');
            out
        }

        fn generated(line: &[u8], expected: &gix_attributes::glob::Pattern) -> bool {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !line.ends_with(SUFFIX.as_bytes()) {
                return false;
            }
            let mut parsed = gix_attributes::parse(line);
            let Some(Ok((gix_attributes::parse::Kind::Pattern(pattern), assignments, _))) =
                parsed.next()
            else {
                return false;
            };
            if &pattern != expected || parsed.next().is_some() {
                return false;
            }
            let values = assignments
                .map(|assignment| assignment.map(|assignment| assignment.to_string()))
                .collect::<std::result::Result<Vec<_>, _>>();
            values.is_ok_and(|values| values == ["filter=lfs", "diff=lfs", "merge=lfs", "-text"])
        }

        ensure!(
            !pattern.is_empty()
                && !pattern.contains('\0')
                && !pattern.starts_with("[attr]")
                && pattern.len() < MAX_LINE,
            "Use a nonempty file glob without NUL or macro definitions; the generated attribute line must be shorter than 2048 bytes"
        );
        let glob = gix_attributes::glob::Pattern::from_bytes(pattern.as_bytes())
            .context("Use a nonempty file glob")?;
        ensure!(
            !glob.is_negative(),
            "Negative attribute patterns are not allowed; escape a literal leading ! as \\!"
        );
        let rule = format!("{}{SUFFIX}", quote(pattern));
        ensure!(
            rule.len() < MAX_LINE,
            "The generated LFS attribute line must be shorter than 2048 bytes"
        );
        let repo = raw(self)?;
        ensure!(
            !super::rebase::active(&repo) && repo.state() == git2::RepositoryState::Clean,
            "Finish the current operation first"
        );
        let path = safe_path(&repo, Path::new(".gitattributes"))?;
        let old = match fs::read(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let mut new = Vec::new();
        for line in old.split_inclusive(|byte| *byte == b'\n') {
            if !generated(line, &glob) {
                new.extend_from_slice(line);
            }
        }
        if track {
            if !new.is_empty() && new.last() != Some(&b'\n') {
                new.push(b'\n');
            }
            new.extend_from_slice(rule.as_bytes());
            new.push(b'\n');
        }
        let current = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        ensure!(current == old, "Attributes changed externally; retry");
        if new != old {
            safe_path(&repo, Path::new(".gitattributes"))?;
            replace(&path, &new)?;
        }
        Ok("LFS attributes updated; stage .gitattributes and the matching files (existing history is unchanged)".into())
    }
    pub(super) fn lfs_checkout(&self) -> Result<String> {
        let count = hydrate(&raw(self)?)?;
        Ok(format!(
            "LFS checkout complete; {count} objects missing locally (Fetch to download)"
        ))
    }
    pub(super) fn lfs_transfer(
        &self,
        push: bool,
        remote: &str,
        control: NetworkControl,
    ) -> Result<String> {
        ensure!(
            !remote.is_empty() && !remote.starts_with('-') && !remote.contains(['\n', '\r', '\0']),
            "Enter a configured remote name"
        );
        let repo = raw(self)?;
        repo.find_remote(remote)?;
        run_lfs(
            &self.root,
            &[if push { "push" } else { "fetch" }, remote, "HEAD"],
            &control,
        )?;
        control.cancellation.finish()?;
        if !push {
            hydrate(&repo)?;
        }
        Ok(if push {
            "LFS objects uploaded"
        } else {
            "LFS objects downloaded and checked out"
        }
        .into())
    }
}
pub(super) fn upload_before_push(
    repo: &git2::Repository,
    remote: &str,
    source: &str,
    control: &NetworkControl,
) -> Result<()> {
    ensure!(
        !remote.is_empty() && !remote.starts_with('-') && !remote.contains(['\n', '\r', '\0']),
        "Enter a configured remote name"
    );
    // Check reachable trees as well as HEAD: older commits can reference LFS
    // objects even after their files were removed from the current index.
    let mut walk = repo.revwalk()?;
    walk.push(repo.revparse_single(source)?.peel_to_commit()?.id())?;
    let mut found = false;
    let odb = repo.odb()?;
    let mut inspected = std::collections::HashSet::new();
    for id in walk {
        control.check()?;
        let tree = repo.find_commit(id?)?.tree()?;
        let mut scan_error = None;
        let result = tree.walk(git2::TreeWalkMode::PreOrder, |_, entry| {
            let scan = (|| -> Result<bool> {
                control.check()?;
                if entry.kind() != Some(git2::ObjectType::Blob) || !inspected.insert(entry.id()) {
                    return Ok(false);
                }
                // Only pointer-sized blobs need to be loaded into memory.
                if odb.read_header(entry.id())?.0 > 1024 {
                    return Ok(false);
                }
                Ok(Pointer::parse(repo.find_blob(entry.id())?.content()).is_some())
            })();
            match scan {
                Ok(true) => {
                    found = true;
                    git2::TreeWalkResult::Abort
                }
                Ok(false) => git2::TreeWalkResult::Ok,
                Err(error) => {
                    scan_error = Some(error);
                    git2::TreeWalkResult::Abort
                }
            }
        });
        if let Some(error) = scan_error {
            return Err(error);
        }
        if found {
            break;
        }
        result?;
    }
    if found {
        run_lfs(
            repo.workdir().context("No working directory")?,
            &["push", remote, source],
            control,
        )?;
    }
    Ok(())
}
fn lfs_command(root: &Path, program: &std::ffi::OsStr) -> Result<Command> {
    let repo = git2::Repository::open(root)?;
    let mut command = Command::new(program);
    command.current_dir(repo.workdir().unwrap_or(root));
    // Native repository opens use the explicit path. Tool children must use
    // that same repository even if GitBuddy was launched from a Git wrapper.
    // Configuration variables remain intact; extension filters append their
    // isolated storage overrides after this routing-only normalization.
    for key in LFS_ROUTING_ENV {
        command.env_remove(key);
    }
    command.env("GIT_DIR", repo.path());
    if let Some(worktree) = repo.workdir() {
        command.env("GIT_WORK_TREE", worktree);
    }
    Ok(command)
}

fn run_lfs(root: &Path, args: &[&str], control: &NetworkControl) -> Result<()> {
    run_lfs_program(root, args, control, &lfs_program(root)?)
}
fn lfs_program(root: &Path) -> Result<std::ffi::OsString> {
    match git2::Repository::open(root)?
        .config()?
        .get_path("gitbuddy.lfsProgram")
    {
        Ok(path) => {
            ensure!(!path.as_os_str().is_empty(), "LFS program cannot be empty");
            Ok(path.into_os_string())
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok("git-lfs".into()),
        Err(error) => Err(error.into()),
    }
}
fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The child was spawned in its own group, so SSH / transfer children
        // cannot keep the error pipe open after cancellation.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
// Own the process group even when the worker unwinds or returns early.
struct TransferChild {
    child: std::process::Child,
    cleaned: bool,
}
impl TransferChild {
    fn cleanup(&mut self) {
        if !self.cleaned {
            stop(&mut self.child);
            self.cleaned = true;
        }
    }
}
impl std::ops::Deref for TransferChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}
impl std::ops::DerefMut for TransferChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}
impl Drop for TransferChild {
    fn drop(&mut self) {
        self.cleanup();
    }
}
fn run_lfs_program(
    root: &Path,
    args: &[&str],
    control: &NetworkControl,
    program: &std::ffi::OsStr,
) -> Result<()> {
    capture_lfs_program(root, args, control, program).map(|_| ())
}

pub(super) fn capture_lfs(root: &Path, args: &[&str], control: &NetworkControl) -> Result<String> {
    capture_lfs_program(root, args, control, &lfs_program(root)?)
}
fn drain(mut pipe: impl Read) -> Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut overflow = false;
    let mut buffer = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let remaining = (2 * 1024 * 1024usize).saturating_sub(kept.len());
        overflow |= n > remaining;
        kept.extend_from_slice(&buffer[..n.min(remaining)]);
    }
    ensure!(
        !overflow,
        "LFS output exceeds 2 MiB; no partial result was accepted"
    );
    Ok(kept)
}
fn capture_lfs_program(
    root: &Path,
    args: &[&str],
    control: &NetworkControl,
    program: &std::ffi::OsStr,
) -> Result<String> {
    control.check()?;
    control.phase(&format!(
        "Running git-lfs {}…",
        args.first().copied().unwrap_or("operation")
    ));
    let mut command = lfs_command(root, program)?;
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = TransferChild {
        child: command.spawn().context(
            "Install git-lfs to transfer LFS objects; ordinary Git operations still use libgit2",
        )?,
        cleaned: false,
    };
    let stderr = child.stderr.take().context("Missing LFS error pipe")?;
    let stdout = child.stdout.take().context("Missing LFS output pipe")?;
    let reader = std::thread::spawn(move || drain(stderr));
    let output = std::thread::spawn(move || drain(stdout));
    loop {
        if let Err(cancel) = control.check() {
            child.cleanup();
            let _ = reader.join();
            let _ = output.join();
            return Err(cancel);
        }
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                child.cleanup();
                let _ = reader.join();
                let _ = output.join();
                return Err(error.into());
            }
        };
        if let Some(status) = status {
            // A descendant may still hold stderr after its parent has exited.
            child.cleanup();
            let error = reader.join();
            let output = output.join();
            let error = error.map_err(|_| anyhow::anyhow!("LFS error reader failed"))??;
            let output = output.map_err(|_| anyhow::anyhow!("LFS output reader failed"))??;
            ensure!(
                status.success(),
                "git-lfs failed: {}",
                String::from_utf8_lossy(&error)
            );
            return String::from_utf8(output).context("LFS output is not UTF-8");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_local_smudge_reads_standard_cache_without_publishing_worktree_changes() {
        let directory = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(directory.path()).unwrap();
        let path = Path::new("data.bin");
        fs::write(directory.path().join(path), b"working sentinel").unwrap();
        let pointer = clean_content_at_bounded(&repo, b"original content", path, 32).unwrap();
        let oid = Pointer::parse(&pointer).unwrap();
        let cache = object(&repo, &oid).unwrap();
        let before = fs::read(&cache).unwrap();
        assert_eq!(
            local_smudge_content_at(&repo, &pointer, path, 16).unwrap(),
            b"original content"
        );
        assert!(local_smudge_content_at(&repo, &pointer, path, 15).is_err());
        assert_eq!(fs::read(&cache).unwrap(), before);
        assert_eq!(
            fs::read(directory.path().join(path)).unwrap(),
            b"working sentinel"
        );
        fs::write(&cache, vec![0u8; 1024]).unwrap();
        assert!(local_smudge_content_at(&repo, &pointer, path, 16).is_err());
        fs::remove_file(cache).unwrap();
        assert!(local_smudge_content_at(&repo, &pointer, path, 16).is_err());
        assert_eq!(
            fs::read(directory.path().join(path)).unwrap(),
            b"working sentinel"
        );
    }

    #[test]
    fn conversion_fingerprint_pins_config_and_routes_without_pinning_cache_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(directory.path()).unwrap();
        let before = conversion_fingerprint(&repo).unwrap();
        clean_content_at_bounded(&repo, b"cache content", Path::new("data.bin"), 128).unwrap();
        assert_eq!(before, conversion_fingerprint(&repo).unwrap());
        repo.config()
            .unwrap()
            .set_i64("lfs.repositoryformatversion", 0)
            .unwrap();
        assert_eq!(before, conversion_fingerprint(&repo).unwrap());
        repo.config()
            .unwrap()
            .set_i64("lfs.repositoryformatversion", 1)
            .unwrap();
        assert!(conversion_fingerprint(&repo).is_err());
        assert!(validate_extension_configuration(&repo).is_err());
        repo.config()
            .unwrap()
            .set_i64("lfs.repositoryformatversion", 0)
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("lfs.storage", "other-storage")
            .unwrap();
        let storage_changed = conversion_fingerprint(&repo).unwrap();
        assert_ne!(before, storage_changed);
        repo.config()
            .unwrap()
            .set_str("lfs.extension.test.clean", "unused-clean")
            .unwrap();
        assert_ne!(storage_changed, conversion_fingerprint(&repo).unwrap());
    }

    #[test]
    fn extension_configuration_validation_does_not_execute_missing_processors() {
        let directory = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(directory.path()).unwrap();
        assert!(validate_extension_configuration(&repo).is_ok());
        repo.config()
            .unwrap()
            .set_str("lfs.extension.test.clean", "must-not-execute")
            .unwrap();
        assert!(validate_extension_configuration(&repo).is_err());
        repo.config()
            .unwrap()
            .set_str("lfs.extension.test.smudge", "must-not-execute")
            .unwrap();
        assert!(validate_extension_configuration(&repo).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_extension_helpers_preserve_source_config_and_validate_raw_cap() {
        let program = std::env::var_os("GITBUDDY_TEST_LFS_PROGRAM")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs")
            });
        if !program.is_file() {
            eprintln!("official git-lfs unavailable; skipping local extension fixture");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(directory.path()).unwrap();
        let processor = directory.path().join("processor.py");
        fs::write(&processor, "import sys\ndata=sys.stdin.buffer.read()\nsys.stdout.buffer.write(b'EXT:'+data[::-1] if sys.argv[1]=='clean' else data[4:][::-1])\n").unwrap();
        let mut config = repo.config().unwrap();
        config
            .set_str("gitbuddy.lfsProgram", program.to_str().unwrap())
            .unwrap();
        for operation in ["clean", "smudge"] {
            config
                .set_str(
                    &format!("lfs.extension.reverse.{operation}"),
                    &format!("python3 {} {operation}", processor.display()),
                )
                .unwrap();
        }
        config.set_i64("lfs.extension.reverse.priority", 0).unwrap();
        let before = fs::read(repo.path().join("config")).unwrap();
        let path = Path::new("data.bin");
        fs::write(directory.path().join(path), b"worktree sentinel").unwrap();
        let pointer = clean_content_at_bounded(&repo, b"original", path, 64).unwrap();
        assert_eq!(
            local_smudge_content_at(&repo, &pointer, path, 8).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(repo.path().join("config")).unwrap(), before);
        assert_eq!(
            fs::read(directory.path().join(path)).unwrap(),
            b"worktree sentinel"
        );
        assert!(clean_content_at_bounded(&repo, b"other", path, 5).is_err());
        let canonical = b"EXT:rehto";
        let rejected = Pointer {
            oid: format!("{:x}", Sha256::digest(canonical)),
            size: canonical.len() as u64,
            extensions: Vec::new(),
        };
        assert!(!object(&repo, &rejected).unwrap().exists());
    }
    #[test]
    fn output_capture_rejects_oversize_data_instead_of_accepting_partial_json() {
        let bytes = vec![b'x'; 2 * 1024 * 1024 + 1];
        assert!(drain(std::io::Cursor::new(bytes)).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn capture_drains_stdout_and_stderr_and_returns_only_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "printf '{\"ours\":[],\"theirs\":[]}'\nprintf 'diagnostic' >&2",
        );
        let result = capture_lfs_program(
            dir.path(),
            &["locks", "--verify", "--json"],
            &NetworkControl::default(),
            program.as_os_str(),
        )
        .unwrap();
        assert_eq!(result, "{\"ours\":[],\"theirs\":[]}");
    }
    #[test]
    fn pointer_parser_rejects_invalid_hashes_and_unknown_keys() {
        let p = Pointer {
            oid: "a".repeat(64),
            size: 123,
            extensions: Vec::new(),
        };
        assert_eq!(Pointer::parse(p.text().as_bytes()), Some(p.clone()));
        assert!(Pointer::parse(p.text().replace("sha256:", "sha1:").as_bytes()).is_none());
        assert!(Pointer::parse(format!("{}unexpected\n", p.text()).as_bytes()).is_none());
    }
    #[cfg(unix)]
    fn script(dir: &Path, content: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        git2::Repository::init(dir).unwrap();
        let path = dir.join("fake-lfs");
        fs::write(&path, format!("#!/bin/sh\n{content}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[cfg(unix)]
    #[test]
    fn transport_passes_arguments_without_shell_interpolation_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            dir.path(),
            "printf '%s\\n' \"$@\" > args\nprintf 'failed transfer' >&2\nexit 7",
        );
        let control = NetworkControl::new(None, super::super::CancellationToken::default());
        let error = run_lfs_program(
            dir.path(),
            &["fetch", "a;touch injected", "HEAD"],
            &control,
            path.as_os_str(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("failed transfer"));
        assert_eq!(
            fs::read_to_string(dir.path().join("args")).unwrap(),
            "fetch\na;touch injected\nHEAD\n"
        );
        assert!(!dir.path().join("injected").exists());
    }
    #[cfg(unix)]
    #[test]
    fn dropped_transfer_owner_terminates_and_reaps_child() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "touch ready\nsleep 30 &\nwait");
        let child = Command::new(path)
            .current_dir(dir.path())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = std::time::Instant::now();
        while !dir.path().join("ready").exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.path().join("ready").exists());
        drop(TransferChild {
            child,
            cleaned: false,
        });
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[cfg(unix)]
    #[test]
    fn completed_parent_does_not_leave_stderr_descendant_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "sleep 30 &\nexit 0");
        let start = std::time::Instant::now();
        run_lfs_program(
            dir.path(),
            &[],
            &NetworkControl::default(),
            path.as_os_str(),
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[cfg(unix)]
    #[test]
    fn cancelling_transport_terminates_its_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "touch ready\nsleep 30 &\nwait");
        let token = super::super::CancellationToken::default();
        let control = NetworkControl::new(None, token.clone());
        let root = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            run_lfs_program(&root, &["fetch"], &control, path.as_os_str())
        });
        let start = std::time::Instant::now();
        while !dir.path().join("ready").exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.path().join("ready").exists());
        assert!(token.cancel());
        assert!(super::super::is_cancelled(
            &worker.join().unwrap().unwrap_err()
        ));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
