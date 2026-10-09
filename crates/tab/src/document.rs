//! Per-tab state: the current document, its resources, and the render
//! pipeline from DOM to scene.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use browser_dom::{Document, HtmlParser, NodeId};
use browser_ipc_types::{Cursor, Key, MouseButton, NetToTab, RequestId, ShellToTab, TabId, TabToShell, Viewport};
use browser_layout::selection::{self, TextPos};
use browser_layout::{LayoutEngine, LayoutTree, Rect, SelectionRanges};
use browser_chrome::scrollbar::{Scrollbar, ScrollbarHit};
use browser_net::{CacheMode, FetchRequest, NetService, Sink};
use browser_paint::{ImageStore, PaintOptions, decode_image, paint};
use browser_script::{ConsoleLevel, ConsoleLine, EventTargetRef, HostRequest, ModuleProgress, ModuleScript, ScriptHost};
use browser_style::media::MediaQueryList;
use browser_style::ua::ua_stylesheet;
use browser_style::{
    ElementStates, InteractionDeps, Origin, Reach, Rule, StateChange, StateKind, StyleMap, Stylesheet, Stylist,
    compute_styles_with, restyle,
};
use html5ever::{local_name, ns};
use url::Url;
use vello::Scene;

use crate::{OutputSink, TabOutput};

mod user_events;

use user_events::{Modifiers, button_bit};

/// Nested `@import` depth allowed.
const MAX_IMPORT_DEPTH: u8 = 6;

enum PendingKind {
    Main,
    Stylesheet {
        /// Every slot in `sheets` with this URL and no sheet yet takes the
        /// response.
        url: Url,
        depth: u8,
    },
    Image {
        url: Url,
    },
    /// An external script element's source; `id` is its `ScriptLoad`.
    Script {
        id: u64,
    },
    /// A module another module imports, for the document's module map.
    ModuleSource {
        url: Url,
    },
}

/// Classic or module, per the `<script type>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
    Classic,
    Module,
}

/// Where a script element's source stands.
enum LoadState {
    /// On its way from the network.
    Fetching,
    /// A classic script ready to run.
    Classic(String),
    /// A module, loading its imports until `ModuleScript::is_ready`.
    Module(ModuleScript),
    /// Cannot run; the message goes to the console in its turn.
    Failed(String),
}

/// A script element that is loading or waiting for its turn to run.
struct ScriptLoad {
    id: u64,
    kind: ScriptKind,
    /// The `src`, for external scripts.
    url: Option<Url>,
    state: LoadState,
}

impl ScriptLoad {
    /// Nothing more to wait for: it runs (or reports) when its turn comes.
    fn is_ready(&self) -> bool {
        match &self.state {
            LoadState::Fetching => false,
            LoadState::Module(m) => m.is_ready(),
            LoadState::Classic(_) | LoadState::Failed(_) => true,
        }
    }
}

/// The document's script loads, in the HTML standard's three groups.
#[derive(Default)]
struct Scripts {
    next_id: u64,
    /// The pending parsing-blocking script: the parser waits for it.
    blocking: Option<ScriptLoad>,
    /// Scripts that run once the document has finished parsing, in order
    /// (`defer`, and modules without `async`).
    deferred: VecDeque<ScriptLoad>,
    /// `async` scripts: each runs as soon as it is ready.
    asap: Vec<ScriptLoad>,
    /// Module URLs fetched for the module map, so each is fetched once.
    module_urls: HashSet<Url>,
    /// A deferred script is running: the document is still `interactive`.
    running_deferred: bool,
}

impl Scripts {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut ScriptLoad> {
        self.blocking
            .iter_mut()
            .chain(self.deferred.iter_mut())
            .chain(self.asap.iter_mut())
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut ScriptLoad> {
        self.iter_mut().find(|l| l.id == id)
    }

    /// Drop every waiting script (the load was stopped).
    fn clear_loads(&mut self) {
        self.blocking = None;
        self.deferred.clear();
        self.asap.clear();
    }
}

/// What "prepare the script element" decided for a parser-inserted script.
enum Prepared {
    /// An inline classic script: run it now, then parse on.
    Run(String),
    /// An external script the parser must wait for.
    Blocking,
    /// Queued, skipped, or already handled: parse on.
    Done,
}

struct Pending {
    kind: PendingKind,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// The stages of a document's load, as `document.readyState` and the
/// lifecycle events see them: `loading` while parsing, `interactive`
/// once parsed (deferred scripts run, then `DOMContentLoaded` fires),
/// `complete` once nothing is still being fetched (`load` fires).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Lifecycle {
    #[default]
    Loading,
    Interactive,
    /// `DOMContentLoaded` has fired; still `interactive`.
    ContentLoaded,
    Complete,
}

/// What a navigation does to the history list when it commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavKind {
    /// A new entry after the current one; later entries are dropped.
    Push,
    /// The current entry is loaded again and overwritten.
    Reload,
    /// The current entry is overwritten by another URL
    /// (`location.replace`); unlike a reload the cache is used as usual.
    Replace,
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

/// Find in page: the query, its matches in tree order, and what to
/// highlight, kept ready for every repaint.
struct Find {
    query: String,
    matches: Vec<(TextPos, TextPos)>,
    /// Index into `matches`.
    current: Option<usize>,
    ranges: SelectionRanges,
    current_ranges: SelectionRanges,
}

/// One author stylesheet in cascade order, possibly still loading.
struct SheetSlot {
    /// The `<style>` or `<link>` element the sheet comes from; an
    /// `@import`'s slot carries its importer's, so they go together.
    source: NodeId,
    url: Option<Url>,
    media: MediaQueryList,
    /// Hash of a `<style>`'s text, so an edit by script is noticed.
    text_hash: u64,
    sheet: Option<Arc<Stylesheet>>,
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
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
    /// External sheets already parsed, by URL, so a script that adds a
    /// `<link>` or `<style>` again does not fetch again.
    loaded_sheets: std::collections::HashMap<Url, Arc<Stylesheet>>,
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
    /// Where the focus ring is drawn: the focused element's boxes, when
    /// focus came from the keyboard. Kept up to date with focus and layout
    /// so a repaint does not walk the tree.
    focus_ring: Vec<Rect>,
    /// Byte offset of the caret in the focused text control's value.
    caret: usize,
    /// Where the caret is drawn, kept up to date like `focus_ring`.
    caret_rect: Option<Rect>,
    /// The checkbox or radio a primary button went down on; released on
    /// the same one, it toggles.
    press_control: Option<NodeId>,
    /// The link a button went down on; released on the same link, the
    /// primary button follows it and the middle button opens it in a new
    /// tab.
    press: Option<(NodeId, MouseButton)>,
    /// Last pointer position in viewport coordinates while over the page.
    mouse: Option<(f32, f32)>,
    /// The pointer or the scroll moved since hover was last evaluated.
    hover_dirty: bool,
    /// The modifier keys held, for the events' `ctrlKey` and friends.
    modifiers: Modifiers,
    /// The mouse buttons held, as `MouseEvent.buttons` bits.
    buttons: u16,
    /// The element a button went down on, for `click`: it goes to the
    /// nearest element the press and the release share.
    press_target: Option<NodeId>,
    /// The click count of the last primary press, for `detail`.
    press_clicks: u32,
    /// The element capturing the mouse (`setPointerCapture`): pointer
    /// and mouse events go to it until the button goes up.
    pointer_capture: Option<NodeId>,
    /// A `pointerdown` was cancelled: `mousedown`, `mousemove` and
    /// `mouseup` are held back until the pointer goes up.
    compat_suppressed: bool,
    /// The pointer moved in this batch: `flush` fires one `pointermove`
    /// and one `mousemove` at its final position.
    pointer_moved: bool,
    /// Where the last move event was fired, for `movementX/Y`.
    last_move: Option<(f32, f32)>,
    /// A button or checkbox Space went down on: it clicks on release.
    space_armed: Option<NodeId>,
    /// A focused text control's value when it got focus; `change` fires
    /// on blur if it differs.
    focus_value: Option<String>,
    /// The viewport size and scroll offset `resize` and `scroll` were
    /// last fired for.
    viewport_reported: (f32, f32),
    scroll_reported: (f32, f32),
    /// The scrollbar thumb is being dragged: where in the thumb it was
    /// grabbed, in logical pixels from its top.
    scroll_drag: Option<f32>,
    cursor: Cursor,

    /// The text selection as anchor (where the drag began) and focus (where
    /// it is now), in either order.
    selection: Option<(TextPos, TextPos)>,
    /// A primary-button drag is extending the selection from this anchor.
    select_anchor: Option<TextPos>,
    /// The last primary press: when, where, and how many in a row, for
    /// double and triple clicks.
    last_click: Option<(std::time::Instant, f32, f32, u32)>,
    /// While a selection drag holds the pointer outside the viewport, the
    /// page scrolls a step at each of these instants.
    autoscroll: Option<std::time::Instant>,
    /// Find in page, while the find bar is open with a query.
    find: Option<Find>,

    /// The document's JavaScript context and queues; a new one per
    /// document, made when the document commits so scripts can run while
    /// it parses.
    script: Option<ScriptHost>,
    /// The document's script elements that are loading or waiting.
    scripts: Scripts,
    /// The main response has ended; the document is finished as soon as
    /// the parser is no longer waiting for a script.
    main_done: bool,
    /// How far the document's load has got, for `document.readyState`
    /// and the events along the way (`update_readiness`).
    lifecycle: Lifecycle,
    /// When the next animation frame is due, while callbacks wait for one.
    frame_due: Option<std::time::Instant>,
    /// The document's time origin, for animation frame timestamps.
    doc_started: std::time::Instant,
    /// Recent console output, for tests and tools; the log gets it too.
    console: Vec<ConsoleLine>,

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
            loaded_sheets: Default::default(),
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
            focus_ring: Vec::new(),
            caret: 0,
            caret_rect: None,
            press_control: None,
            press: None,
            mouse: None,
            hover_dirty: false,
            modifiers: Modifiers::default(),
            buttons: 0,
            press_target: None,
            press_clicks: 1,
            pointer_capture: None,
            compat_suppressed: false,
            pointer_moved: false,
            last_move: None,
            space_armed: None,
            focus_value: None,
            viewport_reported: (viewport.width, viewport.height),
            scroll_reported: (0.0, 0.0),
            scroll_drag: None,
            cursor: Cursor::Default,
            selection: None,
            select_anchor: None,
            last_click: None,
            autoscroll: None,
            find: None,
            script: None,
            scripts: Scripts::default(),
            main_done: false,
            lifecycle: Lifecycle::Loading,
            frame_due: None,
            doc_started: std::time::Instant::now(),
            console: Vec::new(),
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
                // `wheel` first: a listener may take the scroll.
                if !self.fire_wheel(dx, dy) {
                    return;
                }
                self.scroll_x += dx;
                self.scroll_y += dy;
                self.clamp_scroll();
                self.needs_paint = true;
                self.hover_dirty = true;
            }
            ShellToTab::MouseMove { x, y } => {
                self.mouse = Some((x, y));
                if let Some(grab) = self.scroll_drag {
                    if let Some(sb) = self.scrollbar() {
                        self.scroll_y = sb.offset_for_thumb_top(y - grab);
                        self.clamp_scroll();
                        self.needs_paint = true;
                    }
                } else {
                    self.hover_dirty = true;
                    self.pointer_moved = true;
                }
                if self.select_anchor.is_some() {
                    self.extend_selection_to(x, y);
                    // Outside the viewport the page scrolls toward the
                    // pointer until it comes back.
                    let outside = y < 0.0 || y > self.viewport.height;
                    if outside && self.autoscroll.is_none() {
                        self.autoscroll = Some(std::time::Instant::now());
                    } else if !outside {
                        self.autoscroll = None;
                    }
                }
            }
            ShellToTab::MouseLeave => {
                self.mouse = None;
                self.last_move = None;
                self.hover_dirty = true;
                self.autoscroll = None;
            }
            ShellToTab::MouseDown { x, y, button } => {
                self.mouse = Some((x, y));
                if button == MouseButton::Left
                    && let Some(sb) = self.scrollbar()
                    && let Some(hit) = sb.hit(x, y)
                {
                    // The scrollbar is above the page: no click reaches it.
                    match hit {
                        ScrollbarHit::Thumb => self.scroll_drag = Some(y - sb.thumb().y0 as f32),
                        ScrollbarHit::Before => self.scroll_y -= self.viewport.height * 0.9,
                        ScrollbarHit::After => self.scroll_y += self.viewport.height * 0.9,
                    }
                    self.clamp_scroll();
                    self.needs_paint = true;
                    return;
                }
                self.update_hover();
                self.process_pointer_capture();
                let hit = self.hover;
                self.buttons |= button_bit(button);
                if let Some(host) = &mut self.script {
                    host.set_pointer_buttons(self.buttons);
                }
                let clicks = if button == MouseButton::Left { self.count_click(x, y) } else { 1 };
                self.press_clicks = clicks;
                self.press_target = hit;
                // `pointerdown` first; cancelled, it holds back the mouse
                // events until the pointer goes up and starts no
                // selection, but focus still moves. `mousedown` cancelled:
                // no focus change, no caret, no selection. `:active`, the
                // press and `click` happen either way.
                let pointer_ok = self.fire_pointer("pointerdown", hit, x, y, button);
                if !pointer_ok {
                    self.compat_suppressed = true;
                }
                let mouse_ok = if self.compat_suppressed {
                    true
                } else {
                    self.fire_mouse("mousedown", self.hover, x, y, button, clicks)
                };
                let hit = self.hover;
                if button == MouseButton::Left {
                    self.set_active(hit);
                    if mouse_ok {
                        let focus = hit.and_then(|h| self.focusable_ancestor(h));
                        self.set_focus(focus, false);
                    }
                    self.press_control = hit.and_then(|h| self.ancestor_or_self(h, is_toggle));
                    if mouse_ok && pointer_ok {
                        self.place_caret_at(x, y);
                    }
                }
                if matches!(button, MouseButton::Left | MouseButton::Middle) {
                    self.press = hit.and_then(|h| self.ancestor_or_self(h, is_link)).map(|l| (l, button));
                }
                if button == MouseButton::Left && mouse_ok && pointer_ok {
                    let on_link = self.press.is_some();
                    self.begin_selection(x, y, clicks, on_link);
                }
            }
            ShellToTab::MouseUp { x, y, button } => {
                self.mouse = Some((x, y));
                if button == MouseButton::Left {
                    self.select_anchor = None;
                    self.autoscroll = None;
                }
                self.buttons &= !button_bit(button);
                if self.scroll_drag.take().is_some() {
                    self.needs_paint = true;
                    self.hover_dirty = true;
                    return;
                }
                self.update_hover();
                self.process_pointer_capture();
                let clicks = self.press_clicks;
                self.fire_pointer("pointerup", self.hover, x, y, button);
                if !self.compat_suppressed {
                    self.fire_mouse("mouseup", self.hover, x, y, button, clicks);
                }
                if let Some(host) = &mut self.script {
                    host.set_pointer_buttons(self.buttons);
                }
                // The pointer is up: a capture is released, and the mouse
                // events a cancelled `pointerdown` held back come again.
                self.release_pointer_capture();
                self.compat_suppressed = false;
                // `click` (`auxclick` for the other buttons) goes to the
                // nearest element the press and the release share;
                // cancelled, nothing is toggled or followed. `dblclick`
                // and `contextmenu` have no default action here.
                let pressed_on = self.press_target.take();
                let click_target = self.common_ancestor(pressed_on, self.hover);
                let proceed = match button {
                    MouseButton::Left => self.fire_click("click", click_target, x, y, button, clicks),
                    _ => self.fire_click("auxclick", click_target, x, y, button, clicks),
                };
                if button == MouseButton::Right {
                    self.fire_click("contextmenu", self.hover, x, y, button, 0);
                }
                if button == MouseButton::Left && clicks == 2 {
                    self.fire_mouse("dblclick", click_target, x, y, button, 2);
                }
                if button == MouseButton::Left {
                    self.set_active(None);
                    let released_control = self.hover.and_then(|h| self.ancestor_or_self(h, is_toggle));
                    if let Some(c) = self.press_control.take()
                        && released_control == Some(c)
                        && proceed
                    {
                        self.toggle_control(c);
                    }
                }
                let released_on = self.hover.and_then(|h| self.ancestor_or_self(h, is_link));
                if let (Some((pressed, pressed_button)), Some(released)) = (self.press.take(), released_on)
                    && pressed == released
                    && pressed_button == button
                    && proceed
                {
                    match button {
                        MouseButton::Left => self.follow_link(pressed),
                        MouseButton::Middle => self.open_link_in_new_tab(pressed),
                        _ => {}
                    }
                }
            }
            ShellToTab::SelectAll => {
                let extent = self.layout.as_ref().and_then(selection::text_extent);
                self.set_selection(extent);
            }
            ShellToTab::Copy => {
                if let Some(text) = self.selected_text()
                    && !text.is_empty()
                {
                    self.send(TabToShell::CopyText { text });
                }
            }
            ShellToTab::Find { query } => self.find(query),
            ShellToTab::FindNext { forward } => self.find_next(forward),
            ShellToTab::FindClose => {
                if self.find.take().is_some() {
                    self.needs_paint = true;
                }
            }
            ShellToTab::Key {
                key,
                code,
                repeat,
                shift,
                ctrl,
                alt,
            } => self.key(key, &code, repeat, shift, ctrl, alt),
            ShellToTab::KeyUp {
                key,
                code,
                shift,
                ctrl,
                alt,
            } => {
                let proceed = self.fire_key("keyup", &key, &code, false, shift, ctrl, alt);
                // Space clicks a button or checkbox on release, as
                // browsers do, if focus is still where it went down.
                if key == Key::Space
                    && let Some(f) = self.space_armed.take()
                    && self.focus == Some(f)
                    && proceed
                    && self.fire_synthetic_click(f)
                    && self.doc.as_ref().and_then(|d| d.element(f)).is_some_and(is_toggle)
                {
                    self.toggle_control(f);
                }
            }
            ShellToTab::Modifiers { shift, ctrl, alt, meta } => {
                self.modifiers = Modifiers { shift, ctrl, alt, meta };
            }
            ShellToTab::Close => {}
        }
    }

    // ----- keys and form controls -----

    /// A key for the page: Tab moves focus; a focused text control takes
    /// the rest as editing; otherwise Enter clicks a link or button,
    /// Space arms a click on a button, checkbox or radio (made on
    /// release), arrows step a select, and the remaining keys scroll.
    fn key(&mut self, key: Key, code: &str, repeat: bool, shift: bool, ctrl: bool, alt: bool) {
        // `keydown` first; cancelled, the key does nothing. A key that
        // produces a character also fires the legacy `keypress`, whose
        // cancellation stops only the text it would insert: browsers
        // scroll and activate on `keydown`.
        if !self.fire_key("keydown", &key, code, repeat, shift, ctrl, alt) {
            return;
        }
        let keypress_ok = !matches!(key, Key::Character(_) | Key::Enter | Key::Space)
            || ctrl
            || alt
            || self.fire_key("keypress", &key, code, repeat, shift, ctrl, alt);
        if key == Key::Tab && !ctrl && !alt {
            self.focus_step(!shift);
            return;
        }
        if key == Key::ContextMenu {
            self.fire_keyboard_contextmenu();
            return;
        }
        let focus = self.focus;
        let focused_is = |pred: fn(&browser_dom::Element) -> bool| {
            focus
                .and_then(|f| self.doc.as_ref()?.element(f))
                .is_some_and(pred)
        };
        let (text, textarea, link, toggle, select, button) = (
            focused_is(is_text_control),
            focused_is(is_textarea),
            focused_is(is_link),
            focused_is(is_toggle),
            focused_is(is_select),
            focused_is(is_button),
        );
        // Only read where one of the flags above is set, so a missing
        // focus never reaches a control.
        let f = focus.unwrap_or_default();
        if text {
            // Enter in an input commits the value: `change` if edited.
            if key == Key::Enter && !textarea && !ctrl && !alt {
                self.fire_change_if_edited(f);
                return;
            }
            if keypress_ok {
                self.edit_key(f, key, ctrl, alt);
            }
            return;
        }
        let h = self.viewport.height;
        match key {
            Key::Enter if (link || button) && !ctrl && !alt => {
                if self.fire_synthetic_click(f) && link {
                    self.follow_link(f);
                }
            }
            Key::Space if toggle || button => self.space_armed = Some(f),
            Key::ArrowUp | Key::ArrowDown if select => self.step_select(f, key == Key::ArrowDown),
            Key::ArrowDown => self.scroll_by(0.0, LINE_SCROLL),
            Key::ArrowUp => self.scroll_by(0.0, -LINE_SCROLL),
            Key::ArrowRight => self.scroll_by(LINE_SCROLL, 0.0),
            Key::ArrowLeft => self.scroll_by(-LINE_SCROLL, 0.0),
            Key::PageDown => self.scroll_by(0.0, h * 0.9),
            Key::PageUp => self.scroll_by(0.0, -h * 0.9),
            Key::Space => self.scroll_by(0.0, if shift { -h * 0.9 } else { h * 0.9 }),
            Key::Home => self.scroll_by(0.0, -1.0e9),
            Key::End => self.scroll_by(0.0, 1.0e9),
            _ => {}
        }
    }

    fn scroll_by(&mut self, dx: f32, dy: f32) {
        self.scroll_x += dx;
        self.scroll_y += dy;
        self.clamp_scroll();
        self.needs_paint = true;
        self.hover_dirty = true;
    }

    /// The live value of a text control: a textarea's text, an input's
    /// `value` attribute (which editing updates, there being no script to
    /// tell the difference yet).
    fn control_value(&self, id: NodeId) -> String {
        let Some(doc) = &self.doc else { return String::new() };
        if doc.element(id).is_some_and(is_textarea) {
            doc.text_content(id)
        } else {
            doc.element(id).and_then(|e| e.attr("value")).unwrap_or("").to_owned()
        }
    }

    fn set_control_value(&mut self, id: NodeId, value: &str) {
        let Some(doc) = &mut self.doc else { return };
        if doc.element(id).is_some_and(is_textarea) {
            doc.set_text_content(id, value);
        } else if let Some(e) = doc.get_mut(id).as_element_mut() {
            e.set_attr("value", value);
        }
        // The value is not a selector subject yet; layout is enough.
        self.needs_layout = true;
    }

    /// Editing keys in a focused text control.
    fn edit_key(&mut self, f: NodeId, key: Key, ctrl: bool, alt: bool) {
        let textarea = self.doc.as_ref().and_then(|d| d.element(f)).is_some_and(is_textarea);
        let mut value = self.control_value(f);
        let mut caret = self.caret.min(value.len());
        while !value.is_char_boundary(caret) {
            caret -= 1;
        }
        let prev_len = |v: &str, c: usize| v[..c].chars().next_back().map_or(0, char::len_utf8);
        let next_len = |v: &str, c: usize| v[c..].chars().next().map_or(0, char::len_utf8);
        // The edit made, as `InputEvent.inputType` and `data`.
        let mut edit: Option<(&str, Option<String>)> = None;
        match key {
            Key::Character(s) if !ctrl && !alt => {
                let s: String = s.chars().filter(|c| !c.is_control()).collect();
                if !s.is_empty() {
                    value.insert_str(caret, &s);
                    caret += s.len();
                    edit = Some(("insertText", Some(s)));
                }
            }
            Key::Space if !ctrl && !alt => {
                value.insert(caret, ' ');
                caret += 1;
                edit = Some(("insertText", Some(" ".to_owned())));
            }
            Key::Enter if textarea && !ctrl && !alt => {
                value.insert(caret, '\n');
                caret += 1;
                edit = Some(("insertLineBreak", None));
            }
            Key::Backspace => {
                let n = prev_len(&value, caret);
                if n > 0 {
                    value.replace_range(caret - n..caret, "");
                    caret -= n;
                    edit = Some(("deleteContentBackward", None));
                }
            }
            Key::Delete => {
                let n = next_len(&value, caret);
                if n > 0 {
                    value.replace_range(caret..caret + n, "");
                    edit = Some(("deleteContentForward", None));
                }
            }
            Key::ArrowLeft => caret -= prev_len(&value, caret),
            Key::ArrowRight => caret += next_len(&value, caret),
            Key::Home => caret = value[..caret].rfind('\n').map_or(0, |i| i + 1),
            Key::End => caret += value[caret..].find('\n').unwrap_or(value.len() - caret),
            _ => {}
        }
        if let Some((input_type, data)) = edit {
            // `beforeinput` may cancel the edit; `input` follows it.
            if !self.fire_input(f, "beforeinput", input_type, data.as_deref()) {
                self.update_caret();
                return;
            }
            self.set_control_value(f, &value);
            // With a layout pending the caret is placed after it.
            self.caret = caret;
            self.fire_input(f, "input", input_type, data.as_deref());
            return;
        }
        self.caret = caret;
        self.update_caret();
    }

    /// A click in a text control puts the caret where it landed.
    fn place_caret_at(&mut self, x: f32, y: f32) {
        let Some(f) = self.focus else { return };
        let Some(doc) = &self.doc else { return };
        let Some(e) = doc.element(f) else { return };
        if !is_text_control(e) || input_type(e).as_deref() == Some("password") {
            return;
        }
        if let Some(p) = self.text_position_at(x, y)
            && (p.node == f || doc.parent(p.node) == Some(f))
        {
            self.caret = p.offset;
            self.update_caret();
        }
    }

    /// Where the caret goes: after the value's character at `caret` in
    /// the control's own text, or at the content box's start when there
    /// is no text.
    fn update_caret(&mut self) {
        let mut caret = None;
        if let (Some(doc), Some(tree), Some(f)) = (&self.doc, &self.layout, self.focus)
            && let Some(e) = doc.element(f)
            && is_text_control(e)
        {
            let value = self.control_value(f);
            let mut off = self.caret.min(value.len());
            while !value.is_char_boundary(off) {
                off -= 1;
            }
            // A password shows one bullet per character.
            let display = if input_type(e).as_deref() == Some("password") {
                value[..off].chars().count() * '\u{2022}'.len_utf8()
            } else {
                off
            };
            let mut best: Option<(usize, Rect, f32)> = None;
            tree.root.walk(&mut |frag| {
                let browser_layout::FragmentContent::Text(t) = &frag.content else { return };
                let mine = frag.node.is_some_and(|n| n == f || doc.parent(n) == Some(f));
                if !mine || t.range.start > display || best.is_some_and(|(s, _, _)| s > t.range.start) {
                    return;
                }
                let x = if display >= t.range.end {
                    t.clusters.iter().map(|c| c.x + c.advance).fold(0.0, f32::max)
                } else {
                    t.clusters
                        .iter()
                        .find(|c| c.start <= display && display < c.end)
                        .map_or(0.0, |c| c.x)
                };
                best = Some((t.range.start, frag.rect, x));
            });
            caret = match best {
                Some((_, r, x)) => Some(Rect::new(r.x + x, r.y, 1.0, r.height)),
                None => tree.first_rect(|n| n == f).map(|r| {
                    let s = &self.styles[f];
                    let (bl, bt, bb) = (s.border_width.left, s.border_width.top, s.border_width.bottom);
                    let (pl, pt, pb) = (
                        s.padding.left.resolve(r.width),
                        s.padding.top.resolve(r.width),
                        s.padding.bottom.resolve(r.width),
                    );
                    Rect::new(r.x + bl + pl, r.y + bt + pt, 1.0, (r.height - bt - bb - pt - pb).max(1.0))
                }),
            };
        }
        if caret != self.caret_rect {
            self.caret_rect = caret;
            self.needs_paint = true;
        }
    }

    /// Flip a checkbox, or check a radio and clear the rest of its group
    /// (same `name`, same form or document).
    fn toggle_control(&mut self, id: NodeId) {
        let Some(doc) = &mut self.doc else { return };
        let Some(e) = doc.element(id) else { return };
        if e.attr("disabled").is_some() {
            return;
        }
        let radio = input_type(e).as_deref() == Some("radio");
        let name = e.attr("name").map(str::to_owned);
        if radio {
            let mut scope = doc.root();
            let mut cur = doc.parent(id);
            while let Some(n) = cur {
                if doc.element(n).is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("form")) {
                    scope = n;
                    break;
                }
                cur = doc.parent(n);
            }
            let group: Vec<NodeId> = doc
                .descendants(scope)
                .filter(|&n| {
                    n != id
                        && doc.element(n).is_some_and(|o| {
                            input_type(o).as_deref() == Some("radio") && o.attr("name").map(str::to_owned) == name
                        })
                })
                .collect();
            for n in group {
                if let Some(o) = doc.get_mut(n).as_element_mut() {
                    o.remove_attr("checked");
                }
            }
            if let Some(e) = doc.get_mut(id).as_element_mut() {
                e.set_attr("checked", "");
            }
        } else if let Some(e) = doc.get_mut(id).as_element_mut() {
            if e.attr("checked").is_some() {
                e.remove_attr("checked");
            } else {
                e.set_attr("checked", "");
            }
        }
        // `:checked` can restyle anything; the cascade runs again.
        self.needs_style = true;
        self.fire_control_changed(id);
    }

    /// Arrow keys on a focused select move the chosen option.
    fn step_select(&mut self, id: NodeId, down: bool) {
        let Some(doc) = &mut self.doc else { return };
        let options: Vec<NodeId> = doc
            .descendants(id)
            .filter(|&n| {
                doc.element(n)
                    .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("option"))
            })
            .collect();
        if options.is_empty() {
            return;
        }
        let current = options
            .iter()
            .position(|&o| doc.element(o).is_some_and(|e| e.attr("selected").is_some()))
            .unwrap_or(0);
        let next = if down {
            (current + 1).min(options.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        if next == current && doc.element(options[current]).is_some_and(|e| e.attr("selected").is_some()) {
            return;
        }
        for (i, &o) in options.iter().enumerate() {
            if let Some(e) = doc.get_mut(o).as_element_mut() {
                if i == next {
                    e.set_attr("selected", "");
                } else {
                    e.remove_attr("selected");
                }
            }
        }
        self.needs_style = true;
        self.fire_control_changed(id);
    }

    // ----- keyboard focus -----

    /// Elements that have something laid out, with their ancestors: an
    /// element with no box of its own (a link) is rendered if any of its
    /// content is.
    fn rendered_elements(&self) -> std::collections::HashSet<NodeId> {
        let mut out = std::collections::HashSet::new();
        let (Some(doc), Some(tree)) = (&self.doc, &self.layout) else {
            return out;
        };
        tree.root.walk(&mut |f| {
            let mut cur = f.node;
            while let Some(n) = cur {
                if !out.insert(n) {
                    break;
                }
                cur = doc.parent(n);
            }
        });
        out
    }

    /// The page's sequential focus navigation order (HTML "tabindex"):
    /// positive `tabindex` values first, ascending, then everything
    /// focusable with `tabindex` 0 or none, in tree order. Elements with a
    /// negative `tabindex` are click-focusable only. Each entry carries the
    /// element's tree position.
    fn focus_order(&self) -> Vec<(usize, NodeId)> {
        let Some(doc) = &self.doc else { return Vec::new() };
        let rendered = self.rendered_elements();
        let mut positive: Vec<(i64, usize, NodeId)> = Vec::new();
        let mut normal: Vec<(usize, NodeId)> = Vec::new();
        for (i, n) in doc.descendants(doc.root()).enumerate() {
            let Some(e) = doc.element(n) else { continue };
            if !is_focusable(e) || !rendered.contains(&n) {
                continue;
            }
            match tabindex(e) {
                Some(t) if t < 0 => {}
                Some(t) if t > 0 => positive.push((t, i, n)),
                _ => normal.push((i, n)),
            }
        }
        positive.sort();
        positive.into_iter().map(|(_, i, n)| (i, n)).chain(normal).collect()
    }

    /// Tab or Shift+Tab: focus the next or previous element in the tab
    /// order. Past either end, focus leaves the page for the chrome.
    fn focus_step(&mut self, forward: bool) {
        let order = self.focus_order();
        let current = self.focus.and_then(|f| order.iter().position(|&(_, n)| n == f));
        let next = match (current, self.focus) {
            (Some(i), _) => {
                if forward {
                    order.get(i + 1)
                } else {
                    i.checked_sub(1).and_then(|j| order.get(j))
                }
            }
            // Focused by a click on something outside the order (a
            // negative tabindex): continue from its place in the tree.
            (None, Some(f)) => {
                let pos = self
                    .doc
                    .as_ref()
                    .and_then(|d| d.descendants(d.root()).position(|n| n == f))
                    .unwrap_or(0);
                if forward {
                    order.iter().find(|&&(i, _)| i > pos)
                } else {
                    order.iter().rev().find(|&&(i, _)| i < pos)
                }
            }
            (None, None) => {
                if forward {
                    order.first()
                } else {
                    order.last()
                }
            }
        }
        .map(|&(_, n)| n);
        self.set_focus(next, true);
        if next.is_some() {
            self.scroll_focus_into_view();
        } else {
            self.send(TabToShell::FocusOut { forward });
        }
    }

    /// The boxes of the focused element, one per line for inline content,
    /// or nothing when focus did not come from the keyboard.
    fn compute_focus_ring(&self) -> Vec<Rect> {
        let (Some(doc), Some(tree), Some(focus)) = (&self.doc, &self.layout, self.focus) else {
            return Vec::new();
        };
        if !self.states.has(focus, ElementStates::FOCUS_VISIBLE) {
            return Vec::new();
        }
        let mut rects: Vec<Rect> = Vec::new();
        tree.root.walk(&mut |f| {
            let Some(n) = f.node else { return };
            let mut cur = Some(n);
            let inside = loop {
                match cur {
                    Some(c) if c == focus => break true,
                    Some(c) => cur = doc.parent(c),
                    None => break false,
                }
            };
            if inside && f.rect.width > 0.0 && f.rect.height > 0.0 {
                // A box fragment covers its own children; keep the outer one.
                if let Some(last) = rects.last()
                    && last.x <= f.rect.x
                    && last.y <= f.rect.y
                    && last.right() >= f.rect.right()
                    && last.bottom() >= f.rect.bottom()
                {
                    return;
                }
                rects.push(f.rect);
            }
        });
        // Join text fragments that sit side by side on one line.
        rects.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
        let mut merged: Vec<Rect> = Vec::new();
        for r in rects {
            match merged.last_mut() {
                Some(m)
                    if (m.y - r.y).abs() < 0.5 && (m.height - r.height).abs() < 0.5 && r.x <= m.right() + 0.5 =>
                {
                    let right = m.right().max(r.right());
                    m.width = right - m.x;
                }
                _ => merged.push(r),
            }
        }
        merged
    }

    /// Scroll so the focus ring is inside the viewport, with a margin.
    fn scroll_focus_into_view(&mut self) {
        let Some(first) = self.focus_ring.first().copied() else { return };
        let union = self.focus_ring.iter().fold(first, |u, r| {
            let x = u.x.min(r.x);
            let y = u.y.min(r.y);
            Rect::new(x, y, u.right().max(r.right()) - x, u.bottom().max(r.bottom()) - y)
        });
        let margin = 24.0;
        let (w, h) = (self.viewport.width, self.viewport.height);
        if union.bottom() + margin > self.scroll_y + h {
            self.scroll_y = union.bottom() + margin - h;
        }
        if union.y - margin < self.scroll_y {
            self.scroll_y = union.y - margin;
        }
        if union.right() + margin > self.scroll_x + w {
            self.scroll_x = union.right() + margin - w;
        }
        if union.x - margin < self.scroll_x {
            self.scroll_x = union.x - margin;
        }
        self.clamp_scroll();
        self.needs_paint = true;
        self.hover_dirty = true;
    }

    // ----- find in page -----

    /// A new query. The current match stays where it was if a match still
    /// starts there or later (typing more of a word keeps its place);
    /// otherwise the first match is current.
    fn find(&mut self, query: String) {
        if query.is_empty() {
            if self.find.take().is_some() {
                self.needs_paint = true;
            }
            self.send(TabToShell::FindResult {
                current: None,
                total: 0,
            });
            return;
        }
        let prefer = self.current_match_start();
        self.find = Some(Find {
            query,
            matches: Vec::new(),
            current: None,
            ranges: SelectionRanges::default(),
            current_ranges: SelectionRanges::default(),
        });
        self.refresh_find(prefer, true);
    }

    fn current_match_start(&self) -> Option<TextPos> {
        let f = self.find.as_ref()?;
        f.matches.get(f.current?).map(|m| m.0)
    }

    /// Search the current layout again and pick the current match: the
    /// first one starting at or after `prefer`, else the first.
    fn refresh_find(&mut self, prefer: Option<TextPos>, scroll: bool) {
        let Some(tree) = self.layout.as_ref() else { return };
        let Some(f) = self.find.as_mut() else { return };
        f.matches = selection::find_all(tree, &f.query);
        f.current = if f.matches.is_empty() {
            None
        } else {
            let mut positions: Vec<TextPos> = f.matches.iter().map(|m| m.0).collect();
            positions.extend(prefer);
            let keys = selection::position_keys(tree, &positions);
            let index = prefer.and_then(|_| {
                let wanted = keys.last().copied().flatten()?;
                keys[..f.matches.len()].iter().position(|k| k.is_some_and(|k| k >= wanted))
            });
            Some(index.unwrap_or(0))
        };
        f.ranges = selection::ranges_of_all(tree, &f.matches);
        f.current_ranges = match f.current.and_then(|i| f.matches.get(i)) {
            Some(&(a, b)) => selection::selection_ranges(tree, a, b),
            None => SelectionRanges::default(),
        };
        self.needs_paint = true;
        if scroll {
            self.scroll_to_current_match();
        }
        self.report_find();
    }

    fn find_next(&mut self, forward: bool) {
        let Some(tree) = self.layout.as_ref() else { return };
        let Some(f) = self.find.as_mut() else { return };
        let n = f.matches.len();
        if n == 0 {
            return;
        }
        let i = f.current.unwrap_or(0);
        let next = if forward { (i + 1) % n } else { (i + n - 1) % n };
        f.current = Some(next);
        let (a, b) = f.matches[next];
        f.current_ranges = selection::selection_ranges(tree, a, b);
        self.needs_paint = true;
        self.scroll_to_current_match();
        self.report_find();
    }

    /// Bring the current match into the viewport, a third of the way down.
    fn scroll_to_current_match(&mut self) {
        let Some(tree) = self.layout.as_ref() else { return };
        let Some((a, b)) = self.find.as_ref().and_then(|f| f.current.map(|i| f.matches[i])) else {
            return;
        };
        let Some(rect) = selection::first_rect(tree, a, b) else { return };
        let (w, h) = (self.viewport.width, self.viewport.height);
        if rect.y < self.scroll_y || rect.bottom() > self.scroll_y + h {
            self.scroll_y = rect.y - h / 3.0;
        }
        if rect.x < self.scroll_x || rect.right() > self.scroll_x + w {
            self.scroll_x = rect.x - w / 3.0;
        }
        self.clamp_scroll();
        self.hover_dirty = true;
    }

    fn report_find(&mut self) {
        let (current, total) = match &self.find {
            Some(f) => (f.current.map(|i| i + 1), f.matches.len()),
            None => (None, 0),
        };
        self.send(TabToShell::FindResult { current, total });
    }

    // ----- text selection -----

    /// How many primary presses in a row this one makes: a second within
    /// half a second and a few pixels of the first is a double click, a
    /// third a triple; after that it starts over.
    fn count_click(&mut self, x: f32, y: f32) -> u32 {
        let now = std::time::Instant::now();
        let count = match self.last_click {
            Some((at, lx, ly, n))
                if now.duration_since(at) < std::time::Duration::from_millis(500)
                    && (x - lx).abs() <= 4.0
                    && (y - ly).abs() <= 4.0 =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, x, y, count));
        count
    }

    fn text_position_at(&self, x: f32, y: f32) -> Option<TextPos> {
        let tree = self.layout.as_ref()?;
        selection::text_position_at(tree, x + self.scroll_x, y + self.scroll_y)
    }

    fn set_selection(&mut self, sel: Option<(TextPos, TextPos)>) {
        let sel = sel.filter(|(a, b)| a != b);
        if sel != self.selection {
            self.selection = sel;
            self.needs_paint = true;
        }
    }

    /// A primary press: one click clears the selection and starts a drag
    /// from the point, unless on a link (dragging a link is not selecting);
    /// a double click selects the word and a triple the paragraph.
    fn begin_selection(&mut self, x: f32, y: f32, clicks: u32, on_link: bool) {
        self.select_anchor = None;
        let pos = self.text_position_at(x, y);
        let Some(tree) = self.layout.as_ref() else {
            self.set_selection(None);
            return;
        };
        let sel = match (clicks, pos) {
            (2, Some(p)) => selection::word_at(tree, p),
            (3, Some(p)) => selection::paragraph_at(tree, p),
            (_, Some(p)) => {
                if !on_link {
                    self.select_anchor = Some(p);
                }
                None
            }
            _ => None,
        };
        self.set_selection(sel);
    }

    /// Move the selection's focus to the pointer during a drag.
    fn extend_selection_to(&mut self, x: f32, y: f32) {
        let Some(anchor) = self.select_anchor else { return };
        if let Some(focus) = self.text_position_at(x, y) {
            self.set_selection(Some((anchor, focus)));
        }
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let tree = self.layout.as_ref()?;
        Some(selection::selection_text(tree, a, b))
    }

    fn selection_ranges(&self) -> SelectionRanges {
        match (self.selection, self.layout.as_ref()) {
            (Some((a, b)), Some(tree)) => selection::selection_ranges(tree, a, b),
            _ => SelectionRanges::default(),
        }
    }

    /// One step of scrolling toward a pointer held outside the viewport
    /// during a selection drag. Returns when the next step is due.
    fn autoscroll_step(&mut self) -> Option<std::time::Instant> {
        let (x, y) = self.mouse?;
        self.select_anchor?;
        let over = if y < 0.0 {
            y
        } else if y > self.viewport.height {
            y - self.viewport.height
        } else {
            return None;
        };
        let before = self.scroll_y;
        self.scroll_y += (over * 0.25).clamp(-60.0, 60.0);
        self.clamp_scroll();
        if self.scroll_y != before {
            self.needs_paint = true;
            self.hover_dirty = true;
            self.extend_selection_to(x, y);
        }
        Some(std::time::Instant::now() + std::time::Duration::from_millis(50))
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
            NavKind::Reload | NavKind::Replace => match self.history.get_mut(self.history_index) {
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
    /// what has arrived of the new one is shown. Scripts still waiting
    /// (for the network, or for parsing to end) are dropped.
    fn stop(&mut self) {
        self.nav = None;
        self.pending.clear();
        self.scripts.clear_loads();
        // The parser exists only once a navigation has committed.
        if let Some(parser) = self.parser.take() {
            let doc = parser.finish();
            self.set_document(doc);
        }
        self.state_dirty = true;
        // Nothing is being fetched any more: the document is complete.
        self.update_readiness();
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
        // The old document's own fetches are moot now, and so is a parse
        // it may still have had going.
        self.pending.retain(|k, _| *k == id);
        self.parser = None;
        self.main_done = false;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.scroll_reported = (0.0, 0.0);
        // The new document's scripts run while it parses, so its context
        // starts here; the old document's timers stop with its context.
        self.new_script_host();
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
        self.restyle_for(changed, StateKind::Target);
    }

    /// The tab wants to be woken at this instant even if no event arrives.
    pub fn next_wake(&self) -> Option<std::time::Instant> {
        [
            self.refresh.as_ref().map(|(at, _)| *at),
            self.autoscroll,
            self.script.as_ref().and_then(ScriptHost::next_wake),
            self.frame_due,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Run whatever timer is due.
    pub fn tick(&mut self) {
        let now = std::time::Instant::now();
        if let Some(at) = self.autoscroll
            && now >= at
        {
            self.autoscroll = self.autoscroll_step();
        }
        // Script timers, then an animation frame if one is due: each
        // callback is a task with its own microtask checkpoint.
        if self
            .script
            .as_ref()
            .and_then(ScriptHost::next_wake)
            .is_some_and(|w| now >= w)
        {
            self.sync_document_info();
            self.with_script(|s| {
                s.run_timers(now);
            });
            self.after_script();
        }
        if self.frame_due.is_some_and(|due| now >= due) {
            self.frame_due = None;
            let time_ms = self.doc_started.elapsed().as_secs_f64() * 1000.0;
            self.sync_document_info();
            self.with_script(|s| {
                s.run_animation_frames(time_ms);
            });
            self.needs_paint = true;
            self.after_script();
        }
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
        self.ancestor_or_self(id, is_focusable)
    }

    /// Re-evaluate what is under the pointer; restyle if it changed.
    /// The page's vertical scrollbar, if the content overflows.
    fn scrollbar(&self) -> Option<Scrollbar> {
        let content_height = self.layout.as_ref()?.content_height;
        Scrollbar::vertical(self.viewport.width, self.viewport.height, content_height, self.scroll_y)
    }

    fn over_scrollbar(&self) -> bool {
        self.mouse
            .is_some_and(|(x, y)| self.scrollbar().is_some_and(|sb| sb.contains(x, y)))
    }

    fn update_hover(&mut self) {
        self.hover_dirty = false;
        // The scrollbar covers the page under it.
        let raw = if self.over_scrollbar() {
            None
        } else {
            self.mouse.and_then(|(x, y)| self.hit_node(x, y))
        };
        // A capturing element is what the pointer is "over" until release.
        let hit = match self.pointer_capture {
            Some(c) if self.live(c) => Some(c),
            _ => raw.and_then(|n| self.element_of(n)),
        };
        if hit == self.hover {
            return;
        }
        let old = self.hover;
        self.hover = hit;
        self.fire_hover_change(old, hit);
        // A listener may have changed the tree; `hover` is pruned then.
        let hit = self.hover;

        let over_link = hit.and_then(|h| self.ancestor_or_self(h, is_link)).is_some();
        // Text nodes, and the made-up text of a text control (whose
        // fragments carry the control itself).
        let over_text = raw.is_some_and(|n| {
            self.doc
                .as_ref()
                .is_some_and(|d| d.contains(n) && (!d.get(n).is_element() || d.element(n).is_some_and(is_text_control)))
        });
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
        self.restyle_for(changed, StateKind::Hover);
    }

    fn set_active(&mut self, target: Option<NodeId>) {
        let Some(doc) = &self.doc else { return };
        let changed = self.states.set_chain(doc, target, ElementStates::ACTIVE);
        self.restyle_for(changed, StateKind::Active);
    }

    /// Focus `target`. Focus from the keyboard also gets `:focus-visible`
    /// and the focus ring; a click gets neither.
    fn set_focus(&mut self, target: Option<NodeId>, keyboard: bool) {
        let old = self.focus;
        let mut target = target;
        if target != old {
            self.space_armed = None;
            if let Some(o) = old {
                self.fire_blur(o, target);
            }
            // A blur listener may have taken the new target out.
            if target.is_some_and(|t| !self.live(t)) {
                target = None;
            }
        }
        let visible = if keyboard { target } else { None };
        let same = target == self.focus && visible.is_some() == self.states.has_any(ElementStates::FOCUS_VISIBLE);
        self.focus = target;
        let Some(doc) = &self.doc else { return };
        let mut changed = self.states.set_single(target, ElementStates::FOCUS);
        changed.extend(self.states.set_chain(doc, target, ElementStates::FOCUS_WITHIN));
        changed.extend(self.states.set_single(visible, ElementStates::FOCUS_VISIBLE));
        changed.sort();
        changed.dedup();
        if !same {
            self.restyle_for(changed, StateKind::Focus);
            // A text control starts with the caret at its end.
            self.caret = target.map(|t| self.control_value(t).len()).unwrap_or(0);
        }
        let ring = self.compute_focus_ring();
        if ring != self.focus_ring {
            self.focus_ring = ring;
            self.needs_paint = true;
        }
        self.update_caret();
        if target != old
            && let Some(t) = target
        {
            self.fire_focus(t, old);
        }
    }

    /// Restyle what a state change on `changed` can affect, per `reach`,
    /// and schedule layout if any computed style moved.
    fn restyle_for(&mut self, changed: Vec<NodeId>, state: StateKind) {
        let reach = self.deps.reach(state);
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
                state,
                deps: &self.deps,
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
                    self.pump_scripts();
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
        // A finished fetch may have been the last one the load waited on.
        self.update_readiness();
    }

    /// Move the document along its lifecycle and fire what each step
    /// fires: `readystatechange` on the document when it becomes
    /// `interactive` (parsed) and `complete` (nothing left to fetch),
    /// `DOMContentLoaded` on the document (bubbling to `window`) once the
    /// deferred scripts have run, `load` on `window` at `complete`.
    fn update_readiness(&mut self) {
        loop {
            let parsed = self.parser.is_none() && self.doc.is_some() && self.script.is_some();
            let deferred_done = self.scripts.deferred.is_empty() && !self.scripts.running_deferred;
            let fetched = self.nav.is_none() && self.pending.is_empty();
            match self.lifecycle {
                Lifecycle::Loading if parsed => {
                    self.lifecycle = Lifecycle::Interactive;
                    self.fire_lifecycle_event(EventTargetRef::Document, "readystatechange", false);
                }
                Lifecycle::Interactive if parsed && deferred_done => {
                    self.lifecycle = Lifecycle::ContentLoaded;
                    self.fire_lifecycle_event(EventTargetRef::Document, "DOMContentLoaded", true);
                }
                Lifecycle::ContentLoaded if parsed && deferred_done && fetched => {
                    self.lifecycle = Lifecycle::Complete;
                    self.fire_lifecycle_event(EventTargetRef::Document, "readystatechange", false);
                    self.fire_lifecycle_event(EventTargetRef::Window, "load", false);
                }
                _ => return,
            }
        }
    }

    /// Fire a trusted, non-cancelable event from the tab as a task.
    fn fire_lifecycle_event(&mut self, target: EventTargetRef, kind: &str, bubbles: bool) {
        self.sync_document_info();
        self.with_script(|host| {
            host.fire_event(target, kind, bubbles, false);
        });
        self.after_script();
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
                if let Some(parser) = &mut self.parser {
                    // The parser finishes once it is not waiting on a
                    // script; `drive_parser` sets the document then.
                    parser.end_input();
                    self.main_done = true;
                    self.pump_scripts();
                    self.state_dirty = true;
                    return;
                }
                let doc = {
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
            PendingKind::Stylesheet { url, depth } => {
                if p.status == 0 || (200..300).contains(&p.status) {
                    let (_, charset) = Self::content_type_of(&p.headers);
                    let css = browser_dom::encoding::decode_stylesheet(&p.body, charset.as_deref());
                    tracing::debug!(tab = self.id.0, bytes = css.len(), url = url.as_str(), "external stylesheet loaded");
                    let sheet = Arc::new(Stylesheet::parse_with_base(&css, Origin::Author, Some(&url)));
                    self.loaded_sheets.insert(url.clone(), sheet.clone());
                    // Every slot waiting for this URL (a script may have
                    // added the same link twice). Imports are inserted
                    // before a slot, moving it, so each is found afresh.
                    while let Some(slot) = self
                        .sheets
                        .iter()
                        .position(|s| s.url.as_ref() == Some(&url) && s.sheet.is_none())
                    {
                        let slot = slot + self.queue_imports(slot, &sheet, depth);
                        self.sheets[slot].sheet = Some(sheet.clone());
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
            PendingKind::Script { id } => {
                let ok = p.status == 0 || (200..300).contains(&p.status);
                let (ct, charset) = Self::content_type_of(&p.headers);
                if let Some(host) = &mut self.script
                    && let Some(load) = self.scripts.get_mut(id)
                {
                    let url = load.url.clone().unwrap_or_else(|| Url::parse("about:blank").expect("static url"));
                    load.state = if !ok {
                        LoadState::Failed(format!("Failed to load script {url}: HTTP {}", p.status))
                    } else {
                        match load.kind {
                            ScriptKind::Classic => {
                                LoadState::Classic(browser_dom::encoding::decode_script(&p.body, charset.as_deref()))
                            }
                            ScriptKind::Module if !is_javascript_mime(&ct) => LoadState::Failed(format!(
                                "Failed to load module script {url}: expected a JavaScript MIME type but the server responded with \"{ct}\""
                            )),
                            ScriptKind::Module => {
                                // Modules are always UTF-8.
                                let text = browser_dom::encoding::decode_script(&p.body, None);
                                match host.parse_module(&text, &url, true) {
                                    Ok(m) => LoadState::Module(m),
                                    Err(e) => LoadState::Failed(format!("{e} (in {url})")),
                                }
                            }
                        }
                    };
                }
                self.run_ready_scripts();
            }
            PendingKind::ModuleSource { url } => {
                let ok = p.status == 0 || (200..300).contains(&p.status);
                let (ct, _) = Self::content_type_of(&p.headers);
                let result = if !ok {
                    Err(format!("Failed to load module script {url}: HTTP {}", p.status))
                } else if !is_javascript_mime(&ct) {
                    Err(format!(
                        "Failed to load module script {url}: expected a JavaScript MIME type but the server responded with \"{ct}\""
                    ))
                } else {
                    Ok(browser_dom::encoding::decode_script(&p.body, None))
                };
                if let Some(host) = &mut self.script {
                    host.module_fetched(url, result);
                }
                self.run_ready_scripts();
            }
        }
        self.state_dirty = true;
    }

    fn show_error_page(&mut self, url: &str, error: &str) {
        // Whatever was being parsed or loaded for the failed document is
        // over; the error page gets a context of its own.
        self.parser = None;
        self.main_done = false;
        self.script = None;
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
        self.loaded_sheets.clear();
        self.images = ImageStore::new();
        self.doc = Some(doc);
        self.doc_url = self.url.clone();
        // Node ids of the old document mean nothing now. The pointer
        // position is kept: hover is re-evaluated after the first layout.
        self.states = ElementStates::default();
        self.hover = None;
        self.focus = None;
        self.focus_ring.clear();
        self.caret_rect = None;
        self.press_control = None;
        self.press = None;
        self.selection = None;
        self.select_anchor = None;
        self.autoscroll = None;
        // The query outlives the page; the matches are found again after
        // the new document's first layout.
        if let Some(f) = &mut self.find {
            f.matches.clear();
            f.current = None;
            f.ranges = SelectionRanges::default();
            f.current_ranges = SelectionRanges::default();
        }
        self.sync_stylesheets();
        self.collect_images();
        self.pending_fragment = self.url.as_ref().is_some_and(|u| u.fragment().is_some());
        self.schedule_refresh();
        // A parsed document already has its context (made at commit);
        // documents made up here (error pages, wrapped non-HTML) get one.
        if self.script.is_none() {
            self.new_script_host();
        }
        // Parsed: `interactive`, before any deferred script runs.
        self.update_readiness();
        self.needs_style = true;
        self.state_dirty = true;
    }

    // ----- script -----

    /// Frame interval for animation frame callbacks.
    const FRAME: std::time::Duration = std::time::Duration::from_micros(16_667);

    /// A new document gets a new context; the old one's timers, frames
    /// and waiting scripts die with it.
    fn new_script_host(&mut self) {
        self.frame_due = None;
        self.doc_started = std::time::Instant::now();
        self.scripts = Scripts::default();
        self.lifecycle = Lifecycle::Loading;
        self.script = match ScriptHost::new(self.url.clone(), browser_net::USER_AGENT) {
            Ok(host) => Some(host),
            Err(e) => {
                tracing::error!(tab = self.id.0, "script context: {e}");
                None
            }
        };
    }

    /// Run `f` on the script host with the document lent to it, so the
    /// DOM bindings can read and write it, and take the document back
    /// after. The document is moved, not borrowed: it comes out of the
    /// blocked parser while the page parses and out of `doc` after, and
    /// goes back to the same place. Nothing else touches it meanwhile,
    /// since `f` only sees the host.
    fn with_script(&mut self, f: impl FnOnce(&mut ScriptHost)) {
        let Some(host) = &mut self.script else { return };
        let parsing = self.parser.is_some();
        let states = self.states.clone();
        let doc = match (&mut self.parser, &mut self.doc) {
            (Some(parser), _) => std::mem::take(&mut *parser.document_mut()),
            (None, Some(doc)) => std::mem::take(doc),
            (None, None) => Document::new(),
        };
        host.lend_document(doc, parsing, states);
        f(host);
        let doc = host.reclaim_document();
        match (&mut self.parser, &mut self.doc) {
            (Some(parser), _) => *parser.document_mut() = doc,
            (None, Some(slot)) => *slot = doc,
            (None, None) => {}
        }
    }

    /// Tell the bindings where the document stands before a script runs:
    /// its URL, its title so far, and its readiness (`loading` while it
    /// parses, `interactive` while deferred scripts remain, `complete`).
    fn sync_document_info(&mut self) {
        let Some(host) = &mut self.script else { return };
        let title = match (&self.parser, &self.doc) {
            (Some(parser), _) => parser.document().title(),
            (None, Some(doc)) => doc.title(),
            (None, None) => None,
        };
        let ready = match self.lifecycle {
            Lifecycle::Loading => "loading",
            Lifecycle::Interactive | Lifecycle::ContentLoaded => "interactive",
            Lifecycle::Complete => "complete",
        };
        // A document not decoded from bytes (`about:`) counts as UTF-8.
        let (encoding, quirks) = match (&self.parser, &self.doc) {
            (Some(parser), _) => (parser.encoding(), parser.document().quirks_mode),
            (None, Some(doc)) => (doc.encoding, doc.quirks_mode),
            (None, None) => (None, browser_dom::QuirksMode::NoQuirks),
        };
        let charset = encoding.map_or("UTF-8", |e| e.name());
        let quirks = quirks == browser_dom::QuirksMode::Quirks;
        host.set_document_info(self.url.clone(), title.unwrap_or_default(), ready, charset, quirks);
    }

    /// Act on what a script asked of the document or the tab.
    fn apply_host_requests(&mut self, requests: Vec<HostRequest>) {
        for request in requests {
            match request {
                HostRequest::SetTitle(title) => {
                    match (&mut self.parser, &mut self.doc) {
                        (Some(parser), _) => parser.document_mut().set_title(&title),
                        (None, Some(doc)) => doc.set_title(&title),
                        (None, None) => {}
                    }
                    self.state_dirty = true;
                }
                HostRequest::Navigate { url, replace } => {
                    if !matches!(url.scheme(), "http" | "https" | "data" | "about") {
                        continue;
                    }
                    self.go(url, if replace { NavKind::Replace } else { NavKind::Push });
                    // Later requests belong to a document that is going away.
                    break;
                }
                HostRequest::Reload => {
                    self.handle_shell(ShellToTab::Reload);
                    break;
                }
            }
        }
    }

    /// Parse what has arrived, running the scripts the parser stops at,
    /// finish the document once its response has ended, and run whatever
    /// script is ready.
    fn pump_scripts(&mut self) {
        self.drive_parser();
        self.run_ready_scripts();
        self.update_readiness();
    }

    /// Run the parser until it runs dry or waits on a script fetch. Every
    /// `<script>` it stops at goes through `prepare_script`; an inline
    /// classic script runs before the parser continues, as the HTML
    /// standard requires, so a later part of the document is not yet in
    /// the tree when it runs.
    fn drive_parser(&mut self) {
        loop {
            let Some(parser) = &self.parser else { return };
            let Some(node) = parser.blocked_script() else { break };
            if self.scripts.blocking.is_some() {
                // Waiting for the network; `run_ready_scripts` resumes.
                return;
            }
            match self.prepare_script(node) {
                Prepared::Run(source) => {
                    self.sync_document_info();
                    self.with_script(|host| host.run_script(&source));
                    self.after_script();
                }
                Prepared::Blocking => return,
                Prepared::Done => {}
            }
            if let Some(parser) = &mut self.parser {
                parser.resume();
            }
        }
        if self.main_done
            && self.parser.as_ref().is_some_and(HtmlParser::is_done)
            && let Some(parser) = self.parser.take()
        {
            let doc = parser.finish();
            self.set_document(doc);
        }
    }

    /// HTML "prepare the script element" for a script the parser stopped
    /// at. Classic: inline runs now; external is fetched and blocks the
    /// parser, unless `defer` (runs after parsing, in order) or `async`
    /// (runs when it arrives). Module: inline or external, its graph
    /// loads and it runs after parsing in order, or `async` when ready.
    /// Data blocks, unknown types and `nomodule` classic scripts are
    /// skipped.
    fn prepare_script(&mut self, node: NodeId) -> Prepared {
        let Some(parser) = &self.parser else { return Prepared::Done };
        let (kind, src, is_async, defer, text, doc_url) = {
            let doc = parser.document();
            let Some(e) = doc.element(node) else { return Prepared::Done };
            let Some(kind) = script_kind(e) else { return Prepared::Done };
            let src = e.attr("src").map(|s| (s.to_owned(), doc.resolve_url(s)));
            let text = if src.is_none() { doc.text_content(node) } else { String::new() };
            (
                kind,
                src,
                e.attr("async").is_some(),
                e.attr("defer").is_some(),
                text,
                doc.base_url.clone(),
            )
        };
        match src {
            Some((raw, resolved)) => {
                let url = match resolved {
                    Some(u) if !raw.trim().is_empty() && matches!(u.scheme(), "http" | "https" | "data") => u,
                    _ => {
                        self.report_script_error(format!("Failed to load script: cannot use src \"{raw}\""));
                        return Prepared::Done;
                    }
                };
                let id = self.scripts.next_id();
                let load = ScriptLoad {
                    id,
                    kind,
                    url: Some(url.clone()),
                    state: LoadState::Fetching,
                };
                let blocking = !is_async && !defer && kind == ScriptKind::Classic;
                if is_async {
                    self.scripts.asap.push(load);
                } else if blocking {
                    self.scripts.blocking = Some(load);
                } else {
                    self.scripts.deferred.push_back(load);
                }
                let mut request = self.subresource(url);
                request.headers.push(("accept".to_owned(), "*/*".to_owned()));
                self.fetch(request, PendingKind::Script { id });
                if blocking { Prepared::Blocking } else { Prepared::Done }
            }
            None => match kind {
                ScriptKind::Classic => {
                    if text.trim().is_empty() {
                        Prepared::Done
                    } else {
                        Prepared::Run(text)
                    }
                }
                ScriptKind::Module => {
                    let Some(host) = &mut self.script else { return Prepared::Done };
                    let url = doc_url
                        .or_else(|| self.url.clone())
                        .unwrap_or_else(|| Url::parse("about:blank").expect("static url"));
                    match host.parse_module(&text, &url, false) {
                        Ok(m) => {
                            let load = ScriptLoad {
                                id: self.scripts.next_id(),
                                kind,
                                url: None,
                                state: LoadState::Module(m),
                            };
                            if is_async {
                                self.scripts.asap.push(load);
                            } else {
                                self.scripts.deferred.push_back(load);
                            }
                        }
                        // A syntax error is reported where the parser is,
                        // as browsers do.
                        Err(e) => self.report_script_error(format!("Uncaught {e}")),
                    }
                    Prepared::Done
                }
            },
        }
    }

    /// Run every script whose turn has come: the parser-blocking one (and
    /// then parse on), `async` ones as they are ready, and once the
    /// document has parsed the deferred ones in order. Module graphs are
    /// polled first so a module whose last import just arrived counts as
    /// ready.
    fn run_ready_scripts(&mut self) {
        loop {
            let mut progressed = self.poll_modules();
            if self.scripts.blocking.as_ref().is_some_and(ScriptLoad::is_ready)
                && let Some(load) = self.scripts.blocking.take()
            {
                self.execute_script(load);
                if let Some(parser) = &mut self.parser {
                    parser.resume();
                }
                self.drive_parser();
                progressed = true;
            }
            let mut i = 0;
            while i < self.scripts.asap.len() {
                if self.scripts.asap[i].is_ready() {
                    let load = self.scripts.asap.remove(i);
                    self.execute_script(load);
                    progressed = true;
                } else {
                    i += 1;
                }
            }
            // Deferred scripts wait for the end of parsing. Before a new
            // navigation commits, the loads here are the shown document's.
            let parsed = self.parser.is_none() && self.doc.is_some();
            while parsed
                && self.scripts.deferred.front().is_some_and(ScriptLoad::is_ready)
                && let Some(load) = self.scripts.deferred.pop_front()
            {
                self.scripts.running_deferred = true;
                self.execute_script(load);
                self.scripts.running_deferred = false;
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
    }

    /// One loading round for every module still loading its graph; fetch
    /// what the rounds ask for. Returns whether any load settled here
    /// without the network (so the caller polls again).
    fn poll_modules(&mut self) -> bool {
        let Some(host) = &mut self.script else { return false };
        let mut to_fetch = Vec::new();
        for load in self.scripts.iter_mut() {
            let LoadState::Module(m) = &mut load.state else { continue };
            if m.is_ready() {
                continue;
            }
            match host.poll_module(m) {
                ModuleProgress::Ready => {}
                ModuleProgress::Fetch(urls) => to_fetch.extend(urls),
                ModuleProgress::Failed(why) => load.state = LoadState::Failed(format!("Uncaught {why}")),
            }
        }
        let mut settled = false;
        for url in to_fetch {
            settled |= self.fetch_module_source(url);
        }
        settled
    }

    /// Fetch a module for the module map, once per URL. Returns true when
    /// the answer was given on the spot (an unsupported scheme).
    fn fetch_module_source(&mut self, url: Url) -> bool {
        if !self.scripts.module_urls.insert(url.clone()) {
            return false;
        }
        if !matches!(url.scheme(), "http" | "https" | "data") {
            if let Some(host) = &mut self.script {
                host.module_fetched(
                    url.clone(),
                    Err(format!("Failed to load module script {url}: unsupported scheme")),
                );
            }
            return true;
        }
        let mut request = self.subresource(url.clone());
        request.headers.push(("accept".to_owned(), "*/*".to_owned()));
        self.fetch(request, PendingKind::ModuleSource { url });
        false
    }

    /// Run a ready script as a task, or report why it cannot run.
    fn execute_script(&mut self, load: ScriptLoad) {
        self.sync_document_info();
        self.with_script(|host| match load.state {
            LoadState::Classic(source) => host.run_script_from(&source, load.url.as_ref()),
            LoadState::Module(m) => host.run_module(m),
            LoadState::Failed(why) => host.report_error(why),
            LoadState::Fetching => {}
        });
        self.after_script();
    }

    fn report_script_error(&mut self, text: String) {
        if let Some(host) = &mut self.script {
            host.report_error(text);
        }
        self.after_script();
    }

    /// After script ran: forward console output, fetch the modules a
    /// dynamic `import()` asked for, and schedule a frame if one was
    /// asked for.
    fn after_script(&mut self) {
        let Some(host) = &mut self.script else { return };
        let requests = host.take_module_requests();
        let host_requests = host.take_requests();
        let mutated = host.take_dom_mutated();
        for line in host.take_console() {
            match line.level {
                ConsoleLevel::Warn | ConsoleLevel::Error => {
                    tracing::warn!(tab = self.id.0, "console: {}", line.text);
                }
                ConsoleLevel::Log | ConsoleLevel::Info => {
                    tracing::info!(tab = self.id.0, "console: {}", line.text);
                }
            }
            self.console.push(line);
        }
        if self.console.len() > 500 {
            let excess = self.console.len() - 500;
            self.console.drain(..excess);
        }
        if host.has_frame_callbacks() && self.frame_due.is_none() {
            self.frame_due = Some(std::time::Instant::now() + Self::FRAME);
        }
        // A dynamic `import()` of a module not in the map rejects for now
        // and is fetched here, so the map has it the next time (known
        // limitation until fetch, Phase 3 item 3.5, makes imports wait).
        for url in requests {
            self.fetch_module_source(url);
        }
        if mutated {
            self.dom_changed();
        }
        self.apply_host_requests(host_requests);
    }

    /// A script changed the tree: restyle the whole document (per-node
    /// invalidation is later), lay out and paint again, tell the shell
    /// (the title may have changed), and drop every node reference the
    /// change may have made stale: a freed node, or one no longer in the
    /// document. Hover is found again after layout.
    fn dom_changed(&mut self) {
        self.needs_style = true;
        self.needs_layout = true;
        self.state_dirty = true;
        // A `<style>` or `<link>` the script added, changed or removed.
        self.sync_stylesheets();
        let live = |doc: &Document, id: NodeId| doc.contains(id) && doc.is_connected(id);
        let (focus_lost, hover_lost, press_control_lost, press_lost) = match (&self.parser, &self.doc) {
            (Some(parser), _) => {
                let doc = parser.document();
                self.states.retain_live(&doc);
                (
                    self.focus.is_some_and(|f| !live(&doc, f)),
                    self.hover.is_some_and(|h| !live(&doc, h)),
                    self.press_control.is_some_and(|p| !live(&doc, p)),
                    self.press.is_some_and(|(p, _)| !live(&doc, p)),
                )
            }
            (None, Some(doc)) => {
                self.states.retain_live(doc);
                (
                    self.focus.is_some_and(|f| !live(doc, f)),
                    self.hover.is_some_and(|h| !live(doc, h)),
                    self.press_control.is_some_and(|p| !live(doc, p)),
                    self.press.is_some_and(|(p, _)| !live(doc, p)),
                )
            }
            (None, None) => return,
        };
        if focus_lost {
            self.focus = None;
            self.focus_value = None;
            self.focus_ring.clear();
            self.caret_rect = None;
            self.states.set_single(None, ElementStates::FOCUS);
            self.states.set_single(None, ElementStates::FOCUS_VISIBLE);
            self.states.set_single(None, ElementStates::FOCUS_WITHIN);
        }
        if hover_lost {
            self.hover = None;
            self.states.set_single(None, ElementStates::HOVER);
            self.hover_dirty = true;
        }
        if press_control_lost {
            self.press_control = None;
        }
        if press_lost {
            self.press = None;
            self.states.set_single(None, ElementStates::ACTIVE);
        }
        if self.pointer_capture.is_some_and(|c| !self.live(c)) {
            self.pointer_capture = None;
            if let Some(host) = &mut self.script {
                host.clear_pointer_capture();
            }
        }
        if self.space_armed.is_some_and(|s| !self.live(s)) {
            self.space_armed = None;
        }
    }

    /// Make `sheets` match the document's `<style>` and `<link
    /// rel=stylesheet>` elements, in document order. Called when the
    /// document is set and after every script change to it: a slot whose
    /// element is unchanged is kept (with its imports and a fetch in
    /// flight), a new or changed element gets a new slot (an inline sheet
    /// is parsed, an external one fetched unless already loaded), and the
    /// slots of a removed element go.
    fn sync_stylesheets(&mut self) {
        enum Source {
            Inline(String),
            Link(Url),
        }
        let Some(doc) = &self.doc else { return };
        let base = doc.base_url.clone();
        let mut found: Vec<(NodeId, MediaQueryList, Source)> = Vec::new();
        for id in doc.descendants(doc.root()) {
            let Some(e) = doc.element(id) else { continue };
            if e.name.ns != ns!(html) {
                continue;
            }
            let media = parse_media_attr(e.attr("media"));
            if e.name.local == local_name!("style") {
                found.push((id, media, Source::Inline(doc.text_content(id))));
            } else if e.name.local == local_name!("link") {
                let rel = e.attr("rel").unwrap_or("");
                let is_sheet = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet"));
                let alternate = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("alternate"));
                if is_sheet && !alternate
                    && let Some(href) = e.attr("href")
                    && let Some(url) = doc.resolve_url(href)
                {
                    found.push((id, media, Source::Link(url)));
                }
            }
        }
        let mut by_source: std::collections::HashMap<NodeId, Vec<SheetSlot>> = std::collections::HashMap::new();
        for slot in std::mem::take(&mut self.sheets) {
            by_source.entry(slot.source).or_default().push(slot);
        }
        let mut changed = false;
        for (node, media, source) in found {
            // The element's own slot is the last of its group; its
            // imports come before it.
            let mut same = by_source.remove(&node).unwrap_or_default();
            let unchanged = same.last().is_some_and(|main| {
                main.media == media
                    && match &source {
                        Source::Inline(css) => main.url == base && main.text_hash == text_hash(css),
                        Source::Link(url) => main.url.as_ref() == Some(url),
                    }
            });
            if unchanged {
                self.sheets.append(&mut same);
                continue;
            }
            changed = true;
            match source {
                Source::Inline(css) => {
                    tracing::debug!(tab = self.id.0, "inline stylesheet:\n{css}");
                    let sheet = Stylesheet::parse_with_base(&css, Origin::Author, base.as_ref());
                    let slot = self.sheets.len();
                    self.sheets.push(SheetSlot {
                        source: node,
                        url: base.clone(),
                        media,
                        text_hash: text_hash(&css),
                        sheet: None,
                    });
                    let slot = slot + self.queue_imports(slot, &sheet, 0);
                    self.sheets[slot].sheet = Some(Arc::new(sheet));
                }
                Source::Link(url) => {
                    let slot = self.sheets.len();
                    self.sheets.push(SheetSlot {
                        source: node,
                        url: Some(url.clone()),
                        media,
                        text_hash: 0,
                        sheet: None,
                    });
                    self.load_external_sheet(slot, url, 0);
                }
            }
        }
        // Whatever is left belonged to elements that are gone (or were
        // replaced above); a fetch still in flight for them finds no
        // empty slot with its URL and is dropped.
        if !by_source.is_empty() {
            changed = true;
        }
        if changed {
            self.needs_style = true;
        }
    }

    /// Fill `slot` with the sheet at `url`: from the loaded sheets if it
    /// came before, else by fetching it (once; a second slot with the
    /// same URL is filled when the response comes).
    fn load_external_sheet(&mut self, slot: usize, url: Url, depth: u8) {
        if let Some(sheet) = self.loaded_sheets.get(&url).cloned() {
            let slot = slot + self.queue_imports(slot, &sheet, depth);
            self.sheets[slot].sheet = Some(sheet);
            return;
        }
        if self.fetched_urls.insert(url.clone()) {
            let request = self.subresource(url.clone());
            self.fetch(request, PendingKind::Stylesheet { url, depth });
        }
    }

    /// Insert slots for a sheet's `@import`s before it and fill or fetch
    /// them. Returns how many slots were inserted before `slot`.
    fn queue_imports(&mut self, slot: usize, sheet: &Stylesheet, depth: u8) -> usize {
        if depth >= MAX_IMPORT_DEPTH || sheet.imports.is_empty() {
            return 0;
        }
        let source = self.sheets[slot].source;
        let mut insert_at = slot;
        for (href, media) in &sheet.imports {
            let Ok(url) = Url::parse(href) else { continue };
            self.sheets.insert(
                insert_at,
                SheetSlot {
                    source,
                    url: Some(url.clone()),
                    media: media.clone(),
                    text_hash: 0,
                    sheet: None,
                },
            );
            let before = self.sheets.len();
            self.load_external_sheet(insert_at, url, depth + 1);
            // A loaded import may have inserted its own imports before it.
            insert_at += 1 + (self.sheets.len() - before);
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
        // A capture a listener asked for takes effect before the batch's
        // hover change is found, so the boundary events follow it.
        self.process_pointer_capture();
        self.settle();
        // The batch's move events, then `resize` and `scroll`: all run
        // after layout and may change the tree.
        let mut again = self.fire_pointer_move();
        again |= self.fire_view_events();
        if again {
            self.settle();
        }
        if self.needs_paint && self.layout.is_some() {
            self.repaint();
            self.needs_paint = false;
        }
        if self.state_dirty {
            self.send_state();
        }
    }

    /// Style and lay out until nothing is pending.
    fn settle(&mut self) {
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
        if self.find.is_some() {
            let prefer = self.current_match_start();
            self.refresh_find(prefer, false);
        }
        if self.focus.is_some() {
            self.focus_ring = self.compute_focus_ring();
            self.update_caret();
        }
    }

    fn repaint(&mut self) {
        let Some(tree) = &self.layout else { return };
        let options = PaintOptions {
            scroll_x: self.scroll_x,
            scroll_y: self.scroll_y,
            viewport_width: self.viewport.width,
            viewport_height: self.viewport.height,
            scale: self.viewport.scale_factor,
            selection: self.selection_ranges(),
            matches: self.find.as_ref().map(|f| f.ranges.clone()).unwrap_or_default(),
            current_match: self.find.as_ref().map(|f| f.current_ranges.clone()).unwrap_or_default(),
            focus_ring: self.focus_ring.clone(),
            caret: self.caret_rect,
        };
        paint(tree, &self.images, &options, &mut self.scene);
        if let Some(sb) = self.scrollbar() {
            let active = self.scroll_drag.is_some() || self.over_scrollbar();
            sb.draw(&mut self.scene, self.viewport.scale_factor, active);
        }
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

/// Whether an element can take focus at all: links, enabled form
/// controls (not hidden inputs), and anything with a `tabindex`.
fn is_focusable(e: &browser_dom::Element) -> bool {
    if e.name.ns != ns!(html) {
        return false;
    }
    let control = matches!(
        e.name.local,
        local_name!("input") | local_name!("button") | local_name!("select") | local_name!("textarea")
    ) && e.attr("disabled").is_none()
        && !(e.name.local == local_name!("input")
            && e.attr("type").is_some_and(|t| t.trim().eq_ignore_ascii_case("hidden")));
    is_link(e) || control || e.attr("tabindex").is_some()
}

/// The element's `tabindex`, if it has a valid one.
fn tabindex(e: &browser_dom::Element) -> Option<i64> {
    e.attr("tabindex")?.trim().parse().ok()
}

/// Pixels one arrow key scrolls.
const LINE_SCROLL: f32 = 40.0;

/// The HTML standard's JavaScript MIME type essences.
fn is_javascript_mime(essence: &str) -> bool {
    matches!(
        essence.trim().to_ascii_lowercase().as_str(),
        "application/ecmascript"
            | "application/javascript"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "text/javascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    )
}

/// What kind of script a `<script>` element is, per its `type` (and the
/// legacy `language`): `None` for data blocks, unknown types and classic
/// scripts marked `nomodule`.
fn script_kind(e: &browser_dom::Element) -> Option<ScriptKind> {
    let ty = e.attr("type").map(str::trim).unwrap_or("");
    let kind = if ty.is_empty() {
        match e.attr("language").map(str::trim).filter(|l| !l.is_empty()) {
            Some(lang) if !is_javascript_mime(&format!("text/{lang}")) => return None,
            _ => ScriptKind::Classic,
        }
    } else if is_javascript_mime(ty) {
        ScriptKind::Classic
    } else if ty.eq_ignore_ascii_case("module") {
        ScriptKind::Module
    } else {
        return None;
    };
    if kind == ScriptKind::Classic && e.attr("nomodule").is_some() {
        return None;
    }
    Some(kind)
}

/// An `<input>`'s type, lower-cased; `text` when absent. `None` for
/// anything that is not an input.
fn input_type(e: &browser_dom::Element) -> Option<String> {
    (e.name.ns == ns!(html) && e.name.local == local_name!("input"))
        .then(|| e.attr("type").map_or_else(|| "text".to_owned(), |t| t.trim().to_ascii_lowercase()))
}

fn is_textarea(e: &browser_dom::Element) -> bool {
    e.name.ns == ns!(html) && e.name.local == local_name!("textarea")
}

fn is_select(e: &browser_dom::Element) -> bool {
    e.name.ns == ns!(html) && e.name.local == local_name!("select")
}

/// A control whose value is typed: a textarea or a text-like input.
fn is_text_control(e: &browser_dom::Element) -> bool {
    is_textarea(e)
        || input_type(e).is_some_and(|t| {
            !matches!(
                t.as_str(),
                "checkbox" | "radio" | "submit" | "button" | "reset" | "image" | "file" | "hidden" | "range" | "color"
            )
        })
}

/// A checkbox or radio button.
fn is_toggle(e: &browser_dom::Element) -> bool {
    input_type(e).is_some_and(|t| t == "checkbox" || t == "radio")
}

/// A `<button>` or a button-like input: Enter and Space click it.
fn is_button(e: &browser_dom::Element) -> bool {
    e.name.ns == ns!(html)
        && (e.name.local == local_name!("button")
            || input_type(e).is_some_and(|t| matches!(t.as_str(), "button" | "submit" | "reset" | "image")))
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
            let mut h = Self::start(url);
            h.pump();
            h
        }

        /// Start a navigation without delivering any of its events yet.
        fn start(url: Url) -> Self {
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
            h.state.handle_shell(ShellToTab::Navigate { url });
            h
        }

        /// Deliver exactly `n` queued net events, then do the work due.
        fn pump_n(&mut self, n: usize) {
            for _ in 0..n {
                let ev = self.net_events.try_recv().expect("a queued net event");
                self.state.handle_net(ev);
            }
            self.state.tick();
            self.state.flush();
        }

        /// Keep delivering net events, waiting for the network, until the
        /// condition holds. Panics after ten seconds.
        fn pump_until(&mut self, mut done: impl FnMut(&Self) -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                self.pump();
                if done(self) {
                    return;
                }
                let wait = deadline.saturating_duration_since(std::time::Instant::now());
                match self.net_events.recv_timeout(wait) {
                    Ok(ev) => self.state.handle_net(ev),
                    Err(_) => panic!("timed out waiting for the tab"),
                }
            }
        }

        /// Deliver whatever the network sends for a while.
        fn pump_for(&mut self, time: std::time::Duration) {
            let deadline = std::time::Instant::now() + time;
            loop {
                self.pump();
                let wait = deadline.saturating_duration_since(std::time::Instant::now());
                if wait.is_zero() {
                    return;
                }
                match self.net_events.recv_timeout(wait) {
                    Ok(ev) => self.state.handle_net(ev),
                    Err(_) => return,
                }
            }
        }

        fn send(&mut self, msg: ShellToTab) {
            self.state.handle_shell(msg);
            self.pump();
        }

        /// Deliver what the net answered and do the work due, as the tab
        /// loop does after each batch.
        fn pump(&mut self) {
            // A navigation started by a timer or a click queues more net
            // events; keep going until a round delivers none. Timers run
            // first so what they start is delivered in the same call.
            loop {
                self.state.tick();
                self.state.flush();
                let mut delivered = false;
                while let Ok(ev) = self.net_events.try_recv() {
                    delivered = true;
                    self.state.handle_net(ev);
                }
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
    fn scrollbar_pages_drags_and_covers_the_page() {
        let mut h = Harness::load(TALL);
        let sb = h.state.scrollbar().expect("tall page has a scrollbar");
        let x = (sb.track.x0 + sb.track.x1) as f32 / 2.0;
        // Click on the track below the thumb: a page down.
        h.send(ShellToTab::MouseDown { x, y: 500.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y: 500.0, button: MouseButton::Left });
        assert_eq!(h.state.scroll_y, 540.0);
        assert!(h.state.hover.is_none(), "the link under the track was not hit");
        // Drag the thumb back to the top.
        let thumb = h.state.scrollbar().expect("bar").thumb();
        let grab_y = (thumb.y0 + 5.0) as f32;
        h.send(ShellToTab::MouseDown { x, y: grab_y, button: MouseButton::Left });
        h.send(ShellToTab::MouseMove { x, y: 5.0 });
        assert_eq!(h.state.scroll_y, 0.0);
        h.send(ShellToTab::MouseMove { x, y: 5.0 + 100.0 });
        assert!(h.state.scroll_y > 0.0 && h.state.scroll_y < 1500.0, "{}", h.state.scroll_y);
        h.send(ShellToTab::MouseUp { x, y: 105.0, button: MouseButton::Left });
        assert!(h.state.scroll_drag.is_none());
        // A short page has no bar.
        let h2 = Harness::load("<p>short</p>");
        assert!(h2.state.scrollbar().is_none());
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

    const TEXT_PAGE: &str = "<!doctype html><style>body { margin: 0; font-size: 16px; line-height: 20px } p { margin: 0 }</style>\
        <p>Hello <a href='#x'>link</a> world</p><p>Second paragraph</p>";

    impl Harness {
        fn copied(&self) -> Vec<String> {
            self.messages
                .lock()
                .expect("lock")
                .iter()
                .filter_map(|m| match m {
                    TabToShell::CopyText { text } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }

        fn selected(&self) -> Option<String> {
            self.state.selected_text()
        }
    }

    #[test]
    fn dragging_selects_text_and_copy_sends_it_to_the_shell() {
        let mut h = Harness::load(TEXT_PAGE);
        // Nothing to copy yet.
        h.send(ShellToTab::Copy);
        assert!(h.copied().is_empty());

        // Press left of the first line, drag to the end of the second.
        h.send(ShellToTab::MouseDown { x: -5.0, y: 10.0, button: MouseButton::Left });
        assert!(h.state.select_anchor.is_some());
        assert_eq!(h.selected(), None, "a press alone selects nothing");
        h.send(ShellToTab::MouseMove { x: 790.0, y: 30.0 });
        assert_eq!(h.selected().as_deref(), Some("Hello link world\nSecond paragraph"));
        h.send(ShellToTab::MouseUp { x: 790.0, y: 30.0, button: MouseButton::Left });
        assert!(h.state.select_anchor.is_none());
        assert_eq!(h.selected().as_deref(), Some("Hello link world\nSecond paragraph"), "the selection outlives the drag");
        assert!(!h.state.selection_ranges().is_empty(), "the painter is told what to highlight");

        h.send(ShellToTab::Copy);
        assert_eq!(h.copied(), vec!["Hello link world\nSecond paragraph".to_owned()]);

        // Scrolling keeps it; a click elsewhere clears it.
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 10.0 });
        assert!(h.selected().is_some());
        h.send(ShellToTab::MouseDown { x: 400.0, y: 400.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x: 400.0, y: 400.0, button: MouseButton::Left });
        assert_eq!(h.selected(), None);
        h.send(ShellToTab::Copy);
        assert_eq!(h.copied().len(), 1, "nothing selected, nothing sent");
    }

    #[test]
    fn select_all_and_multi_click() {
        let mut h = Harness::load(TEXT_PAGE);
        h.send(ShellToTab::SelectAll);
        assert_eq!(h.selected().as_deref(), Some("Hello link world\nSecond paragraph"));

        // Double click on "Second": the word. Triple: the paragraph.
        let (x, y) = h.center("p");
        let x = x.min(20.0);
        let y = y + 20.0;
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(h.selected(), None, "one click clears the selection");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(h.selected().as_deref(), Some("Second"));
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(h.selected().as_deref(), Some("Second paragraph"));

        // A press on a link does not start a drag selection.
        let (lx, ly) = h.center("a");
        h.send(ShellToTab::MouseDown { x: lx, y: ly, button: MouseButton::Left });
        assert!(h.state.select_anchor.is_none());
        assert_eq!(h.selected(), None);
        h.send(ShellToTab::MouseMove { x: 790.0, y: 30.0 });
        assert_eq!(h.selected(), None);
        h.send(ShellToTab::MouseUp { x: 790.0, y: 30.0, button: MouseButton::Left });

        // A new document drops the selection.
        h.send(ShellToTab::SelectAll);
        assert!(h.selected().is_some());
        h.send(ShellToTab::Navigate { url: data_url("<p>other</p>", None) });
        assert_eq!(h.selected(), None);
    }

    impl Harness {
        /// The last (current, total) the tab reported for find.
        fn find_result(&self) -> Option<(Option<usize>, usize)> {
            self.messages
                .lock()
                .expect("lock")
                .iter()
                .rev()
                .find_map(|m| match m {
                    TabToShell::FindResult { current, total } => Some((*current, *total)),
                    _ => None,
                })
        }

        fn current_match_text(&self) -> Option<String> {
            let f = self.state.find.as_ref()?;
            let (a, b) = f.matches.get(f.current?)?;
            Some(selection::selection_text(self.state.layout.as_ref()?, *a, *b))
        }
    }

    #[test]
    fn find_highlights_steps_wraps_and_scrolls() {
        let mut h = Harness::load(TALL);
        h.send(ShellToTab::Find { query: "TOP".into() });
        assert_eq!(h.find_result(), Some((Some(1), 1)), "case-insensitive");
        assert_eq!(h.current_match_text().as_deref(), Some("top"));
        assert!(!h.state.find.as_ref().expect("find").ranges.is_empty());
        assert_eq!(h.state.scroll_y, 0.0, "already in view");

        // "end" is in the heading far below: the page scrolls to it.
        h.send(ShellToTab::Find { query: "end".into() });
        assert_eq!(h.find_result(), Some((Some(1), 1)));
        assert!(h.state.scroll_y > 900.0, "scrolled to the match, got {}", h.state.scroll_y);

        // Two matches of "o": down, top. Next wraps; previous goes back.
        h.send(ShellToTab::Find { query: "o".into() });
        assert_eq!(h.find_result(), Some((Some(1), 2)));
        assert!(h.state.scroll_y < 100.0, "scrolled back up to the first match");
        h.send(ShellToTab::FindNext { forward: true });
        assert_eq!(h.find_result(), Some((Some(2), 2)));
        h.send(ShellToTab::FindNext { forward: true });
        assert_eq!(h.find_result(), Some((Some(1), 2)), "wrapped");
        h.send(ShellToTab::FindNext { forward: false });
        assert_eq!(h.find_result(), Some((Some(2), 2)));
        assert_eq!(h.current_match_text().as_deref(), Some("o"));

        // A new query keeps the place: the first match at or after the
        // current one's start. After the o of "top" the next t is "The".
        h.send(ShellToTab::Find { query: "t".into() });
        assert_eq!(h.find_result(), Some((Some(2), 2)));
        assert_eq!(h.current_match_text().as_deref(), Some("T"));
        h.send(ShellToTab::Find { query: "th".into() });
        assert_eq!(h.find_result(), Some((Some(1), 1)));
        assert_eq!(h.current_match_text().as_deref(), Some("Th"));

        // No match, empty query, close.
        h.send(ShellToTab::Find { query: "zzz".into() });
        assert_eq!(h.find_result(), Some((None, 0)));
        h.send(ShellToTab::FindNext { forward: true });
        assert_eq!(h.find_result(), Some((None, 0)));
        h.send(ShellToTab::Find { query: String::new() });
        assert!(h.state.find.is_none());
        h.send(ShellToTab::Find { query: "top".into() });
        h.send(ShellToTab::FindClose);
        assert!(h.state.find.is_none());
    }

    #[test]
    fn find_follows_the_page_across_a_navigation() {
        let mut h = Harness::load(TEXT_PAGE);
        h.send(ShellToTab::Find { query: "paragraph".into() });
        assert_eq!(h.find_result(), Some((Some(1), 1)));
        h.send(ShellToTab::Navigate { url: data_url("<p>a paragraph and a paragraph</p>", None) });
        assert_eq!(h.find_result(), Some((Some(1), 2)), "found again on the new page");
        h.send(ShellToTab::Navigate { url: data_url("<p>nothing here</p>", None) });
        assert_eq!(h.find_result(), Some((None, 0)));
    }

    impl Harness {
        fn by_id(&self, id: &str) -> NodeId {
            let doc = self.state.doc.as_ref().expect("document");
            doc.descendants(doc.root())
                .find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))
                .expect("element with id")
        }

        fn key(&mut self, key: Key, shift: bool) {
            self.send(ShellToTab::Key {
                key,
                code: String::new(),
                repeat: false,
                shift,
                ctrl: false,
                alt: false,
            });
        }

        fn key_up(&mut self, key: Key, shift: bool) {
            self.send(ShellToTab::KeyUp {
                key,
                code: String::new(),
                shift,
                ctrl: false,
                alt: false,
            });
        }

        fn focus_id(&self) -> Option<String> {
            let doc = self.state.doc.as_ref()?;
            doc.element(self.state.focus?)?.id().map(str::to_owned)
        }

        fn focus_outs(&self) -> Vec<bool> {
            self.messages
                .lock()
                .expect("lock")
                .iter()
                .filter_map(|m| match m {
                    TabToShell::FocusOut { forward } => Some(*forward),
                    _ => None,
                })
                .collect()
        }

        fn color_of(&self, id: &str) -> [u8; 4] {
            self.state.styles[self.by_id(id)].color.to_rgba8()
        }
    }

    const FOCUS_PAGE: &str = "<!doctype html><style>body { margin: 0; line-height: 20px; color: black } \
        :focus-visible { color: red } div { height: 1500px }</style>\
        <p><a id=a1 href='#x'>one</a> <a id=a2 href='#y' tabindex='-1'>two</a> \
        <span id=s tabindex='0'>three</span> <a id=a3 href='#z' tabindex='2'>four</a> \
        <b id=b tabindex='1'>five</b> <a id=hidden href='#h' style='display:none'>hidden</a> \
        <input id=hid type=hidden> <input id=dis disabled></p>\
        <div></div><p><a id=last href='#last'>last</a></p>";

    #[test]
    fn tab_walks_the_focus_order_shows_a_ring_and_leaves_the_page_at_the_ends() {
        let mut h = Harness::load(FOCUS_PAGE);
        // Positive tabindex first, ascending, then tree order; hidden,
        // disabled and negative-tabindex elements are skipped.
        let expected = ["b", "a3", "a1", "s", "last"];
        for id in expected {
            h.key(Key::Tab, false);
            assert_eq!(h.focus_id().as_deref(), Some(id));
            assert_eq!(h.color_of(id), [255, 0, 0, 255], ":focus-visible on {id}");
            assert!(!h.state.focus_ring.is_empty(), "a ring on {id}");
        }
        assert!(h.state.scroll_y > 900.0, "the last link was scrolled into view");
        assert!(h.focus_outs().is_empty());
        h.key(Key::Tab, false);
        assert_eq!(h.focus_id(), None);
        assert_eq!(h.focus_outs(), vec![true], "past the end, focus leaves the page");
        assert!(h.state.focus_ring.is_empty());

        // Shift+Tab from nothing starts at the last; keeps going back.
        h.key(Key::Tab, true);
        assert_eq!(h.focus_id().as_deref(), Some("last"));
        h.key(Key::Tab, true);
        assert_eq!(h.focus_id().as_deref(), Some("s"));
        assert!(h.state.scroll_y < 100.0, "scrolled back up to it");
        for _ in 0..3 {
            h.key(Key::Tab, true);
        }
        assert_eq!(h.focus_id().as_deref(), Some("b"));
        h.key(Key::Tab, true);
        assert_eq!(h.focus_outs(), vec![true, false]);

        // A click focuses without the ring or :focus-visible; Tab goes
        // on from there. A negative tabindex is click-focusable and Tab
        // continues from its place in the tree.
        h.click("a");
        assert_eq!(h.focus_id().as_deref(), Some("a1"));
        assert_ne!(h.color_of("a1"), [255, 0, 0, 255], "no :focus-visible from a click");
        assert!(h.state.focus_ring.is_empty());
        h.key(Key::Tab, false);
        assert_eq!(h.focus_id().as_deref(), Some("s"));
        let a2 = h.by_id("a2");
        let r = h.rect_of(a2);
        let (x, y) = (r.x + r.width / 2.0, r.y + r.height / 2.0);
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(h.focus_id().as_deref(), Some("a2"));
        h.key(Key::Tab, true);
        assert_eq!(h.focus_id().as_deref(), Some("a1"), "the element before a2 in the order");
        h.key(Key::Tab, true);
        assert_eq!(h.focus_id().as_deref(), Some("a3"));

        // Enter follows a focused link; on a plain tabindex element it
        // does nothing.
        h.key(Key::Enter, false);
        assert_eq!(h.state.url.as_ref().and_then(|u| u.fragment()), Some("z"));
        h.key(Key::Tab, false);
        h.key(Key::Tab, false);
        assert_eq!(h.focus_id().as_deref(), Some("s"));
        h.key(Key::Enter, false);
        assert_eq!(h.state.url.as_ref().and_then(|u| u.fragment()), Some("z"));

        // A new document has no focus.
        h.send(ShellToTab::Navigate { url: data_url("<p>plain</p>", None) });
        assert_eq!(h.state.focus, None);
        h.key(Key::Tab, false);
        assert_eq!(h.focus_outs(), vec![true, false, true], "nothing focusable: straight out");
    }

    const FORM: &str = "<!doctype html><style>body { margin: 0; color: black } input:checked + span { color: red }</style>\
        <p><input id=t value='ab'> <input id=pw type=password> <textarea id=ta>hi</textarea></p>\
        <p><input id=c type=checkbox><span id=cs>c</span> <input id=r1 type=radio name=g checked>\
        <input id=r2 type=radio name=g> <select id=sel><option>One<option>Two<option>Three</select> \
        <input id=d type=checkbox disabled></p>";

    impl Harness {
        fn has_attr(&self, id: &str, attr: &str) -> bool {
            let doc = self.state.doc.as_ref().expect("document");
            doc.element(self.by_id(id)).is_some_and(|e| e.attr(attr).is_some())
        }

        fn attr(&self, id: &str, attr: &str) -> Option<String> {
            let doc = self.state.doc.as_ref().expect("document");
            doc.element(self.by_id(id))?.attr(attr).map(str::to_owned)
        }

        /// Click near the right end of an element's first box.
        fn click_end_of(&mut self, id: &str) {
            let r = self.rect_of(self.by_id(id));
            let (x, y) = (r.right() - 6.0, r.y + r.height / 2.0);
            self.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
            self.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        }

        fn click_id(&mut self, id: &str) {
            let r = self.rect_of(self.by_id(id));
            let (x, y) = (r.x + r.width / 2.0, r.y + r.height / 2.0);
            self.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
            self.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        }

        fn typed(&mut self, s: &str) {
            for ch in s.chars() {
                self.key(Key::Character(ch.to_string()), false);
            }
        }

        /// The text laid out for a control (its value, placeholder or
        /// chosen option).
        fn control_text(&self, id: &str) -> String {
            let node = self.by_id(id);
            let mut out = String::new();
            self.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
                if let browser_layout::FragmentContent::Text(t) = &f.content
                    && f.node == Some(node)
                {
                    out.push_str(&t.text[t.range.clone()]);
                }
            });
            out
        }
    }

    #[test]
    fn form_controls_toggle_edit_and_step() {
        let mut h = Harness::load(FORM);
        // Checkbox: a click toggles, and `:checked` restyles the sibling.
        h.click_id("c");
        assert!(h.has_attr("c", "checked"));
        assert_eq!(h.color_of("cs"), [255, 0, 0, 255]);
        h.click_id("c");
        assert!(!h.has_attr("c", "checked"));
        assert_eq!(h.color_of("cs"), [0, 0, 0, 255]);
        // Space on the focused checkbox toggles too, on release as in
        // browsers; a disabled one never.
        h.key(Key::Space, false);
        assert!(!h.has_attr("c", "checked"), "not before the key goes up");
        h.key_up(Key::Space, false);
        assert!(h.has_attr("c", "checked"));
        h.click_id("d");
        assert!(!h.has_attr("d", "checked"));
        // Radios: one in the group at a time.
        h.click_id("r2");
        assert!(h.has_attr("r2", "checked") && !h.has_attr("r1", "checked"));
        h.click_id("r1");
        assert!(h.has_attr("r1", "checked") && !h.has_attr("r2", "checked"));

        // Text input: the click puts the caret at the end; keys edit.
        assert_eq!(h.control_text("t"), "ab");
        h.click_end_of("t");
        assert_eq!(h.focus_id().as_deref(), Some("t"));
        assert_eq!(h.state.caret, 2);
        let caret0 = h.state.caret_rect.expect("caret shown");
        h.typed("x");
        assert_eq!(h.attr("t", "value").as_deref(), Some("abx"));
        assert_eq!(h.control_text("t"), "abx");
        assert!(h.state.caret_rect.expect("caret").x > caret0.x, "the caret moved right");
        h.key(Key::Backspace, false);
        h.key(Key::Backspace, false);
        h.key(Key::ArrowLeft, false);
        h.typed("z");
        assert_eq!(h.attr("t", "value").as_deref(), Some("za"));
        h.key(Key::Home, false);
        h.key(Key::Delete, false);
        h.key(Key::End, false);
        h.key(Key::Space, false);
        assert_eq!(h.attr("t", "value").as_deref(), Some("a "));
        assert_eq!(h.control_text("t"), "a ", "spaces are kept");
        h.key(Key::Enter, false);
        assert_eq!(h.attr("t", "value").as_deref(), Some("a "), "Enter does nothing in an input yet");
        assert_eq!(h.state.history.len(), 1, "and does not navigate");

        // Password shows bullets; a textarea takes newlines.
        h.click_id("pw");
        h.typed("pq");
        assert_eq!(h.attr("pw", "value").as_deref(), Some("pq"));
        assert_eq!(h.control_text("pw"), "\u{2022}\u{2022}");
        h.click_end_of("ta");
        h.key(Key::End, false);
        h.key(Key::Enter, false);
        h.typed("y");
        let doc = h.state.doc.as_ref().expect("doc");
        assert_eq!(doc.text_content(h.by_id("ta")), "hi\ny");
        assert!(h.state.caret_rect.is_some_and(|c| c.y > 0.0), "the caret is on the second line");

        // Select: arrows step the chosen option, and it shows.
        assert_eq!(h.control_text("sel"), "One");
        h.click_id("sel");
        h.key(Key::ArrowDown, false);
        assert_eq!(h.control_text("sel"), "Two");
        h.key(Key::ArrowDown, false);
        h.key(Key::ArrowDown, false);
        assert_eq!(h.control_text("sel"), "Three", "stops at the last");
        h.key(Key::ArrowUp, false);
        assert_eq!(h.control_text("sel"), "Two");
        assert_eq!(h.state.scroll_y, 0.0, "arrows on a select do not scroll");
    }

    #[test]
    fn inline_scripts_run_and_their_timers_and_frames_fire() {
        let mut h = Harness::load(
            "<title>JS</title><script>var marker = 1; console.log('hi'); \
             setTimeout(() => console.log('later'), 1); \
             requestAnimationFrame(t => console.log('frame ' + (t >= 0)));</script>\
             <p>x</p><script type=module>console.log('no modules yet')</script>\
             <script type='text/javascript'>console.warn('second'); Promise.resolve().then(() => console.log('micro'));</script>\
             <script>oops(</script>",
        );
        let texts = |h: &Harness| h.state.console.iter().map(|l| l.text.clone()).collect::<Vec<String>>();
        let t = texts(&h);
        assert_eq!(&t[..3], ["hi", "second", "micro"], "{t:?}");
        assert!(t[3].starts_with("Uncaught SyntaxError"), "{t:?}");
        // The inline module is deferred: it runs once the document has
        // parsed, after every classic script.
        assert_eq!(t[4], "no modules yet", "{t:?}");
        assert_eq!(h.state.console[1].level, ConsoleLevel::Warn);
        assert!(h.state.next_wake().is_some(), "the timeout is armed");
        assert!(h.state.frame_due.is_some(), "a frame is scheduled");

        std::thread::sleep(std::time::Duration::from_millis(3));
        h.pump();
        assert!(texts(&h).contains(&"later".to_owned()));
        h.state.frame_due = Some(std::time::Instant::now());
        h.pump();
        assert_eq!(texts(&h).last().map(String::as_str), Some("frame true"));
        assert!(h.state.frame_due.is_none(), "no more callbacks, no more frames");

        // A new document gets a fresh context.
        h.send(ShellToTab::Navigate {
            url: data_url("<script>console.log(typeof marker)</script>", None),
        });
        assert_eq!(texts(&h).last().map(String::as_str), Some("undefined"));
    }

    #[test]
    fn window_bindings_see_the_document_and_drive_it() {
        let target = data_url("<title>Second</title><script>console.log(document.readyState, document.title)</script>", None);
        let html = format!(
            "<title>First</title>\
             <script>console.log(document.readyState, document.title, location.protocol, typeof navigator.userAgent, window === self); document.title = 'Renamed';</script>\
             <script defer src=\"data:text/javascript,console.log(document.readyState)\"></script>\
             <script>setTimeout(() => location.assign('{target}'), 50)</script>"
        );
        let mut h = Harness::load(&html);
        assert_eq!(h.title().as_deref(), Some("Renamed"), "the title set from script reaches the document");
        let t = console_texts(&h);
        assert_eq!(t[0], "loading First data: string true");
        assert_eq!(t[1], "interactive", "a deferred script sees the document parsed but not complete");
        std::thread::sleep(std::time::Duration::from_millis(60));
        h.pump();
        assert_eq!(h.title().as_deref(), Some("Second"), "location.assign navigated");
        assert_eq!(console_texts(&h).last().map(String::as_str), Some("loading Second"));
        assert_eq!(h.state.history.len(), 2, "assign pushed an entry");
    }

    #[test]
    fn dom_bindings_see_the_tree_while_parsing_and_after() {
        // The inline script runs with the parser blocked on it: the tree
        // ends at the script element. The deferred script and the timer
        // see the finished document, through the same wrappers.
        let html = "<!DOCTYPE html><title>Tree</title><body class='a b'><p id=first>one</p>\
            <script>var firstP = document.body.firstElementChild; \
            console.log([document.body.childNodes.length, document.body.lastChild.tagName, document.body.lastChild.previousSibling.id, String(document.body.nextSibling), document.body.classList.contains('b'), document.readyState, document.documentElement.parentNode === document].join('|'))</script>\
            <p id=second>two</p>\
            <script defer src='data:text/javascript,console.log(document.body.children.length, document.body.children[2].id, document.readyState)'></script>\
            <script>setTimeout(() => console.log(document.body.children.length, document.readyState, document.body.firstElementChild === firstP, firstP.textContent), 0)</script>";
        let mut h = Harness::load(html);
        assert_eq!(h.title().as_deref(), Some("Tree"));
        std::thread::sleep(std::time::Duration::from_millis(2));
        h.pump();
        assert_eq!(
            console_texts(&h),
            ["2|SCRIPT|first|null|true|loading|true", "5 second interactive", "5 complete true one"]
        );
    }

    #[test]
    fn dom_mutation_from_script_restyles_relays_out_and_drops_stale_focus() {
        // While parsing: emptying the body detaches the running script's
        // element without freeing it, and the parser goes on under body.
        let html = "<!DOCTYPE html><title>Mut</title><style>.big { height: 100px } p { height: 10px }</style>\
            <body><p>gone</p><script>document.body.textContent = ''</script><p id=kept>kept</p>\
            <a id=link href='#x'>link</a><div id=root></div>\
            <script>setTimeout(() => { \
                var p = document.createElement('p'); p.textContent = 'two'; p.classList.add('big'); \
                document.getElementById === undefined; \
                var root = document.body.children[2]; root.appendChild(p); \
                document.body.firstElementChild.remove(); \
                document.body.children[0].remove(); \
                document.title = 'Changed'; \
                console.log(document.body.children.length, root.firstChild.tagName, root.firstChild.className); }, 30)</script>";
        let mut h = Harness::load(html);
        // After the parse: the first <p> and the first script are gone, the
        // rest was built under the (now empty) body.
        {
            let doc = h.state.doc.as_ref().expect("document");
            let body = doc.body().expect("body");
            let tags: Vec<String> = doc
                .children(body)
                .filter_map(|c| doc.element(c).map(|e| e.name.local.to_string()))
                .collect();
            assert_eq!(tags, ["p", "a", "div", "script"]);
        }
        // Focus the link from the keyboard, then let the timer remove it.
        h.key(Key::Tab, false);
        assert_eq!(h.state.focus, Some(h.by_id("link")));
        assert!(!h.state.focus_ring.is_empty());
        std::thread::sleep(std::time::Duration::from_millis(40));
        h.pump();
        assert_eq!(console_texts(&h).last().map(String::as_str), Some("2 P big"));
        assert_eq!(h.state.focus, None, "the focused link left the document");
        assert!(h.state.focus_ring.is_empty());
        assert!(!h.state.needs_style && !h.state.needs_layout, "the flush ran");
        assert_eq!(h.title().as_deref(), Some("Changed"));
        let root = h.by_id("root");
        let p = {
            let doc = h.state.doc.as_ref().expect("document");
            doc.children(root).next().expect("the appended p")
        };
        assert_eq!(h.rect_of(p).height, 100.0, "the new element was styled and laid out");
        {
            let doc = h.state.doc.as_ref().expect("document");
            let body = doc.body().expect("body");
            assert_eq!(doc.children(body).count(), 2, "the removed elements are out of the tree");
            assert!(doc.descendants(doc.root()).all(|n| doc.element(n).is_none_or(|e| e.id() != Some("link"))));
        }
    }

    #[test]
    fn dom_lookups_and_inner_html_from_script_reach_the_page() {
        // A script finds the hovered link through `:hover`, rewrites a
        // container through innerHTML and the new markup is styled and
        // laid out; dataset reads what the markup carried.
        let html = "<!DOCTYPE html><title>Q</title><style>.big { height: 100px } p { height: 10px }</style>\
            <body><a id=link href='#x'>link</a><div id=root><p>old</p></div>\
            <script>setTimeout(() => { \
                var hovered = document.querySelector('a:hover'); \
                var root = document.getElementById('root'); \
                root.innerHTML = '<p class=big data-size=\"L\">new</p><p>more</p>'; \
                console.log(hovered ? hovered.id : 'none', root.children.length, root.querySelector('.big').dataset.size, document.getElementsByTagName('p').length); }, 30)</script>";
        let mut h = Harness::load(html);
        let (x, y) = h.center("a");
        h.send(ShellToTab::MouseMove { x, y });
        assert_eq!(h.state.hover, Some(h.by_id("link")));
        std::thread::sleep(std::time::Duration::from_millis(40));
        h.pump();
        assert_eq!(console_texts(&h).last().map(String::as_str), Some("link 2 L 2"));
        let root = h.by_id("root");
        let first = {
            let doc = h.state.doc.as_ref().expect("document");
            doc.children(root).next().expect("p")
        };
        assert_eq!(h.rect_of(first).height, 100.0, "the markup from innerHTML was styled and laid out");
    }

    #[test]
    fn stylesheets_added_changed_and_removed_by_script_take_effect() {
        // A <style> appended by script styles the page; editing its text
        // restyles; removing it drops its rules; an appended <link> is
        // fetched and applied; the imported sheet of a parsed <style>
        // survives an unrelated DOM change.
        let page = "<!DOCTYPE html><title>CSS</title><style>@import url(/imp.css);</style><body><p id=p>x</p><div id=q>y</div>\
            <script>var s; \
              setTimeout(() => { s = document.createElement('style'); s.textContent = '#p { height: 100px }'; document.head.appendChild(s); }, 10); \
              setTimeout(() => { s.textContent = '#p { height: 50px }'; }, 60); \
              setTimeout(() => { s.remove(); var l = document.createElement('link'); l.setAttribute('rel', 'stylesheet'); l.setAttribute('href', '/ext.css'); document.head.appendChild(l); }, 110); \
            </script>";
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/imp.css", "text/css", "#q { height: 30px }"),
                ("/ext.css", "text/css", "#p { height: 70px }"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.doc.is_some() && !h.state.is_loading());
        let p = h.by_id("p");
        let q = h.by_id("q");
        let plain = h.rect_of(p).height;
        assert!(plain != 100.0 && plain != 50.0 && plain != 70.0);
        assert_eq!(h.rect_of(q).height, 30.0, "the imported sheet applies");
        std::thread::sleep(std::time::Duration::from_millis(25));
        h.pump();
        assert_eq!(h.rect_of(p).height, 100.0, "the appended style applies");
        assert_eq!(h.rect_of(q).height, 30.0, "the import is kept across the change");
        std::thread::sleep(std::time::Duration::from_millis(50));
        h.pump();
        assert_eq!(h.rect_of(p).height, 50.0, "the edited style text applies");
        std::thread::sleep(std::time::Duration::from_millis(50));
        h.pump();
        h.pump_until(|h| h.rect_of(p).height == 70.0);
        assert_eq!(h.rect_of(q).height, 30.0);
        assert_eq!(h.state.sheets.len(), 3, "import, its style, the link");
    }

    #[test]
    fn lifecycle_events_fire_in_order_and_load_waits_for_fetches() {
        // readystatechange (interactive) before deferred scripts, then
        // DOMContentLoaded (bubbling to window), then once the held image
        // arrives: readystatechange (complete) and load on window.
        let page = "<!DOCTYPE html><title>Life</title>\
            <script>var log = []; var note = (n) => () => log.push(n + ':' + document.readyState); \
              document.addEventListener('readystatechange', note('rsc')); document.addEventListener('DOMContentLoaded', note('dcl')); \
              window.addEventListener('DOMContentLoaded', note('dcl@window')); window.addEventListener('load', ev => log.push('load:' + document.readyState + ':' + (ev.target === window) + ':' + ev.bubbles)); \
              document.onreadystatechange = note('onrsc'); console.log('inline:' + document.readyState)</script>\
            <script defer src='/d.js'></script><body><img src='/img.png'>\
            <script>setTimeout(() => console.log(log.join(' ')), 20)</script>";
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/d.js", "text/javascript", "console.log('defer:' + document.readyState)"),
                ("/img.png", "image/png", ""),
            ],
            &["/img.png"],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.doc.is_some() && h.state.scripts.deferred.is_empty());
        assert_eq!(console_texts(&h), ["inline:loading", "defer:interactive"]);
        assert_eq!(h.state.lifecycle, Lifecycle::ContentLoaded);
        std::thread::sleep(std::time::Duration::from_millis(25));
        h.pump();
        assert_eq!(
            console_texts(&h).last().map(String::as_str),
            Some("rsc:interactive onrsc:interactive dcl:interactive dcl@window:interactive"),
            "no load while the image is held"
        );
        server.release("/img.png");
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        assert_eq!(
            h.state.script.as_mut().and_then(|s| s.eval_to_string("log.slice(4).join(' ')").ok()).as_deref(),
            Some("\"rsc:complete onrsc:complete load:complete:true:false\"")
        );
    }

    #[test]
    fn document_reports_its_encoding_mode_and_domain() {
        // No doctype puts the document in quirks mode; the meta charset
        // is honoured by the prescan. The inline script reads through the
        // blocked parser, the deferred one through the finished document.
        let probe = "console.log(document.characterSet, document.inputEncoding, document.compatMode, JSON.stringify(document.domain), JSON.stringify(document.referrer))";
        let html = format!(
            "<meta charset=windows-1252><script>{probe}</script>\
             <script defer src=\"data:text/javascript,{probe}\"></script>"
        );
        let h = Harness::load(&html);
        let t = console_texts(&h);
        assert_eq!(t, ["windows-1252 windows-1252 BackCompat \"\" \"\"", "windows-1252 windows-1252 BackCompat \"\" \"\""]);

        let h = Harness::load(&format!("<!DOCTYPE html><script>{probe}</script>"));
        assert_eq!(console_texts(&h), ["UTF-8 UTF-8 CSS1Compat \"\" \"\""]);
    }

    #[test]
    fn location_replace_overwrites_the_entry_without_bypassing_the_cache() {
        let target = data_url("<title>Replaced</title>", None);
        let mut h = Harness::load(&format!("<title>First</title><script>setTimeout(() => location.replace('{target}'), 0)</script>"));
        assert_eq!(h.state.history.len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(2));
        h.pump();
        assert_eq!(h.title().as_deref(), Some("Replaced"));
        assert_eq!(h.state.history, vec![target], "replace overwrote the only entry");
        assert_eq!(h.state.doc_cache, CacheMode::Default, "a replace is not a reload");
    }

    /// Evaluate `src` in the page's context (no DOM access: the document
    /// is not lent).
    fn js(h: &mut Harness, src: &str) -> String {
        h.state.script.as_mut().expect("host").eval_to_string(src).expect("eval")
    }

    #[test]
    fn mouse_events_fire_in_order_with_cancellable_defaults() {
        let other = data_url("<title>Other</title>", None);
        let html = format!(
            "<!DOCTYPE html><title>Mouse</title><style>body {{ margin: 0 }} #box {{ height: 40px }} p {{ margin: 0; height: 20px }} #tall {{ height: 2000px }}</style>\
             <div id=box><a id=l href='{other}'>link</a></div><p><input type=checkbox id=c></p><div id=tall></div>\
             <script>\
               var log = []; var cancel = {{}}; \
               var l = document.getElementById('l'), box = document.getElementById('box'), c = document.getElementById('c'); \
               var note = ev => log.push(ev.type + ':' + ev.target.id + ':' + ev.button + ':' + ev.detail + ':' + ev.isTrusted); \
               ['mousedown', 'mouseup', 'click', 'dblclick', 'auxclick', 'contextmenu'].forEach(t => document.addEventListener(t, note)); \
               ['mousedown', 'click', 'auxclick'].forEach(t => l.addEventListener(t, ev => {{ if (cancel[t]) ev.preventDefault(); }})); \
               c.addEventListener('click', ev => {{ if (cancel.cclick) ev.preventDefault(); }}); \
               box.onmouseover = ev => log.push('over:' + ev.target.id + ':' + String(ev.relatedTarget && ev.relatedTarget.id)); \
               box.onmouseout = ev => log.push('out:' + ev.target.id + ':' + String(ev.relatedTarget && ev.relatedTarget.id)); \
               l.onmouseenter = ev => log.push('enter:' + ev.target.id); box.onmouseenter = ev => log.push('enter:' + ev.target.id); \
               l.onmouseleave = ev => log.push('leave:' + ev.target.id); box.onmouseleave = ev => log.push('leave:' + ev.target.id); \
               l.addEventListener('mousemove', ev => log.push('move:' + Math.round(ev.clientX) + ':' + Math.round(ev.pageY) + ':' + Math.round(ev.offsetX) + ':' + ev.buttons + ':' + ev.ctrlKey + ':' + (ev instanceof MouseEvent))); \
               l.onfocus = () => log.push('focus:l'); \
               c.oninput = ev => log.push('input:' + ev.target.hasAttribute('checked')); c.onchange = ev => log.push('change:' + ev.target.hasAttribute('checked')); \
               window.addEventListener('wheel', ev => {{ log.push('wheel:' + ev.deltaY + ':' + ev.target.id + ':' + (ev instanceof WheelEvent)); if (cancel.wheel) ev.preventDefault(); }}); \
               document.addEventListener('scroll', ev => log.push('scroll:' + (ev.target === document) + ':' + ev.bubbles)); \
               window.onresize = ev => log.push('resize:' + (ev.target === window) + ':' + (ev instanceof UIEvent)); \
             </script>"
        );
        let mut h = Harness::load(&html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");
        let r = h.rect_of(h.by_id("l"));
        let (x, y) = (r.x + 3.0, r.y + 3.0);
        let (rx, ry) = (x.round() as i32, y.round() as i32);

        // Into the link: `mouseover`, `mouseenter` outermost first, then
        // the move with its coordinates.
        h.send(ShellToTab::MouseMove { x, y });
        assert_eq!(drain(&mut h), format!("\"over:l:null enter:box enter:l move:{rx}:{ry}:3:0:false:true\""));

        // A cancelled `mousedown` gives no focus; the modifiers and the
        // held button ride along on the move; the cancelled `click` does
        // not follow the link.
        js(&mut h, "cancel.mousedown = true; cancel.click = true");
        h.send(ShellToTab::Modifiers { shift: false, ctrl: true, alt: false, meta: false });
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseMove { x: x + 1.0, y });
        h.send(ShellToTab::MouseUp { x: x + 1.0, y, button: MouseButton::Left });
        assert_eq!(
            drain(&mut h),
            format!("\"mousedown:l:0:1:true move:{}:{ry}:4:1:true:true mouseup:l:0:1:true click:l:0:1:true\"", rx + 1)
        );
        assert!(h.state.focus.is_none(), "a cancelled mousedown does not focus");
        assert_eq!(h.title().as_deref(), Some("Mouse"));
        h.send(ShellToTab::Modifiers { shift: false, ctrl: false, alt: false, meta: false });

        // The second press in a row: `detail` 2 and `dblclick`; focus now.
        js(&mut h, "cancel.mousedown = false");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(
            drain(&mut h),
            "\"mousedown:l:0:2:true focus:l mouseup:l:0:2:true click:l:0:2:true dblclick:l:0:2:true\""
        );
        assert_eq!(h.title().as_deref(), Some("Mouse"));

        // The other buttons: `auxclick`, and `contextmenu` for the right
        // one; a cancelled `auxclick` does not open the link in a new tab.
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Right });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Right });
        assert_eq!(
            drain(&mut h),
            "\"mousedown:l:2:1:true mouseup:l:2:1:true auxclick:l:2:1:true contextmenu:l:2:0:true\""
        );
        js(&mut h, "cancel.auxclick = true");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Middle });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Middle });
        assert_eq!(drain(&mut h), "\"mousedown:l:1:1:true mouseup:l:1:1:true auxclick:l:1:1:true\"");
        let opened = |h: &Harness| {
            h.messages
                .lock()
                .expect("lock")
                .iter()
                .filter(|m| matches!(m, TabToShell::OpenInNewTab { .. }))
                .count()
        };
        assert_eq!(opened(&h), 0);
        js(&mut h, "cancel.auxclick = false");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Middle });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Middle });
        assert_eq!(opened(&h), 1);
        drain(&mut h);

        // Over to the checkbox: out and leave on the way; a click toggles
        // it and fires `input` and `change`; a cancelled click does not.
        h.click_id("c");
        assert_eq!(
            drain(&mut h),
            "\"out:l:c leave:l leave:box mousedown:c:0:1:true mouseup:c:0:1:true click:c:0:1:true input:true change:true\""
        );
        js(&mut h, "cancel.cclick = true");
        h.click_id("c");
        assert_eq!(
            drain(&mut h),
            "\"mousedown:c:0:2:true mouseup:c:0:2:true click:c:0:2:true dblclick:c:0:2:true\""
        );
        let c = h.by_id("c");
        assert!(h.state.doc.as_ref().and_then(|d| d.element(c)).is_some_and(|e| e.attr("checked").is_some()));

        // `wheel` before the scroll, `scroll` after it; a cancelled
        // `wheel` scrolls nothing. `resize` on the window.
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 100.0 });
        assert_eq!(drain(&mut h), "\"wheel:100:c:true scroll:true:true\"");
        assert_eq!(h.state.scroll_y, 100.0);
        js(&mut h, "cancel.wheel = true");
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 100.0 });
        assert_eq!(drain(&mut h), "\"wheel:100:tall:true\"");
        assert_eq!(h.state.scroll_y, 100.0);
        h.send(ShellToTab::Resize(Viewport {
            width: 700.0,
            height: 500.0,
            scale_factor: 1.0,
        }));
        assert_eq!(drain(&mut h), "\"resize:true:true\"");

        // Nothing cancelled: the click follows the link.
        js(&mut h, "cancel.wheel = false; cancel.click = false");
        h.send(ShellToTab::Scroll { dx: 0.0, dy: -100.0 });
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(h.title().as_deref(), Some("Other"));
    }

    #[test]
    fn key_focus_and_input_events_fire_with_cancellable_defaults() {
        let html = "<!DOCTYPE html><title>Keys</title><input id=i value=ab><input id=j><select id=s><option>a</option><option>b</option></select><input type=checkbox id=c><div style='height: 3000px'></div>\
             <script>\
               var log = []; var cancel = {}; \
               var i = document.getElementById('i'); \
               ['focus', 'blur', 'focusin', 'focusout'].forEach(t => document.addEventListener(t, ev => log.push(ev.type + ':' + ev.target.id + ':' + (ev.relatedTarget ? ev.relatedTarget.id : '-') + ':' + ev.bubbles + ':' + (ev instanceof FocusEvent)), true)); \
               document.addEventListener('keydown', ev => { log.push('kd:' + ev.key + ':' + ev.code + ':' + ev.keyCode + ':' + ev.shiftKey + ':' + ev.target.tagName.toLowerCase() + (ev.target.id ? '#' + ev.target.id : '') + ':' + (ev instanceof KeyboardEvent)); if (cancel[ev.key]) ev.preventDefault(); }); \
               document.addEventListener('keypress', ev => log.push('kp:' + ev.key + ':' + ev.charCode + ':' + ev.which)); \
               document.addEventListener('keyup', ev => log.push('ku:' + ev.key)); \
               i.addEventListener('beforeinput', ev => { log.push('bi:' + ev.inputType + ':' + ev.data + ':' + ev.cancelable + ':' + (ev instanceof InputEvent)); if (ev.data === 'z') ev.preventDefault(); }); \
               document.addEventListener('input', ev => log.push('in:' + ev.target.id + ':' + ev.inputType + ':' + ev.data + ':' + ev.target.getAttribute('value'))); \
               document.addEventListener('change', ev => log.push('ch:' + ev.target.id + ':' + ev.cancelable)); \
             </script>";
        let mut h = Harness::load(html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");
        let key = |h: &mut Harness, key: Key, shift: bool| h.key(key, shift);

        // Nothing focused: the key goes to `body`; the page scrolls.
        key(&mut h, Key::ArrowDown, false);
        assert_eq!(drain(&mut h), "\"kd:ArrowDown:ArrowDown:40:false:body:true\"");
        assert!(h.state.scroll_y > 0.0);

        // Tab into the first input: `focus` (no bubble), `focusin`.
        key(&mut h, Key::Tab, false);
        assert_eq!(
            drain(&mut h),
            "\"kd:Tab:Tab:9:false:body:true focus:i:-:false:true focusin:i:-:true:true\""
        );

        // Typing: `keydown`, `keypress`, `beforeinput`, `input`; `keyup`
        // from the shell. A cancelled `beforeinput` leaves the value.
        key(&mut h, Key::Character("x".into()), false);
        h.key_up(Key::Character("x".into()), false);
        assert_eq!(
            drain(&mut h),
            "\"kd:x:KeyX:88:false:input#i:true kp:x:120:120 bi:insertText:x:true:true in:i:insertText:x:abx ku:x\""
        );
        key(&mut h, Key::Character("z".into()), false);
        assert_eq!(drain(&mut h), "\"kd:z:KeyZ:90:false:input#i:true kp:z:122:122 bi:insertText:z:true:true\"");
        let i = h.by_id("i");
        assert_eq!(h.state.control_value(i), "abx");

        // Tab on: `change` for the edited value, then `blur` and
        // `focusout` with the new element related, `focus` and `focusin`
        // with the old one.
        key(&mut h, Key::Tab, false);
        assert_eq!(
            drain(&mut h),
            "\"kd:Tab:Tab:9:false:input#i:true ch:i:false blur:i:j:false:true focusout:i:j:true:true focus:j:i:false:true focusin:j:i:true:true\""
        );
        // A cancelled Tab moves nothing.
        js(&mut h, "cancel.Tab = true");
        key(&mut h, Key::Tab, false);
        assert_eq!(drain(&mut h), "\"kd:Tab:Tab:9:false:input#j:true\"");
        assert_eq!(h.focus_id().as_deref(), Some("j"));
        js(&mut h, "cancel.Tab = false");

        // Deleting; an unchanged value on blur fires no `change`.
        key(&mut h, Key::Character("y".into()), false);
        key(&mut h, Key::Backspace, false);
        assert_eq!(
            drain(&mut h),
            "\"kd:y:KeyY:89:false:input#j:true kp:y:121:121 in:j:insertText:y:y kd:Backspace:Backspace:8:false:input#j:true in:j:deleteContentBackward:null:\""
        );
        key(&mut h, Key::Tab, false);
        assert_eq!(
            drain(&mut h),
            "\"kd:Tab:Tab:9:false:input#j:true blur:j:s:false:true focusout:j:s:true:true focus:s:j:false:true focusin:s:j:true:true\""
        );

        // A select stepped and a checkbox toggled by key: `input` and
        // `change` at once.
        key(&mut h, Key::ArrowDown, false);
        assert_eq!(
            drain(&mut h),
            "\"kd:ArrowDown:ArrowDown:40:false:select#s:true in:s::null:null ch:s:false\""
        );
        key(&mut h, Key::Tab, false);
        drain(&mut h);
        key(&mut h, Key::Space, false);
        h.key_up(Key::Space, false);
        assert_eq!(drain(&mut h), "\"kd: :Space:32:false:input#c:true kp: :32:32 ku:  in:c::null:null ch:c:false\"");
        key(&mut h, Key::Tab, true);
        assert_eq!(
            drain(&mut h),
            "\"kd:Tab:Tab:9:true:input#c:true blur:c:s:false:true focusout:c:s:true:true focus:s:c:false:true focusin:s:c:true:true\""
        );
    }

    #[test]
    fn pointer_events_come_first_capture_the_mouse_and_hold_back_mouse_events() {
        let html = "<!DOCTYPE html><title>Pointer</title><style>body { margin: 0 } div { height: 40px; margin: 0 }</style>\
             <div id=a></div><div id=b></div><div id=c></div>\
             <script>\
               var log = []; var cancel = {}; var capture = false; \
               var a = document.getElementById('a'), b = document.getElementById('b'); \
               var name = ev => ev.target.id || ev.target.tagName.toLowerCase(); \
               var note = ev => log.push(ev.type + ':' + name(ev) + ':' + (ev.pointerId === undefined ? 'm' : ev.pointerId) + ':' + ev.button + ':' + ev.buttons); \
               ['pointerdown', 'pointerup', 'pointermove', 'pointerover', 'pointerout', 'pointerenter', 'pointerleave', 'gotpointercapture', 'lostpointercapture', \
                'mousedown', 'mouseup', 'mousemove', 'mouseover', 'mouseout', 'mouseenter', 'mouseleave', 'click'].forEach(t => document.addEventListener(t, note, true)); \
               a.addEventListener('pointerdown', ev => { if (cancel.pointerdown) ev.preventDefault(); if (capture) a.setPointerCapture(ev.pointerId); }); \
               b.addEventListener('pointermove', ev => log.push('mv:' + ev.movementX + ':' + ev.movementY + ':' + ev.getCoalescedEvents().length + ':' + (ev instanceof PointerEvent) + ':' + (ev instanceof MouseEvent) + ':' + ev.pressure + ':' + ev.pointerType)); \
               a.addEventListener('lostpointercapture', () => log.push('hascap:' + a.hasPointerCapture(1))); \
             </script>";
        let mut h = Harness::load(html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");

        // Into `a`: pointer boundary events, then the mouse ones, then
        // the batch's `pointermove` and `mousemove`.
        h.send(ShellToTab::MouseMove { x: 10.0, y: 20.0 });
        assert_eq!(
            drain(&mut h),
            "\"pointerover:a:1:-1:0 pointerenter:html:1:-1:0 pointerenter:body:1:-1:0 pointerenter:a:1:-1:0 \
              mouseover:a:m:0:0 mouseenter:html:m:0:0 mouseenter:body:m:0:0 mouseenter:a:m:0:0 pointermove:a:1:-1:0 mousemove:a:m:0:0\""
        );
        // Over to `b`, then a second move: `movementX/Y` from the last
        // move, one coalesced event (the move itself).
        h.send(ShellToTab::MouseMove { x: 10.0, y: 60.0 });
        assert_eq!(
            drain(&mut h),
            "\"pointerout:a:1:-1:0 pointerleave:a:1:-1:0 mouseout:a:m:0:0 mouseleave:a:m:0:0 \
              pointerover:b:1:-1:0 pointerenter:b:1:-1:0 mouseover:b:m:0:0 mouseenter:b:m:0:0 \
              pointermove:b:1:-1:0 mv:0:40:1:true:true:0:mouse mousemove:b:m:0:0\""
        );
        h.send(ShellToTab::MouseMove { x: 15.0, y: 60.0 });
        assert_eq!(drain(&mut h), "\"pointermove:b:1:-1:0 mv:5:0:1:true:true:0:mouse mousemove:b:m:0:0\"");

        // A cancelled `pointerdown` holds back `mousedown`, `mousemove`
        // and `mouseup` until the pointer goes up; `click` still fires.
        h.send(ShellToTab::MouseMove { x: 10.0, y: 20.0 });
        drain(&mut h);
        js(&mut h, "cancel.pointerdown = true");
        h.send(ShellToTab::MouseDown { x: 10.0, y: 20.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseMove { x: 12.0, y: 20.0 });
        h.send(ShellToTab::MouseUp { x: 12.0, y: 20.0, button: MouseButton::Left });
        assert_eq!(drain(&mut h), "\"pointerdown:a:1:0:1 pointermove:a:1:-1:1 pointerup:a:1:0:0 click:a:1:0:0\"");
        js(&mut h, "cancel.pointerdown = false");
        h.send(ShellToTab::MouseMove { x: 13.0, y: 20.0 });
        assert_eq!(drain(&mut h), "\"pointermove:a:1:-1:0 mousemove:a:m:0:0\"");

        // Capture: `gotpointercapture`, moves go to `a` wherever the
        // pointer is, release on `pointerup` with `lostpointercapture`,
        // then the boundary events catch up and `click` goes to the
        // element the press and the release share.
        js(&mut h, "capture = true");
        h.send(ShellToTab::MouseDown { x: 13.0, y: 20.0, button: MouseButton::Left });
        assert_eq!(drain(&mut h), "\"pointerdown:a:1:0:1 mousedown:a:m:0:1 gotpointercapture:a:1:-1:1\"");
        h.send(ShellToTab::MouseMove { x: 10.0, y: 60.0 });
        assert_eq!(drain(&mut h), "\"pointermove:a:1:-1:1 mousemove:a:m:0:1\"");
        assert_eq!(h.state.hover, Some(h.by_id("a")), "hover follows the capture");
        h.send(ShellToTab::MouseUp { x: 10.0, y: 60.0, button: MouseButton::Left });
        assert_eq!(
            drain(&mut h),
            "\"pointerup:a:1:0:0 mouseup:a:m:0:0 lostpointercapture:a:1:-1:0 hascap:false \
              pointerout:a:1:-1:0 pointerleave:a:1:-1:0 mouseout:a:m:0:0 mouseleave:a:m:0:0 \
              pointerover:b:1:-1:0 pointerenter:b:1:-1:0 mouseover:b:m:0:0 mouseenter:b:m:0:0 click:body:1:0:0\""
        );
        assert_eq!(h.state.hover, Some(h.by_id("b")));
        // The capture methods refuse an unknown pointer.
        assert_eq!(
            js(&mut h, "var e = []; try { a.setPointerCapture(2) } catch (x) { e.push(x.name) } try { a.releasePointerCapture(2) } catch (x) { e.push(x.name) } e.push(a.hasPointerCapture(1), a.hasPointerCapture(2)); e.join()"),
            "\"NotFoundError,NotFoundError,false,false\""
        );
    }

    #[test]
    fn the_keyboard_clicks_links_and_buttons_and_commits_inputs() {
        let html = "<!DOCTYPE html><title>KeyClick</title><a id=l href='#x'>link</a><button id=b>go</button><input type=checkbox id=c><input id=i value=v>\
             <script>\
               var log = []; var cancel = {}; \
               document.addEventListener('click', ev => { log.push('click:' + ev.target.id + ':' + ev.pointerId + ':' + ev.pointerType + ':' + ev.detail + ':' + ev.clientX + ':' + (ev instanceof PointerEvent)); if (cancel[ev.target.id]) ev.preventDefault(); }); \
               document.addEventListener('keydown', ev => log.push('kd:' + ev.code + ':' + ev.repeat)); \
               document.addEventListener('keyup', ev => log.push('ku:' + ev.key + ':' + ev.code + ':' + ev.target.id)); \
               document.addEventListener('change', ev => log.push('change:' + ev.target.id + ':' + ev.target.getAttribute('value'))); \
               document.addEventListener('contextmenu', ev => log.push('ctx:' + ev.target.id + ':' + ev.pointerId + ':' + ev.pointerType + ':' + Math.round(ev.clientX) + ':' + Math.round(ev.clientY))); \
             </script>";
        let mut h = Harness::load(html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");

        // Enter on a link: a synthetic `click` (pointerId -1, no
        // coordinates); cancelled, the link is not followed.
        h.key(Key::Tab, false);
        drain(&mut h);
        js(&mut h, "cancel.l = true");
        h.key(Key::Enter, false);
        assert_eq!(drain(&mut h), "\"kd:Enter:false click:l:-1::0:0:true\"");
        assert_eq!(h.state.url.as_ref().and_then(|u| u.fragment()), None);
        js(&mut h, "cancel.l = false");
        h.key(Key::Enter, false);
        assert_eq!(drain(&mut h), "\"kd:Enter:false click:l:-1::0:0:true\"");
        assert_eq!(h.state.url.as_ref().and_then(|u| u.fragment()), Some("x"));

        // Space on a button clicks on release; the shell's `code` and
        // `repeat` ride along.
        h.key(Key::Tab, false);
        drain(&mut h);
        h.send(ShellToTab::Key {
            key: Key::Space,
            code: "Space".into(),
            repeat: true,
            shift: false,
            ctrl: false,
            alt: false,
        });
        assert_eq!(drain(&mut h), "\"kd:Space:true\"");
        h.send(ShellToTab::KeyUp {
            key: Key::Space,
            code: "Space".into(),
            shift: false,
            ctrl: false,
            alt: false,
        });
        assert_eq!(drain(&mut h), "\"ku: :Space:b click:b:-1::0:0:true\"");

        // A checkbox: the click toggles; cancelled, it does not.
        h.key(Key::Tab, false);
        drain(&mut h);
        h.key(Key::Space, false);
        h.key_up(Key::Space, false);
        assert_eq!(drain(&mut h), "\"kd:Space:false ku: :Space:c click:c:-1::0:0:true change:c:null\"");
        assert!(h.has_attr("c", "checked"));
        js(&mut h, "cancel.c = true");
        h.key(Key::Space, false);
        h.key_up(Key::Space, false);
        assert!(h.has_attr("c", "checked"), "a cancelled click toggles nothing");
        drain(&mut h);

        // Enter in an input commits an edited value with `change`, once.
        h.key(Key::Tab, false);
        h.typed("w");
        drain(&mut h);
        h.key(Key::Enter, false);
        assert_eq!(drain(&mut h), "\"kd:Enter:false change:i:vw\"");
        h.key(Key::Enter, false);
        assert_eq!(drain(&mut h), "\"kd:Enter:false\"");

        // The context-menu key: `contextmenu` at the focused element, at
        // its box.
        let r = h.rect_of(h.by_id("i"));
        h.key(Key::ContextMenu, false);
        assert_eq!(
            drain(&mut h),
            format!("\"kd:ContextMenu:false ctx:i:-1::{}:{}\"", r.x.round() as i32, r.y.round() as i32)
        );
    }

    #[test]
    fn body_handler_attributes_forward_to_window() {
        let html = "<!DOCTYPE html><title>Body</title>\
             <body onload=\"log.push('bodyload:' + (this === window) + ':' + (event.target === window))\" onresize=\"log.push('bodyresize')\" onscroll=\"log.push('bodyscroll:' + (event.target === document))\">\
             <div style='height: 3000px'></div>\
             <script>var log = []; setTimeout(() => log.push('same:' + (document.body.onresize === window.onresize) + ':' + typeof window.onload + ':' + (document.body.onload === window.onload)), 0)</script>";
        let mut h = Harness::load(html);
        std::thread::sleep(std::time::Duration::from_millis(2));
        h.pump();
        assert_eq!(js(&mut h, "log.splice(0).join(' ')"), "\"bodyload:true:true same:true:function:true\"");
        h.send(ShellToTab::Resize(Viewport {
            width: 700.0,
            height: 500.0,
            scale_factor: 1.0,
        }));
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 50.0 });
        assert_eq!(js(&mut h, "log.splice(0).join(' ')"), "\"bodyresize bodyscroll:true\"");
    }

    /// Console text so far, in order.
    fn console_texts(h: &Harness) -> Vec<String> {
        h.state.console.iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn external_scripts_block_the_parser_and_run_in_document_order() {
        let html = "<title>Order</title><script>console.log('a')</script>\
                    <script src=\"data:text/javascript,console.log('b')\"></script>\
                    <script>console.log('c')</script><p>x</p>\
                    <script src=\"data:text/javascript,console.log('d')\" nomodule></script>\
                    <script type=\"text/plain\">not script</script>\
                    <script src=\"\"></script>\
                    <script src=\"data:text/javascript,console.log('bad status')\" language=\"vbscript\"></script>";
        let mut h = Harness::start(data_url(html, None));
        // The main response is three events; handling its body stops the
        // parser at the external script, whose fetch is answered after.
        h.pump_n(3);
        assert_eq!(console_texts(&h), ["a"], "only the script before the external one has run");
        assert!(h.state.scripts.blocking.is_some(), "the parser waits for the fetch");
        assert!(h.state.main_done, "the main response has ended");
        assert!(h.state.doc.is_none(), "the document is not shown while parsing waits");
        h.pump();
        assert_eq!(console_texts(&h)[..3], ["a", "b", "c"]);
        assert_eq!(h.title().as_deref(), Some("Order"));
        // `nomodule`, a data block, a foreign `language` and an empty src
        // do not run; the empty src is reported.
        let t = console_texts(&h);
        assert_eq!(t.len(), 4, "{t:?}");
        assert!(t[3].starts_with("Failed to load script"), "{t:?}");
        assert!(h.state.scripts.blocking.is_none() && h.state.scripts.deferred.is_empty());
        assert!(!h.state.is_loading());
    }

    #[test]
    fn async_and_defer_scripts_run_in_their_own_order() {
        let html = "<title>Async</title>\
                    <script defer src=\"data:text/javascript,console.log('defer1')\"></script>\
                    <script async src=\"data:text/javascript,console.log('async')\"></script>\
                    <script defer src=\"data:text/javascript,console.log('defer2')\"></script>\
                    <script defer>console.log('inline defer runs inline')</script>\
                    <script>console.log('inline')</script>";
        let mut h = Harness::start(data_url(html, None));
        h.pump_n(3);
        // Nothing external blocked the parser: the document is complete
        // and its deferred scripts wait for their bodies.
        assert!(h.state.doc.is_some() && h.state.parser.is_none());
        assert_eq!(console_texts(&h), ["inline defer runs inline", "inline"]);
        assert_eq!(h.state.scripts.deferred.len(), 2);
        assert_eq!(h.state.scripts.asap.len(), 1);
        h.pump();
        // Deferred scripts keep document order; the async one runs as
        // soon as it arrives, here between them.
        assert_eq!(
            console_texts(&h),
            ["inline defer runs inline", "inline", "defer1", "async", "defer2"]
        );
    }

    #[test]
    fn module_scripts_load_their_imports_and_run_after_parsing() {
        let html = "<title>Mod</title>\
                    <script type=module>import { x } from \"data:text/javascript,export const x = 40 %2B 2\"; console.log('module', x);</script>\
                    <script>console.log('classic')</script>\
                    <script type=module>import x from 'nowhere';</script>\
                    <script type=module src=\"data:text/javascript,console.log('external module')\"></script>\
                    <script type=module>export {</script>\
                    <script type=module src=\"data:text/plain,console.log('wrong mime')\"></script>\
                    <script type=module async>console.log('async module')</script>";
        let h = Harness::load(html);
        let t = console_texts(&h);
        assert_eq!(t[0], "classic", "classic scripts run during parsing, modules after: {t:?}");
        assert!(t[1].starts_with("Uncaught SyntaxError"), "a module syntax error is reported at once: {t:?}");
        assert_eq!(t[2], "async module", "an async inline module runs as soon as it is ready: {t:?}");
        assert_eq!(t[3], "module 42", "{t:?}");
        assert!(t[4].contains("Failed to resolve module specifier \"nowhere\""), "{t:?}");
        assert_eq!(t[5], "external module", "{t:?}");
        assert!(t[6].contains("text/plain"), "a module served as text is refused: {t:?}");
        assert_eq!(t.len(), 7, "{t:?}");
        assert!(!h.state.is_loading());
    }

    /// A loopback HTTP/1.1 server answering canned responses by path, able
    /// to hold a response back until told. Nothing leaves the machine.
    struct Server {
        base: Url,
        held: Arc<(Mutex<std::collections::HashSet<String>>, std::sync::Condvar)>,
    }

    impl Server {
        fn start(routes: &[(&str, &str, &str)], held: &[&str]) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let routes: Arc<HashMap<String, (String, String)>> = Arc::new(
                routes
                    .iter()
                    .map(|(p, ct, body)| ((*p).to_owned(), ((*ct).to_owned(), (*body).to_owned())))
                    .collect(),
            );
            let held = Arc::new((
                Mutex::new(held.iter().map(|s| (*s).to_owned()).collect::<std::collections::HashSet<_>>()),
                std::sync::Condvar::new(),
            ));
            let gate = held.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let routes = routes.clone();
                    let gate = gate.clone();
                    std::thread::spawn(move || {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            let Ok(n) = stream.read(&mut buf) else { return };
                            if n == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..n]);
                        }
                        let head = String::from_utf8_lossy(&head).into_owned();
                        let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        {
                            let (set, cv) = &*gate;
                            let mut set = set.lock().expect("lock");
                            while set.contains(&path) {
                                set = cv.wait(set).expect("wait");
                            }
                        }
                        let response = match routes.get(&path) {
                            Some((ct, body)) => format!(
                                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            ),
                            None => "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_owned(),
                        };
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    });
                }
            });
            Self {
                base: Url::parse(&format!("http://127.0.0.1:{port}/")).expect("url"),
                held,
            }
        }

        fn url(&self, path: &str) -> Url {
            self.base.join(path).expect("url")
        }

        fn hold(&self, path: &str) {
            self.held.0.lock().expect("lock").insert(path.to_owned());
        }

        fn release(&self, path: &str) {
            self.held.0.lock().expect("lock").remove(path);
            self.held.1.notify_all();
        }
    }

    #[test]
    fn a_parser_blocking_script_waits_for_the_network_and_stop_gives_up_on_it() {
        let page = "<title>Net</title><script>console.log('a')</script>\
                    <script src=\"/slow.js\"></script><script>console.log('c')</script><p>x</p>";
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/slow.js", "text/javascript", "console.log('b')"),
            ],
            &["/slow.js"],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.scripts.blocking.is_some());
        assert!(h.state.doc.is_none(), "nothing is shown while the parser waits for the script");
        assert_eq!(console_texts(&h), ["a"]);
        assert!(h.state.is_loading());
        server.release("/slow.js");
        h.pump_until(|h| h.state.doc.is_some());
        assert_eq!(console_texts(&h), ["a", "b", "c"]);
        assert_eq!(h.title().as_deref(), Some("Net"));
        h.pump_until(|h| !h.state.is_loading());

        // Stop while the parser waits: what has arrived is shown and the
        // script is dropped, even when its response comes in later.
        server.hold("/slow.js");
        h.state.handle_shell(ShellToTab::Reload);
        h.pump_until(|h| h.state.scripts.blocking.is_some());
        assert_eq!(console_texts(&h), ["a", "b", "c", "a"]);
        h.send(ShellToTab::Stop);
        assert!(h.state.parser.is_none() && h.state.doc.is_some());
        assert_eq!(h.title().as_deref(), Some("Net"));
        assert!(!h.state.is_loading());
        server.release("/slow.js");
        h.pump_for(std::time::Duration::from_millis(300));
        assert_eq!(console_texts(&h), ["a", "b", "c", "a"], "the late script did not run");
    }

    #[test]
    fn keys_scroll_when_nothing_takes_them() {
        let mut h = Harness::load(TALL);
        h.key(Key::ArrowDown, false);
        assert_eq!(h.state.scroll_y, 40.0);
        h.key(Key::PageDown, false);
        assert_eq!(h.state.scroll_y, 580.0);
        h.key(Key::Space, true);
        assert_eq!(h.state.scroll_y, 40.0);
        h.key(Key::End, false);
        assert!(h.state.scroll_y > 900.0);
        h.key(Key::Home, false);
        assert_eq!(h.state.scroll_y, 0.0);
        h.key(Key::Space, false);
        assert_eq!(h.state.scroll_y, 540.0);
    }

    #[test]
    fn dragging_past_the_bottom_scrolls_and_extends() {
        let mut h = Harness::load(TALL);
        h.send(ShellToTab::MouseDown { x: -5.0, y: 10.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseMove { x: 400.0, y: 650.0 });
        assert!(h.state.autoscroll.is_some(), "the pointer is below the viewport");
        assert!(h.state.next_wake().is_some());
        // Fire the timer until the page can scroll no further.
        let mut steps = 0;
        loop {
            let before = h.state.scroll_y;
            h.state.autoscroll = Some(std::time::Instant::now());
            h.pump();
            steps += 1;
            if h.state.scroll_y == before || steps > 500 {
                break;
            }
        }
        assert!(h.state.scroll_y > 900.0, "scrolled toward the pointer, got {}", h.state.scroll_y);
        assert!(steps > 5, "one step at a time, not a jump: {steps}");
        assert_eq!(h.selected().as_deref(), Some("down top\nThe end"), "the drag reached the heading below");
        h.send(ShellToTab::MouseUp { x: 400.0, y: 650.0, button: MouseButton::Left });
        assert!(h.state.autoscroll.is_none());
        assert!(h.state.next_wake().is_none());
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
