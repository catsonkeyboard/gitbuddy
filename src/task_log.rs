//! Bounded task history shared by workers and the UI. No command/credential
//! capture: only operation labels, progress and sanitized result messages.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_TASKS: usize = 100;
const MAX_EVENTS: usize = 128;
const MAX_TEXT: usize = 2048;
const MAX_BYTES: usize = 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Outcome {
    Running,
    Success,
    Failed,
    Cancelled,
    Interrupted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub at: String,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    #[serde(with = "crate::session::path_encoding")]
    pub repository: PathBuf,
    pub label: String,
    pub started: String,
    pub finished: Option<String>,
    pub outcome: Outcome,
    pub events: VecDeque<Event>,
    pub dropped: usize,
}
#[derive(Default, Serialize, Deserialize)]
struct History {
    next: u64,
    records: VecDeque<Record>,
}
pub struct Store {
    path: PathBuf,
    history: Mutex<History>,
}
fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}
/// Strip HTTP credentials, query strings and terminal control sequences before
/// retaining output. UTF-8 boundaries are preserved when truncating messages.
pub fn sanitize(text: &str) -> String {
    let mut result = text
        .split_whitespace()
        .map(|word| {
            if let Some((scheme, rest)) = word.split_once("://") {
                let rest = rest.split(['?', '#']).next().unwrap_or(rest);
                let slash = rest.find('/').unwrap_or(rest.len());
                let authority = &rest[..slash];
                let host = authority
                    .rsplit_once('@')
                    .map_or(authority, |(_, host)| host);
                format!("{scheme}://{host}{}", &rest[slash..])
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    result.retain(|c| !c.is_control());
    if result.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !result.is_char_boundary(end) {
            end -= 1;
        }
        result.truncate(end);
        result.push_str("… [truncated]");
    }
    result
}
impl Store {
    pub fn open(path: PathBuf) -> Self {
        let mut history: History = match std::fs::metadata(&path) {
            Ok(info) if info.len() <= (MAX_BYTES * 2) as u64 => std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
            _ => History::default(),
        };
        for record in &mut history.records {
            if record.outcome == Outcome::Running {
                record.outcome = Outcome::Interrupted;
                record.finished = Some(now());
            }
            record.label = sanitize(&record.label);
            for event in &mut record.events {
                event.text = sanitize(&event.text);
            }
            while record.events.len() > MAX_EVENTS {
                record.events.pop_front();
                record.dropped += 1;
            }
            history.next = history.next.max(record.id);
        }
        Self::trim(&mut history);
        Self {
            path,
            history: Mutex::new(history),
        }
    }
    pub fn start(&self, repository: &Path, label: &str) -> u64 {
        let mut h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        h.next = h.next.saturating_add(1);
        let id = h.next;
        h.records.push_back(Record {
            id,
            repository: repository.into(),
            label: sanitize(label),
            started: now(),
            finished: None,
            outcome: Outcome::Running,
            events: VecDeque::new(),
            dropped: 0,
        });
        Self::trim(&mut h);
        id
    }
    pub fn event(&self, id: u64, text: &str) {
        let mut h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = h.records.iter_mut().find(|r| r.id == id) {
            let text = sanitize(text);
            if record.events.back().is_none_or(|event| event.text != text) {
                record.events.push_back(Event { at: now(), text });
                while record.events.len() > MAX_EVENTS {
                    record.events.pop_front();
                    record.dropped += 1;
                }
            }
        }
        Self::trim(&mut h);
    }
    pub fn finish(&self, id: u64, outcome: Outcome, message: &str) -> anyhow::Result<()> {
        self.event(id, message);
        let mut h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = h.records.iter_mut().find(|r| r.id == id) {
            record.outcome = outcome;
            record.finished = Some(now());
        }
        Self::trim(&mut h);
        self.save(&h)
    }
    pub fn records(&self, repository: Option<&Path>) -> Vec<Record> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .iter()
            .rev()
            .filter(|r| repository.is_none_or(|path| r.repository == path))
            .cloned()
            .collect()
    }
    pub fn checkpoint(&self) -> anyhow::Result<()> {
        let h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        self.save(&h)
    }
    pub fn clear_finished(&self, repository: Option<&Path>) -> anyhow::Result<()> {
        let mut h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::mem::take(&mut h.records);
        h.records = previous
            .iter()
            .filter(|r| {
                r.outcome == Outcome::Running || repository.is_some_and(|p| r.repository != p)
            })
            .cloned()
            .collect();
        if let Err(e) = self.save(&h) {
            h.records = previous;
            return Err(e);
        }
        Ok(())
    }
    fn trim(h: &mut History) {
        // Conservative serialized upper bound without doing JSON serialization
        // for each progress callback (escapes cost at most six bytes per byte).
        let size = |record: &Record| {
            512 + 6
                * (record.label.len()
                    + record.repository.as_os_str().len()
                    + record.started.len()
                    + record.finished.as_ref().map_or(0, String::len)
                    + record
                        .events
                        .iter()
                        .map(|event| 64 + event.at.len() + event.text.len())
                        .sum::<usize>())
        };
        let mut bytes: usize = h.records.iter().map(size).sum();
        while h.records.len() > MAX_TASKS || bytes > MAX_BYTES {
            if h.records.len() <= MAX_TASKS
                && h.records.iter().all(|r| r.outcome == Outcome::Running)
                && let Some(record) = h
                    .records
                    .iter_mut()
                    .filter(|r| !r.events.is_empty())
                    .max_by_key(|r| size(r))
            {
                let before = size(record);
                record.events.pop_front();
                record.dropped += 1;
                bytes -= before - size(record);
                continue;
            }
            // Prefer retaining live tasks during a long transfer.
            let position = h
                .records
                .iter()
                .position(|r| r.outcome != Outcome::Running)
                .unwrap_or(0);
            let Some(record) = h.records.remove(position) else {
                break;
            };
            bytes = bytes.saturating_sub(size(&record));
        }
    }
    fn save(&self, h: &History) -> anyhow::Result<()> {
        use std::io::Write;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Missing task log directory"))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec(h)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_progress_and_recovers_interrupted_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let log = Store::open(path.clone());
        let id = log.start(Path::new("/repo"), "Fetch");
        for i in 0..200 {
            log.event(id, &format!("progress {i}"));
        }
        let other = log.start(Path::new("/other"), "Push");
        log.finish(other, Outcome::Failed, "server rejected")
            .unwrap();
        let log = Store::open(path);
        let rows = log.records(Some(Path::new("/repo")));
        assert_eq!(rows[0].outcome, Outcome::Interrupted);
        assert_eq!(rows[0].events.len(), MAX_EVENTS);
        assert_eq!(rows[0].dropped, 72);
        log.clear_finished(Some(Path::new("/repo"))).unwrap();
        assert_eq!(log.records(None).len(), 1);
    }
    #[test]
    fn task_count_and_utf8_text_are_bounded_and_urls_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let log = Store::open(dir.path().join("tasks.json"));
        for _ in 0..150 {
            let id = log.start(Path::new("/repo"), "Update");
            log.finish(id, Outcome::Success, "ok").unwrap();
        }
        assert_eq!(log.records(None).len(), MAX_TASKS);
        assert_eq!(
            sanitize("https://user:secret@example.com/repo?token=secret#x"),
            "https://example.com/repo"
        );
        assert!(sanitize(&"中".repeat(1000)).len() <= MAX_TEXT + 20);
    }
    #[test]
    fn total_budget_preserves_live_tasks_and_clear_preserves_other_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let log = Store::open(path.clone());
        let id = log.start(Path::new("/repo"), "Fetch");
        for i in 0..200 {
            log.event(id, &format!("{i} {}", "x".repeat(MAX_TEXT)));
        }
        assert_eq!(log.records(None).len(), 1);
        assert!(log.records(None)[0].dropped > 72);
        log.checkpoint().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES as u64);
        let finished = log.start(Path::new("/repo"), "Push");
        log.finish(finished, Outcome::Cancelled, "Cancelled")
            .unwrap();
        let other = log.start(Path::new("/other"), "Refresh");
        log.finish(other, Outcome::Success, "ok").unwrap();
        log.clear_finished(Some(Path::new("/repo"))).unwrap();
        let rows = log.records(None);
        assert!(
            rows.iter()
                .any(|r| r.id == id && r.outcome == Outcome::Running)
        );
        assert!(rows.iter().any(|r| r.id == other));
        assert!(!rows.iter().any(|r| r.id == finished));
    }
}
