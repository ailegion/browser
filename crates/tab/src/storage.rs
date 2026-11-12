//! Web Storage data ("Working sites first" line 7; plan D02: in memory
//! while the browser runs, persisted on tab close from Phase 4).
//!
//! `WebStorage` is the browser's local storage: one per browser, shared by
//! every tab, an area per origin. A tab that changes an area tells the
//! other tabs (`StorageChange`), whose documents of that origin fire
//! `storage`. Session storage is a tab's own: an area per origin, alive as
//! long as the tab.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

use browser_ipc_types::TabId;
use browser_script::{StorageAccess, StorageArea};

/// The most UTF-16 code units of keys and values an area holds: Chrome's
/// 10 MiB of two-byte units.
const QUOTA_UNITS: usize = 5 * 1024 * 1024;

/// One origin's key/value pairs, in the order the keys were first set.
#[derive(Debug, Default, Clone)]
struct Area {
    items: Vec<(Vec<u16>, Vec<u16>)>,
}

impl Area {
    fn size(&self) -> usize {
        self.items.iter().map(|(k, v)| k.len() + v.len()).sum()
    }

    fn get(&self, key: &[u16]) -> Option<&Vec<u16>> {
        self.items.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Set a pair. `Err` when the quota would be exceeded; `Ok(None)`
    /// when the value is what was there; else the value before.
    #[allow(clippy::type_complexity)]
    fn set(&mut self, key: &[u16], value: &[u16]) -> Result<Option<Option<Vec<u16>>>, ()> {
        let old = self.get(key).cloned();
        if old.as_deref() == Some(value) {
            return Ok(None);
        }
        let after = self.size() - old.as_ref().map_or(0, |o| key.len() + o.len()) + key.len() + value.len();
        if after > QUOTA_UNITS {
            return Err(());
        }
        match self.items.iter_mut().find(|(k, _)| k == key) {
            Some(pair) => pair.1 = value.to_vec(),
            None => self.items.push((key.to_vec(), value.to_vec())),
        }
        Ok(Some(old))
    }

    fn remove(&mut self, key: &[u16]) -> Option<Vec<u16>> {
        let at = self.items.iter().position(|(k, _)| k == key)?;
        Some(self.items.remove(at).1)
    }
}

/// A change one tab made to an origin's local storage, for the others.
#[derive(Debug, Clone)]
pub struct StorageChange {
    /// The origin's serialization.
    pub origin: String,
    /// `None` for `clear()`.
    pub key: Option<Vec<u16>>,
    pub old: Option<Vec<u16>>,
    pub new: Option<Vec<u16>>,
    /// The URL of the document that made the change.
    pub url: String,
}

type Listener = Arc<dyn Fn(StorageChange) + Send + Sync>;

#[derive(Default)]
struct Shared {
    areas: HashMap<String, Area>,
    listeners: Vec<(TabId, Listener)>,
}

/// The browser's local storage. One per browser.
#[derive(Default)]
pub struct WebStorage {
    shared: Mutex<Shared>,
}

impl std::fmt::Debug for WebStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebStorage")
    }
}

impl WebStorage {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hear of the changes other tabs make. Called from their threads.
    pub(crate) fn subscribe(&self, tab: TabId, listener: Listener) {
        let mut shared = self.lock();
        shared.listeners.retain(|(t, _)| *t != tab);
        shared.listeners.push((tab, listener));
    }

    pub(crate) fn unsubscribe(&self, tab: TabId) {
        self.lock().listeners.retain(|(t, _)| *t != tab);
    }

    /// Tell every tab but `from`.
    fn broadcast(&self, from: TabId, change: StorageChange) {
        let listeners: Vec<Listener> = self.lock().listeners.iter().filter(|(t, _)| *t != from).map(|(_, l)| l.clone()).collect();
        for listener in listeners {
            listener(change.clone());
        }
    }
}

/// A tab's session storage: an area per origin.
#[derive(Debug, Default)]
pub struct SessionStorage {
    areas: Mutex<HashMap<String, Area>>,
}

/// The storage the tab was given: the browser's local storage and its own
/// session storage.
#[derive(Debug, Clone, Default)]
pub struct TabStores {
    pub local: Arc<WebStorage>,
    pub session: Arc<SessionStorage>,
}

/// The two areas of one document's origin, as its scripts reach them.
pub(crate) struct OriginStorage {
    pub(crate) tab: TabId,
    pub(crate) origin: String,
    /// The document's URL, for the events its changes cause elsewhere.
    pub(crate) url: std::cell::RefCell<String>,
    pub(crate) stores: TabStores,
}

impl OriginStorage {
    pub(crate) fn new(tab: TabId, origin: String, url: &str, stores: TabStores) -> Rc<Self> {
        Rc::new(Self {
            tab,
            origin,
            url: std::cell::RefCell::new(url.to_owned()),
            stores,
        })
    }

    fn with<R>(&self, area: StorageArea, f: impl FnOnce(&mut Area) -> R) -> R {
        match area {
            StorageArea::Local => f(self.stores.local.lock().areas.entry(self.origin.clone()).or_default()),
            StorageArea::Session => {
                let mut areas = self.stores.session.areas.lock().unwrap_or_else(|e| e.into_inner());
                f(areas.entry(self.origin.clone()).or_default())
            }
        }
    }

    /// Other documents of the origin hear of a change to the local area
    /// (the session area has no other document in this tab).
    fn changed(&self, area: StorageArea, key: Option<&[u16]>, old: Option<Vec<u16>>, new: Option<&[u16]>) {
        if area != StorageArea::Local {
            return;
        }
        self.stores.local.broadcast(
            self.tab,
            StorageChange {
                origin: self.origin.clone(),
                key: key.map(<[u16]>::to_vec),
                old,
                new: new.map(<[u16]>::to_vec),
                url: self.url.borrow().clone(),
            },
        );
    }
}

impl StorageAccess for OriginStorage {
    fn keys(&self, area: StorageArea) -> Vec<Vec<u16>> {
        self.with(area, |a| a.items.iter().map(|(k, _)| k.clone()).collect())
    }

    fn get(&self, area: StorageArea, key: &[u16]) -> Option<Vec<u16>> {
        self.with(area, |a| a.get(key).cloned())
    }

    fn set(&self, area: StorageArea, key: &[u16], value: &[u16]) -> bool {
        match self.with(area, |a| a.set(key, value)) {
            Err(()) => false,
            Ok(None) => true,
            Ok(Some(old)) => {
                self.changed(area, Some(key), old, Some(value));
                true
            }
        }
    }

    fn remove(&self, area: StorageArea, key: &[u16]) {
        if let Some(old) = self.with(area, |a| a.remove(key)) {
            self.changed(area, Some(key), Some(old), None);
        }
    }

    fn clear(&self, area: StorageArea) {
        let had = self.with(area, |a| {
            let had = !a.items.is_empty();
            a.items.clear();
            had
        });
        if had {
            self.changed(area, None, None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn areas_are_per_origin_ordered_bounded_and_tell_the_other_tabs() {
        let stores = TabStores::default();
        let heard: Arc<Mutex<Vec<StorageChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        stores.local.subscribe(TabId(2), Arc::new(move |c| sink.lock().unwrap_or_else(|e| e.into_inner()).push(c)));
        let own: Arc<Mutex<Vec<StorageChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = own.clone();
        stores.local.subscribe(TabId(1), Arc::new(move |c| sink.lock().unwrap_or_else(|e| e.into_inner()).push(c)));
        let a = OriginStorage::new(TabId(1), "https://a.test".to_owned(), "https://a.test/page", stores.clone());
        let b = OriginStorage::new(TabId(1), "https://b.test".to_owned(), "https://b.test/", stores.clone());
        assert!(a.set(StorageArea::Local, &units("k1"), &units("v1")));
        assert!(a.set(StorageArea::Local, &units("k2"), &units("v2")));
        assert!(a.set(StorageArea::Local, &units("k1"), &units("v1b")), "an existing key keeps its place");
        assert!(a.set(StorageArea::Local, &units("k1"), &units("v1b")), "the same value is no change");
        assert_eq!(a.keys(StorageArea::Local), vec![units("k1"), units("k2")]);
        assert_eq!(a.get(StorageArea::Local, &units("k1")), Some(units("v1b")));
        assert!(b.keys(StorageArea::Local).is_empty(), "another origin's area");
        assert!(a.keys(StorageArea::Session).is_empty(), "the session area is separate");
        assert!(a.set(StorageArea::Session, &units("s"), &units("1")));
        a.remove(StorageArea::Local, &units("k2"));
        a.remove(StorageArea::Local, &units("nope"));
        a.clear(StorageArea::Local);
        a.clear(StorageArea::Local);
        let heard: Vec<StorageChange> = heard.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let summary: Vec<(Option<String>, Option<String>, Option<String>)> = heard
            .iter()
            .map(|c| {
                let s = |v: &Option<Vec<u16>>| v.as_ref().map(|u| String::from_utf16_lossy(u));
                (s(&c.key), s(&c.old), s(&c.new))
            })
            .collect();
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(
            summary,
            vec![
                (some("k1"), None, some("v1")),
                (some("k2"), None, some("v2")),
                (some("k1"), some("v1"), some("v1b")),
                (some("k2"), some("v2"), None),
                (None, None, None),
            ],
            "each real change of the local area, once; none for the session area"
        );
        assert!(heard.iter().all(|c| c.origin == "https://a.test" && c.url == "https://a.test/page"));
        assert!(own.lock().unwrap_or_else(|e| e.into_inner()).is_empty(), "the tab that made a change does not hear of it");
        // The quota: what would pass it is refused and nothing changes.
        let big = vec![b'x' as u16; QUOTA_UNITS];
        assert!(!b.set(StorageArea::Local, &units("big"), &big));
        assert!(b.set(StorageArea::Local, &units("big"), &big[..QUOTA_UNITS - 3]));
        assert!(!b.set(StorageArea::Local, &units("more"), &units("x")));
        assert_eq!(b.keys(StorageArea::Local).len(), 1);
    }
}
