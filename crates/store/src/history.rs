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

impl Store {
    /// Forget the page at `url` (Shift+Delete on its suggestion).
    pub fn remove_history(&self, url: &str) -> Result<(), StoreError> {
        self.execute("DELETE FROM history WHERE url = ?1", &[Value::build_text(url.to_owned())])
    }
}

/// The address bar's matches for `query` among `entries`, best first, at
/// most `limit`. Every word of the query must occur in the entry's
/// address (its scheme and a leading `www.` aside) or title, ignoring
/// case. The rank weighs how often and how recently the page was visited,
/// typed visits more (Firefox's frecency and Chrome's history scoring do
/// this, as recalled; their exact weights are not reproduced), and puts a
/// page whose host starts with the query first.
pub fn suggest(entries: &[HistoryEntry], query: &str, limit: usize, now: i64) -> Vec<HistoryEntry> {
    let query = query.trim().to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(f64, &HistoryEntry)> = entries
        .iter()
        .filter_map(|e| {
            let address = bare_address(&e.url);
            let title = e.title.as_deref().unwrap_or("").to_lowercase();
            if !words.iter().all(|w| address.contains(w) || title.contains(w)) {
                return None;
            }
            let host_start = words.len() == 1 && address.starts_with(words[0]);
            Some((score(e, now) * if host_start { 4.0 } else { 1.0 }, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.last_visit.cmp(&a.1.last_visit)));
    scored.into_iter().take(limit).map(|(_, e)| e.clone()).collect()
}

/// What the address bar fills in after `query`: the text the box then
/// holds (the query as typed, then the rest, which the box selects) and
/// the page it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Autofill {
    pub text: String,
    pub url: String,
}

/// Inline completion of a visited site (Firefox's origin autofill, as
/// recalled): the best-ranked entry whose address, scheme and `www.`
/// aside (`www.` kept when the query has it), starts with `query` fills
/// up to the end of its host and a `/`; a query that already reaches
/// past the host fills to the next `/`, or the address's end. A query
/// with a space, or a scheme, fills nothing.
pub fn autofill(entries: &[HistoryEntry], query: &str, now: i64) -> Option<Autofill> {
    if query.is_empty() || query.contains(char::is_whitespace) || query.contains("://") {
        return None;
    }
    let lower = query.to_lowercase();
    let keep_www = lower.starts_with("www.");
    let mut best: Option<(f64, Autofill)> = None;
    for e in entries {
        let Some((scheme, rest)) = e.url.split_once("://") else { continue };
        let (www, rest) = match rest.strip_prefix("www.") {
            Some(r) if !keep_www => ("www.", r),
            _ => ("", rest),
        };
        if !rest.to_lowercase().starts_with(&lower) {
            continue;
        }
        let host_end = rest.find('/').unwrap_or(rest.len());
        let end = if lower.len() < host_end {
            host_end
        } else {
            // Past the host: to the next `/` after the query, kept.
            rest[lower.len()..].find('/').map_or(rest.len(), |i| lower.len() + i + 1)
        };
        let mut filled = rest[..end].to_owned();
        if end == host_end && !filled.ends_with('/') {
            filled.push('/');
        }
        if filled.len() <= query.len() && filled.to_lowercase() == lower {
            // Nothing left to fill.
            continue;
        }
        let candidate = Autofill {
            text: format!("{query}{}", &filled[query.len()..]),
            url: format!("{scheme}://{www}{filled}"),
        };
        let s = score(e, now);
        if best.as_ref().is_none_or(|(b, _)| s > *b) {
            best = Some((s, candidate));
        }
    }
    best.map(|(_, a)| a)
}

/// An entry's address as matched: lower case, without its scheme and a
/// leading `www.`.
fn bare_address(url: &str) -> String {
    let lower = url.to_lowercase();
    let rest = lower.split_once("://").map_or(lower.as_str(), |(_, r)| r);
    rest.strip_prefix("www.").unwrap_or(rest).to_owned()
}

/// Visits, typed ones counting three times, weighed by how long ago the
/// last one was (Firefox's recency buckets: 4, 14, 31 and 90 days).
fn score(e: &HistoryEntry, now: i64) -> f64 {
    let days = (now - e.last_visit).max(0) as f64 / 86_400_000.0;
    let recency = if days <= 4.0 {
        100.0
    } else if days <= 14.0 {
        70.0
    } else if days <= 31.0 {
        50.0
    } else if days <= 90.0 {
        30.0
    } else {
        10.0
    };
    (f64::from(e.visit_count) + 2.0 * f64::from(e.typed_count)) * recency
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
    Remove { url: String },
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
                        Op::Remove { url } => store.remove_history(&url),
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

    /// Forget a page.
    pub fn remove(&self, url: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|e| e.url != url);
        }
        let Some(ops) = &self.ops else { return };
        let _ = ops.send(Op::Remove { url: url.to_owned() });
    }

    /// The address bar's matches for what was typed, best first.
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<HistoryEntry> {
        let Ok(entries) = self.entries.lock() else { return Vec::new() };
        suggest(&entries, query, limit, now_ms())
    }

    /// What the address bar fills in after `query`, if anything.
    pub fn autofill(&self, query: &str) -> Option<Autofill> {
        let entries = self.entries.lock().ok()?;
        autofill(&entries, query, now_ms())
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
    fn suggestions_match_every_word_and_rank_by_visits_typing_recency_and_host() {
        let day = 86_400_000;
        let now = 1000 * day;
        let e = |url: &str, title: &str, visits: u32, typed: u32, ago: i64| HistoryEntry {
            url: url.into(),
            title: Some(title.into()),
            visit_count: visits,
            typed_count: typed,
            last_visit: now - ago * day,
        };
        let entries = vec![
            e("https://en.wikipedia.org/wiki/Main_Page", "Wikipedia, the free encyclopedia", 5, 0, 1),
            e("https://www.wiktionary.org/", "Wiktionary", 1, 0, 1),
            e("https://news.example/wiki-story", "A story", 30, 0, 200),
            e("https://iana.org/protocols", "Protocol Registries", 2, 2, 2),
        ];
        let urls = |q: &str| -> Vec<String> { suggest(&entries, q, 8, now).into_iter().map(|e| e.url).collect() };
        // Visits weighed by recency (30 old visits rank below 5 recent),
        // the host's start (`www.` and the scheme aside) four times: 5
        // recent visits still outrank 1 at the host's start.
        assert_eq!(urls("wik"), ["https://en.wikipedia.org/wiki/Main_Page", "https://www.wiktionary.org/", "https://news.example/wiki-story"]);
        let mut level = entries.clone();
        level[0].visit_count = 1;
        let first = suggest(&level, "wik", 8, now);
        assert_eq!(first[0].url, "https://www.wiktionary.org/", "level visits: the host's start first");
        // Titles match too, every word must, case aside.
        assert_eq!(urls("FREE wiki"), ["https://en.wikipedia.org/wiki/Main_Page"]);
        assert_eq!(urls("registries"), ["https://iana.org/protocols"]);
        assert!(urls("https").is_empty(), "the scheme is not matched");
        assert!(urls("   ").is_empty() && urls("nothing-like-it").is_empty());
        assert_eq!(suggest(&entries, "wik", 1, now).len(), 1, "the limit");
    }

    #[test]
    fn autofill_completes_a_visited_host_then_up_to_the_next_slash() {
        let now = 1000 * 86_400_000;
        let e = |url: &str, visits: u32| HistoryEntry {
            url: url.into(),
            title: None,
            visit_count: visits,
            typed_count: 0,
            last_visit: now,
        };
        let entries = vec![
            e("https://www.wikipedia.org/", 3),
            e("https://en.wikipedia.org/wiki/Main_Page", 9),
            e("http://iana.org/protocols/http", 1),
        ];
        let fill = |q: &str| autofill(&entries, q, now).map(|a| (a.text, a.url));
        // The host, `www.` aside, with its scheme and `www.` in the page.
        assert_eq!(fill("wik"), Some(("wikipedia.org/".into(), "https://www.wikipedia.org/".into())));
        assert_eq!(fill("WiK"), Some(("WiKipedia.org/".into(), "https://www.wikipedia.org/".into())), "the typed case is kept");
        assert_eq!(fill("www.w"), Some(("www.wikipedia.org/".into(), "https://www.wikipedia.org/".into())));
        // The best-ranked of several.
        assert_eq!(fill("en"), Some(("en.wikipedia.org/".into(), "https://en.wikipedia.org/".into())));
        // Past the host: to the next slash, then the end; http kept.
        assert_eq!(fill("iana.org/"), Some(("iana.org/protocols/".into(), "http://iana.org/protocols/".into())));
        assert_eq!(fill("iana.org/protocols/h"), Some(("iana.org/protocols/http".into(), "http://iana.org/protocols/http".into())));
        // Nothing to add, a space, a scheme, no match.
        assert_eq!(fill("iana.org/protocols/http"), None);
        assert_eq!(fill("wik i"), None);
        assert_eq!(fill("https://w"), None);
        assert_eq!(fill("zzz"), None);
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
            service.visit("https://c.test/", Some("C"), false);
            service.remove("https://c.test/");
            assert!(service.suggest("c.test", 8).is_empty(), "removed at once");
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
