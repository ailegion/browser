//! Network layer: the only code in the browser that opens sockets.
//!
//! An HTTP client on hyper with rustls and the pure-Rust crypto provider.
//! Requests run on a tokio runtime owned by `NetService`; results stream
//! back to the caller through a sink closure as `NetToTab` messages.
//!
//! Phase 1 scope: GET, redirects, gzip/deflate/br, timeouts, `data:` URLs.
//! Cookies, cache and the ad filter come in later phases.

#![forbid(unsafe_code)]

mod decode;

use std::sync::Arc;
use std::time::Duration;

use browser_ipc_types::{NetToTab, RequestId};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode, Uri, header};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use url::Url;

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
}

impl FetchRequest {
    pub fn get(url: Url) -> Self {
        Self {
            url,
            method: Method::GET,
            headers: Vec::new(),
            body: None,
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

/// Owns the runtime and the HTTP client. One per browser.
pub struct NetService {
    runtime: tokio::runtime::Runtime,
    client: HttpsClient,
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

        Ok(Self { runtime, client })
    }

    /// Start a request. Events arrive on `sink` from a runtime thread until
    /// `ResponseEnd` or `Failed`.
    pub fn fetch(&self, id: RequestId, request: FetchRequest, sink: Sink) {
        if request.url.scheme() == "data" {
            fetch_data_url(id, &request.url, &sink);
            return;
        }
        let client = self.client.clone();
        self.runtime.spawn(async move {
            if let Err(e) = run(client, id, request, &sink).await {
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

async fn run(client: HttpsClient, id: RequestId, request: FetchRequest, sink: &Sink) -> Result<(), FetchError> {
    let mut url = request.url.clone();
    let mut method = request.method.clone();
    let mut body = request.body.clone();

    for _ in 0..=MAX_REDIRECTS {
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(FetchError::Scheme(other.to_owned())),
        }
        let uri: Uri = url.as_str().parse().map_err(|_| FetchError::Url)?;

        let mut req = Request::builder()
            .method(method.clone())
            .uri(uri)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(header::ACCEPT_ENCODING, "gzip, deflate, br");
        for (k, v) in &request.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let req = req
            .body(Full::new(Bytes::from(body.clone().unwrap_or_default())))
            .map_err(|_| FetchError::Url)?;

        let response = tokio::time::timeout(TIMEOUT, client.request(req))
            .await
            .map_err(|_| FetchError::Timeout)??;

        let status = response.status();
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
            headers,
            final_url: url.clone(),
        });

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
                sink(NetToTab::ResponseChunk { id, bytes: out });
            }
        }
        let out = decoder.finish().map_err(FetchError::Decode)?;
        if !out.is_empty() {
            sink(NetToTab::ResponseChunk { id, bytes: out });
        }
        sink(NetToTab::ResponseEnd { id });
        return Ok(());
    }
    Err(FetchError::Redirects)
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
    use std::sync::Mutex;

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
}
