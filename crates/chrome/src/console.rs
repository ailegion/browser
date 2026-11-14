//! The developer console panel, docked at the bottom of the window: the
//! current tab's messages, coloured by level, wrapped, scrolled; level
//! filters, Clear, Copy all, Close; its top edge drags to resize.

use browser_ipc_types::ConsoleLevel;
use parley::{FontContext, LayoutContext, StyleProperty};
use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Color, Fill};

use crate::Brush;
use crate::input::draw_layout;
use crate::widgets::{TextLine, draw_text};

/// The header bar's height, logical pixels.
const HEADER: f32 = 28.0;
/// The resize handle's thickness at the top edge.
const EDGE: f32 = 5.0;
const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 3.0;
const FONT_SIZE: f32 = 12.0;
/// The share of the list's width the text takes; the source the rest.
const TEXT_SHARE: f32 = 0.74;
/// The smallest panel and the least page left above it.
pub(crate) const MIN_HEIGHT: f32 = 90.0;
pub(crate) const MIN_PAGE: f32 = 80.0;
/// The header's buttons: label and width.
const BUTTONS: [(&str, f32); 6] = [("Errors", 58.0), ("Warnings", 72.0), ("Info", 44.0), ("Clear", 50.0), ("Copy all", 66.0), ("✕", 28.0)];

/// One message as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub level: ConsoleLevel,
    pub text: String,
    pub source: Option<String>,
}

/// What a point on the panel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsolePart {
    Resize,
    /// A level filter: errors, warnings, info (with logs).
    Filter(usize),
    Clear,
    Copy,
    Close,
    List,
}

#[derive(Debug)]
pub(crate) struct ConsolePanel {
    pub entries: Vec<ConsoleEntry>,
    /// Logical height, the header included.
    pub height: f32,
    /// Logical pixels in the window, set by `place`.
    pub rect: Rect,
    /// Which levels show: errors, warnings, info and logs.
    pub show: [bool; 3],
    /// How far the list is scrolled from its top, logical pixels.
    scroll: f32,
    /// The list follows new messages while it is at its end.
    follow: bool,
    /// Each entry's measured height at `measured_width` (logical).
    heights: Vec<f32>,
    measured_width: f32,
    pub hover: Option<ConsolePart>,
    /// While the top edge is dragged: the pointer's distance below it.
    pub resizing: Option<f32>,
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
            heights: Vec::new(),
            measured_width: 0.0,
            hover: None,
            resizing: None,
        }
    }

    /// Lay out in a window `width` × `window_height` whose page starts at
    /// `top`: the height kept within its bounds.
    pub fn place(&mut self, width: f32, window_height: f32, top: f32) {
        let max = (window_height - top - MIN_PAGE).max(MIN_HEIGHT);
        self.height = self.height.clamp(MIN_HEIGHT, max);
        let y0 = (window_height - self.height).max(top);
        self.rect = Rect::new(0.0, f64::from(y0), f64::from(width), f64::from(window_height));
        if self.follow {
            self.scroll = f32::MAX;
        }
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
        self.heights.clear();
        self.scroll = 0.0;
        self.follow = true;
    }

    pub fn set(&mut self, entries: Vec<ConsoleEntry>) {
        self.entries = entries;
        self.heights.clear();
        self.scroll = f32::MAX;
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

    fn list_rect(&self) -> Rect {
        Rect::new(self.rect.x0, self.rect.y0 + f64::from(HEADER), self.rect.x1, self.rect.y1)
    }

    fn button_rect(&self, i: usize) -> Rect {
        let y0 = self.rect.y0 + 4.0;
        let y1 = self.rect.y0 + f64::from(HEADER) - 4.0;
        if i == BUTTONS.len() - 1 {
            let w = f64::from(BUTTONS[i].1);
            return Rect::new(self.rect.x1 - 6.0 - w, y0, self.rect.x1 - 6.0, y1);
        }
        // After the "Console" label; a gap between the filters and the rest.
        let mut x = self.rect.x0 + 76.0;
        for (j, (_, w)) in BUTTONS.iter().enumerate().take(i) {
            x += f64::from(*w) + 4.0;
            if j == 2 {
                x += 14.0;
            }
        }
        Rect::new(x, y0, x + f64::from(BUTTONS[i].1), y1)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains((f64::from(x), f64::from(y)))
    }

    pub fn part_at(&self, x: f32, y: f32) -> Option<ConsolePart> {
        let (px, py) = (f64::from(x), f64::from(y));
        if !self.contains(x, y) {
            return None;
        }
        if py < self.rect.y0 + f64::from(EDGE) {
            return Some(ConsolePart::Resize);
        }
        for i in 0..BUTTONS.len() {
            if self.button_rect(i).contains((px, py)) {
                return Some(match i {
                    0..=2 => ConsolePart::Filter(i),
                    3 => ConsolePart::Clear,
                    4 => ConsolePart::Copy,
                    _ => ConsolePart::Close,
                });
            }
        }
        Some(ConsolePart::List)
    }

    /// The pointer dragged the top edge to `y` in a window of this height.
    pub fn drag_to(&mut self, y: f32, window_height: f32) {
        if let Some(grab) = self.resizing {
            self.height = window_height - (y - grab);
        }
    }

    /// Measure the entries not measured yet (all of them after a width
    /// change), wrapped to the text column.
    fn measure(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) {
        let width = self.text_width();
        if (width - self.measured_width).abs() > 0.5 {
            self.heights.clear();
            self.measured_width = width;
        }
        while self.heights.len() < self.entries.len() {
            let e = &self.entries[self.heights.len()];
            let layout = text_layout(fonts, lcx, &e.text, width, 1.0);
            self.heights.push(layout.height() + PAD_Y * 2.0);
        }
    }

    fn text_width(&self) -> f32 {
        ((self.rect.width() as f32 - PAD_X * 2.0) * TEXT_SHARE).max(40.0)
    }

    fn content_height(&self) -> f32 {
        self.entries.iter().zip(&self.heights).filter(|(e, _)| self.shown(e)).map(|(_, h)| *h).sum()
    }

    /// Scroll the list by `dy` logical pixels; at its end it follows new
    /// messages again.
    pub fn wheel(&mut self, dy: f32, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) {
        self.measure(fonts, lcx);
        let max = (self.content_height() - self.list_rect().height() as f32).max(0.0);
        self.scroll = (self.scroll.min(max) + dy).clamp(0.0, max);
        self.follow = self.scroll >= max - 1.0;
    }

    pub fn draw(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, scene: &mut Scene, scale: f32) {
        self.measure(fonts, lcx);
        let s = f64::from(scale);
        let sr = |r: Rect| Rect::new(r.x0 * s, r.y0 * s, r.x1 * s, r.y1 * s);
        let panel = sr(self.rect);
        scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(0xff, 0xff, 0xff), None, &panel);
        // The header: a line above it, its label and buttons.
        let header = Rect::new(panel.x0, panel.y0, panel.x1, panel.y0 + f64::from(HEADER) * s);
        scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(0xf1, 0xf3, 0xf4), None, &header);
        scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(0xc4, 0xc4, 0xcc), None, &Rect::new(panel.x0, panel.y0, panel.x1, panel.y0 + s.max(1.0)));
        scene.fill(Fill::NonZero, Affine::IDENTITY, Color::from_rgb8(0xda, 0xdc, 0xe0), None, &Rect::new(header.x0, header.y1 - s.max(1.0), header.x1, header.y1));
        draw_text(
            fonts,
            lcx,
            scene,
            TextLine { text: "Console", font_size: 12.5, scale, color: [0x20, 0x20, 0x24, 0xff], origin: (header.x0 + 10.0 * s, header.y0), max_width: 70.0 * s, height: header.height() },
        );
        for (i, (label, _)) in BUTTONS.iter().enumerate() {
            let r = sr(self.button_rect(i));
            let on = i < 3 && self.show[i];
            let hovered = self.hover == Some(match i {
                0..=2 => ConsolePart::Filter(i),
                3 => ConsolePart::Clear,
                4 => ConsolePart::Copy,
                _ => ConsolePart::Close,
            });
            let fill = if on {
                Some(Color::from_rgb8(0xd2, 0xe3, 0xfc))
            } else if hovered {
                Some(Color::from_rgb8(0xe3, 0xe5, 0xe8))
            } else {
                None
            };
            if let Some(fill) = fill {
                scene.fill(Fill::NonZero, Affine::IDENTITY, fill, None, &vello::kurbo::RoundedRect::from_rect(r, 4.0 * s));
            }
            draw_text(
                fonts,
                lcx,
                scene,
                TextLine { text: label, font_size: 12.0, scale, color: [0x30, 0x30, 0x38, 0xff], origin: (r.x0 + 8.0 * s, r.y0), max_width: r.width() - 8.0 * s, height: r.height() },
            );
        }
        // The list: the shown entries that reach into it.
        let list = self.list_rect();
        let view = list.height() as f32;
        let max = (self.content_height() - view).max(0.0);
        self.scroll = if self.follow { max } else { self.scroll.min(max) };
        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &sr(list));
        let mut y = list.y0 as f32 - self.scroll;
        let text_w = self.text_width();
        for (e, h) in self.entries.iter().zip(&self.heights) {
            if !self.shown(e) {
                continue;
            }
            if y + h >= list.y0 as f32 && y <= list.y1 as f32 {
                let row = Rect::new(list.x0, f64::from(y), list.x1, f64::from(y + h));
                let (bg, line, color) = match e.level {
                    ConsoleLevel::Error => (Some(Color::from_rgb8(0xfc, 0xeb, 0xeb)), Color::from_rgb8(0xf2, 0xc8, 0xc8), [0xb3, 0x1b, 0x1b, 0xff]),
                    ConsoleLevel::Warn => (Some(Color::from_rgb8(0xfe, 0xf7, 0xe0)), Color::from_rgb8(0xf2, 0xe2, 0xa8), [0x5c, 0x3c, 0x00, 0xff]),
                    ConsoleLevel::Info | ConsoleLevel::Log => (None, Color::from_rgb8(0xee, 0xee, 0xf0), [0x20, 0x21, 0x24, 0xff]),
                };
                let r = sr(row);
                if let Some(bg) = bg {
                    scene.fill(Fill::NonZero, Affine::IDENTITY, bg, None, &r);
                }
                scene.fill(Fill::NonZero, Affine::IDENTITY, line, None, &Rect::new(r.x0, r.y1 - s.max(1.0), r.x1, r.y1));
                let layout = text_layout(fonts, lcx, &e.text, text_w, scale);
                let origin = Affine::translate((r.x0 + f64::from(PAD_X) * s, r.y0 + f64::from(PAD_Y) * s));
                draw_layout(scene, &layout, origin, Some(Color::from_rgba8(color[0], color[1], color[2], color[3])));
                if let Some(source) = &e.source {
                    let x = r.x0 + f64::from(PAD_X + text_w + 12.0) * s;
                    draw_text(
                        fonts,
                        lcx,
                        scene,
                        TextLine {
                            text: source,
                            font_size: 11.0,
                            scale,
                            color: [0x5f, 0x63, 0x68, 0xff],
                            origin: (x, r.y0),
                            max_width: (r.x1 - x - f64::from(PAD_X) * s).max(0.0),
                            height: f64::from(FONT_SIZE + PAD_Y * 2.0 + 4.0) * s,
                        },
                    );
                }
            }
            y += h;
        }
        scene.pop_layer();
    }
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
