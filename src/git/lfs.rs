//! Native local LFS clean/smudge; the official git-lfs binary handles transport.
use super::libgit::raw;
use super::{NetworkControl, Repository};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

const VERSION: &str = "version https://git-lfs.github.com/spec/v1";
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
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pointer {
    oid: String,
    size: u64,
}
impl Pointer {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > 1024 {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        if lines.next()? != VERSION {
            return None;
        }
        let oid = lines.next()?.strip_prefix("oid sha256:")?;
        if oid.len() != 64
            || !oid
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return None;
        }
        let size = lines.next()?.strip_prefix("size ")?.parse().ok()?;
        if lines.next().is_some() {
            return None;
        }
        Some(Self {
            oid: oid.into(),
            size,
        })
    }
    fn text(&self) -> String {
        format!("{VERSION}\noid sha256:{}\nsize {}\n", self.oid, self.size)
    }
}
fn hash(path: &Path) -> Result<Pointer> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut size = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        size += count as u64;
    }
    Ok(Pointer {
        oid: format!("{:x}", digest.finalize()),
        size,
    })
}
fn storage(repo: &git2::Repository) -> Result<PathBuf> {
    ensure!(
        repo.config()?.get_string("lfs.storage").is_err(),
        "Custom lfs.storage is not supported by native LFS; use the default LFS object directory"
    );
    Ok(repo.commondir().join("lfs/objects"))
}
fn object(repo: &git2::Repository, pointer: &Pointer) -> Result<PathBuf> {
    Ok(storage(repo)?
        .join(&pointer.oid[..2])
        .join(&pointer.oid[2..4])
        .join(&pointer.oid))
}
fn safe_path(repo: &git2::Repository, path: &Path) -> Result<PathBuf> {
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
    // libgit2 1.9 does not unquote attribute patterns. Refuse ambiguous LFS
    // rules rather than silently committing raw file contents.
    if let Some(root) = repo.workdir() {
        let mut directory = root.to_path_buf();
        for part in std::iter::once(None).chain(
            path.parent()
                .into_iter()
                .flat_map(|p| p.components())
                .map(Some),
        ) {
            if let Some(part) = part {
                directory.push(part);
            }
            let attributes = directory.join(".gitattributes");
            // FILE_THEN_INDEX also reads indexed attributes if the working
            // file is missing; validate that fallback with the same policy.
            if !attributes.exists() {
                let relative = attributes.strip_prefix(root)?;
                if let Some(entry) = repo.index()?.get_path(relative, 0) {
                    reject_quoted_bytes(repo.find_blob(entry.id)?.content())?;
                }
            }
            reject_quoted_rules(&attributes)?;
        }
        reject_quoted_rules(&repo.commondir().join("info/attributes"))?;
        if let Ok(global) = repo.config()?.get_path("core.attributesfile") {
            reject_quoted_rules(&global)?;
        }
    }
    Ok(repo.get_attr(path, "filter", git2::AttrCheckFlags::FILE_THEN_INDEX)? == Some("lfs"))
}
fn reject_quoted_bytes(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.split(|b| *b == b'\n').any(|line| {
            line.iter()
                .copied()
                .find(|b| !matches!(b, b' ' | b'\t' | b'\r'))
                == Some(b'"')
        }),
        "Quoted attribute patterns (including LFS macros) are not supported by libgit2; replace them with unquoted globs before staging"
    );
    Ok(())
}
fn reject_quoted_rules(path: &Path) -> Result<()> {
    match fs::read(path) {
        Ok(bytes) => reject_quoted_bytes(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Cannot check attribute rules in {}", path.display())),
    }
}
fn index_pointer(repo: &git2::Repository, entry: &git2::IndexEntry) -> Result<Option<Pointer>> {
    if (entry.flags >> 12) & 3 != 0 {
        return Ok(None);
    }
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
    Ok(fs::metadata(&file)?.len() == pointer.size && hash(&file)? == pointer)
}
/// Clean through one implementation for ordinary staging and conflict results.
fn clean_reader(repo: &git2::Repository, mut source: impl Read) -> Result<Vec<u8>> {
    let mut probe = Vec::new();
    source.by_ref().take(1025).read_to_end(&mut probe)?;
    let pointer = Pointer::parse(&probe);
    ensure!(
        pointer.is_some() || !probe.starts_with(VERSION.as_bytes()),
        "Unsupported or malformed LFS pointer; native staging does not support pointer extensions"
    );
    let pointer = if let Some(pointer) = pointer {
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
        };
        let target = object(repo, &pointer)?;
        fs::create_dir_all(target.parent().context("No object directory")?)?;
        temp.as_file().sync_all()?;
        if target.exists() {
            ensure!(hash(&target)? == pointer, "Cached LFS object is corrupt");
        } else {
            temp.persist(&target)?;
        }
        pointer
    };
    Ok(pointer.text().into_bytes())
}

pub(super) fn clean_content(repo: &git2::Repository, bytes: &[u8]) -> Result<Vec<u8>> {
    clean_reader(repo, std::io::Cursor::new(bytes))
}

pub(super) fn stage(repo: &git2::Repository, index: &mut git2::Index, path: &Path) -> Result<()> {
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
    let text = clean_reader(repo, fs::File::open(&file)?)?;
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
    for entry in repo.index()?.iter() {
        let path = super::path_from_bytes(&entry.path);
        if tracked(repo, &path)?
            && let Some(pointer) = index_pointer(repo, &entry)?
            && matches_index(repo, &path)?
        {
            let file = safe_path(repo, &path)?;
            if fs::metadata(&file)?.len() > 1024 || Pointer::parse(&fs::read(&file)?).is_none() {
                // Preserve an object even if an external client removed its cache.
                let mut index = repo.index()?;
                stage(repo, &mut index, &path)?;
                replace(&file, pointer.text().as_bytes())?;
            }
        }
    }
    Ok(())
}
pub(super) fn hydrate(repo: &git2::Repository) -> Result<usize> {
    let mut missing = 0;
    let index = repo.index()?;
    for entry in index.iter() {
        let path = super::path_from_bytes(&entry.path);
        if !tracked(repo, &path)? {
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
        if changed.conflict() || !changed.unstaged() || !tracked(repo, &changed.path)? {
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
fn hydrate_file(repo: &git2::Repository, file: &Path, pointer: &Pointer) -> Result<usize> {
    let object = object(repo, pointer)?;
    if !object.is_file() {
        return Ok(1);
    }
    ensure!(
        hash(&object)? == *pointer,
        "LFS object {} is corrupt",
        pointer.oid
    );
    let mut temp = tempfile::NamedTempFile::new_in(file.parent().context("No parent")?)?;
    temp.as_file()
        .set_permissions(fs::metadata(file)?.permissions())?;
    std::io::copy(&mut fs::File::open(object)?, &mut temp)?;
    temp.as_file().sync_all()?;
    temp.persist(file)?;
    Ok(0)
}
impl Repository {
    pub fn lfs_status(&self) -> Result<LfsStatus> {
        let repo = raw(self)?;
        let tool = Command::new("git-lfs")
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
        })
    }
    pub(super) fn lfs_track(&self, pattern: &str, track: bool) -> Result<String> {
        ensure!(
            !pattern.trim().is_empty()
                && !pattern.starts_with(['!', '#', '-'])
                && !pattern
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\0' | '"' | '\\')),
            "Use an unquoted glob without whitespace, quotes or backslashes (e.g. *.bin)"
        );
        let repo = raw(self)?;
        ensure!(
            !super::rebase::active(&repo) && repo.state() == git2::RepositoryState::Clean,
            "Finish the current operation first"
        );
        let path = safe_path(&repo, Path::new(".gitattributes"))?;
        let old = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let quoted = serde_json::to_string(pattern)?;
        let quoted_rule = format!("{quoted} filter=lfs diff=lfs merge=lfs -text");
        let simple = format!("{pattern} filter=lfs diff=lfs merge=lfs -text");
        let mut lines = old
            .lines()
            .filter(|l| *l != quoted_rule && *l != simple)
            .map(str::to_string)
            .collect::<Vec<_>>();
        if track {
            lines.push(simple);
        }
        let new = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        ensure!(
            fs::read_to_string(&path).unwrap_or_default() == old,
            "Attributes changed externally; retry"
        );
        replace(&path, new.as_bytes())?;
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
    walk.push_ref(source)?;
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
fn run_lfs(root: &Path, args: &[&str], control: &NetworkControl) -> Result<()> {
    run_lfs_program(root, args, control, std::ffi::OsStr::new("git-lfs"))
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
    control.check()?;
    control.phase("Transferring LFS objects…");
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
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
    let mut stderr = child.stderr.take().context("Missing LFS error pipe")?;
    let reader = std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let remaining = 16384usize.saturating_sub(kept.len());
                    kept.extend_from_slice(&buffer[..n.min(remaining)]);
                }
            }
        }
        kept
    });
    loop {
        if let Err(cancel) = control.check() {
            child.cleanup();
            let _ = reader.join();
            return Err(cancel);
        }
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                child.cleanup();
                let _ = reader.join();
                return Err(error.into());
            }
        };
        if let Some(status) = status {
            // A descendant may still hold stderr after its parent has exited.
            child.cleanup();
            let error = reader.join().unwrap_or_default();
            ensure!(
                status.success(),
                "git-lfs failed: {}",
                String::from_utf8_lossy(&error)
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pointer_parser_rejects_invalid_hashes_and_extensions() {
        let p = Pointer {
            oid: "a".repeat(64),
            size: 123,
        };
        assert_eq!(Pointer::parse(p.text().as_bytes()), Some(p.clone()));
        assert!(Pointer::parse(p.text().replace("sha256:", "sha1:").as_bytes()).is_none());
        assert!(Pointer::parse(format!("{}unexpected\n", p.text()).as_bytes()).is_none());
    }
    #[cfg(unix)]
    fn script(dir: &Path, content: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
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
