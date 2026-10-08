//! Portable UI state. No repository writes or network tasks are replayed.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum EncodedPath {
    Text(String),
    Unix { unix: Vec<u8> },
    Windows { windows: Vec<u16> },
}
impl EncodedPath {
    pub(crate) fn from_path(path: &std::path::Path) -> Self {
        if let Some(text) = path.to_str() {
            return Self::Text(text.into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::Unix {
                unix: path.as_os_str().as_bytes().to_vec(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::Windows {
                windows: path.as_os_str().encode_wide().collect(),
            }
        }
    }
    pub(crate) fn into_path(self) -> Result<PathBuf, String> {
        match self {
            Self::Text(text) => Ok(text.into()),
            #[cfg(unix)]
            Self::Unix { unix } => {
                use std::os::unix::ffi::OsStringExt;
                Ok(std::ffi::OsString::from_vec(unix).into())
            }
            #[cfg(windows)]
            Self::Windows { windows } => {
                use std::os::windows::ffi::OsStringExt;
                Ok(std::ffi::OsString::from_wide(&windows).into())
            }
            _ => Err("Session contains a path encoded for another platform".into()),
        }
    }
}
pub(crate) mod paths_encoding {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        paths: &[PathBuf],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        paths
            .iter()
            .map(|p| EncodedPath::from_path(p))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<EncodedPath>::deserialize(deserializer)?
            .into_iter()
            .map(|p| p.into_path().map_err(serde::de::Error::custom))
            .collect()
    }
}
pub(crate) mod path_encoding {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        path: &std::path::Path,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        EncodedPath::from_path(path).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        EncodedPath::deserialize(deserializer)?
            .into_path()
            .map_err(serde::de::Error::custom)
    }
}
mod optional_path_encoding {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_ref()
            .map(|p| EncodedPath::from_path(p))
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Option::<EncodedPath>::deserialize(deserializer)?
            .map(EncodedPath::into_path)
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub version: u32,
    pub tabs: Vec<Tab>,
    pub active: usize,
    #[serde(default)]
    pub tab_scroll: Offset,
}
impl Default for Session {
    fn default() -> Self {
        Self {
            version: 1,
            tabs: Vec::new(),
            active: 0,
            tab_scroll: Offset::default(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Tab {
    #[serde(with = "path_encoding")]
    pub path: PathBuf,
    pub message: String,
    pub message_view: EditorView,
    pub amend: Option<AmendDraft>,
    pub query: String,
    pub selection: Selection,
    pub selected_commits: Vec<String>,
    pub commit_anchor: Option<String>,
    pub inspection: Option<Inspection>,
    pub comparison_base: Option<String>,
    pub history_tab: usize,
    pub history_limit: usize,
    pub show_commit_body: bool,
    pub expanded: Vec<PatchKey>,
    pub diff_preferences: crate::git::DiffPreferences,
    pub lines: Vec<LineSelection>,
    pub scroll: BTreeMap<String, Offset>,
    #[serde(with = "optional_path_encoding")]
    pub conflict_file: Option<PathBuf>,
    pub conflict_drafts: Vec<ConflictDraft>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum Selection {
    #[default]
    Work,
    File(#[serde(with = "path_encoding")] PathBuf, bool),
    Commit(String),
    Inspect,
    Conflicts,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PatchKey {
    Work(#[serde(with = "path_encoding")] PathBuf, bool),
    Commit(String, #[serde(with = "path_encoding")] PathBuf),
    Compare(String, String, #[serde(with = "path_encoding")] PathBuf),
    Stash(String, String, #[serde(with = "path_encoding")] PathBuf),
}
impl PatchKey {
    pub fn scroll_id(&self) -> String {
        format!(
            "patch:{}",
            serde_json::to_string(self).expect("serializable patch key")
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Inspection {
    History(#[serde(with = "path_encoding")] PathBuf, String, usize),
    Blame(#[serde(with = "path_encoding")] PathBuf, String),
    Compare(String, String),
    Stash(String),
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Offset {
    pub x: f32,
    pub y: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LineSelection {
    pub key: PatchKey,
    pub fingerprint: u64,
    pub rows: Vec<usize>,
    pub anchor: Option<usize>,
}
impl LineSelection {
    pub fn restore(
        &self,
        lines: &[crate::git::DiffLine],
    ) -> Option<(std::collections::BTreeSet<usize>, Option<usize>)> {
        if self.fingerprint != patch_fingerprint(lines) {
            return None;
        }
        let rows = self
            .rows
            .iter()
            .copied()
            .filter(|&i| {
                lines
                    .get(i)
                    .is_some_and(|line| matches!(line.kind, '+' | '-'))
            })
            .collect::<std::collections::BTreeSet<_>>();
        let anchor = self.anchor.filter(|i| rows.contains(i));
        Some((rows, anchor))
    }
}
// A deterministic fingerprint across app versions/restarts. Selection is restored
// only against identical diff content, never against newly shifted line numbers.
pub fn patch_fingerprint(lines: &[crate::git::DiffLine]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for line in lines {
        for byte in format!("{:?}\0", line).bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConflictChoice {
    Edited,
    Ours,
    Theirs,
    Worktree,
    Delete,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConflictDraft {
    #[serde(with = "path_encoding")]
    pub path: PathBuf,
    pub text: String,
    pub choice: ConflictChoice,
    pub dirty: bool,
    pub fingerprint: u64,
    #[serde(default)]
    pub editor: EditorView,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EditorView {
    pub start: usize,
    pub end: usize,
    pub scroll: Offset,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AmendDraft {
    pub head: String,
    pub reference: String,
    pub message: String,
    #[serde(default)]
    pub editor: EditorView,
}
pub fn conflict_fingerprint(context: &crate::git::ConflictContext) -> u64 {
    // Includes libgit2's index/operation/worktree tokens, including blob IDs.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in format!("{context:?}").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    state: Arc<Mutex<WriteState>>,
}
#[derive(Default)]
struct WriteState {
    revision: u64,
    last: Option<Session>,
    blocked: bool,
}
impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Arc::new(Mutex::new(WriteState::default())),
        }
    }
    pub fn load(&self) -> Result<Option<Session>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                self.state.lock().unwrap().blocked = true;
                return Err(error).context("Cannot read saved session; automatic saving disabled");
            }
        };
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(version) = value.get("version").and_then(|v| v.as_u64())
            && version != 1
        {
            self.state.lock().unwrap().blocked = true;
            bail!("Unsupported session version {version}; automatic saving disabled");
        }
        let session: Session = match serde_json::from_slice(&bytes) {
            Ok(session) => session,
            Err(error) => {
                let backup = self.path.with_extension(format!(
                    "json.broken-{}",
                    chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
                ));
                if let Err(move_error) = std::fs::rename(&self.path, &backup) {
                    self.state.lock().unwrap().blocked = true;
                    bail!("Invalid session ({error}); cannot preserve it: {move_error}");
                }
                bail!("Invalid session preserved at {}: {error}", backup.display());
            }
        };
        if session.version != 1 {
            self.state.lock().unwrap().blocked = true;
            bail!(
                "Unsupported session version {}; automatic saving disabled",
                session.version
            );
        }
        self.state.lock().unwrap().last = Some(session.clone());
        Ok(Some(session))
    }
    pub fn save(&self, revision: u64, session: &Session) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.blocked || revision < state.revision {
            return Ok(());
        }
        if state.last.as_ref() == Some(session) {
            state.revision = revision;
            return Ok(());
        }
        let parent = self.path.parent().context("Session file has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(file.as_file_mut(), session)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .context("Cannot atomically replace saved session")?;
        state.revision = revision;
        state.last = Some(session.clone());
        Ok(())
    }
}
