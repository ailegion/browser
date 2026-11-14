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

mod console;
mod find;
mod input;
mod menu;
pub mod scrollbar;
mod suggest;
mod widgets;

use std::time::{Duration, Instant};

use browser_ipc_types::{Cursor, MouseButton, TabId};
use parley::{FontContext, LayoutContext};
use taffy::prelude::*;
use url::Url;
use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};

pub use console::ConsoleEntry;
use console::{ConsolePanel, ConsolePart};
use find::{FindBar, FindPart};
use input::{InputEvent, TextInput, rounded_box};
use menu::{Menu, MenuAction, MenuItem};
pub use suggest::Suggestion;
use suggest::{SuggestList, SuggestRow};
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
    /// Find in page: the box's text changed.
    Find(String),
    /// Step to the next or previous match.
    FindNext { forward: bool },
    /// The find bar closed.
    FindClose,
    /// Tab (or Shift+Tab) left a chrome text box: the page should focus
    /// its first (or last) focusable element.
    FocusPage { forward: bool },
    /// The address bar's text changed by the user's typing: the shell
    /// answers with set_suggestions for it.
    AddressTyped(String),
    /// Shift+Delete on a suggestion: forget the page.
    RemoveHistory(Url),
    /// Enter in the console's input line: run this code in the page.
    ConsoleEvaluate(String),
    /// The console's Clear: the shell drops the tab's messages too.
    ConsoleClear,
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
    Find(FindPart),
    /// A row of the address bar's suggestion list.
    Suggestion(usize),
    /// A part of the developer console panel.
    Console(ConsolePart),
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
    /// The find bar, while open.
    find: Option<FindBar>,
    /// The address bar's suggestion list, while the user types.
    suggest: Option<SuggestList>,
    /// The inline completion in the box: the box's text with it, and
    /// the page Enter then opens.
    inline: Option<(String, Url)>,
    /// The developer console panel, while open.
    console: Option<ConsolePanel>,
    /// The panel's height, kept while it is closed.
    console_height: f32,
    /// The panel opened, closed or changed height: the page's viewport
    /// changes (`take_console_layout_changed`).
    console_layout_changed: bool,
    /// The window's logical height, for the panel.
    window_height: f32,
    /// The last edit of the address text inserted text (typing, a
    /// paste, an IME commit), not a deletion: only then is it filled in.
    inserted: bool,
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
            find: None,
            suggest: None,
            inline: None,
            console: None,
            console_height: 260.0,
            console_layout_changed: false,
            window_height: 600.0,
            inserted: false,
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
        if let Some(f) = &mut self.find {
            f.input.set_scale(self.scale);
        }
        self.menu = None;
        self.relayout();
        self.place_console();
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
        if let Some(f) = &mut self.find {
            f.layout(self.width, TABSTRIP_HEIGHT + TOOLBAR_HEIGHT);
        }
        self.dirty = true;
    }

    // ----- find bar -----

    /// Open the find bar, or bring focus back to it, with its text
    /// selected (Ctrl+F).
    pub fn open_find(&mut self) {
        self.close_menu();
        if self.address.focused {
            self.blur();
        }
        let scale = self.scale;
        let f = self.find.get_or_insert_with(|| FindBar::new(scale));
        f.input.focused = true;
        f.input.select_all(&mut self.fonts, &mut self.lcx);
        f.layout(self.width, TABSTRIP_HEIGHT + TOOLBAR_HEIGHT);
        self.dirty = true;
    }

    /// Close the find bar (Escape, its close button). The action tells
    /// the tab to drop its matches.
    pub fn close_find(&mut self) -> Vec<ChromeAction> {
        if self.find.take().is_none() {
            return Vec::new();
        }
        if matches!(self.hover, Some(Part::Find(_))) {
            self.hover = None;
            self.hover_since = None;
            self.tooltip_shown = false;
        }
        self.dirty = true;
        vec![ChromeAction::FindClose]
    }

    pub fn find_open(&self) -> bool {
        self.find.is_some()
    }

    fn find_focused(&self) -> bool {
        self.find.as_ref().is_some_and(|f| f.input.focused)
    }

    /// The history's matches for `typed`: shown under the address bar,
    /// after a first row for what was typed itself (when it is an
    /// address), that row selected. Nothing else starts selected. An
    /// answer for text no longer in the
    /// box, or for a box without focus, is dropped.
    ///
    /// `fill` is the history's inline completion for `typed` (the box's
    /// text with it, and its page). It goes in only after an insertion
    /// with the caret at the end, the added part selected, so typing on
    /// replaces it and Backspace removes it; the first row is then that
    /// page.
    pub fn set_suggestions(&mut self, typed: &str, suggestions: Vec<Suggestion>, fill: Option<(String, Url)>) {
        if !self.address.focused || self.address.text() != typed {
            return;
        }
        let fill = fill.filter(|(text, _)| {
            self.inserted && self.address.caret_at_end() && text.len() > typed.len() && text.starts_with(typed)
        });
        if let Some((text, url)) = &fill {
            self.address.set_text(text);
            self.address.select_to_end_from(&mut self.fonts, &mut self.lcx, typed.len());
            self.inline = Some((text.clone(), url.clone()));
        }
        let own = match &fill {
            Some((text, url)) => Some((url.clone(), text.clone())),
            None => url_from_input(typed).map(|u| (u, String::new())),
        };
        let mut rows: Vec<SuggestRow> = own
            .iter()
            .map(|(url, shown)| SuggestRow {
                url: url.clone(),
                title: shown.clone(),
                typed: true,
            })
            .collect();
        let own = own.map(|(u, _)| u);
        rows.extend(suggestions.into_iter().filter(|s| own.as_ref() != Some(&s.url)).map(|s| SuggestRow {
            url: s.url,
            title: s.title,
            typed: false,
        }));
        self.suggest = (!rows.is_empty()).then(|| SuggestList::new(typed.to_owned(), rows, self.address.rect));
        self.dirty = true;
    }

    /// The suggestion rows shown, as (address, title, is the typed row),
    /// and the selected one (tests and the shell).
    pub fn suggestions(&self) -> (Vec<(Url, String, bool)>, Option<usize>) {
        match &self.suggest {
            Some(l) => (l.rows.iter().map(|r| (r.url.clone(), r.title.clone(), r.typed)).collect(), l.selected),
            None => (Vec::new(), None),
        }
    }

    // ----- the developer console panel -----

    /// The window's logical height (the panel sits at its bottom).
    pub fn set_window_height(&mut self, height: f32) {
        self.window_height = height.max(0.0);
        self.place_console();
    }

    fn place_console(&mut self) {
        let (width, height, top) = (self.width, self.window_height, self.height());
        if let Some(c) = &mut self.console {
            c.input.set_scale(self.scale);
            let before = c.rect;
            c.place(width, height, top);
            if c.rect != before {
                self.console_layout_changed = true;
            }
            self.console_height = c.height;
        }
    }

    /// Open or close the panel (F12, Ctrl+Shift+J), with `entries` (the
    /// current tab's messages) when it opens.
    pub fn toggle_console(&mut self, entries: Vec<ConsoleEntry>) {
        if self.console.take().is_none() {
            let mut panel = ConsolePanel::new(self.console_height);
            panel.set(entries);
            self.console = Some(panel);
            self.place_console();
        }
        self.console_layout_changed = true;
        self.dirty = true;
    }

    pub fn console_open(&self) -> bool {
        self.console.is_some()
    }

    /// The logical height the panel takes from the page (0 when closed).
    pub fn console_height(&self) -> f32 {
        self.console.as_ref().map_or(0.0, |c| c.rect.height() as f32)
    }

    /// Whether the panel opened, closed or was resized since the last
    /// call: the page's viewport is to be sized again.
    pub fn take_console_layout_changed(&mut self) -> bool {
        std::mem::take(&mut self.console_layout_changed)
    }

    /// A new message from the current tab.
    pub fn console_push(&mut self, entry: ConsoleEntry) {
        if let Some(c) = &mut self.console {
            c.push(entry);
            self.dirty = true;
        }
    }

    /// The messages of another tab (the current one changed).
    pub fn console_set(&mut self, entries: Vec<ConsoleEntry>) {
        if let Some(c) = &mut self.console {
            c.set(entries);
            self.dirty = true;
        }
    }

    /// The current tab's document changed: its console starts empty.
    pub fn console_clear(&mut self) {
        if let Some(c) = &mut self.console {
            c.clear();
            self.dirty = true;
        }
    }

    /// A wheel over the panel scrolls its list; true when it took it.
    pub fn console_wheel(&mut self, x: f32, y: f32, dy: f32) -> bool {
        let Some(c) = &mut self.console else { return false };
        if !c.contains(x, y) {
            return false;
        }
        c.wheel(dy, &mut self.fonts, &mut self.lcx);
        self.dirty = true;
        true
    }

    /// What Copy all would put on the clipboard (tests).
    pub fn console_text(&self) -> Option<String> {
        self.console.as_ref().map(ConsolePanel::copy_text)
    }

    /// Open a suggestion row: its page, from the address bar.
    fn open_suggestion(&mut self, index: usize) -> Vec<ChromeAction> {
        let Some(row) = self.suggest.as_ref().and_then(|l| l.rows.get(index)).cloned() else { return Vec::new() };
        self.address.set_text(display_url(&row.url).as_str());
        self.blur();
        vec![ChromeAction::Navigate(row.url)]
    }

    /// The keys the suggestion list takes while open: arrows move the
    /// selection (the box shows the selected address, the typed text on
    /// the first row), Enter opens the selected row, Escape closes the
    /// list and puts the typed text back, Shift+Delete forgets the
    /// selected history row. `None`: the key goes to the box.
    fn suggestion_key(&mut self, input: &KeyInput) -> Option<Vec<ChromeAction>> {
        let list = self.suggest.as_mut()?;
        match input.key {
            Key::ArrowDown | Key::ArrowUp => {
                list.step(input.key == Key::ArrowDown);
                let shown = match list.selected.and_then(|i| list.rows.get(i)) {
                    Some(r) if !r.typed => display_url(&r.url),
                    // The first row: the typed text, completed if it was.
                    Some(r) if !r.title.is_empty() => r.title.clone(),
                    _ => list.typed.clone(),
                };
                self.address.set_text(&shown);
                self.address.move_to_end(&mut self.fonts, &mut self.lcx);
                self.dirty = true;
                Some(Vec::new())
            }
            Key::Enter => {
                let index = list.selected.filter(|&i| list.rows.get(i).is_some_and(|r| !r.typed))?;
                Some(self.open_suggestion(index))
            }
            Key::Escape => {
                let typed = list.typed.clone();
                self.suggest = None;
                self.inline = None;
                self.address.set_text(&typed);
                self.address.move_to_end(&mut self.fonts, &mut self.lcx);
                self.dirty = true;
                Some(Vec::new())
            }
            Key::Delete if input.shift => {
                let index = list.selected.filter(|&i| list.rows.get(i).is_some_and(|r| !r.typed))?;
                let url = list.rows[index].url.clone();
                list.remove(index, self.address.rect);
                if list.rows.is_empty() {
                    self.suggest = None;
                }
                self.dirty = true;
                Some(vec![ChromeAction::RemoveHistory(url)])
            }
            _ => None,
        }
    }

    /// After an edit of the address text: the typed text changed, so the
    /// list waits for the shell's new matches (an empty box has none).
    fn address_edited(&mut self, before: &str, actions: &mut Vec<ChromeAction>) {
        let text = self.address.text();
        if text == before {
            return;
        }
        self.inline = None;
        if text.trim().is_empty() {
            self.suggest = None;
            self.dirty = true;
            return;
        }
        actions.push(ChromeAction::AddressTyped(text));
    }

    /// The tab's answer to the last query: the current match and the total.
    pub fn set_find_result(&mut self, current: Option<usize>, total: usize) {
        if let Some(f) = &mut self.find {
            f.set_result(current, total);
            self.dirty = true;
        }
    }

    /// The find panel in logical pixels, while open.
    pub fn find_rect(&self) -> Option<(f32, f32, f32, f32)> {
        let r = self.find.as_ref()?.rect;
        Some((r.x0 as f32, r.y0 as f32, r.width() as f32, r.height() as f32))
    }

    /// Centers of the find bar's previous, next and close buttons.
    pub fn find_button_centers(&self) -> Option<[(f32, f32); 3]> {
        let f = self.find.as_ref()?;
        let c = |r: Rect| (((r.x0 + r.x1) / 2.0) as f32, ((r.y0 + r.y1) / 2.0) as f32);
        Some([c(f.prev.rect), c(f.next.rect), c(f.close.rect)])
    }

    /// What the find bar shows beside the box: "2/5", "No results", or
    /// nothing yet.
    pub fn find_status(&self) -> Option<String> {
        self.find.as_ref().map(FindBar::count_text)
    }

    /// A key while the find box has focus.
    fn find_key(&mut self, input: KeyInput) -> Vec<ChromeAction> {
        self.dirty = true;
        match input.key {
            Key::Enter => return vec![ChromeAction::FindNext { forward: !input.shift }],
            Key::Escape => return self.close_find(),
            Key::Tab => {
                self.blur();
                return vec![ChromeAction::FocusPage { forward: !input.shift }];
            }
            _ => {}
        }
        let Some(f) = &mut self.find else { return Vec::new() };
        let before = f.input.text();
        let event = f.input.key(&mut self.fonts, &mut self.lcx, &input);
        let mut actions = Vec::new();
        match event {
            Some(InputEvent::Copy(text)) => actions.push(ChromeAction::CopyText(text)),
            Some(InputEvent::RequestPaste) => actions.push(ChromeAction::RequestPaste),
            _ => {}
        }
        let after = f.input.text();
        if after != before {
            actions.push(ChromeAction::Find(after));
        }
        actions
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

    /// The address bar or the find box has keyboard focus.
    pub fn has_focus(&self) -> bool {
        self.address.focused || self.find_focused() || self.console_focused() || self.console_list_focused()
    }

    /// The console's input line has the keyboard.
    fn console_focused(&self) -> bool {
        self.console.as_ref().is_some_and(|c| c.input.focused)
    }

    /// The console's messages have the keyboard (a selection was made).
    fn console_list_focused(&self) -> bool {
        self.console.as_ref().is_some_and(|c| c.list_focused)
    }

    /// The keys the messages take: Ctrl+C copies the selection, Ctrl+A
    /// selects every message; typing goes to the input line, as Chrome's
    /// console sends it to its prompt.
    fn console_list_key(&mut self, input: KeyInput) -> Vec<ChromeAction> {
        let Some(c) = &mut self.console else { return Vec::new() };
        self.dirty = true;
        match &input.key {
            Key::Character(ch) if input.ctrl && ch.eq_ignore_ascii_case("c") => c.selected_text().map(ChromeAction::CopyText).into_iter().collect(),
            Key::Character(ch) if input.ctrl && ch.eq_ignore_ascii_case("a") => {
                c.select_all();
                Vec::new()
            }
            Key::Character(_) if !input.ctrl && !input.alt => {
                c.list_focused = false;
                c.input.focused = true;
                self.console_key(input)
            }
            _ => Vec::new(),
        }
    }

    /// The keys the console's input line takes: Enter runs the code, Up
    /// and Down walk the code run before, Tab leaves for the page; the
    /// rest edit the line.
    fn console_key(&mut self, input: KeyInput) -> Vec<ChromeAction> {
        let Some(c) = &mut self.console else { return Vec::new() };
        self.dirty = true;
        match input.key {
            Key::Enter => return c.submit(&mut self.fonts, &mut self.lcx).map(ChromeAction::ConsoleEvaluate).into_iter().collect(),
            Key::ArrowUp | Key::ArrowDown => {
                c.history_step(input.key == Key::ArrowUp, &mut self.fonts, &mut self.lcx);
                return Vec::new();
            }
            _ => {}
        }
        match c.input.key(&mut self.fonts, &mut self.lcx, &input) {
            Some(InputEvent::Copy(text)) => vec![ChromeAction::CopyText(text)],
            Some(InputEvent::RequestPaste) => vec![ChromeAction::RequestPaste],
            Some(InputEvent::Blur) => {
                self.blur();
                vec![ChromeAction::FocusPage { forward: !input.shift }]
            }
            Some(InputEvent::Cancel) => {
                self.blur();
                Vec::new()
            }
            Some(InputEvent::Submit(_)) | None => Vec::new(),
        }
    }

    /// The chrome wants the keyboard: a text box is focused or a menu is
    /// open.
    pub fn wants_keys(&self) -> bool {
        self.has_focus() || self.menu.is_some()
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
        if self.has_focus() || self.suggest.is_some() {
            self.dirty = true;
        }
        self.suggest = None;
        self.inline = None;
        self.address.blur(&mut self.fonts, &mut self.lcx);
        if let Some(f) = &mut self.find {
            f.input.blur(&mut self.fonts, &mut self.lcx);
        }
        if let Some(c) = &mut self.console {
            c.input.blur(&mut self.fonts, &mut self.lcx);
            // The selection stays, dimmed, without the keyboard.
            c.list_focused = false;
        }
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
        // A held arrow or track of the console's scrollbar.
        if let Some(due) = self.console.as_ref().and_then(ConsolePanel::next_wake) {
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
        if let Some(c) = &mut self.console
            && c.tick(now, &mut self.fonts, &mut self.lcx)
        {
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
        if self.address.focused {
            return Some(self.address.ime_cursor_area(&mut self.fonts, &mut self.lcx));
        }
        if let Some(c) = &mut self.console
            && c.input.focused
        {
            return Some(c.input.ime_cursor_area(&mut self.fonts, &mut self.lcx));
        }
        match &mut self.find {
            Some(f) if f.input.focused => Some(f.input.ime_cursor_area(&mut self.fonts, &mut self.lcx)),
            _ => None,
        }
    }

    /// Whether a point is on the chrome rather than the page: the bars,
    /// the find panel hanging under them. With a menu open the chrome
    /// takes every point, to close it on a click outside.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.menu.is_some()
            || y < self.height()
            || self.find.as_ref().is_some_and(|f| f.contains(x, y))
            || self.suggest.as_ref().is_some_and(|l| l.contains(x, y))
            || self.console.as_ref().is_some_and(|c| c.contains(x, y))
    }

    fn part_at(&self, x: f32, y: f32) -> Option<Part> {
        if let Some(p) = self.console.as_ref().and_then(|c| c.part_at(x, y)) {
            return Some(Part::Console(p));
        }
        if let Some(i) = self.suggest.as_ref().and_then(|l| l.row_at(x, y)) {
            return Some(Part::Suggestion(i));
        }
        if let Some(part) = self.find.as_ref().and_then(|f| f.part_at(x, y)) {
            return Some(Part::Find(part));
        }
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
        if let Some(c) = &mut self.console {
            c.mouse = (x, y);
            // The console's top edge being dragged: the panel follows.
            if c.resizing.is_some() {
                c.drag_edge(y, self.window_height);
                self.place_console();
                self.dirty = true;
                return Cursor::NsResize;
            }
            // Its scrollbar's thumb being dragged: the list follows.
            if c.drag_thumb(y) {
                self.dirty = true;
                return Cursor::Default;
            }
            // A selection drag in its input line, or in its messages.
            if self.pressed == Some(Part::Console(ConsolePart::Input)) {
                c.input.mouse_move(&mut self.fonts, &mut self.lcx, x, y);
                self.dirty = true;
            }
            if self.pressed == Some(Part::Console(ConsolePart::List)) && c.drag_text(x, y, &mut self.fonts, &mut self.lcx) {
                self.dirty = true;
                return Cursor::Text;
            }
        }
        let part = self.part_at(x, y);
        if let Some(c) = &mut self.console {
            let over = match part {
                Some(Part::Console(p)) => Some(p),
                _ => None,
            };
            // A source's tooltip follows the pointer.
            if over != c.hover || matches!(over, Some(ConsolePart::Source(_))) {
                c.hover = over;
                self.dirty = true;
            }
        }
        if let Some(list) = &mut self.suggest {
            let row = match part {
                Some(Part::Suggestion(i)) => Some(i),
                _ => None,
            };
            if row != list.hover {
                list.hover = row;
                self.dirty = true;
            }
        }
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
        if let Some(f) = &mut self.find {
            f.input.mouse_move(&mut self.fonts, &mut self.lcx, x, y);
        }
        if matches!(self.pressed, Some(Part::Address | Part::Find(FindPart::Input))) {
            self.dirty = true;
        }
        match part {
            Some(Part::Address | Part::Find(FindPart::Input)) => Cursor::Text,
            Some(Part::Back) if self.back.enabled => Cursor::Pointer,
            Some(Part::Forward) if self.forward.enabled => Cursor::Pointer,
            Some(Part::Find(FindPart::Prev | FindPart::Next)) => {
                if self.find.as_ref().is_some_and(|f| f.total > 0) {
                    Cursor::Pointer
                } else {
                    Cursor::Default
                }
            }
            Some(Part::Find(FindPart::Close)) => Cursor::Pointer,
            Some(Part::Reload | Part::NewTab | Part::Menu | Part::Tab(_) | Part::TabClose(_) | Part::Suggestion(_)) => Cursor::Pointer,
            Some(Part::Console(ConsolePart::Resize)) => Cursor::NsResize,
            Some(Part::Console(ConsolePart::Input)) => Cursor::Text,
            Some(Part::Console(ConsolePart::Levels | ConsolePart::Level(_) | ConsolePart::Clear | ConsolePart::Copy | ConsolePart::Close | ConsolePart::Source(_))) => {
                Cursor::Pointer
            }
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
        // A press anywhere but the console's levels menu closes it.
        if let Some(c) = &mut self.console
            && c.levels_open
            && !matches!(part, Some(Part::Console(ConsolePart::Levels | ConsolePart::Level(_))))
        {
            c.levels_open = false;
        }
        match (button, part) {
            (MouseButton::Left, Some(Part::Address)) => {
                if self.find_focused() {
                    self.blur();
                }
                self.address.mouse_down(&mut self.fonts, &mut self.lcx, x, y, shift);
                self.pressed = Some(Part::Address);
            }
            (MouseButton::Left, Some(Part::Find(FindPart::Input))) => {
                if self.address.focused {
                    self.blur();
                }
                if let Some(f) = &mut self.find {
                    f.input.mouse_down(&mut self.fonts, &mut self.lcx, x, y, shift);
                }
                self.pressed = Some(Part::Find(FindPart::Input));
            }
            // The panel and its buttons leave the find box's focus alone.
            (MouseButton::Left, Some(Part::Find(FindPart::Panel))) => {}
            // A suggestion opens on release; the box keeps focus till then.
            (MouseButton::Left, Some(p @ Part::Suggestion(_))) => self.pressed = Some(p),
            // The console's top edge: a drag resizes the panel.
            (MouseButton::Left, Some(Part::Console(ConsolePart::Resize))) => {
                if let Some(c) = &mut self.console {
                    c.resizing = Some(y - c.rect.y0 as f32);
                }
            }
            // The input line takes the keyboard and the caret; a press in
            // the list gives it the keyboard too (Chrome's console does).
            (MouseButton::Left, Some(p @ Part::Console(cp @ (ConsolePart::Input | ConsolePart::List)))) => {
                if self.address.focused || self.find_focused() {
                    self.blur();
                }
                if let Some(c) = &mut self.console {
                    c.press(cp, y, &mut self.fonts, &mut self.lcx);
                    if cp == ConsolePart::Input {
                        c.list_focused = false;
                        c.input.mouse_down(&mut self.fonts, &mut self.lcx, x, y, shift);
                    } else {
                        // A selection in the messages starts; a press that
                        // does not drag gives the input line the keyboard
                        // on release.
                        c.input.blur(&mut self.fonts, &mut self.lcx);
                        c.press_text(x, y, shift, &mut self.fonts, &mut self.lcx);
                    }
                }
                self.pressed = Some(p);
            }
            // The rest of the panel: its scrollbar acts at once, its
            // buttons on release.
            (MouseButton::Left, Some(p @ Part::Console(cp))) => {
                if self.has_focus() {
                    self.blur();
                }
                if let Some(c) = &mut self.console {
                    c.press(cp, y, &mut self.fonts, &mut self.lcx);
                }
                self.pressed = Some(p);
            }
            (MouseButton::Left, Some(p @ Part::Find(_))) => self.pressed = Some(p),
            (MouseButton::Left, Some(p)) => {
                if self.has_focus() {
                    self.blur();
                }
                self.pressed = Some(p);
            }
            (MouseButton::Middle, Some(Part::Tab(id) | Part::TabClose(id))) => {
                return vec![ChromeAction::CloseTab(id)];
            }
            _ => {
                if self.has_focus() {
                    self.blur();
                }
            }
        }
        Vec::new()
    }

    /// The button came up. Buttons act on release over the part pressed.
    pub fn mouse_up(&mut self, x: f32, y: f32, button: MouseButton) -> Vec<ChromeAction> {
        let pressed = self.pressed.take();
        if let Some(c) = &mut self.console {
            c.release();
            c.input.mouse_up();
            // A press in the messages: a drag leaves its selection, with
            // the keyboard; a click gives the input line the keyboard.
            if matches!(pressed, Some(Part::Console(ConsolePart::List))) && !c.release_text() {
                c.input.focused = true;
            }
        }
        self.address.mouse_up();
        if let Some(f) = &mut self.find {
            f.input.mouse_up();
        }
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
                Part::Find(FindPart::Prev | FindPart::Next) => {
                    if self.find.as_ref().is_some_and(|f| f.total > 0) {
                        vec![ChromeAction::FindNext {
                            forward: p == Part::Find(FindPart::Next),
                        }]
                    } else {
                        Vec::new()
                    }
                }
                Part::Find(FindPart::Close) => self.close_find(),
                Part::Suggestion(i) => self.open_suggestion(i),
                Part::Console(cp @ (ConsolePart::Levels | ConsolePart::Level(_))) => {
                    if let Some(c) = &mut self.console {
                        c.click(cp);
                    }
                    Vec::new()
                }
                Part::Console(ConsolePart::Clear) => {
                    self.console_clear();
                    vec![ChromeAction::ConsoleClear]
                }
                Part::Console(ConsolePart::Copy) => self.console_text().map(ChromeAction::CopyText).into_iter().collect(),
                Part::Console(ConsolePart::Close) => {
                    self.console = None;
                    self.console_layout_changed = true;
                    Vec::new()
                }
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
        // Ctrl+F from anywhere in the chrome opens or refocuses find.
        if input.ctrl
            && let Key::Character(c) = &input.key
            && c.eq_ignore_ascii_case("f")
        {
            self.open_find();
            return Vec::new();
        }
        if self.find_focused() {
            return self.find_key(input);
        }
        if self.console_focused() {
            return self.console_key(input);
        }
        if self.console_list_focused() {
            return self.console_list_key(input);
        }
        if !self.address.focused {
            return Vec::new();
        }
        self.dirty = true;
        // Enter on the inline completion, the first row selected, opens
        // its page (with the scheme the site was visited with).
        if input.key == Key::Enter
            && let Some((text, url)) = self.inline.clone()
            && self.address.text() == text
            && self.suggest.as_ref().is_none_or(|l| l.selected.is_none_or(|i| l.rows.get(i).is_some_and(|r| r.typed)))
        {
            self.address.set_text(display_url(&url).as_str());
            self.blur();
            return vec![ChromeAction::Navigate(url)];
        }
        if let Some(actions) = self.suggestion_key(&input) {
            return actions;
        }
        self.inserted = matches!(input.key, Key::Character(_)) && !input.ctrl;
        let before = self.address.text();
        let event = self.address.key(&mut self.fonts, &mut self.lcx, &input);
        let mut actions = Vec::new();
        self.address_edited(&before, &mut actions);
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
            Some(InputEvent::Blur) => {
                self.blur();
                actions.push(ChromeAction::FocusPage { forward: !input.shift });
            }
            Some(InputEvent::Copy(text)) => actions.push(ChromeAction::CopyText(text)),
            Some(InputEvent::RequestPaste) => actions.push(ChromeAction::RequestPaste),
            None => {}
        }
        actions
    }

    /// Clipboard text for a `RequestPaste`. Into the find box it is a
    /// new query.
    pub fn paste(&mut self, text: &str) -> Vec<ChromeAction> {
        if let Some(c) = &mut self.console
            && c.input.focused
        {
            c.input.paste(&mut self.fonts, &mut self.lcx, text);
            self.dirty = true;
            return Vec::new();
        }
        if self.address.focused {
            self.inserted = true;
            let before = self.address.text();
            self.address.paste(&mut self.fonts, &mut self.lcx, text);
            self.dirty = true;
            let mut actions = Vec::new();
            self.address_edited(&before, &mut actions);
            return actions;
        } else if let Some(f) = &mut self.find
            && f.input.focused
        {
            f.input.paste(&mut self.fonts, &mut self.lcx, text);
            self.dirty = true;
            return vec![ChromeAction::Find(f.input.text())];
        }
        Vec::new()
    }

    pub fn ime_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        if let Some(c) = &mut self.console
            && c.input.focused
        {
            c.input.ime_preedit(&mut self.fonts, &mut self.lcx, text, cursor);
            self.dirty = true;
            return;
        }
        if self.address.focused {
            self.address.ime_preedit(&mut self.fonts, &mut self.lcx, text, cursor);
            self.dirty = true;
        } else if let Some(f) = &mut self.find
            && f.input.focused
        {
            f.input.ime_preedit(&mut self.fonts, &mut self.lcx, text, cursor);
            self.dirty = true;
        }
    }

    pub fn ime_commit(&mut self, text: &str) -> Vec<ChromeAction> {
        if let Some(c) = &mut self.console
            && c.input.focused
        {
            c.input.ime_commit(&mut self.fonts, &mut self.lcx, text);
            self.dirty = true;
            return Vec::new();
        }
        if self.address.focused {
            self.inserted = true;
            let before = self.address.text();
            self.address.ime_commit(&mut self.fonts, &mut self.lcx, text);
            self.dirty = true;
            let mut actions = Vec::new();
            self.address_edited(&before, &mut actions);
            return actions;
        } else if let Some(f) = &mut self.find
            && f.input.focused
        {
            f.input.ime_commit(&mut self.fonts, &mut self.lcx, text);
            self.dirty = true;
            return vec![ChromeAction::Find(f.input.text())];
        }
        Vec::new()
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

        if let Some(f) = &mut self.find {
            let hover = match self.hover {
                Some(Part::Find(p)) => Some(p),
                _ => None,
            };
            let pressed = match self.pressed {
                Some(Part::Find(p)) => Some(p),
                _ => None,
            };
            f.draw(&mut self.fonts, &mut self.lcx, scene, self.scale, hover, pressed);
        }

        if let Some(panel) = &mut self.console {
            panel.draw(&mut self.fonts, &mut self.lcx, scene, self.scale);
        }
        if let Some(list) = &self.suggest {
            list.draw(&mut self.fonts, &mut self.lcx, scene, self.scale);
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
        Part::Find(FindPart::Prev) => Some("Previous match (Shift+Enter)"),
        Part::Find(FindPart::Next) => Some("Next match (Enter)"),
        Part::Find(FindPart::Close) => Some("Close (Escape)"),
        Part::Address | Part::Find(FindPart::Input | FindPart::Panel) | Part::Suggestion(_) | Part::Console(_) => None,
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

    #[test]
    fn the_address_bar_lists_history_matches_and_its_keys_and_clicks_work() {
        let mut c = chrome();
        c.focus_address();
        let wiki = Url::parse("https://en.wikipedia.org/wiki/Main_Page").unwrap();
        let wikt = Url::parse("https://www.wiktionary.org/").unwrap();
        let rows = || {
            vec![
                Suggestion { url: wiki.clone(), title: "Wikipedia".into() },
                Suggestion { url: wikt.clone(), title: "Wiktionary".into() },
            ]
        };
        // Typing asks for matches; an answer for older text is dropped.
        let typed = type_str(&mut c, "wik");
        assert_eq!(typed.last(), Some(&ChromeAction::AddressTyped("wik".into())));
        c.set_suggestions("wi", rows(), None);
        assert!(c.suggestions().0.is_empty(), "stale answer");
        c.set_suggestions("wik", rows(), None);
        let (list, selected) = c.suggestions();
        assert_eq!(list.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), [wiki.clone(), wikt.clone()]);
        assert_eq!(selected, None, "not an address: no row of its own, nothing selected");
        let (bx, ..) = c.address_rect();
        assert!(c.contains(bx + 20.0, c.height() + 10.0), "the list is the chrome's, over the page");
        // Down twice, up once: the box shows the selected address; Enter
        // opens it.
        c.key(KeyInput::plain(Key::ArrowDown));
        c.key(KeyInput::plain(Key::ArrowDown));
        assert_eq!(c.address_text(), wikt.as_str());
        c.key(KeyInput::plain(Key::ArrowUp));
        assert_eq!(c.address_text(), wiki.as_str());
        assert_eq!(c.key(KeyInput::plain(Key::Enter)), vec![ChromeAction::Navigate(wiki.clone())]);
        assert!(c.suggestions().0.is_empty() && !c.has_focus(), "the list closes with the box");
        // An address typed gets its own first row, selected; Escape puts
        // the typed text back and closes the list; a second Escape is
        // the box's own.
        c.focus_address();
        type_str(&mut c, "wiki.org");
        c.set_suggestions("wiki.org", rows(), None);
        let (list, selected) = c.suggestions();
        assert_eq!((list[0].0.as_str(), list[0].2, selected), ("https://wiki.org/", true, Some(0)));
        c.key(KeyInput::plain(Key::ArrowDown));
        assert_eq!(c.address_text(), wiki.as_str());
        assert!(c.key(KeyInput::plain(Key::Escape)).is_empty());
        assert_eq!(c.address_text(), "wiki.org");
        assert!(c.suggestions().0.is_empty() && c.has_focus());
        // Shift+Delete forgets the selected history row, not the typed one.
        c.set_suggestions("wiki.org", rows(), None);
        assert!(!c.key(KeyInput::shift(Key::Delete)).iter().any(|a| matches!(a, ChromeAction::RemoveHistory(_))));
        assert_eq!(c.suggestions().0.len(), 3);
        c.key(KeyInput::plain(Key::ArrowDown));
        assert_eq!(c.key(KeyInput::shift(Key::Delete)), vec![ChromeAction::RemoveHistory(wiki.clone())]);
        assert_eq!(c.suggestions().0.len(), 2, "the typed row and the other");
        // A click on a row opens it.
        let (x, y, _, h) = c.address_rect();
        let row_y = y + h + 2.0 + 4.0 + 32.0 + 16.0;
        c.mouse_move(x + 20.0, row_y);
        c.mouse_down(x + 20.0, row_y, MouseButton::Left, false);
        assert_eq!(c.mouse_up(x + 20.0, row_y, MouseButton::Left), vec![ChromeAction::Navigate(wikt.clone())]);
        // Clearing the box closes the list.
        c.focus_address();
        type_str(&mut c, "w");
        c.set_suggestions("w", rows(), None);
        c.key(KeyInput::plain(Key::Backspace));
        assert!(c.suggestions().0.is_empty());
    }

    #[test]
    fn typing_fills_in_a_visited_site_and_enter_opens_it() {
        let mut c = chrome();
        c.focus_address();
        let site = Url::parse("http://www.wikipedia.org/").unwrap();
        let fill = || Some(("wikipedia.org/".to_owned(), site.clone()));
        let fill_for = |typed: &str| Some((format!("{typed}{}", &"wikipedia.org/"[typed.len()..]), site.clone()));
        // The rest of the host goes in, selected; the first row is the site.
        type_str(&mut c, "wik");
        c.set_suggestions("wik", Vec::new(), fill());
        assert_eq!(c.address_text(), "wikipedia.org/");
        let (rows, selected) = c.suggestions();
        assert_eq!((rows[0].0.clone(), rows[0].1.as_str(), rows[0].2, selected), (site.clone(), "wikipedia.org/", true, Some(0)));
        // Typing on replaces the selected part; the fill comes again.
        assert_eq!(c.key(KeyInput::typed("i")), vec![ChromeAction::AddressTyped("wiki".into())]);
        c.set_suggestions("wiki", Vec::new(), fill_for("wiki"));
        assert_eq!(c.address_text(), "wikipedia.org/");
        // Enter opens the site, with the scheme it was visited with.
        assert_eq!(c.key(KeyInput::plain(Key::Enter)), vec![ChromeAction::Navigate(site.clone())]);
        // Backspace removes the fill, and nothing fills after a deletion.
        c.focus_address();
        type_str(&mut c, "wik");
        c.set_suggestions("wik", Vec::new(), fill());
        assert_eq!(c.key(KeyInput::plain(Key::Backspace)), vec![ChromeAction::AddressTyped("wik".into())]);
        c.set_suggestions("wik", Vec::new(), fill());
        assert_eq!(c.address_text(), "wik");
        assert_eq!(c.suggestions().0.len(), 0, "\"wik\" is no address, and no fill: no rows");
        // The caret not at the end: no fill.
        c.key(KeyInput::plain(Key::Home));
        assert_eq!(c.key(KeyInput::typed("x")), vec![ChromeAction::AddressTyped("xwik".into())]);
        c.set_suggestions("xwik", Vec::new(), Some(("xwikipedia.org/".to_owned(), site.clone())));
        assert_eq!(c.address_text(), "xwik");
        // Escape takes the fill out and closes the list.
        c.key(KeyInput::plain(Key::End));
        c.key(KeyInput::plain(Key::Backspace));
        c.key(KeyInput::plain(Key::Backspace));
        c.key(KeyInput::plain(Key::Backspace));
        c.key(KeyInput::plain(Key::Backspace));
        type_str(&mut c, "wik");
        c.set_suggestions("wik", Vec::new(), fill());
        assert_eq!(c.address_text(), "wikipedia.org/");
        c.key(KeyInput::plain(Key::Escape));
        assert_eq!(c.address_text(), "wik");
        assert!(c.suggestions().0.is_empty());
    }

    #[test]
    fn the_console_panel_shows_filters_copies_clears_resizes_and_closes() {
        use browser_ipc_types::ConsoleLevel;
        let mut c = chrome();
        c.set_window_height(600.0);
        let entry = |level, text: &str, source: Option<&str>| ConsoleEntry { level, text: text.into(), source: source.map(str::to_owned) };
        assert_eq!(c.console_height(), 0.0);
        c.toggle_console(vec![
            entry(ConsoleLevel::Log, "hello", Some("https://a.test/app.js:1:2")),
            entry(ConsoleLevel::Warn, "careful", None),
            entry(ConsoleLevel::Error, "Uncaught TypeError: x\n    at f (https://a.test/app.js:3:4)", Some("https://a.test/app.js:3:4")),
        ]);
        assert!(c.console_open() && c.take_console_layout_changed());
        assert_eq!(c.console_height(), 260.0);
        assert!(c.contains(400.0, 590.0), "the panel is the chrome's");
        // Pushed messages join; Copy all has them, with level and source.
        c.console_push(entry(ConsoleLevel::Info, "later", None));
        assert_eq!(
            c.console_text().unwrap(),
            "[log] hello  (https://a.test/app.js:1:2)\n[warn] careful\n[error] Uncaught TypeError: x\n    at f (https://a.test/app.js:3:4)  (https://a.test/app.js:3:4)\n[info] later\n"
        );
        let click = |c: &mut Chrome, x: f32, y: f32| {
            c.mouse_move(x, y);
            c.mouse_down(x, y, MouseButton::Left, false);
            c.mouse_up(x, y, MouseButton::Left)
        };
        // The toolbar: Clear, the levels menu, Copy all, ✕. The menu opens
        // under its button; each row shows or hides a level and the menu
        // stays open; a press elsewhere closes it.
        let top = 600.0 - 260.0;
        let header = top + 14.0;
        let menu_row = |i: f32| top + 28.0 + 4.0 + 13.0 + 26.0 * i;
        assert!(click(&mut c, 100.0, header).is_empty());
        assert!(c.console.as_ref().unwrap().levels_open);
        click(&mut c, 100.0, menu_row(0.0));
        assert!(!c.console_text().unwrap().contains("[error]"), "errors hidden");
        click(&mut c, 100.0, menu_row(0.0));
        click(&mut c, 100.0, menu_row(1.0));
        assert!(!c.console_text().unwrap().contains("[warn]"), "warnings hidden");
        click(&mut c, 100.0, menu_row(1.0));
        assert!(c.console.as_ref().unwrap().levels_open, "still open");
        c.mouse_down(400.0, 590.0, MouseButton::Left, false);
        c.mouse_up(400.0, 590.0, MouseButton::Left);
        assert!(!c.console.as_ref().unwrap().levels_open, "closed by a press elsewhere");
        let all = c.console_text().unwrap();
        assert_eq!(click(&mut c, 720.0, header), vec![ChromeAction::CopyText(all)]);
        // Drawing does not fail; the wheel over the list is taken.
        c.draw(&mut Scene::new());
        assert!(c.console_wheel(400.0, 590.0, 40.0) && !c.console_wheel(400.0, 200.0, 40.0));
        // Many messages: the scrollbar's arrow, track and thumb scroll.
        c.console_set((0..200).map(|i| entry(ConsoleLevel::Log, &format!("line {i}"), Some("https://a.test/x.js:1:1"))).collect());
        c.draw(&mut Scene::new());
        let scroll = |c: &Chrome| c.console.as_ref().unwrap().scroll_offset();
        let at_end = scroll(&c);
        assert!(at_end > 1000.0, "it follows the newest: {at_end}");
        let bar_x = 800.0 - 8.0;
        click(&mut c, bar_x, top + 28.0 + 5.0);
        assert_eq!(scroll(&c), at_end - 40.0, "the up arrow: a line step");
        click(&mut c, bar_x, top + 28.0 + 20.0);
        assert!(scroll(&c) < at_end - 40.0 - 100.0, "the track above the thumb: a page");
        // At the end again, the thumb sits at the track's bottom (above
        // the down arrow, the input line under the list): dragged up past
        // the top, the list is at its top.
        c.console_wheel(400.0, 590.0, 1.0e6);
        assert_eq!(scroll(&c), at_end);
        let list_bottom = 600.0 - 26.0;
        let thumb_y = list_bottom - 17.0 - 2.0;
        c.mouse_move(bar_x, thumb_y);
        c.mouse_down(bar_x, thumb_y, MouseButton::Left, false);
        c.mouse_move(bar_x, 0.0);
        c.mouse_up(bar_x, 0.0, MouseButton::Left);
        assert_eq!(scroll(&c), 0.0);
        // A held down arrow repeats.
        let before = scroll(&c);
        c.mouse_move(bar_x, list_bottom - 5.0);
        c.mouse_down(bar_x, list_bottom - 5.0, MouseButton::Left, false);
        let due = c.next_wake().expect("a repeat is due");
        assert!(c.tick(due) && scroll(&c) == before + 80.0);
        c.mouse_up(bar_x, list_bottom - 5.0, MouseButton::Left);
        assert_eq!(click(&mut c, 19.0, header), vec![ChromeAction::ConsoleClear]);
        assert_eq!(c.console_text().unwrap(), "");
        // The top edge drags; the page keeps some room.
        c.mouse_move(400.0, 341.0);
        c.mouse_down(400.0, 341.0, MouseButton::Left, false);
        c.mouse_move(400.0, 241.0);
        c.mouse_up(400.0, 241.0, MouseButton::Left);
        assert_eq!(c.console_height(), 360.0);
        assert!(c.take_console_layout_changed());
        c.mouse_move(400.0, 241.0);
        c.mouse_down(400.0, 241.0, MouseButton::Left, false);
        c.mouse_move(400.0, 0.0);
        c.mouse_up(400.0, 0.0, MouseButton::Left);
        assert_eq!(c.console_height(), 600.0 - c.height() - 80.0);
        // ✕ closes; the height is kept for the next opening.
        let close_y = 600.0 - c.console_height() + 14.0;
        click(&mut c, 800.0 - 20.0, close_y);
        assert!(!c.console_open() && c.console_height() == 0.0 && c.take_console_layout_changed());
        c.toggle_console(Vec::new());
        assert_eq!(c.console_height(), 600.0 - c.height() - 80.0);
    }

    #[test]
    fn console_messages_select_by_drag_word_and_message_and_copy() {
        use browser_ipc_types::ConsoleLevel;
        let mut c = chrome();
        c.set_window_height(600.0);
        let entry = |text: &str| ConsoleEntry { level: ConsoleLevel::Log, text: text.into(), source: None };
        c.toggle_console(vec![entry("hello world"), entry("second line"), entry("third")]);
        c.draw(&mut Scene::new());
        let (row0, row1) = (368.0 + 10.0, 390.0 + 12.0);
        let text_x = 22.0 + 1.0;
        let copy = |c: &mut Chrome| c.key(KeyInput::ctrl(Key::Character("c".into())));
        // A drag from the first message's start to past the second's end.
        c.mouse_move(text_x, row0);
        c.mouse_down(text_x, row0, MouseButton::Left, false);
        c.mouse_move(500.0, row1);
        c.mouse_up(500.0, row1, MouseButton::Left);
        assert!(c.has_focus() && c.wants_keys(), "the messages have the keyboard");
        assert!(!c.console.as_ref().unwrap().input.focused);
        assert_eq!(copy(&mut c), vec![ChromeAction::CopyText("hello world\nsecond line".into())]);
        // A double click past a line's end takes the last word; a triple
        // click the whole message.
        for n in 1..=2 {
            c.mouse_down(500.0, row0, MouseButton::Left, false);
            c.mouse_up(500.0, row0, MouseButton::Left);
            if n == 2 {
                assert_eq!(copy(&mut c), vec![ChromeAction::CopyText("world".into())]);
            }
        }
        c.mouse_down(500.0, row0, MouseButton::Left, false);
        c.mouse_up(500.0, row0, MouseButton::Left);
        assert_eq!(copy(&mut c), vec![ChromeAction::CopyText("hello world".into())]);
        // Shift+click extends; Ctrl+A takes every message.
        c.mouse_down(500.0, row1, MouseButton::Left, true);
        c.mouse_up(500.0, row1, MouseButton::Left);
        assert_eq!(copy(&mut c), vec![ChromeAction::CopyText("hello world\nsecond line".into())]);
        c.key(KeyInput::ctrl(Key::Character("a".into())));
        assert_eq!(copy(&mut c), vec![ChromeAction::CopyText("hello world\nsecond line\nthird".into())]);
        c.draw(&mut Scene::new());
        // Typing goes to the input line.
        c.key(KeyInput::typed("x"));
        assert!(c.console.as_ref().unwrap().input.focused);
        assert_eq!(c.console.as_ref().unwrap().input.text(), "x");
        // A click without a drag: no selection, the input line has the
        // keyboard.
        std::thread::sleep(std::time::Duration::from_millis(510));
        c.mouse_down(text_x, row1, MouseButton::Left, false);
        c.mouse_up(text_x, row1, MouseButton::Left);
        assert!(c.console.as_ref().unwrap().input.focused && !c.console.as_ref().unwrap().has_selection());
        // A press on the page side takes the keyboard away; the panel's
        // messages being replaced drop the selection.
        c.console.as_mut().unwrap().select_all();
        c.blur();
        assert!(!c.has_focus() && c.console.as_ref().unwrap().has_selection());
        c.console_set(vec![entry("new")]);
        assert!(!c.console.as_ref().unwrap().has_selection());
    }

    #[test]
    fn the_console_input_line_runs_code_and_walks_its_history() {
        let mut c = chrome();
        c.set_window_height(600.0);
        c.toggle_console(Vec::new());
        // A click on the input line (the panel's bottom row) takes the
        // keyboard, from the address bar too.
        c.focus_address();
        c.mouse_move(200.0, 590.0);
        c.mouse_down(200.0, 590.0, MouseButton::Left, false);
        c.mouse_up(200.0, 590.0, MouseButton::Left);
        assert!(c.has_focus() && c.wants_keys() && !c.address.focused);
        // Typing and Enter: the code goes to the page, the line empties.
        type_str(&mut c, "1 + 2");
        assert_eq!(c.key(KeyInput::plain(Key::Enter)), vec![ChromeAction::ConsoleEvaluate("1 + 2".into())]);
        assert!(c.key(KeyInput::plain(Key::Enter)).is_empty(), "an empty line runs nothing");
        type_str(&mut c, "document.title");
        c.key(KeyInput::plain(Key::Enter));
        // Up walks back, Down forward, past the newest the draft returns.
        type_str(&mut c, "dra");
        c.key(KeyInput::plain(Key::ArrowUp));
        assert_eq!(c.console.as_ref().unwrap().input.text(), "document.title");
        c.key(KeyInput::plain(Key::ArrowUp));
        assert_eq!(c.console.as_ref().unwrap().input.text(), "1 + 2");
        c.key(KeyInput::plain(Key::ArrowUp));
        assert_eq!(c.console.as_ref().unwrap().input.text(), "1 + 2", "the oldest stays");
        c.key(KeyInput::plain(Key::ArrowDown));
        c.key(KeyInput::plain(Key::ArrowDown));
        assert_eq!(c.console.as_ref().unwrap().input.text(), "dra");
        // A paste goes into the line; Tab leaves it for the page.
        c.paste("ft");
        assert_eq!(c.console.as_ref().unwrap().input.text(), "draft");
        assert_eq!(c.key(KeyInput::plain(Key::Tab)), vec![ChromeAction::FocusPage { forward: true }]);
        assert!(!c.has_focus());
        // A click in the list gives it the keyboard again.
        c.mouse_down(200.0, 450.0, MouseButton::Left, false);
        c.mouse_up(200.0, 450.0, MouseButton::Left);
        assert!(c.has_focus());
        // The echo and the result show whatever the level filters say.
        c.console_push(ConsoleEntry { level: browser_ipc_types::ConsoleLevel::Command, text: "1 + 2".into(), source: None });
        c.console_push(ConsoleEntry { level: browser_ipc_types::ConsoleLevel::Result, text: "3".into(), source: None });
        let panel = c.console.as_mut().unwrap();
        panel.show = [false; 3];
        assert_eq!(c.console_text().unwrap(), "> 1 + 2\n< 3\n");
        c.draw(&mut Scene::new());
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
        assert!(type_str(&mut c, "example.com/a b").iter().all(|a| matches!(a, ChromeAction::AddressTyped(_))));
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
        // A cut changes the text: the bar asks for matches for it too.
        assert_eq!(
            c.key(KeyInput::ctrl(Key::Character("x".into()))),
            vec![ChromeAction::AddressTyped("bc ".into()), ChromeAction::CopyText("xyz".into())]
        );
        assert_eq!(c.address_text(), "bc ");
        assert_eq!(c.key(KeyInput::ctrl(Key::Character("v".into()))), vec![ChromeAction::RequestPaste]);
        c.paste("two\nlines");
        assert_eq!(c.address_text(), "bc twolines", "pasted text is one line");
        c.key(KeyInput::ctrl(Key::Character("a".into())));
        type_str(&mut c, "q");
        assert_eq!(c.address_text(), "q");
        c.key(KeyInput::shift(Key::Home));
        assert_eq!(c.key(KeyInput::ctrl(Key::Character("c".into()))), vec![ChromeAction::CopyText("q".into())]);
        assert_eq!(c.key(KeyInput::plain(Key::Tab)), vec![ChromeAction::FocusPage { forward: true }]);
        assert!(!c.has_focus());
        c.focus_address();
        assert_eq!(c.key(KeyInput::shift(Key::Tab)), vec![ChromeAction::FocusPage { forward: false }]);
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
    fn find_bar_opens_types_steps_and_closes() {
        let mut c = chrome();
        assert!(!c.find_open());
        assert!(c.key(KeyInput::typed("x")).is_empty());
        c.open_find();
        assert!(c.find_open() && c.has_focus() && c.wants_keys());
        let (x, y, w, h) = c.find_rect().unwrap();
        assert!(y >= c.height() && x + w <= 800.0, "hangs under the toolbar at the right");
        assert!(c.contains(x + 5.0, y + 5.0), "the panel is chrome");
        assert!(!c.contains(x - 5.0, y + 5.0) && !c.contains(x + 5.0, y + h + 5.0), "the page around it is not");
        assert_eq!(c.find_status().as_deref(), Some(""));

        // Typing reports each change; Enter and Shift+Enter step.
        assert_eq!(
            type_str(&mut c, "ab"),
            vec![ChromeAction::Find("a".into()), ChromeAction::Find("ab".into())]
        );
        assert_eq!(c.key(KeyInput::plain(Key::Enter)), vec![ChromeAction::FindNext { forward: true }]);
        assert_eq!(c.key(KeyInput::shift(Key::Enter)), vec![ChromeAction::FindNext { forward: false }]);
        assert_eq!(c.find_status().as_deref(), Some(""), "no result yet");
        c.set_find_result(Some(2), 5);
        assert_eq!(c.find_status().as_deref(), Some("2/5"));
        type_str(&mut c, "c");
        assert_eq!(c.find_status().as_deref(), Some(""), "the count was for the old text");
        c.set_find_result(None, 0);
        assert_eq!(c.find_status().as_deref(), Some("No results"));

        // Buttons: disabled without matches, then they step; close closes.
        let [prev, next, close] = c.find_button_centers().unwrap();
        assert!(click(&mut c, next).is_empty());
        assert_eq!(c.mouse_move(next.0, next.1), Cursor::Default);
        c.set_find_result(Some(1), 3);
        assert_eq!(c.mouse_move(next.0, next.1), Cursor::Pointer);
        assert_eq!(click(&mut c, next), vec![ChromeAction::FindNext { forward: true }]);
        assert_eq!(click(&mut c, prev), vec![ChromeAction::FindNext { forward: false }]);
        assert!(c.has_focus(), "the buttons leave the box focused");
        assert_eq!(click(&mut c, close), vec![ChromeAction::FindClose]);
        assert!(!c.find_open() && !c.has_focus());

        // Ctrl+F from the address bar opens it; Escape closes it; a click
        // into the address bar takes focus from it.
        c.focus_address();
        assert!(c.key(KeyInput::ctrl(Key::Character("f".into()))).is_empty());
        assert!(c.find_open() && c.has_focus());
        assert_eq!(c.key(KeyInput::typed("q")), vec![ChromeAction::Find("q".into())]);
        assert_eq!(c.address_text(), "", "typing went to the find box");
        assert_eq!(c.key(KeyInput::plain(Key::Escape)), vec![ChromeAction::FindClose]);
        assert!(!c.find_open());
        assert!(c.close_find().is_empty(), "already closed");
        c.open_find();
        let (ax, ay, _, ah) = c.address_rect();
        click(&mut c, (ax + 40.0, ay + ah / 2.0));
        assert!(c.find_open() && !c.key(KeyInput::typed("z")).contains(&ChromeAction::Find("z".into())));
        assert_eq!(c.address_text(), "z");
        // Paste into the box is a query too.
        c.open_find();
        assert_eq!(c.paste("pasted"), vec![ChromeAction::Find("pasted".into())]);
        assert_eq!(c.ime_commit("!"), vec![ChromeAction::Find("pasted!".into())]);
        // Draw in every state.
        let mut scene = Scene::new();
        c.draw(&mut scene);
        c.set_find_result(None, 0);
        c.mouse_move(close.0, close.1);
        c.draw(&mut scene);
        c.resize(300.0, 2.0);
        assert!(c.find_open());
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
