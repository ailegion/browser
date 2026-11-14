//! Browsing history (block 1 of the address bar's suggestions): every page
//! opened, with its title, how often it was visited and typed, and when it
//! was last visited. Written to the profile database as it changes (plan
//! D02) on a thread of its own, so the shell never waits on the disk; kept
//! in memory too, for the address bar to search.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use turso_core::Value;

use crate::db::{as_int, as_text};
use crate::{Store, StoreError};

/// One page in the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub url: String,
    pub title: Option<String>,
    pub visit_count: u32,
    /// How many of the visits were typed in the address bar.
    pub typed_count: u32,
    /// Milliseconds since the Unix epoch.
    pub last_visit: i64,
}

impl Store {
    /// Count a visit to `url` at `at` (ms since the epoch): a new entry,
    /// or one more visit (and typed visit) of the old, its title kept
    /// unless a new one is known.
    pub fn record_visit(&self, url: &str, title: Option<&str>, typed: bool, at: i64) -> Result<(), StoreError> {
        let text = |s: &str| Value::build_text(s.to_owned());
        let title_value = title.map(text).unwrap_or(Value::Null);
        let known = !self.query("SELECT 1 FROM history WHERE url = ?1", &[text(url)])?.is_empty();
        if known {
            self.execute(
                "UPDATE history SET visit_count = visit_count + 1, typed_count = typed_count + ?2, last_visit = ?3, title = COALESCE(?4, title) WHERE url = ?1",
                &[text(url), Value::from_i64(i64::from(typed)), Value::from_i64(at), title_value],
            )
        } else {
            self.execute(
                "INSERT INTO history (url, title, visit_count, typed_count, last_visit) VALUES (?1, ?2, 1, ?3, ?4)",
                &[text(url), title_value, Value::from_i64(i64::from(typed)), Value::from_i64(at)],
            )
        }
    }

    /// The page at `url` got a title (it arrives after the visit).
    pub fn set_history_title(&self, url: &str, title: &str) -> Result<(), StoreError> {
        self.execute(
            "UPDATE history SET title = ?2 WHERE url = ?1",
            &[Value::build_text(url.to_owned()), Value::build_text(title.to_owned())],
        )
    }

    /// Every entry, the most recent first.
    pub fn history(&self) -> Result<Vec<HistoryEntry>, StoreError> {
        let rows = self.query("SELECT url, title, visit_count, typed_count, last_visit FROM history ORDER BY last_visit DESC, rowid DESC", &[])?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(HistoryEntry {
                    url: as_text(r.first()?)?,
                    title: r.get(1).and_then(as_text),
                    visit_count: u32::try_from(as_int(r.get(2)?)?).unwrap_or(u32::MAX),
                    typed_count: u32::try_from(as_int(r.get(3)?)?).unwrap_or(u32::MAX),
                    last_visit: as_int(r.get(4)?)?,
                })
            })
            .collect())
    }
}

/// The same change to the in-memory list as `record_visit` makes to the
/// table; the entry moves to the front.
fn apply_visit(entries: &mut Vec<HistoryEntry>, url: &str, title: Option<&str>, typed: bool, at: i64) {
    let mut entry = match entries.iter().position(|e| e.url == url) {
        Some(i) => entries.remove(i),
        None => HistoryEntry {
            url: url.to_owned(),
            title: None,
            visit_count: 0,
            typed_count: 0,
            last_visit: at,
        },
    };
    entry.visit_count = entry.visit_count.saturating_add(1);
    entry.typed_count = entry.typed_count.saturating_add(u32::from(typed));
    entry.last_visit = at;
    if let Some(t) = title {
        entry.title = Some(t.to_owned());
    }
    entries.insert(0, entry);
}

enum Op {
    Visit { url: String, title: Option<String>, typed: bool, at: i64 },
    Title { url: String, title: String },
}

/// The history for the shell: changes go to the database on the store's
/// thread; the list in memory is current at once.
#[allow(missing_debug_implementations)]
pub struct HistoryService {
    ops: Option<Sender<Op>>,
    entries: Arc<Mutex<Vec<HistoryEntry>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Dropped (the browser closing): the writes still queued are made before
/// it returns.
impl Drop for HistoryService {
    fn drop(&mut self) {
        self.ops = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl HistoryService {
    /// Open the profile in `dir` on a new thread and load its history.
    pub fn start(dir: PathBuf) -> Result<Self, StoreError> {
        let (ops, inbox) = channel::<Op>();
        let (opened, open_result) = channel::<Result<Vec<HistoryEntry>, StoreError>>();
        let thread = std::thread::Builder::new()
            .name("store".into())
            .spawn(move || {
                let store = match Store::open(&dir).and_then(|s| s.history().map(|h| (s, h))) {
                    Ok((store, history)) => {
                        let _ = opened.send(Ok(history));
                        store
                    }
                    Err(e) => {
                        let _ = opened.send(Err(e));
                        return;
                    }
                };
                for op in inbox {
                    let result = match op {
                        Op::Visit { url, title, typed, at } => store.record_visit(&url, title.as_deref(), typed, at),
                        Op::Title { url, title } => store.set_history_title(&url, &title),
                    };
                    if let Err(e) = result {
                        tracing::error!("history write failed: {e}");
                    }
                }
            })?;
        let history = open_result.recv().map_err(|_| StoreError::Db("the store thread ended".into()))??;
        Ok(Self {
            ops: Some(ops),
            entries: Arc::new(Mutex::new(history)),
            thread: Some(thread),
        })
    }

    /// A page was opened.
    pub fn visit(&self, url: &str, title: Option<&str>, typed: bool) {
        let at = now_ms();
        if let Ok(mut entries) = self.entries.lock() {
            apply_visit(&mut entries, url, title, typed, at);
        }
        let Some(ops) = &self.ops else { return };
        let _ = ops.send(Op::Visit {
            url: url.to_owned(),
            title: title.map(str::to_owned),
            typed,
            at,
        });
    }

    /// The page at `url` got its title.
    pub fn set_title(&self, url: &str, title: &str) {
        // Only a page in the history, and only a new title, is written.
        {
            let Ok(mut entries) = self.entries.lock() else { return };
            let Some(e) = entries.iter_mut().find(|e| e.url == url) else { return };
            if e.title.as_deref() == Some(title) {
                return;
            }
            e.title = Some(title.to_owned());
        }
        let Some(ops) = &self.ops else { return };
        let _ = ops.send(Op::Title {
            url: url.to_owned(),
            title: title.to_owned(),
        });
    }

    /// The history as it stands, the most recent first.
    pub fn entries(&self) -> Vec<HistoryEntry> {
        self.entries.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_profile(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("browser-store-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn visits_are_kept_across_a_restart_and_the_file_is_encrypted() {
        let dir = temp_profile("restart");
        {
            let store = Store::open(&dir).expect("open");
            store.record_visit("https://a.test/", None, true, 1000).expect("visit");
            store.set_history_title("https://a.test/", "A").expect("title");
            store.record_visit("https://b.test/", Some("B"), false, 2000).expect("visit");
            store.record_visit("https://a.test/", None, false, 3000).expect("visit");
        }
        let store = Store::open(&dir).expect("reopen");
        let h = store.history().expect("history");
        assert_eq!(
            h,
            vec![
                HistoryEntry { url: "https://a.test/".into(), title: Some("A".into()), visit_count: 2, typed_count: 1, last_visit: 3000 },
                HistoryEntry { url: "https://b.test/".into(), title: Some("B".into()), visit_count: 1, typed_count: 0, last_visit: 2000 },
            ]
        );
        drop(store);
        // Nothing of it is readable in the file.
        let bytes = std::fs::read(dir.join("browser.db")).expect("db file");
        let plain = |s: &str| bytes.windows(s.len()).any(|w| w == s.as_bytes());
        assert!(!plain("a.test") && !plain("history") && !plain("SQLite format 3"), "the database file is encrypted");
        // A wrong key does not open it.
        std::fs::write(dir.join("key"), "00".repeat(32)).expect("key");
        assert!(Store::open(&dir).and_then(|s| s.history()).is_err(), "the wrong key reads nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_service_keeps_memory_and_disk_in_step() {
        let dir = temp_profile("service");
        {
            let service = HistoryService::start(dir.clone()).expect("start");
            service.visit("https://a.test/", None, true);
            service.set_title("https://a.test/", "A");
            service.visit("https://b.test/", Some("B"), false);
            let urls: Vec<String> = service.entries().into_iter().map(|e| e.url).collect();
            assert_eq!(urls, ["https://b.test/", "https://a.test/"], "most recent first, at once");
        }
        // The thread ends with the service; what it wrote is there.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let h = loop {
            match Store::open(&dir).and_then(|s| s.history()) {
                Ok(h) if h.len() == 2 => break h,
                _ if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
                other => panic!("history not written: {other:?}"),
            }
        };
        let a = h.iter().find(|e| e.url == "https://a.test/").expect("a");
        assert_eq!((a.title.as_deref(), a.typed_count, a.visit_count), (Some("A"), 1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
