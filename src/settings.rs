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

#[derive(Default, Serialize, Deserialize)]
pub struct Settings {
    pub recent: Vec<PathBuf>,
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
        match serde_json::from_slice(&bytes) {
            Ok(settings) => settings,
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
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(12);
        let file = Self::path();
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}
