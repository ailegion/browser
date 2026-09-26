//! Cookie jar: the storage model of RFC 6265bis over the `cookie` crate's
//! parser. Everything lives in memory (plan D02); Phase 4 persists the jar
//! through the encrypted store when a tab closes.
//!
//! What is implemented: `Domain`, `Path`, `Secure`, `HttpOnly`, `Expires`,
//! `Max-Age`, the `__Secure-` and `__Host-` prefixes, "leave secure cookies
//! alone", per-domain and total limits with least-recently-used eviction.
//! `SameSite` is stored but not enforced: enforcement needs the site that
//! initiated a request, which requests do not carry yet (see O16).

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use url::{Host, Url};

/// Longest `name=value` accepted, per RFC 6265bis §5.5.
const MAX_COOKIE_BYTES: usize = 4096;
/// Cookies kept per domain before the least recently used go.
const MAX_PER_DOMAIN: usize = 180;
/// Cookies kept in total.
const MAX_TOTAL: usize = 3000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

/// One cookie as stored. Fields are public so the Phase 4 store can
/// serialize them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    /// Lowercase, without a leading dot.
    pub domain: String,
    /// Set when there was no `Domain` attribute: sent to exactly `domain`.
    pub host_only: bool,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<SameSite>,
    /// `None` is a session cookie.
    pub expires: Option<SystemTime>,
    pub created: SystemTime,
    pub last_access: SystemTime,
}

impl StoredCookie {
    fn expired(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }
}

/// All cookies of the browser, keyed by domain.
#[derive(Debug, Default)]
pub struct CookieJar {
    by_domain: HashMap<String, Vec<StoredCookie>>,
    total: usize,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn clear(&mut self) {
        self.by_domain.clear();
        self.total = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = &StoredCookie> {
        self.by_domain.values().flatten()
    }

    /// Store one `Set-Cookie` header value received from `url`. Returns
    /// whether the cookie was accepted.
    pub fn store(&mut self, url: &Url, set_cookie: &str, now: SystemTime) -> bool {
        let Some(host) = request_host(url) else { return false };
        let Ok(parsed) = cookie::Cookie::parse(set_cookie.to_owned()) else {
            return false;
        };
        if parsed.name().len() + parsed.value().len() > MAX_COOKIE_BYTES {
            return false;
        }
        let secure_origin = is_secure(url);

        // Max-Age wins over Expires. A non-positive Max-Age expires the
        // cookie at once, which is how a server deletes one.
        let expires = if let Some(max_age) = parsed.max_age() {
            let secs = max_age.whole_seconds();
            Some(if secs <= 0 {
                UNIX_EPOCH
            } else {
                now + Duration::from_secs(secs.unsigned_abs())
            })
        } else {
            match parsed.expires() {
                Some(cookie::Expiration::DateTime(at)) => {
                    let ts = at.unix_timestamp();
                    Some(if ts <= 0 {
                        UNIX_EPOCH
                    } else {
                        UNIX_EPOCH + Duration::from_secs(ts.unsigned_abs())
                    })
                }
                _ => None,
            }
        };

        let domain_attr = parsed
            .domain()
            .map(|d| d.trim_start_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty());
        let (domain, host_only) = match domain_attr {
            Some(d) => {
                // The host must be inside the domain, an IP host can only set
                // cookies for itself, and a domain without a dot is a public
                // suffix we will not let anyone claim (a stand-in for the
                // public suffix list; see O16).
                if d != host && (host_is_ip(url) || !d.contains('.') || !domain_matches(&host, &d)) {
                    return false;
                }
                (d, false)
            }
            None => (host.clone(), true),
        };

        let path = match parsed.path() {
            Some(p) if p.starts_with('/') => p.to_owned(),
            _ => default_path(url.path()),
        };

        let secure = parsed.secure().unwrap_or(false);
        if secure && !secure_origin {
            return false;
        }
        let name = parsed.name();
        if name.starts_with("__Secure-") && !secure {
            return false;
        }
        if name.starts_with("__Host-") && (!secure || !host_only || path != "/") {
            return false;
        }

        // Leave secure cookies alone: an insecure origin cannot shadow a
        // secure cookie of the same name whose scope overlaps this one.
        if !secure_origin {
            let shadowed = self.iter().any(|c| {
                c.secure
                    && c.name == name
                    && (domain_matches(&domain, &c.domain) || domain_matches(&c.domain, &domain))
                    && (path_matches(&path, &c.path) || path_matches(&c.path, &path))
            });
            if shadowed {
                return false;
            }
        }

        let same_site = parsed.same_site().map(|s| match s {
            cookie::SameSite::Strict => SameSite::Strict,
            cookie::SameSite::Lax => SameSite::Lax,
            cookie::SameSite::None => SameSite::None,
        });

        let mut cookie = StoredCookie {
            name: name.to_owned(),
            value: parsed.value().to_owned(),
            domain: domain.clone(),
            host_only,
            path,
            secure,
            http_only: parsed.http_only().unwrap_or(false),
            same_site,
            expires,
            created: now,
            last_access: now,
        };

        let list = self.by_domain.entry(domain).or_default();
        if let Some(pos) = list
            .iter()
            .position(|c| c.name == cookie.name && c.path == cookie.path)
        {
            cookie.created = list[pos].created;
            list.remove(pos);
            self.total -= 1;
        }
        if cookie.expired(now) {
            // An expired cookie only serves to delete the old one.
            self.drop_empty(&cookie.domain);
            return true;
        }
        list.push(cookie);
        self.total += 1;
        self.enforce_limits(now);
        true
    }

    /// The `Cookie` header value for a request to `url`, if any cookie
    /// applies. Marks the cookies used as accessed.
    pub fn cookie_header(&mut self, url: &Url, now: SystemTime) -> Option<String> {
        let host = request_host(url)?;
        let secure = is_secure(url);
        let path = url.path();
        let ip = host_is_ip(url);

        let mut matched: Vec<&mut StoredCookie> = Vec::new();
        for (domain, list) in &mut self.by_domain {
            let domain_ok = *domain == host || (!ip && domain_matches(&host, domain));
            if !domain_ok {
                continue;
            }
            for c in list.iter_mut() {
                if c.expired(now) || (c.host_only && c.domain != host) || (c.secure && !secure) {
                    continue;
                }
                if path_matches(path, &c.path) {
                    matched.push(c);
                }
            }
        }
        if matched.is_empty() {
            return None;
        }
        // Longer paths first, then older cookies first.
        matched.sort_by(|a, b| b.path.len().cmp(&a.path.len()).then(a.created.cmp(&b.created)));
        let mut out = String::new();
        for c in matched {
            c.last_access = now;
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(&c.name);
            out.push('=');
            out.push_str(&c.value);
        }
        Some(out)
    }

    /// Drop every cookie past its expiry.
    pub fn remove_expired(&mut self, now: SystemTime) {
        for list in self.by_domain.values_mut() {
            let before = list.len();
            list.retain(|c| !c.expired(now));
            self.total -= before - list.len();
        }
        self.by_domain.retain(|_, l| !l.is_empty());
    }

    fn drop_empty(&mut self, domain: &str) {
        if self.by_domain.get(domain).is_some_and(|l| l.is_empty()) {
            self.by_domain.remove(domain);
        }
    }

    fn enforce_limits(&mut self, now: SystemTime) {
        let over_domain = self.by_domain.iter().any(|(_, l)| l.len() > MAX_PER_DOMAIN);
        if over_domain || self.total > MAX_TOTAL {
            self.remove_expired(now);
        }
        for list in self.by_domain.values_mut() {
            while list.len() > MAX_PER_DOMAIN {
                let oldest = list
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, c)| c.last_access)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                list.remove(oldest);
                self.total -= 1;
            }
        }
        while self.total > MAX_TOTAL {
            let Some((domain, idx)) = self
                .by_domain
                .iter()
                .flat_map(|(d, l)| l.iter().enumerate().map(move |(i, c)| (d, i, c.last_access)))
                .min_by_key(|(_, _, at)| *at)
                .map(|(d, i, _)| (d.clone(), i))
            else {
                break;
            };
            if let Some(list) = self.by_domain.get_mut(&domain) {
                list.remove(idx);
                self.total -= 1;
                if list.is_empty() {
                    self.by_domain.remove(&domain);
                }
            }
        }
    }
}

fn is_secure(url: &Url) -> bool {
    matches!(url.scheme(), "https" | "wss")
}

fn request_host(url: &Url) -> Option<String> {
    match url.host()? {
        Host::Domain(d) => Some(d.to_ascii_lowercase()),
        Host::Ipv4(a) => Some(a.to_string()),
        Host::Ipv6(a) => Some(a.to_string()),
    }
}

fn host_is_ip(url: &Url) -> bool {
    matches!(url.host(), Some(Host::Ipv4(_) | Host::Ipv6(_)))
}

/// RFC 6265bis §5.1.3: `host` is `domain` or a subdomain of it.
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
            && host.parse::<std::net::IpAddr>().is_err())
}

/// RFC 6265bis §5.1.4: the default path of a cookie set without `Path`.
fn default_path(uri_path: &str) -> String {
    if !uri_path.starts_with('/') {
        return "/".to_owned();
    }
    match uri_path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(i) => uri_path[..i].to_owned(),
    }
}

/// RFC 6265bis §5.1.4: the request path is inside the cookie path.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    #[test]
    fn host_only_cookie_goes_back_to_its_host_only() {
        let mut jar = CookieJar::new();
        assert!(jar.store(&url("http://www.example.com/a/b"), "sid=1", now()));
        assert_eq!(jar.cookie_header(&url("http://www.example.com/a/c"), now()).as_deref(), Some("sid=1"));
        assert_eq!(jar.cookie_header(&url("http://example.com/a/c"), now()), None);
        assert_eq!(jar.cookie_header(&url("http://api.www.example.com/a"), now()), None);
        // Default path is the directory of the setting URL.
        assert_eq!(jar.cookie_header(&url("http://www.example.com/x"), now()), None);
    }

    #[test]
    fn domain_cookie_covers_subdomains() {
        let mut jar = CookieJar::new();
        assert!(jar.store(&url("http://www.example.com/"), "a=1; Domain=.example.com; Path=/", now()));
        assert_eq!(jar.cookie_header(&url("http://example.com/"), now()).as_deref(), Some("a=1"));
        assert_eq!(jar.cookie_header(&url("http://api.example.com/x/y"), now()).as_deref(), Some("a=1"));
        assert_eq!(jar.cookie_header(&url("http://notexample.com/"), now()), None);
    }

    #[test]
    fn foreign_and_public_suffix_domains_are_rejected() {
        let mut jar = CookieJar::new();
        assert!(!jar.store(&url("http://www.example.com/"), "a=1; Domain=other.com", now()));
        assert!(!jar.store(&url("http://www.example.com/"), "a=1; Domain=com", now()));
        assert!(!jar.store(&url("http://10.0.0.1/"), "a=1; Domain=0.0.1", now()));
        assert!(jar.store(&url("http://10.0.0.1/"), "a=1; Domain=10.0.0.1", now()));
        assert_eq!(jar.len(), 1);
    }

    #[test]
    fn secure_cookies_need_https_both_ways() {
        let mut jar = CookieJar::new();
        assert!(!jar.store(&url("http://example.com/"), "s=1; Secure", now()));
        assert!(jar.store(&url("https://example.com/"), "s=1; Secure", now()));
        assert_eq!(jar.cookie_header(&url("http://example.com/"), now()), None);
        assert_eq!(jar.cookie_header(&url("https://example.com/"), now()).as_deref(), Some("s=1"));
        // An insecure origin cannot shadow it.
        assert!(!jar.store(&url("http://example.com/"), "s=2", now()));
        assert_eq!(jar.cookie_header(&url("https://example.com/"), now()).as_deref(), Some("s=1"));
    }

    #[test]
    fn prefixes_are_enforced() {
        let mut jar = CookieJar::new();
        assert!(!jar.store(&url("https://example.com/"), "__Secure-a=1", now()));
        assert!(jar.store(&url("https://example.com/"), "__Secure-a=1; Secure", now()));
        assert!(!jar.store(&url("https://example.com/"), "__Host-b=1; Secure; Domain=example.com", now()));
        assert!(!jar.store(&url("https://example.com/"), "__Host-b=1; Secure; Path=/x", now()));
        assert!(jar.store(&url("https://example.com/x/"), "__Host-b=1; Secure; Path=/", now()));
        assert_eq!(jar.len(), 2);
    }

    #[test]
    fn expiry_and_deletion() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        assert!(jar.store(&u, "a=1; Max-Age=10", now()));
        assert!(jar.store(&u, "b=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT", now()));
        assert_eq!(jar.cookie_header(&u, now()).as_deref(), Some("a=1"));
        assert_eq!(jar.cookie_header(&u, now() + Duration::from_secs(11)), None);
        // Max-Age wins over Expires; a zero Max-Age deletes.
        assert!(jar.store(&u, "a=2; Max-Age=100; Expires=Wed, 21 Oct 2015 07:28:00 GMT", now()));
        assert_eq!(jar.cookie_header(&u, now() + Duration::from_secs(50)).as_deref(), Some("a=2"));
        assert!(jar.store(&u, "a=; Max-Age=0", now()));
        assert_eq!(jar.cookie_header(&u, now()), None);
        assert!(jar.is_empty());
    }

    #[test]
    fn overwrite_keeps_creation_order_and_paths_sort_first() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/a/b");
        assert!(jar.store(&u, "x=1; Path=/", now()));
        assert!(jar.store(&u, "y=1; Path=/a", now() + Duration::from_secs(1)));
        assert!(jar.store(&u, "x=2; Path=/", now() + Duration::from_secs(2)));
        assert!(jar.store(&u, "z=1; Path=/", now() + Duration::from_secs(3)));
        assert_eq!(jar.len(), 3);
        assert_eq!(
            jar.cookie_header(&u, now() + Duration::from_secs(4)).as_deref(),
            Some("y=1; x=2; z=1")
        );
    }

    #[test]
    fn per_domain_limit_evicts_least_recently_used() {
        let mut jar = CookieJar::new();
        let u = url("http://example.com/");
        for i in 0..MAX_PER_DOMAIN {
            assert!(jar.store(&u, &format!("c{i}=1"), now() + Duration::from_secs(i as u64)));
        }
        // Every cookie is accessed once, then c1 is made the least recently
        // used; the next store over the limit must evict exactly it.
        let header = jar.cookie_header(&u, now() + Duration::from_secs(10_000)).unwrap();
        assert!(header.contains("c0=1"));
        jar.by_domain.get_mut("example.com").unwrap()[1].last_access = UNIX_EPOCH;
        assert!(jar.store(&u, "extra=1", now() + Duration::from_secs(20_000)));
        assert_eq!(jar.len(), MAX_PER_DOMAIN);
        let header = jar.cookie_header(&u, now() + Duration::from_secs(30_000)).unwrap();
        assert!(!header.contains("c1=1"));
        assert!(header.contains("extra=1"));
    }

    #[test]
    fn garbage_is_rejected_and_data_urls_have_no_host() {
        let mut jar = CookieJar::new();
        assert!(!jar.store(&url("http://example.com/"), "", now()));
        assert!(!jar.store(&url("http://example.com/"), "=", now()));
        assert!(!jar.store(&url("data:text/html,hi"), "a=1", now()));
        assert_eq!(jar.cookie_header(&url("data:text/html,hi"), now()), None);
        let big = format!("a={}", "v".repeat(MAX_COOKIE_BYTES));
        assert!(!jar.store(&url("http://example.com/"), &big, now()));
    }

    #[test]
    fn path_matching_rules() {
        assert!(path_matches("/a/b", "/a"));
        assert!(path_matches("/a/", "/a/"));
        assert!(path_matches("/a", "/a"));
        assert!(!path_matches("/ab", "/a"));
        assert!(path_matches("/anything", "/"));
        assert_eq!(default_path("/a/b/c"), "/a/b");
        assert_eq!(default_path("/a"), "/");
        assert_eq!(default_path(""), "/");
    }
}
