//! Pinned commit targets and native tag objects, with explicit external signing.
use super::libgit::{author, oid, raw};
use super::recovery::head_state;
use super::{HeadState, NetworkControl, Repository};
use anyhow::{Context, Result, ensure};
use git2::{ErrorCode, ObjectType};
use std::{
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const MESSAGE_LIMIT: usize = 1024 * 1024;
const OUTPUT_LIMIT: u64 = 1024 * 1024;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TagKind {
    #[default]
    Lightweight,
    Annotated,
    Signed,
}
impl TagKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lightweight => "Lightweight",
            Self::Annotated => "Annotated",
            Self::Signed => "Signed",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TagSigningFormat {
    #[default]
    OpenPgp,
    Ssh,
}
impl TagSigningFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenPgp => "OpenPGP",
            Self::Ssh => "SSH",
        }
    }
    pub fn program(self) -> &'static str {
        match self {
            Self::OpenPgp => "gpg",
            Self::Ssh => "ssh-keygen",
        }
    }
    fn markers(self) -> (&'static str, &'static str) {
        match self {
            Self::OpenPgp => (
                "-----BEGIN PGP SIGNATURE-----",
                "-----END PGP SIGNATURE-----",
            ),
            Self::Ssh => (
                "-----BEGIN SSH SIGNATURE-----",
                "-----END SSH SIGNATURE-----",
            ),
        }
    }
}
#[derive(Clone, Debug)]
pub struct TagSigner {
    pub format: TagSigningFormat,
    pub program: String,
    pub key: String,
}
#[derive(Clone, Debug, Default)]
pub struct TagSpec {
    pub name: String,
    pub kind: TagKind,
    pub message: String,
    pub signer: Option<TagSigner>,
}
#[derive(Clone, Debug)]
pub struct TagDefaults {
    pub signed: bool,
    pub format: TagSigningFormat,
    pub key: String,
    pub openpgp_program: String,
    pub ssh_program: String,
    pub notice: Option<String>,
}
impl TagDefaults {
    pub fn program(&self, format: TagSigningFormat) -> String {
        match format {
            TagSigningFormat::OpenPgp => self.openpgp_program.clone(),
            TagSigningFormat::Ssh => self.ssh_program.clone(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct TagTarget {
    pub id: String,
    pub label: String,
    head: HeadState,
    config: u64,
}
#[derive(Clone)]
pub struct TagPlan {
    target: TagTarget,
    spec: TagSpec,
    tagger: Option<Tagger>,
}
#[derive(Clone)]
struct Tagger {
    name: String,
    email: String,
    seconds: i64,
    offset: i32,
}
impl Tagger {
    fn signature(&self) -> Result<git2::Signature<'static>> {
        Ok(git2::Signature::new(
            &self.name,
            &self.email,
            &git2::Time::new(self.seconds, self.offset),
        )?)
    }
}
impl std::fmt::Debug for TagPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagPlan")
            .field("target", &self.target)
            .field("name", &self.spec.name)
            .field("kind", &self.spec.kind)
            .finish_non_exhaustive()
    }
}
impl TagPlan {
    pub fn kind(&self) -> TagKind {
        self.spec.kind
    }
    pub fn summary(&self) -> String {
        let tagger = self
            .tagger
            .as_ref()
            .map(|s| format!("{} <{}>", s.name, s.email))
            .unwrap_or_else(|| "No tag object / tagger".into());
        let mut text = format!(
            "Create {} tag {}?\n\nTarget: {}\nCommit: {}\nTagger: {tagger}\nHEAD, branches, index and working files stay unchanged. Existing tags are never replaced.",
            self.spec.kind.label(),
            self.spec.name,
            self.target.label,
            self.target.id
        );
        if !self.spec.message.is_empty() {
            let preview = self.spec.message.chars().take(500).collect::<String>();
            text.push_str(&format!(
                "\n\nMessage:\n{preview}{}",
                if self.spec.message.chars().count() > 500 {
                    "…"
                } else {
                    ""
                }
            ));
        }
        if let Some(signer) = &self.spec.signer {
            text.push_str(&format!("\n\n{} signing tool: {}\nKey: {}\nSigning and verification must succeed before the tag is published.",signer.format.label(),signer.program,signer.key));
        }
        text
    }
}
pub(super) fn config_fingerprint(repo: &git2::Repository) -> Result<u64> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    repo.config()?.snapshot()?.entries(None)?.for_each(|e| {
        e.name_bytes().hash(&mut hash);
        e.has_value().hash(&mut hash);
        if e.has_value() {
            e.value_bytes().hash(&mut hash);
        }
    })?;
    Ok(hash.finish())
}
fn config_string(config: &git2::Config, key: &str) -> Result<Option<String>> {
    match config.get_string(key) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn check_target(repo: &git2::Repository, target: &TagTarget) -> Result<()> {
    ensure!(
        head_state(repo)? == target.head && config_fingerprint(repo)? == target.config,
        "Repository, HEAD or configuration changed; reopen tag creation and review again"
    );
    ensure!(
        repo.revparse_single(&target.label)?.peel_to_commit()?.id() == oid(&target.id)?,
        "Tag target moved; resolve and review it again"
    );
    Ok(())
}
fn check_name(name: &str) -> Result<String> {
    let reference = format!("refs/tags/{name}");
    ensure!(
        !name.is_empty() && !name.starts_with('-') && git2::Reference::is_valid_name(&reference),
        "Enter a valid tag name"
    );
    Ok(reference)
}
fn absent(repo: &git2::Repository, name: &str) -> Result<()> {
    match repo.find_reference(name) {
        Ok(_) => anyhow::bail!("Tag already exists; choose another name"),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
impl Repository {
    pub fn tag_defaults(&self) -> Result<TagDefaults> {
        let repo = raw(self)?;
        let config = repo.config()?.snapshot()?;
        let configured = config_string(&config, "gpg.format")?.unwrap_or_else(|| "openpgp".into());
        let format = if configured == "ssh" {
            TagSigningFormat::Ssh
        } else {
            TagSigningFormat::OpenPgp
        };
        let signed = match config.get_bool("tag.gpgsign") {
            Ok(v) => v,
            Err(e) if e.code() == ErrorCode::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        Ok(TagDefaults {signed,format,key:config_string(&config,"user.signingkey")?.unwrap_or_else(||if format==TagSigningFormat::OpenPgp {config_string(&config,"user.email").ok().flatten().unwrap_or_default()}else{String::new()}),
            openpgp_program:config_string(&config,"gpg.openpgp.program")?.or(config_string(&config,"gpg.program")?).unwrap_or_else(||"gpg".into()),
            ssh_program:config_string(&config,"gpg.ssh.program")?.unwrap_or_else(||"ssh-keygen".into()),
            notice:(!["ssh","openpgp"].contains(&configured.as_str())).then(||format!("Configured signing format {configured} is unsupported here. Explicitly choose OpenPGP or SSH to sign.")),})
    }
    pub fn prepare_tag_target(&self, revision: &str) -> Result<TagTarget> {
        let repo = raw(self)?;
        let label = revision.trim();
        ensure!(!label.is_empty(), "Enter a commit, branch, tag or revision");
        let commit = repo
            .revparse_single(label)?
            .peel_to_commit()
            .context("Tag target must resolve to a commit")?;
        Ok(TagTarget {
            id: commit.id().to_string(),
            label: label.into(),
            head: head_state(&repo)?,
            config: config_fingerprint(&repo)?,
        })
    }
    pub fn prepare_tag(&self, target: &TagTarget, mut spec: TagSpec) -> Result<TagPlan> {
        let repo = raw(self)?;
        check_target(&repo, target)?;
        absent(&repo, &check_name(&spec.name)?)?;
        ensure!(
            spec.message.len() <= MESSAGE_LIMIT && !spec.message.contains('\0'),
            "Tag message must be UTF-8 text up to 1 MiB without NUL bytes"
        );
        let tagger = if spec.kind == TagKind::Lightweight {
            ensure!(
                spec.message.is_empty() && spec.signer.is_none(),
                "Lightweight tags have no message or signature"
            );
            None
        } else {
            ensure!(
                !spec.message.trim().is_empty(),
                "Enter an annotation for this tag"
            );
            ensure!(
                !spec.message.contains("-----BEGIN PGP SIGNATURE-----")
                    && !spec.message.contains("-----BEGIN SSH SIGNATURE-----"),
                "Tag message must not contain an embedded signature block"
            );
            if !spec.message.ends_with('\n') {
                spec.message.push('\n');
            }
            let signature = author(&repo)?;
            Some(Tagger {
                name: signature.name().context("Invalid tagger name")?.into(),
                email: signature.email().context("Invalid tagger email")?.into(),
                seconds: signature.when().seconds(),
                offset: signature.when().offset_minutes(),
            })
        };
        ensure!(
            (spec.kind == TagKind::Signed) == spec.signer.is_some(),
            "Choose a signing tool for Signed tags only"
        );
        if let Some(signer) = &spec.signer {
            validate_signer(signer)?;
        }
        Ok(TagPlan {
            target: target.clone(),
            spec,
            tagger,
        })
    }
    pub(super) fn create_tag(&self, plan: &TagPlan, control: NetworkControl) -> Result<String> {
        control.check()?;
        let repo = raw(self)?;
        check_target(&repo, &plan.target)?;
        let name = check_name(&plan.spec.name)?;
        let mut refs = repo.transaction()?;
        refs.lock_ref(&name)?;
        absent(&repo, &name)?;
        let target = repo.find_commit(oid(&plan.target.id)?)?;
        let id = if let Some(tagger) = &plan.tagger {
            let tagger = tagger.signature()?;
            let annotation = repo.tag_annotation_create(
                &plan.spec.name,
                target.as_object(),
                &tagger,
                &plan.spec.message,
            )?;
            if let Some(signer) = &plan.spec.signer {
                let odb = repo.odb()?;
                let object = odb.read(annotation)?;
                let signature = sign_and_verify(&self.root, object.data(), signer, &control)?;
                let mut signed = object.data().to_vec();
                signed.extend_from_slice(signature.as_bytes());
                let id = odb.write(ObjectType::Tag, &signed)?;
                let tag = repo.find_tag(id)?;
                ensure!(
                    tag.target_id() == target.id() && tag.name()? == plan.spec.name,
                    "Signed tag object does not match the reviewed tag"
                );
                id
            } else {
                annotation
            }
        } else {
            target.id()
        };
        control.check()?;
        check_target(&repo, &plan.target)?;
        absent(&repo, &name)?;
        control.cancellation.finish()?;
        control.phase("Tag: publishing reviewed reference…");
        check_target(&repo, &plan.target)?;
        absent(&repo, &name)?;
        let signature = plan
            .tagger
            .as_ref()
            .map(Tagger::signature)
            .transpose()?
            .or_else(|| repo.signature().ok())
            .unwrap_or(git2::Signature::now("GitBuddy", "gitbuddy@localhost")?);
        refs.set_target(&name, id, Some(&signature), "gitbuddy: create tag")?;
        refs.commit()?;
        Ok(format!(
            "Created {} tag {} at {}",
            plan.spec.kind.label(),
            plan.spec.name,
            plan.target.id
        ))
    }
}
fn validate_signer(signer: &TagSigner) -> Result<()> {
    ensure!(
        !signer.program.trim().is_empty()
            && signer.program.len() <= 4096
            && !signer.program.contains(['\0', '\n', '\r']),
        "Enter a signing executable path, without shell arguments"
    );
    ensure!(
        !signer.key.trim().is_empty()
            && signer.key.len() <= 4096
            && !signer.key.contains(['\0', '\n', '\r']),
        "Enter an OpenPGP key ID / identity, or an SSH key path / key::public-key"
    );
    Ok(())
}
fn key_path(root: &Path, key: &str) -> Result<PathBuf> {
    if let Some(tail) = key.strip_prefix("~/") {
        return Ok(PathBuf::from(
            std::env::var_os("HOME").context("Cannot expand ~/ without a home directory")?,
        )
        .join(tail));
    }
    let path = Path::new(key);
    Ok(if path.is_absolute() {
        path.into()
    } else {
        root.join(path)
    })
}
pub(super) fn sign_and_verify(
    root: &Path,
    payload: &[u8],
    signer: &TagSigner,
    control: &NetworkControl,
) -> Result<String> {
    validate_signer(signer)?;
    let dir = tempfile::Builder::new()
        .prefix("gitbuddy-tag-sign-")
        .tempdir()?;
    let input = dir.path().join("tag.data");
    fs::write(&input, payload)?;
    let signature_path = dir.path().join("tag.sig");
    control.phase("Signing: waiting for signer / agent…");
    control.check()?;
    let mut command = Command::new(&signer.program);
    match signer.format {
        TagSigningFormat::OpenPgp => {
            command
                .args([
                    "--no-tty",
                    "--status-fd=2",
                    "--armor",
                    "--detach-sign",
                    "--output",
                    "-",
                ])
                .arg(format!("--local-user={}", signer.key));
        }
        TagSigningFormat::Ssh => {
            let key = if let Some(public) = signer.key.strip_prefix("key::") {
                let file = dir.path().join("signing-key.pub");
                fs::write(&file, format!("{public}\n"))?;
                file
            } else {
                key_path(root, &signer.key)?
            };
            command
                .args(["-Y", "sign", "-n", "git", "-f"])
                .arg(key)
                .arg("-");
        }
    }
    let output = run_tool(root, &mut command, &input, dir.path(), control)?;
    let mut signature =
        String::from_utf8(output).context("Signer output is not UTF-8 armored text")?;
    let (begin, end) = signer.format.markers();
    ensure!(
        signature.starts_with(begin)
            && signature.trim_end().ends_with(end)
            && signature.matches(begin).count() == 1
            && signature.matches(end).count() == 1
            && !signature.contains('\0'),
        "Signer did not return a complete {} signature",
        signer.format.label()
    );
    if !signature.ends_with('\n') {
        signature.push('\n');
    }
    fs::write(&signature_path, &signature)?;
    control.phase("Signing: verifying signature…");
    control.check()?;
    let mut verify = Command::new(&signer.program);
    match signer.format {
        TagSigningFormat::OpenPgp => {
            verify
                .args(["--no-tty", "--verify"])
                .arg(&signature_path)
                .arg(&input);
        }
        TagSigningFormat::Ssh => {
            verify
                .args(["-Y", "check-novalidate", "-n", "git", "-s"])
                .arg(&signature_path);
        }
    }
    run_tool(root, &mut verify, &input, dir.path(), control)
        .context("Signature verification failed")?;
    Ok(signature)
}
struct SignerChild {
    child: Child,
    finished: bool,
}
impl Drop for SignerChild {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
pub(super) fn run_tool(
    root: &Path,
    command: &mut Command,
    input: &Path,
    dir: &Path,
    control: &NetworkControl,
) -> Result<Vec<u8>> {
    control.check()?;
    let output = tempfile::NamedTempFile::new_in(dir)?;
    let errors = tempfile::NamedTempFile::new_in(dir)?;
    command
        .current_dir(root)
        .stdin(fs::File::open(input)?)
        .stdout(Stdio::from(output.reopen()?))
        .stderr(Stdio::from(errors.reopen()?));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = SignerChild {
        child: command
            .spawn()
            .context("Cannot start external tool; check its executable path")?,
        finished: false,
    };
    let start = Instant::now();
    let status = loop {
        control.check()?;
        ensure!(
            start.elapsed() < Duration::from_secs(300),
            "External tool timed out after 5 minutes"
        );
        ensure!(
            output.as_file().metadata()?.len() <= OUTPUT_LIMIT
                && errors.as_file().metadata()?.len() <= OUTPUT_LIMIT,
            "External tool output exceeds 1 MiB"
        );
        if let Some(status) = child.child.try_wait()? {
            child.finished = true;
            break status;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    control.check()?;
    ensure!(
        output.as_file().metadata()?.len() <= OUTPUT_LIMIT
            && errors.as_file().metadata()?.len() <= OUTPUT_LIMIT,
        "External tool output exceeds 1 MiB"
    );
    if !status.success() {
        let text = fs::read(errors.path())?;
        anyhow::bail!(
            "External tool failed ({status}): {}",
            String::from_utf8_lossy(&text)
                .chars()
                .take(2048)
                .collect::<String>()
        );
    }
    Ok(fs::read(output.path())?)
}
