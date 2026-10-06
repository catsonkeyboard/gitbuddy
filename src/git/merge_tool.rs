//! External tools receive isolated files and an argument vector, never a shell command.
use super::{ConflictContext, NetworkControl, Repository};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergeTool {
    pub program: String,
    pub args: Vec<String>,
}
impl Default for MergeTool {
    fn default() -> Self {
        Self {
            program: "code".into(),
            args: ["--wait", "--merge", "$LOCAL", "$REMOTE", "$BASE", "$MERGED"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
    }
}
impl MergeTool {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.program.trim().is_empty() && !self.program.contains('\0'),
            "Enter the merge tool executable path."
        );
        ensure!(
            self.args.len() <= 64
                && self
                    .args
                    .iter()
                    .all(|s| s.len() <= 4096 && !s.contains('\0')),
            "Invalid merge tool arguments."
        );
        ensure!(
            self.args.iter().any(|s| s == "$MERGED"),
            "Arguments must contain a separate $MERGED output placeholder."
        );
        for arg in &self.args {
            if ["$BASE", "$LOCAL", "$REMOTE", "$MERGED"]
                .iter()
                .any(|token| arg.contains(token))
            {
                ensure!(
                    ["$BASE", "$LOCAL", "$REMOTE", "$MERGED"].contains(&arg.as_str()),
                    "Each file placeholder must be a separate argument."
                );
            }
        }
        Ok(())
    }
    fn arguments(&self, base: &Path, local: &Path, remote: &Path, merged: &Path) -> Vec<OsString> {
        self.args
            .iter()
            .map(|arg| match arg.as_str() {
                "$BASE" => base.as_os_str().to_owned(),
                "$LOCAL" => local.as_os_str().to_owned(),
                "$REMOTE" => remote.as_os_str().to_owned(),
                "$MERGED" => merged.as_os_str().to_owned(),
                _ => OsString::from(arg),
            })
            .collect()
    }
}
impl Repository {
    /// Return a reviewed draft candidate. No worktree, index, or ref is written.
    pub fn run_merge_tool(
        &self,
        context: &ConflictContext,
        draft: &str,
        tool: &MergeTool,
        control: NetworkControl,
    ) -> Result<String> {
        tool.validate()?;
        ensure!(
            context.can_external_merge(),
            "External merge preview currently supports UTF-8 text conflicts up to 2 MB per side only."
        );
        ensure!(
            draft.len() <= 2 * 1024 * 1024,
            "Conflict draft is larger than 2 MB."
        );
        control.check()?;
        self.validate_tool_context(context)?;
        let dir = tempfile::Builder::new()
            .prefix("gitbuddy-merge-")
            .tempdir()?;
        let name = context
            .file
            .path
            .file_name()
            .context("Missing conflict filename")?;
        let make = |side: &str, text: &str| -> Result<std::path::PathBuf> {
            let parent = dir.path().join(side);
            fs::create_dir(&parent)?;
            let file = parent.join(name);
            fs::write(&file, text)?;
            Ok(file)
        };
        let base = make(
            "BASE",
            context
                .base
                .as_ref()
                .and_then(|s| s.text.as_deref())
                .unwrap_or_default(),
        )?;
        let local = make(
            "LOCAL",
            context
                .ours
                .as_ref()
                .and_then(|s| s.text.as_deref())
                .unwrap_or_default(),
        )?;
        let remote = make(
            "REMOTE",
            context
                .theirs
                .as_ref()
                .and_then(|s| s.text.as_deref())
                .unwrap_or_default(),
        )?;
        let merged = make("MERGED", draft)?;
        let mut command = Command::new(&tool.program);
        command
            .args(tool.arguments(&base, &local, &remote, &merged))
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "Cannot launch {}. Use an installed executable, or its absolute path.",
                tool.program
            )
        })?;
        let status = loop {
            if let Err(error) = control.check() {
                stop_child(&mut child);
                return Err(error);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(error) => {
                    stop_child(&mut child);
                    return Err(error.into());
                }
            }
        };
        control.check()?;
        // Keep unsuccessful results for manual recovery, but never import them.
        let result = (|| -> Result<String> {
            ensure!(status.success(), "Merge tool exited with {status}");
            ensure!(
                fs::symlink_metadata(&merged)?.file_type().is_file(),
                "Merge tool output must be a regular file."
            );
            let mut bytes = Vec::new();
            fs::File::open(&merged)?
                .take(2 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 2 * 1024 * 1024,
                "Merge tool output exceeds 2 MB."
            );
            ensure!(
                !bytes.contains(&0),
                "Merge tool output contains binary data."
            );
            let text = String::from_utf8(bytes).context("Merge tool output is not UTF-8 text")?;
            self.validate_tool_context(context)?;
            Ok(text)
        })();
        match result {
            Ok(text) => Ok(text),
            Err(error) => {
                let kept = dir.keep();
                Err(error.context(format!(
                    "Tool result kept at {}. Reload the conflict before trying again.",
                    kept.join("MERGED").join(name).display()
                )))
            }
        }
    }
}
fn stop_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}
