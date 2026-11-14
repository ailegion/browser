//! Messages between the three layers (see plan/02-architecture.md).
//!
//! Shell, tab, and network code talk only through these types. They are all
//! `serde` types so the transport can change (in-process channels today,
//! anything else later) without touching the layers.
//!
//! Variants here are the Phase 0 skeleton. Add variants as phases need them;
//! never add a second channel that bypasses these.

#![forbid(unsafe_code)]

pub mod referrer;

pub use referrer::{NavigationReferrer, ReferrerPolicy, RequestReferrer};
use serde::{Deserialize, Serialize};
use url::Url;

/// Identifies one tab for the lifetime of the browser process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TabId(pub u64);

/// Identifies one in-flight network request within a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

/// Logical size in device-independent pixels plus the scale factor.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub scale_factor: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Other,
}

/// The cursor the shell should show over the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Cursor {
    #[default]
    Default,
    Pointer,
    Text,
    /// Middle-click autoscroll is on, or CSS `all-scroll`.
    AllScroll,
    /// The rest of CSS `cursor`'s keywords (CSS Basic UI 4 §8.2).
    None,
    ContextMenu,
    Help,
    Progress,
    Wait,
    Cell,
    Crosshair,
    VerticalText,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    EResize,
    NResize,
    NeResize,
    NwResize,
    SResize,
    SeResize,
    SwResize,
    WResize,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
    ColResize,
    RowResize,
    ZoomIn,
    ZoomOut,
}

/// A `cursor: url()` image, decoded, with its hotspot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    /// RGBA8 pixels, row-major, straight alpha.
    pub rgba: Vec<u8>,
    pub hotspot: (u16, u16),
}

/// A key the page may act on. Text keys come as `Character` with what
/// the key produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Key {
    Character(String),
    Tab,
    Enter,
    Escape,
    Space,
    Backspace,
    Delete,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    /// The context-menu key, or Shift+F10.
    ContextMenu,
}

/// The body of a navigation that POSTs (a form submitted as an entity
/// body), with the `Origin` of the document that submitted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostData {
    pub content_type: String,
    pub body: Vec<u8>,
    pub origin: String,
}

/// Shell to tab. Mouse positions are logical pixels within the page's
/// viewport (the tab adds its own scroll offset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShellToTab {
    /// Navigate as the user asked (the address bar, a bookmark): no
    /// referrer.
    Navigate { url: Url },
    /// Navigate for a page that opened this tab (a link opened in a new
    /// tab): `referrer` is where its `Referer` comes from, none for a
    /// `noreferrer` link.
    NavigateFrom { url: Url, referrer: NavigationReferrer },
    /// Navigate with a POST body (a form submitted into a new tab), with
    /// the submitting page's referrer.
    NavigateWithBody { url: Url, post: PostData, referrer: NavigationReferrer },
    Reload,
    Stop,
    GoBack,
    GoForward,
    Resize(Viewport),
    /// The window's outer size and the screen's size, in logical pixels,
    /// for `outerWidth`/`outerHeight` and `screen`.
    Screen {
        window: (f32, f32),
        screen: (f32, f32),
    },
    Scroll { dx: f32, dy: f32 },
    MouseMove { x: f32, y: f32 },
    MouseDown { x: f32, y: f32, button: MouseButton },
    MouseUp { x: f32, y: f32, button: MouseButton },
    /// The pointer left the page area.
    MouseLeave,
    /// Select all of the page's text.
    SelectAll,
    /// Put the selected text on the clipboard, if there is any
    /// (answered with `TabToShell::CopyText`).
    Copy,
    /// Find in page: search for this text, highlight every match and
    /// scroll to the current one. Answered with `TabToShell::FindResult`;
    /// an empty query clears the matches.
    Find { query: String },
    /// Move to the next (or previous) match, wrapping around.
    FindNext { forward: bool },
    /// The find bar closed: drop the matches and their highlights.
    FindClose,
    /// A key went down while the page has the keyboard. Tab and
    /// Shift+Tab move focus through the page's tab order; Enter activates
    /// the focused element. With nothing focused, Tab starts from the
    /// first focusable element and Shift+Tab from the last. `code` is the
    /// physical key's name as the DOM's `KeyboardEvent.code` gives it
    /// (`KeyA`, `Digit1`, `Comma`, `ShiftLeft`); empty when unknown.
    /// `repeat` says the key is auto-repeating while held.
    Key {
        key: Key,
        code: String,
        repeat: bool,
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
    /// A key the page was given went back up: `keyup`, and the click a
    /// held Space makes on a button or checkbox.
    KeyUp {
        key: Key,
        code: String,
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
    /// The modifier keys changed. Mouse events carry the modifiers held
    /// at the time (`MouseEvent.ctrlKey` and friends), so the tab keeps
    /// the current set.
    Modifiers {
        shift: bool,
        ctrl: bool,
        alt: bool,
        meta: bool,
    },
    /// The IME's composition text so far, for the focused text control,
    /// with the cursor (a byte range within `text`) if it reports one.
    /// An empty text clears the composition: the IME cancelled it, or a
    /// commit follows.
    ImePreedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    /// The IME committed `text` into the focused text control.
    ImeCommit { text: String },
    /// Cut the focused text control's selection (answered with
    /// `TabToShell::CopyText`).
    Cut,
    /// Clipboard text to paste into the focused text control.
    Paste { text: String },
    /// The tab's last frame was shown, or will not be (the tab is in the
    /// background, the window hidden or minimized). The tab keeps one
    /// frame on its way at most and paints the next after this, so it
    /// paints at the rate the screen shows frames.
    FrameShown,
    Close,
}

/// A console message's level, as the developer console shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsoleLevel {
    Log,
    Info,
    Warn,
    Error,
}

/// Tab to shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TabToShell {
    /// A message for the developer console: what the page logged, an
    /// uncaught error or unhandled rejection (with its stack), a resource
    /// that failed to load. `source` is where it came from (a script's
    /// address and line, or the resource's address).
    Console { level: ConsoleLevel, text: String, source: Option<String> },
    /// A new document: the console starts empty (Chrome's default).
    ConsoleCleared,
    /// A page was opened at `url`, for the browsing history: a document
    /// committed (a reload and a back/forward included, not an error
    /// page), or the document moved to a new URL itself (a fragment, a
    /// `pushState`).
    Visited { url: Url },
    /// Navigation truth lives in the tab; the shell mirrors it for display.
    StateChanged {
        url: Url,
        title: Option<String>,
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
    },
    /// What the pointer is over calls for a different cursor.
    Cursor(Cursor),
    /// What the pointer is over has a `cursor: url()` image.
    CursorImage(CursorImage),
    /// The user asked for a link in a new tab (middle click), with the
    /// page's referrer for it (none for `noreferrer`).
    OpenInNewTab { url: Url, referrer: NavigationReferrer },
    /// A form submitted into a new navigable (`target=_blank` or a name
    /// no tab has): a new tab, shown, navigated to `url` (with `post` as
    /// the body of a POST) and the page's referrer.
    OpenFormSubmission { url: Url, post: Option<PostData>, referrer: NavigationReferrer },
    /// Text the user copied; the shell owns the clipboard.
    CopyText { text: String },
    /// How many find matches there are and which is current (1-based).
    FindResult { current: Option<usize>, total: usize },
    /// Tab moved past the page's last focusable element (or Shift+Tab
    /// before its first): focus goes to the chrome.
    FocusOut { forward: bool },
    /// The caret of the focused text control, as `(x, y, width, height)`
    /// in logical pixels within the page's viewport, or none when no
    /// text control has focus. The shell enables the IME and places its
    /// candidate window there.
    Caret { rect: Option<(f32, f32, f32, f32)> },
    /// The tab thread panicked and was unwound; the shell shows a crashed page.
    Crashed { message: String },
    Closed,
}

/// Tab to network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TabToNet {
    Fetch {
        id: RequestId,
        url: Url,
        method: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    },
    Cancel { id: RequestId },
}

/// Network to tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetToTab {
    ResponseStart {
        id: RequestId,
        status: u16,
        /// The status line's reason phrase as the server sent it over
        /// HTTP/1; empty over HTTP/2 and later, which carry none
        /// (`Response.statusText`, `XMLHttpRequest.statusText`).
        status_text: String,
        headers: Vec<(String, String)>,
        final_url: Url,
        /// The referrer the last hop determined (Fetch's request
        /// referrer once fetched): a document's `document.referrer`.
        /// Boxed: chunks, the common message, stay small.
        referrer: Option<Box<Url>>,
    },
    ResponseChunk { id: RequestId, bytes: Vec<u8> },
    ResponseEnd { id: RequestId },
    Failed { id: RequestId, error: String },
}
