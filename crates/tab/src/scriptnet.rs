//! The Fetch standard's algorithm for the requests scripts make (`fetch()`
//! and `XMLHttpRequest`; "Working sites first" line 7).
//!
//! A `Job` is one request as a state machine: it says what to send to the
//! network next and what to tell the script, and is fed what the network
//! answers. Redirects are followed here, one hop at a time (the net layer
//! hands each redirect response back), because every hop has checks of
//! its own: the request's mode against the hop's origin, the CORS check
//! on the response, a preflight for a request that is not simple, which
//! credentials go along, and what the script may see of the result (the
//! filtered response). The tab drives jobs from its event loop; a
//! synchronous `XMLHttpRequest` drives one to its end on the spot
//! (`BlockingNet`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use browser_ipc_types::{NetToTab, RequestId};
use browser_net::{CacheMode, Credentials, FetchRequest, NetService, Redirect, Sink};
use browser_script::{
    CredentialsMode, NetFailure, NetRequest, NetResponseHead, RedirectMode, RequestCache, RequestMode, ResponseKind, SyncNet,
    is_cors_safelisted_request_header,
};
use sha2::Digest;
use url::Url;

/// Redirects followed before a fetch fails (Fetch §4.4).
const MAX_REDIRECTS: u32 = 20;
/// How long a preflight result is kept without `Access-Control-Max-Age`,
/// and the most it may ask for (Chrome's cap).
const PREFLIGHT_DEFAULT_AGE: u64 = 5;
const PREFLIGHT_MAX_AGE: u64 = 7200;

/// What a job asks of its driver.
#[derive(Debug)]
pub(crate) enum Action {
    /// Send this request; what the network answers for it is fed back,
    /// and answers to an earlier one are dropped.
    Send(FetchRequest),
    Head(NetResponseHead),
    Data(Vec<u8>),
    Done,
    Fail(NetFailure),
}

/// Fetch's response tainting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tainting {
    Basic,
    Cors,
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Preflight,
    Main,
}

/// The CORS-preflight cache (Fetch §4.9): what preflights allowed, until
/// they expire.
#[derive(Debug, Default)]
pub(crate) struct PreflightCache {
    entries: Vec<PreflightEntry>,
}

#[derive(Debug)]
struct PreflightEntry {
    origin: String,
    url: String,
    /// The preflight was for a request with credentials.
    credentials: bool,
    methods: Vec<String>,
    headers: Vec<String>,
    expires: Instant,
}

impl PreflightCache {
    /// Whether cached preflights cover `method` and every one of
    /// `unsafe_names` for this origin, URL and credentials mode. A
    /// safelisted method needs no entry of its own unless the preflight is
    /// `forced` (an upload with listeners).
    fn covers(&mut self, origin: &str, url: &Url, credentials: bool, method: &str, unsafe_names: &[String], forced: bool) -> bool {
        let now = Instant::now();
        self.entries.retain(|e| e.expires > now);
        let matching: Vec<&PreflightEntry> = self
            .entries
            .iter()
            .filter(|e| e.origin == origin && e.url == url.as_str() && (e.credentials || !credentials))
            .collect();
        let method_ok = (is_cors_safelisted_method(method) && !forced)
            || matching.iter().any(|e| e.methods.iter().any(|m| m == method || (!credentials && m == "*")));
        let headers_ok = unsafe_names.iter().all(|name| {
            matching.iter().any(|e| {
                e.headers.iter().any(|h| h.eq_ignore_ascii_case(name) || (!credentials && h == "*" && name != "authorization"))
            })
        });
        method_ok && headers_ok
    }

    fn store(&mut self, origin: String, url: &Url, credentials: bool, methods: Vec<String>, headers: Vec<String>, max_age: u64) {
        if max_age == 0 {
            return;
        }
        self.entries.push(PreflightEntry {
            origin,
            url: url.as_str().to_owned(),
            credentials,
            methods,
            headers,
            expires: Instant::now() + Duration::from_secs(max_age),
        });
        // Bounded: the oldest go first.
        if self.entries.len() > 256 {
            let excess = self.entries.len() - 256;
            self.entries.drain(..excess);
        }
    }
}

pub(crate) type SharedPreflights = Arc<Mutex<PreflightCache>>;

fn is_cors_safelisted_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "POST")
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Every value of a header that is a comma-separated list, split.
fn header_list(headers: &[(String, String)], name: &str) -> Vec<String> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .flat_map(|(_, v)| v.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Fetch's CORS-unsafe request-header names of a header list: the names
/// that are not safelisted (all of them once the safelisted values pass
/// 1024 bytes together), lower-cased, sorted, each once.
fn cors_unsafe_names(headers: &[(String, String)]) -> Vec<String> {
    let mut unsafe_names = Vec::new();
    let mut potentially = Vec::new();
    let mut safelisted_size = 0;
    for (name, value) in headers {
        if is_cors_safelisted_request_header(name, value) {
            potentially.push(name.to_ascii_lowercase());
            safelisted_size += value.len();
        } else {
            unsafe_names.push(name.to_ascii_lowercase());
        }
    }
    if safelisted_size > 1024 {
        unsafe_names.extend(potentially);
    }
    unsafe_names.sort();
    unsafe_names.dedup();
    unsafe_names
}

fn has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

/// One request a script made, on its way through the Fetch algorithm.
#[derive(Debug)]
pub(crate) struct Job {
    request: NetRequest,
    /// The document's origin.
    origin: url::Origin,
    /// The current URL, method, body and headers: redirects change them.
    url: Url,
    method: String,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
    tainting: Tainting,
    /// A cross-origin redirect came from a cross-origin hop: the `Origin`
    /// header is `null` from here on.
    tainted_origin: bool,
    redirects: u32,
    phase: Phase,
    /// The head went to the script: the body follows.
    streaming: bool,
    /// The body is not for the script (an opaque response).
    discard_body: bool,
    /// With integrity metadata: the head and the body so far, held until
    /// the body is whole and checked.
    held: Option<(NetResponseHead, Vec<u8>)>,
    /// The request's referrer and policy (none: no referrer): each hop
    /// determines it again from what the last one determined, under the
    /// policy a redirect's `Referrer-Policy` may have set.
    referrer: Option<browser_ipc_types::RequestReferrer>,
}

impl Job {
    /// Start `request` for a document at `document_url`.
    pub(crate) fn start(request: NetRequest, document_url: &Url, referrer: Option<browser_ipc_types::RequestReferrer>, preflights: &mut PreflightCache) -> (Self, Vec<Action>) {
        let mut job = Self {
            origin: document_url.origin(),
            url: request.url.clone(),
            method: request.method.clone(),
            body: request.body.clone(),
            headers: request.headers.clone(),
            tainting: Tainting::Basic,
            tainted_origin: false,
            redirects: 0,
            phase: Phase::Main,
            streaming: false,
            discard_body: false,
            held: None,
            referrer,
            request,
        };
        let actions = job.main_fetch(preflights);
        (job, actions)
    }

    fn fail(why: impl Into<String>) -> Vec<Action> {
        vec![Action::Fail(NetFailure::Network(why.into()))]
    }

    fn include_credentials(&self) -> bool {
        match self.request.credentials {
            CredentialsMode::Include => true,
            CredentialsMode::SameOrigin => self.tainting == Tainting::Basic,
            CredentialsMode::Omit => false,
        }
    }

    /// The request's origin as the `Origin` header and the CORS check
    /// have it.
    fn serialized_origin(&self) -> String {
        if self.tainted_origin { "null".to_owned() } else { self.origin.ascii_serialization() }
    }

    /// Fetch's "main fetch" for the current URL: the mode decides whether
    /// the hop is allowed and what tainting its response gets, then the
    /// request (or its preflight) goes out.
    fn main_fetch(&mut self, preflights: &mut PreflightCache) -> Vec<Action> {
        match self.url.scheme() {
            "http" | "https" => {}
            // A `data:` URL is its own response, whatever the mode.
            "data" => {
                self.phase = Phase::Main;
                let mut request = FetchRequest::get(self.url.clone());
                request.referrer = self.referrer.clone();
                return vec![Action::Send(request)];
            }
            "about" if self.url.path() == "blank" && self.request.mode == RequestMode::NoCors => {
                self.tainting = Tainting::Opaque;
                return vec![Action::Head(self.opaque_head()), Action::Done];
            }
            other => return Self::fail(format!("fetch of a {other}: URL")),
        }
        let same_origin = self.url.origin() == self.origin;
        if !(same_origin && self.tainting == Tainting::Basic) {
            match self.request.mode {
                RequestMode::SameOrigin => {
                    return Self::fail(format!("{} is not same-origin, which the request's mode requires", self.url));
                }
                RequestMode::NoCors => {
                    if self.request.redirect != RedirectMode::Follow {
                        return Self::fail("a no-cors request must follow redirects");
                    }
                    self.tainting = Tainting::Opaque;
                }
                RequestMode::Cors => self.tainting = Tainting::Cors,
            }
        }
        if self.tainting == Tainting::Cors {
            let unsafe_names = cors_unsafe_names(&self.headers);
            let needs = self.request.force_preflight || !is_cors_safelisted_method(&self.method) || !unsafe_names.is_empty();
            if needs
                && !preflights.covers(
                    &self.serialized_origin(),
                    &self.url,
                    self.request.credentials == CredentialsMode::Include,
                    &self.method,
                    &unsafe_names,
                    self.request.force_preflight,
                )
            {
                self.phase = Phase::Preflight;
                return vec![Action::Send(self.preflight_request(&unsafe_names))];
            }
        }
        self.phase = Phase::Main;
        vec![Action::Send(self.network_request())]
    }

    /// Fetch's CORS-preflight request: `OPTIONS`, without credentials or
    /// the author's headers, naming the method and the headers asked for.
    fn preflight_request(&self, unsafe_names: &[String]) -> FetchRequest {
        let mut request = FetchRequest::get(self.url.clone());
        if let Ok(method) = "OPTIONS".parse() {
            request.method = method;
        }
        request.headers.push(("accept".to_owned(), "*/*".to_owned()));
        request.headers.push(("access-control-request-method".to_owned(), self.method.clone()));
        if !unsafe_names.is_empty() {
            request.headers.push(("access-control-request-headers".to_owned(), unsafe_names.join(",")));
        }
        request.headers.push(("origin".to_owned(), self.serialized_origin()));
        request.credentials = Credentials::Omit;
        request.cache = CacheMode::NoStore;
        request.redirect = Redirect::Manual;
        // The preflight's referrer and policy are the request's (Fetch's
        // CORS-preflight fetch).
        request.referrer = self.referrer.clone();
        request
    }

    /// The request of the current hop (Fetch's HTTP-network-or-cache
    /// fetch): the author's headers and the ones the browser adds.
    fn network_request(&self) -> FetchRequest {
        let mut request = FetchRequest::get(self.url.clone());
        if let Ok(method) = self.method.parse() {
            request.method = method;
        }
        request.body = self.body.clone();
        request.redirect = Redirect::Manual;
        request.headers = self.headers.clone();
        request.referrer = self.referrer.clone();
        let has = |headers: &[(String, String)], name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
        if !has(&request.headers, "accept") {
            request.headers.push(("accept".to_owned(), "*/*".to_owned()));
        }
        // The `Origin` header: on every CORS request, and on any request
        // that is neither GET nor HEAD.
        if self.tainting == Tainting::Cors || !matches!(self.method.as_str(), "GET" | "HEAD") {
            let downgrade = matches!(&self.origin, url::Origin::Tuple(scheme, _, _) if scheme == "https") && self.url.scheme() != "https";
            let origin = if downgrade && self.tainting != Tainting::Cors { "null".to_owned() } else { self.serialized_origin() };
            request.headers.push(("origin".to_owned(), origin));
        }
        // The cache mode: a request with its own validators is not for
        // the cache; the modes that skip it say so to the caches upstream.
        let conditional = ["if-modified-since", "if-none-match", "if-unmodified-since", "if-match", "if-range"]
            .iter()
            .any(|name| has(&request.headers, name));
        let cache = match self.request.cache {
            RequestCache::Default if conditional => RequestCache::NoStore,
            other => other,
        };
        request.cache = match cache {
            RequestCache::Default => CacheMode::Default,
            RequestCache::NoStore => CacheMode::NoStore,
            RequestCache::Reload => CacheMode::Reload,
            RequestCache::NoCache => CacheMode::NoCache,
            RequestCache::ForceCache => CacheMode::ForceCache,
            RequestCache::OnlyIfCached => CacheMode::OnlyIfCached,
        };
        match cache {
            RequestCache::NoCache if !conditional && !has(&request.headers, "cache-control") => {
                request.headers.push(("cache-control".to_owned(), "max-age=0".to_owned()));
            }
            RequestCache::NoStore | RequestCache::Reload => {
                if !has(&request.headers, "pragma") {
                    request.headers.push(("pragma".to_owned(), "no-cache".to_owned()));
                }
                if !has(&request.headers, "cache-control") {
                    request.headers.push(("cache-control".to_owned(), "no-cache".to_owned()));
                }
            }
            _ => {}
        }
        request.credentials = if self.include_credentials() { Credentials::Include } else { Credentials::Omit };
        request
    }

    /// Fetch's CORS check on a response's headers.
    fn cors_check(&self, headers: &[(String, String)]) -> bool {
        let Some(allowed) = header(headers, "access-control-allow-origin") else { return false };
        let include = self.request.credentials == CredentialsMode::Include;
        if !include && allowed == "*" {
            return true;
        }
        if allowed != self.serialized_origin() {
            return false;
        }
        !include || header(headers, "access-control-allow-credentials") == Some("true")
    }

    fn opaque_head(&self) -> NetResponseHead {
        NetResponseHead {
            status: 0,
            status_text: String::new(),
            headers: Vec::new(),
            url: self.url.clone(),
            redirected: false,
            kind: ResponseKind::Opaque,
        }
    }

    /// The head as the script may see it (Fetch's filtered responses).
    fn filtered_head(&self, status: u16, status_text: String, headers: Vec<(String, String)>) -> NetResponseHead {
        let forbidden = |name: &str| name.eq_ignore_ascii_case("set-cookie") || name.eq_ignore_ascii_case("set-cookie2");
        let (kind, headers) = match self.tainting {
            Tainting::Opaque => return self.opaque_head(),
            Tainting::Basic => (ResponseKind::Basic, headers.into_iter().filter(|(k, _)| !forbidden(k)).collect()),
            Tainting::Cors => {
                let exposed = header_list(&headers, "access-control-expose-headers");
                let all = self.request.credentials != CredentialsMode::Include && exposed.iter().any(|e| e == "*");
                const SAFELISTED: &[&str] =
                    &["cache-control", "content-language", "content-length", "content-type", "expires", "last-modified", "pragma"];
                let visible: Vec<(String, String)> = headers
                    .into_iter()
                    .filter(|(k, _)| {
                        !forbidden(k)
                            && (all || SAFELISTED.iter().any(|s| k.eq_ignore_ascii_case(s)) || exposed.iter().any(|e| e.eq_ignore_ascii_case(k)))
                    })
                    .collect();
                (ResponseKind::Cors, visible)
            }
        };
        NetResponseHead {
            status,
            status_text,
            headers,
            url: self.url.clone(),
            redirected: self.redirects > 0,
            kind,
        }
    }

    /// A hop's response head arrived.
    pub(crate) fn on_start(
        &mut self,
        status: u16,
        status_text: String,
        headers: Vec<(String, String)>,
        hop_referrer: Option<Url>,
        preflights: &mut PreflightCache,
    ) -> Vec<Action> {
        if self.phase == Phase::Preflight {
            return self.on_preflight(status, &headers, preflights);
        }
        // The hop determined the request's referrer (main fetch, step 9).
        match (hop_referrer, &mut self.referrer) {
            (Some(u), Some(r)) => r.source = u,
            _ => self.referrer = None,
        }
        if self.url.scheme() == "data" {
            let head = NetResponseHead {
                status,
                status_text,
                headers,
                url: self.url.clone(),
                redirected: self.redirects > 0,
                kind: ResponseKind::Basic,
            };
            return self.deliver_head(head);
        }
        if self.tainting == Tainting::Cors && !self.cors_check(&headers) {
            return Self::fail(format!(
                "CORS: {} does not allow the origin {} (Access-Control-Allow-Origin)",
                self.url,
                self.serialized_origin()
            ));
        }
        let location = header(&headers, "location").map(str::to_owned);
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            match self.request.redirect {
                RedirectMode::Error => return Self::fail(format!("{} redirects, which the request's redirect mode forbids", self.url)),
                RedirectMode::Manual => {
                    self.discard_body = true;
                    let head = NetResponseHead {
                        status: 0,
                        status_text: String::new(),
                        headers: Vec::new(),
                        url: self.url.clone(),
                        redirected: false,
                        kind: ResponseKind::OpaqueRedirect,
                    };
                    return vec![Action::Head(head), Action::Done];
                }
                RedirectMode::Follow => {
                    if let Some(location) = location {
                        // HTTP-redirect fetch, step 19: the redirect's own
                        // `Referrer-Policy` becomes the request's.
                        let policy = browser_ipc_types::ReferrerPolicy::from_headers(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                        if policy != browser_ipc_types::ReferrerPolicy::Empty
                            && let Some(r) = &mut self.referrer
                        {
                            r.policy = policy;
                        }
                        return self.redirect(status, &location, preflights);
                    }
                }
            }
        }
        if self.tainting == Tainting::Opaque {
            // Cross-Origin-Resource-Policy: a resource may refuse to be
            // loaded by another origin (or site) without CORS.
            match header(&headers, "cross-origin-resource-policy") {
                Some("same-origin") => return Self::fail("Cross-Origin-Resource-Policy: same-origin"),
                Some("same-site") if !self.same_site() => return Self::fail("Cross-Origin-Resource-Policy: same-site"),
                _ => {}
            }
            if !self.request.integrity.trim().is_empty() {
                return Self::fail("integrity cannot be checked on an opaque response");
            }
            self.discard_body = true;
            self.streaming = true;
            return vec![Action::Head(self.opaque_head())];
        }
        let head = self.filtered_head(status, status_text, headers);
        self.deliver_head(head)
    }

    /// Hand the head over, or hold it when the body must be checked first.
    fn deliver_head(&mut self, head: NetResponseHead) -> Vec<Action> {
        if !self.request.integrity.trim().is_empty() {
            self.held = Some((head, Vec::new()));
            return Vec::new();
        }
        self.streaming = true;
        vec![Action::Head(head)]
    }

    /// Whether the current URL is of the document's site, schemelessly:
    /// the same registrable domain by the public suffix list (or the same
    /// host, for an IP address or a public suffix).
    fn same_site(&self) -> bool {
        let url::Origin::Tuple(_, host, _) = &self.origin else { return false };
        let Some(theirs) = self.url.host_str() else { return false };
        browser_ipc_types::referrer::same_site_hosts(&host.to_string(), theirs)
    }

    /// Fetch's CORS-preflight fetch, the response's side: the origin, the
    /// method and the headers must be allowed.
    fn on_preflight(&mut self, status: u16, headers: &[(String, String)], preflights: &mut PreflightCache) -> Vec<Action> {
        if !self.cors_check(headers) || !(200..=299).contains(&status) {
            return Self::fail(format!("CORS: the preflight for {} was refused (status {status})", self.url));
        }
        let methods = header_list(headers, "access-control-allow-methods");
        let names = header_list(headers, "access-control-allow-headers");
        let include = self.request.credentials == CredentialsMode::Include;
        let method_ok = methods.contains(&self.method)
            || is_cors_safelisted_method(&self.method)
            || (!include && methods.iter().any(|m| m == "*"));
        if !method_ok {
            return Self::fail(format!("CORS: the preflight for {} does not allow {}", self.url, self.method));
        }
        let unsafe_names = cors_unsafe_names(&self.headers);
        for name in &unsafe_names {
            let listed = names.iter().any(|n| n.eq_ignore_ascii_case(name));
            let wildcard = !include && name != "authorization" && names.iter().any(|n| n == "*");
            if !listed && !wildcard {
                return Self::fail(format!("CORS: the preflight for {} does not allow the header {name}", self.url));
            }
        }
        let max_age = header(headers, "access-control-max-age")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(PREFLIGHT_DEFAULT_AGE)
            .min(PREFLIGHT_MAX_AGE);
        preflights.store(self.serialized_origin(), &self.url, include, methods, names, max_age);
        self.phase = Phase::Main;
        vec![Action::Send(self.network_request())]
    }

    /// Fetch's HTTP-redirect fetch.
    fn redirect(&mut self, status: u16, location: &str, preflights: &mut PreflightCache) -> Vec<Action> {
        let Ok(mut next) = self.url.join(location) else {
            return Self::fail(format!("{} redirects to an invalid URL", self.url));
        };
        if !matches!(next.scheme(), "http" | "https") {
            return Self::fail(format!("{} redirects to a {}: URL", self.url, next.scheme()));
        }
        if self.redirects >= MAX_REDIRECTS {
            return Self::fail("too many redirects");
        }
        self.redirects += 1;
        if next.fragment().is_none() {
            next.set_fragment(self.url.fragment());
        }
        if self.request.mode == RequestMode::Cors && has_credentials(&next) && next.origin() != self.origin {
            return Self::fail("a cross-origin redirect to a URL with credentials");
        }
        if self.tainting == Tainting::Cors && has_credentials(&next) {
            return Self::fail("a CORS redirect to a URL with credentials");
        }
        if (matches!(status, 301 | 302) && self.method == "POST") || (status == 303 && !matches!(self.method.as_str(), "GET" | "HEAD")) {
            self.method = "GET".to_owned();
            self.body = None;
            self.headers.retain(|(k, _)| {
                !["content-encoding", "content-language", "content-location", "content-type"]
                    .iter()
                    .any(|n| k.eq_ignore_ascii_case(n))
            });
        }
        if next.origin() != self.url.origin() {
            self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
            if self.url.origin() != self.origin {
                self.tainted_origin = true;
            }
        }
        self.url = next;
        self.main_fetch(preflights)
    }

    /// More of the current hop's body.
    pub(crate) fn on_chunk(&mut self, bytes: Vec<u8>) -> Vec<Action> {
        if self.phase == Phase::Preflight || self.discard_body {
            return Vec::new();
        }
        if let Some((_, body)) = &mut self.held {
            body.extend_from_slice(&bytes);
            return Vec::new();
        }
        if self.streaming { vec![Action::Data(bytes)] } else { Vec::new() }
    }

    /// The current hop's body ended.
    pub(crate) fn on_end(&mut self) -> Vec<Action> {
        if self.phase == Phase::Preflight {
            return Vec::new();
        }
        if let Some((head, body)) = self.held.take() {
            if !integrity_matches(&self.request.integrity, &body) {
                return Self::fail(format!("the integrity of {} does not match", self.url));
            }
            return vec![Action::Head(head), Action::Data(body), Action::Done];
        }
        if self.streaming { vec![Action::Done] } else { Vec::new() }
    }

    /// The current hop failed on the wire.
    pub(crate) fn on_failed(&mut self, error: String) -> Vec<Action> {
        Self::fail(error)
    }
}

// ----- subresource integrity -----

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            // The URL-safe alphabet is accepted too, as the SRI
            // specification's grammar allows.
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// SRI's "do bytes match metadataList": the hashes of the strongest
/// algorithm named; metadata naming no supported algorithm matches
/// anything.
fn integrity_matches(metadata: &str, bytes: &[u8]) -> bool {
    let mut strongest = 0u8;
    let mut wanted: Vec<Vec<u8>> = Vec::new();
    for item in metadata.split_ascii_whitespace() {
        let item = item.split('?').next().unwrap_or(item);
        let Some((algorithm, value)) = item.split_once('-') else { continue };
        let strength = match algorithm.to_ascii_lowercase().as_str() {
            "sha256" => 1,
            "sha384" => 2,
            "sha512" => 3,
            _ => continue,
        };
        let Some(hash) = base64_decode(value) else { continue };
        if strength > strongest {
            strongest = strength;
            wanted.clear();
        }
        if strength == strongest {
            wanted.push(hash);
        }
    }
    let actual: Vec<u8> = match strongest {
        0 => return true,
        1 => sha2::Sha256::digest(bytes).to_vec(),
        2 => sha2::Sha384::digest(bytes).to_vec(),
        _ => sha2::Sha512::digest(bytes).to_vec(),
    };
    wanted.contains(&actual)
}

// ----- synchronous requests -----

/// A synchronous `XMLHttpRequest`'s network: a job driven to its end on
/// the calling (tab) thread, with a channel of its own for the answers.
pub(crate) struct BlockingNet {
    pub(crate) net: Arc<NetService>,
    pub(crate) document_url: Url,
    pub(crate) preflights: SharedPreflights,
    /// The document's referrer for a request now (its URL and policy as
    /// the tab last synced them, before each script runs).
    pub(crate) referrer: std::rc::Rc<std::cell::RefCell<Option<browser_ipc_types::RequestReferrer>>>,
}

/// Request ids of blocking fetches; their answers never reach a tab's
/// own channel.
static BLOCKING_ID: AtomicU64 = AtomicU64::new(1 << 62);

impl SyncNet for BlockingNet {
    fn fetch(&self, request: NetRequest) -> Result<(NetResponseHead, Vec<u8>), NetFailure> {
        let (tx, rx) = channel::<NetToTab>();
        let sink: Sink = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let lock = || self.preflights.lock().unwrap_or_else(|e| e.into_inner());
        let referrer = self.referrer.borrow().clone();
        let (mut job, mut actions) = Job::start(request, &self.document_url, referrer, &mut lock());
        let mut current: Option<RequestId> = None;
        let mut head: Option<NetResponseHead> = None;
        let mut body = Vec::new();
        loop {
            for action in std::mem::take(&mut actions) {
                match action {
                    Action::Send(request) => {
                        let id = RequestId(BLOCKING_ID.fetch_add(1, Ordering::Relaxed));
                        current = Some(id);
                        self.net.fetch(id, request, sink.clone());
                    }
                    Action::Head(h) => head = Some(h),
                    Action::Data(bytes) => body.extend_from_slice(&bytes),
                    Action::Done => {
                        return head.map(|h| (h, body)).ok_or_else(|| NetFailure::Network("no response".to_owned()));
                    }
                    Action::Fail(failure) => return Err(failure),
                }
            }
            let Ok(event) = rx.recv() else {
                return Err(NetFailure::Network("the network went away".to_owned()));
            };
            actions = match event {
                NetToTab::ResponseStart {
                    id,
                    status,
                    status_text,
                    headers,
                    referrer,
                    ..
                } if Some(id) == current => job.on_start(status, status_text, headers, referrer.map(|r| *r), &mut lock()),
                NetToTab::ResponseChunk { id, bytes } if Some(id) == current => job.on_chunk(bytes),
                NetToTab::ResponseEnd { id } if Some(id) == current => job.on_end(),
                NetToTab::Failed { id, error } if Some(id) == current => job.on_failed(error),
                _ => Vec::new(),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_takes_the_strongest_algorithm_named() {
        // sha256 and sha384 of "hello".
        let sha256 = "sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
        let sha384 = "sha384-WeF0h3dEjGnea4ANejO7+5/xtGPkQ1TDVTvNucZm+pASWjx5+QOXvfX2oT3oKGhP";
        assert!(integrity_matches(sha256, b"hello"));
        assert!(!integrity_matches(sha256, b"hello!"));
        assert!(integrity_matches(&format!("sha256-AAAA {sha384}"), b"hello"), "the weaker one is not consulted");
        assert!(!integrity_matches(&format!("{sha256} sha384-AAAA"), b"hello"));
        assert!(integrity_matches("md5-abc nonsense", b"anything"), "nothing supported: no check");
        assert!(integrity_matches(&format!("{sha256}?opt"), b"hello"));
    }

    #[test]
    fn unsafe_header_names_are_what_a_preflight_asks_for() {
        let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<Vec<_>>();
        assert!(cors_unsafe_names(&h(&[("Accept", "a/b"), ("Content-Type", "text/plain;charset=UTF-8")])).is_empty());
        assert_eq!(cors_unsafe_names(&h(&[("X-B", "1"), ("Content-Type", "application/json"), ("x-a", "2")])), vec!["content-type", "x-a", "x-b"]);
    }
}
