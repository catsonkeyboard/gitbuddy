use super::Repository;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ExternalEditor {
    pub program: String,
    pub args: Vec<String>,
}
impl Default for ExternalEditor {
    fn default() -> Self {
        Self {
            program: "code".into(),
            args: vec!["$FILE".into()],
        }
    }
}
impl ExternalEditor {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.program.trim().is_empty()
                && self.program.len() <= 4096
                && !self.program.contains('\0'),
            "Enter an executable path"
        );
        ensure!(
            self.args.len() <= 64
                && self
                    .args
                    .iter()
                    .all(|s| s.len() <= 4096 && !s.contains('\0')),
            "Invalid editor arguments"
        );
        ensure!(
            self.args.iter().filter(|s| s.as_str() == "$FILE").count() == 1,
            "Arguments must contain one separate $FILE placeholder"
        );
        ensure!(
            self.args
                .iter()
                .all(|s| !s.contains("$FILE") || s == "$FILE"),
            "$FILE must be a separate argument"
        );
        Ok(())
    }
}
impl Repository {
    pub fn launch_editor(&self, path: &Path, tool: &ExternalEditor) -> Result<()> {
        tool.validate()?;
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "Invalid repository path"
        );
        let path = self.root.join(path);
        ensure!(
            path.is_file(),
            "Working file no longer exists or is not a regular file"
        );
        let args = tool
            .args
            .iter()
            .map(|s| {
                if s == "$FILE" {
                    path.as_os_str().to_owned()
                } else {
                    OsString::from(s)
                }
            })
            .collect::<Vec<_>>();
        let mut child = Command::new(&tool.program)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("Cannot start editor {}", tool.program))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}
