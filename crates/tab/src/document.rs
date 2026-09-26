//! Per-tab state: the current document, its resources, and the render
//! pipeline from DOM to scene.

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::{Document, HtmlParser, NodeId};
use browser_ipc_types::{Cursor, MouseButton, NetToTab, RequestId, ShellToTab, TabId, TabToShell, Viewport};
use browser_layout::{LayoutEngine, LayoutTree};
use browser_net::{FetchRequest, NetService, Sink};
use browser_paint::{ImageStore, PaintOptions, decode_image, paint};
use browser_style::media::MediaQueryList;
use browser_style::ua::ua_stylesheet;
use browser_style::{
    ElementStates, InteractionDeps, Origin, Reach, Rule, StyleMap, Stylesheet, Stylist, compute_styles_with, restyle,
};
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

    /// Interaction state behind `:hover`, `:active` and `:focus`, and how
    /// far a change in it reaches under the current sheets.
    states: ElementStates,
    deps: InteractionDeps,
    hover: Option<NodeId>,
    focus: Option<NodeId>,
    /// Last pointer position in viewport coordinates while over the page.
    mouse: Option<(f32, f32)>,
    cursor: Cursor,

    history: Vec<Url>,
    history_index: usize,

    /// Styles must be recomputed from scratch (new document, sheet, or
    /// viewport); implies layout.
    needs_style: bool,
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
            states: ElementStates::default(),
            deps: InteractionDeps::default(),
            hover: None,
            focus: None,
            mouse: None,
            cursor: Cursor::Default,
            history: Vec::new(),
            history_index: 0,
            needs_style: false,
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
                    // Media queries may change with the viewport.
                    self.needs_style = true;
                }
            }
            ShellToTab::Scroll { dx, dy } => {
                self.scroll_x += dx;
                self.scroll_y += dy;
                self.clamp_scroll();
                self.needs_paint = true;
                self.update_hover();
            }
            ShellToTab::MouseMove { x, y } => {
                self.mouse = Some((x, y));
                self.update_hover();
            }
            ShellToTab::MouseLeave => {
                self.mouse = None;
                self.update_hover();
            }
            ShellToTab::MouseDown { x, y, button } => {
                self.mouse = Some((x, y));
                self.update_hover();
                if button == MouseButton::Left {
                    let hit = self.hover;
                    self.set_active(hit);
                    let focus = hit.and_then(|h| self.focusable_ancestor(h));
                    self.set_focus(focus);
                }
            }
            ShellToTab::MouseUp { x, y, button } => {
                self.mouse = Some((x, y));
                if button == MouseButton::Left {
                    self.set_active(None);
                }
                self.update_hover();
            }
            ShellToTab::Close => {}
        }
    }

    // ----- interaction state -----

    /// The innermost node under a viewport point, text nodes included.
    fn hit_node(&self, x: f32, y: f32) -> Option<NodeId> {
        self.layout
            .as_ref()?
            .hit_test(x + self.scroll_x, y + self.scroll_y)
    }

    fn element_of(&self, node: NodeId) -> Option<NodeId> {
        let doc = self.doc.as_ref()?;
        if doc.get(node).is_element() {
            Some(node)
        } else {
            doc.parent(node).filter(|&p| doc.get(p).is_element())
        }
    }

    fn ancestor_or_self(&self, id: NodeId, pred: impl Fn(&browser_dom::Element) -> bool) -> Option<NodeId> {
        let doc = self.doc.as_ref()?;
        let mut cur = Some(id);
        while let Some(n) = cur {
            if let Some(e) = doc.element(n)
                && pred(e)
            {
                return Some(n);
            }
            cur = doc.parent(n);
        }
        None
    }

    fn focusable_ancestor(&self, id: NodeId) -> Option<NodeId> {
        self.ancestor_or_self(id, |e| {
            e.name.ns == ns!(html)
                && (is_link(e)
                    || (matches!(
                        e.name.local,
                        local_name!("input") | local_name!("button") | local_name!("select") | local_name!("textarea")
                    ) && e.attr("disabled").is_none())
                    || e.attr("tabindex").is_some())
        })
    }

    /// Re-evaluate what is under the pointer; restyle if it changed.
    fn update_hover(&mut self) {
        let raw = self.mouse.and_then(|(x, y)| self.hit_node(x, y));
        let hit = raw.and_then(|n| self.element_of(n));
        if hit == self.hover {
            return;
        }
        self.hover = hit;

        let over_link = hit.and_then(|h| self.ancestor_or_self(h, is_link)).is_some();
        let over_text = raw.is_some_and(|n| self.doc.as_ref().is_some_and(|d| !d.get(n).is_element()));
        let cursor = if over_link {
            Cursor::Pointer
        } else if over_text {
            Cursor::Text
        } else {
            Cursor::Default
        };
        if cursor != self.cursor {
            self.cursor = cursor;
            self.send(TabToShell::Cursor(cursor));
        }

        let Some(doc) = &self.doc else { return };
        let changed = self.states.set_chain(doc, hit, ElementStates::HOVER);
        let reach = self.deps.hover;
        self.restyle_for(changed, reach);
    }

    fn set_active(&mut self, target: Option<NodeId>) {
        let Some(doc) = &self.doc else { return };
        let changed = self.states.set_chain(doc, target, ElementStates::ACTIVE);
        let reach = self.deps.active;
        self.restyle_for(changed, reach);
    }

    fn set_focus(&mut self, target: Option<NodeId>) {
        if target == self.focus {
            return;
        }
        self.focus = target;
        let Some(doc) = &self.doc else { return };
        let mut changed = self.states.set_single(target, ElementStates::FOCUS);
        changed.extend(self.states.set_chain(doc, target, ElementStates::FOCUS_WITHIN));
        changed.sort();
        changed.dedup();
        let reach = self.deps.focus;
        self.restyle_for(changed, reach);
    }

    /// Restyle what a state change on `changed` can affect, per `reach`,
    /// and schedule layout if any computed style moved.
    fn restyle_for(&mut self, changed: Vec<NodeId>, reach: Reach) {
        if changed.is_empty() || reach == Reach::None {
            return;
        }
        let Some(doc) = &self.doc else { return };
        let (roots, descendants) = match reach {
            Reach::None => return,
            Reach::Element => (changed, false),
            Reach::Subtree => (changed, true),
            Reach::Parent => {
                let mut parents: Vec<NodeId> = changed
                    .iter()
                    .map(|&n| doc.parent(n).filter(|&p| doc.get(p).is_element()).unwrap_or(n))
                    .collect();
                parents.sort();
                parents.dedup();
                (parents, true)
            }
            Reach::Document => (doc.document_element().into_iter().collect(), true),
        };
        let started = std::time::Instant::now();
        let stylist = self.stylist();
        let vp = self.style_viewport();
        let moved = restyle(doc, &stylist, &vp, &self.states, &mut self.styles, &roots, descendants);
        tracing::debug!(
            tab = self.id.0,
            roots = roots.len(),
            ?reach,
            moved,
            ms = started.elapsed().as_millis(),
            "restyle for interaction"
        );
        if moved {
            self.needs_layout = true;
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
                    self.needs_style = true;
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
        // Node ids of the old document mean nothing now. The pointer
        // position is kept: hover is re-evaluated after the first layout.
        self.states = ElementStates::default();
        self.hover = None;
        self.focus = None;
        self.collect_stylesheets();
        self.collect_images();
        self.needs_style = true;
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
        // Style, then layout, then paint. A new layout can put a different
        // element under the pointer, whose hover styles need one more
        // round; two rounds always settle it.
        for _ in 0..3 {
            if self.doc.is_none() {
                break;
            }
            if self.needs_style {
                self.restyle_all();
                self.needs_style = false;
                self.needs_layout = true;
            }
            if self.needs_layout {
                self.relayout();
                self.needs_layout = false;
                self.needs_paint = true;
                self.update_hover();
            }
            if !self.needs_style && !self.needs_layout {
                break;
            }
        }
        if self.needs_paint && self.layout.is_some() {
            self.repaint();
            self.needs_paint = false;
        }
        if self.state_dirty {
            self.send_state();
        }
    }

    fn style_viewport(&self) -> browser_style::Viewport {
        browser_style::Viewport {
            width: self.viewport.width,
            height: self.viewport.height,
            scale_factor: self.viewport.scale_factor,
            prefers_dark: false,
        }
    }

    fn restyle_all(&mut self) {
        let Some(doc) = &self.doc else { return };
        let started = std::time::Instant::now();
        let stylist = self.stylist();
        let vp = self.style_viewport();
        self.styles = compute_styles_with(doc, &stylist, &vp, &self.states);
        self.deps = stylist.interaction_deps();
        tracing::debug!(
            tab = self.id.0,
            nodes = doc.node_count(),
            style_ms = started.elapsed().as_millis(),
            deps = ?self.deps,
            "restyle"
        );
        // Background images only become known after the cascade; fetching
        // them re-triggers layout when they arrive.
        self.fetch_background_images();
    }

    fn relayout(&mut self) {
        let Some(doc) = &self.doc else { return };
        let started = std::time::Instant::now();
        let tree = self
            .engine
            .layout(doc, &self.styles, self.viewport.width, self.viewport.height, &self.images);
        tracing::debug!(
            tab = self.id.0,
            layout_ms = started.elapsed().as_millis(),
            content_height = tree.content_height,
            "relayout"
        );
        self.layout = Some(tree);
        self.clamp_scroll();
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

fn is_link(e: &browser_dom::Element) -> bool {
    e.name.ns == ns!(html)
        && matches!(e.name.local, local_name!("a") | local_name!("area"))
        && e.attr("href").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};

    use browser_layout::Rect;

    /// A tab with a document loaded from a `data:` URL, which the net
    /// service answers synchronously, so no network and no threads.
    struct Harness {
        state: TabState,
        net_events: Receiver<NetToTab>,
        messages: Arc<Mutex<Vec<TabToShell>>>,
    }

    impl Harness {
        fn load(html: &str) -> Self {
            let net = Arc::new(NetService::new().expect("net service"));
            let (tx, net_events) = channel();
            let net_sink: Sink = Arc::new(move |ev| {
                let _ = tx.send(ev);
            });
            let messages = Arc::new(Mutex::new(Vec::new()));
            let sink_messages = messages.clone();
            let output: Arc<OutputSink> = Arc::new(Box::new(move |_, out| {
                if let TabOutput::Message(m) = out {
                    sink_messages.lock().expect("lock").push(m);
                }
            }));
            let viewport = Viewport {
                width: 800.0,
                height: 600.0,
                scale_factor: 1.0,
            };
            let state = TabState::new(TabId(1), net, net_sink, viewport, output);
            let mut h = Self {
                state,
                net_events,
                messages,
            };
            let encoded: String = html
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            let url = Url::parse(&format!("data:text/html,{encoded}")).expect("data url");
            h.send(ShellToTab::Navigate { url });
            h
        }

        fn send(&mut self, msg: ShellToTab) {
            self.state.handle_shell(msg);
            while let Ok(ev) = self.net_events.try_recv() {
                self.state.handle_net(ev);
            }
            self.state.flush();
        }

        fn find(&self, tag: &str) -> NodeId {
            let doc = self.state.doc.as_ref().expect("document");
            doc.descendants(doc.root())
                .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == tag))
                .expect("element")
        }

        /// The first fragment of the element, or of one of its text nodes:
        /// inline elements have no box of their own, only their text does.
        fn rect_of(&self, id: NodeId) -> Rect {
            let doc = self.state.doc.as_ref().expect("document");
            let mut found = None;
            self.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
                let mine = f.node == Some(id) || f.node.is_some_and(|n| doc.parent(n) == Some(id));
                if mine && found.is_none() {
                    found = Some(f.rect);
                }
            });
            found.expect("fragment")
        }

        fn center(&self, tag: &str) -> (f32, f32) {
            let r = self.rect_of(self.find(tag));
            (r.x + r.width / 2.0, r.y + r.height / 2.0)
        }

        fn style(&self, tag: &str) -> Arc<browser_style::ComputedStyle> {
            self.state.styles[self.find(tag)].clone()
        }

        fn cursors(&self) -> Vec<Cursor> {
            self.messages
                .lock()
                .expect("lock")
                .iter()
                .filter_map(|m| match m {
                    TabToShell::Cursor(c) => Some(*c),
                    _ => None,
                })
                .collect()
        }
    }

    const PAGE: &str = "<!doctype html><style>body { margin: 0; color: black } \
        a { background-color: white } a:hover { background-color: red } \
        p:hover span { color: blue } a:active { color: green } a:focus { font-weight: bold } \
        div { height: 700px }</style>\
        <div></div><p><a href='x'>link</a> <span>text</span></p><p>second paragraph</p>";

    #[test]
    fn hover_restyles_element_and_subtree_and_sets_cursor() {
        let mut h = Harness::load(PAGE);
        assert_eq!(h.style("a").background_color.to_rgba8(), [255, 255, 255, 255]);
        assert_eq!(h.state.deps.hover, Reach::Subtree);

        let (x, y) = h.center("a");
        h.send(ShellToTab::MouseMove { x, y });
        assert_eq!(h.style("a").background_color.to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(h.style("span").color.to_rgba8(), [0, 0, 255, 255], "p:hover reaches the span");
        assert_eq!(h.cursors(), vec![Cursor::Pointer]);

        // Over the empty div: nothing hovered but the div chain.
        h.send(ShellToTab::MouseMove { x: 10.0, y: 10.0 });
        assert_eq!(h.style("a").background_color.to_rgba8(), [255, 255, 255, 255]);
        assert_eq!(h.style("span").color.to_rgba8(), [0, 0, 0, 255]);
        assert_eq!(h.cursors(), vec![Cursor::Pointer, Cursor::Default]);

        // Over plain text: text cursor. Leaving the window: default.
        let (x2, y2) = h.center("span");
        h.send(ShellToTab::MouseMove { x: x2, y: y2 });
        assert_eq!(h.cursors().last(), Some(&Cursor::Text));
        h.send(ShellToTab::MouseLeave);
        assert_eq!(h.cursors().last(), Some(&Cursor::Default));
        assert!(h.state.states.is_empty());
    }

    #[test]
    fn click_sets_active_then_focus_stays() {
        let mut h = Harness::load(PAGE);
        let (x, y) = h.center("a");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        assert_eq!(h.style("a").color.to_rgba8(), [0, 128, 0, 255], "active");
        assert_eq!(h.style("a").font_weight, 700, "focused");
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_ne!(h.style("a").color.to_rgba8(), [0, 128, 0, 255], "no longer active");
        assert_eq!(h.style("a").font_weight, 700, "still focused");

        // Clicking something unfocusable clears focus.
        let (px, py) = h.center("span");
        h.send(ShellToTab::MouseDown { x: px, y: py, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x: px, y: py, button: MouseButton::Left });
        assert_eq!(h.style("a").font_weight, 400);
        assert_eq!(h.state.focus, None);
    }

    #[test]
    fn scrolling_moves_what_is_under_the_pointer() {
        let mut h = Harness::load(PAGE);
        let (x, y) = h.center("a");
        // Point at where the link will be after scrolling down by 50px.
        h.send(ShellToTab::MouseMove { x, y: y - 50.0 });
        assert_eq!(h.style("a").background_color.to_rgba8(), [255, 255, 255, 255]);
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 50.0 });
        assert_eq!(h.style("a").background_color.to_rgba8(), [255, 0, 0, 255]);
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
