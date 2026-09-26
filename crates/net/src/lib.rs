//! Network layer: the only code in the browser that opens sockets.
//!
//! An HTTP client on hyper with rustls and the pure-Rust crypto provider.
//! Requests run on a tokio runtime owned by `NetService`; results stream
//! back to the caller through a sink closure as `NetToTab` messages.
//!
//! Scope: GET, redirects, gzip/deflate/br, timeouts, `data:` URLs, a
//! cookie jar and an in-memory HTTP cache shared by every tab. The ad
//! filter comes in Phase 4.

#![forbid(unsafe_code)]

mod cache;
mod cookies;
mod decode;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use browser_ipc_types::{NetToTab, RequestId};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri, header};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use url::Url;

pub use cache::{CacheMode, CachedResponse, HttpCache, Lookup, Timing};
pub use cookies::{CookieJar, SameSite, StoredCookie};
use decode::Decoder;

/// The user agent string sent with every request.
pub const USER_AGENT: &str = concat!("browser/", env!("CARGO_PKG_VERSION"));

/// Redirects followed before giving up.
const MAX_REDIRECTS: usize = 20;
/// Time allowed for the response headers, and between body chunks.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Bodies larger than this are cut off.
const MAX_BODY: usize = 256 * 1024 * 1024;

type HttpsClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// A request from a tab.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: Url,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub cache: CacheMode,
}

impl FetchRequest {
    pub fn get(url: Url) -> Self {
        Self {
            url,
            method: Method::GET,
            headers: Vec::new(),
            body: None,
            cache: CacheMode::Default,
        }
    }
}

/// Where response events go. Called from runtime threads.
pub type Sink = Arc<dyn Fn(NetToTab) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("runtime: {0}")]
    Runtime(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
}

/// State every request consults: the cookie jar and the cache. Shared by
/// all tabs, since one browser has one jar (a login in one tab holds in
/// the next) and one cache.
#[derive(Debug, Default)]
struct Shared {
    cookies: Mutex<CookieJar>,
    cache: Mutex<HttpCache>,
}

impl Shared {
    fn cookies(&self) -> MutexGuard<'_, CookieJar> {
        self.cookies.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cache(&self) -> MutexGuard<'_, HttpCache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Owns the runtime and the HTTP client. One per browser.
pub struct NetService {
    runtime: tokio::runtime::Runtime,
    client: HttpsClient,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for NetService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NetService")
    }
}

impl NetService {
    pub fn new() -> Result<Self, NetError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("net")
            .enable_all()
            .build()?;

        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls_rustcrypto::provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();

        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .build(https);

        Ok(Self {
            runtime,
            client,
            shared: Arc::new(Shared::default()),
        })
    }

    /// The browser's cookie jar. Hold the guard briefly; requests on the
    /// runtime threads wait on it.
    pub fn cookies(&self) -> MutexGuard<'_, CookieJar> {
        self.shared.cookies()
    }

    /// The in-memory HTTP cache.
    pub fn cache(&self) -> MutexGuard<'_, HttpCache> {
        self.shared.cache()
    }

    /// Start a request. Events arrive on `sink` from a runtime thread until
    /// `ResponseEnd` or `Failed`.
    pub fn fetch(&self, id: RequestId, request: FetchRequest, sink: Sink) {
        if request.url.scheme() == "data" {
            fetch_data_url(id, &request.url, &sink);
            return;
        }
        let client = self.client.clone();
        let shared = self.shared.clone();
        self.runtime.spawn(async move {
            if let Err(e) = run(client, shared, id, request, &sink).await {
                sink(NetToTab::Failed {
                    id,
                    error: e.to_string(),
                });
            }
        });
    }
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("unsupported scheme {0}")]
    Scheme(String),
    #[error("invalid url")]
    Url,
    #[error("too many redirects")]
    Redirects,
    #[error("timed out")]
    Timeout,
    #[error("{0}")]
    Http(#[from] hyper_util::client::legacy::Error),
    #[error("body: {0}")]
    Body(#[from] hyper::Error),
    #[error("response too large")]
    TooLarge,
    #[error("decompression failed: {0}")]
    Decode(std::io::Error),
}

async fn run(
    client: HttpsClient,
    shared: Arc<Shared>,
    id: RequestId,
    request: FetchRequest,
    sink: &Sink,
) -> Result<(), FetchError> {
    let mut url = request.url.clone();
    let mut method = request.method.clone();
    let mut body = request.body.clone();
    let mut mode = request.cache;

    for _ in 0..=MAX_REDIRECTS {
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(FetchError::Scheme(other.to_owned())),
        }
        let uri: Uri = url.as_str().parse().map_err(|_| FetchError::Url)?;

        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"),
        );
        headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate, br"));
        for (k, v) in &request.headers {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                headers.insert(name, value);
            }
        }
        if let Some(cookie) = shared.cookies().cookie_header(&url, SystemTime::now())
            && let Ok(value) = HeaderValue::from_str(&cookie)
        {
            headers.insert(header::COOKIE, value);
        }
        let request_headers = header_list(&headers);

        // Only GET responses are cached.
        let key = HttpCache::key(&url);
        let mut conditional = false;
        if method == Method::GET {
            match shared.cache().lookup(&key, &request_headers, mode, Instant::now()) {
                Lookup::Fresh(cached) => {
                    serve_cached(id, &url, cached, sink);
                    return Ok(());
                }
                Lookup::Stale { etag, last_modified } => {
                    if let Some(v) = etag.and_then(|e| HeaderValue::from_str(&e).ok()) {
                        headers.insert(header::IF_NONE_MATCH, v);
                        conditional = true;
                    }
                    if let Some(v) = last_modified.and_then(|m| HeaderValue::from_str(&m).ok()) {
                        headers.insert(header::IF_MODIFIED_SINCE, v);
                        conditional = true;
                    }
                }
                Lookup::Miss => {}
            }
        }

        let mut req = Request::builder()
            .method(method.clone())
            .uri(uri)
            .body(Full::new(Bytes::from(body.clone().unwrap_or_default())))
            .map_err(|_| FetchError::Url)?;
        *req.headers_mut() = headers;

        let request_time = Instant::now();
        let response = tokio::time::timeout(TIMEOUT, client.request(req))
            .await
            .map_err(|_| FetchError::Timeout)??;
        let timing = Timing {
            request: request_time,
            response: Instant::now(),
            wall: SystemTime::now(),
        };
        let status = response.status();

        // Every hop may set cookies, redirects and 304s included.
        {
            let mut jar = shared.cookies();
            for value in response.headers().get_all(header::SET_COOKIE) {
                jar.store(&url, &String::from_utf8_lossy(value.as_bytes()), timing.wall);
            }
        }

        if status == StatusCode::NOT_MODIFIED && conditional {
            let head = header_list(response.headers());
            if let Some(cached) = shared.cache().revalidated(&key, &head, timing) {
                serve_cached(id, &url, cached, sink);
                return Ok(());
            }
            // The entry went away between lookup and answer: fetch it plain.
            mode = CacheMode::Reload;
            continue;
        }

        if status.is_redirection()
            && let Some(location) = response.headers().get(header::LOCATION)
            && let Ok(location) = location.to_str()
            && let Ok(next) = url.join(location)
        {
            // 303 always becomes GET; 301/302 do for POST, per browsers.
            if status == StatusCode::SEE_OTHER
                || ((status == StatusCode::MOVED_PERMANENTLY || status == StatusCode::FOUND) && method == Method::POST)
            {
                method = Method::GET;
                body = None;
            }
            url = next;
            continue;
        }

        let headers = header_list(response.headers());
        let policy = if method == Method::GET {
            shared.cache().policy(status.as_u16(), &headers, &request_headers, timing)
        } else {
            None
        };
        let max_entry = shared.cache().max_entry();
        let mut decoder = Decoder::for_encoding(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        );
        sink(NetToTab::ResponseStart {
            id,
            status: status.as_u16(),
            headers: headers.clone(),
            final_url: url.clone(),
        });

        // A copy of the decoded body is kept for the cache while it stays
        // under the entry limit.
        let mut copy: Option<Vec<u8>> = policy.as_ref().map(|_| Vec::new());
        let mut body_stream = response.into_body();
        let mut total = 0usize;
        loop {
            let frame = tokio::time::timeout(TIMEOUT, body_stream.frame())
                .await
                .map_err(|_| FetchError::Timeout)?;
            let Some(frame) = frame else { break };
            let frame = frame?;
            let Ok(data) = frame.into_data() else { continue };
            total += data.len();
            if total > MAX_BODY {
                return Err(FetchError::TooLarge);
            }
            let out = decoder.push(&data).map_err(FetchError::Decode)?;
            if !out.is_empty() {
                keep_copy(&mut copy, &out, max_entry);
                sink(NetToTab::ResponseChunk { id, bytes: out });
            }
        }
        let out = decoder.finish().map_err(FetchError::Decode)?;
        if !out.is_empty() {
            keep_copy(&mut copy, &out, max_entry);
            sink(NetToTab::ResponseChunk { id, bytes: out });
        }
        sink(NetToTab::ResponseEnd { id });
        if let (Some(policy), Some(copy)) = (policy, copy) {
            shared
                .cache()
                .store(key, status.as_u16(), headers, Bytes::from(copy), policy, timing);
        }
        return Ok(());
    }
    Err(FetchError::Redirects)
}

fn keep_copy(copy: &mut Option<Vec<u8>>, out: &[u8], max_entry: usize) {
    if let Some(buf) = copy {
        if buf.len() + out.len() > max_entry {
            *copy = None;
        } else {
            buf.extend_from_slice(out);
        }
    }
}

/// Deliver a cached response as if it had just arrived.
fn serve_cached(id: RequestId, url: &Url, cached: CachedResponse, sink: &Sink) {
    let mut headers = cached.headers;
    headers.push(("age".to_owned(), cached.age.to_string()));
    sink(NetToTab::ResponseStart {
        id,
        status: cached.status,
        headers,
        final_url: url.clone(),
    });
    if !cached.body.is_empty() {
        sink(NetToTab::ResponseChunk {
            id,
            bytes: cached.body.to_vec(),
        });
    }
    sink(NetToTab::ResponseEnd { id });
}

fn header_list(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect()
}

/// `data:` URLs are answered synchronously from the caller's thread.
fn fetch_data_url(id: RequestId, url: &Url, sink: &Sink) {
    let parsed = data_url::DataUrl::process(url.as_str());
    let (bytes, mime) = match parsed.and_then(|d| {
        let mime = d.mime_type().to_string();
        d.decode_to_vec().map(|(b, _)| (b, mime)).map_err(|_| data_url::DataUrlError::NotADataUrl)
    }) {
        Ok(v) => v,
        Err(_) => {
            sink(NetToTab::Failed {
                id,
                error: "invalid data: URL".to_owned(),
            });
            return;
        }
    };
    sink(NetToTab::ResponseStart {
        id,
        status: 200,
        headers: vec![("content-type".to_owned(), mime)],
        final_url: url.clone(),
    });
    sink(NetToTab::ResponseChunk { id, bytes });
    sink(NetToTab::ResponseEnd { id });
}

/// Content type helpers used by tabs.
pub fn content_type(headers: &[(String, String)]) -> Option<mime::Mime> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .and_then(|(_, v)| v.parse::<mime::Mime>().ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{Receiver, channel};

    #[test]
    fn data_url_round_trip() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let e2 = events.clone();
        let sink: Sink = Arc::new(move |ev| e2.lock().unwrap().push(ev));
        let url = Url::parse("data:text/plain;base64,aGVsbG8=").unwrap();
        fetch_data_url(RequestId(1), &url, &sink);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        match &events[1] {
            NetToTab::ResponseChunk { bytes, .. } => assert_eq!(bytes, b"hello"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn service_constructs() {
        // Builds the TLS config with the pure-Rust provider and the runtime.
        let svc = NetService::new().expect("net service");
        drop(svc);
    }

    // ----- loopback server -----

    /// A one-connection-at-a-time HTTP/1.1 responder on 127.0.0.1 that
    /// records each request head and answers with the next canned
    /// response. Not the internet: nothing leaves the machine.
    struct Server {
        base: Url,
        heads: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let heads = Arc::new(Mutex::new(Vec::new()));
            let recorded = heads.clone();
            std::thread::spawn(move || {
                for response in responses {
                    let Ok((mut stream, _)) = listener.accept() else { return };
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = stream.read(&mut buf) else { break };
                        if n == 0 {
                            break;
                        }
                        head.extend_from_slice(&buf[..n]);
                    }
                    recorded
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&head).to_ascii_lowercase());
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self {
                base: Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
                heads,
            }
        }

        fn url(&self, path: &str) -> Url {
            self.base.join(path).unwrap()
        }

        fn heads(&self) -> Vec<String> {
            self.heads.lock().unwrap().clone()
        }
    }

    fn response(status: &str, extra_headers: &[&str], body: &str) -> String {
        let mut s = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        for h in extra_headers {
            s.push_str(h);
            s.push_str("\r\n");
        }
        s.push_str("\r\n");
        s.push_str(body);
        s
    }

    struct Fetched {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    fn fetch(svc: &NetService, request: FetchRequest) -> Result<Fetched, String> {
        let (tx, rx): (_, Receiver<NetToTab>) = channel();
        let sink: Sink = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        svc.fetch(RequestId(1), request, sink);
        let mut out = Fetched {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
        };
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).expect("net answered") {
                NetToTab::ResponseStart { status, headers, .. } => {
                    out.status = status;
                    out.headers = headers;
                }
                NetToTab::ResponseChunk { bytes, .. } => out.body.extend(bytes),
                NetToTab::ResponseEnd { .. } => return Ok(out),
                NetToTab::Failed { error, .. } => return Err(error),
            }
        }
    }

    fn has_header(headers: &[(String, String)], name: &str) -> bool {
        headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    #[test]
    fn cookie_set_by_one_response_is_sent_with_the_next_request() {
        let server = Server::start(vec![
            response("200 OK", &["Set-Cookie: sid=abc; Path=/", "Set-Cookie: other=1; Path=/private"], "one"),
            response("200 OK", &[], "two"),
        ]);
        let svc = NetService::new().unwrap();
        let a = fetch(&svc, FetchRequest::get(server.url("/a"))).unwrap();
        assert_eq!(a.body, b"one");
        let b = fetch(&svc, FetchRequest::get(server.url("/b"))).unwrap();
        assert_eq!(b.body, b"two");
        let heads = server.heads();
        assert_eq!(heads.len(), 2);
        assert!(!heads[0].contains("cookie:"));
        assert!(heads[1].contains("cookie: sid=abc\r\n"), "{}", heads[1]);
        assert_eq!(svc.cookies().len(), 2);
    }

    #[test]
    fn cookie_set_on_a_redirect_reaches_the_next_hop() {
        let server = Server::start(vec![
            response("302 Found", &["Location: /landing", "Set-Cookie: hop=1"], ""),
            response("200 OK", &[], "landed"),
        ]);
        let svc = NetService::new().unwrap();
        let r = fetch(&svc, FetchRequest::get(server.url("/start"))).unwrap();
        assert_eq!(r.body, b"landed");
        let heads = server.heads();
        assert!(heads[1].starts_with("get /landing "));
        assert!(heads[1].contains("cookie: hop=1\r\n"), "{}", heads[1]);
    }

    #[test]
    fn fresh_response_is_served_from_cache_without_a_request() {
        let server = Server::start(vec![
            response("200 OK", &["Cache-Control: max-age=60", "Content-Type: text/plain"], "cached"),
            response("200 OK", &[], "should not be asked"),
        ]);
        let svc = NetService::new().unwrap();
        let first = fetch(&svc, FetchRequest::get(server.url("/page"))).unwrap();
        assert_eq!(first.body, b"cached");
        assert!(!has_header(&first.headers, "age"));
        let second = fetch(&svc, FetchRequest::get(server.url("/page#frag"))).unwrap();
        assert_eq!(second.status, 200);
        assert_eq!(second.body, b"cached");
        assert!(has_header(&second.headers, "age"));
        assert!(has_header(&second.headers, "content-type"));
        assert_eq!(server.heads().len(), 1);
        assert_eq!(svc.cache().len(), 1);
    }

    #[test]
    fn stale_entry_is_revalidated_and_a_304_serves_the_stored_body() {
        let server = Server::start(vec![
            response("200 OK", &["Cache-Control: no-cache", "ETag: \"v1\""], "stored"),
            response("304 Not Modified", &["ETag: \"v1\"", "Cache-Control: max-age=60"], ""),
            response("200 OK", &[], "unreachable"),
        ]);
        let svc = NetService::new().unwrap();
        fetch(&svc, FetchRequest::get(server.url("/doc"))).unwrap();
        let second = fetch(&svc, FetchRequest::get(server.url("/doc"))).unwrap();
        assert_eq!(second.status, 200);
        assert_eq!(second.body, b"stored");
        let heads = server.heads();
        assert_eq!(heads.len(), 2);
        assert!(heads[1].contains("if-none-match: \"v1\"\r\n"), "{}", heads[1]);
        // The 304 made it fresh: no third request.
        let third = fetch(&svc, FetchRequest::get(server.url("/doc"))).unwrap();
        assert_eq!(third.body, b"stored");
        assert_eq!(server.heads().len(), 2);
    }

    #[test]
    fn reload_bypasses_the_cache_and_replaces_the_entry() {
        let server = Server::start(vec![
            response("200 OK", &["Cache-Control: max-age=60"], "one"),
            response("200 OK", &["Cache-Control: max-age=60"], "two"),
        ]);
        let svc = NetService::new().unwrap();
        fetch(&svc, FetchRequest::get(server.url("/r"))).unwrap();
        let mut reload = FetchRequest::get(server.url("/r"));
        reload.cache = CacheMode::Reload;
        assert_eq!(fetch(&svc, reload).unwrap().body, b"two");
        // Now cached: the plain fetch sees "two" without a third request.
        assert_eq!(fetch(&svc, FetchRequest::get(server.url("/r"))).unwrap().body, b"two");
        assert_eq!(server.heads().len(), 2);
    }

    #[test]
    fn no_store_responses_are_fetched_every_time() {
        let server = Server::start(vec![
            response("200 OK", &["Cache-Control: no-store"], "a"),
            response("200 OK", &["Cache-Control: no-store"], "b"),
        ]);
        let svc = NetService::new().unwrap();
        assert_eq!(fetch(&svc, FetchRequest::get(server.url("/n"))).unwrap().body, b"a");
        assert_eq!(fetch(&svc, FetchRequest::get(server.url("/n"))).unwrap().body, b"b");
        assert!(svc.cache().is_empty());
    }
}
