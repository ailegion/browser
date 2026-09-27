//! Browser chrome: the tab strip and toolbar above the page, drawn with
//! vello, laid out with taffy, text through parley (plan D05). Phase 2
//! item 6.
//!
//! The address bar's text editing (cursor, selection, word moves, IME
//! preedit) is parley's `PlainEditor`; this crate adds focus, drawing,
//! the URL rules on Enter and the clipboard hand-off to the shell. The
//! shell owns the window and translates winit events into the small
//! `Key` and mouse vocabulary here; the chrome answers with
//! `ChromeAction`s. Navigation truth stays in the tabs: the shell mirrors
//! titles, URLs and history state into the chrome for display.
//!
//! Time-driven parts (the loading animation, tooltips) run off
//! `next_wake` and `tick`: the shell waits until the next wake, ticks, and
//! redraws if asked. The page's scrollbar is a widget here
//! (`scrollbar`) that the tab drives.
//!
//! Coordinates coming in are logical pixels within the window; the scene
//! drawn is in physical pixels, like the page's.

#![forbid(unsafe_code)]

mod input;
mod menu;
pub mod scrollbar;
mod widgets;

use std::time::{Duration, Instant};

use browser_ipc_types::{Cursor, MouseButton, TabId};
use parley::{FontContext, LayoutContext};
use taffy::prelude::*;
use url::Url;
use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};

use input::{InputEvent, TextInput, rounded_box};
use menu::{Menu, MenuAction, MenuItem};
use widgets::{Button, Icon, TextLine, draw_icon, draw_progress, draw_spinner, draw_text, draw_tooltip, scale_rect};

/// Height of the tab strip in logical pixels.
pub const TABSTRIP_HEIGHT: f32 = 34.0;
/// Height of the toolbar row in logical pixels.
pub const TOOLBAR_HEIGHT: f32 = 44.0;
const ADDRESS_HEIGHT: f32 = 30.0;
const BUTTON_SIZE: f32 = 32.0;
const TAB_HEIGHT: f32 = 30.0;
const FONT_SIZE: f32 = 14.0;
const TAB_FONT_SIZE: f32 = 13.0;
/// Space the security indicator takes at the left of the address box.
const INDICATOR_WIDTH: f32 = 26.0;
/// Hover this long before a tooltip shows.
const TOOLTIP_DELAY: Duration = Duration::from_millis(600);
/// Frame interval for the loading animation.
const ANIMATION_FRAME: Duration = Duration::from_millis(33);

const STRIP_BG: Color = Color::from_rgb8(0xe2, 0xe2, 0xe8);
const BAR_BG: Color = Color::from_rgb8(0xf0, 0xf0, 0xf4);
const BAR_LINE: Color = Color::from_rgb8(0xd4, 0xd4, 0xdc);
const BOX_BG: Color = Color::from_rgb8(0xff, 0xff, 0xff);
const BOX_BORDER: Color = Color::from_rgb8(0xc4, 0xc4, 0xcc);
const ACCENT: Color = Color::from_rgb8(0x4a, 0x7b, 0xd8);
const TAB_HOVER: Color = Color::from_rgb8(0xea, 0xea, 0xf0);
const TEXT: [u8; 4] = [0x30, 0x30, 0x38, 0xff];
const TEXT_DIM: [u8; 4] = [0x70, 0x70, 0x78, 0xff];

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
    ArrowUp,
    ArrowDown,
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
    Back,
    Forward,
    Reload,
    Stop,
    NewTab,
    SelectTab(TabId),
    CloseTab(TabId),
}

/// What the tab strip shows for one tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub id: TabId,
    pub title: String,
    pub loading: bool,
}

/// Things the pointer can be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Back,
    Forward,
    Reload,
    NewTab,
    Menu,
    Address,
    Tab(TabId),
    TabClose(TabId),
}

struct TabSlot {
    info: TabInfo,
    node: NodeId,
    /// Logical pixels.
    rect: Rect,
}

impl TabSlot {
    /// The close mark at the tab's right end.
    fn close_rect(&self) -> Rect {
        let size = 18.0;
        let x1 = self.rect.x1 - 6.0;
        let cy = (self.rect.y0 + self.rect.y1) / 2.0;
        Rect::new(x1 - size, cy - size / 2.0, x1, cy + size / 2.0)
    }
}

/// The tab strip and toolbar and everything in them.
pub struct Chrome {
    fonts: FontContext,
    lcx: LayoutContext<Brush>,
    taffy: TaffyTree<()>,
    root: NodeId,
    strip: NodeId,
    address_node: NodeId,
    back_node: NodeId,
    forward_node: NodeId,
    reload_node: NodeId,
    menu_node: NodeId,
    new_tab_node: NodeId,
    width: f32,
    scale: f32,
    address: TextInput,
    back: Button,
    forward: Button,
    reload: Button,
    menu_button: Button,
    new_tab: Button,
    tabs: Vec<TabSlot>,
    current: Option<TabId>,
    /// The current tab's URL as the tab reported it.
    url: Option<Url>,
    loading: bool,
    /// When the current tab started loading, for the animation.
    loading_since: Option<Instant>,
    menu: Option<Menu>,
    hover: Option<Part>,
    /// When the pointer arrived on `hover`, and where it is.
    hover_since: Option<Instant>,
    mouse: (f32, f32),
    tooltip_shown: bool,
    pressed: Option<Part>,
    /// Something changed that a redraw must show.
    dirty: bool,
}

impl std::fmt::Debug for Chrome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chrome")
            .field("width", &self.width)
            .field("tabs", &self.tabs.len())
            .field("focused", &self.address.focused)
            .field("menu", &self.menu.is_some())
            .finish()
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::new()
    }
}

fn square(side: f32) -> Style {
    Style {
        size: Size {
            width: length(side),
            height: length(side),
        },
        flex_shrink: 0.0,
        ..Default::default()
    }
}

fn tab_style() -> Style {
    Style {
        flex_grow: 1.0,
        flex_shrink: 1.0,
        flex_basis: length(200.0),
        min_size: Size {
            width: length(48.0),
            height: auto(),
        },
        max_size: Size {
            width: length(220.0),
            height: auto(),
        },
        size: Size {
            width: auto(),
            height: length(TAB_HEIGHT),
        },
        ..Default::default()
    }
}

impl Chrome {
    pub fn new() -> Self {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let leaf = |taffy: &mut TaffyTree<()>, style: Style| taffy.new_leaf(style).expect("taffy leaf");
        let back_node = leaf(&mut taffy, square(BUTTON_SIZE));
        let forward_node = leaf(&mut taffy, square(BUTTON_SIZE));
        let reload_node = leaf(&mut taffy, square(BUTTON_SIZE));
        let menu_node = leaf(&mut taffy, square(BUTTON_SIZE));
        let address_node = leaf(
            &mut taffy,
            Style {
                flex_grow: 1.0,
                size: Size {
                    width: auto(),
                    height: length(ADDRESS_HEIGHT),
                },
                ..Default::default()
            },
        );
        let toolbar = taffy
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    align_items: Some(AlignItems::CENTER),
                    padding: taffy::Rect {
                        left: length(6.0),
                        right: length(6.0),
                        top: length(0.0),
                        bottom: length(0.0),
                    },
                    gap: Size {
                        width: length(4.0),
                        height: length(0.0),
                    },
                    size: Size {
                        width: percent(1.0),
                        height: length(TOOLBAR_HEIGHT),
                    },
                    ..Default::default()
                },
                &[back_node, forward_node, reload_node, address_node, menu_node],
            )
            .expect("taffy toolbar");
        let new_tab_node = leaf(
            &mut taffy,
            Style {
                margin: taffy::Rect {
                    left: length(4.0),
                    right: length(0.0),
                    top: length(0.0),
                    bottom: length(2.0),
                },
                ..square(28.0)
            },
        );
        let strip = taffy
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    align_items: Some(AlignItems::FLEX_END),
                    padding: taffy::Rect {
                        left: length(8.0),
                        right: length(8.0),
                        top: length(0.0),
                        bottom: length(0.0),
                    },
                    gap: Size {
                        width: length(2.0),
                        height: length(0.0),
                    },
                    size: Size {
                        width: percent(1.0),
                        height: length(TABSTRIP_HEIGHT),
                    },
                    ..Default::default()
                },
                &[new_tab_node],
            )
            .expect("taffy strip");
        let root = taffy
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Column,
                    size: Size {
                        width: percent(1.0),
                        height: length(TABSTRIP_HEIGHT + TOOLBAR_HEIGHT),
                    },
                    ..Default::default()
                },
                &[strip, toolbar],
            )
            .expect("taffy root");
        let mut chrome = Self {
            fonts: FontContext::new(),
            lcx: LayoutContext::new(),
            taffy,
            root,
            strip,
            address_node,
            back_node,
            forward_node,
            reload_node,
            menu_node,
            new_tab_node,
            width: 0.0,
            scale: 1.0,
            address: TextInput::new(FONT_SIZE, "Enter an address"),
            back: Button::new(Icon::Back),
            forward: Button::new(Icon::Forward),
            reload: Button::new(Icon::Reload),
            menu_button: Button::new(Icon::Menu),
            new_tab: Button::new(Icon::Plus),
            tabs: Vec::new(),
            current: None,
            url: None,
            loading: false,
            loading_since: None,
            menu: None,
            hover: None,
            hover_since: None,
            mouse: (0.0, 0.0),
            tooltip_shown: false,
            pressed: None,
            dirty: true,
        };
        chrome.back.enabled = false;
        chrome.forward.enabled = false;
        chrome.resize(800.0, 1.0);
        chrome
    }

    /// Total chrome height in logical pixels. The page starts below it.
    pub fn height(&self) -> f32 {
        TABSTRIP_HEIGHT + TOOLBAR_HEIGHT
    }

    /// The window's logical width and scale factor changed.
    pub fn resize(&mut self, width: f32, scale: f32) {
        self.width = width.max(0.0);
        self.scale = scale.max(0.01);
        self.address.set_scale(self.scale);
        self.menu = None;
        self.relayout();
    }

    fn relayout(&mut self) {
        let _ = self.taffy.compute_layout(
            self.root,
            Size {
                width: AvailableSpace::Definite(self.width),
                height: AvailableSpace::Definite(self.height()),
            },
        );
        let rect_of = |taffy: &TaffyTree<()>, node: NodeId, parent_offset: (f32, f32)| -> Rect {
            let l = taffy.layout(node).copied().unwrap_or_default();
            let x = l.location.x + parent_offset.0;
            let y = l.location.y + parent_offset.1;
            Rect::new(x as f64, y as f64, (x + l.size.width) as f64, (y + l.size.height) as f64)
        };
        // Children are located within their parent; the strip sits at the
        // top and the toolbar below it.
        let toolbar_top = TABSTRIP_HEIGHT;
        self.back.rect = rect_of(&self.taffy, self.back_node, (0.0, toolbar_top));
        self.forward.rect = rect_of(&self.taffy, self.forward_node, (0.0, toolbar_top));
        self.reload.rect = rect_of(&self.taffy, self.reload_node, (0.0, toolbar_top));
        self.menu_button.rect = rect_of(&self.taffy, self.menu_node, (0.0, toolbar_top));
        self.address.rect = rect_of(&self.taffy, self.address_node, (0.0, toolbar_top));
        self.new_tab.rect = rect_of(&self.taffy, self.new_tab_node, (0.0, 0.0));
        for tab in &mut self.tabs {
            tab.rect = rect_of(&self.taffy, tab.node, (0.0, 0.0));
        }
        self.dirty = true;
    }

    /// The address box in logical pixels, for tests and the shell.
    pub fn address_rect(&self) -> (f32, f32, f32, f32) {
        let r = self.address.rect;
        (r.x0 as f32, r.y0 as f32, r.width() as f32, r.height() as f32)
    }

    /// Centers of the buttons, for tests: back, forward, reload, new tab,
    /// menu.
    pub fn button_centers(&self) -> [(f32, f32); 5] {
        let c = |r: Rect| (((r.x0 + r.x1) / 2.0) as f32, ((r.y0 + r.y1) / 2.0) as f32);
        [
            c(self.back.rect),
            c(self.forward.rect),
            c(self.reload.rect),
            c(self.new_tab.rect),
            c(self.menu_button.rect),
        ]
    }

    /// Center of a tab in the strip, and of its close mark.
    pub fn tab_centers(&self, id: TabId) -> Option<((f32, f32), (f32, f32))> {
        let tab = self.tabs.iter().find(|t| t.info.id == id)?;
        let c = |r: Rect| (((r.x0 + r.x1) / 2.0) as f32, ((r.y0 + r.y1) / 2.0) as f32);
        Some((c(tab.rect), c(tab.close_rect())))
    }

    /// The tabs to show, in order, and which is current.
    pub fn set_tabs(&mut self, tabs: Vec<TabInfo>, current: Option<TabId>) {
        let same_set = self.tabs.len() == tabs.len() && self.tabs.iter().zip(&tabs).all(|(a, b)| a.info.id == b.id);
        if same_set {
            for (slot, info) in self.tabs.iter_mut().zip(tabs) {
                slot.info = info;
            }
        } else {
            for slot in self.tabs.drain(..) {
                let _ = self.taffy.remove(slot.node);
            }
            for info in tabs {
                let node = self.taffy.new_leaf(tab_style()).expect("taffy leaf");
                self.tabs.push(TabSlot {
                    info,
                    node,
                    rect: Rect::ZERO,
                });
            }
            let mut children: Vec<NodeId> = self.tabs.iter().map(|t| t.node).collect();
            children.push(self.new_tab_node);
            let _ = self.taffy.set_children(self.strip, &children);
            self.relayout();
        }
        self.current = current;
        self.dirty = true;
    }

    /// The current tab's history state, for the back and forward buttons.
    pub fn set_nav_state(&mut self, can_go_back: bool, can_go_forward: bool) {
        if self.back.enabled != can_go_back || self.forward.enabled != can_go_forward {
            self.back.enabled = can_go_back;
            self.forward.enabled = can_go_forward;
            self.dirty = true;
        }
    }

    /// The tab reported a URL. Shown unless the user is editing.
    pub fn set_url(&mut self, url: &Url) {
        self.url = Some(url.clone());
        if !self.address.focused {
            self.address.set_text(display_url(url).as_str());
        }
        self.address.pad_left = if security_icon(Some(url)).is_some() {
            INDICATOR_WIDTH + 4.0
        } else {
            10.0
        };
        self.dirty = true;
    }

    pub fn set_loading(&mut self, loading: bool) {
        if self.loading != loading {
            self.loading = loading;
            self.loading_since = loading.then(Instant::now);
            self.reload.icon = if loading { Icon::Stop } else { Icon::Reload };
            self.dirty = true;
        }
    }

    pub fn address_text(&self) -> String {
        self.address.text()
    }

    /// Whether the current URL is served over HTTPS (the lock).
    pub fn is_secure(&self) -> bool {
        security_icon(self.url.as_ref()) == Some(Icon::LockClosed)
    }

    /// The address bar has keyboard focus.
    pub fn has_focus(&self) -> bool {
        self.address.focused
    }

    /// The chrome wants the keyboard: the address bar is focused or a
    /// menu is open.
    pub fn wants_keys(&self) -> bool {
        self.address.focused || self.menu.is_some()
    }

    /// A menu is open.
    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    pub fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            self.dirty = true;
        }
    }

    /// Focus the address bar with its text selected (Ctrl+L).
    pub fn focus_address(&mut self) {
        self.close_menu();
        self.address.focused = true;
        self.address.select_all(&mut self.fonts, &mut self.lcx);
        self.dirty = true;
    }

    /// Keyboard focus goes back to the page; an edit in progress is
    /// kept in the box, as browsers do, until the tab reports a URL.
    pub fn blur(&mut self) {
        if self.address.focused {
            self.dirty = true;
        }
        self.address.blur(&mut self.fonts, &mut self.lcx);
    }

    /// Whether something changed since the last draw. Clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// When the shell should call `tick` next: the next animation frame
    /// while loading, or when a tooltip is due.
    pub fn next_wake(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let animating = self.loading || self.tabs.iter().any(|t| t.info.loading);
        if animating {
            next = Some(Instant::now() + ANIMATION_FRAME);
        }
        if !self.tooltip_shown
            && let Some(since) = self.hover_since
            && self.hover.is_some_and(|p| tooltip_for(p).is_some())
        {
            let due = since + TOOLTIP_DELAY;
            next = Some(next.map_or(due, |n| n.min(due)));
        }
        next
    }

    /// Time passed: returns whether a redraw is needed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut redraw = self.loading || self.tabs.iter().any(|t| t.info.loading);
        if !self.tooltip_shown
            && let Some(since) = self.hover_since
            && now >= since + TOOLTIP_DELAY
            && self.hover.is_some_and(|p| tooltip_for(p).is_some())
        {
            self.tooltip_shown = true;
            redraw = true;
        }
        if redraw {
            self.dirty = true;
        }
        redraw
    }

    /// The area the IME should keep clear, in logical pixels, when the
    /// address bar is focused.
    pub fn ime_cursor_area(&mut self) -> Option<(f32, f32, f32, f32)> {
        self.address
            .focused
            .then(|| self.address.ime_cursor_area(&mut self.fonts, &mut self.lcx))
    }

    /// Whether a point is on the chrome rather than the page. With a menu
    /// open the chrome takes every point, to close it on a click outside.
    pub fn contains(&self, _x: f32, y: f32) -> bool {
        self.menu.is_some() || y < self.height()
    }

    fn part_at(&self, x: f32, y: f32) -> Option<Part> {
        if self.address.contains(x, y) {
            return Some(Part::Address);
        }
        if self.back.contains(x, y) {
            return Some(Part::Back);
        }
        if self.forward.contains(x, y) {
            return Some(Part::Forward);
        }
        if self.reload.contains(x, y) {
            return Some(Part::Reload);
        }
        if self.menu_button.contains(x, y) {
            return Some(Part::Menu);
        }
        if self.new_tab.contains(x, y) {
            return Some(Part::NewTab);
        }
        for tab in &self.tabs {
            if tab.close_rect().contains((x as f64, y as f64)) {
                return Some(Part::TabClose(tab.info.id));
            }
            if tab.rect.contains((x as f64, y as f64)) {
                return Some(Part::Tab(tab.info.id));
            }
        }
        None
    }

    fn tooltip_text(&self, part: Part) -> Option<String> {
        match part {
            Part::Reload if self.loading => Some("Stop loading".to_owned()),
            Part::Tab(id) => self
                .tabs
                .iter()
                .find(|t| t.info.id == id)
                .map(|t| t.info.title.clone())
                .filter(|t| !t.is_empty()),
            other => tooltip_for(other).map(str::to_owned),
        }
    }

    pub fn mouse_move(&mut self, x: f32, y: f32) -> Cursor {
        self.mouse = (x, y);
        if let Some(menu) = &mut self.menu {
            let over = menu.item_at(x, y);
            if over != menu.hover {
                menu.hover = over;
                menu.keyed = None;
                self.dirty = true;
            }
            return if over.is_some() { Cursor::Pointer } else { Cursor::Default };
        }
        let part = self.part_at(x, y);
        if part != self.hover {
            self.hover = part;
            self.hover_since = part.map(|_| Instant::now());
            self.tooltip_shown = false;
            self.dirty = true;
        } else if self.tooltip_shown {
            // The tooltip follows the pointer.
            self.dirty = true;
        }
        self.address.mouse_move(&mut self.fonts, &mut self.lcx, x, y);
        if self.pressed == Some(Part::Address) {
            self.dirty = true;
        }
        match part {
            Some(Part::Address) => Cursor::Text,
            Some(Part::Back) if self.back.enabled => Cursor::Pointer,
            Some(Part::Forward) if self.forward.enabled => Cursor::Pointer,
            Some(Part::Reload | Part::NewTab | Part::Menu | Part::Tab(_) | Part::TabClose(_)) => Cursor::Pointer,
            _ => Cursor::Default,
        }
    }

    pub fn mouse_leave(&mut self) {
        if self.hover.is_some() || self.tooltip_shown {
            self.hover = None;
            self.hover_since = None;
            self.tooltip_shown = false;
            self.dirty = true;
        }
    }

    /// A button went down at a point on the chrome.
    pub fn mouse_down(&mut self, x: f32, y: f32, button: MouseButton, shift: bool) -> Vec<ChromeAction> {
        self.dirty = true;
        self.tooltip_shown = false;
        self.hover_since = None;
        if let Some(menu) = &self.menu {
            // A click on a row chooses it on release; anywhere else closes.
            if !menu.contains(x, y) {
                self.close_menu();
                // The menu button itself toggles: swallow the press.
                if self.menu_button.contains(x, y) {
                    self.pressed = None;
                    return Vec::new();
                }
            }
            return Vec::new();
        }
        let part = self.part_at(x, y);
        match (button, part) {
            (MouseButton::Left, Some(Part::Address)) => {
                self.address.mouse_down(&mut self.fonts, &mut self.lcx, x, y, shift);
                self.pressed = Some(Part::Address);
            }
            (MouseButton::Left, Some(p)) => {
                if self.address.focused {
                    self.blur();
                }
                self.pressed = Some(p);
            }
            (MouseButton::Middle, Some(Part::Tab(id) | Part::TabClose(id))) => {
                return vec![ChromeAction::CloseTab(id)];
            }
            _ => {
                if self.address.focused {
                    self.blur();
                }
            }
        }
        Vec::new()
    }

    /// The button came up. Buttons act on release over the part pressed.
    pub fn mouse_up(&mut self, x: f32, y: f32, button: MouseButton) -> Vec<ChromeAction> {
        let pressed = self.pressed.take();
        self.address.mouse_up();
        self.dirty = true;
        if button != MouseButton::Left {
            return Vec::new();
        }
        if let Some(menu) = &self.menu {
            let chosen = menu.item_at(x, y).and_then(|i| menu.items[i].action);
            return match chosen {
                Some(action) => {
                    self.close_menu();
                    self.menu_action(action)
                }
                None => Vec::new(),
            };
        }
        let released = self.part_at(x, y);
        match pressed {
            Some(p) if Some(p) == released => match p {
                Part::Back if self.back.enabled => vec![ChromeAction::Back],
                Part::Forward if self.forward.enabled => vec![ChromeAction::Forward],
                Part::Reload if self.loading => vec![ChromeAction::Stop],
                Part::Reload => vec![ChromeAction::Reload],
                Part::NewTab => vec![ChromeAction::NewTab],
                Part::Menu => {
                    self.open_menu();
                    Vec::new()
                }
                Part::Tab(id) if self.current != Some(id) => vec![ChromeAction::SelectTab(id)],
                Part::TabClose(id) => vec![ChromeAction::CloseTab(id)],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn open_menu(&mut self) {
        self.blur();
        let items = vec![
            MenuItem::new("New tab", Some("Ctrl+T"), MenuAction::NewTab),
            MenuItem::new("Close tab", Some("Ctrl+W"), MenuAction::CloseTab),
            MenuItem::new(if self.loading { "Stop" } else { "Reload" }, Some("F5"), MenuAction::Reload),
            MenuItem::SEPARATOR,
            MenuItem::note(concat!("browser ", env!("CARGO_PKG_VERSION"))),
        ];
        self.menu = Some(Menu::open(items, self.menu_button.rect, self.width));
        self.hover = None;
        self.tooltip_shown = false;
        self.dirty = true;
    }

    fn menu_action(&mut self, action: MenuAction) -> Vec<ChromeAction> {
        match action {
            MenuAction::NewTab => vec![ChromeAction::NewTab],
            MenuAction::CloseTab => self.current.map(ChromeAction::CloseTab).into_iter().collect(),
            MenuAction::Reload => vec![if self.loading { ChromeAction::Stop } else { ChromeAction::Reload }],
        }
    }

    /// A key while the chrome wants them.
    pub fn key(&mut self, input: KeyInput) -> Vec<ChromeAction> {
        if let Some(menu) = &mut self.menu {
            self.dirty = true;
            match input.key {
                Key::ArrowDown => menu.step(true),
                Key::ArrowUp => menu.step(false),
                Key::Enter => {
                    let chosen = menu.keyed.or(menu.hover).and_then(|i| menu.items[i].action);
                    if let Some(action) = chosen {
                        self.close_menu();
                        return self.menu_action(action);
                    }
                }
                Key::Escape => self.close_menu(),
                _ => {}
            }
            return Vec::new();
        }
        if !self.address.focused {
            return Vec::new();
        }
        self.dirty = true;
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
            self.dirty = true;
        }
    }

    pub fn ime_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        if self.address.focused {
            self.address.ime_preedit(&mut self.fonts, &mut self.lcx, text, cursor);
            self.dirty = true;
        }
    }

    pub fn ime_commit(&mut self, text: &str) {
        if self.address.focused {
            self.address.ime_commit(&mut self.fonts, &mut self.lcx, text);
            self.dirty = true;
        }
    }

    /// Draw the chrome at the top of `scene`, in physical pixels. A menu
    /// or tooltip extends below it, over the page.
    pub fn draw(&mut self, scene: &mut Scene) {
        self.dirty = false;
        let now = Instant::now();
        let s = self.scale as f64;
        let w = self.width as f64 * s;
        let strip_h = TABSTRIP_HEIGHT as f64 * s;
        let total_h = self.height() as f64 * s;

        // Backgrounds: strip, toolbar, bottom line.
        scene.fill(Fill::NonZero, Affine::IDENTITY, STRIP_BG, None, &Rect::new(0.0, 0.0, w, strip_h));
        scene.fill(Fill::NonZero, Affine::IDENTITY, BAR_BG, None, &Rect::new(0.0, strip_h, w, total_h));
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            BAR_LINE,
            None,
            &Rect::new(0.0, total_h - s.max(1.0), w, total_h),
        );

        self.draw_tabs(scene, now);

        // Toolbar buttons.
        for (button, part) in [
            (&self.back, Part::Back),
            (&self.forward, Part::Forward),
            (&self.reload, Part::Reload),
            (&self.menu_button, Part::Menu),
        ] {
            let pressed = self.pressed == Some(part) || (part == Part::Menu && self.menu.is_some());
            button.draw(scene, s, self.hover == Some(part), pressed);
        }

        // Address box.
        let box_rect = scale_rect(self.address.rect, s);
        let (border, width) = if self.address.focused {
            (ACCENT, 2.0 * s)
        } else {
            (BOX_BORDER, s.max(1.0))
        };
        rounded_box(scene, box_rect, 8.0 * s, BOX_BG, border, width);
        if let Some(icon) = security_icon(self.url.as_ref()) {
            let r = Rect::new(box_rect.x0, box_rect.y0, box_rect.x0 + INDICATOR_WIDTH as f64 * s, box_rect.y1);
            let color = if icon == Icon::LockClosed {
                Color::from_rgb8(0x2e, 0x7d, 0x32)
            } else {
                Color::from_rgb8(0xc0, 0x6a, 0x00)
            };
            draw_icon(scene, icon, r, 15.0 * s, color);
        }
        self.address.draw(&mut self.fonts, &mut self.lcx, scene);

        // Progress: a sweep along the bottom edge of the chrome.
        if let Some(since) = self.loading_since.filter(|_| self.loading) {
            let phase = now.duration_since(since).as_secs_f64() / 1.4;
            let bar = Rect::new(0.0, total_h - 3.0 * s, w, total_h);
            draw_progress(scene, bar, phase, ACCENT);
        }

        if let Some(menu) = &self.menu {
            menu.draw(&mut self.fonts, &mut self.lcx, scene, self.scale);
        } else if self.tooltip_shown
            && let Some(text) = self.hover.and_then(|p| self.tooltip_text(p))
        {
            let at = ((self.mouse.0 as f64 + 12.0) * s, (self.mouse.1 as f64 + 20.0) * s);
            draw_tooltip(&mut self.fonts, &mut self.lcx, scene, &text, at, self.scale, w);
        }
    }

    fn draw_tabs(&mut self, scene: &mut Scene, now: Instant) {
        let s = self.scale as f64;
        let strip_h = TABSTRIP_HEIGHT as f64 * s;
        let spin_phase = self
            .loading_since
            .map(|since| now.duration_since(since).as_secs_f64())
            .unwrap_or(0.0);
        for tab in &self.tabs {
            let r = scale_rect(tab.rect, s);
            let is_current = self.current == Some(tab.info.id);
            let hovered = matches!(self.hover, Some(Part::Tab(id) | Part::TabClose(id)) if id == tab.info.id);
            if is_current {
                // Rounded top, joined to the toolbar below.
                let shape = RoundedRect::new(r.x0, r.y0, r.x1, strip_h + 8.0 * s, 8.0 * s);
                scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &Rect::new(r.x0, r.y0, r.x1, strip_h));
                scene.fill(Fill::NonZero, Affine::IDENTITY, BAR_BG, None, &shape);
                scene.pop_layer();
            } else if hovered {
                let shape = RoundedRect::from_rect(Rect::new(r.x0, r.y0, r.x1, r.y1 - 2.0 * s), 8.0 * s);
                scene.fill(Fill::NonZero, Affine::IDENTITY, TAB_HOVER, None, &shape);
            }
            let close = scale_rect(tab.close_rect(), s);
            let mut text_x = r.x0 + 10.0 * s;
            if tab.info.loading {
                draw_spinner(scene, (text_x + 5.0 * s, (r.y0 + r.y1) / 2.0), 5.0 * s, spin_phase, ACCENT);
                text_x += 18.0 * s;
            }
            let title = if tab.info.title.is_empty() { "New tab" } else { tab.info.title.as_str() };
            let color = if is_current { TEXT } else { TEXT_DIM };
            draw_text(
                &mut self.fonts,
                &mut self.lcx,
                scene,
                TextLine {
                    text: title,
                    font_size: TAB_FONT_SIZE,
                    scale: self.scale,
                    color,
                    origin: (text_x, r.y0),
                    max_width: (close.x0 - 4.0 * s - text_x).max(0.0),
                    height: r.height(),
                },
            );
            if self.hover == Some(Part::TabClose(tab.info.id)) {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgb8(0xd0, 0xd0, 0xd8),
                    None,
                    &RoundedRect::from_rect(close, 4.0 * s),
                );
            }
            draw_icon(scene, Icon::Close, close, 12.0 * s, Color::from_rgba8(color[0], color[1], color[2], color[3]));
        }
        self.new_tab
            .draw(scene, s, self.hover == Some(Part::NewTab), self.pressed == Some(Part::NewTab));
    }
}

/// The fixed tooltip of a part; tabs and the reload button vary.
fn tooltip_for(part: Part) -> Option<&'static str> {
    match part {
        Part::Back => Some("Back"),
        Part::Forward => Some("Forward"),
        Part::Reload => Some("Reload"),
        Part::NewTab => Some("New tab"),
        Part::Menu => Some("Menu"),
        Part::TabClose(_) => Some("Close tab"),
        Part::Tab(_) => Some(""),
        Part::Address => None,
    }
}

/// The security indicator for a URL: a closed lock for HTTPS, an open
/// one for plain HTTP, nothing for internal pages.
fn security_icon(url: Option<&Url>) -> Option<Icon> {
    match url?.scheme() {
        "https" => Some(Icon::LockClosed),
        "http" => Some(Icon::LockOpen),
        _ => None,
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

    fn click(c: &mut Chrome, (x, y): (f32, f32)) -> Vec<ChromeAction> {
        let mut out = c.mouse_down(x, y, MouseButton::Left, false);
        out.extend(c.mouse_up(x, y, MouseButton::Left));
        out
    }

    fn tabs(n: u64) -> Vec<TabInfo> {
        (1..=n)
            .map(|i| TabInfo {
                id: TabId(i),
                title: format!("Tab {i}"),
                loading: false,
            })
            .collect()
    }

    #[test]
    fn layout_puts_the_strip_above_the_toolbar_and_buttons_before_the_box() {
        let mut c = chrome();
        assert_eq!(c.height(), TABSTRIP_HEIGHT + TOOLBAR_HEIGHT);
        let [back, forward, reload, new_tab, menu] = c.button_centers();
        let (x, y, w, h) = c.address_rect();
        assert!(back.0 < forward.0 && forward.0 < reload.0 && reload.0 < x);
        assert!(x + w < menu.0 && menu.0 < 800.0, "the menu button is right of the box");
        assert!(y > TABSTRIP_HEIGHT && y + h <= c.height());
        assert!(new_tab.1 < TABSTRIP_HEIGHT, "the + lives in the strip");
        assert!(c.contains(100.0, 10.0) && !c.contains(100.0, c.height() + 1.0));

        c.set_tabs(tabs(3), Some(TabId(2)));
        let (t1, _) = c.tab_centers(TabId(1)).unwrap();
        let (t3, close3) = c.tab_centers(TabId(3)).unwrap();
        assert!(t1.0 < t3.0 && t3.0 < close3.0 && close3.0 < c.button_centers()[3].0);
        assert!(t1.1 < TABSTRIP_HEIGHT);
        // Tabs shrink to fit: twenty of them still end before the + button.
        c.set_tabs(tabs(20), Some(TabId(1)));
        let (t20, _) = c.tab_centers(TabId(20)).unwrap();
        assert!(t20.0 < c.button_centers()[3].0);
    }

    #[test]
    fn buttons_act_on_release_and_respect_their_state() {
        let mut c = chrome();
        let [back, forward, reload, new_tab, _] = c.button_centers();
        assert!(click(&mut c, back).is_empty(), "nothing to go back to");
        assert!(click(&mut c, forward).is_empty());
        assert_eq!(c.mouse_move(back.0, back.1), Cursor::Default, "disabled: no pointer cursor");
        c.set_nav_state(true, false);
        assert_eq!(c.mouse_move(back.0, back.1), Cursor::Pointer);
        assert_eq!(click(&mut c, back), vec![ChromeAction::Back]);
        assert!(click(&mut c, forward).is_empty());
        assert_eq!(click(&mut c, reload), vec![ChromeAction::Reload]);
        c.set_loading(true);
        assert_eq!(click(&mut c, reload), vec![ChromeAction::Stop]);
        assert_eq!(click(&mut c, new_tab), vec![ChromeAction::NewTab]);
        // Press on one button, release on another: nothing.
        assert!(c.mouse_down(back.0, back.1, MouseButton::Left, false).is_empty());
        assert!(c.mouse_up(reload.0, reload.1, MouseButton::Left).is_empty());
        // A click on a button takes focus from the address bar.
        c.focus_address();
        click(&mut c, reload);
        assert!(!c.has_focus());
    }

    #[test]
    fn tab_strip_selects_and_closes() {
        let mut c = chrome();
        c.set_tabs(tabs(3), Some(TabId(1)));
        let (t2, close2) = c.tab_centers(TabId(2)).unwrap();
        let (t1, _) = c.tab_centers(TabId(1)).unwrap();
        assert_eq!(click(&mut c, t2), vec![ChromeAction::SelectTab(TabId(2))]);
        assert!(click(&mut c, t1).is_empty(), "the current tab is already selected");
        assert_eq!(click(&mut c, close2), vec![ChromeAction::CloseTab(TabId(2))]);
        assert_eq!(
            c.mouse_down(t2.0, t2.1, MouseButton::Middle, false),
            vec![ChromeAction::CloseTab(TabId(2))]
        );
        assert_eq!(c.mouse_move(t2.0, t2.1), Cursor::Pointer);
        // Same ids in the same order: titles update without a relayout.
        let mut renamed = tabs(3);
        renamed[1].title = "Renamed".into();
        renamed[1].loading = true;
        c.set_tabs(renamed, Some(TabId(2)));
        assert_eq!(c.tab_centers(TabId(2)).unwrap().0, t2);
        c.set_tabs(tabs(1), Some(TabId(1)));
        assert!(c.tab_centers(TabId(3)).is_none());
    }

    #[test]
    fn menu_opens_from_its_button_and_answers_clicks_keys_and_escape() {
        let mut c = chrome();
        c.set_tabs(tabs(2), Some(TabId(2)));
        let menu_btn = c.button_centers()[4];
        assert!(click(&mut c, menu_btn).is_empty());
        assert!(c.menu_open() && c.wants_keys() && !c.has_focus());
        assert!(c.contains(400.0, 600.0), "with a menu open the chrome takes every point");
        // Keyboard: down twice is "Close tab", Enter chooses it.
        c.key(KeyInput::plain(Key::ArrowDown));
        c.key(KeyInput::plain(Key::ArrowDown));
        assert_eq!(c.key(KeyInput::plain(Key::Enter)), vec![ChromeAction::CloseTab(TabId(2))]);
        assert!(!c.menu_open());
        // Mouse: the first row is "New tab".
        click(&mut c, menu_btn);
        let row = c.menu.as_ref().unwrap().rect;
        let first = ((row.x0 + 20.0) as f32, (row.y0 + 6.0 + 15.0) as f32);
        assert_eq!(c.mouse_move(first.0, first.1), Cursor::Pointer);
        assert_eq!(click(&mut c, first), vec![ChromeAction::NewTab]);
        assert!(!c.menu_open());
        // Escape closes; a click outside closes without acting; the
        // button toggles it closed.
        click(&mut c, menu_btn);
        c.key(KeyInput::plain(Key::Escape));
        assert!(!c.menu_open());
        click(&mut c, menu_btn);
        assert!(click(&mut c, (100.0, 400.0)).is_empty());
        assert!(!c.menu_open());
        click(&mut c, menu_btn);
        assert!(click(&mut c, menu_btn).is_empty());
        assert!(!c.menu_open());
        // Up from nothing lands on the last enabled row (Reload).
        click(&mut c, menu_btn);
        c.key(KeyInput::plain(Key::ArrowUp));
        assert_eq!(c.key(KeyInput::plain(Key::Enter)), vec![ChromeAction::Reload]);
    }

    #[test]
    fn tooltips_appear_after_a_pause_and_loading_animates() {
        let mut c = chrome();
        let [back, ..] = c.button_centers();
        assert!(c.next_wake().is_none(), "nothing to wait for");
        c.mouse_move(back.0, back.1);
        let wake = c.next_wake().expect("a tooltip is pending");
        assert!(!c.tick(Instant::now()), "not yet");
        assert!(c.tick(wake + Duration::from_millis(1)));
        assert!(c.tooltip_shown);
        let mut scene = Scene::new();
        c.draw(&mut scene);
        c.mouse_move(back.0 + 100.0, back.1);
        assert!(!c.tooltip_shown, "leaving the part hides it");
        c.mouse_leave();
        assert!(c.next_wake().is_none());
        c.set_loading(true);
        let wake = c.next_wake().expect("animation frames while loading");
        assert!(wake <= Instant::now() + ANIMATION_FRAME);
        assert!(c.tick(Instant::now()));
        c.draw(&mut scene);
        c.set_loading(false);
        assert!(c.next_wake().is_none());
    }

    #[test]
    fn security_indicator_follows_the_scheme() {
        let mut c = chrome();
        c.set_url(&Url::parse("https://example.com/").unwrap());
        assert!(c.is_secure());
        let (x1, ..) = c.address_rect();
        c.set_url(&Url::parse("http://example.com/").unwrap());
        assert!(!c.is_secure());
        c.set_url(&Url::parse("about:blank").unwrap());
        assert!(!c.is_secure());
        assert_eq!(c.address_rect().0, x1, "the box does not move; the text inside does");
    }

    #[test]
    fn hover_and_changes_mark_the_chrome_dirty() {
        let mut c = chrome();
        let mut scene = Scene::new();
        c.draw(&mut scene);
        assert!(!c.take_dirty());
        let [back, ..] = c.button_centers();
        c.mouse_move(back.0, back.1);
        assert!(c.take_dirty());
        c.mouse_move(back.0 + 1.0, back.1);
        assert!(!c.take_dirty(), "still on the same part");
        c.mouse_leave();
        assert!(c.take_dirty());
        c.set_loading(true);
        assert!(c.take_dirty());
        c.set_loading(true);
        assert!(!c.take_dirty());
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
        let inside = (x + INDICATOR_WIDTH + 40.0, y + h / 2.0);
        assert_eq!(c.mouse_move(inside.0, inside.1), Cursor::Text);
        assert_eq!(c.mouse_move(inside.0, TABSTRIP_HEIGHT + 2.0), Cursor::Default);
        click(&mut c, inside);
        assert!(c.has_focus());
        type_str(&mut c, "z");
        assert_eq!(c.address_text(), "z", "first click selected everything");
        // Draw once so the layout exists, then click at the far left (far
        // enough from the first click not to be a double click): the
        // caret lands before the z and typing goes there.
        let mut scene = Scene::new();
        c.draw(&mut scene);
        click(&mut c, (x + INDICATOR_WIDTH + 5.0, y + h / 2.0));
        type_str(&mut c, "a");
        assert_eq!(c.address_text(), "az");
        // A click on the bar outside the box takes focus away.
        click(&mut c, (inside.0, TABSTRIP_HEIGHT + 2.0));
        assert!(!c.has_focus());
    }

    #[test]
    fn draws_every_state_without_panicking() {
        let mut c = chrome();
        let mut scene = Scene::new();
        c.draw(&mut scene);
        c.set_tabs(
            vec![
                TabInfo {
                    id: TabId(1),
                    title: "A rather long title that will not fit in a tab".into(),
                    loading: true,
                },
                TabInfo {
                    id: TabId(2),
                    title: String::new(),
                    loading: false,
                },
            ],
            Some(TabId(1)),
        );
        c.set_nav_state(true, true);
        c.set_loading(true);
        c.set_url(&Url::parse("http://example.com/a/very/long/path/that/goes/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on").unwrap());
        c.resize(300.0, 2.0);
        let (t2, close2) = c.tab_centers(TabId(2)).unwrap();
        c.mouse_move(t2.0, t2.1);
        c.draw(&mut scene);
        c.mouse_move(close2.0, close2.1);
        c.tick(Instant::now() + TOOLTIP_DELAY * 2);
        c.draw(&mut scene);
        c.focus_address();
        c.key(KeyInput::plain(Key::End));
        c.draw(&mut scene);
        c.ime_preedit("あい", Some((0, 6)));
        c.draw(&mut scene);
        let menu_btn = c.button_centers()[4];
        click(&mut c, menu_btn);
        let menu_rect = c.menu.as_ref().unwrap().rect;
        c.mouse_move(menu_rect.x0 as f32 + 10.0, menu_rect.y0 as f32 + 20.0);
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
