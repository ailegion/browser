//! Per-tab state: the current document, its resources, and the render
//! pipeline from DOM to scene.

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::{Document, HtmlParser, NodeId};
use browser_ipc_types::{Cursor, MouseButton, NetToTab, RequestId, ShellToTab, TabId, TabToShell, Viewport};
use browser_layout::{LayoutEngine, LayoutTree};
use browser_net::{CacheMode, FetchRequest, NetService, Sink};
use browser_paint::{ImageStore, PaintOptions, decode_image, paint};
use browser_style::media::MediaQueryList;
use browser_style::ua::ua_stylesheet;
use browser_style::{
    ElementStates, InteractionDeps, Origin, Reach, Rule, StateChange, StyleMap, Stylesheet, Stylist, compute_styles_with,
    restyle,
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

/// What a navigation does to the history list when it commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavKind {
    /// A new entry after the current one; later entries are dropped.
    Push,
    /// The current entry is loaded again and overwritten.
    Reload,
    /// Back or forward to the entry at the index.
    Traverse(usize),
}

/// A navigation in flight. Until the first response bytes arrive nothing
/// visible changes except the address the shell shows, so stopping or
/// failing it leaves the current page, URL and history untouched. On
/// commit the history is updated and the old document's fetches dropped;
/// the old document stays on screen until the new one has parsed.
struct PendingNav {
    url: Url,
    request: RequestId,
    kind: NavKind,
    committed: bool,
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

    /// The committed URL: what history holds for the current entry.
    url: Option<Url>,
    doc: Option<Document>,
    /// The URL `doc` was loaded from; differs from `url` while a later
    /// navigation has committed but not yet finished parsing.
    doc_url: Option<Url>,
    parser: Option<HtmlParser>,
    nav: Option<PendingNav>,
    next_request: u64,
    pending: HashMap<RequestId, Pending>,
    sheets: Vec<SheetSlot>,
    fetched_urls: std::collections::HashSet<Url>,
    images: ImageStore,
    /// How the current document's sub-resources use the HTTP cache: a
    /// reloaded document revalidates them, as browsers do.
    doc_cache: CacheMode,

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
    /// The link a button went down on; released on the same link, the
    /// primary button follows it and the middle button opens it in a new
    /// tab.
    press: Option<(NodeId, MouseButton)>,
    /// Last pointer position in viewport coordinates while over the page.
    mouse: Option<(f32, f32)>,
    /// The pointer or the scroll moved since hover was last evaluated.
    hover_dirty: bool,
    cursor: Cursor,

    history: Vec<Url>,
    history_index: usize,
    /// The URL's fragment must be scrolled to after the next layout.
    pending_fragment: bool,
    /// A `<meta http-equiv=refresh>` or `Refresh` header due at the instant.
    refresh: Option<(std::time::Instant, Url)>,
    /// `Refresh` header of the main response, applied when the document lands.
    refresh_header: Option<String>,

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
            doc_url: None,
            parser: None,
            nav: None,
            next_request: 1,
            pending: HashMap::new(),
            sheets: Vec::new(),
            fetched_urls: Default::default(),
            images: ImageStore::new(),
            doc_cache: CacheMode::Default,
            engine: LayoutEngine::new(),
            styles: StyleMap::new(),
            layout: None,
            scene: Scene::new(),
            states: ElementStates::default(),
            deps: InteractionDeps::default(),
            hover: None,
            focus: None,
            press: None,
            mouse: None,
            hover_dirty: false,
            cursor: Cursor::Default,
            history: Vec::new(),
            history_index: 0,
            pending_fragment: false,
            refresh: None,
            refresh_header: None,
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
        self.nav.is_some() || !self.pending.is_empty()
    }

    fn send_state(&mut self) {
        let title = self.doc.as_ref().and_then(|d| d.title());
        // The address bar shows where we are going as soon as we start.
        let url = self
            .nav
            .as_ref()
            .filter(|n| !n.committed)
            .map(|n| n.url.clone())
            .or_else(|| self.url.clone())
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
            ShellToTab::Navigate { url } => self.go(url, NavKind::Push),
            ShellToTab::Reload => {
                // Reloading while a navigation is pending restarts that one.
                let url = self.nav.as_ref().map(|n| n.url.clone()).or_else(|| self.url.clone());
                if let Some(url) = url {
                    self.go(url, NavKind::Reload);
                }
            }
            ShellToTab::Stop => self.stop(),
            ShellToTab::GoBack => {
                if self.history_index > 0 {
                    let i = self.history_index - 1;
                    self.go(self.history[i].clone(), NavKind::Traverse(i));
                }
            }
            ShellToTab::GoForward => {
                if self.history_index + 1 < self.history.len() {
                    let i = self.history_index + 1;
                    self.go(self.history[i].clone(), NavKind::Traverse(i));
                }
            }
            ShellToTab::Resize(vp) => {
                if vp != self.viewport {
                    self.viewport = vp;
                    // Media queries may change with the viewport.
                    self.needs_style = true;
                }
            }
            // Pointer moves and scrolls arrive in bursts; hover is
            // re-evaluated once per batch, in `flush`, at the final position.
            ShellToTab::Scroll { dx, dy } => {
                self.scroll_x += dx;
                self.scroll_y += dy;
                self.clamp_scroll();
                self.needs_paint = true;
                self.hover_dirty = true;
            }
            ShellToTab::MouseMove { x, y } => {
                self.mouse = Some((x, y));
                self.hover_dirty = true;
            }
            ShellToTab::MouseLeave => {
                self.mouse = None;
                self.hover_dirty = true;
            }
            ShellToTab::MouseDown { x, y, button } => {
                self.mouse = Some((x, y));
                self.update_hover();
                let hit = self.hover;
                if button == MouseButton::Left {
                    self.set_active(hit);
                    let focus = hit.and_then(|h| self.focusable_ancestor(h));
                    self.set_focus(focus);
                }
                if matches!(button, MouseButton::Left | MouseButton::Middle) {
                    self.press = hit.and_then(|h| self.ancestor_or_self(h, is_link)).map(|l| (l, button));
                }
            }
            ShellToTab::MouseUp { x, y, button } => {
                self.mouse = Some((x, y));
                self.update_hover();
                if button == MouseButton::Left {
                    self.set_active(None);
                }
                let released_on = self.hover.and_then(|h| self.ancestor_or_self(h, is_link));
                if let (Some((pressed, pressed_button)), Some(released)) = (self.press.take(), released_on)
                    && pressed == released
                    && pressed_button == button
                {
                    match button {
                        MouseButton::Left => self.follow_link(pressed),
                        MouseButton::Middle => self.open_link_in_new_tab(pressed),
                        _ => {}
                    }
                }
            }
            ShellToTab::Close => {}
        }
    }

    // ----- navigation -----

    /// The URL a link leads to, if it is one this browser opens.
    fn link_target(&self, link: NodeId) -> Option<Url> {
        let doc = self.doc.as_ref()?;
        let href = doc.element(link).and_then(|e| e.attr("href"))?;
        let Some(url) = doc.resolve_url(href) else {
            tracing::debug!(tab = self.id.0, "unresolvable href {href:?}");
            return None;
        };
        if !matches!(url.scheme(), "http" | "https" | "data" | "about") {
            // javascript:, mailto: and the rest are not ours to open.
            tracing::debug!(tab = self.id.0, "ignoring link to {url}");
            return None;
        }
        Some(url)
    }

    /// Follow a link the user clicked.
    fn follow_link(&mut self, link: NodeId) {
        if let Some(url) = self.link_target(link) {
            self.go(url, NavKind::Push);
        }
    }

    /// Ask the shell for a new tab on the link (middle click).
    fn open_link_in_new_tab(&mut self, link: NodeId) {
        if let Some(url) = self.link_target(link) {
            self.send(TabToShell::OpenInNewTab { url });
        }
    }

    /// Go to `url`. A change of fragment within the displayed document
    /// takes effect at once and only scrolls; anything else starts a load
    /// that commits when its first bytes arrive.
    fn go(&mut self, url: Url, kind: NavKind) {
        self.cancel_pending_nav();
        if kind != NavKind::Reload && self.is_same_document(&url) {
            tracing::info!(tab = self.id.0, "fragment navigation {url}");
            self.apply_history(url, kind);
            self.pending_fragment = true;
            self.state_dirty = true;
        } else {
            self.navigate(url, kind);
        }
    }

    /// Whether `url` names the displayed document, fragment aside.
    fn is_same_document(&self, url: &Url) -> bool {
        use url::Position;
        self.doc.is_some()
            && self.nav.is_none()
            && self
                .doc_url
                .as_ref()
                .is_some_and(|cur| cur[..Position::AfterQuery] == url[..Position::AfterQuery])
    }

    /// Record a committed navigation in the history list and adopt its URL.
    fn apply_history(&mut self, url: Url, kind: NavKind) {
        match kind {
            NavKind::Push => {
                if !self.history.is_empty() {
                    self.history.truncate(self.history_index + 1);
                }
                self.history.push(url.clone());
                self.history_index = self.history.len() - 1;
            }
            NavKind::Reload => match self.history.get_mut(self.history_index) {
                Some(entry) => *entry = url.clone(),
                None => {
                    self.history.push(url.clone());
                    self.history_index = self.history.len() - 1;
                }
            },
            NavKind::Traverse(i) => {
                if let Some(entry) = self.history.get_mut(i) {
                    // A redirect on the way back lands on the new URL.
                    *entry = url.clone();
                    self.history_index = i;
                }
            }
        }
        self.url = Some(url);
    }

    /// Drop a navigation in flight. Its late responses are ignored because
    /// its request is no longer pending.
    fn cancel_pending_nav(&mut self) {
        if let Some(nav) = self.nav.take() {
            tracing::debug!(tab = self.id.0, "navigation to {} abandoned", nav.url);
            self.pending.remove(&nav.request);
            if nav.committed {
                self.parser = None;
            }
        }
    }

    /// Stop loading. Before commit the old page is untouched; after it,
    /// what has arrived of the new one is shown.
    fn stop(&mut self) {
        let nav = self.nav.take();
        self.pending.clear();
        if let Some(nav) = nav
            && nav.committed
            && let Some(parser) = self.parser.take()
        {
            let doc = parser.finish();
            self.set_document(doc);
        }
        self.state_dirty = true;
    }

    /// The first bytes of the main response: the navigation is now real.
    fn commit(&mut self, id: RequestId, final_url: Url) {
        let Some(nav) = self.nav.as_mut() else { return };
        if nav.request != id {
            return;
        }
        nav.committed = true;
        let kind = nav.kind;
        tracing::info!(tab = self.id.0, "committed {final_url}");
        self.apply_history(final_url.clone(), kind);
        self.doc_cache = if kind == NavKind::Reload {
            CacheMode::NoCache
        } else {
            CacheMode::Default
        };
        // The old document's own fetches are moot now.
        self.pending.retain(|k, _| *k == id);
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.start_main_document(id, final_url);
        self.state_dirty = true;
    }

    /// Scroll to the element the URL's fragment names and make it `:target`.
    fn scroll_to_fragment(&mut self) {
        let Some(doc) = &self.doc else { return };
        let fragment = self.url.as_ref().and_then(|u| u.fragment()).map(percent_decode);
        // No fragment (back to the plain URL), an empty one, or "top" all
        // mean the top of the document.
        let target = match fragment.as_deref() {
            None | Some("") | Some("top") => None,
            Some(name) => doc.descendants(doc.root()).find(|&n| {
                doc.element(n).is_some_and(|e| {
                    e.id() == Some(name)
                        || (e.name.ns == ns!(html)
                            && e.name.local == local_name!("a")
                            && e.attr("name") == Some(name))
                })
            }),
        };
        if fragment.as_deref().is_some_and(|f| !f.is_empty() && f != "top") && target.is_none() {
            tracing::debug!(tab = self.id.0, "no element for fragment {fragment:?}");
            return;
        }
        let rect = target.and_then(|t| {
            self.layout
                .as_ref()?
                .first_rect(|n| n == t || doc.parent(n) == Some(t))
        });
        self.scroll_y = rect.map_or(0.0, |r| r.y);
        self.clamp_scroll();
        self.needs_paint = true;

        let changed = self.states.set_single(target, ElementStates::TARGET);
        let reach = self.deps.target;
        self.restyle_for(changed, reach);
    }

    /// The tab wants to be woken at this instant even if no event arrives.
    pub fn next_wake(&self) -> Option<std::time::Instant> {
        self.refresh.as_ref().map(|(at, _)| *at)
    }

    /// Run whatever timer is due.
    pub fn tick(&mut self) {
        if let Some((at, url)) = &self.refresh
            && std::time::Instant::now() >= *at
        {
            let url = url.clone();
            self.refresh = None;
            tracing::info!(tab = self.id.0, "refresh to {url}");
            // Refreshing to the same URL replaces the entry; to another
            // adds one, as browsers do.
            let same = self.url.as_ref() == Some(&url);
            self.go(url, if same { NavKind::Reload } else { NavKind::Push });
        }
    }

    /// Arm the declarative refresh of the document, if it has one.
    fn schedule_refresh(&mut self) {
        let Some(doc) = &self.doc else { return };
        let mut spec = self.refresh_header.take();
        if spec.is_none() {
            spec = doc.descendants(doc.root()).find_map(|n| {
                let e = doc.element(n)?;
                (e.name.ns == ns!(html)
                    && e.name.local == local_name!("meta")
                    && e.attr("http-equiv").is_some_and(|v| v.trim().eq_ignore_ascii_case("refresh")))
                .then(|| e.attr("content").unwrap_or("").to_owned())
            });
        }
        let Some(spec) = spec else { return };
        let Some((seconds, target)) = parse_refresh(&spec) else { return };
        let url = match target {
            Some(t) => match doc.resolve_url(&t) {
                Some(u) if matches!(u.scheme(), "http" | "https" | "data") => u,
                _ => return,
            },
            None => match &self.url {
                Some(u) => u.clone(),
                None => return,
            },
        };
        let at = std::time::Instant::now() + std::time::Duration::from_secs_f32(seconds.min(1.0e6));
        self.refresh = Some((at, url));
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
        self.hover_dirty = false;
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
        let started = std::time::Instant::now();
        let stylist = self.stylist();
        let vp = self.style_viewport();
        let result = restyle(
            doc,
            &stylist,
            &vp,
            &self.states,
            &mut self.styles,
            StateChange {
                changed: &changed,
                reach,
                subjects: &self.deps.subjects,
            },
        );
        tracing::debug!(
            tab = self.id.0,
            changed = changed.len(),
            ?reach,
            styled = result.styled,
            moved = result.moved,
            ms = started.elapsed().as_millis(),
            "restyle for interaction"
        );
        if result.moved {
            self.needs_layout = true;
        }
    }

    /// Start loading `url`. Nothing about the current page changes until
    /// the response commits (`commit`).
    fn navigate(&mut self, url: Url, kind: NavKind) {
        tracing::info!(tab = self.id.0, "navigate {url}");
        self.refresh = None;
        self.refresh_header = None;
        self.press = None;
        let mut request = FetchRequest::get(url.clone());
        if kind == NavKind::Reload {
            request.cache = CacheMode::NoCache;
        }
        let id = if url.scheme() == "about" {
            self.fetch_about(request.url)
        } else {
            self.fetch(request, PendingKind::Main)
        };
        self.nav = Some(PendingNav {
            url,
            request: id,
            kind,
            committed: false,
        });
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

    /// Internal pages. `about:blank` is an empty document; `about:crash`
    /// panics the tab thread on purpose, to exercise crash recovery.
    /// Answered through the net sink like a `data:` URL, so the load goes
    /// through the same commit path as any other.
    fn fetch_about(&mut self, url: Url) -> RequestId {
        let id = RequestId(self.next_request);
        self.next_request += 1;
        self.pending.insert(
            id,
            Pending {
                kind: PendingKind::Main,
                status: 0,
                headers: Vec::new(),
                body: Vec::new(),
            },
        );
        match url.path() {
            "blank" => {
                (self.net_sink)(NetToTab::ResponseStart {
                    id,
                    status: 200,
                    headers: vec![("content-type".to_owned(), "text/html".to_owned())],
                    final_url: url,
                });
                (self.net_sink)(NetToTab::ResponseEnd { id });
            }
            "crash" => panic!("about:crash: deliberate tab panic"),
            other => (self.net_sink)(NetToTab::Failed {
                id,
                error: format!("no such page: about:{other}"),
            }),
        }
        id
    }

    /// A `GET` for a sub-resource of the current document.
    fn subresource(&self, url: Url) -> FetchRequest {
        let mut request = FetchRequest::get(url);
        request.cache = self.doc_cache;
        request
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
                    self.refresh_header = p
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("refresh"))
                        .map(|(_, v)| v.clone());
                    self.commit(id, final_url);
                }
            }
            NetToTab::ResponseChunk { id, bytes } => {
                let Some(p) = self.pending.get_mut(&id) else { return };
                let is_current_main = matches!(p.kind, PendingKind::Main)
                    && self.nav.as_ref().is_some_and(|n| n.request == id);
                if is_current_main && let Some(parser) = &mut self.parser {
                    parser.feed(&bytes);
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
                if matches!(p.kind, PendingKind::Main)
                    && let Some(nav) = self.nav.take_if(|n| n.request == id)
                {
                    // The error page takes the entry the page would have.
                    if !nav.committed {
                        self.apply_history(nav.url.clone(), nav.kind);
                        self.pending.clear();
                    }
                    self.show_error_page(nav.url.as_str(), &error);
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

    fn finish_response(&mut self, id: RequestId, p: Pending) {
        match p.kind {
            PendingKind::Main => {
                if self.nav.take_if(|n| n.request == id).is_none() {
                    return;
                }
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
        self.fetched_urls.clear();
        self.images = ImageStore::new();
        self.doc = Some(doc);
        self.doc_url = self.url.clone();
        // Node ids of the old document mean nothing now. The pointer
        // position is kept: hover is re-evaluated after the first layout.
        self.states = ElementStates::default();
        self.hover = None;
        self.focus = None;
        self.press = None;
        self.collect_stylesheets();
        self.collect_images();
        self.pending_fragment = self.url.as_ref().is_some_and(|u| u.fragment().is_some());
        self.schedule_refresh();
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
                    let request = self.subresource(url);
                    self.fetch(request, PendingKind::Stylesheet { slot, depth: 0 });
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
            let request = self.subresource(url);
            self.fetch(
                request,
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
        let request = self.subresource(url.clone());
        self.fetch(request, PendingKind::Image { url });
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
            if self.hover_dirty && !self.needs_style && !self.needs_layout {
                self.update_hover();
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
            if self.pending_fragment && self.layout.is_some() {
                self.pending_fragment = false;
                self.scroll_to_fragment();
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

/// Parse a `Refresh` value (`5`, `5; url=/next`, `0;URL='x'`) into the
/// delay in seconds and the target, per the HTML standard's shared
/// declarative refresh steps. `None` when the value is not a refresh.
fn parse_refresh(spec: &str) -> Option<(f32, Option<String>)> {
    let s = spec.trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let rest = &s[digits.len()..];
    let seconds: f32 = if digits.is_empty() {
        // "; url=..." with no time counts as zero only if a separator follows.
        if !rest.trim_start().starts_with([';', ',']) {
            return None;
        }
        0.0
    } else {
        digits.parse().unwrap_or(0.0)
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix([';', ',']).unwrap_or(rest).trim_start();
    if rest.is_empty() {
        return Some((seconds, None));
    }
    let rest = match rest.get(..3) {
        Some(p) if p.eq_ignore_ascii_case("url") => {
            let after = rest[3..].trim_start();
            match after.strip_prefix('=') {
                Some(a) => a.trim_start(),
                // "url" without "=" is itself the URL, per the standard.
                None => rest,
            }
        }
        _ => rest,
    };
    let url = match rest.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &rest[1..];
            inner.split(q).next().unwrap_or(inner)
        }
        _ => rest.trim_end(),
    };
    Some((seconds, Some(url.to_owned())))
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    fn data_url(html: &str, fragment: Option<&str>) -> Url {
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
        let frag = fragment.map(|f| format!("#{f}")).unwrap_or_default();
        Url::parse(&format!("data:text/html,{encoded}{frag}")).expect("data url")
    }

    impl Harness {
        fn load(html: &str) -> Self {
            Self::load_url(data_url(html, None))
        }

        fn load_url(url: Url) -> Self {
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
            h.send(ShellToTab::Navigate { url });
            h
        }

        fn send(&mut self, msg: ShellToTab) {
            self.state.handle_shell(msg);
            self.pump();
        }

        /// Deliver what the net answered and do the work due, as the tab
        /// loop does after each batch.
        fn pump(&mut self) {
            // A navigation started by a timer or a click queues more net
            // events; keep going until a round delivers none.
            loop {
                let mut delivered = false;
                while let Ok(ev) = self.net_events.try_recv() {
                    delivered = true;
                    self.state.handle_net(ev);
                }
                self.state.tick();
                self.state.flush();
                if !delivered {
                    break;
                }
            }
        }

        fn click(&mut self, tag: &str) {
            let (x, y) = self.center(tag);
            self.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
            self.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        }

        fn title(&self) -> Option<String> {
            self.state.doc.as_ref().and_then(|d| d.title())
        }

        /// The last (url, loading, can_go_back, can_go_forward) reported.
        fn last_state(&self) -> Option<(Url, bool, bool, bool)> {
            self.messages
                .lock()
                .expect("lock")
                .iter()
                .rev()
                .find_map(|m| match m {
                    TabToShell::StateChanged {
                        url,
                        loading,
                        can_go_back,
                        can_go_forward,
                        ..
                    } => Some((url.clone(), *loading, *can_go_back, *can_go_forward)),
                    _ => None,
                })
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

    #[test]
    fn a_burst_of_pointer_moves_restyles_once_at_the_final_position() {
        let mut h = Harness::load(PAGE);
        let (x, y) = h.center("a");
        // Over the link, off it, over it again, then off: no flush between.
        h.state.handle_shell(ShellToTab::MouseMove { x, y });
        h.state.handle_shell(ShellToTab::MouseMove { x: 10.0, y: 10.0 });
        h.state.handle_shell(ShellToTab::MouseMove { x, y });
        h.state.handle_shell(ShellToTab::MouseMove { x: 10.0, y: 10.0 });
        assert!(h.state.hover_dirty);
        assert!(h.cursors().is_empty(), "nothing evaluated before the batch ends");
        h.pump();
        assert!(!h.state.hover_dirty);
        assert!(h.cursors().is_empty(), "the final position is not over the link");
        assert_eq!(h.style("a").background_color.to_rgba8(), [255, 255, 255, 255]);
        // A click resolves hover at once, since it acts on it.
        h.state.handle_shell(ShellToTab::MouseMove { x, y });
        h.state.handle_shell(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        assert_eq!(h.style("a").color.to_rgba8(), [0, 128, 0, 255], ":active applied before the batch ends");
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

    /// A second page a link or refresh can lead to.
    fn second_page_href() -> String {
        data_url("<title>Second</title><p>second page</p>", None).to_string()
    }

    #[test]
    fn clicking_a_link_loads_it_and_records_history() {
        let mut h = Harness::load(&format!("<title>First</title><p><a href='{}'>go</a></p>", second_page_href()));
        assert_eq!(h.title().as_deref(), Some("First"));
        h.click("a");
        assert_eq!(h.title().as_deref(), Some("Second"));
        assert_eq!(h.state.history.len(), 2);
        assert_eq!(h.state.history_index, 1);

        h.send(ShellToTab::GoBack);
        assert_eq!(h.title().as_deref(), Some("First"));
        assert_eq!(h.state.history_index, 0);

        // A press and release on different links is not a click.
        let (x, y) = h.center("a");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x: 5.0, y: 5.0, button: MouseButton::Left });
        assert_eq!(h.title().as_deref(), Some("First"));
        assert_eq!(h.state.history.len(), 2);
    }

    #[test]
    fn unsupported_link_schemes_are_ignored() {
        let mut h = Harness::load("<title>First</title><p><a href='javascript:alert(1)'>js</a> <a href='mailto:x@y'>m</a></p>");
        h.click("a");
        assert_eq!(h.title().as_deref(), Some("First"));
        assert_eq!(h.state.history.len(), 1);
    }

    #[test]
    fn middle_click_asks_the_shell_for_a_new_tab() {
        let mut h = Harness::load(&format!("<title>First</title><p><a href='{}'>go</a></p>", second_page_href()));
        let (x, y) = h.center("a");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Middle });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Middle });
        // This tab stays where it is; the shell gets the request.
        assert_eq!(h.title().as_deref(), Some("First"));
        assert_eq!(h.state.history.len(), 1);
        let opened: Vec<Url> = h
            .messages
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|m| match m {
                TabToShell::OpenInNewTab { url } => Some(url.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1);
        assert!(opened[0].as_str().starts_with("data:"));
        // Middle down on the link, left up on it: not a click of either kind.
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Middle });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(h.title().as_deref(), Some("First"));
        assert_eq!(h.messages.lock().expect("lock").iter().filter(|m| matches!(m, TabToShell::OpenInNewTab { .. })).count(), 1);
    }

    #[test]
    fn about_blank_is_an_empty_document_with_a_history_entry() {
        let mut h = Harness::load("<title>First</title>");
        h.send(ShellToTab::Navigate {
            url: Url::parse("about:blank").expect("url"),
        });
        assert_eq!(h.title(), None);
        assert!(h.state.doc.is_some());
        assert_eq!(h.state.url.as_ref().map(|u| u.as_str()), Some("about:blank"));
        assert_eq!(h.state.history.len(), 2);
        assert_eq!(h.last_state().map(|s| s.1), Some(false), "not loading");
        h.send(ShellToTab::GoBack);
        assert_eq!(h.title().as_deref(), Some("First"));
        // An unknown about: page is an error page, like a failed load.
        h.send(ShellToTab::Navigate {
            url: Url::parse("about:nothing").expect("url"),
        });
        assert_eq!(h.title().as_deref(), Some("Cannot load page"));
    }

    #[test]
    #[should_panic(expected = "about:crash")]
    fn about_crash_panics_the_tab() {
        let mut h = Harness::load("<title>First</title>");
        h.send(ShellToTab::Navigate {
            url: Url::parse("about:crash").expect("url"),
        });
    }

    const TALL: &str = "<title>Tall</title><style>body { margin: 0 } div { height: 1500px } h2:target { color: red }</style>\
        <p><a href='#end'>down</a> <a href='#top'>top</a></p><div></div><h2 id=end>The end</h2>";

    #[test]
    fn fragment_links_scroll_without_reloading_and_set_target() {
        let mut h = Harness::load(TALL);
        let nodes_before = h.state.doc.as_ref().map(|d| d.node_count());
        h.click("a");
        // The heading sits past 1500px; the scroll is clamped to the
        // content height minus the 600px viewport.
        assert!(h.state.scroll_y > 900.0, "scrolled to the heading, got {}", h.state.scroll_y);
        assert_eq!(h.state.url.as_ref().and_then(|u| u.fragment()), Some("end"));
        assert_eq!(h.state.history.len(), 2);
        assert_eq!(h.state.doc.as_ref().map(|d| d.node_count()), nodes_before, "same document");
        assert_eq!(h.style("h2").color.to_rgba8(), [255, 0, 0, 255], ":target applies");

        // Back returns to the top of the same document; the target is gone.
        h.send(ShellToTab::GoBack);
        assert_eq!(h.state.scroll_y, 0.0);
        assert_eq!(h.style("h2").color.to_rgba8(), [0, 0, 0, 255]);
        assert_eq!(h.state.doc.as_ref().map(|d| d.node_count()), nodes_before);
    }

    #[test]
    fn url_fragment_is_scrolled_to_after_load() {
        let h = Harness::load_url(data_url(TALL, Some("end")));
        assert!(h.state.scroll_y > 900.0, "got {}", h.state.scroll_y);
        assert_eq!(h.style("h2").color.to_rgba8(), [255, 0, 0, 255]);
        let h = Harness::load_url(data_url(TALL, Some("nowhere")));
        assert_eq!(h.state.scroll_y, 0.0);
    }

    #[test]
    fn meta_refresh_navigates_when_due() {
        let h = Harness::load(&format!(
            "<title>First</title><meta http-equiv=Refresh content=\"0; URL='{}'\"><p>x</p>",
            second_page_href()
        ));
        // `load` already pumped once, right after the document landed; the
        // refresh was due immediately, so it has already happened.
        assert_eq!(h.title().as_deref(), Some("Second"));
        assert_eq!(h.state.history.len(), 2);
        assert!(h.state.next_wake().is_none());

        // A refresh with a delay waits for it.
        let mut h = Harness::load("<title>Wait</title><meta http-equiv=refresh content='30'>");
        assert!(h.state.next_wake().is_some_and(|t| t > std::time::Instant::now()));
        h.pump();
        assert_eq!(h.title().as_deref(), Some("Wait"));
        h.send(ShellToTab::Navigate { url: data_url("<title>Away</title>", None) });
        assert!(h.state.next_wake().is_none(), "navigating away cancels the refresh");
    }

    fn page(title: &str) -> Url {
        data_url(&format!("<title>{title}</title><p>{title}</p>"), None)
    }

    #[test]
    fn history_back_forward_reload_and_truncation() {
        let mut h = Harness::load_url(page("One"));
        h.send(ShellToTab::Navigate { url: page("Two") });
        h.send(ShellToTab::Navigate { url: page("Three") });
        assert_eq!(h.title().as_deref(), Some("Three"));
        assert_eq!((h.state.history.len(), h.state.history_index), (3, 2));
        assert_eq!(h.last_state().map(|s| (s.2, s.3)), Some((true, false)));

        h.send(ShellToTab::GoBack);
        assert_eq!(h.title().as_deref(), Some("Two"));
        assert_eq!(h.state.history_index, 1);
        assert_eq!(h.last_state().map(|s| (s.2, s.3)), Some((true, true)));
        h.send(ShellToTab::GoBack);
        assert_eq!(h.title().as_deref(), Some("One"));
        h.send(ShellToTab::GoBack);
        assert_eq!(h.title().as_deref(), Some("One"), "nothing before the first entry");
        h.send(ShellToTab::GoForward);
        assert_eq!(h.title().as_deref(), Some("Two"));

        // Navigating from the middle drops the entries after it.
        h.send(ShellToTab::Navigate { url: page("Four") });
        assert_eq!(h.title().as_deref(), Some("Four"));
        assert_eq!((h.state.history.len(), h.state.history_index), (3, 2));
        assert_eq!(h.state.history[1], page("Two"));

        h.send(ShellToTab::Reload);
        assert_eq!(h.title().as_deref(), Some("Four"));
        assert_eq!((h.state.history.len(), h.state.history_index), (3, 2));
        assert_eq!(h.last_state().map(|s| s.1), Some(false), "not loading");
    }

    #[test]
    fn nothing_changes_until_the_response_commits() {
        let mut h = Harness::load_url(page("One"));
        // Start a load but withhold its response.
        h.state.handle_shell(ShellToTab::Navigate { url: page("Two") });
        h.state.flush();
        assert_eq!(h.title().as_deref(), Some("One"));
        assert_eq!(h.state.url, Some(page("One")));
        assert_eq!(h.state.history.len(), 1);
        assert_eq!(h.last_state().map(|s| (s.0, s.1)), Some((page("Two"), true)), "address shows the target");

        // Stop before commit: as if nothing happened, and the withheld
        // response is ignored when it does arrive.
        h.send(ShellToTab::Stop);
        assert_eq!(h.title().as_deref(), Some("One"));
        assert_eq!(h.state.url, Some(page("One")));
        assert_eq!(h.state.history.len(), 1);
        assert_eq!(h.last_state().map(|s| (s.0, s.1)), Some((page("One"), false)));

        // Superseded before commit: only the later one lands.
        h.state.handle_shell(ShellToTab::Navigate { url: page("Two") });
        h.send(ShellToTab::Navigate { url: page("Three") });
        assert_eq!(h.title().as_deref(), Some("Three"));
        assert_eq!(h.state.history.len(), 2);
    }

    #[test]
    fn failed_load_shows_an_error_page_in_its_own_entry() {
        let mut h = Harness::load_url(page("One"));
        let bad = Url::parse("data:text/html;base64,@@@").expect("url");
        h.send(ShellToTab::Navigate { url: bad.clone() });
        assert_eq!(h.title().as_deref(), Some("Cannot load page"));
        assert_eq!(h.state.url, Some(bad));
        assert_eq!((h.state.history.len(), h.state.history_index), (2, 1));
        h.send(ShellToTab::GoBack);
        assert_eq!(h.title().as_deref(), Some("One"));
    }

    #[test]
    fn refresh_values_parse() {
        assert_eq!(parse_refresh("5"), Some((5.0, None)));
        assert_eq!(parse_refresh(" 5 ; url=/next "), Some((5.0, Some("/next".into()))));
        assert_eq!(parse_refresh("0;URL='a b'"), Some((0.0, Some("a b".into()))));
        assert_eq!(parse_refresh("2,url=\"x\"y"), Some((2.0, Some("x".into()))));
        assert_eq!(parse_refresh("3; https://e.com/"), Some((3.0, Some("https://e.com/".into()))));
        assert_eq!(parse_refresh("1.5;url"), Some((1.5, Some("url".into()))));
        assert_eq!(parse_refresh("; url=x"), Some((0.0, Some("x".into()))));
        assert_eq!(parse_refresh("nonsense"), None);
        assert_eq!(parse_refresh(""), None);
        assert_eq!(percent_decode("a%20b%zz%"), "a b%zz%");
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

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
