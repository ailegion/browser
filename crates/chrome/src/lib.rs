//! Browser chrome: the toolbar above the page, drawn with vello, laid out
//! with taffy, text through parley (plan D05). Phase 2 item 6.
//!
//! Block 1: the toolbar frame and the address bar. Its text editing
//! (cursor, selection, word moves, IME preedit) is parley's
//! `PlainEditor`; this crate adds focus, drawing, the URL rules on Enter,
//! and the clipboard hand-off to the shell. The shell owns the window and
//! translates winit events into the small `Key` and mouse vocabulary here;
//! the chrome answers with `ChromeAction`s.
//!
//! Coordinates coming in are logical pixels within the window; the scene
//! drawn is in physical pixels, like the page's.

#![forbid(unsafe_code)]

mod input;

use browser_ipc_types::Cursor;
use parley::{FontContext, LayoutContext};
use taffy::prelude::*;
use url::Url;
use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Color, Fill};

use input::{InputEvent, TextInput, rounded_box};

/// Height of the toolbar in logical pixels. The page starts below it.
pub const TOOLBAR_HEIGHT: f32 = 44.0;
const ADDRESS_HEIGHT: f32 = 30.0;
const FONT_SIZE: f32 = 14.0;

const BAR_BG: Color = Color::from_rgb8(0xf0, 0xf0, 0xf4);
const BAR_LINE: Color = Color::from_rgb8(0xd4, 0xd4, 0xdc);
const BOX_BG: Color = Color::from_rgb8(0xff, 0xff, 0xff);
const BOX_BORDER: Color = Color::from_rgb8(0xc4, 0xc4, 0xcc);
const ACCENT: Color = Color::from_rgb8(0x4a, 0x7b, 0xd8);

/// Parley brush: a color.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Brush(pub [u8; 4]);

/// Keys the chrome understands. Text comes as `Character` with the
/// string the key produced (already dead-key and layout resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Character(String),
    Backspace,
    Delete,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    Enter,
    Escape,
    Tab,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl KeyInput {
    pub fn plain(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            shift: false,
            alt: false,
        }
    }

    pub fn ctrl(key: Key) -> Self {
        Self {
            ctrl: true,
            ..Self::plain(key)
        }
    }

    pub fn shift(key: Key) -> Self {
        Self {
            shift: true,
            ..Self::plain(key)
        }
    }

    pub fn typed(s: &str) -> Self {
        Self::plain(Key::Character(s.to_owned()))
    }
}

/// What the shell has to do after an interaction with the chrome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeAction {
    /// Load this in the current tab.
    Navigate(Url),
    /// Put this on the clipboard.
    CopyText(String),
    /// Read the clipboard and hand it to `paste`.
    RequestPaste,
}

/// The toolbar and everything in it.
pub struct Chrome {
    fonts: FontContext,
    lcx: LayoutContext<Brush>,
    taffy: TaffyTree<()>,
    root: NodeId,
    address_node: NodeId,
    width: f32,
    scale: f32,
    address: TextInput,
    /// The current tab's URL as the tab reported it.
    url: Option<Url>,
    loading: bool,
    /// The pointer is over the address box.
    over_address: bool,
}

impl std::fmt::Debug for Chrome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chrome")
            .field("width", &self.width)
            .field("focused", &self.address.focused)
            .finish()
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::new()
    }
}

impl Chrome {
    pub fn new() -> Self {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let address_node = taffy
            .new_leaf(Style {
                flex_grow: 1.0,
                size: Size {
                    width: auto(),
                    height: length(ADDRESS_HEIGHT),
                },
                ..Default::default()
            })
            .expect("taffy leaf");
        let root = taffy
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    align_items: Some(AlignItems::CENTER),
                    padding: taffy::Rect {
                        left: length(8.0),
                        right: length(8.0),
                        top: length(0.0),
                        bottom: length(0.0),
                    },
                    gap: Size {
                        width: length(8.0),
                        height: length(0.0),
                    },
                    size: Size {
                        width: percent(1.0),
                        height: length(TOOLBAR_HEIGHT),
                    },
                    ..Default::default()
                },
                &[address_node],
            )
            .expect("taffy root");
        let mut chrome = Self {
            fonts: FontContext::new(),
            lcx: LayoutContext::new(),
            taffy,
            root,
            address_node,
            width: 0.0,
            scale: 1.0,
            address: TextInput::new(FONT_SIZE, "Enter an address"),
            url: None,
            loading: false,
            over_address: false,
        };
        chrome.resize(800.0, 1.0);
        chrome
    }

    /// Toolbar height in logical pixels.
    pub fn height(&self) -> f32 {
        TOOLBAR_HEIGHT
    }

    /// The window's logical width and scale factor changed.
    pub fn resize(&mut self, width: f32, scale: f32) {
        self.width = width.max(0.0);
        self.scale = scale.max(0.01);
        self.address.set_scale(self.scale);
        let _ = self.taffy.compute_layout(
            self.root,
            Size {
                width: AvailableSpace::Definite(self.width),
                height: AvailableSpace::Definite(TOOLBAR_HEIGHT),
            },
        );
        if let Ok(l) = self.taffy.layout(self.address_node) {
            self.address.rect = Rect::new(
                l.location.x as f64,
                l.location.y as f64,
                (l.location.x + l.size.width) as f64,
                (l.location.y + l.size.height) as f64,
            );
        }
    }

    /// The address box in logical pixels, for tests and the shell.
    pub fn address_rect(&self) -> (f32, f32, f32, f32) {
        let r = self.address.rect;
        (r.x0 as f32, r.y0 as f32, r.width() as f32, r.height() as f32)
    }

    /// The tab reported a URL. Shown unless the user is editing.
    pub fn set_url(&mut self, url: &Url) {
        self.url = Some(url.clone());
        if !self.address.focused {
            self.address.set_text(display_url(url).as_str());
        }
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    pub fn address_text(&self) -> String {
        self.address.text()
    }

    /// The address bar has keyboard focus.
    pub fn has_focus(&self) -> bool {
        self.address.focused
    }

    /// Focus the address bar with its text selected (Ctrl+L).
    pub fn focus_address(&mut self) {
        self.address.focused = true;
        self.address.select_all(&mut self.fonts, &mut self.lcx);
    }

    /// Keyboard focus goes back to the page; an edit in progress is
    /// kept in the box, as browsers do, until the tab reports a URL.
    pub fn blur(&mut self) {
        self.address.blur(&mut self.fonts, &mut self.lcx);
    }

    /// The area the IME should keep clear, in logical pixels, when the
    /// address bar is focused.
    pub fn ime_cursor_area(&mut self) -> Option<(f32, f32, f32, f32)> {
        self.address
            .focused
            .then(|| self.address.ime_cursor_area(&mut self.fonts, &mut self.lcx))
    }

    /// Whether a point is on the chrome rather than the page.
    pub fn contains(&self, _x: f32, y: f32) -> bool {
        y < TOOLBAR_HEIGHT
    }

    pub fn mouse_move(&mut self, x: f32, y: f32) -> Cursor {
        self.over_address = self.address.contains(x, y);
        self.address.mouse_move(&mut self.fonts, &mut self.lcx, x, y);
        if self.over_address { Cursor::Text } else { Cursor::Default }
    }

    pub fn mouse_leave(&mut self) {
        self.over_address = false;
    }

    /// Primary button down at a point on the chrome.
    pub fn mouse_down(&mut self, x: f32, y: f32, shift: bool) -> Vec<ChromeAction> {
        if self.address.contains(x, y) {
            self.address.mouse_down(&mut self.fonts, &mut self.lcx, x, y, shift);
        } else if self.address.focused {
            self.blur();
        }
        Vec::new()
    }

    pub fn mouse_up(&mut self, _x: f32, _y: f32) {
        self.address.mouse_up();
    }

    /// A key while the chrome has focus.
    pub fn key(&mut self, input: KeyInput) -> Vec<ChromeAction> {
        if !self.address.focused {
            return Vec::new();
        }
        let event = self.address.key(&mut self.fonts, &mut self.lcx, &input);
        let mut actions = Vec::new();
        match event {
            Some(InputEvent::Submit(text)) => {
                if let Some(url) = url_from_input(&text) {
                    self.address.set_text(display_url(&url).as_str());
                    self.blur();
                    actions.push(ChromeAction::Navigate(url));
                }
            }
            Some(InputEvent::Cancel) => {
                if let Some(url) = &self.url {
                    self.address.set_text(display_url(url).as_str());
                } else {
                    self.address.set_text("");
                }
                self.blur();
            }
            Some(InputEvent::Blur) => self.blur(),
            Some(InputEvent::Copy(text)) => actions.push(ChromeAction::CopyText(text)),
            Some(InputEvent::RequestPaste) => actions.push(ChromeAction::RequestPaste),
            None => {}
        }
        actions
    }

    /// Clipboard text for a `RequestPaste`.
    pub fn paste(&mut self, text: &str) {
        if self.address.focused {
            self.address.paste(&mut self.fonts, &mut self.lcx, text);
        }
    }

    pub fn ime_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        if self.address.focused {
            self.address.ime_preedit(&mut self.fonts, &mut self.lcx, text, cursor);
        }
    }

    pub fn ime_commit(&mut self, text: &str) {
        if self.address.focused {
            self.address.ime_commit(&mut self.fonts, &mut self.lcx, text);
        }
    }

    /// Draw the toolbar at the top of `scene`, in physical pixels.
    pub fn draw(&mut self, scene: &mut Scene) {
        let s = self.scale as f64;
        let w = self.width as f64 * s;
        let h = TOOLBAR_HEIGHT as f64 * s;
        scene.fill(Fill::NonZero, Affine::IDENTITY, BAR_BG, None, &Rect::new(0.0, 0.0, w, h));
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            BAR_LINE,
            None,
            &Rect::new(0.0, h - s.max(1.0), w, h),
        );

        let r = self.address.rect;
        let box_rect = Rect::new(r.x0 * s, r.y0 * s, r.x1 * s, r.y1 * s);
        let (border, width) = if self.address.focused {
            (ACCENT, 2.0 * s)
        } else {
            (BOX_BORDER, s.max(1.0))
        };
        rounded_box(scene, box_rect, 8.0 * s, BOX_BG, border, width);
        self.address.draw(&mut self.fonts, &mut self.lcx, scene);

        if self.loading {
            // A thin accent line along the bottom of the address box.
            let line = Rect::new(box_rect.x0 + 8.0 * s, box_rect.y1 - 3.0 * s, box_rect.x1 - 8.0 * s, box_rect.y1 - s);
            scene.fill(Fill::NonZero, Affine::IDENTITY, ACCENT, None, &line);
        }
    }
}

/// What the address bar shows for a URL: the URL itself, except that
/// `about:blank` shows as nothing.
fn display_url(url: &Url) -> String {
    if url.as_str() == "about:blank" {
        String::new()
    } else {
        url.to_string()
    }
}

/// Turn what the user typed into a URL. A full URL is taken as is; a
/// bare host or host and path gets `https://`. Anything else is not a
/// URL, and with no search engine set (plan O6) nothing happens.
pub fn url_from_input(text: &str) -> Option<Url> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(url) = Url::parse(text)
        && matches!(url.scheme(), "http" | "https" | "data" | "about" | "file")
        && (url.scheme() != "http" && url.scheme() != "https" || url.host().is_some())
    {
        return Some(url);
    }
    if text.contains(char::is_whitespace) {
        return None;
    }
    let host = text.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    let looks_like_host = host == "localhost"
        || host.parse::<std::net::IpAddr>().is_ok()
        || (host.contains('.') && !host.starts_with('.') && !host.ends_with('.'));
    if !looks_like_host {
        return None;
    }
    Url::parse(&format!("https://{text}")).ok().filter(|u| u.host().is_some())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn chrome() -> Chrome {
        let mut c = Chrome::new();
        c.resize(800.0, 1.0);
        c
    }

    fn type_str(c: &mut Chrome, s: &str) -> Vec<ChromeAction> {
        let mut out = Vec::new();
        for ch in s.chars() {
            out.extend(c.key(KeyInput::typed(&ch.to_string())));
        }
        out
    }

    #[test]
    fn layout_puts_the_address_box_in_the_toolbar() {
        let c = chrome();
        let (x, y, w, h) = c.address_rect();
        assert_eq!(x, 8.0);
        assert_eq!(w, 800.0 - 16.0);
        assert_eq!(h, ADDRESS_HEIGHT);
        assert_eq!(y, (TOOLBAR_HEIGHT - ADDRESS_HEIGHT) / 2.0);
        assert!(c.contains(100.0, 10.0) && !c.contains(100.0, TOOLBAR_HEIGHT + 1.0));
    }

    #[test]
    fn typing_and_enter_navigate() {
        let mut c = chrome();
        assert!(c.key(KeyInput::typed("x")).is_empty(), "unfocused: keys go nowhere");
        c.focus_address();
        assert!(c.has_focus());
        assert!(type_str(&mut c, "example.com/a b").is_empty());
        assert_eq!(c.address_text(), "example.com/a b");
        assert!(c.key(KeyInput::plain(Key::Enter)).is_empty(), "not a URL, nothing happens");
        assert!(c.has_focus());
        c.focus_address();
        type_str(&mut c, "example.com/path?q=1");
        let actions = c.key(KeyInput::plain(Key::Enter));
        assert_eq!(
            actions,
            vec![ChromeAction::Navigate(Url::parse("https://example.com/path?q=1").unwrap())]
        );
        assert!(!c.has_focus(), "focus goes to the page after Enter");
        assert_eq!(c.address_text(), "https://example.com/path?q=1");
    }

    #[test]
    fn escape_restores_the_tabs_url_and_set_url_waits_while_editing() {
        let mut c = chrome();
        let url = Url::parse("https://a.example/").unwrap();
        c.set_url(&url);
        assert_eq!(c.address_text(), "https://a.example/");
        c.focus_address();
        type_str(&mut c, "typed");
        assert_eq!(c.address_text(), "typed", "focus selected all, typing replaced it");
        let other = Url::parse("https://b.example/").unwrap();
        c.set_url(&other);
        assert_eq!(c.address_text(), "typed", "an edit in progress is kept");
        c.key(KeyInput::plain(Key::Escape));
        assert!(!c.has_focus());
        assert_eq!(c.address_text(), "https://b.example/");
        c.set_url(&Url::parse("about:blank").unwrap());
        assert_eq!(c.address_text(), "");
    }

    #[test]
    fn editing_keys_selection_and_clipboard() {
        let mut c = chrome();
        c.focus_address();
        type_str(&mut c, "abc def");
        c.key(KeyInput::plain(Key::Backspace));
        assert_eq!(c.address_text(), "abc de");
        c.key(KeyInput::ctrl(Key::Backspace));
        assert_eq!(c.address_text(), "abc ");
        c.key(KeyInput::plain(Key::Home));
        c.key(KeyInput::plain(Key::Delete));
        assert_eq!(c.address_text(), "bc ");
        c.key(KeyInput::plain(Key::End));
        type_str(&mut c, "xyz");
        assert_eq!(c.address_text(), "bc xyz");
        // Select the last word with shift+ctrl+left, copy it, cut it.
        c.key(KeyInput {
            key: Key::ArrowLeft,
            ctrl: true,
            shift: true,
            alt: false,
        });
        assert_eq!(c.key(KeyInput::ctrl(Key::Character("c".into()))), vec![ChromeAction::CopyText("xyz".into())]);
        assert_eq!(c.key(KeyInput::ctrl(Key::Character("x".into()))), vec![ChromeAction::CopyText("xyz".into())]);
        assert_eq!(c.address_text(), "bc ");
        assert_eq!(c.key(KeyInput::ctrl(Key::Character("v".into()))), vec![ChromeAction::RequestPaste]);
        c.paste("two\nlines");
        assert_eq!(c.address_text(), "bc twolines", "pasted text is one line");
        c.key(KeyInput::ctrl(Key::Character("a".into())));
        type_str(&mut c, "q");
        assert_eq!(c.address_text(), "q");
        c.key(KeyInput::shift(Key::Home));
        assert_eq!(c.key(KeyInput::ctrl(Key::Character("c".into()))), vec![ChromeAction::CopyText("q".into())]);
        c.key(KeyInput::plain(Key::Tab));
        assert!(!c.has_focus());
    }

    #[test]
    fn ime_preedit_is_shown_but_not_part_of_the_text_until_committed() {
        let mut c = chrome();
        c.focus_address();
        type_str(&mut c, "a");
        c.ime_preedit("に", Some((0, 3)));
        assert_eq!(c.address_text(), "a", "preedit is not text yet");
        assert!(c.ime_cursor_area().is_some());
        c.ime_commit("日");
        assert_eq!(c.address_text(), "a日");
        c.ime_preedit("x", None);
        c.ime_preedit("", None);
        assert_eq!(c.address_text(), "a日");
        c.key(KeyInput::plain(Key::Escape));
        assert!(c.ime_cursor_area().is_none());
    }

    #[test]
    fn mouse_focuses_selects_all_then_places_the_caret() {
        let mut c = chrome();
        c.set_url(&Url::parse("https://example.com/").unwrap());
        let (x, y, _, h) = c.address_rect();
        assert_eq!(c.mouse_move(x + 5.0, y + h / 2.0), Cursor::Text);
        assert_eq!(c.mouse_move(x + 5.0, 2.0), Cursor::Default);
        c.mouse_down(x + 5.0, y + h / 2.0, false);
        c.mouse_up(x + 5.0, y + h / 2.0);
        assert!(c.has_focus());
        type_str(&mut c, "z");
        assert_eq!(c.address_text(), "z", "first click selected everything");
        // Draw once so the layout exists, then click at the far left: the
        // caret lands before the z and typing goes there.
        let mut scene = Scene::new();
        c.draw(&mut scene);
        c.mouse_down(x + 11.0, y + h / 2.0, false);
        c.mouse_up(x + 11.0, y + h / 2.0);
        type_str(&mut c, "a");
        assert_eq!(c.address_text(), "az");
        // A click on the bar outside the box takes focus away.
        c.mouse_down(x + 5.0, 2.0, false);
        assert!(!c.has_focus());
    }

    #[test]
    fn draws_focused_unfocused_empty_and_loading_without_panicking() {
        let mut c = chrome();
        let mut scene = Scene::new();
        c.draw(&mut scene);
        c.set_loading(true);
        c.set_url(&Url::parse("https://example.com/a/very/long/path/that/goes/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on").unwrap());
        c.resize(300.0, 2.0);
        c.draw(&mut scene);
        c.focus_address();
        c.key(KeyInput::plain(Key::End));
        c.draw(&mut scene);
        c.ime_preedit("あい", Some((0, 6)));
        c.draw(&mut scene);
    }

    #[test]
    fn input_to_url_rules() {
        let u = |s: &str| url_from_input(s).map(|u| u.to_string());
        assert_eq!(u("example.com").as_deref(), Some("https://example.com/"));
        assert_eq!(u("  https://example.com/x?y#z ").as_deref(), Some("https://example.com/x?y#z"));
        assert_eq!(u("localhost:8080/p").as_deref(), Some("https://localhost:8080/p"));
        assert_eq!(u("127.0.0.1").as_deref(), Some("https://127.0.0.1/"));
        assert_eq!(u("about:blank").as_deref(), Some("about:blank"));
        assert_eq!(u("data:text/html,hi").as_deref(), Some("data:text/html,hi"));
        assert_eq!(u("hello world"), None);
        assert_eq!(u("hello"), None);
        assert_eq!(u(""), None);
        assert_eq!(u("javascript:alert(1)"), None);
        assert_eq!(u("http://"), None);
    }
}
