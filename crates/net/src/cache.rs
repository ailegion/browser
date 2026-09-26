//! In-memory HTTP cache following RFC 9111. It never touches the disk
//! (plan D02): entries live in RAM and go when the browser exits.
//!
//! Bodies are stored decoded (after `Content-Encoding`), since that is
//! what tabs receive. Freshness comes from `Cache-Control: max-age`,
//! `Expires`, or the heuristic on `Last-Modified`; a stale entry with a
//! validator is revalidated with `If-None-Match` / `If-Modified-Since`
//! and refreshed on `304`. `Vary` is honored by remembering the request
//! headers the entry was stored under. Redirects are not cached.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;

/// What the caller wants from the cache, after the Fetch standard's
/// request cache modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Serve fresh entries, revalidate stale ones, fetch the rest.
    #[default]
    Default,
    /// Always revalidate before use (a normal reload).
    NoCache,
    /// Ignore the cache for the lookup; still store the response.
    Reload,
}

/// Default size limits.
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
/// Cap on the heuristic freshness taken from `Last-Modified`.
const MAX_HEURISTIC: Duration = Duration::from_secs(24 * 3600);

/// When the request went out and the response head came back.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub request: Instant,
    pub response: Instant,
    /// Wall clock at `response`, compared against the `Date` header.
    pub wall: SystemTime,
}

impl Timing {
    pub fn now() -> Self {
        let now = Instant::now();
        Self {
            request: now,
            response: now,
            wall: SystemTime::now(),
        }
    }
}

/// A response handed out by the cache.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// Current age in seconds, for the `Age` header.
    pub age: u64,
}

/// Result of a lookup.
#[derive(Debug)]
pub enum Lookup {
    Miss,
    Fresh(CachedResponse),
    /// Usable after validation with the given validators.
    Stale {
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// How a response may be stored, decided from its head before the body
/// streams.
#[derive(Debug, Clone)]
pub struct Policy {
    freshness: Duration,
    corrected_initial_age: Duration,
    no_cache: bool,
    /// Request header values the entry is valid for, per `Vary`.
    vary: Vec<(String, Option<String>)>,
}

#[derive(Debug)]
struct Entry {
    status: u16,
    headers: Vec<(String, String)>,
    body: Bytes,
    response_time: Instant,
    policy: Policy,
    last_used: u64,
}

impl Entry {
    fn current_age(&self, now: Instant) -> Duration {
        self.policy.corrected_initial_age + now.saturating_duration_since(self.response_time)
    }

    fn response(&self, now: Instant) -> CachedResponse {
        CachedResponse {
            status: self.status,
            headers: self.headers.clone(),
            body: self.body.clone(),
            age: self.current_age(now).as_secs(),
        }
    }
}

#[derive(Debug)]
pub struct HttpCache {
    entries: HashMap<String, Entry>,
    total: usize,
    max_total: usize,
    max_entry: usize,
    clock: u64,
}

impl Default for HttpCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpCache {
    pub fn new() -> Self {
        Self::with_limits(MAX_TOTAL_BYTES, MAX_ENTRY_BYTES)
    }

    pub fn with_limits(max_total: usize, max_entry: usize) -> Self {
        Self {
            entries: HashMap::new(),
            total: 0,
            max_total,
            max_entry: max_entry.min(max_total),
            clock: 0,
        }
    }

    /// Largest body that will be stored.
    pub fn max_entry(&self) -> usize {
        self.max_entry
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of body held.
    pub fn bytes(&self) -> usize {
        self.total
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.total = 0;
    }

    /// The cache key of a URL: everything but the fragment.
    pub fn key(url: &url::Url) -> String {
        let mut u = url.clone();
        u.set_fragment(None);
        u.into()
    }

    /// Look for a stored response for a `GET` of `key` with these request
    /// headers.
    pub fn lookup(&mut self, key: &str, request_headers: &[(String, String)], mode: CacheMode, now: Instant) -> Lookup {
        if mode == CacheMode::Reload {
            return Lookup::Miss;
        }
        self.clock += 1;
        let clock = self.clock;
        let Some(entry) = self.entries.get_mut(key) else {
            return Lookup::Miss;
        };
        let vary_ok = entry
            .policy
            .vary
            .iter()
            .all(|(name, value)| header(request_headers, name) == value.as_deref());
        if !vary_ok {
            return Lookup::Miss;
        }
        entry.last_used = clock;
        let fresh = mode == CacheMode::Default && !entry.policy.no_cache && entry.current_age(now) < entry.policy.freshness;
        if fresh {
            return Lookup::Fresh(entry.response(now));
        }
        let etag = header(&entry.headers, "etag").map(str::to_owned);
        let last_modified = header(&entry.headers, "last-modified").map(str::to_owned);
        if etag.is_none() && last_modified.is_none() {
            // Nothing to validate with; the entry is dead weight.
            self.remove(key);
            return Lookup::Miss;
        }
        Lookup::Stale { etag, last_modified }
    }

    /// Decide from a response head whether and how it may be stored for
    /// the request (whose headers `Vary` may name). `None` means do not
    /// store.
    pub fn policy(
        &self,
        status: u16,
        headers: &[(String, String)],
        request_headers: &[(String, String)],
        timing: Timing,
    ) -> Option<Policy> {
        if !matches!(status, 200..=499) || status == 206 || status == 304 {
            return None;
        }
        if header(headers, "content-length").and_then(|v| v.trim().parse::<usize>().ok()) > Some(self.max_entry) {
            return None;
        }
        let cc = CacheControl::parse(headers);
        if cc.no_store {
            return None;
        }
        let mut vary = Vec::new();
        for name in headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("vary"))
            .flat_map(|(_, v)| v.split(','))
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
        {
            if name == "*" {
                return None;
            }
            let value = header(request_headers, &name).map(str::to_owned);
            vary.push((name, value));
        }
        let has_validator = header(headers, "etag").is_some() || header(headers, "last-modified").is_some();
        let freshness = freshness_lifetime(status, headers, &cc, timing.wall);
        if !has_validator && (cc.no_cache || freshness.is_zero()) {
            return None;
        }
        Some(Policy {
            freshness,
            corrected_initial_age: initial_age(headers, timing),
            no_cache: cc.no_cache,
            vary,
        })
    }

    /// Store a complete response under `key`.
    pub fn store(
        &mut self,
        key: String,
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
        policy: Policy,
        timing: Timing,
    ) {
        if body.len() > self.max_entry {
            return;
        }
        let headers = stored_headers(headers, body.len());
        self.remove(&key);
        self.clock += 1;
        self.total += body.len();
        self.entries.insert(
            key,
            Entry {
                status,
                headers,
                body,
                response_time: timing.response,
                policy,
                last_used: self.clock,
            },
        );
        self.evict();
    }

    /// A conditional request for `key` answered `304`: refresh the entry
    /// with the new head and return it. `None` if the entry is gone or the
    /// new head forbids storing it.
    pub fn revalidated(&mut self, key: &str, headers_304: &[(String, String)], timing: Timing) -> Option<CachedResponse> {
        let entry = self.entries.get(key)?;
        // Headers in the 304 replace the stored ones of the same name; the
        // ones describing the wire body stay as the stored body has them.
        let wire = |k: &str| {
            k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("content-encoding")
                || k.eq_ignore_ascii_case("transfer-encoding")
                || k.eq_ignore_ascii_case("connection")
        };
        let mut headers = entry.headers.clone();
        for (k, _) in headers_304.iter().filter(|(k, _)| !wire(k)) {
            headers.retain(|(h, _)| !h.eq_ignore_ascii_case(k));
        }
        headers.extend(headers_304.iter().filter(|(k, _)| !wire(k)).cloned());
        let status = entry.status;
        let len = entry.body.len();
        // The 304 answered a request that already matched the entry's
        // `Vary` values, so they carry over.
        let vary = entry.policy.vary.clone();
        let Some(mut policy) = self.policy(status, &headers, &[], timing) else {
            self.remove(key);
            return None;
        };
        policy.vary = vary;
        let entry = self.entries.get_mut(key)?;
        entry.headers = stored_headers(headers, len);
        entry.policy = policy;
        entry.response_time = timing.response;
        Some(entry.response(timing.response))
    }

    fn remove(&mut self, key: &str) {
        if let Some(e) = self.entries.remove(key) {
            self.total -= e.body.len();
        }
    }

    fn evict(&mut self) {
        while self.total > self.max_total {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&key);
        }
    }
}

/// Headers as kept: the body is decoded, so the encoding and length of
/// the wire form are replaced.
fn stored_headers(mut headers: Vec<(String, String)>, body_len: usize) -> Vec<(String, String)> {
    headers.retain(|(k, _)| {
        !(k.eq_ignore_ascii_case("content-encoding")
            || k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("transfer-encoding")
            || k.eq_ignore_ascii_case("age"))
    });
    headers.push(("content-length".to_owned(), body_len.to_string()));
    headers
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[derive(Debug, Default)]
struct CacheControl {
    no_store: bool,
    no_cache: bool,
    max_age: Option<u64>,
}

impl CacheControl {
    fn parse(headers: &[(String, String)]) -> Self {
        let mut cc = Self::default();
        for (_, value) in headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("cache-control")) {
            for directive in value.split(',') {
                let directive = directive.trim();
                let (name, arg) = match directive.split_once('=') {
                    Some((n, a)) => (n.trim(), Some(a.trim().trim_matches('"'))),
                    None => (directive, None),
                };
                if name.eq_ignore_ascii_case("no-store") {
                    cc.no_store = true;
                } else if name.eq_ignore_ascii_case("no-cache") {
                    cc.no_cache = true;
                } else if name.eq_ignore_ascii_case("max-age")
                    && let Some(secs) = arg.and_then(|a| a.parse::<u64>().ok())
                {
                    cc.max_age = Some(secs);
                }
            }
        }
        cc
    }
}

fn parse_date(headers: &[(String, String)], name: &str) -> Option<SystemTime> {
    header(headers, name).and_then(|v| httpdate::parse_http_date(v.trim()).ok())
}

/// RFC 9111 §4.2.1.
fn freshness_lifetime(status: u16, headers: &[(String, String)], cc: &CacheControl, wall: SystemTime) -> Duration {
    if let Some(secs) = cc.max_age {
        return Duration::from_secs(secs);
    }
    let date = parse_date(headers, "date").unwrap_or(wall);
    if header(headers, "expires").is_some() {
        // An unparsable Expires means already expired.
        return parse_date(headers, "expires")
            .and_then(|e| e.duration_since(date).ok())
            .unwrap_or_default();
    }
    let heuristic_ok = matches!(status, 200 | 203 | 204 | 300 | 301 | 308 | 404 | 405 | 410 | 414 | 501);
    if heuristic_ok && let Some(modified) = parse_date(headers, "last-modified") {
        let since = date.duration_since(modified).unwrap_or_default();
        return (since / 10).min(MAX_HEURISTIC);
    }
    Duration::ZERO
}

/// RFC 9111 §4.2.3: the age of the response at the moment it arrived.
fn initial_age(headers: &[(String, String)], timing: Timing) -> Duration {
    let apparent = parse_date(headers, "date")
        .and_then(|d| timing.wall.duration_since(d).ok())
        .unwrap_or_default();
    let age_value = header(headers, "age")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_default();
    let delay = timing.response.saturating_duration_since(timing.request);
    apparent.max(age_value + delay)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn timing() -> Timing {
        Timing::now()
    }

    fn put(cache: &mut HttpCache, key: &str, headers: &[(&str, &str)], body: &str, t: Timing) -> bool {
        put_with(cache, key, headers, &[], body, t)
    }

    fn put_with(
        cache: &mut HttpCache,
        key: &str,
        headers: &[(&str, &str)],
        request: &[(&str, &str)],
        body: &str,
        t: Timing,
    ) -> bool {
        let headers = h(headers);
        let Some(policy) = cache.policy(200, &headers, &h(request), t) else {
            return false;
        };
        cache.store(key.to_owned(), 200, headers, Bytes::from(body.to_owned()), policy, t);
        true
    }

    #[test]
    fn max_age_governs_freshness() {
        let mut cache = HttpCache::new();
        let t = timing();
        assert!(put(&mut cache, "k", &[("cache-control", "max-age=60")], "body", t));
        match cache.lookup("k", &[], CacheMode::Default, t.response + Duration::from_secs(30)) {
            Lookup::Fresh(r) => {
                assert_eq!(r.body, "body");
                assert_eq!(r.age, 30);
                assert_eq!(header(&r.headers, "content-length"), Some("4"));
            }
            other => panic!("{other:?}"),
        }
        // Past max-age with no validator: gone.
        assert!(matches!(
            cache.lookup("k", &[], CacheMode::Default, t.response + Duration::from_secs(61)),
            Lookup::Miss
        ));
        assert!(cache.is_empty());
    }

    #[test]
    fn expires_relative_to_date_and_age_header() {
        let mut cache = HttpCache::new();
        let t = timing();
        let date = httpdate::fmt_http_date(t.wall);
        let expires = httpdate::fmt_http_date(t.wall + Duration::from_secs(100));
        assert!(put(
            &mut cache,
            "k",
            &[("Date", &date), ("Expires", &expires), ("Age", "40")],
            "b",
            t
        ));
        assert!(matches!(
            cache.lookup("k", &[], CacheMode::Default, t.response + Duration::from_secs(59)),
            Lookup::Fresh(_)
        ));
        assert!(matches!(
            cache.lookup("k", &[], CacheMode::Default, t.response + Duration::from_secs(61)),
            Lookup::Miss
        ));
    }

    #[test]
    fn heuristic_freshness_from_last_modified() {
        let mut cache = HttpCache::new();
        let t = timing();
        let date = httpdate::fmt_http_date(t.wall);
        let modified = httpdate::fmt_http_date(t.wall - Duration::from_secs(1000));
        assert!(put(&mut cache, "k", &[("date", &date), ("last-modified", &modified)], "b", t));
        assert!(matches!(
            cache.lookup("k", &[], CacheMode::Default, t.response + Duration::from_secs(99)),
            Lookup::Fresh(_)
        ));
        match cache.lookup("k", &[], CacheMode::Default, t.response + Duration::from_secs(101)) {
            Lookup::Stale { last_modified, etag } => {
                assert_eq!(last_modified.as_deref(), Some(modified.as_str()));
                assert!(etag.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_store_and_uncacheable_statuses_are_not_kept() {
        let mut cache = HttpCache::new();
        let t = timing();
        assert!(!put(&mut cache, "a", &[("cache-control", "max-age=60, no-store")], "b", t));
        assert!(!put(&mut cache, "b", &[], "b", t));
        assert!(!put(&mut cache, "c", &[("cache-control", "no-cache")], "b", t));
        assert!(!put(&mut cache, "d", &[("vary", "*"), ("etag", "\"x\"")], "b", t));
        assert!(cache.policy(206, &h(&[("cache-control", "max-age=60")]), &[], t).is_none());
        assert!(cache.policy(500, &h(&[("cache-control", "max-age=60")]), &[], t).is_none());
        assert!(cache.policy(404, &h(&[("cache-control", "max-age=60")]), &[], t).is_some());
        assert!(cache.is_empty());
    }

    #[test]
    fn no_cache_with_validator_always_revalidates_and_304_refreshes() {
        let mut cache = HttpCache::new();
        let t = timing();
        assert!(put(&mut cache, "k", &[("cache-control", "no-cache"), ("etag", "\"v1\"")], "one", t));
        match cache.lookup("k", &[], CacheMode::Default, t.response) {
            Lookup::Stale { etag, .. } => assert_eq!(etag.as_deref(), Some("\"v1\"")),
            other => panic!("{other:?}"),
        }
        let later = Timing {
            request: t.response + Duration::from_secs(10),
            response: t.response + Duration::from_secs(10),
            wall: t.wall + Duration::from_secs(10),
        };
        let r = cache
            .revalidated("k", &h(&[("cache-control", "max-age=30"), ("etag", "\"v1\"")]), later)
            .unwrap();
        assert_eq!(r.body, "one");
        assert_eq!(r.age, 0);
        assert!(matches!(
            cache.lookup("k", &[], CacheMode::Default, later.response + Duration::from_secs(5)),
            Lookup::Fresh(_)
        ));
        // A 304 that removes cacheability drops the entry.
        assert!(cache.revalidated("k", &h(&[("cache-control", "no-store")]), later).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn modes_override_freshness() {
        let mut cache = HttpCache::new();
        let t = timing();
        assert!(put(&mut cache, "k", &[("cache-control", "max-age=60"), ("etag", "\"x\"")], "b", t));
        assert!(matches!(cache.lookup("k", &[], CacheMode::Reload, t.response), Lookup::Miss));
        assert!(matches!(cache.lookup("k", &[], CacheMode::NoCache, t.response), Lookup::Stale { .. }));
        assert!(matches!(cache.lookup("k", &[], CacheMode::Default, t.response), Lookup::Fresh(_)));
    }

    #[test]
    fn vary_compares_request_headers() {
        let mut cache = HttpCache::new();
        let t = timing();
        assert!(put_with(
            &mut cache,
            "k",
            &[("cache-control", "max-age=60"), ("vary", "Cookie, Accept-Language")],
            &[("cookie", "a=1"), ("accept-language", "en")],
            "b",
            t
        ));
        let same = h(&[("Cookie", "a=1"), ("Accept-Language", "en")]);
        assert!(matches!(cache.lookup("k", &same, CacheMode::Default, t.response), Lookup::Fresh(_)));
        let other = h(&[("cookie", "a=2"), ("accept-language", "en")]);
        assert!(matches!(cache.lookup("k", &other, CacheMode::Default, t.response), Lookup::Miss));
        let missing = h(&[("accept-language", "en")]);
        assert!(matches!(cache.lookup("k", &missing, CacheMode::Default, t.response), Lookup::Miss));
    }

    #[test]
    fn size_limits_and_lru_eviction() {
        let mut cache = HttpCache::with_limits(10, 6);
        let t = timing();
        assert!(put(&mut cache, "big", &[("cache-control", "max-age=60")], "1234567", t));
        assert!(cache.is_empty(), "a body over the entry limit is not kept");
        assert!(put(&mut cache, "a", &[("cache-control", "max-age=60")], "aaaa", t));
        assert!(put(&mut cache, "b", &[("cache-control", "max-age=60")], "bbbb", t));
        assert_eq!(cache.bytes(), 8);
        // Use `a` so `b` is the least recently used.
        assert!(matches!(cache.lookup("a", &[], CacheMode::Default, t.response), Lookup::Fresh(_)));
        assert!(put(&mut cache, "c", &[("cache-control", "max-age=60")], "cccc", t));
        assert_eq!(cache.len(), 2);
        assert!(matches!(cache.lookup("b", &[], CacheMode::Default, t.response), Lookup::Miss));
        assert!(matches!(cache.lookup("a", &[], CacheMode::Default, t.response), Lookup::Fresh(_)));
        // Content-Length above the entry limit is refused before the body streams.
        assert!(
            cache
                .policy(200, &h(&[("cache-control", "max-age=60"), ("content-length", "7")]), &[], t)
                .is_none()
        );
    }

    #[test]
    fn key_drops_fragment() {
        let a = url::Url::parse("https://example.com/x?y=1#top").unwrap();
        let b = url::Url::parse("https://example.com/x?y=1").unwrap();
        assert_eq!(HttpCache::key(&a), HttpCache::key(&b));
    }
}
