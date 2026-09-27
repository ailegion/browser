//! Messages between the three layers (see plan/02-architecture.md).
//!
//! Shell, tab, and network code talk only through these types. They are all
//! `serde` types so the transport can change (in-process channels today,
//! anything else later) without touching the layers.
//!
//! Variants here are the Phase 0 skeleton. Add variants as phases need them;
//! never add a second channel that bypasses these.

#![forbid(unsafe_code)]

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
}

/// Shell to tab. Mouse positions are logical pixels within the page's
/// viewport (the tab adds its own scroll offset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShellToTab {
    Navigate { url: Url },
    Reload,
    Stop,
    GoBack,
    GoForward,
    Resize(Viewport),
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
    /// first focusable element and Shift+Tab from the last.
    Key {
        key: Key,
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
    Close,
}

/// Tab to shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TabToShell {
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
    /// The user asked for a link in a new tab (middle click).
    OpenInNewTab { url: Url },
    /// Text the user copied; the shell owns the clipboard.
    CopyText { text: String },
    /// How many find matches there are and which is current (1-based).
    FindResult { current: Option<usize>, total: usize },
    /// Tab moved past the page's last focusable element (or Shift+Tab
    /// before its first): focus goes to the chrome.
    FocusOut { forward: bool },
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
        headers: Vec<(String, String)>,
        final_url: Url,
    },
    ResponseChunk { id: RequestId, bytes: Vec<u8> },
    ResponseEnd { id: RequestId },
    Failed { id: RequestId, error: String },
}
