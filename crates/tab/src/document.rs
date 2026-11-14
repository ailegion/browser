//! Per-tab state: the current document, its resources, and the render
//! pipeline from DOM to scene.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use browser_dom::{Document, HtmlParser, NodeId};
use browser_ipc_types::{
    Cursor, Key, MouseButton, NetToTab, ReferrerPolicy, RequestId, RequestReferrer, ShellToTab, TabId, TabToShell, Viewport,
};
use browser_layout::selection::{self, TextPos};
use browser_layout::{LayoutEngine, LayoutTree, Rect, ScrollbarPart, SelectionRanges};
use browser_net::{CacheMode, FetchRequest, NetService, Sink};

/// `document.cookie` over the browser's jar (HTML §7.9.2): reads leave
/// `HttpOnly` cookies out, writes go through the jar's non-HTTP rules.
struct TabCookies(Arc<NetService>);

impl browser_script::CookieAccess for TabCookies {
    fn cookie_string(&self, url: &Url) -> String {
        self.0.cookies().script_cookie_string(url, std::time::SystemTime::now())
    }

    fn set_cookie(&self, url: &Url, value: &str) {
        self.0.cookies().store_from_script(url, value, std::time::SystemTime::now());
    }
}
use browser_paint::scrollbar::{ScrollbarState, draw_corner_colored, draw_scrollbar};
use browser_paint::{ImageStore, PaintOptions, decode_image, paint};
use browser_script::{
    ConsoleLevel, ConsoleLine, EventTargetRef, HostRequest, ImageSizeMap, MAX_HISTORY_ENTRIES, ModuleProgress, ModuleScript,
    NetFailure, NetOp, ScriptHost, SerializedState, StorageArea, View,
};

use crate::scriptnet::{Action, BlockingNet, Job, SharedPreflights};
use crate::storage::{OriginStorage, StorageChange, TabStores};
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

mod editing;
mod scrolling;
mod user_events;

use editing::Composition;
use scrolling::{AnimationKind, BarRepeat, KeyScroll, Latch, Pan, Reveal, ScrollAnimation, ScrollDrag, ScrollTarget, element_rect};
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
        /// Whether the document's load event waits for it (not for a
        /// lazily loaded `<img>`'s, HTML §4.8.4.3.5 step 26).
        delays_load: bool,
    },
    /// A web font's source ("Working sites first" line 6).
    Font {
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
    /// The element's `referrerpolicy` (its fetch options').
    policy: ReferrerPolicy,
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
    /// An inline classic script: run it now, then parse on; with its
    /// base URL (the document's when prepared) and its element's
    /// `referrerpolicy`.
    Run(String, Option<Url>, ReferrerPolicy),
    /// An external script the parser must wait for.
    Blocking,
    /// Queued, skipped, or already handled: parse on.
    Done,
}

struct Pending {
    kind: PendingKind,
    status: u16,
    /// The status line's reason phrase (empty over HTTP/2).
    status_text: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Where the response came from after redirects.
    final_url: Option<Url>,
}

/// The current document's web fonts: the sources asked for, and those
/// that failed (their families' fallbacks are used, and a later family
/// in a list may be fetched instead).
#[derive(Default)]
struct WebFonts {
    /// The font URLs fetched (or being fetched).
    requested: std::collections::HashSet<Url>,
    /// The sources that could not be used: an error, a CORS refusal, data
    /// that is no font, a `local()` name no installed font has.
    failed: std::collections::HashSet<Source>,
    /// The faces registered (by `face_key`).
    loaded: std::collections::HashSet<usize>,
    /// When each face began loading, and its `font-display`: the periods
    /// count from then.
    started: HashMap<usize, (std::time::Instant, browser_style::fontface::FontDisplay)>,
    /// The faces whose swap period ended before they loaded: failed for
    /// the document (the fallback stays).
    timed_out: std::collections::HashSet<usize>,
    /// The faces whose block period has ended (their text is drawn).
    unblocked: std::collections::HashSet<usize>,
    /// Every face the document has had, kept so a face's key (its
    /// address) is never another face's while the document lives.
    seen: Vec<Arc<browser_style::fontface::FontFaceRule>>,
    /// The faces scripts made (`new FontFace`), in `document.fonts` or
    /// not, as the script host last listed them.
    script: Vec<browser_script::ScriptFace>,
    /// The font data of script faces, by id: bytes a script gave (checked
    /// to be a font) or a response that loaded the face; kept so a face
    /// added to the set later, or whose descriptors changed, registers
    /// from it.
    script_data: HashMap<u64, Arc<[u8]>>,
    /// Script faces made from bytes that are no font.
    script_failed: std::collections::HashSet<u64>,
    /// Faces a script asked to load (`FontFace.load()`,
    /// `document.fonts.load()`), used by text or not: `@font-face` faces
    /// by key, script faces by id.
    forced_css: std::collections::HashSet<usize>,
    forced_script: std::collections::HashSet<u64>,
    /// Faces whose data arrived after their swap period: loaded (as the
    /// script sees them) but not used.
    late: std::collections::HashSet<usize>,
    /// Something font loading reports changed since `document.fonts` was
    /// last told.
    dirty: bool,
    /// What `document.fonts` was last told.
    reported: Option<(Vec<usize>, Vec<(browser_script::FaceRef, browser_script::FontLoadStatus)>, bool)>,
}

/// A face's identity: the address of its rule (kept alive in
/// `WebFonts::seen`).
fn face_key(face: &Arc<browser_style::fontface::FontFaceRule>) -> usize {
    Arc::as_ptr(face) as usize
}

/// The block and swap periods of `font-display` (CSS Fonts 4 §4.9); `None`
/// is an infinite swap period. `auto` is `block`, as in browsers. As the
/// WPT `css-fonts/font-display` reference has it: `block` hides the text
/// for 3 s, `swap` not at all, `fallback` for 100 ms and swaps within 3 s
/// after; `optional` shows the fallback at once and takes the face only if
/// it arrives almost at once (within 100 ms).
fn display_periods(display: browser_style::fontface::FontDisplay) -> (std::time::Duration, Option<std::time::Duration>) {
    use browser_style::fontface::FontDisplay as D;
    use std::time::Duration;
    match display {
        D::Auto | D::Block => (Duration::from_secs(3), None),
        D::Swap => (Duration::ZERO, None),
        D::Fallback => (Duration::from_millis(100), Some(Duration::from_secs(3))),
        D::Optional => (Duration::ZERO, Some(Duration::from_millis(100))),
    }
}

/// The clock the `font-display` periods run on: the real time. A test
/// may freeze it (`FontClock::frozen`) and move it by hand (`advance`),
/// so a period ends exactly when the test says, however busy the machine.
#[derive(Debug, Clone, Default)]
struct FontClock {
    #[cfg(test)]
    frozen: Option<std::rc::Rc<std::cell::Cell<std::time::Instant>>>,
}

impl FontClock {
    fn now(&self) -> std::time::Instant {
        #[cfg(test)]
        if let Some(t) = &self.frozen {
            return t.get();
        }
        std::time::Instant::now()
    }

    /// An instant of this clock as a real one, for the tab's wake-up.
    fn to_real(&self, at: std::time::Instant) -> std::time::Instant {
        #[cfg(test)]
        if let Some(t) = &self.frozen {
            return std::time::Instant::now() + at.saturating_duration_since(t.get());
        }
        at
    }

    /// A clock stopped at the real time now.
    #[cfg(test)]
    fn frozen() -> Self {
        Self {
            frozen: Some(std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now()))),
        }
    }

    /// Move a frozen clock on by `by`.
    #[cfg(test)]
    fn advance(&self, by: std::time::Duration) {
        if let Some(t) = &self.frozen {
            t.set(t.get() + by);
        }
    }
}

/// One `src` entry of an `@font-face` rule, as loaded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Source {
    Url(Url),
    /// `local()`, its name lowercased.
    Local(String),
}

impl WebFonts {
    /// Whether text waiting for the face at `order` is in its block
    /// period (invisible): from the face's start (now, for a face not yet
    /// started) until the period ends.
    fn blocking(&self, key: usize, display: browser_style::fontface::FontDisplay, now: std::time::Instant) -> bool {
        let (block, _) = display_periods(display);
        match self.started.get(&key) {
            Some((start, _)) => now < *start + block,
            None => !block.is_zero(),
        }
    }

    /// The next instant a period of a face still loading ends (its text
    /// becomes visible, or the face is given up).
    fn next_wake(&self) -> Option<std::time::Instant> {
        self.started
            .iter()
            .filter(|(o, _)| !self.loaded.contains(o) && !self.timed_out.contains(o))
            .filter_map(|(o, (start, display))| {
                let (block, swap) = display_periods(*display);
                if !self.unblocked.contains(o) {
                    Some(*start + block)
                } else {
                    swap.map(|s| *start + block + s)
                }
            })
            .min()
    }

    /// Move the faces still loading along their periods; true when text
    /// changes (becomes visible, or falls back for good).
    fn tick(&mut self, now: std::time::Instant) -> bool {
        let mut changed = false;
        for (o, (start, display)) in &self.started {
            if self.loaded.contains(o) || self.timed_out.contains(o) {
                continue;
            }
            let (block, swap) = display_periods(*display);
            if now >= *start + block && !self.unblocked.contains(o) {
                self.unblocked.insert(*o);
                changed = true;
            }
            if swap.is_some_and(|s| now >= *start + block + s) {
                self.timed_out.insert(*o);
                changed = true;
            }
        }
        changed
    }

    /// Whether the face at `order` loads too late to be used: its swap
    /// period has ended.
    fn expired(&self, key: usize, display: browser_style::fontface::FontDisplay, now: std::time::Instant) -> bool {
        let (block, swap) = display_periods(display);
        self.timed_out.contains(&key)
            || swap.zip(self.started.get(&key)).is_some_and(|(swap, (start, _))| now >= *start + block + swap)
    }

    /// The source a face loads from: its first usable one that has not
    /// failed (a failed source gives way to the next, CSS Fonts 4 §4.3.1);
    /// `None` when none is left.
    fn source_of(&self, face: &browser_style::fontface::FontFaceRule) -> Option<Source> {
        use browser_style::fontface::FontSource;
        face.sources.iter().find_map(|s| {
            let source = match s {
                FontSource::Local(name) => Source::Local(name.to_lowercase()),
                FontSource::Url { .. } => Source::Url(Url::parse(face.loadable_url(s)?).ok()?),
            };
            (!self.failed.contains(&source)).then_some(source)
        })
    }
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

/// One entry of the tab's session history (HTML §7.4.1).
#[derive(Debug, Clone, PartialEq)]
struct HistoryEntry {
    url: Url,
    /// Which document the entry belongs to (HTML's document sequence
    /// number): entries a document made with `pushState` or a fragment
    /// navigation share it, and a traversal between them loads nothing.
    /// It stays with the entry across a reload.
    document: u64,
    /// The state `pushState`/`replaceState` gave the entry.
    state: Option<SerializedState>,
    /// Where the page was scrolled when the entry was left.
    scroll: Option<(f32, f32)>,
    /// `history.scrollRestoration` is `manual` for the entry.
    manual_scroll: bool,
    /// Its document state's request referrer, as the load determined it
    /// (none: no referrer), and the request referrer policy the
    /// navigation was given (`Empty`: the source document's then). A
    /// reload or a traversal that loads requests with them again (HTML
    /// §7.4.5); entries of one document share them.
    referrer: Option<Url>,
    referrer_policy: ReferrerPolicy,
}

/// A request a script made (`fetch`, `XMLHttpRequest`) on its way through
/// the fetch algorithm.
struct ScriptFetch {
    job: Job,
    /// The network request of the hop under way.
    hop: Option<RequestId>,
    started: std::time::Instant,
    /// `XMLHttpRequest.timeout`.
    timeout: Option<std::time::Duration>,
    /// The request goes on when its document is replaced (`keepalive`,
    /// a beacon).
    keepalive: bool,
    /// Its document is gone: it is carried to its end and nobody is told.
    detached: bool,
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
    /// Where its request's `Referer` came from, and the policy it was
    /// given (for its history entry; a restarted load uses them again).
    referrer: NavReferrer,
}

/// A navigation's referrer (`browser_ipc_types::NavigationReferrer`).
type NavReferrer = browser_ipc_types::NavigationReferrer;

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
    /// Image URLs fetched or being fetched for the current document's
    /// `<img>` elements (one fetch per URL).
    fetched_images: std::collections::HashSet<Url>,
    /// Every image URL the current document asked for, fetched or found
    /// decoded already: what stays when the parsed document replaces the
    /// one before.
    wanted_images: std::collections::HashSet<Url>,
    /// Image URLs whose fetch failed or did not decode: a request for one
    /// is broken at once ("Working sites first" 6m).
    failed_images: std::collections::HashSet<Url>,
    /// Image fetches that wait for their `<img>` to be rendered (one that
    /// allows auto-sizes, lazily loaded), with the URL they fetch.
    deferred_images: Vec<(NodeId, Url, ReferrerPolicy)>,
    /// External sheets already parsed, by URL, so a script that adds a
    /// `<link>` or `<style>` again does not fetch again.
    loaded_sheets: std::collections::HashMap<Url, Arc<Stylesheet>>,
    images: ImageStore,
    web_fonts: WebFonts,
    /// How the current document's sub-resources use the HTTP cache: a
    /// reloaded document revalidates them, as browsers do.
    doc_cache: CacheMode,

    /// Lent to the script host around every script run (`View`), so a
    /// script can force a layout; always back here between runs.
    engine: Option<LayoutEngine>,
    styles: StyleMap,
    layout: Option<LayoutTree>,
    /// How many layouts this tab has run: a paint-only restyle must not
    /// add one (checked by tests; useful in logs).
    layouts: u64,
    /// What the current batch of events costs (the `perf` log line).
    perf: BatchPerf,
    /// A change the engine could not be told about (it was lent to a
    /// script): it builds every box at its next layout.
    box_changes_unknown: bool,
    scene: Scene,
    /// The view lent to scripts: the viewport and what it needs to lay
    /// out; its caches live on between runs.
    view: View,
    /// The window's outer size and the screen's, from the shell.
    window_size: (f32, f32),
    screen_size: (f32, f32),
    /// Intrinsic image sizes for a layout a script forces.
    image_sizes: ImageSizeMap,
    /// A script forced a layout that is still current: `dom_changed`
    /// need not schedule another.
    host_layout_fresh: bool,
    /// The main response's `Referrer-Policy` (the document's policy
    /// container's, until a `<meta name=referrer>` replaces it).
    doc_header_policy: ReferrerPolicy,
    /// The referrer the document's load determined: `document.referrer`.
    document_referrer: Option<Url>,
    /// The document's referrer for a synchronous `XMLHttpRequest`, kept
    /// current by `sync_document_info`.
    sync_referrer: std::rc::Rc<std::cell::RefCell<Option<RequestReferrer>>>,
    /// What the `font-display` periods count on (`FontClock`).
    font_clock: FontClock,

    /// Interaction state behind `:hover`, `:active` and `:focus`, and how
    /// far a change in it reaches under the current sheets.
    states: ElementStates,
    deps: InteractionDeps,
    hover: Option<NodeId>,
    focus: Option<NodeId>,
    /// The `cursor: url()` image last sent to the shell, by URL, so a
    /// pointer move over the same element does not send it again.
    cursor_image_sent: Option<Arc<str>>,
    /// Byte offset of the caret in the focused text control's value.
    caret: usize,
    /// The other end of the selection inside the focused text control,
    /// when there is one (`editing.rs`).
    sel_anchor: Option<usize>,
    /// A primary drag inside the focused text control extends its
    /// selection.
    control_drag: bool,
    /// An IME composition in the focused text control.
    composition: Option<Composition>,
    /// The control selection `select` was last fired for.
    last_select: Option<(usize, usize)>,
    /// The caret last reported to the shell (`TabToShell::Caret`).
    caret_reported: Option<(f32, f32, f32, f32)>,
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
    /// last fired for, the page's and each element scroller's.
    viewport_reported: (f32, f32),
    scroll_reported: (f32, f32),
    element_scroll_reported: HashMap<NodeId, (f32, f32)>,
    /// The scroll event targets pending, in the order they scrolled
    /// (HTML's "pending scroll event targets"), and those owed a
    /// `scrollend` once no gesture holds them.
    pending_scroll_targets: Vec<ScrollTarget>,
    scrollend_pending: Vec<ScrollTarget>,
    /// The element scroll containers' offsets (`scrolling.rs`): the truth
    /// the layout tree is given and keeps applied.
    scrolls: HashMap<NodeId, (f32, f32)>,
    /// The wheel gesture in progress and the scroller it is latched to.
    wheel_latch: Option<Latch>,
    /// A scrollbar thumb being dragged.
    scroll_drag: Option<ScrollDrag>,
    /// A scrollbar button or track held down, repeating.
    bar_repeat: Option<BarRepeat>,
    /// The scrollbar part under the pointer and the one pressed, for the
    /// highlights: the target, whether the vertical bar, the part.
    bar_hover: Option<(ScrollTarget, bool, ScrollbarPart)>,
    bar_pressed: Option<(ScrollTarget, bool, ScrollbarPart)>,
    /// Middle-click autoscroll.
    pan: Option<Pan>,
    /// The scrolls in flight, one per target (`scrolling.rs`).
    scroll_animations: HashMap<ScrollTarget, ScrollAnimation>,
    /// The node under the last primary press: scrolling keys start their
    /// search for a scroller there when nothing is focused (Blink's
    /// mouse press node).
    press_scroll_start: Option<NodeId>,
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

    history: Vec<HistoryEntry>,
    history_index: usize,
    /// The next document sequence number.
    next_document: u64,
    /// The URL's fragment must be scrolled to after the next layout.
    pending_fragment: bool,
    /// Whether that scrolls (it only moves `:target` otherwise).
    fragment_scrolls: bool,
    /// The tab changed the layout tree (a layout, a scroll) since the
    /// script view last indexed its boxes.
    view_rects_stale: bool,
    /// A reload or a traversal reached an entry with a scroll position to
    /// restore: tried at each layout until the page reaches it or has
    /// loaded, given up once anything else scrolls the page
    /// (`restore_applied`: where the last try left it).
    pending_scroll_restore: Option<(f32, f32)>,
    restore_applied: Option<(f32, f32)>,
    /// The requests scripts have under way, by the script's id for each,
    /// and the network request each one's current hop is.
    script_fetches: HashMap<u64, ScriptFetch>,
    net_to_script: HashMap<RequestId, u64>,
    /// The key the next request that outlives its document is kept
    /// under: from the top of the range down, clear of the scripts' ids.
    next_detached: u64,
    preflights: SharedPreflights,
    /// The browser's local storage and the tab's session storage, and
    /// the current document's view of them (none for an opaque origin).
    stores: TabStores,
    origin_storage: Option<std::rc::Rc<OriginStorage>>,
    /// A `<meta http-equiv=refresh>` or `Refresh` header due at the instant.
    refresh: Option<(std::time::Instant, Url)>,
    /// `Refresh` header of the main response, applied when the document lands.
    refresh_header: Option<String>,

    /// Styles must be recomputed from scratch (new document, sheet, or
    /// viewport); implies layout.
    needs_style: bool,
    needs_layout: bool,
    needs_paint: bool,
    /// A frame was sent and the shell has not yet said it was shown
    /// (`ShellToTab::FrameShown`): the next paint waits for that, so at
    /// most one frame is on its way and the tab paints at the rate the
    /// screen shows frames, the latest state each time.
    frame_in_flight: bool,
    state_dirty: bool,
}

/// What one batch of events cost the tab, for the `perf` log line
/// (`TabState::end_batch`): the events, and the time and count of each
/// kind of work (a script's time includes the style and layout it
/// forces).
#[derive(Debug, Default)]
struct BatchPerf {
    started: Option<std::time::Instant>,
    shell_events: usize,
    net_events: usize,
    script: std::time::Duration,
    scripts: u32,
    restyle: std::time::Duration,
    full_restyles: u32,
    partial_restyles: u32,
    styled: usize,
    layout: std::time::Duration,
    layouts: u32,
    paint: std::time::Duration,
    paints: u32,
    /// Why style or layout was asked for, in order (each reason once).
    why: Vec<&'static str>,
    /// Time spent handling each kind of event: (kind, count, time).
    events: Vec<(&'static str, u32, std::time::Duration)>,
    tick: std::time::Duration,
    flush: std::time::Duration,
}

impl TabState {
    /// Ask for layout, saying why (the timing log lists the reasons). What
    /// changed is not described: every box is built again.
    fn want_layout(&mut self, why: &'static str) {
        self.note_unknown_box_change();
        self.want_layout_noted(why);
    }

    /// Ask for layout after the changes were told to the engine
    /// (`note_box_changes`): only their boxes are built again.
    fn want_layout_noted(&mut self, why: &'static str) {
        self.needs_layout = true;
        if !self.perf.why.contains(&why) {
            self.perf.why.push(why);
        }
    }

    /// Ask for a full restyle (and so layout), saying why. The restyle
    /// tells layout which elements' styles changed (`restyle_all`); a
    /// change it cannot see (the tree changed undescribed, a control's
    /// state) is noted by the caller.
    fn want_style(&mut self, why: &'static str) {
        self.needs_style = true;
        if !self.perf.why.contains(&why) {
            self.perf.why.push(why);
        }
    }

    /// Incremental layout: tell the engine which elements' styles or
    /// attributes changed and which parents' children did.
    fn note_box_changes(&mut self, elements: &[NodeId], parents: &[NodeId]) {
        match &mut self.engine {
            Some(engine) => engine.note_box_changes(elements, parents),
            None => self.box_changes_unknown = true,
        }
    }

    /// Incremental layout: something changed that is not described.
    fn note_unknown_box_change(&mut self) {
        match &mut self.engine {
            Some(engine) => engine.note_unknown_box_change(),
            None => self.box_changes_unknown = true,
        }
    }

    /// The timing log: an event of `kind` took `time` to handle.
    pub fn note_event(&mut self, kind: &'static str, time: std::time::Duration) {
        match self.perf.events.iter_mut().find(|e| e.0 == kind) {
            Some(e) => {
                e.1 += 1;
                e.2 += time;
            }
            None => self.perf.events.push((kind, 1, time)),
        }
    }

    /// The timing log: the batch's timers and its flush took this long.
    pub fn note_tick_and_flush(&mut self, tick: std::time::Duration, flush: std::time::Duration) {
        self.perf.tick += tick;
        self.perf.flush += flush;
    }
    /// A batch of events starts: the timing log counts from here.
    pub fn begin_batch(&mut self, shell_events: usize, net_events: usize) {
        self.perf = BatchPerf {
            started: Some(std::time::Instant::now()),
            shell_events,
            net_events,
            ..BatchPerf::default()
        };
    }

    /// The batch is done: one `perf` line (target `perf`, level info) when
    /// it did any work or took over 2 ms, with the heap live and its peak
    /// (when the process counts its allocations).
    pub fn end_batch(&mut self) {
        let p = std::mem::take(&mut self.perf);
        let Some(started) = p.started else { return };
        let total = started.elapsed();
        let worked = p.scripts + p.full_restyles + p.partial_restyles + p.layouts + p.paints > 0;
        if !worked && total < std::time::Duration::from_millis(2) {
            return;
        }
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        let events: Vec<String> = p.events.iter().map(|(k, n, t)| format!("{k}x{n}={:.1}", ms(*t))).collect();
        tracing::info!(
            target: "perf",
            tab = self.id.0,
            batch_ms = format!("{:.1}", ms(total)),
            shell_events = p.shell_events,
            net_events = p.net_events,
            events_ms = events.join(","),
            tick_ms = format!("{:.1}", ms(p.tick)),
            flush_ms = format!("{:.1}", ms(p.flush)),
            why = p.why.join("; "),
            script_ms = format!("{:.1}", ms(p.script)),
            scripts = p.scripts,
            restyle_ms = format!("{:.1}", ms(p.restyle)),
            full_restyles = p.full_restyles,
            partial_restyles = p.partial_restyles,
            styled = p.styled,
            layout_ms = format!("{:.1}", ms(p.layout)),
            layouts = p.layouts,
            paint_ms = format!("{:.1}", ms(p.paint)),
            paints = p.paints,
            nodes = self.doc.as_ref().map_or(0, |d| d.node_count()),
            heap_mib = format!("{:.1}", browser_platform::mib(browser_platform::heap_live())),
            heap_peak_mib = format!("{:.1}", browser_platform::mib(browser_platform::heap_peak())),
            "batch"
        );
    }

    pub fn new(id: TabId, net: Arc<NetService>, net_sink: Sink, stores: TabStores, viewport: Viewport, output: Arc<OutputSink>) -> Self {
        Self {
            next_document: 1,
            fragment_scrolls: true,
            view_rects_stale: false,
            pending_scroll_restore: None,
            restore_applied: None,
            script_fetches: HashMap::new(),
            net_to_script: HashMap::new(),
            next_detached: u64::MAX,
            preflights: SharedPreflights::default(),
            stores,
            origin_storage: None,
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
            fetched_images: Default::default(),
            wanted_images: Default::default(),
            failed_images: Default::default(),
            deferred_images: Vec::new(),
            loaded_sheets: Default::default(),
            images: ImageStore::new(),
            web_fonts: WebFonts::default(),
            doc_cache: CacheMode::Default,
            engine: Some(LayoutEngine::new()),
            styles: StyleMap::new(),
            layout: None,
            layouts: 0,
            perf: BatchPerf::default(),
            box_changes_unknown: false,
            scene: Scene::new(),
            view: View::default(),
            window_size: (viewport.width, viewport.height),
            screen_size: (viewport.width, viewport.height),
            image_sizes: ImageSizeMap::default(),
            host_layout_fresh: false,
            doc_header_policy: ReferrerPolicy::Empty,
            document_referrer: None,
            sync_referrer: Default::default(),
            font_clock: FontClock::default(),
            states: ElementStates::default(),
            deps: InteractionDeps::default(),
            hover: None,
            focus: None,
            cursor_image_sent: None,
            caret: 0,
            sel_anchor: None,
            control_drag: false,
            composition: None,
            last_select: None,
            caret_reported: None,
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
            element_scroll_reported: HashMap::new(),
            pending_scroll_targets: Vec::new(),
            scrollend_pending: Vec::new(),
            scrolls: HashMap::new(),
            wheel_latch: None,
            scroll_drag: None,
            bar_repeat: None,
            bar_hover: None,
            bar_pressed: None,
            pan: None,
            scroll_animations: HashMap::new(),
            press_scroll_start: None,
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
            frame_in_flight: false,
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
        // Not input: the next paint may go (in this batch's flush), and
        // nothing else changes, an IME composition waiting for its commit
        // included.
        if let ShellToTab::FrameShown = msg {
            self.frame_in_flight = false;
            return;
        }
        // A composition the IME cleared is a cancel unless a commit comes
        // next.
        if !matches!(msg, ShellToTab::ImeCommit { .. }) {
            self.settle_composition();
        }
        match msg {
            // The user's own navigation: no referrer (HTML §7.4.5, a
            // request with no client).
            ShellToTab::Navigate { url } => self.go(url, NavKind::Push, NavReferrer::default()),
            ShellToTab::NavigateFrom { url, referrer } => self.go(url, NavKind::Push, referrer),
            ShellToTab::NavigateWithBody { url, post, referrer } => {
                self.cancel_pending_nav();
                self.navigate_with(url, NavKind::Push, Some(post), referrer);
            }
            ShellToTab::Reload => {
                // The position to come back to after the reload.
                if self.nav.is_none() {
                    self.leave_entry();
                }
                // Reloading while a navigation is pending restarts that one,
                // with its referrer; else the entry's request referrer and
                // policy are used again (HTML §7.4.3 reload, §7.4.5).
                let pending = self.nav.as_ref().map(|n| (n.url.clone(), n.referrer.clone()));
                let target = pending.or_else(|| self.url.clone().map(|u| (u, self.entry_referrer(self.history_index))));
                if let Some((url, referrer)) = target {
                    self.go(url, NavKind::Reload, referrer);
                }
            }
            ShellToTab::Stop => self.stop(),
            ShellToTab::GoBack => {
                if self.history_index > 0 {
                    self.traverse_to(self.history_index - 1);
                }
            }
            ShellToTab::GoForward => {
                if self.history_index + 1 < self.history.len() {
                    self.traverse_to(self.history_index + 1);
                }
            }
            ShellToTab::Resize(vp) => {
                if vp != self.viewport {
                    self.viewport = vp;
                    // Media queries may change with the viewport.
                    self.want_style("viewport resized");
                    // And the image each `srcset` or `<picture>` selects.
                    self.images_react_to_environment();
                }
            }
            ShellToTab::Screen { window, screen } => {
                self.window_size = window;
                self.screen_size = screen;
            }
            // Pointer moves and scrolls arrive in bursts; hover is
            // re-evaluated once per batch, in `flush`, at the final position.
            ShellToTab::Scroll { dx, dy } => {
                // `wheel` first: a listener may take the scroll.
                if !self.fire_wheel(dx, dy) {
                    return;
                }
                self.wheel_scroll(dx, dy);
            }
            ShellToTab::MouseMove { x, y } => {
                self.mouse = Some((x, y));
                self.pan_move(x, y);
                if !self.scrollbar_mouse_move(x, y) {
                    self.hover_dirty = true;
                    self.pointer_moved = true;
                }
                if self.control_drag {
                    self.control_drag_to(x, y);
                }
                if self.select_anchor.is_some() {
                    self.extend_selection_to(x, y);
                    // Outside a scroller's port (or the viewport) the
                    // scroller scrolls toward the pointer until it comes
                    // back.
                    let outside = self.autoscroll_wanted(x, y);
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
                // A press while middle-click autoscroll is on only ends it.
                if self.pan.is_some() {
                    self.stop_pan();
                    return;
                }
                // The scrollbars are above the page: no press reaches it.
                if button == MouseButton::Left && self.scrollbar_mouse_down(x, y, self.modifiers.shift) {
                    return;
                }
                if self.over_scrollbar_area(x, y) {
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
                // `pointerdown` first; its default action is `mousedown`'s
                // (Pointer Events: focus and selection start), and
                // cancelling it also holds the mouse events back until
                // the pointer goes up. Either cancelled: no focus change,
                // no caret, no selection. `:active`, the press and `click`
                // happen either way.
                let pointer_ok = self.fire_pointer("pointerdown", hit, x, y, button);
                if !pointer_ok {
                    self.compat_suppressed = true;
                }
                let mouse_ok = if self.compat_suppressed {
                    true
                } else {
                    self.fire_mouse("mousedown", self.hover, x, y, button, clicks)
                };
                let proceed = pointer_ok && mouse_ok;
                let hit = self.hover;
                // A press inside the focused text control edits the
                // selection within it, not the page's.
                let mut in_control = false;
                if button == MouseButton::Left {
                    self.press_scroll_start = hit;
                    self.set_active(hit);
                    if proceed {
                        let focus = hit.and_then(|h| self.focusable_ancestor(h));
                        self.set_focus(focus, false);
                        if let Some(f) = self.text_focus()
                            && hit.and_then(|h| self.ancestor_or_self(h, is_text_control)) == Some(f)
                        {
                            self.control_press(f, x, y, clicks);
                            in_control = true;
                        }
                    }
                    self.press_control = hit.and_then(|h| self.ancestor_or_self(h, is_toggle));
                }
                if matches!(button, MouseButton::Left | MouseButton::Middle) {
                    self.press = hit.and_then(|h| self.ancestor_or_self(h, is_link)).map(|l| (l, button));
                }
                if button == MouseButton::Left && proceed && !in_control {
                    let on_link = self.press.is_some();
                    self.begin_selection(x, y, clicks, on_link);
                }
                // A middle press off a link starts autoscroll (unless a
                // listener cancelled the press), as browsers do on
                // Windows and Linux.
                if button == MouseButton::Middle && proceed && self.press.is_none() {
                    self.start_pan(x, y, hit);
                }
            }
            ShellToTab::MouseUp { x, y, button } => {
                self.mouse = Some((x, y));
                if button == MouseButton::Left {
                    self.select_anchor = None;
                    self.control_drag = false;
                    self.autoscroll = None;
                }
                if button == MouseButton::Middle {
                    self.pan_release();
                }
                self.buttons &= !button_bit(button);
                if self.scrollbar_mouse_up() {
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
                // A disabled form control gets no `click` (HTML §4.10.18.2),
                // and nothing follows from one.
                let on_disabled = matches!(
                    (click_target, &self.doc),
                    (Some(t), Some(d)) if d.disabled_state(t) == Some(true)
                        && d.element(t).is_some_and(|e| matches!(&*e.name.local, "button" | "input" | "select" | "textarea"))
                );
                let proceed = match button {
                    MouseButton::Left if on_disabled => false,
                    MouseButton::Left => {
                        // An image button's selected coordinate: where in
                        // it the click landed (whole CSS pixels).
                        let coordinate = click_target.map(|t| {
                            let (ox, oy) = self.click_offset(t, x, y);
                            (ox.floor() as i32, oy.floor() as i32)
                        });
                        self.click_with_activation(click_target, coordinate, |s| s.fire_click("click", click_target, x, y, button, clicks))
                    }
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
                    self.press_control = None;
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
                if let Some(f) = self.text_focus() {
                    self.select_all_in_control(f);
                } else {
                    let extent = self.layout.as_ref().and_then(selection::text_extent);
                    self.set_selection(extent);
                }
            }
            ShellToTab::Copy => {
                if self.copy_from_control() {
                    return;
                }
                if let Some(text) = self.selected_text()
                    && !text.is_empty()
                {
                    self.send(TabToShell::CopyText { text });
                }
            }
            ShellToTab::Cut => self.cut_from_control(),
            ShellToTab::Paste { text } => self.paste_into_control(&text),
            ShellToTab::ImePreedit { text, cursor } => self.ime_preedit(&text, cursor),
            ShellToTab::ImeCommit { text } => self.ime_commit(&text),
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
                {
                    self.click_with_activation(Some(f), Some((0, 0)), |s| s.fire_synthetic_click(f));
                }
            }
            ShellToTab::Modifiers { shift, ctrl, alt, meta } => {
                self.modifiers = Modifiers { shift, ctrl, alt, meta };
            }
            ShellToTab::Close | ShellToTab::FrameShown => {}
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
        // Any key ends middle-click autoscroll.
        self.stop_pan();
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
            // Enter in an input commits the value (`change` if edited),
            // then submits its form implicitly, unless `keypress` was
            // cancelled.
            if key == Key::Enter && !textarea && !ctrl && !alt {
                self.fire_change_if_edited(f);
                if keypress_ok {
                    self.implicit_submission(f);
                }
                return;
            }
            if keypress_ok {
                self.edit_key(f, key, shift, ctrl, alt);
            }
            return;
        }
        // An input that is no button (a checkbox, a radio, ...) submits its
        // form implicitly on Enter, as a text field does.
        let other_input = focus
            .and_then(|f| self.doc.as_ref()?.input_type(f))
            .is_some_and(|t| !matches!(t, browser_dom::forms::InputType::Submit | browser_dom::forms::InputType::Image | browser_dom::forms::InputType::Reset | browser_dom::forms::InputType::Button));
        match key {
            Key::Enter if link && !ctrl && !alt => {
                if self.fire_synthetic_click(f) {
                    self.follow_link(f);
                }
            }
            // Enter clicks a button (on `keypress`), with its activation.
            Key::Enter if button && !ctrl && !alt => {
                if keypress_ok {
                    self.click_with_activation(Some(f), Some((0, 0)), |s| s.fire_synthetic_click(f));
                }
            }
            Key::Enter if other_input && !ctrl && !alt => {
                if keypress_ok {
                    self.implicit_submission(f);
                }
            }
            Key::Space if toggle || button => self.space_armed = Some(f),
            Key::ArrowUp | Key::ArrowDown if select => self.step_select(f, key == Key::ArrowDown),
            Key::ArrowDown => self.keyboard_scroll(KeyScroll::Line(0.0, LINE_SCROLL)),
            Key::ArrowUp => self.keyboard_scroll(KeyScroll::Line(0.0, -LINE_SCROLL)),
            Key::ArrowRight => self.keyboard_scroll(KeyScroll::Line(LINE_SCROLL, 0.0)),
            Key::ArrowLeft => self.keyboard_scroll(KeyScroll::Line(-LINE_SCROLL, 0.0)),
            Key::PageDown => self.keyboard_scroll(KeyScroll::Page(1.0)),
            Key::PageUp => self.keyboard_scroll(KeyScroll::Page(-1.0)),
            Key::Space => self.keyboard_scroll(KeyScroll::Page(if shift { -1.0 } else { 1.0 })),
            Key::Home => self.keyboard_scroll(KeyScroll::Home),
            Key::End => self.keyboard_scroll(KeyScroll::End),
            _ => {}
        }
    }

    /// The text a text control shows and the caret moves in: what the
    /// user typed, else its value ("Working sites first" line 8: the value
    /// is the control's state, never its `value` attribute or children).
    fn control_value(&self, id: NodeId) -> String {
        let Some(doc) = &self.doc else { return String::new() };
        doc.control_display_value(id)
    }

    /// The user edited a text control: its value is `value` (dirty).
    fn set_control_value(&mut self, id: NodeId, value: &str) {
        let Some(doc) = &mut self.doc else { return };
        doc.set_value_by_user(id, value);
        self.controls_changed();
    }

    /// Controls whose state changed (the user's edits, a toggle, a select
    /// step, a reset, what a tree change settled): `:checked` restyles
    /// what it reaches and their boxes are built again, through the same
    /// path as a script's changes.
    fn controls_changed(&mut self) {
        let changed = match &mut self.doc {
            Some(doc) => {
                doc.settle_forms();
                doc.take_controls_changed()
            }
            None => return,
        };
        if changed.is_empty() {
            return;
        }
        let changes: Vec<browser_script::DomChange> =
            changed.iter().map(|&element| browser_script::DomChange::Control { element, generation: 0 }).collect();
        if !self.restyle_changes(&changes) {
            self.note_box_changes(&changed, &[]);
            if self.layout.is_some() {
                self.want_style("control state changed before styles were current");
                self.want_layout_noted("control state changed before styles were current");
            }
        }
        self.needs_paint = true;
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
                let mine = frag.node.is_some_and(|n| n == f || (doc.parent(n) == Some(f) && doc.get(n).pseudo_kind().is_none()));
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
            // The caret stays in view: the control scrolls to it (an
            // input sideways, a textarea down), then the scrollers above
            // and the page, as browsers reveal a moved caret.
            if let (Some(rect), Some(control)) = (caret, self.text_focus()) {
                self.reveal_caret(control, rect);
            }
        }
    }

    /// Scroll the control and everything above it so the caret shows;
    /// the caret rectangle is placed again after, since it moved with
    /// the control's content.
    fn reveal_caret(&mut self, control: NodeId, rect: Rect) {
        // The control itself is a scroll container (`overflow: auto` on
        // a textarea, `hidden` on an input): the chain starts with it.
        let chain = self.scroll_chain(Some(control));
        let mut rect = rect;
        for t in chain {
            let Some(info) = self.scroll_info(t) else { continue };
            let port = match t {
                ScrollTarget::Page => info.scrollport.translate(self.scroll_x, self.scroll_y),
                ScrollTarget::Element(_) => info.scrollport,
            };
            let mut dy = 0.0;
            if rect.bottom() > port.bottom() {
                dy = rect.bottom() - port.bottom();
            }
            if rect.y < port.y {
                dy = rect.y - port.y;
            }
            let mut dx = 0.0;
            if rect.right() > port.right() {
                dx = rect.right() - port.right();
            }
            if rect.x < port.x {
                dx = rect.x - port.x;
            }
            if dx == 0.0 && dy == 0.0 {
                continue;
            }
            let before = info.offset;
            if self.scroll_by(t, dx, dy, false)
                && let ScrollTarget::Element(_) = t
                && let Some(after) = self.scroll_info(t)
            {
                // The content, caret included, moved with the scroll (by
                // what the clamp allowed).
                rect = rect.translate(before.0 - after.offset.0, before.1 - after.offset.1);
            }
        }
        self.caret_rect = Some(rect);
    }

    /// The form control a `click` at `target` activates (DOM dispatch's
    /// activation target): the target or its nearest ancestor with
    /// activation behavior, unless a link comes first (links keep their
    /// own handling).
    fn activation_control(&self, target: NodeId) -> Option<NodeId> {
        let doc = self.doc.as_ref()?;
        if !doc.contains(target) {
            return None;
        }
        let first = std::iter::once(target)
            .chain(doc.ancestors(target))
            .find(|&n| doc.has_form_activation(n) || doc.element(n).is_some_and(is_link))?;
        doc.has_form_activation(first).then_some(first)
    }

    /// A `click` (`fire` dispatches it, returning whether it was not
    /// cancelled) at `target`, with the form controls' activation around
    /// it: a checkbox flips and a radio checks before the listeners run
    /// (legacy-pre-activation), back when one cancels, and after it the
    /// activation behavior runs: `input` and `change`, a submission (an
    /// image button with `coordinate`), a reset.
    fn click_with_activation(&mut self, target: Option<NodeId>, coordinate: Option<(i32, i32)>, fire: impl FnOnce(&mut Self) -> bool) -> bool {
        let control = target.and_then(|t| self.activation_control(t));
        let saved = match (control, &mut self.doc) {
            (Some(c), Some(doc)) => Some(doc.pre_activate(c)),
            _ => None,
        };
        if saved.is_some() {
            self.controls_changed();
        }
        let proceed = fire(self);
        if let (Some(c), Some(saved)) = (control, saved) {
            if !self.doc.as_ref().is_some_and(|d| d.contains(c)) {
                return proceed;
            }
            if proceed {
                self.run_activation(c, coordinate);
            } else {
                if let Some(doc) = &mut self.doc {
                    doc.cancel_activation(c, saved);
                }
                self.controls_changed();
            }
        }
        proceed
    }

    /// A form control's activation behavior.
    fn run_activation(&mut self, control: NodeId, coordinate: Option<(i32, i32)>) {
        let Some(activation) = self.doc.as_ref().map(|d| d.activation(control)) else { return };
        match activation {
            browser_dom::forms::Activation::Changed => self.fire_control_changed(control),
            browser_dom::forms::Activation::Submit { form } => {
                let image = self.doc.as_ref().and_then(|d| d.input_type(control)) == Some(browser_dom::forms::InputType::Image);
                self.submit_form(form, Some(control), image.then(|| coordinate.unwrap_or((0, 0))));
            }
            browser_dom::forms::Activation::Reset { form } => self.reset_form(form),
            browser_dom::forms::Activation::Nothing => {}
        }
    }

    /// Submit `form` from `submitter` (HTML §4.10.22.3): its events and
    /// entry list in script, which plans the navigation
    /// (`HostRequest::Submit`); with no script context, the same entry
    /// list and navigation without the events.
    fn submit_form(&mut self, form: NodeId, submitter: Option<NodeId>, coordinate: Option<(i32, i32)>) {
        if self.script.is_some() && self.parser.is_none() {
            self.sync_document_info();
            self.with_script(|host| host.submit_form(form, submitter, coordinate));
            self.after_script();
            return;
        }
        let Some(doc) = &self.doc else { return };
        if !doc.is_connected(form) {
            return;
        }
        let encoding = doc.form_encoding(form);
        let entries = doc.construct_entry_list(form, submitter, encoding, coordinate.unwrap_or((0, 0)));
        let boundary = browser_dom::forms::new_boundary();
        if let Some(browser_dom::forms::SubmitAction::Navigate { url, post, target, noreferrer }) =
            doc.submission_action(form, submitter, &entries, encoding, coordinate, &boundary)
        {
            self.navigate_submission(url, post, &target, noreferrer);
        }
    }

    /// Reset `form` as the user asked: `reset` in script, then the reset
    /// algorithms.
    fn reset_form(&mut self, form: NodeId) {
        if self.script.is_some() && self.parser.is_none() {
            self.sync_document_info();
            self.with_script(|host| host.reset_form(form));
            self.after_script();
            return;
        }
        // The `<output>`s reset, whose children were replaced.
        let mut changes = Vec::new();
        if let Some(doc) = &mut self.doc {
            for c in doc.form_resettable_elements(form) {
                doc.reset_control(c);
                // An `<output>`'s reset: string replace all with its
                // default value.
                if let Some(text) = doc.output_default_value(c) {
                    let held = doc.children(c).any(|n| {
                        std::iter::once(n).chain(doc.descendants(n)).any(|d| {
                            doc.element(d).is_some_and(|e| e.is_html() && matches!(&*e.name.local, "style" | "link" | "base"))
                        })
                    });
                    doc.set_text_content(c, &text);
                    if doc.is_connected(c) {
                        changes.push(if held {
                            browser_script::DomChange::Sheets { parent: c, generation: 0 }
                        } else {
                            browser_script::DomChange::Children {
                                parent: c,
                                inserted: doc.children(c).collect(),
                                generation: 0,
                            }
                        });
                    }
                }
            }
        }
        self.controls_changed();
        if !changes.is_empty() {
            // As after a script's tree change: restyle, lay out, drop the
            // references to removed nodes, run the image algorithms.
            self.dom_changed(&changes);
            self.drive_images();
        }
    }

    /// Implicit submission (HTML §4.10.22.2): Enter in a field of a form
    /// clicks its default button, or with none submits the form when at
    /// most one field blocks implicit submission.
    fn implicit_submission(&mut self, field: NodeId) {
        let Some(doc) = &self.doc else { return };
        let Some(form) = doc.form_owner(field) else { return };
        match doc.default_button(form) {
            Some(button) => {
                if doc.disabled_state(button) == Some(true) {
                    return;
                }
                self.click_with_activation(Some(button), Some((0, 0)), |s| s.fire_synthetic_click(button));
            }
            None => {
                if doc.implicit_submission_blockers(form) <= 1 {
                    self.submit_form(form, None, None);
                }
            }
        }
    }

    /// Arrow keys on a focused drop-down select pick the next (or
    /// previous) option that is not disabled; `input` and `change` follow.
    fn step_select(&mut self, id: NodeId, down: bool) {
        let Some(doc) = &mut self.doc else { return };
        if !doc.control_is_mutable(id) {
            return;
        }
        let options = doc.list_of_options(id);
        let current = options.iter().position(|&o| doc.option_selectedness(o));
        let candidates: Box<dyn Iterator<Item = usize>> = match (current, down) {
            (Some(c), true) => Box::new(c + 1..options.len()),
            (Some(c), false) => Box::new((0..c).rev()),
            (None, _) => Box::new(0..options.len()),
        };
        let Some(next) = candidates.into_iter().find(|&i| !doc.option_disabled(options[i])) else { return };
        doc.pick_option(id, options[next]);
        self.controls_changed();
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
            // Scrollers with nothing focusable inside take keyboard focus
            // too (Chrome's keyboard-focusable scrollers).
            if !rendered.contains(&n) || !(is_focusable(doc, n) || self.keyboard_focusable_scroller(n)) {
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

    /// Scroll so the focused element is inside every scroller above it
    /// and the viewport, with a margin, its `scroll-margin` counted. (The
    /// focus ring itself is the `:focus-visible` element's `outline:
    /// auto` from the UA sheet, painted with the page.)
    fn scroll_focus_into_view(&mut self) {
        let Some(focus) = self.focus else { return };
        self.reveal(focus, &|tree, doc| element_rect(tree, doc, focus), Reveal::Nearest(24.0), true, true);
        if self.focus.is_some() {
            self.update_caret();
        }
    }

    /// The outermost ancestor-or-self of `node` whose used `user-select`
    /// is `pred`'s (`all` takes the outermost, `contain` the nearest).
    fn user_select_root(&self, node: NodeId, want: browser_style::UserSelect, outermost: bool) -> Option<NodeId> {
        let doc = self.doc.as_ref()?;
        let mut found = None;
        let mut cur = Some(node);
        while let Some(n) = cur {
            if doc.get(n).is_element() && self.styles.get(n).is_some_and(|s| s.user_select_used == want) {
                found = Some(n);
                if !outermost {
                    break;
                }
            }
            cur = doc.parent(n);
        }
        found
    }

    /// The first and last selectable text positions inside an element, in
    /// tree order.
    fn element_text_extent(&self, node: NodeId) -> Option<(TextPos, TextPos)> {
        let (doc, tree) = (self.doc.as_ref()?, self.layout.as_ref()?);
        let mut first = None;
        let mut last = None;
        tree.root.walk(&mut |f| {
            let (browser_layout::FragmentContent::Text(t), Some(n)) = (&f.content, f.node) else { return };
            if !t.selectable {
                return;
            }
            let mut cur = Some(n);
            let inside = loop {
                match cur {
                    Some(c) if c == node => break true,
                    Some(c) => cur = doc.parent(c),
                    None => break false,
                }
            };
            if !inside {
                return;
            }
            let start = TextPos { node: n, offset: t.range.start };
            let end = TextPos { node: n, offset: t.range.end };
            if first.is_none() {
                first = Some(start);
            }
            last = Some(end);
        });
        Some((first?, last?))
    }

    /// `user-select: all` and `contain` on a selection end: a position
    /// inside an `all` element becomes that element's start or end
    /// (whichever is on `anchor`'s far side), a focus outside the
    /// `contain` element the anchor is in is held at that element's edge.
    fn constrain_selection_focus(&self, anchor: TextPos, focus: TextPos) -> TextPos {
        use browser_style::UserSelect;
        let Some(tree) = self.layout.as_ref() else { return focus };
        let before = |a: TextPos, b: TextPos| {
            let keys = selection::position_keys(tree, &[a, b]);
            matches!((keys[0], keys[1]), (Some(x), Some(y)) if x < y)
        };
        let anchor_el = self.element_of(anchor.node);
        let focus_el = self.element_of(focus.node);
        let mut focus = focus;
        if let Some(c) = anchor_el.and_then(|e| self.user_select_root(e, UserSelect::Contain, false))
            && focus_el.and_then(|e| self.user_select_root(e, UserSelect::Contain, false)) != Some(c)
            && let Some((start, end)) = self.element_text_extent(c)
        {
            focus = if before(focus, anchor) { start } else { end };
        }
        if let Some(a) = focus_el.and_then(|e| self.user_select_root(e, UserSelect::All, true))
            && anchor_el.and_then(|e| self.user_select_root(e, UserSelect::All, true)) != Some(a)
            && let Some((start, end)) = self.element_text_extent(a)
        {
            focus = if before(focus, anchor) { start } else { end };
        }
        focus
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

    /// Bring the current match into view, a third of the way down, through
    /// every scroller above it and the viewport. The highlights are
    /// placed again after, since the match may have moved.
    fn scroll_to_current_match(&mut self) {
        let Some((a, b)) = self.find.as_ref().and_then(|f| f.current.map(|i| f.matches[i])) else {
            return;
        };
        let Some(node) = self.element_of(a.node) else { return };
        self.reveal(node, &|tree, _| selection::first_rect(tree, a, b), Reveal::Third, false, false);
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
        // A press in a `user-select: all` element selects all of it.
        let all = pos
            .and_then(|p| self.element_of(p.node))
            .and_then(|e| self.user_select_root(e, browser_style::UserSelect::All, true))
            .and_then(|a| self.element_text_extent(a));
        let sel = match (clicks, pos, all) {
            (_, Some(_), Some((start, end))) => {
                if !on_link {
                    self.select_anchor = Some(start);
                }
                Some((start, end))
            }
            (2, Some(p), _) => selection::word_at(tree, p),
            (3, Some(p), _) => selection::paragraph_at(tree, p),
            (_, Some(p), _) => {
                if !on_link {
                    self.select_anchor = Some(p);
                }
                None
            }
            _ => None,
        };
        self.set_selection(sel);
    }

    /// Move the selection's focus to the pointer during a drag, within
    /// what `user-select` allows.
    fn extend_selection_to(&mut self, x: f32, y: f32) {
        let Some(anchor) = self.select_anchor else { return };
        if let Some(focus) = self.text_position_at(x, y) {
            let focus = self.constrain_selection_focus(anchor, focus);
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
            let referrer = self.link_referrer(link);
            self.go(url, NavKind::Push, referrer);
        }
    }

    // ----- referrers (plan O23) -----

    /// The document's referrer policy now (its policy container's).
    fn document_policy(&self) -> ReferrerPolicy {
        match (&self.parser, &self.doc) {
            (Some(parser), _) => parser.document().referrer_policy,
            (None, Some(doc)) => doc.referrer_policy,
            (None, None) => ReferrerPolicy::Empty,
        }
    }

    /// Fetch's "client" referrer of a request the document makes: its
    /// URL (none for an opaque origin), under `policy`, else the
    /// document's own.
    fn client_referrer(&self, policy: ReferrerPolicy) -> Option<RequestReferrer> {
        let url = self.url.clone()?;
        if !url.origin().is_tuple() {
            return None;
        }
        Some(RequestReferrer {
            source: url,
            policy: policy.or(self.document_policy()),
        })
    }

    /// A navigation the document starts with `policy` (an element's,
    /// `Empty` for none).
    fn page_referrer(&self, policy: ReferrerPolicy) -> NavReferrer {
        NavReferrer {
            request: if policy == ReferrerPolicy::NoReferrer { None } else { self.client_referrer(policy) },
            policy,
        }
    }

    /// The referrer of an `<a>` or `<area>`'s navigation: none for
    /// `rel=noreferrer` (HTML §4.6.6.13: the policy `no-referrer`), else
    /// its `referrerpolicy` (HTML §4.6.4 "follow the hyperlink").
    fn link_referrer(&self, link: NodeId) -> NavReferrer {
        let Some(e) = self.doc.as_ref().and_then(|d| d.element(link)) else { return self.page_referrer(ReferrerPolicy::Empty) };
        let noreferrer = e.attr("rel").is_some_and(|r| r.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("noreferrer")));
        let policy = if noreferrer { ReferrerPolicy::NoReferrer } else { ReferrerPolicy::from_attribute(e.attr("referrerpolicy")) };
        self.page_referrer(policy)
    }

    /// The referrer of a request for an element's resource (`<img>`,
    /// `<script>`, `<link>`): its `referrerpolicy`, else the document's.
    fn element_referrer(&self, element: Option<NodeId>) -> Option<RequestReferrer> {
        let policy = element
            .and_then(|el| match (&self.parser, &self.doc) {
                (Some(parser), _) => parser.document().element(el).map(|e| ReferrerPolicy::from_attribute(e.attr("referrerpolicy"))),
                (None, Some(doc)) => doc.element(el).map(|e| ReferrerPolicy::from_attribute(e.attr("referrerpolicy"))),
                (None, None) => None,
            })
            .unwrap_or(ReferrerPolicy::Empty);
        self.client_referrer(policy)
    }

    /// The referrer of a script's request (`fetch`, XHR, a beacon): the
    /// `Request`'s own `referrer` (a URL of the document's origin) or the
    /// document's, under the `Request`'s `referrerPolicy`, else the
    /// document's policy; none for `referrer: ''`.
    fn script_request_referrer(&self, request: &browser_script::NetRequest) -> Option<RequestReferrer> {
        match &request.referrer {
            browser_script::ScriptReferrer::NoReferrer => None,
            browser_script::ScriptReferrer::Client => self.client_referrer(request.referrer_policy),
            browser_script::ScriptReferrer::Url(source) => Some(RequestReferrer {
                source: (**source).clone(),
                policy: request.referrer_policy.or(self.document_policy()),
            }),
        }
    }

    /// The referrer of a fetch a stylesheet makes: its URL under its
    /// response's policy (else the document's) for an external sheet, the
    /// document's for an inline one or a `style` attribute.
    fn css_referrer(&self, sheet: Option<&RequestReferrer>) -> Option<RequestReferrer> {
        match sheet {
            Some(r) => Some(RequestReferrer {
                source: r.source.clone(),
                policy: r.policy.or(self.document_policy()),
            }),
            None => self.client_referrer(ReferrerPolicy::Empty),
        }
    }

    /// A reload or a traversal that loads the entry at `index`: its
    /// stored referrer again, under its stored policy, else the current
    /// document's (the navigation's source snapshot, HTML §7.4.5).
    fn entry_referrer(&self, index: usize) -> NavReferrer {
        let Some(entry) = self.history.get(index) else { return NavReferrer::default() };
        NavReferrer {
            request: entry.referrer.clone().map(|source| RequestReferrer {
                source,
                policy: entry.referrer_policy.or(self.document_policy()),
            }),
            policy: entry.referrer_policy,
        }
    }

    /// A form submission's navigation (HTML §4.10.22.3 steps 16 to 21):
    /// to `url`, POSTing `post` when given, in the navigable `target`
    /// names: this tab for none, `_self`, `_parent` and `_top` (there are
    /// no frames), a new tab for `_blank` or any name. It replaces the
    /// current entry while the document has not completely loaded. Only
    /// http(s) and `about:` are navigated to: a top-level `data:` from a
    /// page is blocked as in Chrome and Firefox; `javascript:` and
    /// `mailto:` are not opened, as for links.
    fn navigate_submission(&mut self, url: Url, post: Option<browser_dom::forms::PostResource>, target: &str, noreferrer: bool) {
        if !matches!(url.scheme(), "http" | "https" | "about") {
            tracing::debug!(tab = self.id.0, "form submission to {url} not navigated");
            return;
        }
        // The form's `rel=noreferrer` navigates with the policy
        // `no-referrer` (HTML §4.10.22.3, "plan to navigate"); forms have
        // no `referrerpolicy` of their own.
        let referrer = self.page_referrer(if noreferrer { ReferrerPolicy::NoReferrer } else { ReferrerPolicy::Empty });
        let origin = self.url.as_ref().map_or_else(|| "null".to_owned(), |u| u.origin().ascii_serialization());
        let post = post.map(|p| browser_ipc_types::PostData {
            content_type: p.content_type,
            body: p.body,
            origin: origin.clone(),
        });
        let t = target.trim().to_ascii_lowercase();
        if !(t.is_empty() || t == "_self" || t == "_parent" || t == "_top") {
            self.send(TabToShell::OpenFormSubmission { url, post, referrer });
            return;
        }
        let kind = if self.lifecycle == Lifecycle::Complete { NavKind::Push } else { NavKind::Replace };
        match post {
            None => self.go(url, kind, referrer),
            Some(post) => {
                // A POST always loads, fragment or not.
                self.cancel_pending_nav();
                self.navigate_with(url, kind, Some(post), referrer);
            }
        }
    }

    /// Ask the shell for a new tab on the link (middle click), with the
    /// link's referrer.
    fn open_link_in_new_tab(&mut self, link: NodeId) {
        if let Some(url) = self.link_target(link) {
            let referrer = self.link_referrer(link);
            self.send(TabToShell::OpenInNewTab { url, referrer });
        }
    }

    /// Go to `url` with `referrer`. A change of fragment within the
    /// displayed document takes effect at once and only scrolls; anything
    /// else starts a load that commits when its first bytes arrive.
    fn go(&mut self, url: Url, kind: NavKind, referrer: NavReferrer) {
        self.cancel_pending_nav();
        if kind != NavKind::Reload && self.is_same_document(&url) {
            tracing::info!(tab = self.id.0, "fragment navigation {url}");
            let old = self.url.clone();
            if kind == NavKind::Push {
                self.leave_entry();
            }
            self.apply_history(url, kind, true);
            self.pending_fragment = true;
            self.state_dirty = true;
            self.same_document_navigated(old);
        } else {
            self.navigate(url, kind, referrer);
        }
    }

    /// Remember where the page is scrolled in the entry being left, for
    /// a traversal back to it.
    fn leave_entry(&mut self) {
        let scroll = (self.scroll_x, self.scroll_y);
        if let Some(entry) = self.history.get_mut(self.history_index) {
            entry.scroll = Some(scroll);
        }
    }

    fn fresh_document(&mut self) -> u64 {
        let document = self.next_document;
        self.next_document += 1;
        document
    }

    /// Back or forward to the entry at `index`: within the document when
    /// the entry is one of its own (`pushState`, a fragment navigation),
    /// else by loading the entry's URL.
    fn traverse_to(&mut self, index: usize) {
        let Some(target) = self.history.get(index).cloned() else { return };
        let same_document = self.history.get(self.history_index).is_some_and(|c| c.document == target.document)
            && self.nav.is_none()
            && (self.doc.is_some() || self.parser.is_some());
        if !same_document {
            let referrer = self.entry_referrer(index);
            self.go(target.url, NavKind::Traverse(index), referrer);
            return;
        }
        if index == self.history_index {
            return;
        }
        tracing::info!(tab = self.id.0, "traversal within the document to {}", target.url);
        let old = self.url.clone();
        self.leave_entry();
        self.history_index = index;
        self.url = Some(target.url.clone());
        self.state_dirty = true;
        // The entry's `:target`, and its scroll position unless the page
        // restores it itself (`history.scrollRestoration = 'manual'`).
        self.pending_fragment = true;
        self.fragment_scrolls = !target.manual_scroll;
        self.pending_scroll_restore = if target.manual_scroll { None } else { target.scroll };
        self.same_document_navigated(old);
    }

    /// One try at restoring the scroll position `to`: given up when the
    /// page is no longer where the last try left it (the user or a script
    /// scrolled); done when it is reached, or once the page has loaded (the
    /// page cannot grow to it any more).
    fn restore_scroll_step(&mut self, to: (f32, f32)) {
        if self.restore_applied.is_some_and(|at| at != (self.scroll_x, self.scroll_y)) {
            self.pending_scroll_restore = None;
            self.restore_applied = None;
            return;
        }
        self.programmatic_scroll(ScrollTarget::Page, to, false);
        self.needs_paint = true;
        let at = (self.scroll_x, self.scroll_y);
        let reached = (at.0 - to.0).abs() < 0.5 && (at.1 - to.1).abs() < 0.5;
        if reached || self.lifecycle == Lifecycle::Complete {
            self.pending_scroll_restore = None;
            self.restore_applied = None;
        } else {
            self.restore_applied = Some(at);
        }
    }

    /// A script navigated to a fragment of the document (`location`): it
    /// adopted the URL, moved its history and fired `popstate` itself; the
    /// entry goes in the list (sharing the document's state) and the page
    /// scrolls to the fragment, with no event from here.
    fn script_fragment_navigated(&mut self, url: Url, replace: bool) {
        self.cancel_pending_nav();
        let kind = if replace { NavKind::Replace } else { NavKind::Push };
        if kind == NavKind::Push {
            self.leave_entry();
        }
        self.apply_history(url.clone(), kind, true);
        self.pending_fragment = true;
        self.state_dirty = true;
        self.document_url_changed(&url);
        let (index, length) = (self.history_index, self.history.len());
        if let (Some(host), Some(entry)) = (&mut self.script, self.history.get(index)) {
            host.set_history(index, length, None, entry.manual_scroll);
        }
    }

    /// The document's URL changed without a load (a fragment navigation
    /// or a traversal between its entries): the document and its scripts
    /// follow, `popstate` fires with the entry's state, and `hashchange`
    /// when the fragment differs (HTML §7.4.6.2, "update document for
    /// history step application").
    fn same_document_navigated(&mut self, old: Option<Url>) {
        let Some(new) = self.url.clone() else { return };
        self.document_url_changed(&new);
        let Some(entry) = self.history.get(self.history_index).cloned() else { return };
        if self.script.is_none() {
            return;
        }
        let (index, length) = (self.history_index, self.history.len());
        self.sync_document_info();
        self.with_script(|host| {
            host.set_history(index, length, entry.state.clone(), entry.manual_scroll);
            host.fire_popstate(entry.state.clone());
        });
        self.after_script();
        if let Some(old) = old
            && old.fragment() != new.fragment()
            && self.url.as_ref() == Some(&new)
        {
            self.sync_document_info();
            self.with_script(|host| host.fire_hashchange(&old, &new));
            self.after_script();
        }
    }

    /// The displayed document's URL is now `new`: what relative URLs
    /// resolve against (unless a `<base>` says otherwise) and what its
    /// storage events name follow.
    fn document_url_changed(&mut self, new: &Url) {
        if self.parser.is_none() {
            self.doc_url = Some(new.clone());
        }
        // The fallback base URL; a `<base>`'s frozen base URL stays.
        let rebase = |doc: &mut Document| doc.set_document_url(Some(new.clone()));
        match (&mut self.parser, &mut self.doc) {
            (Some(parser), _) => rebase(&mut parser.document_mut()),
            (None, Some(doc)) => rebase(doc),
            (None, None) => {}
        }
        if let Some(storage) = &self.origin_storage {
            *storage.url.borrow_mut() = new.to_string();
        }
    }

    /// `history.pushState` (a new entry after the current one) or
    /// `replaceState` (the current one): the entry carries the state and
    /// the URL; the script has already adopted both.
    fn push_state(&mut self, url: Url, state: SerializedState, replace: bool) {
        if replace {
            if let Some(entry) = self.history.get_mut(self.history_index) {
                entry.url = url.clone();
                entry.state = Some(state);
            }
        } else {
            self.leave_entry();
            // The new entry shares the document's state (its referrer too).
            let (document, manual_scroll, referrer, referrer_policy) = match self.history.get(self.history_index) {
                Some(entry) => (entry.document, entry.manual_scroll, entry.referrer.clone(), entry.referrer_policy),
                None => (self.fresh_document(), false, self.document_referrer.clone(), ReferrerPolicy::Empty),
            };
            if !self.history.is_empty() {
                self.history.truncate(self.history_index + 1);
            }
            self.history.push(HistoryEntry {
                url: url.clone(),
                document,
                state: Some(state),
                scroll: None,
                manual_scroll,
                referrer,
                referrer_policy,
            });
            self.cap_history();
        }
        self.url = Some(url.clone());
        self.document_url_changed(&url);
        self.state_dirty = true;
        let (index, length) = (self.history_index, self.history.len());
        if let (Some(host), Some(entry)) = (&mut self.script, self.history.get(index)) {
            host.set_history(index, length, entry.state.clone(), entry.manual_scroll);
        }
    }

    /// Keep the list at the most entries a tab holds (the oldest go) and
    /// make the last one current.
    fn cap_history(&mut self) {
        if self.history.len() > MAX_HISTORY_ENTRIES {
            let excess = self.history.len() - MAX_HISTORY_ENTRIES;
            self.history.drain(..excess);
        }
        self.history_index = self.history.len() - 1;
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
    /// `same_document` says the URL is the displayed document's own (a
    /// fragment navigation): the entry then belongs to that document.
    fn apply_history(&mut self, url: Url, kind: NavKind, same_document: bool) {
        let current = self.history.get(self.history_index).cloned();
        let has_entry = current.is_some();
        // A new entry: the current document's, or a new document's.
        // A new document's entry keeps the referrer its load determined
        // (`commit` set it) and the policy its navigation was given; one of
        // the current document shares that document's.
        let new_entry = |state: &mut Self| {
            let (document, manual_scroll, referrer, referrer_policy) = match (&current, same_document) {
                (Some(c), true) => (c.document, c.manual_scroll, c.referrer.clone(), c.referrer_policy),
                _ => (
                    state.fresh_document(),
                    false,
                    state.document_referrer.clone(),
                    state.nav.as_ref().map_or(ReferrerPolicy::Empty, |n| n.referrer.policy),
                ),
            };
            HistoryEntry {
                url: url.clone(),
                document,
                state: None,
                scroll: None,
                manual_scroll,
                referrer,
                referrer_policy,
            }
        };
        match kind {
            NavKind::Push => {
                if !self.history.is_empty() {
                    self.history.truncate(self.history_index + 1);
                }
                let entry = new_entry(self);
                self.history.push(entry);
                self.cap_history();
            }
            NavKind::Reload if has_entry => {
                // The same entry, document and state: only the URL may
                // have moved (a redirect).
                self.history[self.history_index].url = url.clone();
            }
            NavKind::Replace if has_entry => {
                let entry = new_entry(self);
                self.history[self.history_index] = entry;
            }
            NavKind::Reload | NavKind::Replace => {
                let entry = new_entry(self);
                self.history.push(entry);
                self.history_index = self.history.len() - 1;
            }
            NavKind::Traverse(i) => {
                if let Some(entry) = self.history.get_mut(i) {
                    // A redirect on the way back lands on the new URL.
                    entry.url = url.clone();
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
            self.set_document(doc, true);
        }
        self.state_dirty = true;
        // Nothing is being fetched any more: the document is complete.
        self.update_readiness();
    }

    /// The first bytes of the main response: the navigation is now real.
    fn commit(&mut self, id: RequestId, final_url: Url, referrer: Option<Url>) {
        let Some(nav) = self.nav.as_mut() else { return };
        if nav.request != id {
            return;
        }
        nav.committed = true;
        // The referrer the load determined: `document.referrer`, and the
        // new entry's request referrer (`apply_history`).
        self.document_referrer = referrer;
        let kind = nav.kind;
        tracing::info!(tab = self.id.0, "committed {final_url}");
        // The entry being left keeps where it was scrolled; one reached
        // by a traversal gets its own position back once it is laid out.
        if matches!(kind, NavKind::Push | NavKind::Traverse(_)) {
            self.leave_entry();
        }
        self.apply_history(final_url.clone(), kind, false);
        // A reload and a traversal restore the entry's scroll position
        // (`auto` scroll restoration).
        self.pending_scroll_restore = match kind {
            NavKind::Traverse(_) | NavKind::Reload => self.history.get(self.history_index).filter(|e| !e.manual_scroll).and_then(|e| e.scroll),
            _ => None,
        };
        self.restore_applied = None;
        self.fragment_scrolls = true;
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
        // The new document's images load while it parses; the old one's
        // stay decoded (it is on screen until the new one is) and go when
        // parsing ends, unless the new one asked for them too.
        self.fetched_images.clear();
        self.failed_images.clear();
        self.deferred_images.clear();
        self.wanted_images.clear();
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.scroll_reported = (0.0, 0.0);
        self.element_scroll_reported.clear();
        self.pending_scroll_targets.clear();
        self.scrollend_pending.clear();
        self.scrolls.clear();
        self.scroll_animations.clear();
        self.wheel_latch = None;
        self.scroll_drag = None;
        self.bar_repeat = None;
        self.bar_pressed = None;
        self.press_scroll_start = None;
        self.stop_pan();
        // The new document's scripts run while it parses, so its context
        // starts here; the old document's timers stop with its context.
        self.new_script_host();
        if let Some(host) = &mut self.script {
            host.set_document_referrer(self.document_referrer.clone());
        }
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
            self.fragment_scrolls = true;
            return;
        }
        // A traversal to an entry whose page restores its own scroll
        // position only moves `:target`.
        let scrolls = std::mem::replace(&mut self.fragment_scrolls, true);
        match target {
            _ if !scrolls => {}
            // The element's top at the top of every scroller above it and
            // of the viewport, its `scroll-margin` counted.
            Some(t) => self.reveal(
                t,
                &|tree, doc| tree.first_shown_rect(|n| n == t || (doc.parent(n) == Some(t) && doc.get(n).pseudo_kind().is_none())),
                Reveal::Start,
                true,
                true,
            ),
            None => {
                let smooth = self.smooth_behavior(ScrollTarget::Page);
                self.programmatic_scroll(ScrollTarget::Page, (self.scroll_x, 0.0), smooth);
            }
        }
        self.needs_paint = true;

        let changed = self.states.set_single(target, ElementStates::TARGET);
        self.restyle_for(changed, StateKind::Target);
    }

    /// The tab wants to be woken at this instant even if no event arrives.
    pub fn next_wake(&self) -> Option<std::time::Instant> {
        [
            self.refresh.as_ref().map(|(at, _)| *at),
            self.autoscroll,
            self.bar_repeat.as_ref().map(|r| r.next),
            self.scroll_drag_wake(),
            self.pan_next_wake(),
            self.scrollend_wake(),
            self.scroll_animation_wake(),
            self.script.as_ref().and_then(ScriptHost::next_wake),
            self.frame_due,
            self.web_fonts.next_wake().map(|t| self.font_clock.to_real(t)),
            self.script_fetch_wake(),
            // A `document.fonts` task is due.
            (self.web_fonts.dirty && self.script.is_some()).then(std::time::Instant::now),
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
        self.bar_repeat_tick(now);
        self.scroll_drag_tick(now);
        self.pan_tick(now);
        self.scroll_animations_tick(now);
        // A web font's block period ended (its text is drawn in the
        // fallback) or its swap period (the fallback stays for good): the
        // fonts are walked again after the next layout.
        // No font changes with a period (the engine notes the families of
        // fonts that do change): the boxes stay; the layout is for the
        // walk after it, which decides the invisible text.
        if self.web_fonts.tick(self.font_clock.now()) {
            self.want_layout_noted("web font period ended");
        }
        // `document.fonts`: its promises and events run as a task here,
        // never from inside rendering or a network handler.
        self.report_fonts();
        self.script_fetch_tick(now);
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
            // The document navigates (HTML's shared declarative refresh
            // steps): its referrer, as a link's.
            let referrer = self.page_referrer(ReferrerPolicy::Empty);
            self.go(url, if same { NavKind::Reload } else { NavKind::Push }, referrer);
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
        let doc = self.doc.as_ref()?;
        let mut cur = Some(id);
        while let Some(n) = cur {
            if is_focusable(doc, n) {
                return Some(n);
            }
            cur = doc.parent(n);
        }
        None
    }

    /// Re-evaluate what is under the pointer; restyle if it changed.
    fn update_hover(&mut self) {
        self.hover_dirty = false;
        self.update_bar_hover();
        // The page's scrollbars cover the page under them; an element's
        // scrollbars are part of the element (`hit_test` says so).
        let raw = match self.mouse {
            Some((x, y)) if !self.over_page_scrollbar(x, y) => self.hit_node(x, y),
            _ => None,
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
        // A `::before`/`::after` box is not text (its generated text has
        // no node; what is hit is the pseudo-element's box).
        let over_text = raw.is_some_and(|n| {
            self.doc.as_ref().is_some_and(|d| {
                d.contains(n)
                    && d.get(n).pseudo_kind().is_none()
                    && (!d.get(n).is_element() || d.element(n).is_some_and(is_text_control))
            })
        });
        let over_bar = self.mouse.is_some_and(|(x, y)| self.over_scrollbar_area(x, y));
        // The hovered element's `cursor` (inherited): an image of its list
        // that is decoded and no larger than 128×128 (Chrome's limit),
        // else its keyword; `auto` is the pointer over links, the I-beam
        // over text, the arrow elsewhere.
        // Over a `::before`/`::after` box, its own `cursor`.
        let pseudo_cursor = raw.and_then(|n| {
            let d = self.doc.as_ref()?;
            let kind = d.contains(n).then(|| d.get(n).pseudo_kind()).flatten()?;
            let owner = d.parent(n)?;
            self.styles.get(owner)?.pseudo_style(kind == browser_dom::PseudoKind::After).map(|s| s.cursor.clone())
        });
        let css = pseudo_cursor.or_else(|| hit.and_then(|h| self.styles.get(h)).map(|s| s.cursor.clone()));
        let image = css.as_ref().and_then(|c| {
            c.images.iter().find_map(|i| {
                let url = Url::parse(&i.url).ok()?;
                let img = self.images.get(&url)?;
                (img.width <= 128 && img.height <= 128).then(|| (i.url.clone(), i.hotspot, img.clone()))
            })
        });
        let keyword = css.as_ref().map(|c| c.keyword).unwrap_or_default();
        let cursor = if self.pan.is_some() {
            Cursor::AllScroll
        } else if over_bar {
            Cursor::Default
        } else if keyword != browser_style::CursorKeyword::Auto {
            css_cursor(keyword)
        } else if over_link {
            Cursor::Pointer
        } else if over_text {
            Cursor::Text
        } else {
            Cursor::Default
        };
        match image.filter(|_| self.pan.is_none() && !over_bar) {
            Some((url, hotspot, img)) => {
                if self.cursor_image_sent.as_deref() != Some(&*url) {
                    self.cursor_image_sent = Some(url.url.clone());
                    self.cursor = cursor;
                    let (w, h) = (img.width as u16, img.height as u16);
                    let (hx, hy) = hotspot.map_or((0, 0), |(x, y)| (x.round().clamp(0.0, f32::from(w)) as u16, y.round().clamp(0.0, f32::from(h)) as u16));
                    self.send(TabToShell::CursorImage(browser_ipc_types::CursorImage {
                        width: w,
                        height: h,
                        rgba: img.data.data().to_vec(),
                        hotspot: (hx, hy),
                    }));
                }
            }
            None => {
                if self.cursor_image_sent.take().is_some() {
                    // Back from an image cursor: the keyword one, resent.
                    self.cursor = Cursor::Default;
                    self.send(TabToShell::Cursor(cursor));
                    self.cursor = cursor;
                } else {
                    self.set_cursor(cursor);
                }
            }
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
            self.control_drag = false;
            if let Some(o) = old {
                self.end_composition(o);
                self.fire_blur(o, target);
            }
            self.sel_anchor = None;
            self.last_select = None;
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
            repaint = result.repaint,
            ms = started.elapsed().as_millis(),
            "restyle for interaction"
        );
        // A `:hover::before` may come or go.
        let pseudos = self.sync_pseudos();
        self.perf.restyle += started.elapsed();
        self.perf.partial_restyles += 1;
        self.perf.styled += result.styled;
        // The boxes of the restyled elements are built again at the next
        // layout, even when only paint changed (they carry the styles).
        self.note_box_changes(&result.changed, &[]);
        if result.moved || pseudos {
            // A state may ask for another face (bold on hover): the layout
            // that follows asks for it.
            self.want_layout_noted(if result.moved { "hover/focus/active restyle moved a box" } else { "hover/focus/active pseudo-element came or went" });
        } else if result.repaint {
            // Paint-only (colours, backgrounds, decorations): the boxes
            // stay where they are and take the new styles.
            if let (Some(layout), Some(engine), Some(doc)) = (&mut self.layout, &mut self.engine, &self.doc) {
                engine.refresh_styles(layout, doc, &self.styles);
            }
            self.needs_paint = true;
        }
        if result.repaint {
            // A new style may name an image not fetched yet.
            self.fetch_background_images();
        }
    }

    /// Create and remove the `::before`/`::after` nodes to match the
    /// styles; whether any came or went (layout must run again then).
    fn sync_pseudos(&mut self) -> bool {
        let Some(doc) = &mut self.doc else { return false };
        browser_style::sync_pseudo_elements(doc, &self.styles)
    }

    /// Restyle only what a script's attribute changes reach, when the
    /// styles are current and every change can be targeted; then lay out
    /// again if that moved something (or an attribute layout reads
    /// changed), else repaint with the new styles. `false` when the whole
    /// document has to be restyled instead.
    fn restyle_changes(&mut self, changes: &[browser_script::DomChange]) -> bool {
        if self.needs_style || self.parser.is_some() || self.layout.is_none() {
            return false;
        }
        let Some(doc) = &self.doc else { return false };
        let Some(delta) = browser_script::feature_changes(doc, changes, None) else { return false };
        let started = std::time::Instant::now();
        let stylist = self.stylist();
        let vp = self.style_viewport();
        let Some(doc) = &self.doc else { return false };
        let r = browser_style::restyle_mutations(
            doc,
            &stylist,
            &vp,
            &self.states,
            &mut self.styles,
            &self.deps,
            &delta.changes,
            &delta.trees,
        );
        tracing::debug!(
            tab = self.id.0,
            elements = delta.changes.len(),
            trees = delta.trees.len(),
            styled = r.styled,
            moved = r.moved,
            repaint = r.repaint,
            ms = started.elapsed().as_millis(),
            "restyle for DOM changes"
        );
        let pseudos = self.sync_pseudos();
        self.perf.restyle += started.elapsed();
        self.perf.partial_restyles += 1;
        self.perf.styled += r.styled;
        // The boxes to build again: the restyled elements, those whose
        // attributes changed (a `src`, a `colspan`, a control's value),
        // and the parents whose children changed.
        let mut elements = r.changed.clone();
        elements.extend(delta.changes.iter().map(|c| c.element));
        let parents: Vec<NodeId> = delta.trees.iter().map(|t| t.parent).collect();
        self.note_box_changes(&elements, &parents);
        if r.moved || delta.layout || pseudos {
            self.want_layout_noted(if r.moved {
                "script change restyle moved a box"
            } else if delta.layout {
                "script changed an attribute layout reads"
            } else {
                "script change made or removed a pseudo-element"
            });
        } else if r.repaint {
            if let (Some(layout), Some(engine), Some(doc)) = (&mut self.layout, &mut self.engine, &self.doc) {
                engine.refresh_styles(layout, doc, &self.styles);
            }
            self.needs_paint = true;
        }
        // A new style may name an image not fetched yet.
        if r.repaint {
            self.fetch_background_images();
        }
        true
    }

    /// Start loading `url`. Nothing about the current page changes until
    /// the response commits (`commit`).
    fn navigate(&mut self, url: Url, kind: NavKind, referrer: NavReferrer) {
        self.navigate_with(url, kind, None, referrer);
    }

    /// `navigate`, POSTing `post` when given (a form submitted as an
    /// entity body): its `Content-Type` and the submitting document's
    /// `Origin` go with it; never from the cache.
    fn navigate_with(&mut self, url: Url, kind: NavKind, post: Option<browser_ipc_types::PostData>, referrer: NavReferrer) {
        tracing::info!(tab = self.id.0, post = post.is_some(), "navigate {url}");
        self.refresh = None;
        self.refresh_header = None;
        self.press = None;
        let mut request = FetchRequest::get(url.clone());
        request.referrer = referrer.request.clone();
        if kind == NavKind::Reload {
            request.cache = CacheMode::NoCache;
        }
        if let Some(post) = post {
            if let Ok(method) = "POST".parse() {
                request.method = method;
            }
            request.headers.push(("content-type".to_owned(), post.content_type));
            request.headers.push(("origin".to_owned(), post.origin));
            request.body = Some(post.body);
            request.cache = CacheMode::NoStore;
        }
        let id = if url.scheme() == "about" {
            // Main fetch determines the referrer before the scheme fetch.
            let referrer = request
                .referrer
                .as_ref()
                .and_then(|r| browser_ipc_types::referrer::determine(&r.source, r.policy, &request.url));
            self.fetch_about(request.url, referrer)
        } else {
            self.fetch(request, PendingKind::Main)
        };
        self.nav = Some(PendingNav {
            url,
            request: id,
            kind,
            committed: false,
            referrer,
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
                status_text: String::new(),
                headers: Vec::new(),
                body: Vec::new(),
                final_url: None,
            },
        );
        self.net.fetch(id, request, self.net_sink.clone());
        id
    }

    /// Internal pages. `about:blank` is an empty document; `about:crash`
    /// panics the tab thread on purpose, to exercise crash recovery.
    /// Answered through the net sink like a `data:` URL, so the load goes
    /// through the same commit path as any other.
    fn fetch_about(&mut self, url: Url, referrer: Option<Url>) -> RequestId {
        let id = RequestId(self.next_request);
        self.next_request += 1;
        self.pending.insert(
            id,
            Pending {
                kind: PendingKind::Main,
                status: 0,
                status_text: String::new(),
                headers: Vec::new(),
                body: Vec::new(),
                final_url: None,
            },
        );
        match url.path() {
            "blank" => {
                (self.net_sink)(NetToTab::ResponseStart {
                    id,
                    status: 200,
                    status_text: "OK".to_owned(),
                    headers: vec![("content-type".to_owned(), "text/html".to_owned())],
                    final_url: url,
                    referrer: referrer.map(Box::new),
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

    /// A `GET` for a sub-resource of the current document, with its
    /// referrer (an element's, a stylesheet's, a module's, the document's).
    fn subresource(&self, url: Url, referrer: Option<RequestReferrer>) -> FetchRequest {
        let mut request = FetchRequest::get(url);
        request.cache = self.doc_cache;
        request.referrer = referrer;
        request
    }

    // ----- network messages -----

    pub fn handle_net(&mut self, msg: NetToTab) {
        // A hop of a request a script made goes to its job.
        let net_id = match &msg {
            NetToTab::ResponseStart { id, .. }
            | NetToTab::ResponseChunk { id, .. }
            | NetToTab::ResponseEnd { id }
            | NetToTab::Failed { id, .. } => *id,
        };
        if let Some(&script_id) = self.net_to_script.get(&net_id) {
            self.script_fetch_event(script_id, msg);
            return;
        }
        match msg {
            NetToTab::ResponseStart {
                id,
                status,
                status_text,
                headers,
                final_url,
                referrer,
            } => {
                let Some(p) = self.pending.get_mut(&id) else { return };
                p.status = status;
                p.status_text = status_text;
                p.headers = headers;
                p.final_url = Some(final_url.clone());
                if matches!(p.kind, PendingKind::Main) {
                    self.refresh_header = p
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("refresh"))
                        .map(|(_, v)| v.clone());
                    self.commit(id, final_url, referrer.map(|r| *r));
                }
            }
            NetToTab::ResponseChunk { id, bytes } => {
                let Some(p) = self.pending.get_mut(&id) else { return };
                let is_current_main = matches!(p.kind, PendingKind::Main)
                    && self.nav.as_ref().is_some_and(|n| n.request == id);
                if is_current_main && let Some(parser) = &mut self.parser {
                    let started = std::time::Instant::now();
                    parser.feed(&bytes);
                    let feed = started.elapsed();
                    self.pump_scripts();
                    let total = started.elapsed();
                    if total.as_millis() > 20 {
                        tracing::debug!(
                            tab = self.id.0,
                            bytes = bytes.len(),
                            feed_ms = feed.as_millis(),
                            total_ms = total.as_millis(),
                            "slow chunk"
                        );
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
                if let PendingKind::Image { url, .. } = &p.kind {
                    let url = url.clone();
                    self.image_fetch_finished(&url, false);
                }
                if matches!(p.kind, PendingKind::Main)
                    && let Some(nav) = self.nav.take_if(|n| n.request == id)
                {
                    // The error page takes the entry the page would have.
                    if !nav.committed {
                        self.apply_history(nav.url.clone(), nav.kind, false);
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

    // ----- requests scripts make (fetch, XMLHttpRequest) -----

    /// Run `f` on the script host as a task: the document lent, then what
    /// the script asked for carried out.
    fn script_task(&mut self, f: impl FnOnce(&mut ScriptHost)) {
        self.sync_document_info();
        self.with_script(f);
        self.after_script();
    }

    /// Carry out what scripts asked of the network.
    fn apply_net_ops(&mut self, ops: Vec<NetOp>) {
        for op in ops {
            match op {
                NetOp::Start(request) => {
                    let Some(document_url) = self.url.clone() else { continue };
                    let (id, timeout, keepalive) = (request.id, request.timeout, request.keepalive);
                    let referrer = self.script_request_referrer(&request);
                    let (job, actions) = {
                        let mut preflights = self.preflights.lock().unwrap_or_else(|e| e.into_inner());
                        Job::start(request, &document_url, referrer, &mut preflights)
                    };
                    self.script_fetches.insert(
                        id,
                        ScriptFetch {
                            job,
                            hop: None,
                            started: std::time::Instant::now(),
                            timeout,
                            keepalive,
                            detached: false,
                        },
                    );
                    self.apply_fetch_actions(id, actions);
                }
                NetOp::Abort(id) => self.finish_script_fetch(id),
                NetOp::SetTimeout(id, timeout) => {
                    if let Some(fetch) = self.script_fetches.get_mut(&id) {
                        fetch.timeout = timeout;
                    }
                }
            }
        }
    }

    fn finish_script_fetch(&mut self, id: u64) {
        if let Some(fetch) = self.script_fetches.remove(&id)
            && let Some(hop) = fetch.hop
        {
            self.net_to_script.remove(&hop);
        }
    }

    /// The network answered the current hop of a script's request.
    fn script_fetch_event(&mut self, id: u64, msg: NetToTab) {
        let Some(fetch) = self.script_fetches.get_mut(&id) else { return };
        let actions = {
            let mut preflights = self.preflights.lock().unwrap_or_else(|e| e.into_inner());
            match msg {
                NetToTab::ResponseStart {
                    status,
                    status_text,
                    headers,
                    referrer,
                    ..
                } => fetch.job.on_start(status, status_text, headers, referrer.map(|r| *r), &mut preflights),
                NetToTab::ResponseChunk { bytes, .. } => fetch.job.on_chunk(bytes),
                NetToTab::ResponseEnd { .. } => fetch.job.on_end(),
                NetToTab::Failed { error, .. } => fetch.job.on_failed(error),
            }
        };
        self.apply_fetch_actions(id, actions);
    }

    /// Do what a job asks: send its next request, or tell the script.
    fn apply_fetch_actions(&mut self, id: u64, actions: Vec<Action>) {
        // A request that outlived its document has nobody to tell.
        let detached = self.script_fetches.get(&id).is_some_and(|f| f.detached);
        for action in actions {
            match action {
                Action::Head(_) | Action::Data(_) if detached => {}
                Action::Done | Action::Fail(_) if detached => self.finish_script_fetch(id),
                Action::Send(request) => {
                    let Some(fetch) = self.script_fetches.get_mut(&id) else { return };
                    let hop = RequestId(self.next_request);
                    self.next_request += 1;
                    if let Some(old) = fetch.hop.replace(hop) {
                        self.net_to_script.remove(&old);
                    }
                    self.net_to_script.insert(hop, id);
                    self.net.fetch(hop, request, self.net_sink.clone());
                }
                Action::Head(head) => self.script_task(|host| host.net_response(id, head)),
                Action::Data(bytes) => self.script_task(|host| host.net_data(id, bytes)),
                Action::Done => {
                    self.finish_script_fetch(id);
                    self.script_task(|host| host.net_done(id));
                }
                Action::Fail(failure) => {
                    self.finish_script_fetch(id);
                    if let NetFailure::Network(why) = &failure {
                        tracing::warn!(tab = self.id.0, "script request failed: {why}");
                        if let Some(host) = &mut self.script {
                            host.report_error(format!("Fetch failed: {why}"));
                        }
                    }
                    self.script_task(|host| host.net_failed(id, failure));
                }
            }
        }
    }

    /// When the earliest `XMLHttpRequest.timeout` runs out.
    fn script_fetch_wake(&self) -> Option<std::time::Instant> {
        self.script_fetches.values().filter_map(|f| f.timeout.map(|t| f.started + t)).min()
    }

    /// Fail the requests whose timeout ran out.
    fn script_fetch_tick(&mut self, now: std::time::Instant) {
        let expired: Vec<u64> = self
            .script_fetches
            .iter()
            .filter(|(_, f)| f.timeout.is_some_and(|t| now >= f.started + t))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.finish_script_fetch(id);
            self.script_task(|host| host.net_failed(id, NetFailure::Timeout));
        }
    }

    /// Another tab changed the local storage of an origin: the document
    /// fires `storage` when the origin is its own.
    pub fn handle_storage(&mut self, change: StorageChange) {
        if self.origin_storage.as_ref().is_none_or(|s| s.origin != change.origin) || self.script.is_none() {
            return;
        }
        self.script_task(|host| {
            host.fire_storage_event(
                StorageArea::Local,
                change.key.as_deref(),
                change.old.as_deref(),
                change.new.as_deref(),
                &change.url,
            );
        });
    }

    /// The "update intersection observations" step of updating the
    /// rendering: after layout and the scroll events, before painting.
    /// Returns whether a callback ran (it may have changed the tree).
    fn update_intersection_observers(&mut self) -> bool {
        let due = match &mut self.script {
            Some(host) => host.has_intersection_observers() && (self.needs_paint || host.intersection_observers_dirty()),
            None => false,
        };
        if !due || self.layout.is_none() || self.parser.is_some() {
            return false;
        }
        let time_ms = self.doc_started.elapsed().as_secs_f64() * 1000.0;
        let mut ran = false;
        self.sync_document_info();
        self.with_script(|host| ran = host.update_intersection_observers(time_ms));
        self.after_script();
        ran
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
            let fetched = self.nav.is_none()
                && self.pending.values().all(|p| matches!(p.kind, PendingKind::Image { delays_load: false, .. }));
            // `document.fonts.ready` waits for the document to be parsed:
            // a step of the lifecycle is news for it.
            if parsed && self.lifecycle != Lifecycle::Complete {
                self.web_fonts.dirty = true;
            }
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
        // The document's policy container takes the response's
        // `Referrer-Policy` (HTML's "create a policy container from a
        // fetch response"); a `<meta name=referrer>` may replace it.
        self.doc_header_policy = ReferrerPolicy::from_headers(p.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if html_like {
            let parser = HtmlParser::with_charset(Some(final_url), charset.as_deref());
            parser.document_mut().referrer_policy = self.doc_header_policy;
            self.parser = Some(parser);
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
                    parser.document_mut().referrer_policy = self.doc_header_policy;
                    parser.feed(html.as_bytes());
                    parser.finish()
                };
                self.set_document(doc, false);
            }
            PendingKind::Stylesheet { url, depth } => {
                if p.status == 0 || (200..300).contains(&p.status) {
                    let (_, charset) = Self::content_type_of(&p.headers);
                    let css = browser_dom::encoding::decode_stylesheet(&p.body, charset.as_deref());
                    tracing::debug!(tab = self.id.0, bytes = css.len(), url = url.as_str(), "external stylesheet loaded");
                    // What the sheet fetches has the sheet's own URL as
                    // referrer, under its response's policy (WPT
                    // `referrer-policy/css-integration`).
                    let referrer = RequestReferrer {
                        source: p.final_url.clone().unwrap_or_else(|| url.clone()),
                        policy: ReferrerPolicy::from_headers(p.headers.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
                    };
                    let sheet = Arc::new(Stylesheet::parse_external(&css, Origin::Author, &url, referrer));
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
                    self.want_style("stylesheet arrived");
                }
            }
            PendingKind::Image { url, .. } => {
                // The body is the image whatever the status (HTML: a 404
                // carrying image data shows it).
                let ok = if let Some(img) = decode_image(&p.body) {
                    self.image_arrived(&url);
                    self.images.insert(url.clone(), img);
                    self.image_sizes = ImageSizeMap(Arc::new(self.images.sizes()));
                    true
                } else if let Some(svg) = browser_paint::decode_svg(&p.body) {
                    // An SVG image: kept as vector drawing (line 6b).
                    self.image_arrived(&url);
                    self.images.insert_svg(url.clone(), svg);
                    self.image_sizes = ImageSizeMap(Arc::new(self.images.sizes()));
                    true
                } else {
                    false
                };
                self.image_fetch_finished(&url, ok);
            }
            PendingKind::Font { url } => {
                // A CORS-mode load (CSS Fonts 4 §4.8.2): a font from another
                // origin needs the server's leave.
                let ok = (p.status == 0 || (200..300).contains(&p.status)) && self.cors_allows(&url, p.final_url.as_ref(), &p.headers);
                let mut registered = false;
                if ok {
                    let faces = self.document_faces();
                    // The faces loading from this URL: it is their current
                    // source (the first usable one that has not failed).
                    // Script faces out of the set are among them when a
                    // script loads them.
                    let source = Source::Url(url.clone());
                    let now = self.font_clock.now();
                    let font_data = LayoutEngine::is_font_data(&p.body);
                    let body: Arc<[u8]> = Arc::from(p.body.as_slice());
                    let outside: Vec<Arc<browser_style::fontface::FontFaceRule>> = self
                        .web_fonts
                        .script
                        .iter()
                        .filter(|s| !s.in_set && self.web_fonts.forced_script.contains(&s.id))
                        .map(|s| s.rule.clone())
                        .collect();
                    for (order, face) in faces.iter().map(Some).enumerate().chain(outside.iter().map(|f| (usize::MAX, Some(f)))) {
                        let Some(face) = face else { continue };
                        let key = face_key(face);
                        if self.web_fonts.loaded.contains(&key) || self.web_fonts.source_of(face).as_ref() != Some(&source) {
                            continue;
                        }
                        if !font_data {
                            continue;
                        }
                        if let Some(id) = self.script_face_id(face) {
                            self.web_fonts.script_data.insert(id, body.clone());
                            registered = true;
                        }
                        // Arrived after its swap period: loaded, not used.
                        if self.web_fonts.expired(key, face.display, now) {
                            self.web_fonts.timed_out.insert(key);
                            self.web_fonts.late.insert(key);
                            registered = true;
                            continue;
                        }
                        if order == usize::MAX {
                            continue;
                        }
                        if self.engine.as_mut().is_some_and(|e| e.register_web_font(&p.body, face, order as u32)) {
                            self.web_fonts.loaded.insert(key);
                            registered = true;
                        }
                    }
                    self.web_fonts.dirty = true;
                }
                if registered {
                    // The engine noted the families it registered: their
                    // text is laid out again.
                    self.want_layout_noted("web font arrived");
                } else {
                    tracing::debug!(tab = self.id.0, url = url.as_str(), "web font failed");
                    self.web_fonts.failed.insert(Source::Url(url));
                    // The face's next source, or a family later in a list.
                    self.fetch_web_fonts();
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
                                match host.parse_module(&text, &url, true, load.policy) {
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
        self.set_document(doc, false);
    }

    /// The images of the document before go ("Working sites first" 6m):
    /// the decoded ones, the fetched and failed URLs, the deferred fetches.
    fn reset_images(&mut self) {
        self.images = ImageStore::new();
        self.image_sizes = ImageSizeMap::default();
        self.fetched_images.clear();
        self.failed_images.clear();
        self.deferred_images.clear();
        self.wanted_images.clear();
    }

    /// A finished document: collect its stylesheets and images, then render.
    /// `from_parser`: the document the parser built, whose images loaded
    /// while it parsed (kept); else a document made here, replacing what
    /// was before.
    fn set_document(&mut self, doc: Document, from_parser: bool) {
        self.sheets.clear();
        self.fetched_urls.clear();
        self.loaded_sheets.clear();
        if from_parser {
            let wanted = &self.wanted_images;
            self.images.retain(|u| wanted.contains(u));
            self.image_sizes = ImageSizeMap(Arc::new(self.images.sizes()));
        } else {
            self.reset_images();
        }
        // The old document's web fonts are its own.
        self.web_fonts = WebFonts::default();
        if let Some(engine) = &mut self.engine {
            engine.clear_web_fonts();
            // A new document: none of the kept boxes are its.
            engine.note_unknown_box_change();
        }
        self.doc = Some(doc);
        self.doc_url = self.url.clone();
        // Node ids of the old document mean nothing now. The pointer
        // position is kept: hover is re-evaluated after the first layout.
        self.states = ElementStates::default();
        self.hover = None;
        self.focus = None;
        self.cursor_image_sent = None;
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
        self.drive_images();
        self.pending_fragment = self.url.as_ref().is_some_and(|u| u.fragment().is_some());
        self.schedule_refresh();
        // A parsed document already has its context (made at commit);
        // documents made up here (error pages, wrapped non-HTML) get one.
        if self.script.is_none() {
            self.new_script_host();
        }
        // Parsed: `interactive`, before any deferred script runs.
        self.update_readiness();
        self.want_style("parsing finished");
        self.state_dirty = true;
    }

    // ----- script -----

    /// Frame interval for animation frame callbacks.
    const FRAME: std::time::Duration = std::time::Duration::from_micros(16_667);

    /// A new document gets a new context; the old one's timers, frames
    /// and waiting scripts die with it. `document.cookie` reads and
    /// writes the browser's jar directly (`TabCookies`), so a cookie a
    /// script sets goes on the next request and a cookie a response set
    /// is readable at once.
    fn new_script_host(&mut self) {
        self.frame_due = None;
        self.doc_started = std::time::Instant::now();
        self.scripts = Scripts::default();
        self.lifecycle = Lifecycle::Loading;
        // The old document's requests are no longer anyone's; the ones it
        // asked to be kept alive (a beacon sent while leaving) go on to
        // their end under keys no new request takes.
        let kept: Vec<ScriptFetch> = self.script_fetches.drain().map(|(_, f)| f).filter(|f| f.keepalive).collect();
        self.net_to_script.clear();
        for mut fetch in kept {
            fetch.detached = true;
            fetch.timeout = None;
            let key = self.next_detached;
            self.next_detached -= 1;
            if let Some(hop) = fetch.hop {
                self.net_to_script.insert(hop, key);
            }
            self.script_fetches.insert(key, fetch);
        }
        // Storage is by origin; an opaque one (`data:`, `about:`) has none.
        self.origin_storage = match &self.url {
            Some(url) if matches!(url.scheme(), "http" | "https") => Some(OriginStorage::new(
                self.id,
                url.origin().ascii_serialization(),
                url.as_str(),
                self.stores.clone(),
            )),
            _ => None,
        };
        self.script = match ScriptHost::new(self.url.clone(), browser_net::USER_AGENT) {
            Ok(mut host) => {
                host.set_cookie_access(std::rc::Rc::new(TabCookies(self.net.clone())));
                if let Some(storage) = &self.origin_storage {
                    host.set_storage_access(storage.clone());
                }
                if let Some(url) = &self.url {
                    host.set_sync_net(std::rc::Rc::new(BlockingNet {
                        net: self.net.clone(),
                        document_url: url.clone(),
                        preflights: self.preflights.clone(),
                        referrer: self.sync_referrer.clone(),
                    }));
                }
                if let Some(entry) = self.history.get(self.history_index) {
                    host.set_history(self.history_index, self.history.len(), entry.state.clone(), entry.manual_scroll);
                }
                Some(host)
            }
            Err(e) => {
                tracing::error!(tab = self.id.0, "script context: {e}");
                None
            }
        };
    }

    /// Run `f` on the script host with the document lent to it, so the
    /// DOM bindings can read and write it, and take the document back
    /// after. (See `TabCookies` for what `document.cookie` reaches.) The document is moved, not borrowed: it comes out of the
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
        // The view: what a geometry read needs. The styles and layout
        // are current unless a restyle or layout is pending (or the
        // document on screen is not the one lent, while parsing).
        let mut view = std::mem::take(&mut self.view);
        view.viewport = (self.viewport.width, self.viewport.height);
        view.scale = self.viewport.scale_factor;
        view.scroll = (self.scroll_x, self.scroll_y);
        view.element_scrolls = std::mem::take(&mut self.scrolls);
        view.window = self.window_size;
        view.screen = self.screen_size;
        view.engine = self.engine.take();
        view.styles = std::mem::take(&mut self.styles);
        view.layout = self.layout.take();
        // The tab laid out or scrolled since the view last indexed the
        // tree's boxes: the index is built again when a script asks.
        if std::mem::take(&mut self.view_rects_stale) {
            view.forget_rects();
        }
        view.deps = std::mem::take(&mut self.deps);
        view.loaded_sheets = std::mem::take(&mut self.loaded_sheets);
        view.images = self.image_sizes.clone();
        let fresh = !parsing && !self.needs_style && !self.needs_layout && view.layout.is_some();
        view.clean_generation = if fresh { Some(host.generation()) } else { None };
        view.recomputed = false;
        view.scroll_changed = false;
        view.elements_scrolled = false;
        self.host_layout_fresh = false;
        host.lend_view(view);
        let started = std::time::Instant::now();
        // What the parser (or the tab) changed since the last task: the
        // mutation observers hear of it before this one runs.
        host.deliver_mutations();
        f(host);
        self.perf.script += started.elapsed();
        self.perf.scripts += 1;
        let doc = host.reclaim_document();
        match (&mut self.parser, &mut self.doc) {
            (Some(parser), _) => *parser.document_mut() = doc,
            (None, Some(slot)) => *slot = doc,
            (None, None) => {}
        }
        let Some(mut view) = host.reclaim_view() else { return };
        self.engine = view.engine.take();
        if std::mem::take(&mut self.box_changes_unknown)
            && let Some(engine) = &mut self.engine
        {
            engine.note_unknown_box_change();
        }
        self.styles = std::mem::take(&mut view.styles);
        self.layout = view.layout.take();
        self.scrolls = std::mem::take(&mut view.element_scrolls);
        self.deps = std::mem::take(&mut view.deps);
        self.loaded_sheets = std::mem::take(&mut view.loaded_sheets);
        // A layout the script forced that nothing changed after is the
        // layout the tab would compute next: keep it.
        let generation = host.generation();
        if view.recomputed && view.clean_generation == Some(generation) && !parsing {
            self.needs_style = false;
            self.needs_layout = false;
            self.needs_paint = true;
            self.host_layout_fresh = true;
            self.fetch_background_images();
            self.fetch_web_fonts();
            self.after_layout();
        }
        if view.scroll_changed {
            self.set_scroll(ScrollTarget::Page, view.scroll, false);
        }
        if view.elements_scrolled {
            // The script moved scrollers in the tree itself; the `scroll`
            // events follow at the next flush (`fire_view_events` finds
            // the offsets that differ from the ones last reported).
            self.needs_paint = true;
            self.hover_dirty = true;
        }
        // The script's scrolls in their order: an instant one (already
        // applied) drops any animation of its target, a smooth one starts
        // animating from where the target is now.
        for (node, to, smooth) in view.scroll_requests.drain(..) {
            let target = node.map_or(ScrollTarget::Page, ScrollTarget::Element);
            if smooth {
                self.animate_scroll(target, to, false, AnimationKind::Programmatic);
            } else {
                self.cancel_scroll_animation(target);
            }
        }
        self.view = view;
    }

    /// Tell the bindings where the document stands before a script runs:
    /// its URL, its title so far, and its readiness (`loading` while it
    /// parses, `interactive` while deferred scripts remain, `complete`).
    fn sync_document_info(&mut self) {
        // A synchronous `XMLHttpRequest` sends the document's referrer as
        // it is now.
        let referrer = self.client_referrer(ReferrerPolicy::Empty);
        *self.sync_referrer.borrow_mut() = referrer;
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
        // A form's planned navigation is a task queued after the script:
        // it runs after the script's own navigations, the last one
        // winning.
        let (submissions, requests): (Vec<HostRequest>, Vec<HostRequest>) =
            requests.into_iter().partition(|r| matches!(r, HostRequest::Submit { .. }));
        self.apply_script_requests(requests);
        for s in submissions {
            if let HostRequest::Submit { url, post, target, noreferrer, .. } = s {
                self.navigate_submission(url, post, &target, noreferrer);
            }
        }
    }

    fn apply_script_requests(&mut self, requests: Vec<HostRequest>) {
        for request in requests {
            match request {
                HostRequest::Submit { .. } => {}
                // The script changed the document itself; the shell hears
                // of the new title.
                HostRequest::SetTitle(_) => self.state_dirty = true,
                HostRequest::PushState { url, state, replace } => self.push_state(url, state, replace),
                HostRequest::FragmentNavigate { url, replace } => self.script_fragment_navigated(url, replace),
                HostRequest::Traverse { delta } => {
                    let target = self.history_index as i64 + i64::from(delta);
                    if target >= 0 && (target as usize) < self.history.len() {
                        self.traverse_to(target as usize);
                        // A traversal to another document: later requests
                        // belong to one that is going away.
                        if self.nav.is_some() {
                            break;
                        }
                    }
                }
                HostRequest::ScrollRestoration { manual } => {
                    if let Some(entry) = self.history.get_mut(self.history_index) {
                        entry.manual_scroll = manual;
                    }
                }
                HostRequest::Navigate { url, replace } => {
                    if !matches!(url.scheme(), "http" | "https" | "data" | "about") {
                        continue;
                    }
                    // `location`'s navigations: the document's referrer.
                    let referrer = self.page_referrer(ReferrerPolicy::Empty);
                    self.go(url, if replace { NavKind::Replace } else { NavKind::Push }, referrer);
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
        self.deliver_parser_mutations();
        self.run_ready_scripts();
        self.update_readiness();
    }

    /// The parser added to a document a `MutationObserver` watches: its
    /// callbacks run at the end of the parser's turn, script following or
    /// not.
    fn deliver_parser_mutations(&mut self) {
        let pending = match (&self.parser, &self.doc) {
            (Some(parser), _) => parser.document().has_logged_mutations(),
            (None, Some(doc)) => doc.has_logged_mutations(),
            (None, None) => false,
        };
        if pending && self.script.is_some() {
            self.script_task(|_| {});
        }
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
                Prepared::Run(source, base, policy) => {
                    self.sync_document_info();
                    self.with_script(|host| match &base {
                        Some(base) => host.run_inline_script(&source, base, policy),
                        None => host.run_script(&source),
                    });
                    self.after_script();
                }
                Prepared::Blocking => return,
                Prepared::Done => {}
            }
            if let Some(parser) = &mut self.parser {
                parser.resume();
            }
        }
        // The `<img>` elements parsed so far update their image data
        // (HTML: on creation), so their fetches start while parsing goes on.
        self.drive_images();
        if self.main_done
            && self.parser.as_ref().is_some_and(HtmlParser::is_done)
            && let Some(parser) = self.parser.take()
        {
            let started = std::time::Instant::now();
            let doc = parser.finish();
            let finish_ms = started.elapsed().as_millis();
            self.set_document(doc, true);
            tracing::debug!(tab = self.id.0, finish_ms, set_ms = started.elapsed().as_millis() - finish_ms, "document parsed");
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
        let (kind, src, is_async, defer, text, doc_url, policy) = {
            let doc = parser.document();
            let Some(e) = doc.element(node) else { return Prepared::Done };
            let Some(kind) = script_kind(e) else { return Prepared::Done };
            let policy = ReferrerPolicy::from_attribute(e.attr("referrerpolicy"));
            let src = e.attr("src").map(|s| (s.to_owned(), doc.resolve_url(s)));
            let text = if src.is_none() { doc.text_content(node) } else { String::new() };
            (
                kind,
                src,
                e.attr("async").is_some(),
                e.attr("defer").is_some(),
                text,
                doc.base_url.clone(),
                policy,
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
                    policy,
                };
                let blocking = !is_async && !defer && kind == ScriptKind::Classic;
                if is_async {
                    self.scripts.asap.push(load);
                } else if blocking {
                    self.scripts.blocking = Some(load);
                } else {
                    self.scripts.deferred.push_back(load);
                }
                // The element's `referrerpolicy` (HTML's script fetch options).
                let mut request = self.subresource(url, self.element_referrer(Some(node)));
                request.headers.push(("accept".to_owned(), "*/*".to_owned()));
                self.fetch(request, PendingKind::Script { id });
                if blocking { Prepared::Blocking } else { Prepared::Done }
            }
            None => match kind {
                ScriptKind::Classic => {
                    if text.trim().is_empty() {
                        Prepared::Done
                    } else {
                        Prepared::Run(text, doc_url, policy)
                    }
                }
                ScriptKind::Module => {
                    let Some(host) = &mut self.script else { return Prepared::Done };
                    let url = doc_url
                        .or_else(|| self.url.clone())
                        .unwrap_or_else(|| Url::parse("about:blank").expect("static url"));
                    match host.parse_inline_module(&text, &url, policy) {
                        Ok(m) => {
                            let load = ScriptLoad {
                                id: self.scripts.next_id(),
                                kind,
                                url: None,
                                state: LoadState::Module(m),
                                policy,
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
        for m in to_fetch {
            settled |= self.fetch_module_source(m);
        }
        settled
    }

    /// Fetch a module for the module map, once per URL. Returns true when
    /// the answer was given on the spot (an unsupported scheme).
    fn fetch_module_source(&mut self, module: browser_script::ModuleFetch) -> bool {
        let browser_script::ModuleFetch { url, referrer, policy } = module;
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
        // An import's `Referer` is its importer's URL, under the policy
        // its fetch options inherited; the document's for an inline script.
        let referrer = match referrer {
            Some(source) => Some(RequestReferrer {
                source,
                policy: policy.or(self.document_policy()),
            }),
            None => self.client_referrer(policy),
        };
        let mut request = self.subresource(url.clone(), referrer);
        request.headers.push(("accept".to_owned(), "*/*".to_owned()));
        self.fetch(request, PendingKind::ModuleSource { url });
        false
    }

    /// Run a ready script as a task, or report why it cannot run.
    fn execute_script(&mut self, load: ScriptLoad) {
        self.sync_document_info();
        self.with_script(|host| match load.state {
            LoadState::Classic(source) => host.run_script_from(&source, load.url.as_ref(), load.policy),
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
        let changes = host.take_dom_changes();
        let net_ops = host.take_net_ops();
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
        for m in requests {
            self.fetch_module_source(m);
        }
        if mutated {
            self.dom_changed(&changes);
        }
        self.apply_host_requests(host_requests);
        // The requests the script made (`fetch`, `XMLHttpRequest`) start.
        self.apply_net_ops(net_ops);
        self.apply_font_requests();
        // The images the script created or changed: their fetches and
        // tasks (the script ran the algorithms; what it left undone runs).
        self.drive_images();
    }

    /// A script changed the tree: restyle what the changes reach (the
    /// whole document for changes that cannot be targeted), lay out and
    /// paint again as needed, tell the shell (the title may have changed),
    /// and drop every node reference the change may have made stale: a
    /// freed node, or one no longer in the document. Hover is found again
    /// after layout.
    fn dom_changed(&mut self, changes: &[browser_script::DomChange]) {
        // A layout the script forced after its last change is current.
        let fresh = std::mem::take(&mut self.host_layout_fresh);
        self.state_dirty = true;
        // A script changed the focused text control's value: the caret
        // goes to its end and the selection is dropped (HTML's `value`
        // setter).
        if let Some(f) = self.text_focus()
            && changes.iter().any(|c| matches!(c, browser_script::DomChange::Control { element, .. } if *element == f))
        {
            self.caret = self.control_value(f).len();
            self.sel_anchor = None;
            self.last_select = None;
            self.needs_paint = true;
        }
        // A `<style>` or `<link>` the script added, changed or removed.
        self.sync_stylesheets();
        if !fresh && !self.restyle_changes(changes) {
            // A full restyle: the boxes the changes reach (the full
            // restyle adds those whose style changed), unless the tree
            // changed in a way the log does not describe.
            match browser_script::dom_box_changes(changes, None) {
                Some((elements, parents)) => self.note_box_changes(&elements, &parents),
                None => self.note_unknown_box_change(),
            }
            self.want_style("script change needing a full restyle");
            self.want_layout_noted("script change needing a full restyle");
        }
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
            self.sel_anchor = None;
            self.composition = None;
            self.control_drag = false;
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
            /// The text, and the base URL it was set under.
            Inline(String, Option<Url>),
            Link(Url),
        }
        let Some(doc) = &self.doc else { return };
        let mut found: Vec<(NodeId, MediaQueryList, Source)> = Vec::new();
        for id in doc.descendants(doc.root()) {
            let Some(e) = doc.element(id) else { continue };
            if e.name.ns != ns!(html) {
                continue;
            }
            let media = parse_media_attr(e.attr("media"));
            if e.name.local == local_name!("style") {
                found.push((id, media, Source::Inline(doc.text_content(id), doc.sheet_base(id).cloned())));
            } else if e.name.local == local_name!("link") {
                let rel = e.attr("rel").unwrap_or("");
                let is_sheet = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet"));
                let alternate = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("alternate"));
                if is_sheet && !alternate
                    && let Some(href) = e.attr("href")
                    && let Some(url) = doc.link_url(id, href)
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
                        Source::Inline(css, base) => main.url == *base && main.text_hash == text_hash(css),
                        Source::Link(url) => main.url.as_ref() == Some(url),
                    }
            });
            if unchanged {
                self.sheets.append(&mut same);
                continue;
            }
            changed = true;
            match source {
                Source::Inline(css, base) => {
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
                    // The `<link>`'s `referrerpolicy`.
                    let referrer = self.element_referrer(Some(node));
                    self.load_external_sheet(slot, url, 0, referrer);
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
            self.want_style("document's stylesheets changed");
        }
    }

    /// Fill `slot` with the sheet at `url`: from the loaded sheets if it
    /// came before, else by fetching it (once; a second slot with the
    /// same URL is filled when the response comes).
    fn load_external_sheet(&mut self, slot: usize, url: Url, depth: u8, referrer: Option<RequestReferrer>) {
        if let Some(sheet) = self.loaded_sheets.get(&url).cloned() {
            let slot = slot + self.queue_imports(slot, &sheet, depth);
            self.sheets[slot].sheet = Some(sheet);
            return;
        }
        if self.fetched_urls.insert(url.clone()) {
            let request = self.subresource(url.clone(), referrer);
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
            // An `@import`'s `Referer` is its importing sheet's (the
            // document's for an inline one).
            let referrer = self.css_referrer(sheet.referrer.as_deref());
            self.load_external_sheet(insert_at, url, depth + 1, referrer);
            // A loaded import may have inserted its own imports before it.
            insert_at += 1 + (self.sheets.len() - before);
        }
        insert_at - slot
    }

    // ----- images ("Working sites first" 6m) -----

    /// Run `f` on the document the image algorithms work on (the parser's
    /// while it parses) with their environment: the viewport and device
    /// pixel ratio, the decoded images (the list of available images),
    /// the laid-out images' concrete widths.
    fn with_image_doc<R>(&mut self, f: impl FnOnce(&mut Document, &browser_style::responsive::ImageEnv<'_>) -> R) -> Option<R> {
        let viewport = self.style_viewport();
        let images = &self.images;
        // While parsing, the layout on hand is the old document's.
        let layout = self.layout.as_ref().filter(|_| self.parser.is_none());
        let available = |u: &Url| images.contains(u);
        // One walk of the tree for every image the run asks about.
        let auto: Vec<NodeId> = match (&self.doc, layout) {
            (Some(doc), Some(_)) => {
                doc.images_with_data().into_iter().filter(|&i| browser_style::responsive::allows_auto_sizes(doc, i)).collect()
            }
            _ => Vec::new(),
        };
        let widths: std::cell::RefCell<Option<HashMap<NodeId, f32>>> = std::cell::RefCell::new(None);
        let rendered = |n: NodeId| {
            let layout = layout?;
            let mut cache = widths.borrow_mut();
            let map = cache.get_or_insert_with(|| layout.content_widths(&auto));
            if !map.contains_key(&n) && !auto.contains(&n) {
                return layout.content_widths(&[n]).get(&n).copied();
            }
            map.get(&n).copied()
        };
        let env = browser_style::responsive::ImageEnv {
            viewport: &viewport,
            available: &available,
            rendered_width: &rendered,
        };
        match (&mut self.parser, &mut self.doc) {
            (Some(parser), _) => Some(f(&mut parser.document_mut(), &env)),
            (None, Some(doc)) => Some(f(doc, &env)),
            (None, None) => None,
        }
    }

    /// The image algorithms for what happened to the document's images:
    /// every `<img>` created or mutated in a way relevant to it runs
    /// "update the image data" (both halves at once: no script runs here,
    /// so the microtask would follow at once), then their fetches, box
    /// changes and tasks are carried out.
    fn drive_images(&mut self) {
        self.with_image_doc(|doc, env| {
            for (img, generation) in browser_style::responsive::run_image_mutations(doc, env) {
                browser_style::responsive::update_image_data_rest(doc, img, generation, env);
            }
        });
        self.image_outputs();
    }

    /// Carry out what the image algorithms asked for, until they ask for
    /// nothing more: fetch the URLs (an available or failed one ends at
    /// once), build again the boxes (and styles) of the `<img>` elements
    /// whose image changed, and run the queued tasks (`currentSrc`
    /// updates, `load`/`error` events, each a task of its own).
    fn image_outputs(&mut self) {
        loop {
            let Some((fetches, changed, tasks)) = self.with_image_doc(|doc, _| {
                (
                    std::mem::take(&mut doc.image_fetches),
                    std::mem::take(&mut doc.images_changed),
                    std::mem::take(&mut doc.image_tasks),
                )
            }) else {
                return;
            };
            if fetches.is_empty() && changed.is_empty() && tasks.is_empty() {
                return;
            }
            for (url, deferred, policy) in fetches {
                self.request_image(url, deferred, policy);
            }
            self.images_changed(changed);
            for task in tasks {
                self.run_image_task(task);
            }
        }
    }

    /// A request wants `url`: done at once when it is decoded already or
    /// failed before, else fetched (once per URL; requests share it). A
    /// deferred one waits until its `<img>` is rendered (`after_layout_images`).
    /// `policy`: the `<img>`'s `referrerpolicy` when the request was
    /// made.
    fn request_image(&mut self, url: Url, deferred: bool, policy: ReferrerPolicy) {
        self.wanted_images.insert(url.clone());
        if self.images.contains(&url) {
            self.with_image_doc(|doc, _| browser_style::responsive::image_fetch_done(doc, &url, true));
            return;
        }
        if self.failed_images.contains(&url) || !matches!(url.scheme(), "http" | "https" | "data") {
            self.with_image_doc(|doc, _| browser_style::responsive::image_fetch_done(doc, &url, false));
            return;
        }
        if deferred {
            let imgs: Vec<NodeId> = self
                .with_image_doc(|doc, _| {
                    doc.images_with_data()
                        .into_iter()
                        .filter(|&i| {
                            doc.image_data(i).is_some_and(|d| {
                                d.current.url.as_ref() == Some(&url) || d.pending.as_ref().is_some_and(|(p, _)| p.url.as_ref() == Some(&url))
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            for img in imgs {
                if !self.deferred_images.iter().any(|(i, u, _)| *i == img && *u == url) {
                    self.deferred_images.push((img, url.clone(), policy));
                }
            }
            return;
        }
        let referrer = self.client_referrer(policy);
        self.fetch_image(url, true, referrer);
    }

    /// Fetch an image once per URL (a later request for it shares the
    /// first, whose `referrer` went).
    fn fetch_image(&mut self, url: Url, delays_load: bool, referrer: Option<RequestReferrer>) {
        self.wanted_images.insert(url.clone());
        if self.fetched_images.contains(&url) || self.images.contains(&url) {
            return;
        }
        if !matches!(url.scheme(), "http" | "https" | "data") {
            return;
        }
        self.fetched_images.insert(url.clone());
        let request = self.subresource(url.clone(), referrer);
        self.fetch(request, PendingKind::Image { url, delays_load });
    }

    /// An image fetch ended: the requests waiting on it complete.
    fn image_fetch_finished(&mut self, url: &Url, ok: bool) {
        if !ok {
            self.failed_images.insert(url.clone());
        }
        self.with_image_doc(|doc, _| browser_style::responsive::image_fetch_done(doc, url, ok));
        self.image_outputs();
    }

    /// The `<img>` elements whose current image or dimension attribute
    /// source changed: restyled (their `width`/`height` hints) and laid
    /// out again, through the same path as a script's changes.
    fn images_changed(&mut self, changed: Vec<NodeId>) {
        if changed.is_empty() {
            return;
        }
        let changes: Vec<browser_script::DomChange> =
            changed.iter().map(|&element| browser_script::DomChange::Image { element, generation: 0 }).collect();
        if !self.restyle_changes(&changes) {
            // A restyle or the first layout is still to come: it builds
            // these boxes; the elements are noted for it.
            self.note_box_changes(&changed, &[]);
            if self.layout.is_some() {
                self.want_style("image changed before styles were current");
                self.want_layout_noted("image changed before styles were current");
            }
        }
        self.needs_paint = true;
    }

    /// A task an image algorithm queued: the current request's URL set
    /// (what `currentSrc` reads), then its event fired at the `<img>`
    /// (`load` and `error` do not bubble).
    fn run_image_task(&mut self, task: browser_dom::image::ImageTask) {
        let element = task.element;
        let alive = self
            .with_image_doc(|doc, _| {
                if !doc.contains(element) {
                    return false;
                }
                if let Some(url) = task.set_current_url {
                    doc.image_data_mut(element).current.current_url = url;
                }
                true
            })
            .unwrap_or(false);
        if alive && let Some(event) = task.event {
            self.fire_lifecycle_event(EventTargetRef::Node(element), event, false);
        }
    }

    /// After a layout: an `<img>` that allows auto-sizes and has started
    /// being rendered or changed its concrete width selects again (with
    /// "maybe omit events", HTML §4.8.4.3.2), then the fetches that waited
    /// for their `<img>` to be rendered go out (not delaying the load
    /// event).
    fn after_layout_images(&mut self) {
        let Some(layout) = &self.layout else { return };
        let candidates: Vec<NodeId> = self
            .doc
            .as_ref()
            .map(|doc| doc.images_with_data().into_iter().filter(|&i| browser_style::responsive::allows_auto_sizes(doc, i)).collect())
            .unwrap_or_default();
        let deferred: Vec<NodeId> = self.deferred_images.iter().map(|d| d.0).collect();
        let mut wanted = candidates.clone();
        wanted.extend(deferred);
        if wanted.is_empty() {
            return;
        }
        let widths = layout.content_widths(&wanted);
        let mut ran = false;
        self.with_image_doc(|doc, env| {
            for img in candidates {
                let Some(&w) = widths.get(&img) else {
                    // Not rendered: the next time it is counts as new.
                    doc.image_data_mut(img).rendered_width_seen = None;
                    continue;
                };
                if doc.image_data(img).is_some_and(|d| d.rendered_width_seen == Some(w)) {
                    continue;
                }
                doc.image_data_mut(img).rendered_width_seen = Some(w);
                if let Some(generation) = browser_style::responsive::update_image_data(doc, img, env, true) {
                    browser_style::responsive::update_image_data_rest(doc, img, generation, env);
                }
                ran = true;
            }
        });
        if ran {
            self.image_outputs();
        }
        // The deferred fetches whose `<img>` is rendered and still wants
        // that URL.
        let pending = std::mem::take(&mut self.deferred_images);
        for (img, url, policy) in pending {
            let state = self.with_image_doc(|doc, _| {
                doc.image_data(img).map(|d| {
                    d.current.url.as_ref() == Some(&url) && d.current.state == browser_dom::image::RequestState::Unavailable
                        || d.pending.as_ref().is_some_and(|(p, _)| p.url.as_ref() == Some(&url))
                })
            });
            match state.flatten() {
                Some(true) if widths.contains_key(&img) => {
                    let referrer = self.client_referrer(policy);
                    self.fetch_image(url, false, referrer);
                }
                Some(true) => self.deferred_images.push((img, url, policy)),
                _ => {}
            }
        }
    }

    /// The viewport or device pixel ratio changed: every `<img>` that uses
    /// `srcset` or `<picture>` reacts (HTML §4.8.4.3.13).
    fn images_react_to_environment(&mut self) {
        self.with_image_doc(|doc, env| {
            for img in doc.images_with_data() {
                browser_style::responsive::react_to_environment_changes(doc, img, env);
            }
        });
        self.image_outputs();
    }

    /// An image finished loading: layout only when a box takes its size
    /// from it (an `<img>`, a list marker's image: an image fragment of
    /// that URL), else a repaint (backgrounds, masks, border images do not
    /// size boxes). Before the first layout there is nothing to redo.
    fn image_arrived(&mut self, url: &Url) {
        let Some(tree) = &self.layout else {
            self.want_layout("image arrived before the first layout");
            return;
        };
        // The elements whose boxes take their size from the image (block
        // B3: only their boxes are built again): those showing it (an
        // `<img>`, a `content` image's owner), and below those whose list
        // marker or generated content names it.
        let mut elements: Vec<NodeId> = Vec::new();
        let href = url.as_str();
        if let Some(doc) = &self.doc {
            tree.root.walk(&mut |f| {
                if let browser_layout::FragmentContent::Image(browser_layout::ImageSource::Url(u)) = &f.content
                    && u.as_str() == href
                    && let Some(n) = f.node.filter(|&n| doc.contains(n))
                {
                    // A pseudo-element's box: its element's.
                    elements.push(if doc.get(n).pseudo_kind().is_some() { doc.parent(n).unwrap_or(n) } else { n });
                }
            });
        }
        // A marker image, and a `content` image of a `::before`/`::after`
        // or marker, get their boxes only once decoded.
        let scale = self.viewport.scale_factor;
        let in_content = |s: &browser_style::ComputedStyle| {
            s.content.items().is_some_and(|items| {
                items.iter().any(|i| match i {
                    browser_style::GeneratedItem::Image(img) => {
                        img.url_to_fetch(scale).is_some_and(|u| &**u == href)
                            || matches!(img, browser_style::ComputedImage::ImageSet(c) if c.iter().any(|c| c.image.url_to_fetch(scale).is_some_and(|u| &**u == href)))
                    }
                    _ => false,
                })
            })
        };
        elements.extend(
            self.styles
                .iter()
                .filter(|(_, s)| {
                    s.list_style_image.iter().any(|u| &**u == href)
                        || s.pseudo.as_deref().is_some_and(|p| [&p.marker, &p.before, &p.after].into_iter().flatten().any(|ps| in_content(ps)))
                })
                .map(|(id, _)| id),
        );
        if !elements.is_empty() {
            self.note_box_changes(&elements, &[]);
            self.want_layout_noted("image arrived that sizes a box");
        } else {
            self.needs_paint = true;
        }
    }

    /// Background, list marker and `content` images only become known
    /// after the cascade.
    fn fetch_background_images(&mut self) {
        let scale = self.viewport.scale_factor;
        let mut urls = Vec::new();
        // Each URL with the sheet its declaration came from (the
        // `Referer` of its fetch).
        let mut collect = |style: &browser_style::ComputedStyle| {
            // Every layer's image; an `image-set()` the candidate this
            // device's scale chooses.
            let mut names: Vec<browser_style::CssUrl> =
                style.background_images.iter().filter_map(|i| i.url_to_fetch(scale)).cloned().collect();
            names.extend(style.list_style_image.iter().cloned());
            // Mask layers' images.
            names.extend(style.mask_images.iter().filter_map(|i| i.url_to_fetch(scale)).cloned());
            // `cursor: url()` images too.
            names.extend(style.cursor.images.iter().map(|i| i.url.clone()));
            // A pseudo-element's or marker's `content` images.
            if let Some(items) = style.content.items() {
                names.extend(items.iter().filter_map(|i| match i {
                    browser_style::GeneratedItem::Image(img) => img.url_to_fetch(scale).cloned(),
                    _ => None,
                }));
            }
            for u in names {
                if let Ok(url) = Url::parse(&u) {
                    urls.push((url, u.referrer));
                }
            }
        };
        for (_, style) in self.styles.iter() {
            collect(style);
            // The `::marker`, `::before` and `::after` styles too.
            if let Some(p) = style.pseudo.as_deref() {
                for ps in [&p.marker, &p.before, &p.after].into_iter().flatten() {
                    collect(ps);
                }
            }
        }
        for (url, sheet) in urls {
            let referrer = self.css_referrer(sheet.as_deref());
            self.fetch_image(url, true, referrer);
        }
    }

    /// The document's faces, in the order font matching takes them: the
    /// `@font-face` rules that apply (document order), then the faces a
    /// script added to `document.fonts` (in the order added).
    fn document_faces(&mut self) -> Vec<Arc<browser_style::fontface::FontFaceRule>> {
        let mut faces = self.stylist().font_faces(&self.style_viewport());
        if let Some(host) = &mut self.script {
            self.web_fonts.script = host.script_faces();
        }
        faces.extend(self.web_fonts.script.iter().filter(|s| s.in_set).map(|s| s.rule.clone()));
        faces
    }

    /// The script face whose rule `face` is.
    fn script_face_id(&self, face: &Arc<browser_style::fontface::FontFaceRule>) -> Option<u64> {
        self.web_fonts.script.iter().find(|s| Arc::ptr_eq(&s.rule, face)).map(|s| s.id)
    }

    /// The load state of a face, as `FontFace.status` reports it.
    fn face_status(&self, face: &Arc<browser_style::fontface::FontFaceRule>, script: Option<u64>) -> browser_script::FontLoadStatus {
        use browser_script::FontLoadStatus as S;
        let key = face_key(face);
        let w = &self.web_fonts;
        if w.loaded.contains(&key) || w.late.contains(&key) || script.is_some_and(|id| w.script_data.contains_key(&id)) {
            return S::Loaded;
        }
        if script.is_some_and(|id| w.script_failed.contains(&id)) {
            return S::Error;
        }
        let wanted = w.started.contains_key(&key)
            || w.forced_css.contains(&key)
            || script.is_some_and(|id| w.forced_script.contains(&id));
        match w.source_of(face) {
            Some(Source::Url(url)) if w.requested.contains(&url) => S::Loading,
            Some(_) if wanted => S::Loading,
            None if wanted || face.sources.iter().any(|s| match s {
                browser_style::fontface::FontSource::Local(n) => w.failed.contains(&Source::Local(n.to_lowercase())),
                browser_style::fontface::FontSource::Url { url, .. } => {
                    Url::parse(url).is_ok_and(|u| w.failed.contains(&Source::Url(u)))
                }
            }) =>
            {
                S::Error
            }
            _ => S::Unloaded,
        }
    }

    /// Tell `document.fonts` what changed: the `@font-face` faces, every
    /// face's load state, and whether the document has settled (parsed,
    /// styled, laid out, no font fetch pending) so `ready` may resolve.
    fn report_fonts(&mut self) {
        if !std::mem::take(&mut self.web_fonts.dirty) || self.script.is_none() || self.doc.is_none() {
            return;
        }
        let css = self.stylist().font_faces(&self.style_viewport());
        if let Some(host) = &mut self.script {
            self.web_fonts.script = host.script_faces();
        }
        let mut statuses = Vec::new();
        for face in &css {
            statuses.push((browser_script::FaceRef::Css(face_key(face)), self.face_status(face, None)));
        }
        for s in &self.web_fonts.script {
            statuses.push((browser_script::FaceRef::Script(s.id), self.face_status(&s.rule, Some(s.id))));
        }
        let fetching = self.pending.values().any(|p| matches!(p.kind, PendingKind::Font { .. }));
        // Once settled, it stays so (the flag only lets `ready` resolve the
        // first time; later rounds must not run scripts for it).
        let settled = self.web_fonts.reported.as_ref().is_some_and(|r| r.2)
            || (!matches!(self.lifecycle, Lifecycle::Loading)
                && self.parser.is_none()
                && !self.needs_style
                && !self.needs_layout
                && self.layout.is_some()
                && !fetching);
        let keys: Vec<usize> = css.iter().map(face_key).collect();
        let report = (keys, statuses, settled);
        if self.web_fonts.reported.as_ref() == Some(&report) {
            return;
        }
        let css_list: Vec<(usize, Arc<browser_style::fontface::FontFaceRule>)> = css.iter().map(|f| (face_key(f), f.clone())).collect();
        for f in &css {
            if !self.web_fonts.seen.iter().any(|s| Arc::ptr_eq(s, f)) {
                self.web_fonts.seen.push(f.clone());
            }
        }
        let (_, statuses, settled) = &report;
        let (statuses, settled) = (statuses.clone(), *settled);
        self.web_fonts.reported = Some(report);
        self.sync_document_info();
        self.with_script(|host| host.sync_fonts(&css_list, &statuses, settled));
        self.after_script();
    }

    /// What scripts asked of font loading: faces to load now, and the
    /// bytes of faces made from a buffer (checked here: a font loads the
    /// face, anything else errors it).
    fn apply_font_requests(&mut self) {
        let Some(host) = &mut self.script else { return };
        let requests = host.take_font_requests();
        if requests.is_empty() {
            return;
        }
        for r in requests {
            match r {
                browser_script::FontRequest::Load(browser_script::FaceRef::Css(key)) => {
                    self.web_fonts.forced_css.insert(key);
                }
                browser_script::FontRequest::Load(browser_script::FaceRef::Script(id)) => {
                    self.web_fonts.forced_script.insert(id);
                }
                browser_script::FontRequest::Data { id, bytes } => {
                    if LayoutEngine::is_font_data(&bytes) {
                        self.web_fonts.script_data.insert(id, bytes);
                    } else {
                        self.web_fonts.script_failed.insert(id);
                    }
                }
            }
        }
        self.web_fonts.dirty = true;
        self.fetch_web_fonts();
    }

    /// Fetch the web fonts the document's text uses ("Working sites
    /// first" line 6), as browsers fetch them: for every character the
    /// last layout set, in its element's font (family list, style, weight,
    /// stretch), the families of the list are tried in order. A generic
    /// family ends the walk; an installed family ends it when its font has
    /// the character; an `@font-face` family takes the composite face CSS
    /// Fonts 4 §5.2 matches (its faces equal in descriptors, the latest
    /// rule first) and the first face whose `unicode-range` holds the
    /// character: loaded with the glyph, it is used; loaded without it,
    /// the next face is tried; still loading, the character waits for it;
    /// not yet asked for, it is fetched (or taken from an installed font
    /// for `local()`). Until a face arrives, or if it fails, the next
    /// families in the list are used.
    fn fetch_web_fonts(&mut self) {
        use browser_style::fontface::{FontFaceRule, best_face, family_names, is_generic_family};
        let faces = self.document_faces();
        for f in &faces {
            if !self.web_fonts.seen.iter().any(|s| Arc::ptr_eq(s, f)) {
                self.web_fonts.seen.push(f.clone());
            }
        }
        // Every family the document declares hides an installed one of
        // that name, loaded or not; registered fonts follow their faces'
        // positions.
        if let Some(engine) = &mut self.engine {
            engine.sync_web_faces(&faces);
        }
        if !faces.is_empty() || !self.web_fonts.script.is_empty() {
            self.web_fonts.dirty = true;
        }
        // Script faces in the set whose data is here (bytes a script gave,
        // or loaded before): registered now.
        for (order, face) in faces.iter().enumerate() {
            let Some(id) = self.script_face_id(face) else { continue };
            let key = face_key(face);
            if self.web_fonts.loaded.contains(&key) {
                continue;
            }
            let Some(data) = self.web_fonts.script_data.get(&id).cloned() else { continue };
            if self.engine.as_mut().is_some_and(|e| e.register_web_font(&data, face, order as u32)) {
                self.web_fonts.loaded.insert(key);
                // The engine noted the family it registered.
                self.want_layout_noted("script font face registered");
            }
        }
        if faces.is_empty() && self.web_fonts.forced_script.is_empty() {
            return;
        }
        // The characters each font of the layout's text is asked for.
        let mut runs: HashMap<(usize, u8, u16, u32), (Arc<str>, browser_style::FontStyle, u16, f32, std::collections::BTreeSet<char>)> =
            HashMap::new();
        if let Some(tree) = &self.layout {
            tree.root.walk(&mut |f| {
                if let browser_layout::FragmentContent::Text(t) = &f.content {
                    let s = &f.style;
                    let key = (Arc::as_ptr(&s.font_family) as *const u8 as usize, s.font_style as u8, s.font_weight, s.font_stretch.to_bits());
                    let run = runs
                        .entry(key)
                        .or_insert_with(|| (s.font_family.clone(), s.font_style, s.font_weight, s.font_stretch, Default::default()));
                    run.4.extend(t.text[t.range.clone()].chars());
                }
            });
        }
        let mut families: HashMap<String, Vec<(u32, &Arc<FontFaceRule>)>> = HashMap::new();
        for (i, f) in faces.iter().enumerate() {
            families.entry(f.family.to_lowercase()).or_default().push((i as u32, f));
        }
        let state = &self.web_fonts;
        let now = self.font_clock.now();
        // Each source URL with its rule's sheet (the `Referer` of the fetch).
        let mut fetch: Vec<(Url, Option<Arc<RequestReferrer>>)> = Vec::new();
        let mut locals: Vec<(u32, String)> = Vec::new();
        // The faces that begin loading now, and the characters of each run
        // that wait for a face in its block period (drawn invisibly).
        let mut starting: Vec<(usize, browser_style::fontface::FontDisplay)> = Vec::new();
        let mut blocked: HashMap<(usize, u8, u16, u32), std::collections::HashSet<char>> = HashMap::new();
        let Some(engine) = self.engine.as_mut() else { return };
        for (key, (list, style, weight, stretch, chars)) in runs {
            let names = family_names(&list);
            for c in chars {
                for (name, quoted) in &names {
                    if !quoted && is_generic_family(name) {
                        break;
                    }
                    if let Some(family) = families.get(&name.to_lowercase()) {
                        // The faces that can still load (a face whose every
                        // source failed, or whose swap period ended before
                        // it loaded, is no face).
                        let usable: Vec<(u32, &Arc<FontFaceRule>, Option<Source>)> = family
                            .iter()
                            .filter(|(_, f)| !state.timed_out.contains(&face_key(f)))
                            .filter_map(|&(o, f)| {
                                let source = state.source_of(f);
                                (source.is_some() || state.loaded.contains(&face_key(f))).then_some((o, f, source))
                            })
                            .collect();
                        let rules: Vec<&FontFaceRule> = usable.iter().map(|(_, f, _)| &***f).collect();
                        let Some(best) = best_face(&rules, style, f32::from(weight), stretch) else { continue };
                        let matched = usable[best].1.matched();
                        let mut composite: Vec<&(u32, &Arc<FontFaceRule>, Option<Source>)> =
                            usable.iter().filter(|(_, f, _)| f.matched() == matched).collect();
                        composite.sort_by(|a, b| b.0.cmp(&a.0));
                        let mut settled = false;
                        for (order, face, source) in composite {
                            if !face.covers(u32::from(c)) {
                                continue;
                            }
                            let fk = face_key(face);
                            if state.loaded.contains(&fk) {
                                if engine.web_face_has_char(face, c) {
                                    settled = true;
                                    break;
                                }
                                continue;
                            }
                            match source {
                                Some(Source::Url(url)) => {
                                    if !state.requested.contains(url) && !fetch.iter().any(|(u, _)| u == url) {
                                        fetch.push((url.clone(), face.referrer.clone()));
                                    }
                                    if !state.started.contains_key(&fk) && !starting.iter().any(|(k, _)| *k == fk) {
                                        starting.push((fk, face.display));
                                    }
                                    if state.blocking(fk, face.display, now) {
                                        blocked.entry(key).or_default().insert(c);
                                    }
                                }
                                Some(Source::Local(local)) => {
                                    if !locals.iter().any(|(o, _)| o == order) {
                                        locals.push((*order, local.clone()));
                                    }
                                }
                                None => continue,
                            }
                            settled = true;
                            break;
                        }
                        if settled {
                            break;
                        }
                        continue;
                    }
                    if engine.family_has_char(name, style, weight, stretch, c) {
                        break;
                    }
                }
            }
        }
        // Faces a script asked to load, used by text or not: an
        // `@font-face` face, or a script face (in the set or not; out of
        // it, a `local()` font only reports loaded, the face is not used).
        let mut forced: Vec<(Option<u32>, &Arc<FontFaceRule>)> = faces
            .iter()
            .enumerate()
            .filter(|(_, f)| state.forced_css.contains(&face_key(f)))
            .map(|(i, f)| (Some(i as u32), f))
            .collect();
        for s in state.script.iter().filter(|s| state.forced_script.contains(&s.id) && !state.script_data.contains_key(&s.id)) {
            forced.push((faces.iter().position(|f| Arc::ptr_eq(f, &s.rule)).map(|i| i as u32), &s.rule));
        }
        let mut local_out_of_set: Vec<usize> = Vec::new();
        let mut locals_failed_out_of_set: Vec<String> = Vec::new();
        for (order, face) in forced {
            let fk = face_key(face);
            if state.loaded.contains(&fk) || state.late.contains(&fk) || state.timed_out.contains(&fk) {
                continue;
            }
            match state.source_of(face) {
                Some(Source::Url(url)) => {
                    if !state.requested.contains(&url) && !fetch.iter().any(|(u, _)| *u == url) {
                        fetch.push((url, face.referrer.clone()));
                    }
                    if !state.started.contains_key(&fk) && !starting.iter().any(|(k, _)| *k == fk) {
                        starting.push((fk, face.display));
                    }
                }
                Some(Source::Local(name)) => match order {
                    Some(o) if !locals.iter().any(|(x, _)| *x == o) => locals.push((o, name)),
                    Some(_) => {}
                    None => {
                        if engine.has_local_font(&name) {
                            local_out_of_set.push(fk);
                        } else {
                            locals_failed_out_of_set.push(name);
                        }
                    }
                },
                None => {}
            }
        }
        for fk in local_out_of_set {
            self.web_fonts.late.insert(fk);
        }
        let mut local_changed = false;
        for name in locals_failed_out_of_set {
            self.web_fonts.failed.insert(Source::Local(name));
            local_changed = true;
        }
        for (key, display) in starting {
            self.web_fonts.started.insert(key, (now, display));
        }
        // Text waiting in a block period is drawn invisibly; the rest is
        // drawn (a period that ended, a face that arrived).
        if let Some(tree) = &mut self.layout {
            let changed = tree.set_invisible_text(|s, text| {
                let key = (Arc::as_ptr(&s.font_family) as *const u8 as usize, s.font_style as u8, s.font_weight, s.font_stretch.to_bits());
                blocked.get(&key).is_some_and(|set| text.chars().any(|c| set.contains(&c)))
            });
            if changed {
                self.needs_paint = true;
            }
        }
        // `local()` sources: the installed font of that full or PostScript
        // name, registered at once; none of that name, the next source.
        for (order, name) in locals {
            let face = &faces[order as usize];
            if self.engine.as_mut().is_some_and(|e| e.register_local_font(&name, face, order)) {
                self.web_fonts.loaded.insert(face_key(face));
                // The engine noted the family it registered.
                self.want_layout_noted("local font face registered");
            } else {
                self.web_fonts.failed.insert(Source::Local(name.to_lowercase()));
                local_changed = true;
            }
        }
        for (url, sheet) in fetch {
            if !matches!(url.scheme(), "http" | "https" | "data") {
                self.web_fonts.failed.insert(Source::Url(url));
                local_changed = true;
                continue;
            }
            if !self.web_fonts.requested.insert(url.clone()) {
                continue;
            }
            let referrer = self.css_referrer(sheet.as_deref());
            let mut request = self.subresource(url.clone(), referrer);
            request.headers.push(("Accept".to_owned(), "*/*".to_owned()));
            // CORS mode, credentials only to the document's own origin.
            if let Some(origin) = self.doc_url.as_ref().map(Url::origin) {
                if url.origin() != origin {
                    request.headers.push(("Origin".to_owned(), origin.ascii_serialization()));
                }
                request.credentials = browser_net::Credentials::SameOrigin(origin);
            }
            self.fetch(request, PendingKind::Font { url });
        }
        // A source that could not be used gives way to the next one now
        // (each round fails at least one more source, so this ends).
        if local_changed {
            self.fetch_web_fonts();
        }
    }

    /// Whether a CORS-mode response may be used by the document: from its
    /// own origin (before and after redirects), or allowed by
    /// `Access-Control-Allow-Origin` (`*` or the document's origin).
    fn cors_allows(&self, url: &Url, final_url: Option<&Url>, headers: &[(String, String)]) -> bool {
        if url.scheme() == "data" {
            return true;
        }
        let Some(origin) = self.doc_url.as_ref().map(Url::origin) else { return true };
        let same = url.origin() == origin && final_url.is_none_or(|f| f.origin() == origin);
        if same {
            return true;
        }
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("access-control-allow-origin"))
            .is_some_and(|(_, v)| {
                let v = v.trim();
                v == "*" || v == origin.ascii_serialization()
            })
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
                    referrer: sheet.referrer.clone(),
                })
            };
            stylist.add_sheet(sheet);
        }
        stylist
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
        // Intersection observers see the layout and scroll offsets the
        // frame will paint; what their callbacks change is laid out too.
        if self.update_intersection_observers() {
            self.settle();
        }
        self.report_caret();
        // With a frame still on its way to the screen, the paint waits
        // (`needs_paint` stays) for the shell's `FrameShown`.
        if self.needs_paint && self.layout.is_some() && !self.frame_in_flight {
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
            // The position a traversal's entry was left at, once its
            // document is the one laid out.
            if self.layout.is_some()
                && self.nav.is_none()
                && let Some(to) = self.pending_scroll_restore
            {
                self.restore_scroll_step(to);
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
        // The parser's (and any other) tree changes settled for the
        // controls first: a select's chosen option, a radio group.
        let settled = match &mut self.doc {
            Some(doc) => {
                doc.settle_forms();
                doc.take_controls_changed()
            }
            None => Vec::new(),
        };
        if !settled.is_empty() {
            self.note_box_changes(&settled, &[]);
        }
        let Some(doc) = &self.doc else { return };
        let started = std::time::Instant::now();
        let stylist = self.stylist();
        let vp = self.style_viewport();
        let old = std::mem::take(&mut self.styles);
        self.styles = compute_styles_with(doc, &stylist, &vp, &self.states);
        self.deps = stylist.interaction_deps();
        // Every style computed afresh: the boxes of the elements whose
        // style changed are built again (all of them when the changes
        // since the last layout are not all described anyway).
        if let Some(engine) = &mut self.engine
            && engine.box_changes_known()
        {
            let t = std::time::Instant::now();
            let changed = browser_style::changed_styles(&old, &self.styles);
            tracing::info!(target: "perf", compare_ms = t.elapsed().as_secs_f64() * 1000.0, changed = changed.len(), "full restyle compared");
            engine.note_box_changes(&changed, &[]);
        }
        tracing::debug!(
            tab = self.id.0,
            nodes = doc.node_count(),
            style_ms = started.elapsed().as_millis(),
            deps = ?self.deps,
            "restyle"
        );
        self.sync_pseudos();
        self.perf.restyle += started.elapsed();
        self.perf.full_restyles += 1;
        self.perf.styled += self.styles.len();
        // Background images only become known after the cascade (the web
        // fonts after the layout, which knows the characters each font
        // sets); their arrival triggers layout again.
        self.fetch_background_images();
    }

    fn relayout(&mut self) {
        let Some(doc) = &self.doc else { return };
        let Some(engine) = &mut self.engine else { return };
        let started = std::time::Instant::now();
        let tree = engine.layout_with_scrolls(
            doc,
            &self.styles,
            self.viewport.width,
            self.viewport.height,
            &self.images,
            &self.scrolls,
            (self.scroll_x, self.scroll_y),
        );
        self.layouts += 1;
        self.perf.layout += started.elapsed();
        self.perf.layouts += 1;
        tracing::debug!(
            tab = self.id.0,
            layout_ms = started.elapsed().as_millis(),
            content_height = tree.content_height,
            layouts = self.layouts,
            "relayout"
        );
        self.layout = Some(tree);
        self.view_rects_stale = true;
        self.after_layout();
        self.fetch_web_fonts();
    }

    /// What follows a new layout, the tab's own or one a script forced:
    /// the scroll offsets as the tree applied them, find matches and the
    /// focus ring and caret placed again.
    fn after_layout(&mut self) {
        self.sync_scrolls_after_layout();
        self.after_layout_images();
        if self.find.is_some() {
            let prefer = self.current_match_start();
            self.refresh_find(prefer, false);
        }
        if self.focus.is_some() {
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
            selection: {
                let mut ranges = self.selection_ranges();
                ranges.extend(self.control_selection_ranges());
                ranges
            },
            matches: self.find.as_ref().map(|f| f.ranges.clone()).unwrap_or_default(),
            current_match: self.find.as_ref().map(|f| f.current_ranges.clone()).unwrap_or_default(),
            caret: self.caret_rect,
            caret_node: self.focus,
            composition: self.composition_ranges(),
            scrollbar_hover: match self.bar_hover {
                Some((ScrollTarget::Element(n), v, p)) => Some((n, v, p)),
                _ => None,
            },
            scrollbar_pressed: match self.bar_pressed {
                Some((ScrollTarget::Element(n), v, p)) => Some((n, v, p)),
                _ => None,
            },
        };
        let started = std::time::Instant::now();
        paint(tree, &self.images, &options, &mut self.scene);
        tracing::debug!(tab = self.id.0, paint_ms = started.elapsed().as_millis(), "repaint");
        // The page's classic scrollbars, over everything, in their gutters
        // (the elements' are the painter's, roadmap 4a (4s) block 4).
        let page = tree.viewport.clone();
        let (vertical, horizontal, corner) = {
            let mut page = page;
            page.offset = (self.scroll_x, self.scroll_y);
            (page.vertical_scrollbar(), page.horizontal_scrollbar(), page.corner())
        };
        let transform = vello::kurbo::Affine::scale(f64::from(self.viewport.scale_factor));
        // The viewport's bars take the root element's `scrollbar-color`.
        let colors = self
            .doc
            .as_ref()
            .and_then(|d| d.document_element())
            .and_then(|r| self.styles.get(r))
            .and_then(|s| match s.scrollbar_color {
                browser_style::ScrollbarColor::Auto => None,
                browser_style::ScrollbarColor::Colors { thumb, track } => {
                    let c = |c: browser_style::Rgba| vello::peniko::Color::new([c.r, c.g, c.b, c.a]);
                    Some((c(thumb), c(track)))
                }
            });
        let state = |vertical: bool| ScrollbarState {
            hover: self.bar_hover.filter(|&(t, v, _)| t == ScrollTarget::Page && v == vertical).map(|(_, _, p)| p),
            pressed: self.bar_pressed.filter(|&(t, v, _)| t == ScrollTarget::Page && v == vertical).map(|(_, _, p)| p),
            colors,
        };
        if let Some(g) = &vertical {
            draw_scrollbar(&mut self.scene, transform, g, state(true));
        }
        if let Some(g) = &horizontal {
            draw_scrollbar(&mut self.scene, transform, g, state(false));
        }
        if let Some(c) = &corner {
            draw_corner_colored(&mut self.scene, transform, c, colors);
        }
        let scene = std::mem::take(&mut self.scene);
        self.perf.paint += started.elapsed();
        self.perf.paints += 1;
        (self.output)(self.id, TabOutput::Frame(scene));
        self.frame_in_flight = true;
    }
}

fn parse_media_attr(attr: Option<&str>) -> MediaQueryList {
    match attr {
        Some(s) => MediaQueryList::parse_str(s),
        None => MediaQueryList::all(),
    }
}

/// The shell cursor for a CSS `cursor` keyword other than `auto`.
fn css_cursor(k: browser_style::CursorKeyword) -> Cursor {
    use browser_style::CursorKeyword as K;
    match k {
        K::Auto | K::Default => Cursor::Default,
        K::None => Cursor::None,
        K::ContextMenu => Cursor::ContextMenu,
        K::Help => Cursor::Help,
        K::Pointer => Cursor::Pointer,
        K::Progress => Cursor::Progress,
        K::Wait => Cursor::Wait,
        K::Cell => Cursor::Cell,
        K::Crosshair => Cursor::Crosshair,
        K::Text => Cursor::Text,
        K::VerticalText => Cursor::VerticalText,
        K::Alias => Cursor::Alias,
        K::Copy => Cursor::Copy,
        K::Move => Cursor::Move,
        K::NoDrop => Cursor::NoDrop,
        K::NotAllowed => Cursor::NotAllowed,
        K::Grab => Cursor::Grab,
        K::Grabbing => Cursor::Grabbing,
        K::EResize => Cursor::EResize,
        K::NResize => Cursor::NResize,
        K::NeResize => Cursor::NeResize,
        K::NwResize => Cursor::NwResize,
        K::SResize => Cursor::SResize,
        K::SeResize => Cursor::SeResize,
        K::SwResize => Cursor::SwResize,
        K::WResize => Cursor::WResize,
        K::EwResize => Cursor::EwResize,
        K::NsResize => Cursor::NsResize,
        K::NeswResize => Cursor::NeswResize,
        K::NwseResize => Cursor::NwseResize,
        K::ColResize => Cursor::ColResize,
        K::RowResize => Cursor::RowResize,
        K::AllScroll => Cursor::AllScroll,
        K::ZoomIn => Cursor::ZoomIn,
        K::ZoomOut => Cursor::ZoomOut,
    }
}

fn is_link(e: &browser_dom::Element) -> bool {
    e.name.ns == ns!(html)
        && matches!(e.name.local, local_name!("a") | local_name!("area"))
        && e.attr("href").is_some()
}

/// Whether an element can take focus at all: links, enabled form
/// controls (not hidden inputs; a control in a disabled fieldset is
/// disabled), and anything with a `tabindex`.
fn is_focusable(doc: &Document, id: NodeId) -> bool {
    let Some(e) = doc.element(id) else { return false };
    if e.name.ns != ns!(html) {
        return false;
    }
    let control = matches!(
        e.name.local,
        local_name!("input") | local_name!("button") | local_name!("select") | local_name!("textarea")
    ) && doc.disabled_state(id) != Some(true)
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

/// An `<input>`'s type state's keyword (`text` when absent or invalid,
/// as HTML's enumerated attribute has it). `None` for anything that is
/// not an input.
fn input_type(e: &browser_dom::Element) -> Option<String> {
    (e.name.ns == ns!(html) && e.name.local == local_name!("input"))
        .then(|| browser_dom::forms::InputType::from_attr(e.attr("type")).as_str().to_owned())
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
        /// How many frames the tab has sent.
        frames: Arc<std::sync::atomic::AtomicUsize>,
        /// Scroll animations complete within each `pump` (the default);
        /// off, a test steps them itself.
        instant_scroll: bool,
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
            Self::start_in(url, TabId(1), TabStores::default())
        }

        /// `start` for a tab of a browser whose storage is `stores`.
        fn start_in(url: Url, id: TabId, stores: TabStores) -> Self {
            let net = Arc::new(NetService::new().expect("net service"));
            let (tx, net_events) = channel();
            let net_sink: Sink = Arc::new(move |ev| {
                let _ = tx.send(ev);
            });
            let messages = Arc::new(Mutex::new(Vec::new()));
            let sink_messages = messages.clone();
            let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let sink_frames = frames.clone();
            let output: Arc<OutputSink> = Arc::new(Box::new(move |_, out| match out {
                TabOutput::Message(m) => sink_messages.lock().expect("lock").push(m),
                TabOutput::Frame(_) => {
                    sink_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }));
            let viewport = Viewport {
                width: 800.0,
                height: 600.0,
                scale_factor: 1.0,
            };
            let state = TabState::new(id, net, net_sink, stores, viewport, output);
            let mut h = Self {
                state,
                net_events,
                messages,
                frames,
                instant_scroll: true,
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
            self.show_frame();
            self.state.flush();
        }

        /// What the shell does with a frame: shown at once here, so every
        /// flush may paint (tests that watch the pacing skip this).
        fn show_frame(&mut self) {
            if self.state.frame_in_flight {
                self.state.handle_shell(ShellToTab::FrameShown);
            }
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

        /// `pump_until` for a condition that sends the tab no event (a
        /// request the server holds): checked every few milliseconds, the
        /// network delivered in between. Panics after ten seconds.
        fn pump_polling(&mut self, mut done: impl FnMut(&Self) -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                self.pump();
                if done(self) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "timed out waiting for the tab");
                self.pump_for(std::time::Duration::from_millis(5));
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
                self.state.begin_batch(0, 0);
                self.state.tick();
                // Scroll animations reach their end at once unless a test
                // watches them frame by frame.
                if self.instant_scroll {
                    self.state.finish_scroll_animations();
                    self.state.finish_scroll_drag();
                }
                self.show_frame();
                self.state.flush();
                // The timing log, as the tab loop writes it.
                self.state.end_batch();
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

        /// Whether the focused element shows the focus ring: its computed
        /// `outline-style` is `auto` (the UA's `:focus-visible` rule).
        fn focus_ring_outline(&self) -> bool {
            self.state
                .focus
                .and_then(|f| self.state.styles.get(f))
                .is_some_and(|s| s.outline_style == browser_style::OutlineStyle::Auto)
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
    fn a_frame_waits_until_the_shell_has_shown_the_last() {
        let t = browser_layout::SCROLLBAR_THICKNESS;
        if t == 0.0 {
            return; // overlay scrollbars: nothing to drag
        }
        let mut h = Harness::load(TALL);
        let frames = |h: &Harness| h.frames.load(std::sync::atomic::Ordering::Relaxed);
        let bar = h.state.scroll_info(ScrollTarget::Page).expect("layout").vertical_scrollbar().expect("bar");
        let x = bar.bar.x + t / 2.0;
        let grab = bar.thumb_rect().y + 5.0;
        // The load's last frame is on its way; a thumb drag's moves come
        // before the shell has shown it, each in a batch of its own.
        assert!(h.state.frame_in_flight);
        let sent = frames(&h);
        let batch = |h: &mut Harness, msg: ShellToTab| {
            h.state.handle_shell(msg);
            h.state.tick();
            h.state.finish_scroll_drag();
            h.state.flush();
        };
        batch(&mut h, ShellToTab::MouseDown { x, y: grab, button: MouseButton::Left });
        for i in 1..=5 {
            batch(&mut h, ShellToTab::MouseMove { x, y: grab + 10.0 * i as f32 });
        }
        // The page scrolled with every move; nothing was painted.
        let dragged = h.state.scroll_y;
        assert!(dragged > 0.0, "{dragged}");
        assert_eq!(frames(&h), sent, "no frame before the last is shown");
        assert!(h.state.needs_paint);
        // Shown: one frame, of where the drag is now.
        batch(&mut h, ShellToTab::FrameShown);
        assert_eq!(frames(&h), sent + 1);
        assert!(!h.state.needs_paint && h.state.frame_in_flight);
        // Shown again with nothing changed: no frame.
        batch(&mut h, ShellToTab::FrameShown);
        assert_eq!(frames(&h), sent + 1);
        assert!(!h.state.frame_in_flight);
        // Nothing waiting: the next change paints in its own batch.
        batch(&mut h, ShellToTab::MouseMove { x, y: grab + 60.0 });
        assert!(h.state.scroll_y > dragged);
        assert_eq!(frames(&h), sent + 2);
        batch(&mut h, ShellToTab::MouseUp { x, y: grab + 60.0, button: MouseButton::Left });
        assert!(h.state.scroll_drag.is_none());
    }

    #[test]
    fn a_dragged_thumb_follows_the_pointer_once_per_frame() {
        let t = browser_layout::SCROLLBAR_THICKNESS;
        if t == 0.0 {
            return; // overlay scrollbars: nothing to drag
        }
        let mut h = Harness::load(TALL);
        let bar = h.state.scroll_info(ScrollTarget::Page).expect("layout").vertical_scrollbar().expect("bar");
        let x = bar.bar.x + t / 2.0;
        let grab = bar.thumb_rect().y + 5.0;
        h.send(ShellToTab::MouseDown { x, y: grab, button: MouseButton::Left });
        // The first move after the press: the thumb follows at once.
        h.state.handle_shell(ShellToTab::MouseMove { x, y: grab + 10.0 });
        h.state.tick();
        let first = h.state.scroll_y;
        assert!(first > 0.0, "{first}");
        // More moves before the next frame: recorded, nothing scrolls,
        // and the tab wakes for that frame.
        h.state.scroll_drag.as_mut().expect("drag").next = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        for i in 2..=5 {
            h.state.handle_shell(ShellToTab::MouseMove { x, y: grab + 10.0 * i as f32 });
            h.state.tick();
        }
        assert_eq!(h.state.scroll_y, first, "no scroll per move");
        let due = h.state.scroll_drag.expect("drag").next;
        assert!(h.state.next_wake().is_some_and(|w| w <= due));
        // The frame: one scroll, to the latest move.
        h.state.scroll_drag.as_mut().expect("drag").next = std::time::Instant::now();
        h.state.tick();
        let latest = h.state.scroll_y;
        assert!(latest > first, "{latest}");
        assert!(h.state.scroll_drag.expect("drag").pointer.is_none());
        assert!(h.state.scroll_drag_wake().is_none(), "nothing waits, no wake");
        // Where a drag with no frame since its last move is released: the
        // thumb ends there.
        h.state.scroll_drag.as_mut().expect("drag").next = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        h.state.handle_shell(ShellToTab::MouseMove { x, y: grab + 80.0 });
        h.state.tick();
        assert_eq!(h.state.scroll_y, latest);
        h.state.handle_shell(ShellToTab::MouseUp { x, y: grab + 80.0, button: MouseButton::Left });
        assert!(h.state.scroll_y > latest, "the release applies the last move");
        assert!(h.state.scroll_drag.is_none());
    }

    #[test]
    fn scrollbar_pages_drags_and_covers_the_page() {
        let t = browser_layout::SCROLLBAR_THICKNESS;
        if t == 0.0 {
            return; // overlay scrollbars: nothing to press
        }
        let mut h = Harness::load(TALL);
        let page = |h: &Harness| h.state.scroll_info(ScrollTarget::Page).expect("layout");
        let bar = page(&h).vertical_scrollbar().expect("tall page has a scrollbar");
        assert_eq!(bar.bar, Rect::new(800.0 - t, 0.0, t, 600.0), "the classic bar sits in its gutter");
        assert_eq!(page(&h).scrollport.width, 800.0 - t);
        let x = bar.bar.x + t / 2.0;
        // Click on the track below the thumb: a page down (seven eighths
        // of the viewport or all but 40px, Blink's step).
        h.send(ShellToTab::MouseDown { x, y: 500.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y: 500.0, button: MouseButton::Left });
        assert_eq!(h.state.scroll_y, 560.0);
        assert!(h.state.hover.is_none(), "the link under the track was not hit");
        // The arrow buttons: a line each way.
        h.send(ShellToTab::MouseDown { x, y: 600.0 - t / 2.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y: 600.0 - t / 2.0, button: MouseButton::Left });
        assert_eq!(h.state.scroll_y, 600.0);
        h.send(ShellToTab::MouseDown { x, y: t / 2.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y: t / 2.0, button: MouseButton::Left });
        assert_eq!(h.state.scroll_y, 560.0);
        // Drag the thumb back to the top, then down; far off the bar it
        // snaps back to where the drag began.
        let thumb = page(&h).vertical_scrollbar().expect("bar").thumb_rect();
        let grab_y = thumb.y + 5.0;
        h.send(ShellToTab::MouseDown { x, y: grab_y, button: MouseButton::Left });
        h.send(ShellToTab::MouseMove { x, y: t + 5.0 });
        assert_eq!(h.state.scroll_y, 0.0);
        h.send(ShellToTab::MouseMove { x, y: t + 5.0 + 100.0 });
        assert!(h.state.scroll_y > 0.0 && h.state.scroll_y < 1500.0, "{}", h.state.scroll_y);
        h.send(ShellToTab::MouseMove { x: x - 10.0 * t, y: t + 5.0 + 100.0 });
        assert_eq!(h.state.scroll_y, 560.0, "snapped back to the drag origin");
        h.send(ShellToTab::MouseUp { x, y: 105.0, button: MouseButton::Left });
        assert!(h.state.scroll_drag.is_none());
        // Shift+click on the track (below the thumb, which ends near 427
        // here) jumps so the thumb centres there.
        let thumb = page(&h).vertical_scrollbar().expect("bar").thumb_rect();
        assert!(thumb.bottom() < 450.0, "{thumb:?}");
        h.send(ShellToTab::Modifiers { shift: true, ctrl: false, alt: false, meta: false });
        h.send(ShellToTab::MouseDown { x, y: 450.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y: 450.0, button: MouseButton::Left });
        h.send(ShellToTab::Modifiers { shift: false, ctrl: false, alt: false, meta: false });
        let thumb = page(&h).vertical_scrollbar().expect("bar").thumb_rect();
        assert!((thumb.y + thumb.height / 2.0 - 450.0).abs() < 1.0, "{thumb:?}");
        // A short page has no bar and no gutter.
        let h2 = Harness::load("<p>short</p>");
        assert!(page(&h2).vertical_scrollbar().is_none());
        assert_eq!(page(&h2).scrollport.width, 800.0);
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
                TabToShell::OpenInNewTab { url, .. } => Some(url.clone()),
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
        assert_eq!(h.state.history[1].url, page("Two"));

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
        <fieldset disabled style='margin:0;padding:0;border:0'><legend><input id=leg style='width:5px'></legend><input id=fd style='width:5px'></fieldset>\
        <div></div><p><a id=last href='#last'>last</a></p>";

    #[test]
    fn tab_walks_the_focus_order_shows_a_ring_and_leaves_the_page_at_the_ends() {
        let mut h = Harness::load(FOCUS_PAGE);
        // Positive tabindex first, ascending, then tree order; hidden,
        // disabled (by a fieldset too, but not in its first legend) and
        // negative-tabindex elements are skipped.
        let expected = ["b", "a3", "a1", "s", "leg", "last"];
        for id in expected {
            h.key(Key::Tab, false);
            assert_eq!(h.focus_id().as_deref(), Some(id));
            assert_eq!(h.color_of(id), [255, 0, 0, 255], ":focus-visible on {id}");
            assert!(h.focus_ring_outline(), "a ring on {id}");
        }
        assert!(h.state.scroll_y > 900.0, "the last link was scrolled into view");
        assert!(h.focus_outs().is_empty());
        h.key(Key::Tab, false);
        assert_eq!(h.focus_id(), None);
        assert_eq!(h.focus_outs(), vec![true], "past the end, focus leaves the page");
        assert!(!h.focus_ring_outline());

        // Shift+Tab from nothing starts at the last; keeps going back.
        h.key(Key::Tab, true);
        assert_eq!(h.focus_id().as_deref(), Some("last"));
        h.key(Key::Tab, true);
        assert_eq!(h.focus_id().as_deref(), Some("leg"));
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
        assert!(!h.focus_ring_outline());
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
        /// A control's checkedness (its state, not its attribute).
        fn checked(&self, id: &str) -> bool {
            self.state.doc.as_ref().expect("document").checkedness(self.by_id(id))
        }

        /// A control's value as its alue IDL attribute reads it.
        fn value(&self, id: &str) -> Option<String> {
            Some(self.state.doc.as_ref().expect("document").control_value(self.by_id(id)))
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
        assert!(h.checked("c"));
        assert_eq!(h.color_of("cs"), [255, 0, 0, 255]);
        h.click_id("c");
        assert!(!h.checked("c"));
        assert_eq!(h.color_of("cs"), [0, 0, 0, 255]);
        // Space on the focused checkbox toggles too, on release as in
        // browsers; a disabled one never.
        h.key(Key::Space, false);
        assert!(!h.checked("c"), "not before the key goes up");
        h.key_up(Key::Space, false);
        assert!(h.checked("c"));
        h.click_id("d");
        assert!(!h.checked("d"));
        // Radios: one in the group at a time.
        h.click_id("r2");
        assert!(h.checked("r2") && !h.checked("r1"));
        h.click_id("r1");
        assert!(h.checked("r1") && !h.checked("r2"));

        // Text input: the click puts the caret at the end; keys edit.
        assert_eq!(h.control_text("t"), "ab");
        h.click_end_of("t");
        assert_eq!(h.focus_id().as_deref(), Some("t"));
        assert_eq!(h.state.caret, 2);
        let caret0 = h.state.caret_rect.expect("caret shown");
        h.typed("x");
        assert_eq!(h.value("t").as_deref(), Some("abx"));
        assert_eq!(h.control_text("t"), "abx");
        assert!(h.state.caret_rect.expect("caret").x > caret0.x, "the caret moved right");
        h.key(Key::Backspace, false);
        h.key(Key::Backspace, false);
        h.key(Key::ArrowLeft, false);
        h.typed("z");
        assert_eq!(h.value("t").as_deref(), Some("za"));
        h.key(Key::Home, false);
        h.key(Key::Delete, false);
        h.key(Key::End, false);
        h.key(Key::Space, false);
        assert_eq!(h.value("t").as_deref(), Some("a "));
        assert_eq!(h.control_text("t"), "a ", "spaces are kept");
        h.key(Key::Enter, false);
        assert_eq!(h.value("t").as_deref(), Some("a "), "Enter in an input outside a form submits nothing");
        assert_eq!(h.state.history.len(), 1, "and does not navigate");

        // Password shows bullets; a textarea takes newlines.
        h.click_id("pw");
        h.typed("pq");
        assert_eq!(h.value("pw").as_deref(), Some("pq"));
        assert_eq!(h.control_text("pw"), "\u{2022}\u{2022}");
        h.click_end_of("ta");
        h.key(Key::End, false);
        h.key(Key::Enter, false);
        h.typed("y");
        assert_eq!(h.value("ta").as_deref(), Some("hi\ny"), "the value; its children stay the default");
        assert_eq!(h.state.doc.as_ref().expect("doc").text_content(h.by_id("ta")), "hi");
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
        assert!(h.focus_ring_outline());
        std::thread::sleep(std::time::Duration::from_millis(40));
        h.pump();
        assert_eq!(console_texts(&h).last().map(String::as_str), Some("2 P big"));
        assert_eq!(h.state.focus, None, "the focused link left the document");
        assert!(!h.focus_ring_outline());
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
        // The timers start when the test says so (`go`), so a slow load
        // under a parallel test run cannot overtake the first reading.
        let page = "<!DOCTYPE html><title>CSS</title><style>@import url(/imp.css);</style><body><p id=p>x</p><div id=q>y</div>\
            <script>var s; function go() { \
              setTimeout(() => { s = document.createElement('style'); s.textContent = '#p { height: 100px }'; document.head.appendChild(s); }, 10); \
              setTimeout(() => { s.textContent = '#p { height: 50px }'; }, 60); \
              setTimeout(() => { s.remove(); var l = document.createElement('link'); l.setAttribute('rel', 'stylesheet'); l.setAttribute('href', '/ext.css'); document.head.appendChild(l); }, 110); } \
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
        run_js(&mut h, "go()");
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
            <script defer src='/d.js'></script><body><img src='/img.png'>";
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
        // The events so far, read from the page (a page timer printing
        // them raced the test's own pace on a busy machine).
        h.pump();
        assert_eq!(
            h.state.script.as_mut().and_then(|s| s.eval_to_string("log.join(' ')").ok()).as_deref(),
            Some("\"rsc:interactive onrsc:interactive dcl:interactive dcl@window:interactive\""),
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
    fn list_style_image_is_fetched_and_hangs_outside_the_item() {
        // A 1x1 PNG. The cascade names it, the tab fetches it like a
        // background image, and the next layout puts it before the
        // item's text, left of the content edge.
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        let html = format!(
            "<!DOCTYPE html><body style='margin:0'><ul style='margin:0;padding-left:40px;list-style-image:url({png})'><li>item</li></ul>"
        );
        let mut h = Harness::load(&html);
        h.pump_until(|h| !h.state.images.is_empty());
        h.pump();
        let tree = h.state.layout.as_ref().expect("layout");
        let mut image = None;
        let mut text = None;
        tree.root.walk(&mut |f| match (&f.content, f.node) {
            (browser_layout::FragmentContent::Image(_), None) => image = Some(f.rect),
            (browser_layout::FragmentContent::Text(_), Some(_)) if text.is_none() => text = Some(f.rect),
            _ => {}
        });
        let image = image.expect("the image marker is laid out");
        let text = text.expect("text");
        assert_eq!((image.width, image.height), (1.0, 1.0));
        assert!(image.right() < 40.0 && (text.x - 40.0).abs() < 0.5, "{image:?} {text:?}");
    }

    /// The generated text inside the current layout (nodeless text).
    fn generated_texts(h: &Harness) -> Vec<String> {
        let mut out = Vec::new();
        h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
            if let (browser_layout::FragmentContent::Text(t), None) = (&f.content, f.node) {
                out.push(t.text[t.range.clone()].to_owned());
            }
        });
        out
    }

    /// `::before`/`::after` ("Working sites first" 6e) through the tab:
    /// `getComputedStyle`, an `attr()` following its attribute, an inline
    /// element's rectangles with its pseudo-elements, a click on a
    /// pseudo-element's box going to its element, `:hover::after`.
    #[test]
    fn pseudo_elements_through_the_tab() {
        let html = "<!DOCTYPE html><style>body { margin: 0 } \
             #p::before { content: attr(data-x); color: rgb(0, 128, 0); display: inline-block; width: 50px } \
             #q::after { content: '!!!' } #h:hover::after { content: 'H' }</style>\
             <p id=p data-x=one>text</p><span id=q>q</span> <span id=r>q</span><div id=h style='height:20px'>h</div>\
             <script>var clicks = []; document.addEventListener('click', e => clicks.push(e.target.id)); \
             var p = document.getElementById('p'), q = document.getElementById('q'), r = document.getElementById('r');</script>";
        let mut h = Harness::load(html);
        assert_eq!(
            run_js(&mut h, "var s = getComputedStyle(p, '::before'); [s.content, s.color, s.width].join('|')"),
            "\"\\\"one\\\"|rgb(0, 128, 0)|50px\""
        );
        assert_eq!(run_js(&mut h, "[getComputedStyle(q, '::before').content, getComputedStyle(q).content, getComputedStyle(q, ':after').content].join('|')"), "\"none|normal|\\\"!!!\\\"\"");
        assert!(generated_texts(&h).contains(&"one".to_owned()));
        // The attribute changes: so does the generated text.
        run_js(&mut h, "p.setAttribute('data-x', 'two')");
        assert!(generated_texts(&h).contains(&"two".to_owned()), "{:?}", generated_texts(&h));
        // The inline element's box takes in its ::after.
        assert_eq!(run_js(&mut h, "q.getBoundingClientRect().width > r.getBoundingClientRect().width + 5"), "true");
        // A click on the ::before box goes to the paragraph.
        let p = h.find("p");
        let before = h.state.doc.as_ref().expect("doc").pseudo_element(p, browser_dom::PseudoKind::Before).expect("::before node");
        let mut rect = None;
        h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
            if f.node == Some(before) && rect.is_none() {
                rect = Some(f.rect);
            }
        });
        let rect = rect.expect("::before box");
        let (x, y) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(js(&mut h, "clicks.join()"), "\"p\"");
        // Hovering makes the ::after.
        assert!(!generated_texts(&h).contains(&"H".to_owned()));
        let (x, y) = h.center("div");
        h.send(ShellToTab::MouseMove { x, y });
        h.pump();
        assert!(generated_texts(&h).contains(&"H".to_owned()), "{:?}", generated_texts(&h));
    }

    #[test]
    fn content_images_are_fetched_and_laid_out() {
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        let html = format!("<!DOCTYPE html><style>p::before {{ content: url({png}) }}</style><p>x</p>");
        let mut h = Harness::load(&html);
        h.pump_until(|h| !h.state.images.is_empty());
        h.pump();
        let mut image = None;
        h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
            if let browser_layout::FragmentContent::Image(_) = &f.content {
                image = Some(f.rect);
            }
        });
        let image = image.expect("the content image is laid out once it arrives");
        assert_eq!((image.width, image.height), (1.0, 1.0));
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
        assert_eq!(h.state.history.iter().map(|e| e.url.clone()).collect::<Vec<_>>(), vec![target], "replace overwrote the only entry");
        assert_eq!(h.state.doc_cache, CacheMode::Default, "a replace is not a reload");
    }

    /// Evaluate `src` in the page's context (no DOM access: the document
    /// is not lent).
    fn js(h: &mut Harness, src: &str) -> String {
        h.state.script.as_mut().expect("host").eval_to_string(src).expect("eval")
    }

    /// Evaluate `src` as the tab runs a script: the document and the
    /// view lent, the aftermath applied, the batch flushed.
    fn run_js(h: &mut Harness, src: &str) -> String {
        h.state.sync_document_info();
        let mut out = None;
        h.state.with_script(|host| out = Some(host.eval_to_string(src)));
        h.state.after_script();
        h.pump();
        out.expect("a script host").expect("eval")
    }

    #[test]
    fn geometry_scrolling_and_the_viewport_from_script() {
        let html = "<!DOCTYPE html><title>Geo</title>\
             <style>body { margin: 0 } #a { width: 100px; height: 50px; margin: 10px; padding: 5px; border: 2px solid black } \
                    #rel { position: relative; margin-top: 20px; padding: 3px } #in { width: 30px; height: 30px } #tall { height: 3000px }</style>\
             <div id=a></div><div id=rel><div id=in></div><span id=s>words</span></div><div id=tall></div>\
             <script>\
               var log = []; \
               document.addEventListener('scroll', () => log.push('scroll:' + window.scrollY)); \
               window.addEventListener('resize', () => log.push('resize:' + innerWidth)); \
               visualViewport.addEventListener('resize', () => log.push('vv:' + visualViewport.width)); \
               var mql = matchMedia('(max-width: 700px)'); \
               mql.addEventListener('change', ev => log.push('mql:' + ev.matches + ':' + ev.media + ':' + (ev instanceof MediaQueryListEvent))); \
               var a = document.getElementById('a'), rel = document.getElementById('rel'), inn = document.getElementById('in'), tall = document.getElementById('tall'); \
               var r = el => { var b = el.getBoundingClientRect(); return [b.x, b.y, b.width, b.height, b.top, b.right, b.bottom, b.left].join(); }; \
             </script>";
        let mut h = Harness::load(html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");

        // Boxes: the border box, the padding box, offsets from the
        // offset parent.
        assert_eq!(run_js(&mut h, "r(a)"), "\"10,10,114,64,10,124,74,10\"");
        // `offsetTop` is 0: the top margin collapses through `body`, so
        // body's padding edge is at 10 too (the known gotcha).
        assert_eq!(
            run_js(&mut h, "[a.offsetWidth, a.offsetHeight, a.offsetLeft, a.offsetTop, a.clientWidth, a.clientHeight, a.clientLeft, a.clientTop, a.offsetParent === document.body, a.scrollWidth, a.scrollHeight, a.scrollTop].join()"),
            "\"114,64,10,0,110,60,2,2,true,110,60,0\""
        );
        assert_eq!(run_js(&mut h, "[inn.offsetParent === rel, inn.offsetLeft, inn.offsetTop, rel.offsetParent === document.body, document.body.offsetParent].join()"), "\"true,3,3,true,\"");
        assert_eq!(
            run_js(&mut h, "var cr = document.getElementById('s').getClientRects(); [cr.length, cr[0].width > 0, cr.item(0) === cr[0], cr.item(5), document.getElementById('s').offsetWidth > 0].join()"),
            "\"1,true,true,,true\""
        );
        // The window and the scrolling element: `innerWidth` counts the
        // page scrollbar, the scrolling element's `clientWidth` does not.
        assert_eq!(
            run_js(&mut h, "[document.scrollingElement === document.documentElement, innerWidth, innerHeight, devicePixelRatio, scrollX, scrollY, document.documentElement.clientWidth, document.documentElement.scrollHeight > 3000].join()"),
            format!("\"true,800,600,1,0,0,{},true\"", 800.0 - browser_layout::SCROLLBAR_THICKNESS)
        );
        assert_eq!(
            run_js(&mut h, "window.scrollTo(0, 100); [scrollY, pageYOffset, document.documentElement.scrollTop, a.getBoundingClientRect().y].join()"),
            "\"100,100,100,-90\""
        );
        assert_eq!(h.state.scroll_y, 100.0, "the tab applied the scroll");
        assert_eq!(drain(&mut h), "\"scroll:100\"");
        assert_eq!(run_js(&mut h, "scrollBy({ top: 50 }); var y1 = scrollY; document.documentElement.scrollTop = 20; [y1, scrollY].join()"), "\"150,20\"");
        assert_eq!(h.state.scroll_y, 20.0);
        drain(&mut h);
        // A change then a read: layout is forced on the spot, a `<style>`
        // the script added included; the tab keeps the result.
        assert_eq!(
            run_js(&mut h, "a.setAttribute('style', 'width: 200px'); var w = a.getBoundingClientRect().width; \
                            var st = document.createElement('style'); st.textContent = '#a { height: 80px }'; document.head.appendChild(st); [w, a.offsetHeight].join()"),
            "\"214,94\""
        );
        assert_eq!(h.rect_of(h.by_id("a")).height, 94.0);
        // scrollIntoView: to the top, to the end (clamped), centred.
        let tall_y = h.rect_of(h.by_id("tall")).y;
        assert_eq!(run_js(&mut h, "tall.scrollIntoView(); scrollY"), format!("{tall_y}"));
        assert_eq!(
            run_js(&mut h, "tall.scrollIntoView({ block: 'end' }); scrollY === document.documentElement.scrollHeight - innerHeight"),
            "true"
        );
        assert_eq!(run_js(&mut h, "a.scrollIntoView({ block: 'center' }); scrollY"), "0");
        drain(&mut h);
        // The screen and the window, as the shell reports them; a
        // resize fires `resize` on window and visualViewport and
        // `change` on the media query list.
        h.send(ShellToTab::Screen {
            window: (900.0, 700.0),
            screen: (1920.0, 1080.0),
        });
        assert_eq!(
            run_js(&mut h, "[outerWidth, outerHeight, screen.width, screen.availHeight, screen.colorDepth, screen.orientation.type, mql.matches, mql.media].join()"),
            "\"900,700,1920,1080,24,landscape-primary,false,(max-width: 700px)\""
        );
        h.send(ShellToTab::Resize(Viewport {
            width: 600.0,
            height: 500.0,
            scale_factor: 1.0,
        }));
        assert_eq!(drain(&mut h), "\"resize:600 vv:600 mql:true:(max-width: 700px):true\"");
        assert_eq!(run_js(&mut h, "[mql.matches, innerWidth, visualViewport.height, visualViewport.pageTop].join()"), "\"true,600,500,0\"");
        // The rectangle classes.
        assert_eq!(
            run_js(&mut h, "var d = new DOMRect(1, 2, 3, 4); d.width = 5; var ro = DOMRectReadOnly.fromRect({ x: 1, width: -2 }); \
                            [d.right, d.bottom, ro.left, ro.right, ro instanceof DOMRectReadOnly, d instanceof DOMRectReadOnly, JSON.stringify(ro.toJSON()).includes('top'), ro.width].join()"),
            "\"6,6,-1,1,true,true,true,-2\""
        );
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
               c.oninput = ev => log.push('input:' + ev.target.checked); c.onchange = ev => log.push('change:' + ev.target.checked); \
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
        assert!(h.state.doc.as_ref().is_some_and(|d| d.checkedness(c)));

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
               document.addEventListener('input', ev => log.push('in:' + ev.target.id + ':' + ev.inputType + ':' + ev.data + ':' + ev.target.value)); \
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
            "\"kd:ArrowDown:ArrowDown:40:false:select#s:true in:s::null:b ch:s:false\""
        );
        key(&mut h, Key::Tab, false);
        drain(&mut h);
        key(&mut h, Key::Space, false);
        h.key_up(Key::Space, false);
        assert_eq!(drain(&mut h), "\"kd: :Space:32:false:input#c:true kp: :32:32 ku:  in:c::null:on ch:c:false\"");
        key(&mut h, Key::Tab, true);
        assert_eq!(
            drain(&mut h),
            "\"kd:Tab:Tab:9:true:input#c:true blur:c:s:false:true focusout:c:s:true:true focus:s:c:false:true focusin:s:c:true:true\""
        );
    }

    #[test]
    fn pointer_events_come_first_capture_the_mouse_and_hold_back_mouse_events() {
        let html = "<!DOCTYPE html><title>Pointer</title><style>body { margin: 0 } div { height: 40px; margin: 0 }</style>\
             <div id=a tabindex=0></div><div id=b></div><div id=c></div>\
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
        // and `mouseup` until the pointer goes up, and takes
        // `mousedown`'s default with it: no focus. `click` still fires.
        h.send(ShellToTab::MouseMove { x: 10.0, y: 20.0 });
        drain(&mut h);
        js(&mut h, "cancel.pointerdown = true");
        h.send(ShellToTab::MouseDown { x: 10.0, y: 20.0, button: MouseButton::Left });
        assert!(h.state.focus.is_none(), "a cancelled pointerdown does not focus");
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
        assert_eq!(h.state.focus, Some(h.by_id("a")), "an uncancelled press focuses");
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
               document.addEventListener('change', ev => log.push('change:' + ev.target.id + ':' + ev.target.value)); \
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
        assert_eq!(drain(&mut h), "\"kd:Space:false ku: :Space:c click:c:-1::0:0:true change:c:on\"");
        assert!(h.checked("c"));
        js(&mut h, "cancel.c = true");
        h.key(Key::Space, false);
        h.key_up(Key::Space, false);
        assert!(h.checked("c"), "a cancelled click toggles nothing");
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

    impl Harness {
        fn key_mod(&mut self, key: Key, shift: bool, ctrl: bool) {
            self.send(ShellToTab::Key {
                key,
                code: String::new(),
                repeat: false,
                shift,
                ctrl,
                alt: false,
            });
        }

        fn copies(&self) -> Vec<String> {
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

        fn last_caret(&self) -> Option<Option<(f32, f32, f32, f32)>> {
            self.messages
                .lock()
                .expect("lock")
                .iter()
                .rev()
                .find_map(|m| match m {
                    TabToShell::Caret { rect } => Some(*rect),
                    _ => None,
                })
        }
    }

    #[test]
    fn text_controls_select_edit_and_use_the_clipboard() {
        let html = "<!DOCTYPE html><title>Edit</title><style>body { margin: 0 }</style>\
             <p><input id=t value='hello world'> <input id=pw type=password value=abc></p><textarea id=ta>ab\ncd</textarea>\
             <script>\
               var log = []; \
               document.addEventListener('select', ev => log.push('select:' + ev.target.id + ':' + ev.bubbles + ':' + ev.cancelable)); \
               document.addEventListener('beforeinput', ev => log.push('bi:' + ev.inputType + ':' + ev.data)); \
               document.addEventListener('input', ev => log.push('in:' + ev.inputType + ':' + ev.target.value)); \
             </script>";
        let mut h = Harness::load(html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");

        // Shift+arrows select; typing replaces the selection; Ctrl+Backspace
        // takes a word.
        h.click_end_of("t");
        assert_eq!(h.state.caret, 11);
        drain(&mut h);
        for _ in 0..5 {
            h.key_mod(Key::ArrowLeft, true, false);
        }
        assert_eq!(h.state.control_selection(), Some((6, 11)));
        assert_eq!(drain(&mut h), "\"select:t:true:false select:t:true:false select:t:true:false select:t:true:false select:t:true:false\"");
        h.typed("x");
        assert_eq!(drain(&mut h), "\"bi:insertText:x in:insertText:hello x\"");
        assert_eq!(h.state.caret, 7);
        h.key_mod(Key::Backspace, false, true);
        assert_eq!(drain(&mut h), "\"bi:deleteWordBackward:null in:deleteWordBackward:hello \"");

        // Home, Shift+End; copy, cut, paste (an input takes one line).
        h.key(Key::Home, false);
        h.key(Key::End, true);
        assert_eq!(h.state.control_selection(), Some((0, 6)));
        assert_eq!(drain(&mut h), "\"select:t:true:false\"");
        h.send(ShellToTab::Copy);
        assert_eq!(h.copies(), ["hello "]);
        h.send(ShellToTab::Cut);
        assert_eq!(h.copies(), ["hello ", "hello "]);
        assert_eq!(drain(&mut h), "\"bi:deleteByCut:null in:deleteByCut:\"");
        h.send(ShellToTab::Paste { text: "a\nb".into() });
        assert_eq!(drain(&mut h), "\"bi:insertFromPaste:ab in:insertFromPaste:ab\"");
        assert_eq!(h.value("t").as_deref(), Some("ab"));

        // A double click selects the word; Ctrl+A everything; Shift+click
        // and a drag extend from the caret.
        h.click_id("t");
        h.click_id("t");
        assert_eq!(h.state.control_selection(), Some((0, 2)));
        assert_eq!(drain(&mut h), "\"select:t:true:false\"");
        h.send(ShellToTab::SelectAll);
        assert_eq!(h.state.control_selection(), Some((0, 2)));
        assert_eq!(drain(&mut h), "\"\"", "the same selection again is no new select");
        h.click_end_of("t");
        assert_eq!(h.state.control_selection(), None);
        let r = h.rect_of(h.by_id("t"));
        h.send(ShellToTab::Modifiers { shift: true, ctrl: false, alt: false, meta: false });
        h.send(ShellToTab::MouseDown { x: r.x + 1.0, y: r.y + r.height / 2.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x: r.x + 1.0, y: r.y + r.height / 2.0, button: MouseButton::Left });
        h.send(ShellToTab::Modifiers { shift: false, ctrl: false, alt: false, meta: false });
        assert_eq!((h.state.sel_anchor, h.state.caret), (Some(2), 0));
        assert_eq!(drain(&mut h), "\"select:t:true:false\"");
        h.send(ShellToTab::MouseDown { x: r.right() - 6.0, y: r.y + r.height / 2.0, button: MouseButton::Left });
        assert_eq!(h.state.control_selection(), None);
        h.send(ShellToTab::MouseMove { x: r.x + 1.0, y: r.y + r.height / 2.0 });
        assert_eq!(h.state.control_selection(), Some((0, 2)));
        h.send(ShellToTab::MouseUp { x: r.x + 1.0, y: r.y + r.height / 2.0, button: MouseButton::Left });
        assert_eq!(h.state.selection, None, "no page selection was started");
        assert_eq!(drain(&mut h), "\"select:t:true:false\"");

        // A password: the caret and the selection work, the clipboard
        // gets nothing.
        h.click_end_of("pw");
        assert_eq!(h.state.caret, 3);
        h.key(Key::Home, true);
        assert_eq!(h.state.control_selection(), Some((0, 3)));
        h.send(ShellToTab::Copy);
        h.send(ShellToTab::Cut);
        assert_eq!(h.copies().len(), 2);
        assert_eq!(h.value("pw").as_deref(), Some("abc"));
        drain(&mut h);

        // A textarea: lines up and down keep the column.
        let r = h.rect_of(h.by_id("ta"));
        h.send(ShellToTab::MouseDown { x: r.x + 2.0, y: r.y + 10.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x: r.x + 2.0, y: r.y + 10.0, button: MouseButton::Left });
        assert_eq!((h.focus_id().as_deref(), h.state.caret), (Some("ta"), 0));
        h.key(Key::End, false);
        assert_eq!(h.state.caret, 2);
        h.key(Key::ArrowDown, false);
        assert_eq!(h.state.caret, 5);
        h.key(Key::ArrowUp, false);
        assert_eq!(h.state.caret, 2);
        h.key_mod(Key::End, false, true);
        assert_eq!(h.state.caret, 5);
        h.key(Key::Home, false);
        assert_eq!(h.state.caret, 3);
        h.key(Key::ArrowUp, true);
        assert_eq!(h.state.control_selection(), Some((0, 3)));
        h.key(Key::Delete, false);
        assert_eq!(h.value("ta").as_deref(), Some("cd"));
    }

    #[test]
    fn ime_composition_in_a_text_control_fires_its_events() {
        let html = "<!DOCTYPE html><title>IME</title><input id=t value=ab>\
             <script>\
               var log = []; var refuse = false; \
               ['compositionstart', 'compositionupdate', 'compositionend'].forEach(t => document.addEventListener(t, ev => { \
                 log.push(t.replace('composition', 'c') + ':' + ev.data + ':' + (ev instanceof CompositionEvent) + ':' + ev.cancelable); \
                 if (refuse && t === 'compositionstart') ev.preventDefault(); })); \
               document.addEventListener('beforeinput', ev => log.push('bi:' + ev.inputType + ':' + ev.data + ':' + ev.cancelable)); \
               document.addEventListener('input', ev => log.push('in:' + ev.target.value)); \
             </script>";
        let mut h = Harness::load(html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");
        let preedit = |h: &mut Harness, text: &str, cursor: Option<(usize, usize)>| {
            h.send(ShellToTab::ImePreedit {
                text: text.into(),
                cursor,
            });
        };

        h.click_end_of("t");
        assert!(matches!(h.last_caret(), Some(Some(_))), "the shell learns where the caret is");
        drain(&mut h);
        // Composing: start, then each text replaces the last.
        preedit(&mut h, "n", Some((1, 1)));
        assert_eq!(
            drain(&mut h),
            "\"cstart::true:true cupdate:n:true:false bi:insertCompositionText:n:false in:abn\""
        );
        assert_eq!(h.state.composition.map(|c| (c.start, c.len)), Some((2, 1)));
        assert_eq!(h.state.caret, 3);
        preedit(&mut h, "ni", None);
        assert_eq!(drain(&mut h), "\"cupdate:ni:true:false bi:insertCompositionText:ni:false in:abni\"");
        // A clear then a commit: the committed text replaces it.
        preedit(&mut h, "", None);
        h.send(ShellToTab::ImeCommit { text: "你".into() });
        assert_eq!(drain(&mut h), "\"bi:insertCompositionText:你:false in:ab你 cend:你:true:false\"");
        assert!(h.state.composition.is_none());
        assert_eq!(h.value("t").as_deref(), Some("ab你"));
        // A clear with no commit: the IME cancelled.
        preedit(&mut h, "k", None);
        drain(&mut h);
        preedit(&mut h, "", None);
        assert_eq!(h.value("t").as_deref(), Some("ab你k"), "nothing decided yet");
        h.key(Key::ArrowLeft, false);
        assert_eq!(
            drain(&mut h),
            "\"cupdate::true:false bi:insertCompositionText::false in:ab你 cend::true:false\""
        );
        assert_eq!(h.value("t").as_deref(), Some("ab你"));
        // A refused start: nothing is inserted, the end still comes.
        js(&mut h, "refuse = true");
        preedit(&mut h, "z", None);
        assert_eq!(drain(&mut h), "\"cstart::true:true\"");
        h.send(ShellToTab::ImeCommit { text: "z".into() });
        assert_eq!(drain(&mut h), "\"cend:z:true:false\"");
        assert_eq!(h.value("t").as_deref(), Some("ab你"));
        js(&mut h, "refuse = false");
        // Focus leaving mid-composition commits what is there.
        h.key(Key::End, false);
        preedit(&mut h, "q", None);
        drain(&mut h);
        h.key(Key::Tab, false);
        assert_eq!(drain(&mut h), "\"cend:q:true:false\"");
        assert_eq!(h.value("t").as_deref(), Some("ab你q"));
        assert!(h.state.composition.is_none());
        assert_eq!(h.last_caret(), Some(None), "no text control has focus");
        // A commit with nothing composed (a dead key) is plain text.
        h.key(Key::Tab, true);
        h.send(ShellToTab::ImeCommit { text: "é".into() });
        assert_eq!(drain(&mut h), "\"bi:insertText:é:true in:ab你qé\"");
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
        /// The head of every request received, in arrival order.
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(routes: &[(&str, &str, &str)], held: &[&str]) -> Self {
            Self::start_with(
                routes
                    .iter()
                    .map(|(p, ct, body)| ((*p).to_owned(), vec![("Content-Type".to_owned(), (*ct).to_owned())], body.as_bytes().to_vec()))
                    .collect(),
                held,
            )
        }

        /// Routes with their own headers (after `Connection: close`,
        /// before `Content-Length`) and binary bodies.
        fn start_with(routes: Vec<(String, Vec<(String, String)>, Vec<u8>)>, held: &[&str]) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let routes: Arc<HashMap<String, (Vec<(String, String)>, Vec<u8>)>> =
                Arc::new(routes.into_iter().map(|(p, headers, body)| (p, (headers, body))).collect());
            let log = Arc::new(Mutex::new(Vec::new()));
            let heads = log.clone();
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
                    let heads = heads.clone();
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
                        // A body (a form's POST) is read to its
                        // `Content-Length` and logged after the head.
                        let split = head.windows(4).position(|w| w == b"\r\n\r\n").map_or(head.len(), |p| p + 4);
                        let mut body = head.split_off(split);
                        let head = String::from_utf8_lossy(&head).into_owned();
                        let length = head
                            .lines()
                            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
                            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        while body.len() < length {
                            let Ok(n) = stream.read(&mut buf) else { return };
                            if n == 0 {
                                break;
                            }
                            body.extend_from_slice(&buf[..n]);
                        }
                        let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        heads.lock().expect("lock").push(format!("{head}{}", String::from_utf8_lossy(&body)));
                        {
                            let (set, cv) = &*gate;
                            let mut set = set.lock().expect("lock");
                            let bare = path.split('?').next().unwrap_or("/").to_owned();
                            while set.contains(&path) || set.contains(&bare) {
                                set = cv.wait(set).expect("wait");
                            }
                        }
                        // A route answers its path with any query (a GET
                        // form's).
                        let route = routes.get(&path).or_else(|| routes.get(path.split('?').next().unwrap_or("/")));
                        let response = match route {
                            Some((headers, body)) => {
                                // A `:status` pseudo-header gives a route
                                // another status line than `200 OK`.
                                let status = headers.iter().find(|(k, _)| k == ":status").map_or("200 OK", |(_, v)| v.as_str());
                                let mut r = format!("HTTP/1.1 {status}\r\nConnection: close\r\n").into_bytes();
                                for (k, v) in headers.iter().filter(|(k, _)| k != ":status") {
                                    r.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
                                }
                                r.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
                                r.extend_from_slice(body);
                                r
                            }
                            None => b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_vec(),
                        };
                        let _ = stream.write_all(&response);
                        let _ = stream.flush();
                    });
                }
            });
            Self {
                base: Url::parse(&format!("http://127.0.0.1:{port}/")).expect("url"),
                held,
                log,
            }
        }

        /// The heads (with any body) of the requests received for `path`.
        fn requests(&self, path: &str) -> Vec<String> {
            self.log
                .lock()
                .expect("lock")
                .iter()
                .filter(|h| h.split_whitespace().nth(1) == Some(path))
                .cloned()
                .collect()
        }

        /// The requests whose path (query included) starts with `prefix`.
        fn requests_starting(&self, prefix: &str) -> Vec<String> {
            self.log
                .lock()
                .expect("lock")
                .iter()
                .filter(|h| h.split_whitespace().nth(1).is_some_and(|p| p.starts_with(prefix)))
                .cloned()
                .collect()
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
        // A page is seven eighths of the viewport or all but 40px,
        // whichever is more (Blink's page step): 560 here.
        h.key(Key::PageDown, false);
        assert_eq!(h.state.scroll_y, 600.0);
        h.key(Key::Space, true);
        assert_eq!(h.state.scroll_y, 40.0);
        h.key(Key::End, false);
        assert!(h.state.scroll_y > 900.0);
        h.key(Key::Home, false);
        assert_eq!(h.state.scroll_y, 0.0);
        h.key(Key::Space, false);
        assert_eq!(h.state.scroll_y, 560.0);
    }

    impl Harness {
        /// An element scroll container's offset.
        fn offset_of(&self, id: &str) -> (f32, f32) {
            let node = self.by_id(id);
            self.state.layout.as_ref().and_then(|t| t.scroller(node)).map(|i| i.offset).unwrap_or_else(|| panic!("#{id} is not a scroller"))
        }

        fn wheel_at(&mut self, x: f32, y: f32, dy: f32) {
            self.send(ShellToTab::MouseMove { x, y });
            self.send(ShellToTab::Scroll { dx: 0.0, dy });
        }

        /// End the wheel gesture: the next wheel event picks its target anew.
        fn expire_latch(&mut self) {
            if let Some(l) = self.state.wheel_latch.as_mut() {
                l.at -= std::time::Duration::from_secs(1);
            }
        }
    }

    const SCROLL_PAGE: &str = "<!doctype html><style>body { margin: 0 } \
        #s { overflow: auto; width: 300px; height: 100px } #c { height: 1000px } \
        #n { overflow: auto; height: 50px; overscroll-behavior: contain } #tall { height: 1500px }</style>\
        <div id=s><div id=c><a href='#x' id=l>link</a><p id=m>needle</p></div></div>\
        <div id=n><div style='height: 300px'>inner</div></div><div id=tall></div>";

    #[test]
    fn wheel_scrolls_the_innermost_scroller_latches_and_chains() {
        let mut h = Harness::load(SCROLL_PAGE);
        // Over the scroller: it scrolls, the page does not.
        h.wheel_at(150.0, 50.0, 30.0);
        assert_eq!(h.offset_of("s"), (0.0, 30.0));
        assert_eq!(h.state.scroll_y, 0.0);
        // The gesture stays latched to it at its end: nothing chains yet.
        h.wheel_at(150.0, 50.0, 2000.0);
        assert_eq!(h.offset_of("s"), (0.0, 900.0));
        assert_eq!(h.state.scroll_y, 0.0);
        // A new gesture from a scroller at its end goes on to the page.
        h.expire_latch();
        h.wheel_at(150.0, 50.0, 10.0);
        assert_eq!(h.state.scroll_y, 10.0);
        assert_eq!(h.offset_of("s"), (0.0, 900.0));
        // Upward, the scroller can move again.
        h.expire_latch();
        h.wheel_at(150.0, 50.0, -10.0);
        assert_eq!(h.offset_of("s"), (0.0, 890.0));
        assert_eq!(h.state.scroll_y, 10.0);
        // `overscroll-behavior: contain` stops the chain at #n (at page
        // y 100..150, so viewport y 90..140 now).
        h.expire_latch();
        h.wheel_at(150.0, 120.0, 1000.0);
        assert_eq!(h.offset_of("n"), (0.0, 250.0));
        h.expire_latch();
        h.wheel_at(150.0, 120.0, 10.0);
        assert_eq!(h.state.scroll_y, 10.0, "contained: the page did not take the rest");
        // A layout a script forces keeps the offsets.
        let kept = run_js(&mut h, "document.body.appendChild(document.createElement('i')); document.getElementById('s').offsetHeight");
        assert_eq!(kept, "100");
        assert_eq!(h.offset_of("s"), (0.0, 890.0));
        assert_eq!(h.offset_of("n"), (0.0, 250.0));
        // `overflow: hidden` on the body: the viewport takes it, the user
        // cannot scroll the page, a script can.
        let mut h = Harness::load("<style>body { overflow: hidden; margin: 0 }</style><div style='height: 3000px'><a href=x>x</a></div>");
        h.wheel_at(100.0, 100.0, 100.0);
        assert_eq!(h.state.scroll_y, 0.0);
        h.key(Key::PageDown, false);
        assert_eq!(h.state.scroll_y, 0.0);
        assert_eq!(run_js(&mut h, "scrollTo(0, 100); scrollY"), "100");
        assert_eq!(h.state.scroll_y, 100.0);
    }

    #[test]
    fn keys_scroll_the_scroller_of_the_focus_or_of_the_last_press() {
        let mut h = Harness::load(SCROLL_PAGE);
        // A press inside the scroller: the keys go to it (nothing is
        // focused by a press on plain text).
        h.send(ShellToTab::MouseDown { x: 150.0, y: 60.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x: 150.0, y: 60.0, button: MouseButton::Left });
        h.key(Key::ArrowDown, false);
        assert_eq!(h.offset_of("s"), (0.0, 40.0));
        assert_eq!(h.state.scroll_y, 0.0);
        // Its page step is its own scrollport's.
        h.key(Key::PageDown, false);
        assert_eq!(h.offset_of("s"), (0.0, 40.0 + scrolling::page_step(100.0)));
        h.key(Key::End, false);
        assert_eq!(h.offset_of("s"), (0.0, 900.0));
        // At its end the key goes on to the page.
        h.key(Key::ArrowDown, false);
        assert_eq!(h.state.scroll_y, 40.0);
        h.key(Key::Home, false);
        assert_eq!(h.offset_of("s"), (0.0, 0.0), "Home moves the scroller that can");
        // Focus inside the scroller (Tab to the link) makes it the target.
        h.key(Key::Tab, false);
        assert_eq!(h.state.focus, Some(h.by_id("l")));
        h.key(Key::ArrowDown, false);
        assert_eq!(h.offset_of("s"), (0.0, 40.0));
        // A scroller with nothing focusable inside is in the Tab order
        // and scrolls while focused; the link after it comes next.
        let mut h = Harness::load(
            "<style>body { margin: 0 }</style><div id=k style='overflow: auto; height: 50px'><div style='height: 500px'>text</div></div><a href=x id=a>after</a>",
        );
        h.key(Key::Tab, false);
        assert_eq!(h.state.focus, Some(h.by_id("k")));
        h.key(Key::ArrowDown, false);
        assert_eq!(h.offset_of("k"), (0.0, 40.0));
        h.key(Key::Tab, false);
        assert_eq!(h.state.focus, Some(h.by_id("a")));
    }

    #[test]
    fn element_scrollbars_page_drag_and_take_the_wheel() {
        let t = browser_layout::SCROLLBAR_THICKNESS;
        if t == 0.0 {
            return;
        }
        let mut h = Harness::load(SCROLL_PAGE);
        let s = h.by_id("s");
        let bar = h.state.scroll_info(ScrollTarget::Element(s)).expect("scroller").vertical_scrollbar().expect("bar");
        assert_eq!(bar.bar, Rect::new(300.0 - t, 0.0, t, 100.0));
        let x = bar.bar.x + t / 2.0;
        // The track below the thumb (above the bottom button): the
        // scroller's own page step; the press reaches nothing in the page.
        h.send(ShellToTab::MouseDown { x, y: 70.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y: 70.0, button: MouseButton::Left });
        assert_eq!(h.offset_of("s"), (0.0, scrolling::page_step(100.0)));
        assert_eq!(h.state.scroll_y, 0.0);
        assert!(h.state.select_anchor.is_none());
        // The end button: a line; held, it repeats after a quarter second.
        h.send(ShellToTab::MouseDown { x, y: 100.0 - t / 2.0, button: MouseButton::Left });
        let after_press = h.offset_of("s").1;
        assert_eq!(after_press, scrolling::page_step(100.0) + 40.0);
        assert!(h.state.next_wake().is_some(), "a repeat is pending");
        if let Some(r) = h.state.bar_repeat.as_mut() {
            r.next = std::time::Instant::now();
        }
        h.pump();
        assert_eq!(h.offset_of("s").1, after_press + 40.0);
        h.send(ShellToTab::MouseUp { x, y: 100.0 - t / 2.0, button: MouseButton::Left });
        assert!(h.state.bar_repeat.is_none());
        // Drag the thumb down.
        let thumb = h.state.scroll_info(ScrollTarget::Element(s)).expect("scroller").vertical_scrollbar().expect("bar").thumb_rect();
        let before = h.offset_of("s").1;
        h.send(ShellToTab::MouseDown { x, y: thumb.y + 3.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseMove { x, y: thumb.y + 13.0 });
        let dragged = h.offset_of("s").1;
        assert!(dragged > before && dragged < 900.0, "{dragged}");
        h.send(ShellToTab::MouseUp { x, y: thumb.y + 13.0, button: MouseButton::Left });
        // The pointer over the bar hovers the scroller with the arrow
        // cursor, and the wheel there scrolls it.
        h.send(ShellToTab::MouseMove { x, y: 50.0 });
        assert_eq!(h.state.hover, Some(s));
        assert_eq!(h.state.cursor, Cursor::Default);
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 10.0 });
        assert_eq!(h.offset_of("s").1, dragged + 10.0);
        assert_eq!(h.state.scroll_y, 0.0);
    }

    #[test]
    fn a_selection_drag_autoscrolls_the_scroller_it_started_in() {
        let lines: String = (1..=40).map(|i| format!("<p>line {i}</p>")).collect();
        let mut h = Harness::load(&format!(
            "<style>body {{ margin: 0 }} p {{ margin: 0 }} #s {{ overflow: auto; height: 100px; width: 200px }}</style><div id=s>{lines}</div><div style='height: 2000px'></div>"
        ));
        h.send(ShellToTab::MouseDown { x: 2.0, y: 2.0, button: MouseButton::Left });
        assert!(h.state.select_anchor.is_some());
        // Below the scroller but inside the viewport: the scroller scrolls.
        h.send(ShellToTab::MouseMove { x: 50.0, y: 160.0 });
        assert!(h.state.autoscroll.is_some(), "the pointer is outside the scroller's port");
        let mut steps = 0;
        loop {
            let before = h.offset_of("s").1;
            h.state.autoscroll = Some(std::time::Instant::now());
            h.pump();
            steps += 1;
            if h.offset_of("s").1 == before || steps > 500 {
                break;
            }
        }
        assert!(h.offset_of("s").1 > 100.0, "scrolled toward the pointer: {:?}", h.offset_of("s"));
        assert_eq!(h.state.scroll_y, 0.0, "the page did not move");
        assert!(steps > 3, "a step at a time: {steps}");
        let selected = h.selected().unwrap_or_default();
        assert!(selected.contains("line 20"), "the selection followed: {selected:?}");
        h.send(ShellToTab::MouseUp { x: 50.0, y: 160.0, button: MouseButton::Left });
        assert!(h.state.autoscroll.is_none());
    }

    #[test]
    fn middle_click_autoscroll_pans_toward_the_pointer() {
        let mut h = Harness::load(TALL);
        // A press off a link starts it; the four-way cursor shows.
        h.send(ShellToTab::MouseDown { x: 400.0, y: 300.0, button: MouseButton::Middle });
        assert!(h.state.pan.is_some());
        assert_eq!(h.cursors().last(), Some(&Cursor::AllScroll));
        h.send(ShellToTab::MouseMove { x: 400.0, y: 400.0 });
        if let Some(p) = h.state.pan.as_mut() {
            p.last -= std::time::Duration::from_millis(100);
        }
        h.state.tick();
        let after_tick = h.state.scroll_y;
        assert!(after_tick > 0.0, "scrolled down toward the pointer");
        // Released after a drag: it ends and the cursor returns.
        h.send(ShellToTab::MouseUp { x: 400.0, y: 400.0, button: MouseButton::Middle });
        assert!(h.state.pan.is_none());
        assert_eq!(h.cursors().last(), Some(&Cursor::Default));
        // A click (no drag) leaves the mode on until a key or a press.
        h.send(ShellToTab::MouseDown { x: 400.0, y: 300.0, button: MouseButton::Middle });
        h.send(ShellToTab::MouseUp { x: 400.0, y: 300.0, button: MouseButton::Middle });
        assert!(h.state.pan.is_some_and(|p| !p.pressed));
        h.send(ShellToTab::MouseMove { x: 400.0, y: 200.0 });
        if let Some(p) = h.state.pan.as_mut() {
            p.last -= std::time::Duration::from_millis(100);
        }
        h.state.tick();
        assert!(h.state.scroll_y < after_tick, "panning up");
        h.key(Key::Escape, false);
        assert!(h.state.pan.is_none());
        // Within the dead zone nothing moves; on a link the middle button
        // keeps its meaning (no pan).
        h.send(ShellToTab::MouseDown { x: 400.0, y: 300.0, button: MouseButton::Middle });
        h.send(ShellToTab::MouseMove { x: 405.0, y: 305.0 });
        let before = h.state.scroll_y;
        if let Some(p) = h.state.pan.as_mut() {
            p.last -= std::time::Duration::from_millis(100);
        }
        h.state.tick();
        assert_eq!(h.state.scroll_y, before);
        h.send(ShellToTab::MouseUp { x: 405.0, y: 305.0, button: MouseButton::Middle });
        h.stop_pan_for_test();
        let (x, y) = h.center("a");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Middle });
        assert!(h.state.pan.is_none());
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Middle });
    }

    impl Harness {
        fn stop_pan_for_test(&mut self) {
            self.state.stop_pan();
        }
    }

    #[test]
    fn focus_fragment_and_find_reach_into_scrollers() {
        const PAGE: &str = "<style>body { margin: 0 } #s { overflow: auto; height: 100px; margin-top: 2000px; scroll-padding-top: 10px } \
            #a { scroll-margin-top: 30px }</style>\
            <div id=s><div style='height: 900px'></div><a href='#t' id=a>deep</a><h2 id=t>target</h2><div style='height: 500px'></div></div>";
        // Tab to the link: the scroller and the page both move so it shows.
        let mut h = Harness::load(PAGE);
        h.key(Key::Tab, false);
        assert_eq!(h.state.focus, Some(h.by_id("a")));
        let s = h.by_id("s");
        let port = h.state.layout.as_ref().expect("layout").scroller(s).expect("a scroller").scrollport;
        let link = h.rect_of(h.by_id("a"));
        assert!(h.offset_of("s").1 > 0.0, "the scroller scrolled: {:?}", h.offset_of("s"));
        assert!(link.y >= port.y && link.bottom() <= port.bottom(), "inside the scroller's port: {link:?} in {port:?}");
        assert!(link.y - 30.0 - 24.0 >= port.y - 0.5, "its scroll-margin and the focus margin are inside too");
        // The page scrolled as far as it goes (2100px of content).
        assert_eq!(h.state.scroll_y, 1500.0, "the page scrolled to the scroller");
        assert!(link.y >= h.state.scroll_y && link.bottom() <= h.state.scroll_y + 600.0);
        // The fragment: the heading's top at the scroller's top, past its
        // `scroll-padding`, and the scroller's top at the viewport's.
        let h = Harness::load_url(data_url(PAGE, Some("t")));
        let s = h.by_id("s");
        let port = h.state.layout.as_ref().expect("layout").scroller(s).expect("a scroller").scrollport;
        let heading = h.rect_of(h.by_id("t"));
        assert!((heading.y - port.y - 10.0).abs() < 0.5, "{heading:?} in {port:?}");
        // The page goes as far toward the scroller as its 2100px allow.
        assert_eq!(h.state.scroll_y, 1500.0, "{port:?}");
        // Find: the match a third of the way down the scroller and the page.
        let mut h = Harness::load(PAGE);
        h.send(ShellToTab::Find { query: "target".into() });
        let s = h.by_id("s");
        let port = h.state.layout.as_ref().expect("layout").scroller(s).expect("a scroller").scrollport;
        let heading = h.rect_of(h.by_id("t"));
        assert!(h.offset_of("s").1 > 0.0);
        assert!(heading.y >= port.y && heading.bottom() <= port.bottom(), "{heading:?} in {port:?}");
        assert!(h.state.scroll_y > 1000.0 && heading.y >= h.state.scroll_y && heading.bottom() <= h.state.scroll_y + 600.0);
    }

    #[test]
    fn element_scrolling_from_script_and_its_events() {
        let t = browser_layout::SCROLLBAR_THICKNESS;
        let mut h = Harness::load(
            "<style>body { margin: 0 } p { margin: 0 } #s { overflow: auto; width: 200px; height: 100px; position: relative; margin-top: 300px } \
             #in { height: 50px } #c { height: 1000px } #h { overflow: hidden; height: 50px } \
             #r { overflow: auto; width: 100px; height: 40px; direction: rtl } #t { scroll-margin-top: 20px }</style>\
             <div id=s><div id=in></div><div id=c><p id=t>t</p><div style='height: 882px'></div><div id=u style='height: 50px'></div></div></div>\
             <div id=h><div style='height: 500px'></div></div>\
             <div id=r><div style='width: 300px; height: 10px'></div></div><div style='height: 2000px'></div>\
             <script>var log = []; var s = document.getElementById('s'), inn = document.getElementById('in'), t = document.getElementById('t'), \
               u = document.getElementById('u'), hd = document.getElementById('h'), r = document.getElementById('r');</script>",
        );
        // `scrollTop` moves the content at once; the client box leaves the
        // gutter out; `offsetTop` is the layout position, scrolled or not.
        assert_eq!(
            run_js(&mut h, "s.scrollTop = 150; [s.scrollTop, s.scrollHeight, s.clientWidth, s.clientHeight, s.scrollWidth, s.clientLeft, inn.getBoundingClientRect().y, inn.offsetTop, t.offsetTop, t.getBoundingClientRect().y].join()"),
            format!("\"150,1050,{w},100,{w},0,150,0,50,200\"", w = 200.0 - t)
        );
        assert_eq!(h.offset_of("s"), (0.0, 150.0), "the tab took the offset");
        // `overflow: hidden` scrolls for a script, clamped; NaN is 0; a
        // box that is no scroller ignores it.
        assert_eq!(run_js(&mut h, "hd.scrollTop = 30; var a = hd.scrollTop; hd.scrollTop = 1e9; var b = hd.scrollTop; hd.scrollTop = NaN; var c = hd.scrollTop; t.scrollTop = 10; [a, b, c, t.scrollTop].join()"), "\"30,450,0,0\"");
        // Right-to-left: `scrollLeft` runs from -max to 0.
        assert_eq!(run_js(&mut h, "var a = r.scrollLeft; r.scrollLeft = -50; var b = r.scrollLeft; r.scrollLeft = 10; var c = r.scrollLeft; r.scrollLeft = -1000; [a, b, c, r.scrollLeft].join()"), "\"0,-50,0,-200\"");
        // `scrollTo`/`scrollBy` on an element; `behavior` is checked; a
        // non-scroller reads its arguments and stays.
        assert_eq!(run_js(&mut h, "s.scrollTo({top: 10}); s.scrollBy(0, 5); s.scrollTo(0, 7); var e = ''; try { s.scrollTo({top: 1, behavior: 'bogus'}) } catch (x) { e = x.name } t.scrollTo(0, 10); [s.scrollTop, e, t.scrollTop].join()"), "\"7,TypeError,0\"");
        // `scrollIntoView` through the scroller and the page, with the
        // target's `scroll-margin`; `block: end`; `container: nearest`
        // leaves the page alone.
        assert_eq!(run_js(&mut h, "s.scrollTop = 0; scrollTo(0, 0); t.scrollIntoView(); [s.scrollTop, scrollY].join()"), "\"30,300\"");
        // `end`: u's bottom at the scroller's bottom (900), then at the
        // viewport's bottom, which is above the page's top: clamped to 0.
        assert_eq!(run_js(&mut h, "u.scrollIntoView({block: 'end'}); [s.scrollTop, scrollY].join()"), "\"900,0\"");
        assert_eq!(run_js(&mut h, "scrollTo(0, 0); s.scrollTop = 0; u.scrollIntoView({block: 'start', container: 'nearest'}); [s.scrollTop, scrollY].join()"), "\"950,0\"");
        assert_eq!(run_js(&mut h, "var e = ''; try { u.scrollIntoView({block: 'middle'}) } catch (x) { e = x.name } e"), "\"TypeError\"");
        // Events: `scroll` at the element (not bubbling) and `scrollend`
        // right after for a script's scroll; a wheel gesture's `scrollend`
        // waits for the gesture to end.
        run_js(&mut h, "s.scrollTop = 0; scrollTo(0, 0); document.addEventListener('scroll', e => log.push('doc:' + e.target.nodeName)); \
                        s.onscroll = () => log.push('s:' + s.scrollTop); s.onscrollend = () => log.push('send'); document.onscrollend = () => log.push('dend'); \
                        window.addEventListener('scroll', () => log.push('win')); 0");
        // The scroll back to 0 above fires after that script, at the
        // listeners it registered (as in browsers): drain it.
        h.pump();
        assert_eq!(run_js(&mut h, "log.splice(0).join(' ')"), "\"s:0 send\"");
        assert_eq!(run_js(&mut h, "s.scrollTop = 20; 0"), "0");
        h.pump();
        assert_eq!(run_js(&mut h, "log.splice(0).join(' ')"), "\"s:20 send\"");
        h.wheel_at(100.0, 350.0, 10.0);
        assert_eq!(run_js(&mut h, "log.splice(0).join(' ')"), "\"s:30\"");
        assert!(h.state.next_wake().is_some(), "the scrollend waits for the latch");
        h.expire_latch();
        h.pump();
        assert_eq!(run_js(&mut h, "log.splice(0).join(' ')"), "\"send\"");
        // A key with nothing focused or pressed scrolls the page: the
        // document's `scroll` bubbles to the window, its `scrollend`
        // follows at once.
        h.key(Key::End, false);
        assert_eq!(run_js(&mut h, "log.splice(0).join(' ')"), "\"doc:#document win dend\"");
    }

    #[test]
    fn scrolls_animate_like_chrome_and_fire_scroll_per_frame() {
        use std::time::Duration;
        let mut h = Harness::load(TALL);
        h.instant_scroll = false;
        let page = ScrollTarget::Page;
        let advance = |h: &mut Harness, ms: u64| {
            for a in h.state.scroll_animations.values_mut() {
                a.start -= Duration::from_millis(ms);
            }
            h.pump();
        };
        // A wheel tick: ease-in-out over 200ms (a delta up to 120px); the
        // first frame moves nothing, midway it is between, at the end 100.
        h.wheel_at(400.0, 300.0, 100.0);
        assert!(h.state.scroll_y < 1.0, "{}", h.state.scroll_y);
        assert!(h.state.scroll_animations.contains_key(&page));
        assert!(h.state.next_wake().is_some());
        advance(&mut h, 100);
        let mid = h.state.scroll_y;
        assert!(mid > 20.0 && mid < 80.0, "{mid}");
        // A second tick adds to the target and the motion goes on.
        h.wheel_at(400.0, 300.0, 100.0);
        assert!(h.state.scroll_y >= mid);
        advance(&mut h, 300);
        assert_eq!(h.state.scroll_y, 200.0);
        assert!(h.state.scroll_animations.is_empty());
        // Keys: 150ms each, a second press adds its line to the target.
        h.key(Key::ArrowDown, false);
        h.key(Key::ArrowDown, false);
        assert!(h.state.scroll_y < 280.0);
        advance(&mut h, 300);
        assert_eq!(h.state.scroll_y, 280.0);
        h.expire_latch();
        h.pump();
        // A programmatic smooth scroll: `scrollY` still reads the current
        // offset; `scroll` fires per frame, `scrollend` once at the end.
        assert_eq!(run_js(&mut h, "var n = 0, ends = 0; addEventListener('scroll', () => n++); addEventListener('scrollend', () => ends++); scrollTo({top: 0, behavior: 'smooth'}); scrollY"), "280");
        let mut frames = 0;
        while !h.state.scroll_animations.is_empty() && frames < 100 {
            advance(&mut h, 40);
            frames += 1;
        }
        assert_eq!(h.state.scroll_y, 0.0);
        assert_eq!(run_js(&mut h, "[n >= 3, ends].join()"), "\"true,1\"");
        // An instant scroll cancels a smooth one in flight.
        assert_eq!(run_js(&mut h, "scrollTo({top: 200, behavior: 'smooth'}); scrollTo(0, 50); scrollY"), "50");
        h.pump();
        assert!(h.state.scroll_animations.is_empty());
        assert_eq!(h.state.scroll_y, 50.0);
        // `scroll-behavior: smooth` on the root makes `auto` scrolls smooth,
        // `instant` still jumps; the `scrollTop` setter follows it too.
        assert_eq!(run_js(&mut h, "document.documentElement.style.scrollBehavior = 'smooth'; scrollTo(0, 300); scrollY"), "50");
        assert!(h.state.scroll_animations.contains_key(&page));
        advance(&mut h, 2000);
        assert_eq!(h.state.scroll_y, 300.0);
        assert_eq!(run_js(&mut h, "scrollTo({top: 0, behavior: 'instant'}); scrollY"), "0");
        // (The page has no doctype: the scrolling element is `body`.)
        assert_eq!(run_js(&mut h, "document.scrollingElement.scrollTop = 100; scrollY"), "0");
        assert!(h.state.scroll_animations.contains_key(&page), "the setter's smooth scroll animates");
        advance(&mut h, 2000);
        assert_eq!(h.state.scroll_y, 100.0);
        // A thumb drag takes over from an animation at once.
        let t = browser_layout::SCROLLBAR_THICKNESS;
        if t > 0.0 {
            h.wheel_at(400.0, 300.0, 100.0);
            assert!(h.state.scroll_animations.contains_key(&page));
            let thumb = h.state.scroll_info(page).expect("page").vertical_scrollbar().expect("bar").thumb_rect();
            h.send(ShellToTab::MouseDown { x: 800.0 - t / 2.0, y: thumb.y + 2.0, button: MouseButton::Left });
            assert!(h.state.scroll_animations.is_empty());
            h.send(ShellToTab::MouseUp { x: 800.0 - t / 2.0, y: thumb.y + 2.0, button: MouseButton::Left });
        }
    }

    #[test]
    fn cursor_pointer_events_and_user_select() {
        let mut h = Harness::load(
            "<style>body { margin: 0 } #c { cursor: move; width: 100px; height: 50px } \
             #n { pointer-events: none; position: absolute; top: 50px; left: 0; width: 100px; height: 50px; background: red } \
             #l { display: block; height: 50px; width: 100px } p { margin: 0; height: 20px }</style>\
             <div id=c></div><a href=x id=l>link</a><div id=n></div>\
             <p id=u style='user-select: none'>secret</p><p id=a style='user-select: all'>all of this</p>\
             <p id=k style='user-select: contain'>inside contain</p><p id=after>after</p>",
        );
        // `cursor: move` on the box; through a `pointer-events: none` box
        // the link below is hovered, with the pointer cursor.
        h.send(ShellToTab::MouseMove { x: 50.0, y: 25.0 });
        assert_eq!(h.state.cursor, Cursor::Move);
        h.send(ShellToTab::MouseMove { x: 50.0, y: 75.0 });
        assert_eq!(h.state.hover, Some(h.by_id("l")));
        assert_eq!(h.state.cursor, Cursor::Pointer);
        // `user-select: none` text is left out of select-all and copy.
        h.send(ShellToTab::SelectAll);
        let all = h.selected().unwrap_or_default();
        assert!(!all.contains("secret") && all.contains("all of this"), "{all:?}");
        // A press in a `user-select: all` element selects the whole of it.
        let (x, y) = h.center("p");
        let _ = (x, y);
        let a = h.rect_of(h.by_id("a"));
        h.send(ShellToTab::MouseDown { x: a.x + 5.0, y: a.y + a.height / 2.0, button: MouseButton::Left });
        assert_eq!(h.selected().as_deref(), Some("all of this"));
        h.send(ShellToTab::MouseUp { x: a.x + 5.0, y: a.y + a.height / 2.0, button: MouseButton::Left });
        // A drag out of a `contain` element stays inside it.
        let k = h.rect_of(h.by_id("k"));
        let after = h.rect_of(h.by_id("after"));
        h.send(ShellToTab::MouseDown { x: k.x + 2.0, y: k.y + k.height / 2.0, button: MouseButton::Left });
        h.send(ShellToTab::MouseMove { x: after.right() - 2.0, y: after.y + after.height / 2.0 });
        assert_eq!(h.selected().as_deref(), Some("inside contain"));
        h.send(ShellToTab::MouseUp { x: after.right() - 2.0, y: after.y + after.height / 2.0, button: MouseButton::Left });
    }

    #[test]
    fn stacking_and_transforms_reach_hover_clicks_and_geometry() {
        let mut h = Harness::load(
            "<style>body { margin: 0 } #base { position: absolute; top: 0; left: 0; width: 100px; height: 100px; z-index: 2 } \
             #over { position: absolute; top: 0; left: 0; width: 100px; height: 100px; z-index: 1; cursor: pointer } \
             #moved { position: absolute; top: 200px; left: 0; width: 100px; height: 50px; transform: translateX(300px) } \
             #fixed { position: fixed; top: 20px; left: 400px; width: 50px; height: 50px } \
             #tall { height: 3000px }</style>\
             <div id=base></div><div id=over></div><div id=moved>text</div><div id=fixed></div><div id=tall></div>",
        );
        // The box with the higher z-index is hovered though it comes
        // first in the tree.
        h.send(ShellToTab::MouseMove { x: 50.0, y: 50.0 });
        assert_eq!(h.state.hover, Some(h.by_id("base")));
        // A translated box is hovered where it shows, not where layout
        // put it; its client rectangle says so, offsetLeft does not.
        h.send(ShellToTab::MouseMove { x: 350.0, y: 225.0 });
        assert_eq!(h.state.hover, Some(h.by_id("moved")));
        h.send(ShellToTab::MouseMove { x: 50.0, y: 225.0 });
        assert_ne!(h.state.hover, Some(h.by_id("moved")));
        assert_eq!(
            run_js(&mut h, "var m = document.getElementById('moved'), r = m.getBoundingClientRect(); [r.left, r.top, m.offsetLeft].join('|')"),
            "\"300|200|0\""
        );
        // A fixed box stays under the pointer when the page scrolls;
        // its client rectangle is viewport-relative throughout.
        h.send(ShellToTab::MouseMove { x: 425.0, y: 45.0 });
        assert_eq!(h.state.hover, Some(h.by_id("fixed")));
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 500.0 });
        assert_eq!(h.state.scroll_y, 500.0);
        h.send(ShellToTab::MouseMove { x: 425.0, y: 46.0 });
        assert_eq!(h.state.hover, Some(h.by_id("fixed")));
        assert_eq!(run_js(&mut h, "var f = document.getElementById('fixed').getBoundingClientRect(); [f.left, f.top, document.getElementById('fixed').offsetTop].join('|')"), "\"400|20|20\"");
        // The resolved transform is the matrix.
        assert_eq!(run_js(&mut h, "getComputedStyle(document.getElementById('moved')).transform"), "\"matrix(1, 0, 0, 1, 300, 0)\"");
    }

    /// "Working sites first" line 6: an `@font-face` face is fetched when
    /// rendered text uses it (a face nothing shows is not), in CORS mode
    /// with credentials only to the document's origin; a cross-origin font
    /// needs `Access-Control-Allow-Origin`, else the next family in the
    /// list shows. The load event waits for the fonts.
    #[test]
    fn web_fonts_load_when_used_and_cross_origin_needs_cors() {
        let mut collection = fontique::Collection::new(fontique::CollectionOptions::default());
        let mut cache = fontique::SourceCache::default();
        let names: Vec<String> = collection.family_names().map(str::to_owned).collect();
        let ttf = names
            .iter()
            .filter_map(|n| collection.family_by_name(n))
            .flat_map(|fam| fam.fonts().to_vec())
            .filter_map(|f| f.load(Some(&mut cache)))
            .map(|b| b.data().to_vec())
            .find(|d| {
                use read_fonts::TableProvider;
                // A single font that draws the test text (not a symbol font).
                let Ok(read_fonts::FileRef::Font(f)) = read_fonts::FileRef::new(d) else { return false };
                f.cmap().is_ok_and(|cmap| "usedclosopnhiubmrtaf".chars().all(|c| cmap.map_codepoint(c).is_some_and(|g| g.to_u32() != 0)))
            })
            .expect("an installed single-font file");
        // The served font's full name, for `local()`.
        let full_name = {
            use read_fonts::TableProvider;
            let f = read_fonts::FontRef::new(&ttf).expect("font");
            let name = f.name().expect("name table");
            name.name_record()
                .iter()
                .filter(|r| r.name_id() == read_fonts::types::NameId::FULL_NAME)
                .find_map(|r| r.string(name.string_data()).ok().map(|s| s.chars().collect::<String>()))
                .expect("a full name")
        };
        // An installed family whose fonts are not the served one.
        let installed = names
            .iter()
            .find(|n| {
                collection.family_by_name(n).is_some_and(|fam| {
                    !fam.fonts().is_empty() && fam.fonts().iter().all(|f| f.load(Some(&mut cache)).is_some_and(|b| b.data() != &ttf[..]))
                })
            })
            .expect("another installed family")
            .clone();
        let font = |cors: Option<&str>| {
            let mut headers = vec![("Content-Type".to_owned(), "font/ttf".to_owned())];
            if let Some(v) = cors {
                headers.push(("Access-Control-Allow-Origin".to_owned(), v.to_owned()));
            }
            headers
        };
        let other = Server::start_with(
            vec![
                ("/closed.ttf".to_owned(), font(None), ttf.clone()),
                ("/open.ttf".to_owned(), font(Some("*")), ttf.clone()),
            ],
            &[],
        );
        let page = format!(
            "<!DOCTYPE html><script>document.cookie = 'k=v'; var loaded = false; addEventListener('load', () => loaded = true)</script>\
             <style>@font-face {{ font-family: Used; src: url(/missing.ttf), url(/bad.ttf) format('woff2'), url(/used.ttf) }} \
             @font-face {{ font-family: Unused; src: url(/unused.ttf) }} @font-face {{ font-family: \"{installed}\"; src: url(/gone.ttf) }} \
             @font-face {{ font-family: Sub; src: url(/latin.ttf); unicode-range: U+0000-00FF }} \
             @font-face {{ font-family: Sub; src: url(/cyrillic.ttf); unicode-range: U+0400-04FF }} \
             @font-face {{ font-family: LocMiss; src: local(\"No Such Font Anywhere\"), url(/after-local.ttf) }} \
             @font-face {{ font-family: LocHit; src: local(\"{full_name}\"), url(/never.ttf) }} \
             @font-face {{ font-family: Closed; src: url({}) }} @font-face {{ font-family: Open; src: url({}) format('truetype') }}</style>\
             <p style='font-family: Used'>used</p><p style='font-family: Closed, Used'>closed</p><p style='font-family: Open'>open</p>\
             <p style='font-family: \"{installed}\", Used'>hidden</p><div style='font-family: Unused'><span></span></div>\
             <p style='font-family: Sub'>sub</p><p style='font-family: LocMiss'>locmiss</p><p style='font-family: LocHit'>lochit</p>",
            other.url("/closed.ttf"),
            other.url("/open.ttf")
        );
        let server = Server::start_with(
            vec![
                ("/page".to_owned(), vec![("Content-Type".to_owned(), "text/html; charset=utf-8".to_owned())], page.into_bytes()),
                ("/used.ttf".to_owned(), font(None), ttf.clone()),
                ("/bad.ttf".to_owned(), font(None), b"not a font".to_vec()),
                ("/latin.ttf".to_owned(), font(None), ttf.clone()),
                ("/cyrillic.ttf".to_owned(), font(None), ttf.clone()),
                ("/after-local.ttf".to_owned(), font(None), ttf.clone()),
                ("/never.ttf".to_owned(), font(None), ttf.clone()),
                ("/unused.ttf".to_owned(), font(None), ttf.clone()),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        // The fonts the layout asks for, through their fallbacks.
        h.pump_until(|h| server.requests("/after-local.ttf").len() == 1 && !h.state.pending.values().any(|p| matches!(p.kind, PendingKind::Font { .. })));
        h.pump();
        assert_eq!(run_js(&mut h, "loaded"), "true");
        let origin = server.base.origin().ascii_serialization();
        let header = |head: &str, name: &str| {
            head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_owned()))
        };
        // A source that fails (404, then data that is no font) gives way
        // to the next.
        assert_eq!(server.requests("/missing.ttf").len(), 1);
        assert_eq!(server.requests("/bad.ttf").len(), 1);
        let used = server.requests("/used.ttf");
        assert_eq!(
            used.len(),
            1,
            "fetched once: {:?}",
            server.log.lock().expect("lock").iter().map(|h| h.lines().next().unwrap_or("").to_owned()).collect::<Vec<_>>()
        );
        assert_eq!(header(&used[0], "cookie").as_deref(), Some("k=v"), "credentials to the document's origin");
        assert!(server.requests("/unused.ttf").is_empty(), "no text uses the face");
        for path in ["/closed.ttf", "/open.ttf"] {
            let r = other.requests(path);
            assert_eq!(r.len(), 1, "{path} fetched once");
            assert_eq!(header(&r[0], "cookie"), None, "no credentials across origins: {}", r[0]);
            assert_eq!(header(&r[0], "origin"), Some(origin.clone()), "a CORS request: {}", r[0]);
        }
        // Which font each paragraph's text is in.
        let mut fonts: HashMap<String, bool> = HashMap::new();
        h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
            if let browser_layout::FragmentContent::Text(t) = &f.content {
                fonts.insert(t.text[t.range.clone()].to_owned(), t.font.data.data() == &ttf[..]);
            }
        });
        assert_eq!(fonts.get("used"), Some(&true), "{fonts:?}");
        assert_eq!(fonts.get("open"), Some(&true), "allowed by ACAO: {fonts:?}");
        assert_eq!(fonts.get("closed"), Some(&true), "refused, so the next family (Used) shows: {fonts:?}");
        assert_eq!(
            fonts.get("hidden"),
            Some(&true),
            "an @font-face family named like an installed one hides it; it failed, so the next family shows: {fonts:?}"
        );
        assert_eq!(server.requests("/gone.ttf").len(), 1);
        // unicode-range: only the subset the text uses is fetched.
        assert_eq!(server.requests("/latin.ttf").len(), 1);
        assert!(server.requests("/cyrillic.ttf").is_empty(), "no Cyrillic text");
        assert_eq!(fonts.get("sub"), Some(&true));
        // local(): a name no installed font has gives way to the next
        // source; an installed font's full name is used without a fetch.
        assert_eq!(server.requests("/after-local.ttf").len(), 1);
        assert_eq!(fonts.get("locmiss"), Some(&true));
        assert!(server.requests("/never.ttf").is_empty(), "the local font is used");
        assert_eq!(fonts.get("lochit"), Some(&true), "{fonts:?}");
        // Refused leaves Closed unregistered: a page that names only it
        // shows the fallback.
        assert!(!h.state.engine.as_mut().expect("engine").has_font_family("Closed"));
    }

    /// CSS Font Loading: `document.fonts` holds the `@font-face` faces in
    /// document order, then the faces scripts add; the set's events and
    /// `ready` follow the loads; `check` and `load` match a `font`
    /// shorthand and a text; `FontFace` from a URL or from bytes loads
    /// and, in the set, is used by text; descriptors parse, serialize and
    /// refuse what does not parse.
    #[test]
    fn document_fonts_and_font_face() {
        let mut collection = fontique::Collection::new(fontique::CollectionOptions::default());
        let mut cache = fontique::SourceCache::default();
        let names: Vec<String> = collection.family_names().map(str::to_owned).collect();
        let ttf = names
            .iter()
            .filter_map(|n| collection.family_by_name(n))
            .flat_map(|fam| fam.fonts().to_vec())
            .filter_map(|f| f.load(Some(&mut cache)))
            .map(|b| b.data().to_vec())
            .filter(|d| {
                use read_fonts::TableProvider;
                let Ok(read_fonts::FileRef::Font(f)) = read_fonts::FileRef::new(d) else { return false };
                f.cmap().is_ok_and(|cmap| "usedbytesviaurl".chars().all(|c| cmap.map_codepoint(c).is_some_and(|g| g.to_u32() != 0)))
            })
            .min_by_key(Vec::len)
            .expect("an installed single-font file");
        let b64 = {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for chunk in ttf.chunks(3) {
                let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
                for i in 0..4 {
                    if i <= chunk.len() {
                        s.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                    } else {
                        s.push('=');
                    }
                }
            }
            s
        };
        let font = vec![("Content-Type".to_owned(), "font/ttf".to_owned())];
        let page = "<!DOCTYPE html><style>\
             @font-face { font-family: Used; src: url(/used.ttf) } \
             @font-face { font-family: Spare; src: url(/spare.ttf); font-weight: 700 }</style>\
             <script>var log = []; \
               document.fonts.addEventListener('loading', e => log.push('loading:' + e.fontfaces.map(f => f.family).join(','))); \
               document.fonts.onloadingdone = e => log.push('done:' + e.fontfaces.map(f => f.family).join(',')); \
               document.fonts.addEventListener('loadingerror', e => log.push('error:' + e.fontfaces.map(f => f.family).join(','))); \
               document.fonts.ready.then(s => log.push('ready:' + (s === document.fonts) + ':' + s.status));</script>\
             <p style='font-family: Used, monospace'>used</p><p style='font-family: ViaUrl, monospace'>viaurl</p>\
             <p style='font-family: Bytes, monospace'>bytes</p>";
        let server = Server::start_with(
            vec![
                ("/page".to_owned(), vec![("Content-Type".to_owned(), "text/html; charset=utf-8".to_owned())], page.as_bytes().to_vec()),
                ("/used.ttf".to_owned(), font.clone(), ttf.clone()),
                ("/spare.ttf".to_owned(), font.clone(), ttf.clone()),
                ("/via.ttf".to_owned(), font.clone(), ttf.clone()),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        h.pump_for(std::time::Duration::from_millis(50));
        h.pump();
        assert_eq!(run_js(&mut h, "log.join(' | ')"), "\"loading:Used | done:Used | ready:true:loaded\"");
        assert_eq!(
            run_js(&mut h, "[document.fonts.size, [...document.fonts].map(f => f.family + ':' + f.status).join(',')].join(' ')"),
            "\"2 Used:loaded,Spare:unloaded\""
        );
        assert_eq!(
            run_js(&mut h, "[document.fonts.check('16px Used'), document.fonts.check('bold 16px Spare'), document.fonts.check('16px NoSuch, serif')].join()"),
            "\"true,false,true\""
        );
        assert_eq!(run_js(&mut h, "try { document.fonts.check('bogus') } catch (e) { e.name }"), "\"SyntaxError\"");
        let css_face = "[...document.fonts][0]";
        assert_eq!(run_js(&mut h, &format!("try {{ document.fonts.add({css_face}) }} catch (e) {{ e.name }}")), "\"InvalidModificationError\"");
        assert_eq!(run_js(&mut h, &format!("document.fonts.delete({css_face})")), "false");
        assert_eq!(run_js(&mut h, &format!("{css_face}.weight + '/' + [...document.fonts][1].weight + '/' + {css_face}.unicodeRange + '/' + {css_face}.display")), "\"normal/700/U+0-10FFFF/auto\"");

        // load(): the faces a font and text would use; the set switches to
        // loading and back.
        // Pump until the script expression has the value.
        let wait = |h: &mut Harness, expr: &str, want: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while run_js(h, expr) != want {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {expr} == {want}: {}", run_js(h, "log.join(' | ')"));
                h.pump_for(std::time::Duration::from_millis(10));
                h.pump();
            }
        };
        run_js(&mut h, "log = []; document.fonts.load('bold 16px Spare').then(fs => log.push('loaded:' + fs.length + ':' + fs[0].family + ':' + fs[0].status))");
        wait(&mut h, "log.length", "3");
        assert_eq!(run_js(&mut h, "log.join(' | ')"), "\"loading:Spare | done:Spare | loaded:1:Spare:loaded\"");
        assert_eq!(server.requests("/spare.ttf").len(), 1, "loaded because asked, no text uses it");

        // A script face from a URL, added to the set: used by its text.
        run_js(
            &mut h,
            "log = []; var via = new FontFace('ViaUrl', 'url(/via.ttf)'); document.fonts.add(via); \
             via.load().then(f => log.push('via:' + (f === via) + ':' + f.status))",
        );
        wait(&mut h, "log.includes('via:true:loaded')", "true");
        // A face from bytes: a font loads; anything else errors.
        run_js(
            &mut h,
            &format!(
                "var bin = atob('{b64}'); var buf = new Uint8Array(bin.length); for (var i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i); \
                 var bytes = new FontFace('Bytes', buf.buffer); document.fonts.add(bytes); \
                 bytes.loaded.then(f => log.push('bytes:' + f.status)); \
                 var bad = new FontFace('Bad', new Uint8Array([1, 2, 3])); bad.loaded.catch(e => log.push('bad:' + e.name + ':' + bad.status))"
            ),
        );
        wait(&mut h, "log.filter(l => l.startsWith('b')).length", "2");
        h.pump();
        let log = run_js(&mut h, "log.filter(l => l.startsWith('b')).sort().join(' | ')");
        assert_eq!(log, "\"bad:NetworkError:error | bytes:loaded\"");
        let mut fonts: HashMap<String, bool> = HashMap::new();
        h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
            if let browser_layout::FragmentContent::Text(t) = &f.content {
                fonts.insert(t.text[t.range.clone()].to_owned(), t.font.data.data() == &ttf[..]);
            }
        });
        assert_eq!(fonts.get("viaurl"), Some(&true), "{fonts:?}");
        assert_eq!(fonts.get("bytes"), Some(&true), "{fonts:?}");
        assert_eq!(run_js(&mut h, "document.fonts.size + ':' + document.fonts.has(via) + ':' + document.fonts.delete(via) + ':' + document.fonts.size"), "\"4:true:true:3\"");

        // Descriptors: parsed and serialized; a bad one errors the face
        // (no throw) and a bad setter throws.
        assert_eq!(
            run_js(
                &mut h,
                "var x = new FontFace('X', 'url(x.woff2)', {weight: '300 500', unicodeRange: 'U+0-7F, U+100', display: 'swap', style: 'oblique 10deg', sizeAdjust: '90%'}); \
                 [x.weight, x.unicodeRange, x.display, x.style, x.sizeAdjust, x.stretch, x.status].join('/')"
            ),
            "\"300 500/U+0-7F, U+100/swap/oblique 10deg/90%/normal/unloaded\""
        );
        assert_eq!(run_js(&mut h, "var y = new FontFace('Y', 'url(y)', {weight: 'heavy'}); y.status"), "\"error\"");
        assert_eq!(run_js(&mut h, "try { x.weight = 'heavy'; 'no' } catch (e) { e.name + ':' + x.weight }"), "\"SyntaxError:300 500\"");
        assert_eq!(run_js(&mut h, "try { new FontFace('Z') } catch (e) { e.name }"), "\"TypeError\"");
    }

    /// `font-display`, as the WPT `css-fonts/font-display` reference has
    /// it: while a face loads, its text is invisible during the block
    /// period (`block`: 3 s; `fallback`: 100 ms; `swap`, `optional`: none)
    /// and shown in the fallback after it; a face arriving in its swap
    /// period swaps in (`swap`, `fallback`; `optional` only within 100 ms),
    /// one arriving after it does not.
    #[test]
    fn font_display_blocks_swaps_and_gives_up() {
        let mut collection = fontique::Collection::new(fontique::CollectionOptions::default());
        let mut cache = fontique::SourceCache::default();
        let names: Vec<String> = collection.family_names().map(str::to_owned).collect();
        let ttf = names
            .iter()
            .filter_map(|n| collection.family_by_name(n))
            .flat_map(|fam| fam.fonts().to_vec())
            .filter_map(|f| f.load(Some(&mut cache)))
            .map(|b| b.data().to_vec())
            .find(|d| {
                use read_fonts::TableProvider;
                let Ok(read_fonts::FileRef::Font(f)) = read_fonts::FileRef::new(d) else { return false };
                f.cmap().is_ok_and(|cmap| "blockswapfallbackoptional".chars().all(|c| cmap.map_codepoint(c).is_some_and(|g| g.to_u32() != 0)))
            })
            .expect("an installed single-font file");
        let font = vec![("Content-Type".to_owned(), "font/ttf".to_owned())];
        let page = "<!DOCTYPE html><style>\
             @font-face { font-family: B; src: url(/b.ttf); font-display: block } \
             @font-face { font-family: S; src: url(/s.ttf); font-display: swap } \
             @font-face { font-family: F; src: url(/f.ttf); font-display: fallback } \
             @font-face { font-family: O; src: url(/o.ttf); font-display: optional } \
             @font-face { font-family: Q; src: url(/q.ttf); font-display: optional }</style>\
             <p style='font-family: B, monospace'>block</p><p style='font-family: S, monospace'>swap</p>\
             <p style='font-family: F, monospace'>fallback</p><p style='font-family: O, monospace'>optional</p>\
             <p style='font-family: Q, monospace'>soon</p>";
        let server = Server::start_with(
            vec![
                ("/page".to_owned(), vec![("Content-Type".to_owned(), "text/html; charset=utf-8".to_owned())], page.as_bytes().to_vec()),
                ("/b.ttf".to_owned(), font.clone(), ttf.clone()),
                ("/s.ttf".to_owned(), font.clone(), ttf.clone()),
                ("/f.ttf".to_owned(), font.clone(), ttf.clone()),
                ("/o.ttf".to_owned(), font.clone(), ttf.clone()),
                ("/q.ttf".to_owned(), font.clone(), ttf.clone()),
            ],
            &["/b.ttf", "/s.ttf", "/f.ttf", "/o.ttf"],
        );
        // Each paragraph's text: (invisible, in the web font).
        let shown = |h: &Harness| {
            let mut out: HashMap<String, (bool, bool)> = HashMap::new();
            if let Some(tree) = &h.state.layout {
                tree.root.walk(&mut |f| {
                    if let browser_layout::FragmentContent::Text(t) = &f.content {
                        out.insert(t.text[t.range.clone()].to_owned(), (t.invisible, t.font.data.data() == &ttf[..]));
                    }
                });
            }
            out
        };
        // The periods run on a clock the test moves: no period ends while
        // a busy machine is slow to get here.
        let mut h = Harness::start(server.url("/page"));
        h.state.font_clock = FontClock::frozen();
        h.pump();
        // The held font's request sends the tab no event: polled.
        h.pump_polling(|h| shown(h).len() == 5 && server.requests("/o.ttf").len() == 1);
        let s = shown(&h);
        assert_eq!(s["block"], (true, false), "block: invisible while loading");
        assert_eq!(s["swap"], (false, false), "swap: the fallback at once");
        assert_eq!(s["fallback"], (true, false), "fallback: a short block period");
        assert_eq!(s["optional"], (false, false), "optional: the fallback at once");
        // An optional face that arrives at once is used.
        h.pump_until(|h| shown(h).get("soon") == Some(&(false, true)));
        // Past 100 ms: fallback and optional show their fallback; block
        // still waits.
        h.state.font_clock.advance(std::time::Duration::from_millis(250));
        h.pump();
        let s = shown(&h);
        assert_eq!(s["block"], (true, false));
        assert_eq!(s["fallback"], (false, false));
        assert_eq!(s["optional"], (false, false));
        // The fonts arrive (within fallback's 3 s swap period): swap and
        // fallback swap in, optional's swap period is over, block's text
        // shows in its face.
        for p in ["/b.ttf", "/s.ttf", "/f.ttf", "/o.ttf"] {
            server.release(p);
        }
        h.pump_until(|h| shown(h).get("block") == Some(&(false, true)) && !h.state.pending.values().any(|p| matches!(p.kind, PendingKind::Font { .. })));
        h.pump();
        let s = shown(&h);
        assert_eq!(s["block"], (false, true));
        assert_eq!(s["swap"], (false, true));
        assert_eq!(s["fallback"], (false, true));
        assert_eq!(s["optional"], (false, false), "too late: the fallback stays");
    }

    #[test]
    fn document_cookie_reads_and_writes_the_jar_and_is_disabled_on_data_urls() {
        let server = Server::start(&[("/page", "text/html; charset=utf-8", "<p>cookies</p>")], &[]);
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| !h.state.is_loading());
        assert_eq!(run_js(&mut h, "document.cookie"), "\"\"");
        assert_eq!(
            run_js(&mut h, "document.cookie = 'a=1'; document.cookie = 'b=2; path=/'; document.cookie = 'h=3; HttpOnly'; document.cookie"),
            "\"a=1; b=2\""
        );
        // The jar has them; the next request to the server carries them.
        assert_eq!(h.state.net.cookies().len(), 2);
        assert_eq!(run_js(&mut h, "document.cookie = 'a=; max-age=0'; document.cookie"), "\"b=2\"");
        // A `data:` document has an opaque origin: SecurityError.
        let mut d = Harness::load("<p>x</p>");
        assert_eq!(
            run_js(&mut d, "try { document.cookie } catch (e) { e.name + ': ' + e.message }"),
            "\"SecurityError: Failed to read the 'cookie' property from 'Document': Cookies are disabled inside 'data:' URLs.\""
        );
    }

    #[test]
    fn the_caret_keeps_its_control_scrolled_to_it() {
        let lines: String = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let mut h = Harness::load(&format!(
            "<style>body {{ margin: 0 }}</style><textarea id=ta rows=3>{lines}</textarea><input id=i style='width: 100px' value='{}'>",
            "wide ".repeat(30)
        ));
        // Focus by click puts the caret at the end: the textarea scrolls
        // down to it and the caret rectangle is inside its port.
        let (x, y) = h.center("textarea");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        assert_eq!(h.state.focus, Some(h.by_id("ta")));
        let ta = h.by_id("ta");
        let port = h.state.layout.as_ref().expect("layout").scroller(ta).expect("a scroller").scrollport;
        let caret = h.state.caret_rect.expect("a caret");
        assert!(h.offset_of("ta").1 > 0.0, "{:?}", h.offset_of("ta"));
        assert!(caret.y >= port.y - 0.5 && caret.bottom() <= port.bottom() + 0.5, "{caret:?} in {port:?}");
        // Up through the lines: the textarea follows back up to the first
        // line. The caret's line is what is revealed, so the 1px of
        // padding above it stays scrolled out (Blink reveals the caret
        // rectangle, not the padding).
        for _ in 0..25 {
            h.key(Key::ArrowUp, false);
        }
        assert_eq!(h.offset_of("ta"), (0.0, 1.0));
        let caret = h.state.caret_rect.expect("a caret");
        assert!(caret.y >= port.y - 0.5);
        // An input scrolls sideways to its caret at the end of a long value.
        let (x, y) = h.center("input");
        h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
        h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
        h.key(Key::End, false);
        let i = h.by_id("i");
        let port = h.state.layout.as_ref().expect("layout").scroller(i).expect("a scroller").scrollport;
        let caret = h.state.caret_rect.expect("a caret");
        assert!(h.offset_of("i").0 > 0.0, "{:?}", h.offset_of("i"));
        assert!(caret.x >= port.x - 0.5 && caret.right() <= port.right() + 0.5, "{caret:?} in {port:?}");
        h.key(Key::Home, false);
        assert_eq!(h.offset_of("i").0, 0.0);
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

    #[test]
    fn computed_style_reports_resolved_values_and_style_writes_restyle() {
        let html = "<!DOCTYPE html><title>CSSOM</title>\
             <style>body { margin: 0 } :root { --theme: dark } \
                    #a { width: 50%; height: 40px; padding: 5%; margin: 10px auto; border: 2px solid; line-height: 1.5; font-size: 20px; color: #123456; background: rgba(255, 0, 0, .5) url(x.png) no-repeat; font-family: Helvetica Neue, serif; text-decoration: dotted underline } \
                    #rel { position: relative } #abs { position: absolute; top: 10px; left: 20px; width: 30px; height: 30px } \
                    #none { display: none; width: 100px } #em { font-size: 2em }</style>\
             <div id=a>t<em id=em>e</em></div><div id=rel><div id=abs></div></div><div id=none></div><span id=inl>x</span>\
             <script>var a = document.getElementById('a'), em = document.getElementById('em'), abs = document.getElementById('abs'), none = document.getElementById('none'), inl = document.getElementById('inl'); \
                     var cs = el => getComputedStyle(el);</script>";
        let mut h = Harness::load(html);
        // Used values: the content box width, percentage padding and
        // auto margins in pixels, the line height in pixels; colors as
        // rgb(); the computed keyword for the rest.
        assert_eq!(
            run_js(&mut h, "var c = cs(a); [c.width, c.height, c.paddingLeft, c.paddingTop, c.paddingBottom, a.offsetHeight, c.marginLeft, c.marginTop, c.margin, c.borderTopWidth, c.borderTopColor, c.lineHeight, c.fontSize, c.color, c.backgroundColor, c.backgroundImage.endsWith('x.png\")'), c.backgroundRepeat, c.display, c.position, c.fontFamily, c.textDecorationLine, c.textDecorationStyle, c.boxSizing].join('|')"),
            "\"400px|40px|40px|40px|40px|124|158px|10px|10px 158px|2px|rgb(18, 52, 86)|30px|20px|rgb(18, 52, 86)|rgba(255, 0, 0, 0.5)|true|no-repeat|block|static|\\\"Helvetica Neue\\\", serif|underline|dotted|content-box\""
        );
        // The decoration propagates to the text but the child's own value
        // is none; `em` resolves against the parent's size; the shorthand
        // `font` is rebuilt from resolved values.
        assert_eq!(
            run_js(&mut h, "var c = cs(em); [c.textDecorationLine, c.fontSize, c.font, c.fontStyle].join('|')"),
            "\"none|40px|italic 40px / 60px \\\"Helvetica Neue\\\", serif|italic\""
        );
        // Positioned: used insets from the containing block; display
        // none: computed values; an inline element: no used size.
        assert_eq!(
            run_js(&mut h, "[cs(abs).top, cs(abs).left, cs(abs).right, cs(none).width, cs(none).display, cs(inl).width, cs(inl).display, cs(inl).marginLeft].join('|')"),
            "\"10px|20px|750px|100px|none|auto|inline|0px\""
        );
        // A grid container ("Working sites first" line 5) reports its used
        // tracks with their line names; the declared value stays as
        // written; `grid-area` collapses back.
        assert_eq!(
            run_js(&mut h, "var gr = document.createElement('div'); gr.style.cssText = 'display:grid;width:300px;grid-template-columns:[a] 100px 1fr;grid-area:2 / 1'; document.body.appendChild(gr); \
                            [cs(gr).display, cs(gr).gridTemplateColumns, gr.style.gridTemplateColumns, gr.style.gridArea, cs(gr).gridRowStart, cs(gr).gridColumnEnd].join('|')"),
            "\"grid|[a] 100px 200px|[a] 100px 1fr|2 / 1|2|auto\""
        );
        // The used insets are the distances to the margin box ("Working
        // sites first" line 4): a margin does not change `top`.
        assert_eq!(
            run_js(&mut h, "abs.style.margin = '5px'; [cs(abs).top, cs(abs).left, cs(abs).right].join('|')"),
            "\"10px|20px|740px\""
        );
        // Custom properties are listed after the longhands; the list is
        // read-only and `cssText` empty.
        assert_eq!(
            run_js(&mut h, "var c = cs(document.body); [c.getPropertyValue('--theme'), c.length > 80, c.item(c.length - 1), c.item(0), JSON.stringify(c.cssText), c.getPropertyValue('nope'), (() => { try { c.width = '1px'; return 'set' } catch (e) { return e.name } })()].join('|')"),
            "\"dark|true|--theme|display|\\\"\\\"||NoModificationAllowedError\""
        );
        // A write then a read in one script: style and layout are current
        // at once; the tab keeps the result.
        assert_eq!(
            run_js(&mut h, "a.style.width = '100px'; a.style.boxSizing = 'border-box'; var w1 = cs(a).width; a.style.boxSizing = ''; [w1, cs(a).width, a.getBoundingClientRect().width, a.getAttribute('style')].join('|')"),
            "\"100px|100px|184|width: 100px;\""
        );
        assert_eq!(
            run_js(&mut h, "a.style.setProperty('color', 'blue', 'important'); a.style.display = 'none'; [cs(a).color, cs(a).width, a.offsetWidth, cs(em).color].join('|')"),
            "\"rgb(0, 0, 255)|100px|0|rgb(0, 0, 255)\""
        );
        // The tab restyled and laid out after the script: nothing is
        // left on screen for `a`, and the attribute set back clears it.
        assert_eq!(run_js(&mut h, "a.offsetWidth"), "0");
        assert_eq!(run_js(&mut h, "a.setAttribute('style', ''); [a.style.length, cs(a).display, a.offsetWidth].join('|')"), "\"0|block|484\"");
        // Pseudo-elements: no rules or boxes here, so they inherit from
        // the element; an unknown one is empty; a name without a colon
        // is the element itself.
        assert_eq!(
            run_js(&mut h, "[cs(a, '::before').color, getComputedStyle(a, ':after').display, getComputedStyle(a, '::nope').length, getComputedStyle(a, 'garbage').width].join('|')"),
            "\"rgb(18, 52, 86)|inline|0|400px\""
        );
        // A detached element has inline style but no computed style.
        assert_eq!(
            run_js(&mut h, "var e = document.createElement('div'); e.style.color = 'red'; [JSON.stringify(cs(e).color), cs(e).length, e.style.color].join('|')"),
            "\"\\\"\\\"|0|red\""
        );
        // `revert` in the style attribute rolls back past the author
        // sheet: no UA color for a div, so the inherited black.
        assert_eq!(
            run_js(&mut h, "a.style.color = 'revert'; [a.style.color, cs(a).color, cs(em).color].join('|')"),
            "\"revert|rgb(0, 0, 0)|rgb(0, 0, 0)\""
        );
        // Roadmap 4a block 1: the text properties resolve (`letter-spacing`
        // in pixels, `word-wrap` as `overflow-wrap`, both inherited by
        // `em`), and a `letter-spacing` write moves the text, so the
        // element laid out wider at once.
        assert_eq!(
            run_js(&mut h, "var c = cs(a); var w0 = a.offsetWidth; a.style.width = 'auto'; a.style.display = 'inline-block'; a.style.whiteSpace = 'nowrap'; var w1 = a.offsetWidth; \
                            a.style.letterSpacing = '0.5em'; a.style.wordWrap = 'break-word'; a.style.textOverflow = 'ellipsis'; a.style.wordBreak = 'keep-all'; var w2 = a.offsetWidth; \
                            c = cs(a); [c.letterSpacing, c.wordWrap, c.overflowWrap, c.wordBreak, c.textOverflow, cs(em).letterSpacing, cs(em).overflowWrap, cs(em).textOverflow, w2 - w1 >= 20, c.getPropertyValue('word-wrap')].join('|')"),
            "\"10px|break-word|break-word|keep-all|ellipsis|10px|break-word|clip|true|break-word\""
        );
        // "Working sites first" 6h: `word-spacing` resolves to its computed
        // value (`normal` as 0px, percentages kept, inherited), and a write
        // widens each space of the laid-out text at once.
        assert_eq!(
            run_js(&mut h, "var ws = document.createElement('span'); ws.textContent = 'a b c'; ws.style.display = 'inline-block'; ws.style.whiteSpace = 'nowrap'; \
                            var wsi = document.createElement('b'); wsi.textContent = 'x'; ws.append(wsi); document.body.append(ws); \
                            var v0 = cs(ws).wordSpacing; var w0 = ws.offsetWidth; ws.style.wordSpacing = '10px'; var w1 = ws.offsetWidth; var v1 = cs(wsi).wordSpacing; \
                            ws.style.wordSpacing = '50%'; var v2 = cs(wsi).wordSpacing; ws.remove(); [v0, Math.round(w1 - w0), v1, v2].join('|')"),
            "\"0px|20|10px|50%\""
        );
        // Roadmap 4a block 3, the inline box model: an inline element has
        // line fragments of its own, so its rects grow with its padding
        // and its used padding is reported; `box-decoration-break`
        // resolves, the `-webkit-` alias through both IDL attributes.
        assert_eq!(
            run_js(&mut h, "var w1 = inl.getBoundingClientRect().width; inl.style.padding = '0 10px'; var r = inl.getClientRects(); var w0 = inl.getBoundingClientRect().width; var pl = cs(inl).paddingLeft; \
                            inl.style.padding = ''; inl.style.webkitBoxDecorationBreak = 'clone'; var c = cs(inl); \
                            [r.length, w0 - w1, pl, c.boxDecorationBreak, c.webkitBoxDecorationBreak, c.WebkitBoxDecorationBreak, inl.style.getPropertyValue('-webkit-box-decoration-break'), inl.style.cssText, cs(inl).paddingLeft].join('|')"),
            "\"1|20|10px|clone|clone|clone|clone|box-decoration-break: clone;|0px\""
        );
        // Roadmap 4a block 4: the position axes, gradients computed to
        // `rgb()` and pixels, the `-webkit-background-clip` alias and the
        // blend mode resolve.
        assert_eq!(
            run_js(&mut h, "a.style.backgroundPosition = 'right 10px center'; a.style.backgroundImage = 'linear-gradient(to right, red 1em, blue)'; \
                            a.style.webkitBackgroundClip = 'text'; a.style.backgroundBlendMode = 'multiply, screen'; var c = cs(a); \
                            [c.backgroundPositionX, c.backgroundPositionY, c.backgroundPosition, c.backgroundImage, c.backgroundClip, c.webkitBackgroundClip, c.backgroundBlendMode, a.style.backgroundPosition].join('|')"),
            "\"calc(100% - 10px)|50%|calc(100% - 10px) 50%|linear-gradient(to right, rgb(255, 0, 0) 20px, rgb(0, 0, 255))|text|text|multiply, screen|right 10px center\""
        );
        // Roadmap 4a (4s) block 1: the scroll container properties
        // through the CSSOM: the shorthands expand and collapse, the
        // resolved values (`scrollbar-color` inherited, `proximity` left
        // out of `scroll-snap-type`), the `style` attribute as written.
        assert_eq!(
            run_js(&mut h, "var s = document.createElement('div'); document.body.appendChild(s); s.innerHTML = '<i>x</i>'; var si = s.firstChild; \
                            s.style.scrollMargin = '10px 2px'; s.style.scrollPadding = 'auto 5%'; s.style.overscrollBehavior = 'contain'; s.style.scrollbarColor = 'red blue'; \
                            s.style.scrollbarGutter = 'both-edges stable'; s.style.scrollSnapType = 'x proximity'; s.style.scrollBehavior = 'smooth'; s.style.scrollbarWidth = 'thin'; var c = cs(s); \
                            [c.scrollMarginTop, c.scrollMarginRight, c.scrollMargin, c.scrollPaddingLeft, c.scrollPadding, c.overscrollBehaviorX, c.overscrollBehavior, c.scrollbarColor, cs(si).scrollbarColor, \
                             cs(si).scrollMarginTop, c.scrollbarGutter, c.scrollSnapType, s.style.scrollSnapType, c.scrollBehavior, c.scrollbarWidth, s.style.cssText].join('|')"),
            "\"10px|2px|10px 2px|5%|auto 5%|contain|contain|rgb(255, 0, 0) rgb(0, 0, 255)|rgb(255, 0, 0) rgb(0, 0, 255)|0px|stable both-edges|x|x proximity|smooth|thin|\
             scroll-margin: 10px 2px; scroll-padding: auto 5%; overscroll-behavior: contain; scrollbar-color: red blue; scrollbar-gutter: stable both-edges; scroll-snap-type: x proximity; scroll-behavior: smooth; scrollbar-width: thin;\""
        );
    }

    #[test]
    fn every_background_layer_image_is_fetched() {
        // Two `url()` layers, an `image-set()` (the 1x candidate for this
        // 1x device) and a `list-style-image`: four distinct data URLs of
        // the same 1×1 GIF, all decoded into the image store.
        const GIF: &str = "R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw";
        let html = format!(
            "<!DOCTYPE html><style>div {{ background-image: url(data:image/gif;base64,{GIF}==), url(\"data:image/gif;base64,R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAO%77==\"), \
             image-set(url(data:image/gif;base64,{GIF}) 1x, url(data:image/gif;base64,{GIF}%3D) 2x) }} \
             li {{ list-style-image: url(data:image/gif;base64,{GIF}%3D%3D) }}</style><div>x</div><ul><li>y</li></ul>"
        );
        let h = Harness::load(&html);
        let mut urls: Vec<String> = h.state.images.sizes().keys().map(|u| u.to_string()).collect();
        urls.sort();
        assert_eq!(h.state.images.len(), 4, "the 2x candidate is not fetched: {urls:?}");
    }

    #[test]
    fn paint_only_hover_repaints_without_a_layout() {
        // `a:hover` changes colours and the underline: the boxes stay,
        // the text fragment takes the new colour and the line, no layout
        // runs. `b:hover` changes padding: a layout runs.
        let html = "<!DOCTYPE html><style>body { margin: 0 } a { text-decoration: none } a:hover { background: red; color: white; text-decoration: underline } b:hover { padding: 10px }</style>\
             <p style='margin:0'><a href=x>link</a> and <b>bold</b> text</p>";
        let mut h = Harness::load(html);
        let layouts = h.state.layouts;
        let doc = h.state.doc.as_ref().expect("doc");
        let a = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "a"))
            .expect("a");
        let link_text = |h: &Harness| {
            let doc = h.state.doc.as_ref().expect("doc");
            let mut found = None;
            h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
                if let browser_layout::FragmentContent::Text(t) = &f.content
                    && doc.parent(f.node.expect("node")) == Some(a)
                    && found.is_none()
                {
                    let underline = t.decorations.iter().any(|d| d.line == browser_layout::DecorationLine::Underline);
                    found = Some((f.rect, t.color.to_rgba8(), underline, f.style.background_color.to_rgba8()));
                }
            });
            found.expect("the link's text fragment")
        };
        let (rect, color, underline, bg) = link_text(&h);
        assert_eq!((color, underline, bg), ([0, 0, 238, 255], false, [0, 0, 0, 0]));

        let (x, y) = h.center("a");
        h.send(ShellToTab::MouseMove { x, y });
        assert_eq!(h.state.layouts, layouts, "a paint-only hover does not lay out");
        let (rect2, color, underline, bg) = link_text(&h);
        assert_eq!(rect2, rect);
        assert_eq!((color, underline, bg), ([255, 255, 255, 255], true, [255, 0, 0, 255]), "painted from the new styles");
        assert_eq!(h.style("a").color.to_rgba8(), [255, 255, 255, 255]);

        let (x, y) = h.center("b");
        h.send(ShellToTab::MouseMove { x, y });
        assert_eq!(h.state.layouts, layouts + 1, "padding on hover lays out");
        let (_, color, underline, _) = link_text(&h);
        assert_eq!((color, underline), ([0, 0, 238, 255], false), "the link is plain again");
        assert_eq!(h.style("b").padding.left, browser_style::ComputedLp::Px(10.0));
    }

    /// A script's attribute changes restyle only what they reach: a colour
    /// class repaints without layout, a width class lays out again, a read
    /// in the same script sees the new values, a node insertion restyles
    /// the whole document.
    #[test]
    fn script_attribute_changes_restyle_what_they_reach() {
        let page = "<!DOCTYPE html><style>.red { color: red } .wide { width: 200px } .blue { color: blue }</style>\
            <body><button>b</button><span id=s>t</span><p id=p>x</p>\
            <script>let n = 0; const s = document.getElementById('s'), p = document.getElementById('p'); \
              document.querySelector('button').onclick = () => { n++; \
              if (n == 1) s.classList.add('red'); \
              if (n == 2) p.className = 'wide'; \
              if (n == 3) { s.setAttribute('class', 'blue'); console.log(getComputedStyle(s).color + ' ' + p.offsetWidth); } \
              if (n == 4) p.append(document.createElement('i')); }</script>";
        let mut h = Harness::load(page);
        h.pump_until(|h| h.state.layout.is_some() && h.state.lifecycle == Lifecycle::Complete);
        let node = |h: &Harness, id: &str| {
            let doc = h.state.doc.as_ref().expect("doc");
            doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id))).expect("element")
        };
        let (s, p) = (node(&h, "s"), node(&h, "p"));
        let layouts = h.state.layouts;
        h.click("button");
        h.pump();
        assert_eq!(h.state.styles[s].color.to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(h.state.layouts, layouts, "a colour change repaints only");
        h.click("button");
        h.pump();
        assert!(h.state.layouts > layouts, "a width change lays out again");
        let width = |h: &Harness| {
            let mut w = None;
            h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
                if f.node == Some(p) && w.is_none() {
                    w = Some(f.rect.width);
                }
            });
            w
        };
        assert_eq!(width(&h), Some(200.0));
        h.click("button");
        h.pump();
        assert_eq!(console_texts(&h).last().map(String::as_str), Some("rgb(0, 0, 255) 200"), "a read sees the change");
        assert_eq!(h.state.styles[s].color.to_rgba8(), [0, 0, 255, 255]);
        let layouts = h.state.layouts;
        h.click("button");
        h.pump();
        assert!(h.state.layouts > layouts, "an insertion lays out again");
        assert_eq!(width(&h), Some(200.0));
    }

    /// Part 3 of incremental layout B2: a full restyle (a stylesheet a
    /// script removes, a checkbox toggled, the viewport resized) lays out
    /// only the boxes whose styles (or controls) changed, and the result
    /// is a full layout's.
    #[test]
    fn full_restyles_lay_out_only_what_they_change() {
        let page = "<!DOCTYPE html><style>#box { color: blue } input:checked + span { padding-left: 30px }</style>\
            <style id=extra>#box { width: 300px }</style>\
            <style>@media (max-width: 500px) { #narrow { width: 50px } }</style>\
            <body><div><button>b</button><input type=checkbox id=c><span id=s>label</span>\
            <div id=box>x</div><p id=narrow>narrow text</p></div>\
            <script>document.querySelector('button').onclick = () => document.getElementById('extra').remove();</script>";
        let mut h = Harness::load(page);
        h.pump_until(|h| h.state.layout.is_some() && h.state.lifecycle == Lifecycle::Complete);
        let check = |h: &Harness, what: &str| {
            assert!(!browser_layout::last_layout_was_full(), "{what}: laid out incrementally");
            let doc = h.state.doc.as_ref().expect("doc");
            let full = browser_layout::LayoutEngine::new().layout(doc, &h.state.styles, h.state.viewport.width, h.state.viewport.height, &h.state.images);
            let rects = |t: &browser_layout::LayoutTree| {
                let mut v = Vec::new();
                t.root.walk(&mut |f| v.push((f.node, f.rect)));
                v
            };
            assert_eq!(rects(h.state.layout.as_ref().expect("layout")), rects(&full), "{what}: equal to a full layout");
        };
        let layouts = h.state.layouts;
        h.click("button");
        h.pump();
        assert!(h.state.layouts > layouts);
        check(&h, "a stylesheet removed");
        let layouts = h.state.layouts;
        h.click("input");
        h.pump();
        assert!(h.state.layouts > layouts);
        check(&h, "a checkbox toggled");
        h.send(ShellToTab::Resize(Viewport { width: 400.0, height: 600.0, scale_factor: 1.0 }));
        h.pump();
        check(&h, "the viewport resized");
    }

    /// A script's tree changes restyle what they reach: an appended item
    /// and its siblings, a replaced subtree inheriting from its parent,
    /// and a removed stylesheet (a full restyle).
    #[test]
    fn script_tree_changes_restyle_what_they_reach() {
        let page = "<!DOCTYPE html><style>li:first-child { color: red } li + li { color: green } #box { color: blue }</style>\
            <style id=extra>#box b { color: orange }</style>\
            <body><button>b</button><ul id=u><li id=a>a</li></ul><div id=box>x</div>\
            <script>let n = 0; document.querySelector('button').onclick = () => { n++; \
              if (n == 1) { const li = document.createElement('li'); li.id = 'b'; document.getElementById('u').append(li); } \
              if (n == 2) document.getElementById('box').innerHTML = '<i id=i>i</i><b id=bb>b</b>'; \
              if (n == 3) document.getElementById('extra').remove(); }</script>";
        let mut h = Harness::load(page);
        h.pump_until(|h| h.state.layout.is_some() && h.state.lifecycle == Lifecycle::Complete);
        let color = |h: &Harness, id: &str| {
            let doc = h.state.doc.as_ref().expect("doc");
            let n = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id))).expect(id);
            h.state.styles[n].color.to_rgba8()
        };
        h.click("button");
        h.pump();
        assert_eq!((color(&h, "a"), color(&h, "b")), ([255, 0, 0, 255], [0, 128, 0, 255]));
        h.click("button");
        h.pump();
        assert_eq!((color(&h, "i"), color(&h, "bb")), ([0, 0, 255, 255], [255, 165, 0, 255]), "inherited, and a rule");
        h.click("button");
        h.pump();
        assert_eq!(color(&h, "bb"), [0, 0, 255, 255], "the removed sheet's rule is gone");
    }

    /// A control disabled by its fieldset gets no `click`; one in the
    /// fieldset's first legend does.
    #[test]
    fn disabled_controls_get_no_click() {
        let page = "<!DOCTYPE html><body style='margin:0'><fieldset disabled style='margin:0;padding:0;border:0'>\
            <legend><button id=ok style='width:50px;height:20px'>a</button></legend>\
            <button id=no style='width:50px;height:20px'>b</button></fieldset>\
            <script>var log = []; for (const b of document.querySelectorAll('button')) b.onclick = () => log.push(b.id);</script>";
        let mut h = Harness::load(page);
        h.pump_until(|h| h.state.layout.is_some() && h.state.lifecycle == Lifecycle::Complete);
        let center = |h: &Harness, id: &str| {
            let doc = h.state.doc.as_ref().expect("doc");
            let node = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id))).expect(id);
            let mut c = None;
            h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
                if f.node == Some(node) && c.is_none() {
                    c = Some((f.rect.x + f.rect.width / 2.0, f.rect.y + f.rect.height / 2.0));
                }
            });
            c.expect("box")
        };
        for id in ["no", "ok"] {
            let (x, y) = center(&h, id);
            h.send(ShellToTab::MouseMove { x, y });
            h.send(ShellToTab::MouseDown { x, y, button: MouseButton::Left });
            h.send(ShellToTab::MouseUp { x, y, button: MouseButton::Left });
            h.pump();
        }
        assert_eq!(
            h.state.script.as_mut().and_then(|s| s.eval_to_string("log.join(' ')").ok()).as_deref(),
            Some("\"ok\"")
        );
    }

    /// An `<img>` a script inserts, or whose `src` it changes, is fetched
    /// and laid out once it arrives.
    #[test]
    fn script_set_image_sources_load() {
        let svg = |w: u32| format!("<svg xmlns='http://www.w3.org/2000/svg' width='{w}' height='10'><rect width='{w}' height='10'/></svg>");
        let (a, b) = (svg(20), svg(30));
        let page = "<!DOCTYPE html><body><button>b</button><img id=i src='/a.svg'>\
            <script>let n = 0; document.querySelector('button').onclick = () => { n++; \
              if (n == 1) document.getElementById('i').setAttribute('src', '/b.svg'); \
              if (n == 2) { const j = document.createElement('img'); j.id = 'j'; j.setAttribute('src', '/c.svg'); document.body.append(j); } }</script>";
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/a.svg", "image/svg+xml", &a),
                ("/b.svg", "image/svg+xml", &b),
                ("/c.svg", "image/svg+xml", &a),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete && h.state.images.get_svg(&server.url("/a.svg")).is_some());
        let width_of = |h: &Harness, id: &str| {
            let doc = h.state.doc.as_ref().expect("doc");
            let node = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))?;
            let mut w = None;
            h.state.layout.as_ref()?.root.walk(&mut |f| {
                if f.node == Some(node) && w.is_none() {
                    w = Some(f.rect.width);
                }
            });
            w
        };
        assert_eq!(width_of(&h, "i"), Some(20.0));
        h.click("button");
        let b_url = server.url("/b.svg");
        h.pump_until(|h| h.state.images.get_svg(&b_url).is_some());
        h.pump();
        assert_eq!(width_of(&h, "i"), Some(30.0), "the new source, laid out");
        h.click("button");
        let c_url = server.url("/c.svg");
        h.pump_until(|h| h.state.images.get_svg(&c_url).is_some());
        h.pump();
        assert_eq!(width_of(&h, "j"), Some(20.0), "an inserted image");
    }

    /// "Working sites first" 6m: `srcset` by density and by width with
    /// `sizes`, `<picture>` sources by `type` and `media` with their
    /// dimension attributes, only the chosen candidate fetched, its density
    /// sizing the box, `currentSrc`, `load`/`error` events, and selection
    /// again when the viewport and the device pixel ratio change (no fetch
    /// when only the density changes).
    #[test]
    fn responsive_images_select_load_and_react() {
        let svg = |w: u32| format!("<svg xmlns='http://www.w3.org/2000/svg' width='{w}' height='10'><rect width='{w}' height='10'/></svg>");
        let page = "<!DOCTYPE html><body style='margin:0'>\
            <img id=x src='/fallback.svg' srcset='/a1.svg 1x, /a2.svg 2x'>\
            <img id=w srcset='/w200.svg 200w, /w450.svg 450w, /w800.svg 800w' sizes='(max-width: 500px) 100vw, 300px'>\
            <picture><source srcset='/p.avif' type='image/avif'><source media='(max-width: 500px)' srcset='/narrow.svg'>\
              <source srcset='/wide.svg' width=50 height=20><img id=p src='/pfallback.svg'></picture>\
            <img id=e srcset=''>\
            <script>var log = []; document.querySelectorAll('img').forEach(i => { i.onload = () => log.push('load:' + i.id); i.onerror = () => log.push('error:' + i.id); });</script>";
        let a2 = "<svg xmlns='http://www.w3.org/2000/svg' width='80' height='20'><rect width='80' height='20'/></svg>".to_owned();
        let (a1, wide, narrow) = (svg(40), svg(70), svg(35));
        let (w200, w450, w800) = (svg(200), svg(450), svg(800));
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/fallback.svg", "image/svg+xml", &a1),
                ("/a1.svg", "image/svg+xml", &a1),
                ("/a2.svg", "image/svg+xml", &a2),
                ("/w200.svg", "image/svg+xml", &w200),
                ("/w450.svg", "image/svg+xml", &w450),
                ("/w800.svg", "image/svg+xml", &w800),
                ("/p.avif", "image/avif", "x"),
                ("/narrow.svg", "image/svg+xml", &narrow),
                ("/wide.svg", "image/svg+xml", &wide),
                ("/pfallback.svg", "image/svg+xml", &a1),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        h.pump();
        let size_of = |h: &Harness, id: &str| {
            let doc = h.state.doc.as_ref().expect("doc");
            let node = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))?;
            h.state.layout.as_ref()?.first_rect(|n| n == node).map(|r| (r.width, r.height))
        };
        let current = |h: &mut Harness| run_js(h, "['x', 'w', 'p', 'e'].map(id => document.getElementById(id).currentSrc.replace(location.origin, '')).join(' ')");
        assert_eq!(current(&mut h), "\"/a1.svg /w450.svg /wide.svg \"");
        // Only the chosen candidates were fetched.
        for path in ["/fallback.svg", "/a2.svg", "/w200.svg", "/w800.svg", "/p.avif", "/narrow.svg", "/pfallback.svg"] {
            assert!(server.requests(path).is_empty(), "{path} was fetched");
        }
        // Density-corrected sizes: 450w at 1.5x is 300px; the source's
        // width and height size the picture's image.
        assert_eq!(size_of(&h, "x"), Some((40.0, 10.0)));
        let (w, _) = size_of(&h, "w").expect("w");
        assert!((w - 300.0).abs() < 0.01, "{w}");
        assert_eq!(size_of(&h, "p"), Some((50.0, 20.0)));
        assert_eq!(run_js(&mut h, "log.sort().join(' ')"), "\"error:e load:p load:w load:x\"");
        assert_eq!(run_js(&mut h, "[document.getElementById('x') instanceof HTMLImageElement, 'currentSrc' in document.body].join()"), "\"true,false\"");

        // Narrower: the picture's media source wins (fetched, shown once
        // loaded, its own size: no dimension attributes); the 450w
        // candidate stays at another density, with no new fetch.
        run_js(&mut h, "log.length = 0");
        h.send(ShellToTab::Resize(Viewport { width: 400.0, height: 600.0, scale_factor: 1.0 }));
        h.pump_until(|h| h.state.images.get_svg(&server.url("/narrow.svg")).is_some());
        h.pump();
        assert_eq!(current(&mut h), "\"/a1.svg /w450.svg /narrow.svg \"");
        assert_eq!(size_of(&h, "p"), Some((35.0, 10.0)));
        let (w, _) = size_of(&h, "w").expect("w");
        assert!((w - 400.0).abs() < 0.01, "450w at 1.125x: {w}");
        assert_eq!(server.requests("/w450.svg").len(), 1, "no new fetch for another density");
        assert_eq!(run_js(&mut h, "log.sort().join(' ')"), "\"load:p load:w\"");

        // Twice the pixels: the 2x candidate, drawn at half its size.
        h.send(ShellToTab::Resize(Viewport { width: 400.0, height: 600.0, scale_factor: 2.0 }));
        // Both new candidates are fetched; each may arrive last.
        h.pump_until(|h| {
            h.state.images.get_svg(&server.url("/a2.svg")).is_some() && h.state.images.get_svg(&server.url("/w800.svg")).is_some()
        });
        h.pump();
        assert_eq!(current(&mut h), "\"/a2.svg /w800.svg /narrow.svg \"");
        assert_eq!(size_of(&h, "x"), Some((40.0, 10.0)));
        let (w, _) = size_of(&h, "w").expect("w");
        assert!((w - 400.0).abs() < 0.01, "800w at 2x: {w}");
    }

    /// `document.elementFromPoint`/`elementsFromPoint` ("Working sites
    /// first" line 7; CSSOM View §5, the WPT `elementsFromPoint` cases):
    /// viewport coordinates through the page scroll, topmost first,
    /// `pointer-events: none` passed over, text as its element once, a
    /// `::before` box as its element, the root appended, points outside
    /// the viewport empty, non-finite or missing arguments a TypeError.
    #[test]
    fn elements_from_point_list_the_boxes_under_a_point() {
        let page = "<!DOCTYPE html><body style='margin:0;height:3000px'>\
            <div id=purple style='position:absolute;left:0;top:0;width:50px;height:50px;background:purple'></div>\
            <div id=yellow style='position:absolute;left:0;top:60px;width:50px;height:50px'></div>\
            <div id=teal style='position:absolute;left:0;top:60px;width:50px;height:50px;pointer-events:none'></div>\
            <div id=c style='position:absolute;left:100px;top:0'><span id=s1><span id=s2>text here</span></span></div>\
            <div id=p style='position:absolute;left:200px;top:0;width:40px;height:40px'><b id=mid style='position:absolute;left:0;top:0;width:40px;height:40px;z-index:1'></b></div>\
            <style>#p::before { content: ''; position: absolute; left: 0; top: 0; width: 40px; height: 40px; z-index: 2 }</style>\
            <div id=low style='position:absolute;left:0;top:1000px;width:50px;height:50px'></div>";
        let mut h = Harness::load(page);
        let ids = |h: &mut Harness, x: &str, y: &str| {
            run_js(h, &format!("document.elementsFromPoint({x}, {y}).map(e => e.id || e.localName).join(' ')"))
        };
        assert_eq!(ids(&mut h, "10", "10"), "\"purple body html\"");
        assert_eq!(ids(&mut h, "10", "80"), "\"yellow body html\"", "pointer-events: none passed over");
        assert_eq!(ids(&mut h, "105", "8"), "\"s2 s1 c body html\"", "text and its inline box once");
        assert_eq!(ids(&mut h, "210", "10"), "\"p mid p body html\"", "the ::before box is its element's");
        assert_eq!(ids(&mut h, "700", "500"), "\"body html\"");
        assert_eq!(run_js(&mut h, "document.elementFromPoint(10, 80).id"), "\"yellow\"");
        // Outside the viewport: nothing.
        assert_eq!(ids(&mut h, "-1", "10"), "\"\"");
        assert_eq!(ids(&mut h, "10", "-1"), "\"\"");
        assert_eq!(ids(&mut h, "innerWidth + 100", "10"), "\"\"");
        assert_eq!(run_js(&mut h, "String(document.elementFromPoint(10, innerHeight + 100))"), "\"null\"");
        // Arguments: WebIDL doubles, both required.
        assert_eq!(
            run_js(&mut h, "var r = []; for (const f of [() => document.elementFromPoint(NaN, 0), () => document.elementsFromPoint(0, Infinity), () => document.elementFromPoint(1)]) { try { f(); r.push('ok') } catch (e) { r.push(e.name) } } r.join()"),
            "\"TypeError,TypeError,TypeError\""
        );
        // Through the page scroll: viewport coordinates.
        run_js(&mut h, "scrollTo(0, 990)");
        assert_eq!(ids(&mut h, "10", "20"), "\"low body html\"");
    }

    /// `sizes=auto` (HTML §4.8.4.3.11): a lazily loaded `<img>` that allows
    /// auto-sizes selects by its rendered width once laid out, and only
    /// that candidate is fetched (its fetch waits for layout and does not
    /// hold the load event); an eager one ignores `auto` (100vw); a width
    /// change selects again.
    #[test]
    fn auto_sizes_select_by_the_rendered_width() {
        let svg = |w: u32| format!("<svg xmlns='http://www.w3.org/2000/svg' width='{w}' height='10'><rect width='{w}' height='10'/></svg>");
        let page = "<!DOCTYPE html><body style='margin:0'>\
            <img id=lazy loading=lazy sizes=auto width=100 srcset='/a100.svg 100w, /a300.svg 300w'>\
            <img id=eager sizes=auto width=100 srcset='/b100.svg 100w, /b300.svg 300w'>";
        let (s100, s300) = (svg(100), svg(300));
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/a100.svg", "image/svg+xml", &s100),
                ("/a300.svg", "image/svg+xml", &s300),
                ("/b100.svg", "image/svg+xml", &s100),
                ("/b300.svg", "image/svg+xml", &s300),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.images.get_svg(&server.url("/a100.svg")).is_some() && h.state.lifecycle == Lifecycle::Complete);
        h.pump();
        let current = |h: &mut Harness| run_js(h, "['lazy', 'eager'].map(id => document.getElementById(id).currentSrc.replace(location.origin, '')).join(' ')");
        assert_eq!(current(&mut h), "\"/a100.svg /b300.svg\"");
        assert!(server.requests("/a300.svg").is_empty(), "the 100vw candidate is never fetched");
        assert!(server.requests("/b100.svg").is_empty());
        // A wider rendering selects again.
        run_js(&mut h, "document.getElementById('lazy').setAttribute('width', '300')");
        h.pump_until(|h| h.state.images.get_svg(&server.url("/a300.svg")).is_some());
        h.pump();
        assert_eq!(current(&mut h), "\"/a300.svg /b300.svg\"");
    }

    /// "Update the image data" from script: the first request becomes
    /// current in the microtask, so a promise callback queued after the
    /// change sees `currentSrc` while the synchronous read does not; a
    /// later change in the same task supersedes it (one fetch); a failed
    /// fetch fires `error`; an available image loads at once; a removal
    /// from a `<picture>` selects again.
    #[test]
    fn script_image_updates_follow_the_microtask() {
        let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='20' height='10'><rect width='20' height='10'/></svg>";
        let page = "<!DOCTYPE html><body><picture id=pic><source id=s srcset='/s.svg'><img id=pi src='/f.svg'></picture>";
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/one.svg", "image/svg+xml", svg),
                ("/two.svg", "image/svg+xml", svg),
                ("/s.svg", "image/svg+xml", svg),
                ("/f.svg", "image/svg+xml", svg),
                ("/bad.svg", "image/svg+xml", "not an image"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        assert_eq!(
            run_js(&mut h, "var log = []; var i = document.createElement('img'); i.onload = () => log.push('load ' + i.currentSrc.replace(location.origin, '')); \
                            i.onerror = () => log.push('error'); i.setAttribute('srcset', '/one.svg'); i.setAttribute('srcset', '/two.svg'); \
                            var sync = i.currentSrc; Promise.resolve().then(() => log.push('micro ' + i.currentSrc.replace(location.origin, ''))); 'sync:' + sync"),
            "\"sync:\""
        );
        h.pump_until(|h| h.state.images.get_svg(&server.url("/two.svg")).is_some());
        h.pump();
        assert_eq!(run_js(&mut h, "log.join('|')"), "\"micro /two.svg|load /two.svg\"");
        assert!(server.requests("/one.svg").is_empty(), "superseded in the same task");
        // An undecodable image: `error`.
        run_js(&mut h, "log.length = 0; i.setAttribute('src', '/bad.svg'); i.removeAttribute('srcset')");
        h.pump_until(|h| h.state.failed_images.contains(&server.url("/bad.svg")));
        h.pump();
        assert_eq!(run_js(&mut h, "log.join('|')"), "\"error\"");
        // An available image without srcset: shown at once, `load` as a task.
        assert_eq!(
            run_js(&mut h, "log.length = 0; i.setAttribute('src', '/two.svg'); log.push('after set'); 'ok'"),
            "\"ok\""
        );
        assert_eq!(run_js(&mut h, "log.join('|')"), "\"after set|load /two.svg\"");
        // The picture's source removed: its image selects its own `src`.
        assert_eq!(run_js(&mut h, "document.getElementById('pi').currentSrc.replace(location.origin, '')"), "\"/s.svg\"");
        run_js(&mut h, "document.getElementById('s').remove()");
        h.pump_until(|h| h.state.images.get_svg(&server.url("/f.svg")).is_some());
        h.pump();
        assert_eq!(run_js(&mut h, "document.getElementById('pi').currentSrc.replace(location.origin, '')"), "\"/f.svg\"");
    }

    // ----- "Working sites first" line 7 -----

    /// Pump the tab until `condition` (a script expression) is true.
    fn wait_js(h: &mut Harness, condition: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            h.pump_for(std::time::Duration::from_millis(15));
            if js(h, condition) == "true" {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting for `{condition}`; console: {:?}", console_texts(h));
        }
    }

    /// A string a script expression evaluates to, unquoted.
    fn js_text(h: &mut Harness, src: &str) -> String {
        js(h, src).trim_matches('"').to_owned()
    }

    fn route(path: &str, headers: &[(&str, &str)], body: &str) -> (String, Vec<(String, String)>, Vec<u8>) {
        (
            path.to_owned(),
            headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            body.as_bytes().to_vec(),
        )
    }

    fn request_header(head: &str, name: &str) -> Option<String> {
        head.lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_owned()))
    }

    /// `fetch` and `XMLHttpRequest` over real sockets: same-origin
    /// responses in full, cross-origin ones by CORS (the check, exposed
    /// headers, credentials, preflights and their cache), the request
    /// modes, redirects in each mode, opaque responses, `data:` URLs,
    /// integrity, cookies only where the credentials mode sends them, and
    /// a synchronous request.
    #[test]
    fn script_requests_follow_the_fetch_algorithm() {
        let a = Server::start_with(
            vec![
                route("/page", &[("Content-Type", "text/html")], "<!DOCTYPE html><title>Fetch</title><p>x</p>"),
                route("/same.json", &[("Content-Type", "application/json"), ("X-Secret", "s"), ("Set-Cookie", "sc=1; Path=/")], "{\"a\":1}"),
                route("/redir", &[(":status", "302 Found"), ("Location", "/same.json")], ""),
                route("/post-redir", &[(":status", "303 See Other"), ("Location", "/same.json")], ""),
                route("/hello", &[("Content-Type", "text/plain")], "hello"),
            ],
            &[],
        );
        let origin = a.base.origin().ascii_serialization();
        let b = Server::start_with(
            vec![
                route(
                    "/cors.txt",
                    &[("Content-Type", "text/plain"), ("Access-Control-Allow-Origin", "*"), ("X-Hidden", "h"), ("X-Shown", "s"), ("Access-Control-Expose-Headers", "X-Shown")],
                    "cross",
                ),
                route("/nocors.txt", &[("Content-Type", "text/plain")], "secret"),
                route("/cred.txt", &[("Access-Control-Allow-Origin", &origin), ("Access-Control-Allow-Credentials", "true")], "with credentials"),
                route(
                    "/pre",
                    &[("Access-Control-Allow-Origin", &origin), ("Access-Control-Allow-Methods", "PUT"), ("Access-Control-Allow-Headers", "X-Custom, Content-Type"), ("Access-Control-Max-Age", "60")],
                    "preflighted",
                ),
                route("/pre-denied", &[("Access-Control-Allow-Origin", "*")], "never"),
                route("/corp", &[("Cross-Origin-Resource-Policy", "same-origin")], "kept"),
            ],
            &[],
        );
        // A redirect from the document's origin to the other one.
        let hop = Server::start_with(
            vec![route("/to-b", &[(":status", "307 Temporary Redirect"), ("Location", b.url("/cors.txt").as_str()), ("Access-Control-Allow-Origin", "*")], "")],
            &[],
        );
        let mut h = Harness::load_url(a.url("/page"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        let script = format!(
            "var B = '{b}', HOP = '{hop}'; var out = {{}}; var done = 0; var rec = k => v => {{ out[k] = String(v); done++; }}; var err = e => 'rejected ' + e.name; \
             fetch('/same.json').then(r => r.json().then(j => [r.status, r.statusText, r.ok, r.type, r.headers.get('x-secret'), String(r.headers.get('set-cookie')), j.a, r.url.endsWith('/same.json'), r.redirected].join(' '))).then(rec('same'), e => rec('same')(err(e))); \
             fetch('/redir').then(r => r.text().then(t => [r.redirected, r.url.endsWith('/same.json'), t].join(' '))).then(rec('redir'), e => rec('redir')(err(e))); \
             fetch('/redir', {{redirect: 'error'}}).then(r => 'resolved', err).then(rec('redirect-error')); \
             fetch('/redir', {{redirect: 'manual'}}).then(r => [r.type, r.status, r.url.endsWith('/redir'), r.headers.has('location')].join(' '), err).then(rec('redirect-manual')); \
             fetch('/post-redir', {{method: 'POST', body: 'x'}}).then(r => r.text(), err).then(rec('post-redirect')); \
             fetch(B + 'cors.txt').then(r => r.text().then(t => [r.type, r.status, t, String(r.headers.get('x-hidden')), r.headers.get('x-shown'), r.headers.get('content-type')].join(' '))).then(rec('cors'), e => rec('cors')(err(e))); \
             fetch(B + 'nocors.txt').then(r => 'resolved', err).then(rec('blocked')); \
             fetch(B + 'nocors.txt', {{mode: 'no-cors'}}).then(r => r.text().then(t => [r.type, r.status, t.length, [...r.headers].length].join(' ')), err).then(rec('opaque')); \
             fetch(B + 'corp', {{mode: 'no-cors'}}).then(r => 'resolved', err).then(rec('corp')); \
             fetch(B + 'cors.txt', {{mode: 'same-origin'}}).then(r => 'resolved', err).then(rec('same-origin-mode')); \
             fetch(HOP + 'to-b').then(r => r.text().then(t => [r.type, r.redirected, t, r.url === B + 'cors.txt'].join(' ')), err).then(rec('cross-redirect')); \
             fetch(B + 'pre', {{method: 'PUT', headers: {{'X-Custom': '1', 'Content-Type': 'application/json'}}, body: '{{}}'}}).then(r => r.text(), err).then(rec('preflight')) \
                 .then(() => fetch(B + 'pre', {{method: 'PUT', headers: {{'X-Custom': '2'}}}})).then(r => r.text(), err).then(rec('preflight-cached')); \
             fetch(B + 'pre-denied', {{method: 'DELETE'}}).then(r => 'resolved', err).then(rec('preflight-denied')); \
             fetch(B + 'cred.txt', {{credentials: 'include'}}).then(r => r.text(), err).then(rec('credentials')); \
             fetch(B + 'cors.txt', {{credentials: 'include'}}).then(r => 'resolved', err).then(rec('credentials-star')); \
             fetch('data:text/plain,hi%20there').then(r => r.text().then(t => r.type + ' ' + r.status + ' ' + t), err).then(rec('data')); \
             fetch('/hello', {{integrity: 'sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ='}}).then(r => r.text(), err).then(rec('integrity')); \
             fetch('/hello', {{integrity: 'sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='}}).then(r => 'resolved', err).then(rec('integrity-bad')); \
             fetch('ftp://example.test/x').then(r => 'resolved', err).then(rec('scheme')); \
             var x = new XMLHttpRequest(); x.open('GET', B + 'cors.txt'); x.onload = () => rec('xhr')([x.status, x.statusText, x.responseText, String(x.getResponseHeader('x-hidden')), x.getResponseHeader('x-shown')].join(' ')); x.onerror = () => rec('xhr')('error'); x.send(); \
             var xe = new XMLHttpRequest(); xe.open('GET', B + 'nocors.txt'); xe.onerror = () => rec('xhr-blocked')(xe.status + ' error ' + xe.readyState); xe.onload = () => rec('xhr-blocked')('loaded'); xe.send(); \
             var xu = new XMLHttpRequest(); xu.open('POST', B + 'pre-denied'); xu.upload.onprogress = () => {{}}; xu.onerror = () => rec('xhr-upload-preflight')('error'); xu.onload = () => rec('xhr-upload-preflight')('loaded'); xu.send('body'); \
             var xs = new XMLHttpRequest(); xs.open('GET', '/same.json', false); xs.send(); out.sync = [xs.status, xs.responseText, xs.getResponseHeader('content-type')].join(' '); \
             var xt = ''; var xb = new XMLHttpRequest(); xb.open('GET', B + 'nocors.txt', false); try {{ xb.send() }} catch (e) {{ xt = e.name }} out['sync-blocked'] = xt; 'started'",
            b = b.base,
            hop = hop.base,
        );
        assert_eq!(run_js(&mut h, &script), "\"started\"");
        wait_js(&mut h, "done >= 23");
        let got = |h: &mut Harness, key: &str| js_text(h, &format!("out['{key}']"));
        assert_eq!(got(&mut h, "same"), "200 OK true basic s null 1 true false", "Set-Cookie is never shown");
        assert_eq!(got(&mut h, "redir"), "true true {\\\"a\\\":1}");
        assert_eq!(got(&mut h, "redirect-error"), "rejected TypeError");
        assert_eq!(got(&mut h, "redirect-manual"), "opaqueredirect 0 true false");
        assert_eq!(got(&mut h, "post-redirect"), "{\\\"a\\\":1}");
        assert_eq!(got(&mut h, "cors"), "cors 200 cross null s text/plain", "only safelisted and exposed headers show");
        assert_eq!(got(&mut h, "blocked"), "rejected TypeError");
        assert_eq!(got(&mut h, "opaque"), "opaque 0 0 0");
        assert_eq!(got(&mut h, "corp"), "rejected TypeError");
        assert_eq!(got(&mut h, "same-origin-mode"), "rejected TypeError");
        assert_eq!(got(&mut h, "cross-redirect"), "cors true cross true");
        assert_eq!(got(&mut h, "preflight"), "preflighted");
        assert_eq!(got(&mut h, "preflight-cached"), "preflighted");
        assert_eq!(got(&mut h, "preflight-denied"), "rejected TypeError");
        assert_eq!(got(&mut h, "credentials"), "with credentials");
        assert_eq!(got(&mut h, "credentials-star"), "rejected TypeError", "a wildcard does not allow credentials");
        assert_eq!(got(&mut h, "data"), "basic 200 hi there");
        assert_eq!(got(&mut h, "integrity"), "hello");
        assert_eq!(got(&mut h, "integrity-bad"), "rejected TypeError");
        assert_eq!(got(&mut h, "scheme"), "rejected TypeError");
        assert_eq!(got(&mut h, "xhr"), "200 OK cross null s");
        assert_eq!(got(&mut h, "xhr-blocked"), "0 error 4");
        assert_eq!(got(&mut h, "xhr-upload-preflight"), "loaded", "upload listeners force a preflight, which a simple POST passes");
        assert_eq!(got(&mut h, "sync"), "200 {\\\"a\\\":1} application/json");
        assert_eq!(got(&mut h, "sync-blocked"), "NetworkError");

        // What went over the wire.
        let method = |head: &str| head.split_whitespace().next().unwrap_or("").to_owned();
        let pre = b.requests("/pre");
        assert_eq!(pre.iter().map(|r| method(r)).collect::<Vec<_>>(), vec!["OPTIONS", "PUT", "PUT"], "one preflight; the second request is covered by its cached result");
        assert_eq!(request_header(&pre[0], "access-control-request-method").as_deref(), Some("PUT"));
        assert_eq!(request_header(&pre[0], "access-control-request-headers").as_deref(), Some("content-type,x-custom"));
        assert_eq!(request_header(&pre[0], "origin").as_deref(), Some(origin.as_str()));
        assert_eq!(request_header(&pre[0], "cookie"), None, "a preflight carries no credentials");
        assert_eq!(request_header(&pre[1], "x-custom").as_deref(), Some("1"));
        assert_eq!(request_header(&pre[1], "content-type").as_deref(), Some("application/json"));
        let denied = b.requests("/pre-denied");
        let mut methods: Vec<String> = denied.iter().map(|r| method(r)).collect();
        methods.sort();
        assert_eq!(methods, vec!["OPTIONS", "OPTIONS", "POST"], "the DELETE's preflight (refused: no DELETE follows), the upload's forced one and its POST");
        let cors = b.requests("/cors.txt");
        assert!(cors.iter().all(|r| request_header(r, "origin").is_some()), "every CORS request names its origin");
        assert_eq!(cors.iter().filter(|r| request_header(r, "cookie").is_some()).count(), 1, "cookies only with credentials: 'include'");
        assert_eq!(request_header(&b.requests("/cred.txt")[0], "cookie").as_deref(), Some("sc=1"));
        assert_eq!(request_header(&b.requests("/nocors.txt")[0], "cookie"), None);
        let same = a.requests("/same.json");
        assert!(same.iter().skip(1).all(|r| request_header(r, "cookie").as_deref() == Some("sc=1")), "same-origin requests carry the cookie once it is set");
        assert!(same.iter().any(|r| method(r) == "GET" && request_header(r, "accept").as_deref() == Some("*/*")));
        let posted = a.requests("/post-redir");
        assert_eq!((method(&posted[0]), request_header(&posted[0], "origin")), ("POST".to_owned(), Some(origin.clone())), "a same-origin POST names its origin too");
        assert_eq!(request_header(&posted[0], "content-type").as_deref(), Some("text/plain;charset=UTF-8"));
        assert!(same.iter().all(|r| method(r) == "GET"), "a 303 turns the POST into a GET");
    }

    /// `XMLHttpRequest.timeout`: the request fails with `timeout` when the
    /// server has not answered in time; a late answer changes nothing.
    #[test]
    fn xml_http_request_times_out() {
        let server = Server::start(&[("/page", "text/html", "<!DOCTYPE html><title>T</title>"), ("/slow", "text/plain", "late")], &["/slow"]);
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        run_js(&mut h, "var log = []; var x = new XMLHttpRequest(); x.timeout = 60; x.open('GET', '/slow'); ['timeout', 'error', 'load', 'loadend'].forEach(k => x.addEventListener(k, () => log.push(k + x.readyState + x.status))); x.send();");
        assert!(h.state.next_wake().is_some(), "the tab wakes for the timeout");
        wait_js(&mut h, "log.length >= 2");
        assert_eq!(js_text(&mut h, "log.join(' ')"), "timeout40 loadend40");
        server.release("/slow");
        h.pump_for(std::time::Duration::from_millis(100));
        assert_eq!(js_text(&mut h, "log.join(' ') + ' ' + JSON.stringify(x.responseText) + ' end'"), "timeout40 loadend40 \\\"\\\" end");
    }

    /// Storage through the tab: per origin, the local area the browser's
    /// (another tab sees it and hears of changes), the session area the
    /// tab's, both kept across navigations; none for a `data:` document.
    #[test]
    fn storage_is_by_origin_and_tells_other_tabs() {
        let server = Server::start(
            &[
                ("/one", "text/html", "<!DOCTYPE html><title>One</title><script>localStorage.setItem('k', 'v'); sessionStorage.s = '1';</script>"),
                ("/two", "text/html", "<!DOCTYPE html><title>Two</title><script>var ev = []; onstorage = e => ev.push([e.key, e.oldValue, e.newValue, e.url, e.storageArea === localStorage].join(' '));</script>"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/one"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        h.send(ShellToTab::Navigate { url: server.url("/two") });
        h.pump_until(|h| h.title().as_deref() == Some("Two") && h.state.lifecycle == Lifecycle::Complete);
        assert_eq!(js_text(&mut h, "[localStorage.k, sessionStorage.s, localStorage.length].join(' ')"), "v 1 1", "both areas outlive the document");
        // Another tab of the browser: the same local storage, its own
        // session storage; its changes arrive here as events.
        let stores = TabStores {
            local: h.state.stores.local.clone(),
            session: Default::default(),
        };
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        stores.local.subscribe(TabId(1), Arc::new(move |change| sink.lock().expect("lock").push(change)));
        let mut other = Harness::start_in(server.url("/two"), TabId(2), stores);
        other.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        assert_eq!(js_text(&mut other, "[localStorage.k, String(sessionStorage.s)].join(' ')"), "v undefined");
        run_js(&mut other, "localStorage.k = 'w'; localStorage.setItem('k', 'w'); localStorage.n = '1'; delete localStorage.n; sessionStorage.only = 'here'; localStorage.clear();");
        let changes: Vec<StorageChange> = std::mem::take(&mut *heard.lock().expect("lock"));
        assert_eq!(changes.len(), 4, "a set, a new key, its removal, the clear; not the unchanged set, not the session area");
        for change in changes {
            h.state.handle_storage(change);
        }
        h.pump();
        let two = server.url("/two");
        assert_eq!(
            js_text(&mut h, "ev.join('|')"),
            format!("k v w {two} true|n  1 {two} true|n 1  {two} true|   {two} true")
        );
        assert_eq!(js_text(&mut h, "localStorage.length + ' ' + sessionStorage.s"), "0 1");
        assert_eq!(js_text(&mut other, "ev.length"), "0", "the tab that made the changes hears nothing");
        // A change in another origin is not this document's business.
        h.state.handle_storage(StorageChange {
            origin: "https://elsewhere.test".to_owned(),
            key: None,
            old: None,
            new: None,
            url: String::new(),
        });
        h.pump();
        assert_eq!(js_text(&mut h, "ev.length"), "4");
        // An opaque origin has no storage.
        let mut data = Harness::load("<!DOCTYPE html><title>D</title>");
        assert_eq!(js_text(&mut data, "var n = ''; try { localStorage } catch (e) { n = e.name } n"), "SecurityError");
    }

    /// `history.pushState`, `replaceState` and traversals: the entries
    /// are the tab's (the shell's back button walks them), a traversal
    /// between a document's own entries loads nothing and fires
    /// `popstate` (then `hashchange` when the fragment differs), the
    /// state survives a reload and a trip to another document.
    #[test]
    fn push_state_entries_traverse_without_loading() {
        let server = Server::start(
            &[
                ("/app", "text/html", "<!DOCTYPE html><title>App</title><body style='margin:0'><div style='height:3000px'></div><a id=frag href='#here'>frag</a><p id=here>here</p>\
                  <script>var ev = []; onpopstate = e => ev.push('pop:' + JSON.stringify(e.state) + ':' + location.pathname + location.hash + ':' + (e.state === history.state)); onhashchange = e => ev.push('hash:' + URL0(e.oldURL) + '>' + URL0(e.newURL)); function URL0(u) { return u.slice(u.indexOf('/', 8)); }</script>"),
                ("/other", "text/html", "<!DOCTYPE html><title>Other</title>"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/app"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        let loads = |s: &Server| s.requests("/app").len() + s.requests("/app/one").len() + s.requests("/app/two").len();
        assert_eq!(run_js(&mut h, "history.pushState({n: 1}, '', '/app/one'); history.pushState({n: 2}, '', '/app/two?q#h'); history.replaceState({n: 22}, '', '/app/two'); [history.length, history.state.n, location.pathname, ev.length].join(' ')"), "\"3 22 /app/two 0\"");
        assert_eq!(h.state.history.len(), 3);
        assert_eq!(h.state.history.iter().map(|e| e.url.path().to_owned()).collect::<Vec<_>>(), vec!["/app", "/app/one", "/app/two"]);
        assert_eq!(h.last_state().map(|(url, _, back, forward)| (url.path().to_owned(), back, forward)), Some(("/app/two".to_owned(), true, false)));
        // The shell's back button, then the script's own traversals.
        h.send(ShellToTab::GoBack);
        assert_eq!(js_text(&mut h, "[ev.join(' '), history.state.n, location.pathname, history.length].join(' ')"), "pop:{\\\"n\\\":1}:/app/one:true 1 /app/one 3");
        run_js(&mut h, "ev = []; history.back();");
        assert_eq!(js_text(&mut h, "[ev.join(' '), String(history.state), location.pathname].join(' ')"), "pop:null:/app:true null /app");
        run_js(&mut h, "ev = []; history.go(2);");
        assert_eq!(js_text(&mut h, "[ev.join(' '), history.state.n].join(' ')"), "pop:{\\\"n\\\":22}:/app/two:true 22");
        run_js(&mut h, "ev = []; history.go(5); history.forward();");
        assert_eq!(js_text(&mut h, "ev.length + location.pathname"), "0/app/two", "nowhere to go: nothing happens");
        assert_eq!(loads(&server), 1, "none of it loaded anything");
        // A fragment navigation is an entry of the document too: popstate
        // with no state, then hashchange.
        run_js(&mut h, "ev = []; location.hash = 'here';");
        assert_eq!(js_text(&mut h, "[ev.join(' '), String(history.state), history.length].join(' ')"), "pop:null:/app/two#here:true hash:/app/two>/app/two#here null 4");
        assert!(h.state.scroll_y > 0.0, "and it scrolled to the fragment");
        // Back: the entry's state and scroll position return.
        let at_fragment = h.state.scroll_y;
        h.send(ShellToTab::GoBack);
        assert_eq!(js_text(&mut h, "ev.slice(2).join(' ')"), "pop:{\\\"n\\\":22}:/app/two:true hash:/app/two#here>/app/two");
        assert_eq!(h.state.scroll_y, 0.0, "the position the entry was left at");
        h.send(ShellToTab::GoForward);
        assert_eq!(h.state.scroll_y, at_fragment);
        // `scrollRestoration = 'manual'`: the page keeps its position.
        run_js(&mut h, "history.scrollRestoration = 'manual'; history.pushState(null, '', '/app/three'); scrollTo(0, 40);");
        h.send(ShellToTab::GoBack);
        assert_eq!(h.state.scroll_y, 40.0);
        assert_eq!(loads(&server) + server.requests("/app/three").len(), 1);
        // Another document, and back: the entry is loaded again (at the
        // URL it was pushed with) and has its state.
        h.send(ShellToTab::GoBack);
        assert_eq!(js_text(&mut h, "location.pathname + history.state.n"), "/app/two22");
        h.send(ShellToTab::Navigate { url: server.url("/other") });
        h.pump_until(|h| h.title().as_deref() == Some("Other") && h.state.lifecycle == Lifecycle::Complete);
        assert_eq!(js_text(&mut h, "String(history.state) + history.length"), "null4", "the entries after the current one went");
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.state.nav.is_none() && h.state.lifecycle == Lifecycle::Complete);
        assert_eq!(server.requests("/app/two").len(), 1, "the pushed URL is what loads");
        // The server knows no such path: whatever came, the entry's state did too.
        assert_eq!(js_text(&mut h, "location.pathname + ' ' + JSON.stringify(history.state) + ' ' + history.length"), "/app/two {\\\"n\\\":22} 4");
        // A reload keeps it as well.
        h.send(ShellToTab::Reload);
        h.pump_until(|h| h.state.nav.is_none() && h.state.lifecycle == Lifecycle::Complete);
        assert_eq!(js_text(&mut h, "JSON.stringify(history.state)"), "{\\\"n\\\":22}");
    }

    /// `IntersectionObserver` through the tab: an entry for every target
    /// when it is first observed, then when it crosses a threshold as the
    /// page scrolls or a scroller between it and the root does; root
    /// margins, an element root, a target that is not rendered.
    #[test]
    fn intersection_observers_follow_scrolling_and_layout() {
        let html = "<!DOCTYPE html><title>IO</title><style>body { margin: 0 } #far { margin-top: 1000px; width: 200px; height: 100px } \
             #box { width: 300px; height: 200px; overflow: auto; margin-top: 500px } #inner { margin-top: 400px; width: 50px; height: 50px } #none { display: none } #tail { height: 2000px }</style>\
             <div id=far></div><div id=box><div id=inner></div></div><div id=none></div><div id=tail></div>\
             <script>var log = []; var show = e => [e.target.id, e.isIntersecting, e.intersectionRatio.toFixed(2), Math.round(e.boundingClientRect.top), Math.round(e.intersectionRect.height), e.rootBounds && Math.round(e.rootBounds.height), e instanceof IntersectionObserverEntry, e.time >= 0].join(',');\
             var io = new IntersectionObserver(function (list, o) { log.push(list.map(show).join(' ') + (o === io && this === io ? '' : '!')); }, {threshold: [0, 0.5, 1]});\
             var far = document.getElementById('far'), inner = document.getElementById('inner'), box = document.getElementById('box');\
             io.observe(far); io.observe(document.getElementById('none')); io.observe(far);</script>";
        let mut h = Harness::load(html);
        assert_eq!(js_text(&mut h, "[String(io.root), io.rootMargin, io.scrollMargin, io.thresholds.join('/'), Object.isFrozen(io.thresholds)].join(' ')"), "null 0px 0px 0px 0px 0px 0px 0px 0px 0/0.5/1 true");
        // The first entries: both targets, neither intersecting.
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "far,false,0.00,1000,0,600,true,true none,false,0.00,0,0,600,true,true");
        // Scrolled so half of it shows, then all of it, then past it.
        run_js(&mut h, "log = []; scrollTo(0, 450);");
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "far,true,0.50,550,50,600,true,true");
        run_js(&mut h, "log = []; scrollTo(0, 500);");
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "far,true,1.00,500,100,600,true,true");
        run_js(&mut h, "log = []; scrollTo(0, 520);");
        assert_eq!(js_text(&mut h, "log.length"), "0", "no threshold crossed: no entry");
        run_js(&mut h, "log = []; scrollTo(0, 1200);");
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "far,false,0.00,-200,0,600,true,true");
        // A layout change moves it back in; takeRecords, unobserve, disconnect.
        run_js(&mut h, "log = []; far.style.marginTop = '1300px';");
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "far,true,1.00,100,100,600,true,true");
        assert_eq!(run_js(&mut h, "log = []; io.unobserve(far); far.style.marginTop = '5000px'; io.takeRecords().length"), "0");
        assert_eq!(js_text(&mut h, "log.length"), "0");
        // A root margin grows the root; an element root clips to its own box, and a scroller in between clips too.
        run_js(&mut h, "scrollTo(0, 0); far.style.marginTop = '700px'; log = []; \
             var wide = new IntersectionObserver(l => log.push('wide ' + l.map(show).join(' ')), {rootMargin: '0px 0px 150px 0px'}); wide.observe(far); \
             var inbox = new IntersectionObserver(l => log.push('inbox ' + l.map(show).join(' ')), {root: box}); inbox.observe(inner); inbox.observe(far); \
             var page = new IntersectionObserver(l => log.push('page ' + l.map(show).join(' '))); page.observe(inner);");
        assert_eq!(
            js_text(&mut h, "log.join(' | ')"),
            "wide far,true,0.50,700,50,750,true,true | inbox inner,false,0.00,1700,0,200,true,true far,false,0.00,0,0,200,true,true | page inner,false,0.00,1700,0,600,true,true"
        );
        // The box scrolls its content into its own view, still below the page's: only the observer rooted at the box sees it.
        run_js(&mut h, "log = []; box.scrollTop = 300;");
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "inbox inner,true,1.00,1450,50,200,true,true");
        // The page scrolls the box into view: now the page-rooted one does (through the box's clip).
        run_js(&mut h, "log = []; scrollTo(0, 1000);");
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "wide far,false,0.00,-300,0,750,true,true | page inner,true,1.00,450,50,600,true,true");
        run_js(&mut h, "log = []; box.scrollTop = 0;");
        assert_eq!(js_text(&mut h, "log.join(' | ')"), "inbox inner,false,0.00,700,0,200,true,true | page inner,false,0.00,700,0,600,true,true");
        // Bad arguments.
        assert_eq!(
            js_text(&mut h, "var errs = []; for (var f of [() => new IntersectionObserver(1), () => new IntersectionObserver(() => {}, {threshold: 2}), () => new IntersectionObserver(() => {}, {threshold: [0.5, NaN]}), () => new IntersectionObserver(() => {}, {rootMargin: '1em'}), \
                          () => new IntersectionObserver(() => {}, {rootMargin: '1px 2px 3px 4px 5px'}), () => new IntersectionObserver(() => {}, {root: document.createTextNode('x')}), () => io.observe(document), () => io.observe(null)]) \
                          { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } } errs.join(',') + ' ' + new IntersectionObserver(() => {}, {rootMargin: '10px 5%', root: document}).rootMargin"),
            "TypeError,RangeError,TypeError,SyntaxError,SyntaxError,TypeError,TypeError,TypeError 10px 5% 10px 5%"
        );
        assert_eq!(console_texts(&h), Vec::<String>::new());
    }

    /// A `MutationObserver` made while the page parses hears of what the
    /// parser adds after it, at the next microtask checkpoint.
    #[test]
    fn mutation_observers_see_the_parser_at_work() {
        let html = "<!DOCTYPE html><html><head><title>MO</title>\
             <script>var seen = []; new MutationObserver(list => { for (var r of list) for (var n of r.addedNodes) seen.push((r.target.nodeName + '>' + n.nodeName).toLowerCase()); }).observe(document, {childList: true, subtree: true});</script>\
             </head><body><div id=a><b>x</b></div><script>var first = seen.slice();</script><p>late</p></body></html>";
        let mut h = Harness::load(html);
        assert_eq!(js_text(&mut h, "first.join(' ')"), "html>body body>div div>b b>#text body>script script>#text", "what was parsed before the second script ran, in order");
        assert_eq!(js_text(&mut h, "seen.slice(first.length).join(' ')"), "body>p p>#text");
        run_js(&mut h, "seen = []; document.title = 'Changed'; document.body.append(document.createElement('hr'));");
        assert_eq!(js_text(&mut h, "seen.join(' ')"), "title>#text body>hr");
        assert_eq!(h.title().as_deref(), Some("Changed"));
    }

    /// An image arriving lays the page out again only when a box takes its
    /// size from it: a background repaints, an `<img>` relayouts.
    #[test]
    fn only_images_that_size_boxes_relayout() {
        let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='20' height='20'><rect width='20' height='20'/></svg>";
        // The background in a `style` attribute, its URL relative.
        let page = "<!DOCTYPE html><body><div style='width:10px;height:10px;background:url(/bg.svg)'></div><img src='/i.svg'>";
        let server = Server::start(
            &[
                ("/page", "text/html; charset=utf-8", page),
                ("/bg.svg", "image/svg+xml", svg),
                ("/i.svg", "image/svg+xml", svg),
            ],
            &["/bg.svg", "/i.svg"],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.layout.is_some() && h.state.lifecycle == Lifecycle::ContentLoaded);
        let before = h.state.layouts;
        let bg = server.url("/bg.svg");
        server.release("/bg.svg");
        h.pump_until(|h| h.state.images.get_svg(&bg).is_some());
        h.pump();
        assert_eq!(h.state.layouts, before, "a background image only repaints");
        let img = server.url("/i.svg");
        server.release("/i.svg");
        h.pump_until(|h| h.state.images.get_svg(&img).is_some());
        h.pump();
        assert!(h.state.layouts > before, "an <img> takes its size from the image");
    }

    /// Block B3 of incremental layout: a web font and images arriving lay
    /// out again only the boxes they reach; each result is a full layout's,
    /// and the text in the web font is drawn in it.
    #[test]
    fn fonts_and_images_arriving_lay_out_only_what_they_reach() {
        let mut collection = fontique::Collection::new(fontique::CollectionOptions::default());
        let mut cache = fontique::SourceCache::default();
        let names: Vec<String> = collection.family_names().map(str::to_owned).collect();
        let ttf = names
            .iter()
            .filter_map(|n| collection.family_by_name(n))
            .flat_map(|fam| fam.fonts().to_vec())
            .filter_map(|f| f.load(Some(&mut cache)))
            .map(|b| b.data().to_vec())
            .find(|d| {
                use read_fonts::TableProvider;
                let Ok(read_fonts::FileRef::Font(f)) = read_fonts::FileRef::new(d) else { return false };
                f.cmap().is_ok_and(|cmap| "webfont text".chars().all(|c| c == ' ' || cmap.map_codepoint(c).is_some_and(|g| g.to_u32() != 0)))
            })
            .expect("an installed single-font file");
        let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='40' height='30'><rect width='40' height='30'/></svg>";
        let page = "<!DOCTYPE html><style>@font-face { font-family: Arriving; src: url(/f.ttf) }</style>\
            <body><div><p>before</p><img src='/i.svg'><ul style='list-style-image: url(/m.svg)'><li>item</li></ul>\
            <p id=w style='font-family: Arriving, monospace'>webfont text</p><p>after</p></div>";
        let html = vec![("Content-Type".to_owned(), "text/html; charset=utf-8".to_owned())];
        let svg_type = vec![("Content-Type".to_owned(), "image/svg+xml".to_owned())];
        let server = Server::start_with(
            vec![
                ("/page".to_owned(), html, page.as_bytes().to_vec()),
                ("/f.ttf".to_owned(), vec![("Content-Type".to_owned(), "font/ttf".to_owned())], ttf.clone()),
                ("/i.svg".to_owned(), svg_type.clone(), svg.as_bytes().to_vec()),
                ("/m.svg".to_owned(), svg_type, svg.as_bytes().to_vec()),
            ],
            &["/f.ttf", "/i.svg", "/m.svg"],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| h.state.layout.is_some() && h.state.lifecycle == Lifecycle::ContentLoaded);
        // The layout just made was incremental, and equals a full one made
        // by the same engine (the same fonts registered).
        let check = |h: &mut Harness, what: &str| {
            assert!(!browser_layout::last_layout_was_full(), "{what}: laid out incrementally");
            let rects = |t: &browser_layout::LayoutTree| {
                let mut v = Vec::new();
                t.root.walk(&mut |f| v.push((f.node, f.rect)));
                v
            };
            let incremental = rects(h.state.layout.as_ref().expect("layout"));
            let st = &mut h.state;
            let (w, ht) = (st.viewport.width, st.viewport.height);
            let full = st.engine.as_mut().expect("engine").layout(st.doc.as_ref().expect("doc"), &st.styles, w, ht, &st.images);
            assert_eq!(incremental, rects(&full), "{what}: equal to a full layout");
        };
        for (path, what) in [("/i.svg", "an <img>'s image"), ("/m.svg", "a list marker's image")] {
            let layouts = h.state.layouts;
            let url = server.url(path);
            server.release(path);
            h.pump_until(|h| h.state.images.get_svg(&url).is_some());
            h.pump();
            assert!(h.state.layouts > layouts, "{what}: laid out again");
            check(&mut h, what);
        }
        let layouts = h.state.layouts;
        server.release("/f.ttf");
        h.pump_until(|h| h.state.layouts > layouts);
        h.pump();
        check(&mut h, "a web font");
        let mut font = None;
        h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
            if let browser_layout::FragmentContent::Text(t) = &f.content
                && t.text.contains("webfont")
                && font.is_none()
            {
                font = Some(t.font.data.data().to_vec());
            }
        });
        assert!(font == Some(ttf), "the text is drawn in the arrived font");
    }

    /// Where the time goes on a real page, served over loopback from a
    /// file: `BROWSER_PROBE_HTML=<file> cargo test -p browser-tab --release
    /// probe_page_timings -- --ignored --nocapture`. Prints the tab's debug
    /// logs (parse, style, layout, paint) and the wall time of a load, two
    /// pointer moves and a scroll.
    #[test]
    #[ignore]
    fn probe_page_timings() {
        let Ok(path) = std::env::var("BROWSER_PROBE_HTML") else { return };
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("probe html {path}: {e}"));
        let html = String::from_utf8_lossy(&bytes).into_owned();
        // No debug subscriber: html5ever logs every token at debug level
        // through `log`, which would multiply the parse time; the shell
        // filters at `info`. The stages are timed here directly instead.
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();
        let server = Server::start(&[("/page", "text/html; charset=utf-8", &html)], &[]);
        // `BROWSER_PROBE_URL` loads a real URL through the real network
        // instead, to see where a load that never finishes is stuck.
        let url = match std::env::var("BROWSER_PROBE_URL") {
            Ok(u) => Url::parse(&u).expect("probe url"),
            Err(_) => server.url("/page"),
        };
        let started = std::time::Instant::now();
        let mut h = Harness::start(url);
        let deadline = started + std::time::Duration::from_secs(180);
        let mut rounds = 0;
        let mut last_report = started;
        while h.state.lifecycle != Lifecycle::Complete && std::time::Instant::now() < deadline {
            let t = std::time::Instant::now();
            h.pump();
            rounds += 1;
            let ms = t.elapsed().as_millis();
            if ms > 20 || last_report.elapsed().as_secs() >= 1 {
                last_report = std::time::Instant::now();
                let s = &h.state;
                let pending: Vec<String> = s
                    .pending
                    .values()
                    .map(|p| match &p.kind {
                        PendingKind::Main => format!("main({} bytes buffered)", p.body.len()),
                        PendingKind::Stylesheet { url, .. } => format!("css {url}"),
                        PendingKind::Image { url, .. } => format!("img {url}"),
                        PendingKind::Script { .. } => "script".to_owned(),
                        _ => "other".to_owned(),
                    })
                    .collect();
                eprintln!(
                    "{:5} ms round {rounds} ({ms} ms): parser={} blocked_script={:?} doc={} lifecycle={:?} nav={} blocking_script={:?} pending={:?}",
                    started.elapsed().as_millis(),
                    s.parser.is_some(),
                    s.parser.as_ref().and_then(HtmlParser::blocked_script).is_some(),
                    s.doc.is_some(),
                    s.lifecycle,
                    s.nav.is_some(),
                    s.scripts.blocking.as_ref().map(|b| b.url.clone()),
                    pending
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        eprintln!(
            "load: {:?} in {rounds} rounds, lifecycle {:?}, nodes {}, layouts {}, heap {:.1} MiB (peak {:.1} MiB)",
            started.elapsed(),
            h.state.lifecycle,
            h.state.doc.as_ref().map_or(0, |d| d.node_count()),
            h.state.layouts,
            browser_platform::mib(browser_platform::heap_live()),
            browser_platform::mib(browser_platform::heap_peak()),
        );
        for _ in 0..2 {
            let t = std::time::Instant::now();
            h.state.restyle_all();
            let style = t.elapsed();
            let t = std::time::Instant::now();
            h.state.relayout();
            let layout = t.elapsed();
            let t = std::time::Instant::now();
            h.state.repaint();
            eprintln!("restyle {style:?}, relayout {layout:?}, repaint {:?}", t.elapsed());
        }
        // What a user does on the page, each input timed on its own.
        let timed = |h: &mut Harness, what: &str, msg: ShellToTab| {
            let t = std::time::Instant::now();
            let layouts = h.state.layouts;
            h.send(msg);
            eprintln!("{what}: {:?} ({} layouts)", t.elapsed(), h.state.layouts - layouts);
        };
        for i in 0..5 {
            let y = 100.0 + i as f32 * 60.0;
            timed(&mut h, "mouse move", ShellToTab::MouseMove { x: 300.0, y });
        }
        for _ in 0..3 {
            timed(&mut h, "wheel scroll", ShellToTab::Scroll { dx: 0.0, dy: 300.0 });
        }
        timed(&mut h, "mouse move after scroll", ShellToTab::MouseMove { x: 300.0, y: 250.0 });
        timed(&mut h, "mouse down", ShellToTab::MouseDown { x: 300.0, y: 250.0, button: MouseButton::Left });
        timed(&mut h, "drag 1", ShellToTab::MouseMove { x: 320.0, y: 300.0 });
        timed(&mut h, "drag 2", ShellToTab::MouseMove { x: 340.0, y: 420.0 });
        timed(&mut h, "drag 3 (past the bottom: autoscroll)", ShellToTab::MouseMove { x: 340.0, y: 620.0 });
        timed(&mut h, "mouse up", ShellToTab::MouseUp { x: 340.0, y: 420.0, button: MouseButton::Left });
        let t = std::time::Instant::now();
        h.pump();
        eprintln!("pump after the drag: {:?}", t.elapsed());
        let tab = || ShellToTab::Key {
            key: Key::Tab,
            code: "Tab".to_owned(),
            repeat: false,
            shift: false,
            ctrl: false,
            alt: false,
        };
        // Down the page as a user reads it: a wheel step, the pointer
        // moving, the page's timers (its scroll handlers' work) run.
        let height = h.state.layout.as_ref().map_or(0.0, |t| t.content_height);
        let mut y = 0.0;
        let mut step = 0;
        while y < height && step < 40 {
            step += 1;
            timed(&mut h, &format!("read down {step} (at {y:.0} of {height:.0})"), ShellToTab::Scroll { dx: 0.0, dy: 600.0 });
            timed(&mut h, &format!("  pointer moves {step}"), ShellToTab::MouseMove { x: 300.0 + step as f32, y: 300.0 });
            std::thread::sleep(std::time::Duration::from_millis(20));
            let t = std::time::Instant::now();
            let layouts = h.state.layouts;
            h.pump();
            eprintln!("  timers {step}: {:?} ({} layouts)", t.elapsed(), h.state.layouts - layouts);
            y = h.state.scroll_y;
        }
        timed(&mut h, "tab key", tab());
        timed(&mut h, "tab key again", tab());
        timed(&mut h, "scroll far", ShellToTab::Scroll { dx: 0.0, dy: 400_000.0 });
        timed(&mut h, "mouse move far down", ShellToTab::MouseMove { x: 300.0, y: 300.0 });
        timed(&mut h, "resize", ShellToTab::Resize(Viewport { width: 900.0, height: 600.0, scale_factor: 1.0 }));
        // A click on whatever link is at the pointer: a navigation starts.
        let before = h.state.history.len();
        timed(&mut h, "click (down)", ShellToTab::MouseDown { x: 300.0, y: 300.0, button: MouseButton::Left });
        timed(&mut h, "click (up)", ShellToTab::MouseUp { x: 300.0, y: 300.0, button: MouseButton::Left });
        eprintln!("navigations started by the click: {}", h.state.history.len() - before + usize::from(h.state.nav.is_some()));
        eprintln!(
            "heap at the end: {:.1} MiB (peak {:.1} MiB)",
            browser_platform::mib(browser_platform::heap_live()),
            browser_platform::mib(browser_platform::heap_peak())
        );
    }

    // ----- forms ("Working sites first" line 8) -----

    /// The body of a logged request (after its head).
    fn body_of(request: &str) -> &str {
        request.split_once("\r\n\r\n").map_or("", |(_, b)| b)
    }

    /// A header of a logged request, lowercased name.
    fn header_of(request: &str, name: &str) -> Option<String> {
        request
            .lines()
            .take_while(|l| !l.is_empty())
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_owned()))
    }

    #[test]
    fn requests_carry_the_referrer_their_policy_gives() {
        let html = |h: &str| ("Content-Type".to_owned(), h.to_owned());
        let page = "<!DOCTYPE html><title>Page</title><link rel=stylesheet href=/s.css>\
             <style>@font-face { font-family: F; src: url(/inline.ttf) } p { font-family: F }</style>\
             <img src=/i.png referrerpolicy=origin><script src=/a.js></script>\
             <script type=module src=/m.js referrerpolicy=no-referrer-when-downgrade></script>\
             <p>text</p><h1>head</h1><a id=l href='/next#top'>next</a>";
        let next = "<!DOCTYPE html><title>Next</title><meta name=referrer content=origin>\
             <a id=n href=/last rel=noreferrer>last</a><a id=o href=/other>other</a>";
        let server = Server::start_with(
            vec![
                ("/page".to_owned(), vec![html("text/html")], page.as_bytes().to_vec()),
                (
                    "/s.css".to_owned(),
                    vec![html("text/css"), ("Referrer-Policy".to_owned(), "unsafe-url".to_owned())],
                    b"@import url(/imp.css); @font-face { font-family: G; src: url(/g.ttf) } body { background: url(/bg.png) } h1 { font-family: G }".to_vec(),
                ),
                ("/imp.css".to_owned(), vec![html("text/css")], b"p { color: red }".to_vec()),
                ("/a.js".to_owned(), vec![html("text/javascript")], b"fetch('/f');".to_vec()),
                ("/m.js".to_owned(), vec![html("text/javascript")], b"import './dep.js';".to_vec()),
                ("/dep.js".to_owned(), vec![html("text/javascript")], b"".to_vec()),
                ("/f".to_owned(), vec![html("text/plain")], b"ok".to_vec()),
                ("/next".to_owned(), vec![html("text/html")], next.as_bytes().to_vec()),
                ("/last".to_owned(), vec![html("text/html")], b"<title>Last</title>".to_vec()),
                ("/other".to_owned(), vec![html("text/html")], b"<title>Other</title>".to_vec()),
                ("/plain".to_owned(), vec![html("text/html")], b"<title>Plain</title>".to_vec()),
            ],
            &[],
        );
        let referer = |s: &Server, path: &str, n: usize| {
            let got = s.requests(path);
            assert!(got.len() > n, "{path} requested {} times", got.len());
            header_of(&got[n], "referer")
        };
        let page_url = server.url("/page").to_string();
        let origin = server.url("/").to_string();
        let sheet = server.url("/s.css").to_string();
        let mut h = Harness::load_url(server.url("/page"));
        // Every request the page makes has gone out.
        let paths = ["/s.css", "/a.js", "/m.js", "/f", "/i.png", "/inline.ttf", "/dep.js", "/imp.css", "/bg.png", "/g.ttf"];
        h.pump_until(|_| paths.iter().all(|p| !server.requests(p).is_empty()));
        // The page's own resources: its URL by the default policy, an
        // element's `referrerpolicy` when it has one.
        assert_eq!(referer(&server, "/s.css", 0).as_deref(), Some(&*page_url));
        assert_eq!(referer(&server, "/a.js", 0).as_deref(), Some(&*page_url));
        assert_eq!(referer(&server, "/m.js", 0).as_deref(), Some(&*page_url));
        assert_eq!(referer(&server, "/f", 0).as_deref(), Some(&*page_url), "a script's fetch");
        assert_eq!(referer(&server, "/i.png", 0).as_deref(), Some(&*origin), "the img's referrerpolicy");
        assert_eq!(referer(&server, "/inline.ttf", 0).as_deref(), Some(&*page_url), "an inline sheet's font");
        // A module's import: the importer, under the root's policy.
        assert_eq!(referer(&server, "/dep.js", 0).as_deref(), Some(&*server.url("/m.js").to_string()));
        // What the external sheet fetches: the sheet, under its response's
        // `unsafe-url`.
        assert_eq!(referer(&server, "/imp.css", 0).as_deref(), Some(&*sheet), "an @import");
        assert_eq!(referer(&server, "/bg.png", 0).as_deref(), Some(&*sheet), "a url() image");
        assert_eq!(referer(&server, "/g.ttf", 0).as_deref(), Some(&*sheet), "a font");
        assert_eq!(js_text(&mut h, "document.referrer"), "", "typed into the address bar: none");
        // A link: the page's URL, without the fragment; the new document's
        // `document.referrer`.
        h.click_id("l");
        h.pump_until(|h| h.title().as_deref() == Some("Next"));
        assert_eq!(referer(&server, "/next", 0).as_deref(), Some(&*page_url));
        assert_eq!(js_text(&mut h, "document.referrer"), page_url);
        // `rel=noreferrer`: none.
        h.click_id("n");
        h.pump_until(|h| h.title().as_deref() == Some("Last"));
        assert_eq!(referer(&server, "/last", 0), None);
        assert_eq!(js_text(&mut h, "document.referrer"), "");
        // Back loads the entry with its stored referrer again.
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Next"));
        assert_eq!(referer(&server, "/next", 1).as_deref(), Some(&*page_url));
        assert_eq!(js_text(&mut h, "document.referrer"), page_url);
        // The `<meta name=referrer content=origin>` sets the document's
        // policy: the origin only.
        h.click_id("o");
        h.pump_until(|h| h.title().as_deref() == Some("Other"));
        let next_origin = origin.clone();
        assert_eq!(referer(&server, "/other", 0).as_deref(), Some(&*next_origin));
        // A reload sends the entry's referrer again.
        h.send(ShellToTab::Reload);
        h.pump_until(|_| server.requests("/other").len() == 2);
        assert_eq!(referer(&server, "/other", 1).as_deref(), Some(&*next_origin));
        // The user's own navigation: none.
        h.send(ShellToTab::Navigate { url: server.url("/plain") });
        h.pump_until(|h| h.title().as_deref() == Some("Plain"));
        assert_eq!(referer(&server, "/plain", 0), None);
    }

    #[test]
    fn a_reload_restores_the_scroll_position_as_the_page_grows() {
        let server = Server::start(
            &[
                ("/tall", "text/html", "<!DOCTYPE html><title>T</title><body style='margin:0'><div style='height:1000px'></div><script async src=/grow.js></script>"),
                ("/grow.js", "text/javascript", "var d = document.createElement('div'); d.style.height = '3000px'; document.body.append(d);"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/tall"));
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        h.send(ShellToTab::Scroll { dx: 0.0, dy: 1500.0 });
        assert_eq!(h.state.scroll_y, 1500.0);
        // Reloaded with the script held: the page is short, so the restore
        // waits; it lands once the script makes the page tall enough.
        server.hold("/grow.js");
        let before = server.requests("/grow.js").len();
        h.send(ShellToTab::Reload);
        h.pump_polling(|_| server.requests("/grow.js").len() > before);
        h.pump();
        assert!(h.state.scroll_y < 1500.0, "not reachable yet: {}", h.state.scroll_y);
        server.release("/grow.js");
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        h.pump();
        assert_eq!(h.state.scroll_y, 1500.0, "restored after the reload");
        // The user scrolls while it waits: the restore gives up.
        server.hold("/grow.js");
        let before = server.requests("/grow.js").len();
        h.send(ShellToTab::Reload);
        h.pump_polling(|_| server.requests("/grow.js").len() > before);
        h.pump();
        h.send(ShellToTab::Scroll { dx: 0.0, dy: -100.0 });
        let user = h.state.scroll_y;
        assert!(user > 0.0 && user < 1500.0, "{user}");
        server.release("/grow.js");
        h.pump_until(|h| h.state.lifecycle == Lifecycle::Complete);
        h.pump();
        assert_eq!(h.state.scroll_y, user, "left where the user put it");
    }

    #[test]
    fn location_fragment_changes_fire_popstate_at_once_and_hashchange_after() {
        let server = Server::start(&[("/f", "text/html", "<!DOCTYPE html><title>F</title><p id=a>a</p>")], &[]);
        let mut h = Harness::load_url(server.url("/f"));
        h.pump_until(|h| !h.state.is_loading());
        // `popstate` inside the setter, `hashchange` as a task after it.
        assert_eq!(
            run_js(
                &mut h,
                "var ev = []; onpopstate = e => ev.push('pop:' + location.hash + ':' + e.state); onhashchange = () => ev.push('hash'); \
                 location.hash = 'a'; ev.push('after:' + history.length); ev.join(' ')"
            ),
            "\"pop:#a:null after:2\""
        );
        assert_eq!(run_js(&mut h, "ev.join(' ')"), "\"pop:#a:null after:2 hash\"");
        assert_eq!(h.state.history.len(), 2, "the tab has the entry");
        assert!(h.state.url.as_ref().is_some_and(|u| u.fragment() == Some("a")));
        // The same fragment: nothing; one leading `#` goes.
        assert_eq!(run_js(&mut h, "ev = []; location.hash = '#a'; [ev.length, history.length].join()"), "\"0,2\"");
        assert_eq!(run_js(&mut h, "location.hash = '##b'; location.hash"), "\"##b\"");
        // `location.replace` to a fragment replaces the entry.
        assert_eq!(run_js(&mut h, "location.replace('#c'); [location.hash, history.length].join()"), "\"#c,3\"");
        assert_eq!(h.state.history.len(), 3);
    }

    #[test]
    fn a_base_href_moves_every_relative_url() {
        let page = "<!DOCTYPE html><title>Page</title><base href='/sub/'><link rel=stylesheet href=s.css>\
             <img src=i.png><script src=a.js></script><a id=l href=next>next</a>\
             <form id=f><button id=b>go</button></form><div id=d style='background-image: url(bg.png)'>d</div>\
             <style>#s { background-image: url(st.png) }</style><p id=s>s</p>";
        let server = Server::start(
            &[
                ("/dir/page", "text/html", page),
                ("/dir/page?", "text/html", "<title>Self</title><style>#s { background-image: url(st.png) }</style><p id=s>s</p>"),
                ("/sub/s.css", "text/css", "p { color: red }"),
                ("/sub/a.js", "text/javascript", "fetch('f');"),
                ("/sub/f", "text/plain", "f"),
                ("/sub/next", "text/html", "<title>Next</title>"),
                ("/sub/loc", "text/html", "<title>Loc</title>"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/dir/page"));
        // The page's resources resolve against the base.
        let paths = ["/sub/s.css", "/sub/i.png", "/sub/a.js", "/sub/f", "/sub/bg.png", "/sub/st.png"];
        h.pump_until(|_| paths.iter().all(|p| !server.requests(p).is_empty()));
        h.pump_until(|h| !h.state.is_loading());
        let sub = server.url("/sub/").to_string();
        assert_eq!(run_js(&mut h, "document.baseURI"), format!("\"{sub}\""));
        assert_eq!(run_js(&mut h, "document.getElementById('l').baseURI"), format!("\"{sub}\""));
        assert_eq!(
            run_js(&mut h, "document.body.append(document.createElement('i')); getComputedStyle(document.getElementById('s')).backgroundImage"),
            format!("\"url(\\\"{}\\\")\"", server.url("/sub/st.png"))
        );
        // A form without an action submits to the document's own URL.
        h.click_id("b");
        h.pump_until(|h| h.title().as_deref() == Some("Self"));
        assert_eq!(server.requests("/dir/page?").len(), 1);
        // The same `<style>` text in another document resolves against
        // that document's base.
        assert_eq!(
            run_js(&mut h, "document.body.append(document.createElement('i')); getComputedStyle(document.getElementById('s')).backgroundImage"),
            format!("\"url(\\\"{}\\\")\"", server.url("/dir/st.png"))
        );
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Page") && !h.state.is_loading());
        // A link, and `location`, resolve against the base.
        h.click_id("l");
        h.pump_until(|h| h.title().as_deref() == Some("Next"));
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Page") && !h.state.is_loading());
        run_js(&mut h, "location.assign('loc')");
        h.pump_until(|h| h.title().as_deref() == Some("Loc"));
        // A script changing the base: what resolves later follows.
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Page") && !h.state.is_loading());
        let sheet_loads = server.requests("/sub/s.css").len();
        assert_eq!(
            run_js(&mut h, "document.querySelector('base').setAttribute('href', '/other/'); document.baseURI"),
            format!("\"{}\"", server.url("/other/"))
        );
        // What was parsed or processed keeps its base (no fetch again, even
        // after a restyle); what is set or inserted now takes the new one.
        run_js(&mut h, "document.getElementById('d').className = 'x'; document.body.offsetWidth");
        run_js(
            &mut h,
            "var d2 = document.createElement('div'); d2.textContent = 'e'; document.body.append(d2); d2.setAttribute('style', 'background-image: url(bg2.png)'); \
             var l2 = document.createElement('link'); l2.rel = 'stylesheet'; l2.setAttribute('rel', 'stylesheet'); l2.setAttribute('href', 's2.css'); document.head.append(l2);",
        );
        h.pump_until(|_| !server.requests("/other/bg2.png").is_empty() && !server.requests("/other/s2.css").is_empty());
        h.pump();
        assert!(server.requests("/other/s.css").is_empty(), "the loaded link is not fetched again");
        assert!(server.requests("/other/bg.png").is_empty(), "the style attribute keeps its base");
        assert!(server.requests("/other/st.png").is_empty(), "the inline sheet keeps its base");
        assert_eq!(
            run_js(&mut h, "document.body.append(document.createElement('i')); getComputedStyle(document.getElementById('s')).backgroundImage"),
            format!("\"url(\\\"{}\\\")\"", server.url("/sub/st.png")),
            "getComputedStyle agrees"
        );
        // Its text changed: parsed again, with the base of now.
        run_js(&mut h, "document.querySelector('style').textContent += ' '");
        h.pump_until(|_| !server.requests("/other/st.png").is_empty());
        assert_eq!(server.requests("/sub/s.css").len(), sheet_loads, "nor from its old URL");
        assert_eq!(run_js(&mut h, "document.querySelector('base').remove(); document.baseURI"), format!("\"{}\"", server.url("/dir/page")));
    }

    #[test]
    fn fetch_sends_the_referrer_and_policy_it_was_given() {
        let server = Server::start(
            &[
                ("/p", "text/html", "<title>P</title>"),
                ("/n1", "text/plain", "1"),
                ("/n2", "text/plain", "2"),
                ("/n3", "text/plain", "3"),
                ("/n4", "text/plain", "4"),
                ("/n5", "text/plain", "5"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/p?q"));
        h.pump_until(|h| !h.state.is_loading());
        run_js(
            &mut h,
            "fetch('/n1', {referrerPolicy: 'no-referrer'}); fetch('/n2', {referrerPolicy: 'origin'}); \
             fetch('/n3', {referrer: '/from?x#f'}); fetch('/n4', {referrer: 'https://other.test/'}); \
             fetch(new Request('/n5', {referrer: ''}));",
        );
        let paths = ["/n1", "/n2", "/n3", "/n4", "/n5"];
        h.pump_until(|_| paths.iter().all(|p| !server.requests(p).is_empty()));
        let referer = |p: &str| header_of(&server.requests(p)[0], "referer");
        assert_eq!(referer("/n1"), None, "referrerPolicy: no-referrer");
        assert_eq!(referer("/n2").as_deref(), Some(&*server.url("/").to_string()), "referrerPolicy: origin");
        assert_eq!(referer("/n3").as_deref(), Some(&*server.url("/from?x").to_string()), "its own referrer, stripped");
        assert_eq!(referer("/n4").as_deref(), Some(&*server.url("/p?q").to_string()), "cross-origin: the document's");
        assert_eq!(referer("/n5"), None, "referrer: ''");
    }

    #[test]
    fn forms_scripts_and_new_tabs_carry_the_referrer() {
        let page = "<!DOCTYPE html><title>P</title><form id=f action=/r><button id=b>go</button></form>\
             <form id=fn action=/r2 rel=noreferrer><button id=bn>go</button></form>\
             <a id=nt href='/x?q' referrerpolicy=origin>x</a>";
        let server = Server::start(
            &[
                ("/p", "text/html", page),
                ("/r", "text/html", "<title>R</title>"),
                ("/r2", "text/html", "<title>R2</title>"),
                ("/after", "text/html", "<title>After</title>"),
            ],
            &[],
        );
        let referer = |s: &Server, prefix: &str| {
            let got = s.requests_starting(prefix);
            assert_eq!(got.len(), 1, "{prefix}");
            header_of(&got[0], "referer")
        };
        let page_url = server.url("/p").to_string();
        let mut h = Harness::load_url(server.url("/p"));
        h.pump_until(|h| !h.state.is_loading());
        // A form's submission: the page; with `rel=noreferrer`, none.
        h.click_id("b");
        h.pump_until(|h| h.title().as_deref() == Some("R"));
        assert_eq!(referer(&server, "/r?").as_deref(), Some(&*page_url));
        assert_eq!(js_text(&mut h, "document.referrer"), page_url);
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("P") && !h.state.is_loading());
        h.click_id("bn");
        h.pump_until(|h| h.title().as_deref() == Some("R2"));
        assert_eq!(referer(&server, "/r2"), None);
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("P") && !h.state.is_loading());
        // A link opened in a new tab hands the shell its referrer.
        let link = h.by_id("nt");
        h.state.open_link_in_new_tab(link);
        let sent = h.messages.lock().expect("lock").iter().rev().find_map(|m| match m {
            TabToShell::OpenInNewTab { referrer, .. } => Some(referrer.clone()),
            _ => None,
        });
        assert_eq!(
            sent,
            Some(NavReferrer {
                request: Some(RequestReferrer {
                    source: server.url("/p"),
                    policy: ReferrerPolicy::Origin
                }),
                policy: ReferrerPolicy::Origin,
            })
        );
        // A `<meta name=referrer>` a script inserts sets the policy for
        // what follows, `location` included.
        run_js(
            &mut h,
            "var m = document.createElement('meta'); m.setAttribute('name', 'Referrer'); m.setAttribute('content', 'NEVER'); \
             document.head.append(m); location.assign('/after');",
        );
        h.pump_until(|h| h.title().as_deref() == Some("After"));
        assert_eq!(referer(&server, "/after"), None);
    }

    #[test]
    fn forms_submit_get_and_post_with_their_encodings() {
        let form_page = "<!DOCTYPE html><title>Form</title>\
             <form id=g action=/result><input name=q value='a b'><input type=checkbox name=c checked><input type=checkbox name=d>\
             <select name=s><option>x<option selected value=yv>y</select><textarea name=t>l1\nl2</textarea>\
             <button id=go name=go value=1>Go</button><button id=other name=other value=2>Other</button></form>\
             <form id=p method=post action=/post><input id=pq name=q value=x><input id=pq2 name=r value=é><input type=submit id=ps></form>\
             <form id=m method=post enctype=multipart/form-data action=/multi><input name=f value=v><input type=file name=up></form>\
             <form id=tp method=post enctype=text/plain action=/plain><input name=a value=1><input name=b value=2></form>";
        let server = Server::start(
            &[
                ("/form", "text/html; charset=utf-8", form_page),
                ("/result", "text/html", "<title>Result</title>"),
                ("/post", "text/html", "<title>Posted</title>"),
                ("/multi", "text/html", "<title>Multi</title>"),
                ("/plain", "text/html", "<title>Plain</title>"),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/form"));
        h.pump_until(|h| !h.state.is_loading());
        // A click on the submit button: GET with the entries in the query,
        // the button's own entry, unchecked boxes and other buttons left
        // out, the textarea's newline as CRLF.
        h.click_id("go");
        h.pump_until(|h| h.title().as_deref() == Some("Result"));
        let got = server.requests_starting("/result");
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with("GET /result?q=a+b&c=on&s=yv&t=l1%0D%0Al2&go=1 HTTP/1.1"), "{}", got[0]);
        assert_eq!(h.state.history.len(), 2, "a new entry");

        // Enter in a field: implicit submission through the default button,
        // POSTed form-urlencoded with the document's origin.
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Form"));
        h.click_end_of("pq");
        h.typed("y");
        h.key(Key::Enter, false);
        h.pump_until(|h| h.title().as_deref() == Some("Posted"));
        let post = server.requests("/post");
        assert_eq!(post.len(), 1);
        assert!(post[0].starts_with("POST /post HTTP/1.1"), "{}", post[0]);
        assert_eq!(header_of(&post[0], "content-type").as_deref(), Some("application/x-www-form-urlencoded"));
        let origin = server.base.origin().ascii_serialization();
        assert_eq!(header_of(&post[0], "origin").as_deref(), Some(origin.as_str()));
        assert_eq!(body_of(&post[0]), "q=xy&r=%C3%A9");

        // multipart/form-data from `requestSubmit()`, an empty file input
        // sending an empty file part.
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Form"));
        run_js(&mut h, "document.getElementById('m').requestSubmit()");
        h.pump_until(|h| h.title().as_deref() == Some("Multi"));
        let multi = &server.requests("/multi")[0];
        let ct = header_of(multi, "content-type").expect("content type");
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").expect("boundary").to_owned();
        assert!(boundary.starts_with("----WebKitFormBoundary"), "{ct}");
        assert_eq!(
            body_of(multi),
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"up\"; filename=\"\"\r\nContent-Type: application/octet-stream\r\n\r\n\r\n\
                 --{boundary}--\r\n"
            )
        );

        // text/plain from `submit()`.
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Form"));
        run_js(&mut h, "document.getElementById('tp').submit()");
        h.pump_until(|h| h.title().as_deref() == Some("Plain"));
        let plain = &server.requests("/plain")[0];
        assert_eq!(header_of(plain, "content-type").as_deref(), Some("text/plain"));
        assert_eq!(body_of(plain), "a=1\r\nb=2\r\n");
    }

    #[test]
    fn form_events_cancel_change_and_replace_submissions() {
        let page = "<!DOCTYPE html><title>Events</title>\
             <form id=f action=/r><input id=q name=q value=1><button id=b>Go</button><input type=reset id=rs></form>\
             <script>var log = []; var cancel = false; var f = document.getElementById('f'); \
               f.addEventListener('submit', ev => { log.push('submit:' + (ev instanceof SubmitEvent) + ':' + (ev.submitter && ev.submitter.id) + ':' + ev.cancelable + ':' + ev.bubbles); if (cancel) ev.preventDefault(); }); \
               f.addEventListener('formdata', ev => { log.push('formdata:' + (ev.formData instanceof FormData) + ':' + ev.formData.get('q')); ev.formData.append('extra', 'yes'); }); \
               f.addEventListener('reset', ev => log.push('reset:' + ev.cancelable));</script>";
        // /r is held so the log can be read before the new page lands.
        let server = Server::start(&[("/page", "text/html", page), ("/r", "text/html", "<title>R</title>")], &["/r"]);
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| !h.state.is_loading());
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");
        // A cancelled `submit` navigates nowhere; `formdata` does not fire.
        js(&mut h, "cancel = true");
        h.click_id("b");
        assert_eq!(drain(&mut h), "\"submit:true:b:true:true\"");
        assert!(h.state.nav.is_none());
        // The user's edit, then a reset: `reset` fires and the value is
        // the default again.
        h.click_end_of("q");
        h.typed("23");
        assert_eq!(h.value("q").as_deref(), Some("123"));
        h.click_id("rs");
        assert_eq!(drain(&mut h), "\"reset:true\"");
        assert_eq!(h.value("q").as_deref(), Some("1"));
        // Not cancelled: `formdata` sees the entries and adds one, which is
        // submitted.
        js(&mut h, "cancel = false");
        h.click_id("b");
        assert_eq!(drain(&mut h), "\"submit:true:b:true:true formdata:true:1\"");
        server.release("/r");
        h.pump_until(|h| h.title().as_deref() == Some("R"));
        assert!(server.requests_starting("/r")[0].starts_with("GET /r?q=1&extra=yes "), "{:?}", server.requests_starting("/r"));
        // `submit()` fires no `submit` event; two submissions in one task:
        // the later one is the planned navigation.
        server.hold("/r");
        h.send(ShellToTab::GoBack);
        h.pump_until(|h| h.title().as_deref() == Some("Events"));
        run_js(
            &mut h,
            "f.querySelector('input').value = 'first'; f.submit(); f.querySelector('input').value = 'second'; f.submit()",
        );
        assert_eq!(drain(&mut h), "\"formdata:true:first formdata:true:second\"");
        server.release("/r");
        h.pump_until(|h| h.title().as_deref() == Some("R"));
        let rs = server.requests_starting("/r");
        assert_eq!(rs.len(), 2, "only the last submission went out: {rs:?}");
        assert!(rs[1].starts_with("GET /r?q=second&extra=yes "), "{rs:?}");
    }

    #[test]
    fn activation_checks_before_listeners_and_scripts_click_controls() {
        let html = "<!DOCTYPE html><title>Act</title><form id=f action='data:text/html,x'>\
             <input type=checkbox id=c><input type=radio name=g id=r1 checked><input type=radio name=g id=r2>\
             <input id=t value=v></form>\
             <script>var log = []; var cancel = false; \
               document.getElementById('c').onclick = ev => { log.push('click:' + ev.target.checked); if (cancel) ev.preventDefault(); }; \
               document.getElementById('r2').onclick = ev => { log.push('r:' + document.getElementById('r1').checked + ':' + ev.target.checked); if (cancel) ev.preventDefault(); }; \
               document.addEventListener('change', ev => log.push('change:' + ev.target.id + ':' + ev.target.checked));</script>";
        let mut h = Harness::load(html);
        let drain = |h: &mut Harness| js(h, "log.splice(0).join(' ')");
        // The listener sees the new checkedness; `change` follows.
        h.click_id("c");
        assert_eq!(drain(&mut h), "\"click:true change:c:true\"");
        assert!(h.checked("c"));
        // A cancelled click: checked while it runs, back after, no change.
        js(&mut h, "cancel = true");
        h.click_id("c");
        assert_eq!(drain(&mut h), "\"click:false\"");
        assert!(h.checked("c"), "restored");
        h.click_id("r2");
        assert_eq!(drain(&mut h), "\"r:false:true\"");
        assert!(h.checked("r1") && !h.checked("r2"), "the group's checked radio is back");
        js(&mut h, "cancel = false");
        // A script's `click` MouseEvent activates as the user's does; a
        // plain `Event` does not.
        assert_eq!(
            run_js(&mut h, "var c = document.getElementById('c'); c.dispatchEvent(new MouseEvent('click', {bubbles: true})); c.checked"),
            "false"
        );
        assert_eq!(drain(&mut h), "\"click:false change:c:false\"");
        assert_eq!(run_js(&mut h, "c.dispatchEvent(new Event('click')); c.checked"), "false");
        // The `checked` IDL attribute is the state; the attribute stays the
        // default, and a reset goes back to it.
        assert_eq!(run_js(&mut h, "c.checked = true; [c.checked, c.hasAttribute('checked')].join()"), "\"true,false\"");
        assert!(h.checked("c"));
        // `:checked` follows the state.
        assert_eq!(run_js(&mut h, "document.querySelectorAll(':checked').length"), "2");
        assert_eq!(run_js(&mut h, "document.getElementById('f').reset(); c.checked"), "false");
    }

    #[test]
    fn output_reset_replaces_its_children_with_its_text() {
        let html = "<!DOCTYPE html><title>Out</title>\
             <style>output { display: inline-block } #wide { display: inline-block; width: 100px }</style>\
             <form id=f><output id=o><span id=wide>5</span>6<!--c--></output><output id=e></output><output id=t>x</output>\
             <input type=reset id=r></form>\
             <script>var recs = []; \
               new MutationObserver(l => l.forEach(m => recs.push(m.target.id + ':' + m.addedNodes.length + ':' + m.removedNodes.length))) \
                 .observe(document.getElementById('f'), {childList: true, subtree: true});</script>";
        let mut h = Harness::load(html);
        let o = h.by_id("o");
        assert!(h.rect_of(o).width >= 100.0, "the span's width at first");
        // Its children become one text node of its descendant text (the
        // comment's data is not text); an empty one stays empty and no
        // record is queued for it; a text-only one gets a new text node.
        assert_eq!(
            run_js(
                &mut h,
                "var o = document.getElementById('o'), w = document.getElementById('wide'), t = document.getElementById('t'), t0 = t.firstChild; \
                 document.getElementById('f').reset(); \
                 [o.childNodes.length, o.firstChild.nodeType, o.textContent, w.parentNode === null, \
                  document.getElementById('e').childNodes.length, t.textContent, t.firstChild !== t0].join()"
            ),
            "\"1,3,56,true,0,x,true\""
        );
        assert_eq!(js(&mut h, "recs.splice(0).join(' ')"), "\"o:1:3 t:1:1\"");
        assert!(h.rect_of(o).width < 100.0, "laid out again with the text alone: {:?}", h.rect_of(o));
        // The user's reset button runs the same reset.
        run_js(&mut h, "o.innerHTML = '<b>7</b>'");
        js(&mut h, "recs.length = 0");
        h.click_id("r");
        assert_eq!(run_js(&mut h, "[o.childNodes.length, o.firstChild.nodeType, o.textContent].join()"), "\"1,3,7\"");
        assert_eq!(js(&mut h, "recs.splice(0).join(' ')"), "\"o:1:1 t:1:1\"");
    }

    #[test]
    fn method_dialog_fires_close_as_a_task() {
        let html = "<!DOCTYPE html><title>Dlg</title>\
             <dialog id=d open><form id=f method=dialog><button id=b value=ok>OK</button></form></dialog>\
             <script>var log = []; document.getElementById('d').onclose = ev => log.push('close:' + ev.isTrusted + ':' + ev.bubbles);</script>";
        let mut h = Harness::load(html);
        // `open` goes at once; `close` waits for a task of its own, after
        // every microtask of this one (those they queue included).
        assert_eq!(
            run_js(
                &mut h,
                "document.getElementById('f').requestSubmit(); \
                 Promise.resolve().then(() => { log.push('micro'); Promise.resolve().then(() => log.push('micro2')); }); \
                 [document.getElementById('d').hasAttribute('open'), log.length].join()"
            ),
            "\"false,0\""
        );
        assert_eq!(js(&mut h, "log.splice(0).join(' ')"), "\"micro micro2 close:true:false\"");
        // A closed dialog: no second `close`.
        run_js(&mut h, "document.getElementById('f').requestSubmit()");
        assert_eq!(js(&mut h, "log.length"), "0");
        // The user's click closes it the same way.
        run_js(&mut h, "document.getElementById('d').setAttribute('open', '')");
        h.click_id("b");
        h.pump();
        assert_eq!(js(&mut h, "log.splice(0).join(' ')"), "\"close:true:false\"");
    }

    #[test]
    fn control_values_and_form_interfaces_in_script() {
        let html = "<!DOCTYPE html><title>IDL</title>\
             <form id=f name=login><input id=u name=user value=def><input id=n type=number name=num>\
             <select id=s name=sel><option value=a>A<option value=b>B</select><textarea id=ta name=ta>x</textarea>\
             <input type=image id=img name=im><button id=b name=btn>Go</button></form><input id=out form=f name=outside>";
        let mut h = Harness::load(html);
        let r = |h: &mut Harness, s: &str| run_js(h, s);
        assert_eq!(r(&mut h, "var u = document.getElementById('u'); [u instanceof HTMLInputElement, u.value].join()"), "\"true,def\"");
        assert_eq!(r(&mut h, "u.value = 'a\\nb'; [u.value, u.getAttribute('value')].join()"), "\"ab,def\"");
        assert_eq!(h.control_text("u"), "ab", "the box shows the value");
        assert_eq!(r(&mut h, "var n = document.getElementById('n'); n.value = 'abc'; n.value"), "\"\"");
        assert_eq!(r(&mut h, "n.value = '1e3'; n.value"), "\"1e3\"");
        assert_eq!(r(&mut h, "var s = document.getElementById('s'); [s.value, s.selectedIndex, s.length, s.options.length].join()"), "\"a,0,2,2\"");
        assert_eq!(r(&mut h, "s.value = 'b'; [s.selectedIndex, s.options[1].selected].join()"), "\"1,true\"");
        assert_eq!(h.control_text("s"), "B");
        assert_eq!(r(&mut h, "s.selectedIndex = 5; [s.value, s.selectedIndex].join()"), "\",-1\"");
        assert_eq!(r(&mut h, "s.options[0].selected = true; s.value"), "\"a\"");
        assert_eq!(r(&mut h, "var ta = document.getElementById('ta'); ta.value = 'y\\r\\nz'; [JSON.stringify(ta.value), ta.textContent].join()"), "\"\\\"y\\\\nz\\\",x\"");
        // The form's members and its named and indexed properties.
        assert_eq!(r(&mut h, "var f = document.forms[0]; [f instanceof HTMLFormElement, f.elements.length, f.length, document.forms.length].join()"), "\"true,6,6,1\"");
        assert_eq!(r(&mut h, "[f.user === u, f[0] === u, f.outside.id, f.elements.ta.id, 'im' in f, f.img].join()"), "\"true,true,out,ta,false,\"");
        assert_eq!(r(&mut h, "[u.form === f, document.getElementById('out').form === f, s.options[0].form === f].join()"), "\"true,true,true\"");
        // FormData from the form, and its API.
        assert_eq!(
            r(&mut h, "var d = new FormData(f); [...d.keys()].join()"),
            "\"user,num,sel,ta,outside\""
        );
        assert_eq!(r(&mut h, "d.append('user', 'two'); d.set('ta', 'T'); [d.getAll('user').join('+'), d.get('ta'), d.has('nope'), d.get('nope')].join()"), "\"ab+two,T,false,\"");
        assert_eq!(r(&mut h, "d.delete('user'); [...d].map(e => e.join('=')).join('&')"), "\"num=1e3&sel=a&ta=T&outside=\"");
        assert_eq!(r(&mut h, "var e = new FormData(f, document.getElementById('b')); e.get('btn')"), "\"\"");
        assert_eq!(r(&mut h, "try { new FormData(f, u); 'no' } catch (x) { x.name }"), "\"TypeError\"");
        assert_eq!(r(&mut h, "try { f.requestSubmit(u); 'no' } catch (x) { x.name }"), "\"TypeError\"");
        assert_eq!(r(&mut h, "var ev = new SubmitEvent('submit', {submitter: u}); [ev.submitter === u, new SubmitEvent('x').submitter].join()"), "\"true,\"");
        assert_eq!(r(&mut h, "try { new FormDataEvent('x', {}); 'no' } catch (x) { x.name }"), "\"TypeError\"");
    }

    #[test]
    fn implicit_submission_follows_the_default_button_rules() {
        let page = "<!DOCTYPE html><title>Implicit</title>\
             <form id=two action=/two><input id=a name=a><input id=b name=b></form>\
             <form id=one action=/one><input id=c name=c><input type=checkbox id=cb name=cb></form>\
             <form id=dis action=/dis><input id=d name=d><button disabled>x</button></form>\
             <form id=blank action=/blank target=_blank method=post><input id=e name=e value=1></form>";
        let server = Server::start(
            &[("/page", "text/html", page), ("/one", "text/html", "<title>One</title>"), ("/two", "text/html", "<title>Two</title>")],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| !h.state.is_loading());
        // Two fields that block implicit submission and no submit button:
        // nothing.
        h.click_id("a");
        h.key(Key::Enter, false);
        assert!(h.state.nav.is_none());
        // A disabled default button: nothing.
        h.click_id("d");
        h.key(Key::Enter, false);
        assert!(h.state.nav.is_none());
        // A new tab for `target=_blank`, carrying its POST.
        h.click_id("e");
        h.key(Key::Enter, false);
        let opened: Vec<(Url, Option<browser_ipc_types::PostData>)> = h
            .messages
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|m| match m {
                TabToShell::OpenFormSubmission { url, post, .. } => Some((url.clone(), post.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].0, server.url("/blank"));
        assert_eq!(opened[0].1.as_ref().map(|p| p.body.as_slice()), Some(&b"e=1"[..]));
        assert!(h.state.nav.is_none(), "this tab stays");
        // One blocking field (a checkbox does not block): Enter submits, also
        // from the checkbox.
        h.click_id("cb");
        h.key(Key::Enter, false);
        h.pump_until(|h| h.title().as_deref() == Some("One"));
        assert!(server.requests_starting("/one")[0].starts_with("GET /one?c=&cb=on "), "{:?}", server.requests_starting("/one"));
    }

    #[test]
    fn a_post_redirected_by_303_loads_the_target_with_get() {
        let page = "<!DOCTYPE html><title>P</title><form id=f method=post action=/save><input name=q value=1><button id=b>Save</button></form>";
        let server = Server::start_with(
            vec![
                ("/page".to_owned(), vec![("Content-Type".to_owned(), "text/html".to_owned())], page.as_bytes().to_vec()),
                (
                    "/save".to_owned(),
                    vec![(":status".to_owned(), "303 See Other".to_owned()), ("Location".to_owned(), "/done".to_owned())],
                    Vec::new(),
                ),
                ("/done".to_owned(), vec![("Content-Type".to_owned(), "text/html".to_owned())], b"<title>Done</title>".to_vec()),
            ],
            &[],
        );
        let mut h = Harness::load_url(server.url("/page"));
        h.pump_until(|h| !h.state.is_loading());
        h.click_id("b");
        h.pump_until(|h| h.title().as_deref() == Some("Done"));
        assert!(server.requests("/save")[0].starts_with("POST /save "));
        let done = &server.requests("/done")[0];
        assert!(done.starts_with("GET /done "), "{done}");
        assert_eq!(header_of(done, "content-type"), None, "the body's headers went with the body");
        assert_eq!(header_of(done, "origin"), None);
        assert_eq!(h.state.url, Some(server.url("/done")));
    }

    #[test]
    fn appearance_none_drops_the_native_look() {
        let html = "<!DOCTYPE html><body style='margin:0'>\
             <input type=checkbox id=c1><input type=checkbox id=c2 style='appearance:none'>\
             <input type=checkbox id=c3 style='-webkit-appearance:none; width:20px; height:20px'>\
             <select id=s1><option>A rather long option label</option></select>\
             <select id=s2 style='-moz-appearance:none'><option>A rather long option label</option></select>";
        let mut h = Harness::load(html);
        let w = |h: &Harness, id: &str| h.rect_of(h.by_id(id)).width;
        assert_eq!(w(&h, "c1"), 13.0, "the native box");
        assert_eq!(w(&h, "c2"), 0.0, "no look, no size of its own");
        assert_eq!(w(&h, "c3"), 20.0);
        let (w1, w2) = (w(&h, "s1"), w(&h, "s2"));
        assert!(
            w2 < w1,
            "no arrow, no room for it: {w2} {w1} {}",
            run_js(&mut h, "[getComputedStyle(s1).appearance, getComputedStyle(s2).appearance, s2.style.cssText].join('|')")
        );
        let controls: Vec<(String, browser_layout::Control)> = {
            let doc = h.state.doc.as_ref().expect("doc");
            let mut out = Vec::new();
            h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
                if let browser_layout::FragmentContent::Control(c) = &f.content {
                    let id = f.node.and_then(|n| doc.element(n)).and_then(|e| e.id()).unwrap_or("").to_owned();
                    out.push((id, *c));
                }
            });
            out
        };
        let ids: Vec<&str> = controls.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(ids, ["c1", "s1"], "only the native controls are drawn as controls");
        assert_eq!(
            run_js(&mut h, "[getComputedStyle(c1).appearance, getComputedStyle(c2).appearance, getComputedStyle(document.body).appearance, c3.style.cssText].join('|')"),
            "\"auto|none|none|appearance: none; width: 20px; height: 20px;\""
        );
        // The indeterminate dash, and its control state.
        run_js(&mut h, "c1.indeterminate = true");
        h.pump();
        let mut dash = None;
        h.state.layout.as_ref().expect("layout").root.walk(&mut |f| {
            if let browser_layout::FragmentContent::Control(browser_layout::Control::Checkbox { indeterminate, .. }) = &f.content {
                dash = Some(*indeterminate);
            }
        });
        assert_eq!(dash, Some(true));
    }
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
