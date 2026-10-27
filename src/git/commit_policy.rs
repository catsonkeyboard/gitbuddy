//! Commit object signing using the repository's effective Git configuration.
use super::{NetworkControl, Repository, TagSigner};
use anyhow::{Context, Result, ensure};
use git2::{Commit, ErrorCode, Oid, Repository as RawRepo, Signature, Tree};
use std::{fs, process::Command};

fn hook_path(repo: &RawRepo, name: &str) -> Result<std::path::PathBuf> {
    let root = repo
        .workdir()
        .context("Hooks require a working directory")?;
    let config = repo.config()?;
    let path = match config.get_path("core.hooksPath") {
        Ok(p) if p.is_absolute() => p,
        Ok(p) => root.join(p),
        Err(e) if e.code() == ErrorCode::NotFound => repo.commondir().join("hooks"),
        Err(e) => return Err(e.into()),
    };
    Ok(path.join(name))
}

fn run_hook(repo: &RawRepo, name: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let path = hook_path(repo, name)?;
    let metadata = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Ok(());
        }
    }
    ensure!(metadata.is_file(), "Hook {name} is not a file");
    let root = repo
        .workdir()
        .context("Hooks require a working directory")?;
    let tmp = tempfile::Builder::new()
        .prefix("gitbuddy-hook-")
        .tempdir()?;
    let input = tmp.path().join("stdin");
    fs::write(&input, "")?;
    let mut command = Command::new(&path);
    command
        .args(args)
        .env("GIT_DIR", repo.path())
        .env("GIT_WORK_TREE", root)
        .env("GIT_EDITOR", ":");
    super::tags::run_tool(
        root,
        &mut command,
        &input,
        tmp.path(),
        &NetworkControl::default(),
    )
    .with_context(|| format!("{name} hook failed"))?;
    Ok(())
}

pub(super) fn prepare_message(repo: &RawRepo, message: &str, amend: bool) -> Result<String> {
    run_hook(repo, "pre-commit", &[])?;
    let file = tempfile::Builder::new()
        .prefix("gitbuddy-commit-message-")
        .tempfile_in(repo.path())?;
    fs::write(file.path(), message)?;
    let mut args = vec![
        file.path().as_os_str(),
        std::ffi::OsStr::new(if amend { "commit" } else { "message" }),
    ];
    if amend {
        args.push(std::ffi::OsStr::new("HEAD"));
    }
    run_hook(repo, "prepare-commit-msg", &args)?;
    run_hook(repo, "commit-msg", &[file.path().as_os_str()])?;
    let size = fs::metadata(file.path())?.len();
    ensure!(size <= 1024 * 1024, "Commit message exceeds 1 MiB");
    let text =
        fs::read_to_string(file.path()).context("Hook produced a non UTF-8 commit message")?;
    ensure!(
        !text.trim().is_empty() && !text.contains('\0'),
        "Hook produced an empty or invalid commit message"
    );
    Ok(text)
}

pub(super) fn post_commit(repo: &RawRepo) -> Result<()> {
    run_hook(repo, "post-commit", &[])
}

pub(super) fn create_object(
    repo: &RawRepo,
    author: &Signature<'_>,
    committer: &Signature<'_>,
    message: &str,
    tree: &Tree<'_>,
    parents: &[&Commit<'_>],
) -> Result<Oid> {
    let config = repo.config()?.snapshot()?;
    let signed = match config.get_bool("commit.gpgsign") {
        Ok(value) => value,
        Err(error) if error.code() == ErrorCode::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !signed {
        return Ok(repo.commit(None, author, committer, message, tree, parents)?);
    }
    let root = repo
        .workdir()
        .context("Signing requires a working directory")?;
    let defaults = Repository {
        root: root.to_path_buf(),
    }
    .tag_defaults()?;
    ensure!(
        defaults.notice.is_none(),
        "Unsupported commit signing format: {}",
        defaults.notice.unwrap_or_default()
    );
    let signer = TagSigner {
        format: defaults.format,
        program: defaults.program(defaults.format),
        key: defaults.key,
    };
    let payload = repo.commit_create_buffer(author, committer, message, tree, parents)?;
    let signature =
        super::tags::sign_and_verify(root, payload.as_ref(), &signer, &NetworkControl::default())?;
    let text = std::str::from_utf8(payload.as_ref()).context("Commit content is not UTF-8")?;
    Ok(repo.commit_signed(text, &signature, None)?)
}

impl Repository {
    /// Verify the signature's cryptographic validity; this does not establish identity trust.
    pub fn verify_commit_signature(&self, id: &str) -> Result<String> {
        let repo = super::libgit::raw(self)?;
        let oid = super::libgit::oid(id)?;
        repo.find_commit(oid).context("Commit no longer exists")?;
        let (signature, content) = match repo.extract_signature(&oid, None) {
            Ok(pair) => pair,
            Err(error) if error.code() == ErrorCode::NotFound => {
                return Ok("Commit has no signature".into());
            }
            Err(error) => return Err(error.into()),
        };
        let text = std::str::from_utf8(signature.as_ref()).context("Signature is not UTF-8")?;
        let defaults = self.tag_defaults()?;
        let dir = tempfile::Builder::new()
            .prefix("gitbuddy-commit-verify-")
            .tempdir()?;
        let data = dir.path().join("commit.data");
        let sig = dir.path().join("commit.sig");
        fs::write(&data, content.as_ref())?;
        fs::write(&sig, signature.as_ref())?;
        let (mut command, label) = if text.starts_with("-----BEGIN PGP SIGNATURE-----") {
            let mut c = Command::new(defaults.openpgp_program);
            c.args(["--no-tty", "--verify"]).arg(&sig).arg(&data);
            (c, "OpenPGP")
        } else if text.starts_with("-----BEGIN SSH SIGNATURE-----") {
            let mut c = Command::new(defaults.ssh_program);
            c.args(["-Y", "check-novalidate", "-n", "git", "-s"])
                .arg(&sig);
            (c, "SSH")
        } else {
            anyhow::bail!("Unsupported commit signature format")
        };
        super::tags::run_tool(
            &self.root,
            &mut command,
            &data,
            dir.path(),
            &NetworkControl::default(),
        )
        .context("Commit signature verification failed")?;
        Ok(format!(
            "{label} signature is cryptographically valid; signer identity and trust were not verified"
        ))
    }
}
