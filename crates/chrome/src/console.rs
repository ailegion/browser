//! The developer console panel, docked at the bottom of the window, laid
//! out as Chrome's: a toolbar (Clear, a levels menu with the error and
//! warning counts, Copy all, close), then the current tab's messages,
//! each with its level's icon, its text wrapped, its source at the right
//! (the file and line, the full address on hover), and a classic
//! scrollbar. The top edge drags to resize.

use std::time::{Duration, Instant};

use browser_ipc_types::ConsoleLevel;
use parley::{FontContext, LayoutContext, StyleProperty};
use vello::Scene;
use vello::kurbo::{Affine, BezPath, Circle, Point, Rect, RoundedRect, Stroke};
use vello::peniko::{Color, Fill};

use crate::Brush;
use crate::input::draw_layout;
use crate::widgets::{Icon, TextLine, draw_icon, draw_text, draw_tooltip};

/// The toolbar's height, logical pixels.
const TOOLBAR: f32 = 28.0;
/// The resize handle's thickness at the top edge.
const EDGE: f32 = 5.0;
const PAD_X: f32 = 8.0;
const PAD_Y: f32 = 3.0;
/// The column the level icon takes at a row's left.
const ICON_COL: f32 = 22.0;
const FONT_SIZE: f32 = 12.0;
const SOURCE_SIZE: f32 = 11.0;
/// The most of a row's width the source may take.
const SOURCE_SHARE: f32 = 0.4;
/// The list's scrollbar: as thick as the page's classic one on Windows.
const BAR: f32 = 17.0;
const MIN_THUMB: f32 = 18.0;
/// An arrow button's step; the track's is most of the view.
const LINE_STEP: f32 = 40.0;
/// A held arrow or track steps again after these (as the page's bar).
const REPEAT_INITIAL: Duration = Duration::from_millis(250);
const REPEAT: Duration = Duration::from_millis(50);
/// The smallest panel and the least page left above it.
pub(crate) const MIN_HEIGHT: f32 = 90.0;
pub(crate) const MIN_PAGE: f32 = 80.0;
/// The levels menu's rows.
const LEVELS: [&str; 3] = ["Errors", "Warnings", "Info"];
const LEVELS_W: f32 = 132.0;
const MENU_ROW: f32 = 26.0;
const MENU_W: f32 = 150.0;

const TEXT: [u8; 4] = [0x20, 0x21, 0x24, 0xff];
const DIM: [u8; 4] = [0x5f, 0x63, 0x68, 0xff];
const LINK: [u8; 4] = [0x1a, 0x73, 0xe8, 0xff];
const ERROR_TEXT: [u8; 4] = [0xc5, 0x22, 0x1f, 0xff];
const WARN_TEXT: [u8; 4] = [0x5c, 0x3c, 0x00, 0xff];
const ERROR_RED: Color = Color::from_rgb8(0xd9, 0x30, 0x25);
const WARN_YELLOW: Color = Color::from_rgb8(0xf9, 0xab, 0x00);

/// One message as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub level: ConsoleLevel,
    pub text: String,
    pub source: Option<String>,
}

/// A part of the list's scrollbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarPart {
    Up,
    Down,
    TrackBefore,
    TrackAfter,
    Thumb,
}

/// What a point on the panel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsolePart {
    Resize,
    Clear,
    /// The levels menu's button.
    Levels,
    /// A row of the open levels menu: errors, warnings, info (with logs).
    Level(usize),
    Copy,
    Close,
    Bar(BarPart),
    /// The source of the entry at this index.
    Source(usize),
    List,
}

/// What a measured entry needs for drawing: its height and its source
/// as shown (shortened to fit) with that text's width.
#[derive(Debug, Clone)]
struct Metrics {
    height: f32,
    source: String,
    source_width: f32,
}

#[derive(Debug)]
pub(crate) struct ConsolePanel {
    pub entries: Vec<ConsoleEntry>,
    /// Logical height, the toolbar included.
    pub height: f32,
    /// Logical pixels in the window, set by `place`.
    pub rect: Rect,
    /// Which levels show: errors, warnings, info and logs.
    pub show: [bool; 3],
    /// How far the list is scrolled from its top, logical pixels.
    scroll: f32,
    /// The list follows new messages while it is at its end.
    follow: bool,
    metrics: Vec<Metrics>,
    measured_width: f32,
    pub hover: Option<ConsolePart>,
    /// The part the primary button went down on.
    pub pressed: Option<ConsolePart>,
    /// While the top edge is dragged: the pointer's distance below it.
    pub resizing: Option<f32>,
    pub levels_open: bool,
    /// While the thumb is dragged: the pointer's distance below its top.
    thumb_grab: Option<f32>,
    /// A held arrow or track, and when it steps again.
    repeat: Option<(BarPart, Instant)>,
    /// The pointer, for the source's tooltip.
    pub mouse: (f32, f32),
}

impl ConsolePanel {
    pub fn new(height: f32) -> Self {
        Self {
            entries: Vec::new(),
            height,
            rect: Rect::ZERO,
            show: [true; 3],
            scroll: 0.0,
            follow: true,
            metrics: Vec::new(),
            measured_width: 0.0,
            hover: None,
            pressed: None,
            resizing: None,
            levels_open: false,
            thumb_grab: None,
            repeat: None,
            mouse: (0.0, 0.0),
        }
    }

    /// Lay out in a window `width` × `window_height` whose page starts at
    /// `top`: the height kept within its bounds.
    pub fn place(&mut self, width: f32, window_height: f32, top: f32) {
        let max = (window_height - top - MIN_PAGE).max(MIN_HEIGHT);
        self.height = self.height.clamp(MIN_HEIGHT, max);
        let y0 = (window_height - self.height).max(top);
        self.rect = Rect::new(0.0, f64::from(y0), f64::from(width), f64::from(window_height));
    }

    fn shown(&self, e: &ConsoleEntry) -> bool {
        match e.level {
            ConsoleLevel::Error => self.show[0],
            ConsoleLevel::Warn => self.show[1],
            ConsoleLevel::Info | ConsoleLevel::Log => self.show[2],
        }
    }

    pub fn push(&mut self, entry: ConsoleEntry) {
        self.entries.push(entry);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.metrics.clear();
        self.scroll = 0.0;
        self.follow = true;
    }

    pub fn set(&mut self, entries: Vec<ConsoleEntry>) {
        self.entries = entries;
        self.metrics.clear();
        self.follow = true;
    }

    /// The text Copy all puts on the clipboard: the shown messages, one
    /// per line (their own lines kept), level and source with each.
    pub fn copy_text(&self) -> String {
        let mut out = String::new();
        for e in self.entries.iter().filter(|e| self.shown(e)) {
            let level = match e.level {
                ConsoleLevel::Error => "error",
                ConsoleLevel::Warn => "warn",
                ConsoleLevel::Info => "info",
                ConsoleLevel::Log => "log",
            };
            out.push_str(&format!("[{level}] {}", e.text));
            if let Some(s) = &e.source {
                out.push_str(&format!("  ({s})"));
            }
            out.push('\n');
        }
        out
    }

    fn counts(&self) -> (usize, usize) {
        let errors = self.entries.iter().filter(|e| e.level == ConsoleLevel::Error).count();
        let warnings = self.entries.iter().filter(|e| e.level == ConsoleLevel::Warn).count();
        (errors, warnings)
    }

    // ----- geometry (logical pixels) -----

    fn toolbar(&self) -> Rect {
        Rect::new(self.rect.x0, self.rect.y0, self.rect.x1, self.rect.y0 + f64::from(TOOLBAR))
    }

    fn list(&self) -> Rect {
        Rect::new(self.rect.x0, self.rect.y0 + f64::from(TOOLBAR), self.rect.x1, self.rect.y1)
    }

    /// The rows' area: the list without the scrollbar's column.
    fn rows(&self) -> Rect {
        let l = self.list();
        Rect::new(l.x0, l.y0, (l.x1 - f64::from(BAR)).max(l.x0), l.y1)
    }

    fn button(&self, x0: f64, width: f64) -> Rect {
        let t = self.toolbar();
        Rect::new(x0, t.y0 + 3.0, x0 + width, t.y1 - 3.0)
    }

    fn clear_rect(&self) -> Rect {
        self.button(self.rect.x0 + 6.0, 26.0)
    }

    fn levels_rect(&self) -> Rect {
        self.button(self.rect.x0 + 42.0, f64::from(LEVELS_W))
    }

    fn close_rect(&self) -> Rect {
        self.button(self.rect.x1 - 32.0, 26.0)
    }

    fn copy_rect(&self) -> Rect {
        let close = self.close_rect();
        self.button(close.x0 - 6.0 - 70.0, 70.0)
    }

    fn menu_rect(&self) -> Rect {
        let l = self.levels_rect();
        let y0 = self.toolbar().y1;
        Rect::new(l.x0, y0, l.x0 + f64::from(MENU_W), y0 + f64::from(MENU_ROW) * 3.0 + 8.0)
    }

    fn menu_row(&self, i: usize) -> Rect {
        let m = self.menu_rect();
        let y = m.y0 + 4.0 + f64::from(MENU_ROW) * i as f64;
        Rect::new(m.x0, y, m.x1, y + f64::from(MENU_ROW))
    }

    fn content_height(&self) -> f32 {
        self.entries.iter().zip(&self.metrics).filter(|(e, _)| self.shown(e)).map(|(_, m)| m.height).sum()
    }

    fn max_scroll(&self) -> f32 {
        (self.content_height() - self.list().height() as f32).max(0.0)
    }

    /// The scrollbar's buttons, track and thumb, when the list overflows.
    fn bar(&self) -> Option<Bar> {
        let max = self.max_scroll();
        if max <= 0.0 {
            return None;
        }
        let l = self.list();
        let area = Rect::new(l.x1 - f64::from(BAR), l.y0, l.x1, l.y1);
        let length = area.height() as f32;
        let button = if length >= 3.0 * BAR { BAR } else { 0.0 };
        let track = (area.y0 as f32 + button, area.y1 as f32 - button);
        let track_len = (track.1 - track.0).max(0.0);
        let view = l.height() as f32;
        let thumb_len = (track_len * view / (view + max)).clamp(MIN_THUMB.min(track_len), track_len);
        let thumb_top = track.0 + (track_len - thumb_len) * (self.scroll.min(max) / max);
        Some(Bar { area, button, track, thumb: (thumb_top, thumb_top + thumb_len), max })
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains((f64::from(x), f64::from(y)))
    }

    pub fn part_at(&self, x: f32, y: f32) -> Option<ConsolePart> {
        let p = (f64::from(x), f64::from(y));
        if self.levels_open && self.menu_rect().contains(p) {
            return (0..LEVELS.len()).find(|&i| self.menu_row(i).contains(p)).map(ConsolePart::Level).or(Some(ConsolePart::List));
        }
        if !self.contains(x, y) {
            return None;
        }
        if p.1 < self.rect.y0 + f64::from(EDGE) {
            return Some(ConsolePart::Resize);
        }
        for (rect, part) in [
            (self.clear_rect(), ConsolePart::Clear),
            (self.levels_rect(), ConsolePart::Levels),
            (self.copy_rect(), ConsolePart::Copy),
            (self.close_rect(), ConsolePart::Close),
        ] {
            if rect.contains(p) {
                return Some(part);
            }
        }
        if let Some(bar) = self.bar()
            && bar.area.contains(p)
        {
            return Some(ConsolePart::Bar(bar.part_at(y)));
        }
        let rows = self.rows();
        if rows.contains(p) {
            for (i, top, _) in self.visible_rows() {
                let m = &self.metrics[i];
                if m.source.is_empty() {
                    continue;
                }
                let source = self.source_rect(top, m);
                if source.contains(p) {
                    return Some(ConsolePart::Source(i));
                }
            }
        }
        Some(ConsolePart::List)
    }

    /// The shown entries that reach into the list: index, top, height.
    fn visible_rows(&self) -> Vec<(usize, f32, f32)> {
        let l = self.list();
        let mut y = l.y0 as f32 - self.scroll;
        let mut out = Vec::new();
        for (i, (e, m)) in self.entries.iter().zip(&self.metrics).enumerate() {
            if !self.shown(e) {
                continue;
            }
            if y + m.height >= l.y0 as f32 && y <= l.y1 as f32 {
                out.push((i, y, m.height));
            }
            y += m.height;
            if y > l.y1 as f32 {
                break;
            }
        }
        out
    }

    /// The first line's height, where the icon and the source sit.
    fn line_height() -> f32 {
        FONT_SIZE * 1.35 + PAD_Y * 2.0
    }

    fn source_rect(&self, top: f32, m: &Metrics) -> Rect {
        let rows = self.rows();
        let x1 = rows.x1 - f64::from(PAD_X);
        Rect::new(x1 - f64::from(m.source_width), f64::from(top), x1, f64::from(top + Self::line_height()))
    }

    // ----- interaction -----

    /// The pointer dragged the top edge to `y` in a window of this height.
    pub fn drag_edge(&mut self, y: f32, window_height: f32) {
        if let Some(grab) = self.resizing {
            self.height = window_height - (y - grab);
        }
    }

    /// The primary button went down on `part` at `y`: the scrollbar acts
    /// (a step, a drag), the levels menu closes unless it was the target.
    pub fn press(&mut self, part: ConsolePart, y: f32, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) {
        self.measure(fonts, lcx);
        self.pressed = Some(part);
        if !matches!(part, ConsolePart::Level(_) | ConsolePart::Levels) {
            self.levels_open = false;
        }
        if let ConsolePart::Bar(b) = part {
            match b {
                BarPart::Thumb => {
                    if let Some(bar) = self.bar() {
                        self.thumb_grab = Some(y - bar.thumb.0);
                    }
                }
                _ => {
                    self.step(b);
                    self.repeat = Some((b, Instant::now() + REPEAT_INITIAL));
                }
            }
        }
    }

    /// The primary button came up: drags and repeats end.
    pub fn release(&mut self) {
        self.pressed = None;
        self.thumb_grab = None;
        self.repeat = None;
        self.resizing = None;
    }

    /// A click (down and up on the same part) on the levels button or a
    /// level's row: the menu opens or closes, the level shows or hides.
    pub fn click(&mut self, part: ConsolePart) {
        match part {
            ConsolePart::Levels => self.levels_open = !self.levels_open,
            ConsolePart::Level(i) => self.show[i] = !self.show[i],
            _ => {}
        }
    }

    /// The thumb dragged to `y`. True while a thumb drag is on.
    pub fn drag_thumb(&mut self, y: f32) -> bool {
        let Some(grab) = self.thumb_grab else { return false };
        if let Some(bar) = self.bar() {
            let range = (bar.track.1 - bar.track.0) - (bar.thumb.1 - bar.thumb.0);
            if range > 0.0 {
                let at = (y - grab - bar.track.0).clamp(0.0, range);
                self.scroll = at / range * bar.max;
                self.follow = self.scroll >= bar.max - 1.0;
            }
        }
        true
    }

    fn step(&mut self, part: BarPart) {
        let view = self.list().height() as f32;
        let page = (view * 0.875).max(view - 40.0).max(1.0);
        let d = match part {
            BarPart::Up => -LINE_STEP,
            BarPart::Down => LINE_STEP,
            BarPart::TrackBefore => -page,
            BarPart::TrackAfter => page,
            BarPart::Thumb => 0.0,
        };
        self.scroll_by(d);
    }

    fn scroll_by(&mut self, dy: f32) {
        let max = self.max_scroll();
        self.scroll = (self.scroll.min(max) + dy).clamp(0.0, max);
        self.follow = self.scroll >= max - 1.0;
    }

    /// Scroll the list by `dy` logical pixels (the wheel); at its end it
    /// follows new messages again.
    pub fn wheel(&mut self, dy: f32, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) {
        self.measure(fonts, lcx);
        self.scroll_by(dy);
    }

    /// How far the list is scrolled (tests).
    #[cfg(test)]
    pub(crate) fn scroll_offset(&self) -> f32 {
        self.scroll
    }

    /// When a held arrow or track steps next.
    pub fn next_wake(&self) -> Option<Instant> {
        self.repeat.map(|(_, at)| at)
    }

    /// A held arrow or track steps again when due, the track only while
    /// the thumb has not reached the pointer. True when it scrolled.
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some((part, at)) = self.repeat else { return false };
        if now < at {
            return false;
        }
        self.repeat = Some((part, now + REPEAT));
        let still = self.bar().is_some_and(|b| b.area.contains((f64::from(self.mouse.0), f64::from(self.mouse.1))) && b.part_at(self.mouse.1) == part);
        if !still {
            return false;
        }
        let before = self.scroll;
        self.step(part);
        self.scroll != before
    }

    // ----- measuring and drawing -----

    fn text_width(&self, source_width: f32) -> f32 {
        let rows = self.rows().width() as f32;
        let source = if source_width > 0.0 { source_width + 12.0 } else { 0.0 };
        (rows - ICON_COL - PAD_X - source).max(40.0)
    }

    /// Measure the entries not measured yet (all of them after a width
    /// change): the source as shown and the wrapped text's height.
    fn measure(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) {
        let width = self.rows().width() as f32;
        if (width - self.measured_width).abs() > 0.5 {
            self.metrics.clear();
            self.measured_width = width;
        }
        while self.metrics.len() < self.entries.len() {
            let e = &self.entries[self.metrics.len()];
            let (source, source_width) = match &e.source {
                Some(s) => fit_source(fonts, lcx, &short_source(s), width * SOURCE_SHARE),
                None => (String::new(), 0.0),
            };
            let layout = text_layout(fonts, lcx, &e.text, self.text_width(source_width), 1.0);
            let height = (layout.height() + PAD_Y * 2.0).max(Self::line_height());
            self.metrics.push(Metrics { height, source, source_width });
        }
    }

    pub fn draw(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, scene: &mut Scene, scale: f32) {
        self.measure(fonts, lcx);
        let max = self.max_scroll();
        self.scroll = if self.follow { max } else { self.scroll.min(max) };
        let s = f64::from(scale);
        let sr = |r: Rect| Rect::new(r.x0 * s, r.y0 * s, r.x1 * s, r.y1 * s);
        let fill = |scene: &mut Scene, r: Rect, c: Color| scene.fill(Fill::NonZero, Affine::IDENTITY, c, None, &r);
        let panel = sr(self.rect);
        fill(scene, panel, Color::from_rgb8(0xff, 0xff, 0xff));

        // The toolbar.
        let tb = sr(self.toolbar());
        fill(scene, tb, Color::from_rgb8(0xf1, 0xf3, 0xf4));
        fill(scene, Rect::new(panel.x0, panel.y0, panel.x1, panel.y0 + s.max(1.0)), Color::from_rgb8(0xca, 0xcd, 0xd1));
        fill(scene, Rect::new(tb.x0, tb.y1 - s.max(1.0), tb.x1, tb.y1), Color::from_rgb8(0xda, 0xdc, 0xe0));
        let hovered = |p: ConsolePart| self.hover == Some(p);
        let button_bg = |scene: &mut Scene, r: Rect, on: bool| {
            if on {
                scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(0xde, 0xe1, 0xe6), None, &RoundedRect::from_rect(r, 4.0 * s));
            }
        };
        // Clear: a circle with a slash.
        let clear = sr(self.clear_rect());
        button_bg(scene, clear, hovered(ConsolePart::Clear));
        let (cx, cy) = ((clear.x0 + clear.x1) / 2.0, (clear.y0 + clear.y1) / 2.0);
        let r = 6.0 * s;
        let stroke = Stroke::new(1.4 * s);
        scene.stroke(&stroke, Affine::IDENTITY, Color::from_rgb8(0x5f, 0x63, 0x68), None, &Circle::new((cx, cy), r));
        let mut slash = BezPath::new();
        slash.move_to((cx - r * 0.7, cy - r * 0.7));
        slash.line_to((cx + r * 0.7, cy + r * 0.7));
        scene.stroke(&stroke, Affine::IDENTITY, Color::from_rgb8(0x5f, 0x63, 0x68), None, &slash);
        fill(scene, Rect::new(clear.x1 + 5.0 * s, tb.y0 + 6.0 * s, clear.x1 + 6.0 * s, tb.y1 - 6.0 * s), Color::from_rgb8(0xda, 0xdc, 0xe0));
        // The levels menu's button: its name and a down triangle.
        let levels = sr(self.levels_rect());
        button_bg(scene, levels, hovered(ConsolePart::Levels) || self.levels_open);
        let name = if self.show.iter().all(|&v| v) { "Default levels" } else { "Custom levels" };
        draw_text(fonts, lcx, scene, TextLine { text: name, font_size: 12.0, scale, color: TEXT, origin: (levels.x0 + 8.0 * s, levels.y0), max_width: levels.width() - 24.0 * s, height: levels.height() });
        let (tx, ty) = (levels.x1 - 12.0 * s, (levels.y0 + levels.y1) / 2.0);
        let mut tri = BezPath::new();
        tri.move_to((tx - 4.0 * s, ty - 2.0 * s));
        tri.line_to((tx + 4.0 * s, ty - 2.0 * s));
        tri.line_to((tx, ty + 3.0 * s));
        tri.close_path();
        scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(0x5f, 0x63, 0x68), None, &tri);
        // The counts: errors and warnings, each with its icon.
        let (errors, warnings) = self.counts();
        let mut x = levels.x1 + 12.0 * s;
        for (count, level) in [(errors, ConsoleLevel::Error), (warnings, ConsoleLevel::Warn)] {
            if count == 0 {
                continue;
            }
            level_icon(scene, level, (x + 6.0 * s, (tb.y0 + tb.y1) / 2.0), s);
            let text = count.to_string();
            draw_text(fonts, lcx, scene, TextLine { text: &text, font_size: 12.0, scale, color: DIM, origin: (x + 15.0 * s, tb.y0), max_width: 50.0 * s, height: tb.height() });
            x += (22.0 + 7.5 * text.len() as f64) * s;
        }
        // Copy all and close.
        let copy = sr(self.copy_rect());
        button_bg(scene, copy, hovered(ConsolePart::Copy));
        draw_text(fonts, lcx, scene, TextLine { text: "Copy all", font_size: 12.0, scale, color: TEXT, origin: (copy.x0 + 10.0 * s, copy.y0), max_width: copy.width() - 10.0 * s, height: copy.height() });
        let close = sr(self.close_rect());
        button_bg(scene, close, hovered(ConsolePart::Close));
        draw_icon(scene, Icon::Close, close, 11.0 * s, Color::from_rgb8(0x5f, 0x63, 0x68));

        // The rows.
        let list = sr(self.list());
        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &list);
        let rows = sr(self.rows());
        for (i, top, height) in self.visible_rows() {
            let e = &self.entries[i];
            let m = &self.metrics[i];
            let row = Rect::new(rows.x0, f64::from(top) * s, rows.x1, f64::from(top + height) * s);
            let (bg, line, color) = match e.level {
                ConsoleLevel::Error => (Some(Color::from_rgb8(0xff, 0xf0, 0xf0)), Color::from_rgb8(0xff, 0xd6, 0xd6), ERROR_TEXT),
                ConsoleLevel::Warn => (Some(Color::from_rgb8(0xff, 0xfb, 0xe5)), Color::from_rgb8(0xff, 0xf5, 0xc2), WARN_TEXT),
                ConsoleLevel::Info | ConsoleLevel::Log => (None, Color::from_rgb8(0xf0, 0xf0, 0xf0), TEXT),
            };
            if let Some(bg) = bg {
                fill(scene, row, bg);
            }
            fill(scene, Rect::new(row.x0, row.y1 - s.max(1.0), row.x1, row.y1), line);
            let first_line = f64::from(Self::line_height()) * s;
            if matches!(e.level, ConsoleLevel::Error | ConsoleLevel::Warn) {
                level_icon(scene, e.level, (row.x0 + f64::from(ICON_COL) / 2.0 * s + 2.0 * s, row.y0 + first_line / 2.0), s);
            }
            let layout = text_layout(fonts, lcx, &e.text, self.text_width(m.source_width), scale);
            let origin = Affine::translate((row.x0 + f64::from(ICON_COL) * s, row.y0 + f64::from(PAD_Y) * s));
            draw_layout(scene, &layout, origin, Some(Color::from_rgba8(color[0], color[1], color[2], color[3])));
            if !m.source.is_empty() {
                let r = sr(self.source_rect(top, m));
                let link = if hovered(ConsolePart::Source(i)) { [0x17, 0x4e, 0xa6, 0xff] } else { LINK };
                draw_text(fonts, lcx, scene, TextLine { text: &m.source, font_size: SOURCE_SIZE, scale, color: link, origin: (r.x0, r.y0), max_width: r.width() + 2.0 * s, height: r.height() });
                if hovered(ConsolePart::Source(i)) {
                    fill(scene, Rect::new(r.x0, r.y1 - 4.0 * s, r.x1, r.y1 - 4.0 * s + s.max(1.0)), Color::from_rgba8(link[0], link[1], link[2], link[3]));
                }
            }
        }
        scene.pop_layer();

        // The scrollbar, as the page's classic one.
        if let Some(bar) = self.bar() {
            let pressed = match self.pressed {
                Some(ConsolePart::Bar(b)) => Some(b),
                _ => None,
            };
            let hover = match self.hover {
                Some(ConsolePart::Bar(b)) => Some(b),
                _ => None,
            };
            bar.draw(scene, s, self.scroll, hover, pressed);
        }

        // The levels menu over the list.
        if self.levels_open {
            let m = sr(self.menu_rect());
            scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgba8(0, 0, 0, 0x28), None, &RoundedRect::from_rect(Rect::new(m.x0 + 2.0 * s, m.y0 + 3.0 * s, m.x1 + 2.0 * s, m.y1 + 3.0 * s), 6.0 * s));
            crate::input::rounded_box(scene, m, 6.0 * s, Color::from_rgb8(0xff, 0xff, 0xff), Color::from_rgb8(0xc4, 0xc4, 0xcc), s.max(1.0));
            for (i, label) in LEVELS.iter().enumerate() {
                let row = sr(self.menu_row(i));
                if hovered(ConsolePart::Level(i)) {
                    fill(scene, Rect::new(row.x0 + 4.0 * s, row.y0, row.x1 - 4.0 * s, row.y1), Color::from_rgb8(0xe8, 0xee, 0xfa));
                }
                if self.show[i] {
                    let (cx, cy) = (row.x0 + 16.0 * s, (row.y0 + row.y1) / 2.0);
                    let mut check = BezPath::new();
                    check.move_to((cx - 4.5 * s, cy));
                    check.line_to((cx - 1.5 * s, cy + 3.5 * s));
                    check.line_to((cx + 5.0 * s, cy - 4.0 * s));
                    scene.stroke(&Stroke::new(1.6 * s), Affine::IDENTITY, Color::from_rgb8(0x20, 0x21, 0x24), None, &check);
                }
                draw_text(fonts, lcx, scene, TextLine { text: label, font_size: 12.0, scale, color: TEXT, origin: (row.x0 + 30.0 * s, row.y0), max_width: row.width() - 34.0 * s, height: row.height() });
            }
        }

        // The full address of a hovered source.
        if let Some(ConsolePart::Source(i)) = self.hover
            && let Some(full) = self.entries.get(i).and_then(|e| e.source.as_deref())
        {
            let at = ((f64::from(self.mouse.0) - 120.0) * s, (f64::from(self.mouse.1) - 30.0) * s);
            draw_tooltip(fonts, lcx, scene, full, (at.0.max(0.0), at.1), scale, panel.x1);
        }
    }
}

/// The list's scrollbar laid out, logical pixels.
#[derive(Debug, Clone, Copy)]
struct Bar {
    area: Rect,
    /// Each arrow button's length (0 when the bar is too short).
    button: f32,
    track: (f32, f32),
    thumb: (f32, f32),
    max: f32,
}

impl Bar {
    fn part_at(&self, y: f32) -> BarPart {
        if y < self.area.y0 as f32 + self.button {
            BarPart::Up
        } else if y >= self.area.y1 as f32 - self.button {
            BarPart::Down
        } else if y < self.thumb.0 {
            BarPart::TrackBefore
        } else if y >= self.thumb.1 {
            BarPart::TrackAfter
        } else {
            BarPart::Thumb
        }
    }

    /// Drawn as the page's classic bar: track, arrows, thumb, with their
    /// hover and pressed shades.
    fn draw(&self, scene: &mut Scene, s: f64, scroll: f32, hover: Option<BarPart>, pressed: Option<BarPart>) {
        let sr = |r: Rect| Rect::new(r.x0 * s, r.y0 * s, r.x1 * s, r.y1 * s);
        scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(0xf1, 0xf1, 0xf1), None, &sr(self.area));
        if self.button > 0.0 {
            let b = f64::from(self.button);
            for (part, rect, enabled, up) in [
                (BarPart::Up, Rect::new(self.area.x0, self.area.y0, self.area.x1, self.area.y0 + b), scroll > 0.0, true),
                (BarPart::Down, Rect::new(self.area.x0, self.area.y1 - b, self.area.x1, self.area.y1), scroll < self.max, false),
            ] {
                let r = sr(rect);
                let (bg, arrow) = if !enabled {
                    (None, Color::from_rgb8(0xa3, 0xa3, 0xa3))
                } else if pressed == Some(part) {
                    (Some(Color::from_rgb8(0x78, 0x78, 0x78)), Color::WHITE)
                } else if hover == Some(part) && pressed.is_none() {
                    (Some(Color::from_rgb8(0xd2, 0xd2, 0xd2)), Color::from_rgb8(0x50, 0x50, 0x50))
                } else {
                    (None, Color::from_rgb8(0x50, 0x50, 0x50))
                };
                if let Some(bg) = bg {
                    scene.fill(Fill::NonZero, Affine::IDENTITY, bg, None, &r);
                }
                let (cx, cy) = ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
                let half = (r.width().min(r.height()) / 4.0).round().max(2.0);
                let h = (half / 2.0).round().max(1.0);
                let mut path = BezPath::new();
                if up {
                    path.move_to(Point::new(cx - half, cy + h / 2.0));
                    path.line_to(Point::new(cx + half, cy + h / 2.0));
                    path.line_to(Point::new(cx, cy - h / 2.0));
                } else {
                    path.move_to(Point::new(cx - half, cy - h / 2.0));
                    path.line_to(Point::new(cx + half, cy - h / 2.0));
                    path.line_to(Point::new(cx, cy + h / 2.0));
                }
                path.close_path();
                scene.fill(Fill::NonZero, Affine::IDENTITY, arrow, None, &path);
            }
        }
        let thumb = Rect::new(self.area.x0 + 2.0, f64::from(self.thumb.0), self.area.x1 - 2.0, f64::from(self.thumb.1));
        let color = if pressed == Some(BarPart::Thumb) {
            Color::from_rgb8(0x78, 0x78, 0x78)
        } else if hover == Some(BarPart::Thumb) && pressed.is_none() {
            Color::from_rgb8(0xa8, 0xa8, 0xa8)
        } else {
            Color::from_rgb8(0xc1, 0xc1, 0xc1)
        };
        scene.fill(Fill::NonZero, Affine::IDENTITY, color, None, &sr(thumb));
    }
}

/// A level's icon centred at `c` (physical pixels): a red circle with a
/// white cross for errors, a yellow triangle with a dark mark for
/// warnings.
fn level_icon(scene: &mut Scene, level: ConsoleLevel, c: (f64, f64), s: f64) {
    match level {
        ConsoleLevel::Error => {
            scene.fill(Fill::NonZero, Affine::IDENTITY, ERROR_RED, None, &Circle::new(c, 5.5 * s));
            let d = 2.2 * s;
            let mut x = BezPath::new();
            x.move_to((c.0 - d, c.1 - d));
            x.line_to((c.0 + d, c.1 + d));
            x.move_to((c.0 + d, c.1 - d));
            x.line_to((c.0 - d, c.1 + d));
            scene.stroke(&Stroke::new(1.4 * s), Affine::IDENTITY, Color::WHITE, None, &x);
        }
        ConsoleLevel::Warn => {
            let mut t = BezPath::new();
            t.move_to((c.0, c.1 - 5.5 * s));
            t.line_to((c.0 + 6.0 * s, c.1 + 5.0 * s));
            t.line_to((c.0 - 6.0 * s, c.1 + 5.0 * s));
            t.close_path();
            scene.fill(Fill::NonZero, Affine::IDENTITY, WARN_YELLOW, None, &t);
            let dark = Color::from_rgb8(0x20, 0x21, 0x24);
            scene.fill(Fill::NonZero, Affine::IDENTITY, dark, None, &Rect::new(c.0 - 0.7 * s, c.1 - 2.5 * s, c.0 + 0.7 * s, c.1 + 1.5 * s));
            scene.fill(Fill::NonZero, Affine::IDENTITY, dark, None, &Rect::new(c.0 - 0.7 * s, c.1 + 2.5 * s, c.0 + 0.7 * s, c.1 + 3.9 * s));
        }
        ConsoleLevel::Info | ConsoleLevel::Log => {}
    }
}

/// A source as Chrome shows it: the file's name and the line
/// (`app.js:12`), `(index)` for an address ending in `/`; the column
/// dropped.
fn short_source(source: &str) -> String {
    let mut parts = source.rsplitn(3, ':');
    let (last, mid, rest) = (parts.next(), parts.next(), parts.next());
    let numeric = |s: Option<&str>| s.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let (address, line) = match (last, mid, rest) {
        (Some(_), Some(line), Some(rest)) if numeric(last) && numeric(Some(line)) => (rest, Some(line)),
        _ => match source.rsplit_once(':') {
            Some((rest, line)) if numeric(Some(line)) => (rest, Some(line)),
            _ => (source, None),
        },
    };
    let address = address.split(['?', '#']).next().unwrap_or(address);
    let name = match address.rsplit_once('/') {
        Some((_, "")) => "(index)",
        Some((_, name)) => name,
        None => address,
    };
    match line {
        Some(line) => format!("{name}:{line}"),
        None => name.to_owned(),
    }
}

/// `text` as it fits in `max` logical pixels: whole, or cut with `…`.
/// With its width.
fn fit_source(fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, text: &str, max: f32) -> (String, f32) {
    let width = |fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, t: &str| source_layout(fonts, lcx, t).width();
    let full = width(fonts, lcx, text);
    if full <= max {
        return (text.to_owned(), full);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = chars.len();
    while keep > 1 {
        keep -= 1;
        let cut: String = chars[..keep].iter().collect::<String>() + "…";
        let w = width(fonts, lcx, &cut);
        if w <= max {
            return (cut, w);
        }
    }
    let cut = "…".to_owned();
    let w = width(fonts, lcx, &cut);
    (cut, w)
}

fn source_layout(fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, text: &str) -> parley::Layout<Brush> {
    let mut builder = lcx.ranged_builder(fonts, text, 1.0, true);
    builder.push_default(StyleProperty::FontFamily(parley::FontFamily::Source(std::borrow::Cow::Borrowed("sans-serif"))));
    builder.push_default(StyleProperty::FontSize(SOURCE_SIZE));
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    layout
}

/// A message's text, wrapped to `width` logical pixels, at `scale`.
fn text_layout(fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, text: &str, width: f32, scale: f32) -> parley::Layout<Brush> {
    let text = if text.is_empty() { " " } else { text };
    let mut builder = lcx.ranged_builder(fonts, text, scale, true);
    builder.push_default(StyleProperty::FontFamily(parley::FontFamily::Source(std::borrow::Cow::Borrowed("monospace"))));
    builder.push_default(StyleProperty::FontSize(FONT_SIZE));
    let mut layout = builder.build(text);
    layout.break_all_lines(Some(width * scale));
    layout.align(parley::Alignment::Start, parley::AlignmentOptions::default());
    layout
}

#[cfg(test)]
mod tests {
    use super::short_source;

    #[test]
    fn sources_show_as_file_and_line() {
        assert_eq!(short_source("https://react.dev/_next/static/chunks/main-8bc6.js:1:86880"), "main-8bc6.js:1");
        assert_eq!(short_source("https://a.test/:3:5"), "(index):3");
        assert_eq!(short_source("https://a.test/app.js?v=2:7:1"), "app.js:7");
        assert_eq!(short_source("https://a.test/missing.css"), "missing.css");
        assert_eq!(short_source("https://a.test/page:12"), "page:12");
    }
}
