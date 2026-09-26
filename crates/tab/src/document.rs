//! Per-tab state: the current document, its resources, and the render
//! pipeline from DOM to scene.

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::{Document, HtmlParser};
use browser_ipc_types::{NetToTab, RequestId, ShellToTab, TabId, TabToShell, Viewport};
use browser_layout::{LayoutEngine, LayoutTree};
use browser_net::{FetchRequest, NetService, Sink};
use browser_paint::{ImageStore, PaintOptions, decode_image, paint};
use browser_style::media::MediaQueryList;
use browser_style::ua::ua_stylesheet;
use browser_style::{Origin, Rule, StyleMap, Stylesheet, Stylist, compute_styles};
use html5ever::{local_name, ns};
use url::Url;
use vello::Scene;

use crate::{OutputSink, TabOutput};

/// Nested `@import` depth allowed.
const MAX_IMPORT_DEPTH: u8 = 6;

enum PendingKind {
    Main,
    Stylesheet {
        /// Index into `sheets`.
        slot: usize,
        depth: u8,
    },
    Image {
        url: Url,
    },
}

struct Pending {
    kind: PendingKind,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// One author stylesheet in cascade order, possibly still loading.
struct SheetSlot {
    url: Option<Url>,
    media: MediaQueryList,
    sheet: Option<Arc<Stylesheet>>,
}

pub(crate) struct TabState {
    id: TabId,
    net: Arc<NetService>,
    net_sink: Sink,
    output: Arc<OutputSink>,
    viewport: Viewport,
    scroll_x: f32,
    scroll_y: f32,

    url: Option<Url>,
    doc: Option<Document>,
    parser: Option<HtmlParser>,
    main_request: Option<RequestId>,
    next_request: u64,
    pending: HashMap<RequestId, Pending>,
    sheets: Vec<SheetSlot>,
    fetched_urls: std::collections::HashSet<Url>,
    images: ImageStore,

    engine: LayoutEngine,
    styles: StyleMap,
    layout: Option<LayoutTree>,
    scene: Scene,

    history: Vec<Url>,
    history_index: usize,

    needs_layout: bool,
    needs_paint: bool,
    state_dirty: bool,
}

impl TabState {
    pub fn new(id: TabId, net: Arc<NetService>, net_sink: Sink, viewport: Viewport, output: Arc<OutputSink>) -> Self {
        Self {
            id,
            net,
            net_sink,
            output,
            viewport,
            scroll_x: 0.0,
            scroll_y: 0.0,
            url: None,
            doc: None,
            parser: None,
            main_request: None,
            next_request: 1,
            pending: HashMap::new(),
            sheets: Vec::new(),
            fetched_urls: Default::default(),
            images: ImageStore::new(),
            engine: LayoutEngine::new(),
            styles: StyleMap::new(),
            layout: None,
            scene: Scene::new(),
            history: Vec::new(),
            history_index: 0,
            needs_layout: false,
            needs_paint: false,
            state_dirty: false,
        }
    }

    pub fn send(&self, msg: TabToShell) {
        (self.output)(self.id, TabOutput::Message(msg));
    }

    fn is_loading(&self) -> bool {
        self.main_request.is_some() || !self.pending.is_empty()
    }

    fn send_state(&mut self) {
        let title = self.doc.as_ref().and_then(|d| d.title());
        let url = self
            .url
            .clone()
            .unwrap_or_else(|| Url::parse("about:blank").expect("static url"));
        self.send(TabToShell::StateChanged {
            url,
            title,
            loading: self.is_loading(),
            can_go_back: self.history_index > 0,
            can_go_forward: self.history_index + 1 < self.history.len(),
        });
        self.state_dirty = false;
    }

    // ----- shell messages -----

    pub fn handle_shell(&mut self, msg: ShellToTab) {
        match msg {
            ShellToTab::Navigate { url } => {
                self.push_history(url.clone());
                self.navigate(url);
            }
            ShellToTab::Reload => {
                if let Some(url) = self.url.clone() {
                    self.navigate(url);
                }
            }
            ShellToTab::Stop => {
                self.main_request = None;
                self.pending.clear();
                self.parser = None;
                self.state_dirty = true;
            }
            ShellToTab::GoBack => {
                if self.history_index > 0 {
                    self.history_index -= 1;
                    let url = self.history[self.history_index].clone();
                    self.navigate(url);
                }
            }
            ShellToTab::GoForward => {
                if self.history_index + 1 < self.history.len() {
                    self.history_index += 1;
                    let url = self.history[self.history_index].clone();
                    self.navigate(url);
                }
            }
            ShellToTab::Resize(vp) => {
                if vp != self.viewport {
                    self.viewport = vp;
                    self.needs_layout = true;
                }
            }
            ShellToTab::Scroll { dx, dy } => {
                self.scroll_x += dx;
                self.scroll_y += dy;
                self.clamp_scroll();
                self.needs_paint = true;
            }
            ShellToTab::Close => {}
        }
    }

    fn push_history(&mut self, url: Url) {
        if !self.history.is_empty() {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(url);
        self.history_index = self.history.len() - 1;
    }

    fn navigate(&mut self, url: Url) {
        tracing::info!(tab = self.id.0, "navigate {url}");
        self.pending.clear();
        self.parser = None;
        self.sheets.clear();
        self.fetched_urls.clear();
        self.images = ImageStore::new();
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.url = Some(url.clone());
        let id = self.fetch(FetchRequest::get(url), PendingKind::Main);
        self.main_request = Some(id);
        self.state_dirty = true;
    }

    fn fetch(&mut self, request: FetchRequest, kind: PendingKind) -> RequestId {
        let id = RequestId(self.next_request);
        self.next_request += 1;
        self.pending.insert(
            id,
            Pending {
                kind,
                status: 0,
                headers: Vec::new(),
                body: Vec::new(),
            },
        );
        self.net.fetch(id, request, self.net_sink.clone());
        id
    }

    // ----- network messages -----

    pub fn handle_net(&mut self, msg: NetToTab) {
        match msg {
            NetToTab::ResponseStart {
                id,
                status,
                headers,
                final_url,
            } => {
                let Some(p) = self.pending.get_mut(&id) else { return };
                p.status = status;
                p.headers = headers;
                if matches!(p.kind, PendingKind::Main) {
                    self.url = Some(final_url.clone());
                    if let Some(i) = self.history.get_mut(self.history_index) {
                        *i = final_url.clone();
                    }
                    self.start_main_document(id, final_url);
                }
            }
            NetToTab::ResponseChunk { id, bytes } => {
                let Some(p) = self.pending.get_mut(&id) else { return };
                if matches!(p.kind, PendingKind::Main) {
                    if let Some(parser) = &mut self.parser {
                        parser.feed(&bytes);
                    } else {
                        p.body.extend_from_slice(&bytes);
                    }
                } else {
                    p.body.extend_from_slice(&bytes);
                }
            }
            NetToTab::ResponseEnd { id } => {
                let Some(p) = self.pending.remove(&id) else { return };
                self.finish_response(id, p);
            }
            NetToTab::Failed { id, error } => {
                let Some(p) = self.pending.remove(&id) else { return };
                tracing::warn!(tab = self.id.0, "request {} failed: {error}", id.0);
                if matches!(p.kind, PendingKind::Main) {
                    self.main_request = None;
                    let url = self.url.clone().map(|u| u.to_string()).unwrap_or_default();
                    self.show_error_page(&url, &error);
                }
                self.state_dirty = true;
            }
        }
    }

    fn content_type_of(headers: &[(String, String)]) -> (String, Option<String>) {
        match browser_net::content_type(headers) {
            Some(m) => (
                format!("{}/{}", m.type_(), m.subtype()),
                m.get_param("charset").map(|c| c.to_string()),
            ),
            None => (String::new(), None),
        }
    }

    fn start_main_document(&mut self, id: RequestId, final_url: Url) {
        let Some(p) = self.pending.get(&id) else { return };
        let (ct, charset) = Self::content_type_of(&p.headers);
        let html_like = ct.is_empty()
            || ct == "text/html"
            || ct == "application/xhtml+xml"
            || ct == "application/xml"
            || ct == "text/xml";
        if html_like {
            self.parser = Some(HtmlParser::with_charset(Some(final_url), charset.as_deref()));
        }
        // Other types are wrapped when the body completes.
    }

    fn finish_response(&mut self, _id: RequestId, p: Pending) {
        match p.kind {
            PendingKind::Main => {
                self.main_request = None;
                let doc = if let Some(parser) = self.parser.take() {
                    parser.finish()
                } else {
                    let (ct, _) = Self::content_type_of(&p.headers);
                    let body = String::from_utf8_lossy(&p.body);
                    let html = if ct.starts_with("text/") || ct.ends_with("json") || ct.ends_with("javascript") {
                        format!(
                            "<!doctype html><html><head><title>{}</title></head><body><pre>{}</pre></body></html>",
                            escape(self.url.as_ref().map(|u| u.as_str()).unwrap_or("")),
                            escape(&body)
                        )
                    } else if ct.starts_with("image/") {
                        let src = self.url.as_ref().map(|u| u.as_str()).unwrap_or("");
                        format!(
                            "<!doctype html><html><head><title>{0}</title></head><body style='margin:0;background:#0e0e0e;display:flex;justify-content:center;align-items:center'><img src='{0}'></body></html>",
                            escape(src)
                        )
                    } else {
                        format!(
                            "<!doctype html><html><head><title>Cannot display</title></head><body><h1>Cannot display this content</h1><p>Type: <code>{}</code>. Downloads arrive in a later phase.</p></body></html>",
                            escape(&ct)
                        )
                    };
                    let mut parser = HtmlParser::new(self.url.clone());
                    parser.feed(html.as_bytes());
                    parser.finish()
                };
                self.set_document(doc);
            }
            PendingKind::Stylesheet { slot, depth } => {
                if p.status == 0 || (200..300).contains(&p.status) {
                    let (_, charset) = Self::content_type_of(&p.headers);
                    let css = browser_dom::encoding::decode_stylesheet(&p.body, charset.as_deref());
                    tracing::debug!(
                        tab = self.id.0,
                        bytes = css.len(),
                        url = ?self.sheets.get(slot).and_then(|s| s.url.as_ref()).map(|u| u.as_str()),
                        "external stylesheet loaded"
                    );
                    let base = self.sheets.get(slot).and_then(|s| s.url.clone());
                    let sheet = Stylesheet::parse_with_base(&css, Origin::Author, base.as_ref());
                    // Imports are inserted before this sheet, moving its slot.
                    let slot = slot + self.queue_imports(slot, &sheet, depth);
                    if let Some(s) = self.sheets.get_mut(slot) {
                        s.sheet = Some(Arc::new(sheet));
                    }
                    self.needs_layout = true;
                }
            }
            PendingKind::Image { url } => {
                if let Some(img) = decode_image(&p.body) {
                    self.images.insert(url, img);
                    self.needs_layout = true;
                }
            }
        }
        self.state_dirty = true;
    }

    fn show_error_page(&mut self, url: &str, error: &str) {
        let html = format!(
            "<!doctype html><html><head><title>Cannot load page</title></head>\
             <body style='font-family:sans-serif;margin:40px'><h1>This page cannot be loaded</h1>\
             <p><code>{}</code></p><p>{}</p></body></html>",
            escape(url),
            escape(error)
        );
        let mut parser = HtmlParser::new(self.url.clone());
        parser.feed(html.as_bytes());
        let doc = parser.finish();
        self.set_document(doc);
    }

    /// A finished document: collect its stylesheets and images, then render.
    fn set_document(&mut self, doc: Document) {
        self.sheets.clear();
        self.doc = Some(doc);
        self.collect_stylesheets();
        self.collect_images();
        self.needs_layout = true;
        self.state_dirty = true;
    }

    fn collect_stylesheets(&mut self) {
        let Some(doc) = &self.doc else { return };
        let base = doc.base_url.clone();
        let mut found: Vec<(Option<Url>, MediaQueryList, Option<String>)> = Vec::new();
        for id in doc.descendants(doc.root()) {
            let Some(e) = doc.element(id) else { continue };
            if e.name.ns != ns!(html) {
                continue;
            }
            let media = parse_media_attr(e.attr("media"));
            if e.name.local == local_name!("style") {
                let css = doc.text_content(id);
                tracing::debug!(tab = self.id.0, "inline stylesheet:\n{css}");
                found.push((None, media, Some(css)));
            } else if e.name.local == local_name!("link") {
                let rel = e.attr("rel").unwrap_or("");
                let is_sheet = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet"));
                let alternate = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("alternate"));
                if is_sheet && !alternate
                    && let Some(href) = e.attr("href")
                    && let Some(url) = doc.resolve_url(href)
                {
                    found.push((Some(url), media, None));
                }
            }
        }
        for (url, media, inline) in found {
            let slot = self.sheets.len();
            match (url, inline) {
                (None, Some(css)) => {
                    let sheet = Stylesheet::parse_with_base(&css, Origin::Author, base.as_ref());
                    self.sheets.push(SheetSlot {
                        url: base.clone(),
                        media,
                        sheet: None,
                    });
                    let slot = slot + self.queue_imports(slot, &sheet, 0);
                    self.sheets[slot].sheet = Some(Arc::new(sheet));
                }
                (Some(url), _) => {
                    self.sheets.push(SheetSlot {
                        url: Some(url.clone()),
                        media,
                        sheet: None,
                    });
                    self.fetch(FetchRequest::get(url), PendingKind::Stylesheet { slot, depth: 0 });
                }
                _ => {}
            }
        }
    }

    /// Insert slots for a sheet's `@import`s before it and fetch them.
    /// Returns how many slots were inserted before `slot`.
    fn queue_imports(&mut self, slot: usize, sheet: &Stylesheet, depth: u8) -> usize {
        if depth >= MAX_IMPORT_DEPTH || sheet.imports.is_empty() {
            return 0;
        }
        let mut insert_at = slot;
        for (href, media) in &sheet.imports {
            let Ok(url) = Url::parse(href) else { continue };
            if self.fetched_urls.contains(&url) {
                continue;
            }
            self.fetched_urls.insert(url.clone());
            self.sheets.insert(
                insert_at,
                SheetSlot {
                    url: Some(url.clone()),
                    media: media.clone(),
                    sheet: None,
                },
            );
            // Slots after the insertion point moved by one; fix pending kinds.
            for p in self.pending.values_mut() {
                if let PendingKind::Stylesheet { slot: s, .. } = &mut p.kind
                    && *s >= insert_at
                {
                    *s += 1;
                }
            }
            self.fetch(
                FetchRequest::get(url),
                PendingKind::Stylesheet {
                    slot: insert_at,
                    depth: depth + 1,
                },
            );
            insert_at += 1;
        }
        insert_at - slot
    }

    fn collect_images(&mut self) {
        let Some(doc) = &self.doc else { return };
        let mut urls = Vec::new();
        for id in doc.descendants(doc.root()) {
            let Some(e) = doc.element(id) else { continue };
            if e.name.ns == ns!(html)
                && e.name.local == local_name!("img")
                && let Some(src) = e.attr("src")
                && let Some(url) = doc.resolve_url(src)
            {
                urls.push(url);
            }
        }
        for url in urls {
            self.fetch_image(url);
        }
    }

    fn fetch_image(&mut self, url: Url) {
        if self.fetched_urls.contains(&url) || self.images.contains(&url) {
            return;
        }
        if !matches!(url.scheme(), "http" | "https" | "data") {
            return;
        }
        self.fetched_urls.insert(url.clone());
        self.fetch(FetchRequest::get(url.clone()), PendingKind::Image { url });
    }

    /// Background images only become known after the cascade.
    fn fetch_background_images(&mut self) {
        let mut urls = Vec::new();
        for (_, style) in self.styles.iter() {
            if let Some(u) = &style.background_image
                && let Ok(url) = Url::parse(u)
            {
                urls.push(url);
            }
        }
        for url in urls {
            self.fetch_image(url);
        }
    }

    // ----- rendering -----

    fn stylist(&self) -> Stylist {
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        for slot in &self.sheets {
            let Some(sheet) = &slot.sheet else { continue };
            // A `media` attribute wraps the whole sheet.
            let sheet = if slot.media == MediaQueryList::all() {
                sheet.clone()
            } else {
                Arc::new(Stylesheet {
                    origin: Origin::Author,
                    rules: vec![Rule::Media(slot.media.clone(), sheet.rules.clone())],
                    imports: Vec::new(),
                })
            };
            stylist.add_sheet(sheet);
        }
        stylist
    }

    fn clamp_scroll(&mut self) {
        let (cw, ch) = self
            .layout
            .as_ref()
            .map(|l| (l.content_width, l.content_height))
            .unwrap_or((self.viewport.width, self.viewport.height));
        let max_x = (cw - self.viewport.width).max(0.0);
        let max_y = (ch - self.viewport.height).max(0.0);
        self.scroll_x = self.scroll_x.clamp(0.0, max_x);
        self.scroll_y = self.scroll_y.clamp(0.0, max_y);
    }

    /// Called after each batch of events: do the work that is due.
    pub fn flush(&mut self) {
        if self.needs_layout && self.doc.is_some() {
            self.relayout();
            self.needs_layout = false;
            self.needs_paint = true;
        }
        if self.needs_paint && self.layout.is_some() {
            self.repaint();
            self.needs_paint = false;
        }
        if self.state_dirty {
            self.send_state();
        }
    }

    fn relayout(&mut self) {
        let Some(doc) = &self.doc else { return };
        let started = std::time::Instant::now();
        let stylist = self.stylist();
        let vp = browser_style::Viewport {
            width: self.viewport.width,
            height: self.viewport.height,
            scale_factor: self.viewport.scale_factor,
            prefers_dark: false,
        };
        self.styles = compute_styles(doc, &stylist, &vp);
        let styled = started.elapsed();
        let tree = self
            .engine
            .layout(doc, &self.styles, self.viewport.width, self.viewport.height, &self.images);
        tracing::debug!(
            tab = self.id.0,
            nodes = doc.node_count(),
            style_ms = styled.as_millis(),
            layout_ms = (started.elapsed() - styled).as_millis(),
            content_height = tree.content_height,
            "relayout"
        );
        self.layout = Some(tree);
        self.clamp_scroll();
        // Background images only become known after the cascade; fetching
        // them re-triggers layout when they arrive.
        self.fetch_background_images();
    }

    fn repaint(&mut self) {
        let Some(tree) = &self.layout else { return };
        let options = PaintOptions {
            scroll_x: self.scroll_x,
            scroll_y: self.scroll_y,
            viewport_width: self.viewport.width,
            viewport_height: self.viewport.height,
            scale: self.viewport.scale_factor,
        };
        paint(tree, &self.images, &options, &mut self.scene);
        let scene = std::mem::take(&mut self.scene);
        (self.output)(self.id, TabOutput::Frame(scene));
    }
}

fn parse_media_attr(attr: Option<&str>) -> MediaQueryList {
    match attr {
        Some(s) => MediaQueryList::parse_str(s),
        None => MediaQueryList::all(),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
