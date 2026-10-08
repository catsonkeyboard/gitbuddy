use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("GITBUDDY_CONFIG_DIR") {
        return path.into();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".config/gitbuddy")
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, with = "crate::session::paths_encoding")]
    pub recent: Vec<PathBuf>,
    #[serde(default)]
    pub merge_tool: crate::git::MergeTool,
    #[serde(default)]
    pub preferences: Preferences,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub history_limit: usize,
    pub commit_cache_entries: usize,
    pub commit_cache_mb: usize,
    pub patch_cache_entries: usize,
    pub patch_cache_mb: usize,
    /// Empty binding disables an action; absent entries inherit defaults.
    pub shortcuts: std::collections::BTreeMap<String, String>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            history_limit: 10_000,
            commit_cache_entries: 64,
            commit_cache_mb: 16,
            patch_cache_entries: 16,
            patch_cache_mb: 64,
            shortcuts: Default::default(),
        }
    }
}
pub const SHORTCUTS: &[(&str, &str, &str)] = &[
    ("open", "Open repository", "secondary-o"),
    ("clone", "Clone repository", ""),
    ("init", "Initialize repository", ""),
    ("refresh", "Refresh", "secondary-r"),
    ("commit", "Commit staged changes", "secondary-enter"),
    ("amend", "Amend", ""),
    ("reflog", "Reflog", ""),
    ("compare", "Compare", ""),
    ("conflicts", "Resolve conflicts", ""),
    ("fetch", "Fetch", ""),
    ("pull", "Pull", ""),
    ("push", "Push", ""),
    ("next_tab", "Next repository", "ctrl-tab"),
    ("previous_tab", "Previous repository", "ctrl-shift-tab"),
    ("close_tab", "Close repository", "secondary-w"),
    (
        "cancel_task",
        "Cancel current task",
        "secondary-shift-escape",
    ),
    ("task_log", "Task log", "secondary-shift-l"),
    ("preferences", "Preferences", "secondary-,"),
    ("quit", "Quit", "secondary-q"),
    ("close_modal", "Close dialog", "escape"),
];
impl Preferences {
    pub fn binding(&self, action: &str) -> &str {
        self.shortcuts
            .get(action)
            .map(String::as_str)
            .unwrap_or_else(|| SHORTCUTS.iter().find(|s| s.0 == action).map_or("", |s| s.2))
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (300..=100_000).contains(&self.history_limit),
            "History limit must be 300–100,000"
        );
        anyhow::ensure!(
            (1..=256).contains(&self.commit_cache_entries)
                && (1..=64).contains(&self.patch_cache_entries),
            "Cache entries: commits 1–256, patches 1–64"
        );
        anyhow::ensure!(
            (1..=128).contains(&self.commit_cache_mb) && (16..=512).contains(&self.patch_cache_mb),
            "Cache MB: commits 1–128, patches 16–512"
        );
        for (action, binding) in &self.shortcuts {
            anyhow::ensure!(
                SHORTCUTS.iter().any(|s| s.0 == action),
                "Unknown shortcut action: {action}"
            );
            anyhow::ensure!(binding.len() <= 128, "Shortcut is too long: {action}");
        }
        Ok(())
    }
}
impl Settings {
    fn path() -> PathBuf {
        config_dir().join("settings.json")
    }
    pub fn load() -> Self {
        let path = Self::path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(error) => {
                eprintln!("Cannot read {}: {error}", path.display());
                return Self::default();
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut settings) => {
                if let Err(error) = settings.preferences.validate() {
                    eprintln!("Ignoring invalid performance preferences: {error}");
                    settings.preferences = Preferences::default();
                }
                settings
            }
            Err(error) => {
                // Keep the broken file for inspection instead of silently
                // overwriting the user's recent list on the next save.
                let backup = path.with_extension("json.broken");
                if std::fs::rename(&path, &backup).is_err() {
                    eprintln!("Cannot move the unreadable settings file away either.");
                }
                eprintln!(
                    "Ignoring invalid settings at {}: {error}; the file was moved to {}",
                    path.display(),
                    backup.display()
                );
                Self::default()
            }
        }
    }
    pub fn remember(&mut self, path: PathBuf) -> anyhow::Result<()> {
        self.remember_at(path, &Self::path())
    }
    pub fn set_merge_tool(&mut self, tool: crate::git::MergeTool) -> anyhow::Result<()> {
        tool.validate()?;
        let mut next = self.clone();
        next.merge_tool = tool;
        next.save_at(&Self::path())?;
        *self = next;
        Ok(())
    }
    pub fn set_preferences(&mut self, preferences: Preferences) -> anyhow::Result<()> {
        preferences.validate()?;
        let mut next = self.clone();
        next.preferences = preferences;
        next.save_at(&Self::path())?;
        *self = next;
        Ok(())
    }
    fn remember_at(&mut self, path: PathBuf, file: &std::path::Path) -> anyhow::Result<()> {
        let mut next = self.clone();
        next.recent.retain(|p| p != &path);
        next.recent.insert(0, path);
        next.recent.truncate(12);
        next.save_at(file)?;
        *self = next;
        Ok(())
    }
    fn save_at(&self, file: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write;
        let bytes = serde_json::to_vec_pretty(self)?;
        let parent = file
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Missing settings parent"))?;
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(file)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_default_migration_validation_and_persistence() {
        let legacy: Settings = serde_json::from_str(r#"{"recent":["/repo"]}"#).unwrap();
        assert_eq!(legacy.preferences, Preferences::default());
        let mut settings = legacy;
        settings.preferences.history_limit = 600;
        settings
            .preferences
            .shortcuts
            .insert("open".into(), "ctrl-k ctrl-o".into());
        settings.preferences.validate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        settings.remember_at("/another".into(), &path).unwrap();
        let restored: Settings = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(restored.preferences, settings.preferences);
        settings.preferences.patch_cache_mb = usize::MAX;
        assert!(settings.preferences.validate().is_err());
        assert!(serde_json::from_str::<Preferences>(r#"{"unknown":12}"#).is_err());
    }
    #[test]
    fn merge_tool_survives_recent_repository_updates_and_old_settings_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let tool = crate::git::MergeTool {
            program: "/tmp/tool with spaces".into(),
            args: vec!["$LOCAL".into(), "$MERGED".into()],
        };
        let mut settings = Settings {
            merge_tool: tool.clone(),
            ..Settings::default()
        };
        settings.remember_at("/repo".into(), &path).unwrap();
        let restored: Settings = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(restored.merge_tool, tool);
        assert_eq!(
            serde_json::from_str::<Settings>(r#"{"recent":[]}"#)
                .unwrap()
                .merge_tool,
            crate::git::MergeTool::default()
        );
    }
    #[test]
    fn legacy_recent_strings_remain_readable_and_order_is_preserved() {
        let mut settings: Settings =
            serde_json::from_str(r#"{"recent":["/old/a","/old/b"]}"#).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        settings.remember_at("/old/b".into(), &file).unwrap();
        for i in 0..15 {
            settings
                .remember_at(format!("/new/{i}").into(), &file)
                .unwrap();
        }
        assert_eq!(settings.recent.len(), 12);
        let restored: Settings = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(restored.recent, settings.recent);
        assert_eq!(restored.recent[0], PathBuf::from("/new/14"));
        assert!(
            serde_json::from_str::<Settings>("{}")
                .unwrap()
                .recent
                .is_empty()
        );
    }
    #[cfg(unix)]
    #[test]
    fn non_utf8_recent_path_round_trips_without_poisoning_later_saves() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let invalid = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"repo-\xff".to_vec()));
        let mut settings = Settings::default();
        settings.remember_at(invalid.clone(), &file).unwrap();
        settings.remember_at("/normal/repo".into(), &file).unwrap();
        let restored: Settings = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(
            restored.recent,
            vec![PathBuf::from("/normal/repo"), invalid]
        );
    }
    #[test]
    fn failed_atomic_save_keeps_previous_in_memory_and_disk_values() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.remember_at("/first".into(), &file).unwrap();
        let previous = std::fs::read(&file).unwrap();
        let invalid = dir.path().join("not-a-directory");
        std::fs::write(&invalid, b"block").unwrap();
        assert!(
            settings
                .remember_at("/second".into(), &invalid.join("settings.json"))
                .is_err()
        );
        assert_eq!(settings.recent, vec![PathBuf::from("/first")]);
        assert_eq!(std::fs::read(&file).unwrap(), previous);
        // Failure of the final rename must also leave the in-memory list intact.
        let directory = dir.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(settings.remember_at("/third".into(), &directory).is_err());
        assert_eq!(settings.recent, vec![PathBuf::from("/first")]);
    }
}
