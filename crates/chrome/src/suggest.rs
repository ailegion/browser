//! The address bar's suggestion list: what was typed, then the history's
//! matches, under the address box and over the page.

use parley::{FontContext, LayoutContext};
use url::Url;
use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};

use crate::Brush;
use crate::widgets::{TextLine, draw_text};

const ROW: f32 = 32.0;
const PAD: f32 = 4.0;
const FONT_SIZE: f32 = 13.0;

/// A page the history offers for what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub url: Url,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SuggestRow {
    pub url: Url,
    pub title: String,
    /// The row of what was typed itself (first, not from the history).
    pub typed: bool,
}

#[derive(Debug)]
pub(crate) struct SuggestList {
    /// What the user typed; the box shows it again when the list closes
    /// or the selection comes back to the first row.
    pub typed: String,
    pub rows: Vec<SuggestRow>,
    /// Logical pixels in the window.
    pub rect: Rect,
    pub hover: Option<usize>,
    pub selected: Option<usize>,
}

impl SuggestList {
    /// The list for `typed` under the address box `anchor`. The typed
    /// row, when there is one, starts selected; a history row only by the
    /// arrows (Enter then still goes where the text says).
    pub fn new(typed: String, rows: Vec<SuggestRow>, anchor: Rect) -> Self {
        let selected = rows.first().filter(|r| r.typed).map(|_| 0);
        let mut list = Self {
            typed,
            rows,
            rect: Rect::ZERO,
            hover: None,
            selected,
        };
        list.place(anchor);
        list
    }

    fn place(&mut self, anchor: Rect) {
        let height = self.rows.len() as f32 * ROW + PAD * 2.0;
        let y0 = anchor.y1 + 2.0;
        self.rect = Rect::new(anchor.x0, y0, anchor.x1, y0 + f64::from(height));
    }

    /// Drop a row (removed from the history), keeping the selection on
    /// the row that takes its place.
    pub fn remove(&mut self, index: usize, anchor: Rect) {
        if index >= self.rows.len() {
            return;
        }
        self.rows.remove(index);
        self.hover = None;
        self.selected = self.selected.map(|s| s.min(self.rows.len().saturating_sub(1))).filter(|_| !self.rows.is_empty());
        self.place(anchor);
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains((f64::from(x), f64::from(y)))
    }

    fn row_rect(&self, index: usize) -> Rect {
        let y = self.rect.y0 + f64::from(PAD + index as f32 * ROW);
        Rect::new(self.rect.x0, y, self.rect.x1, y + f64::from(ROW))
    }

    pub fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.contains(x, y) {
            return None;
        }
        (0..self.rows.len()).find(|&i| self.row_rect(i).contains((f64::from(x), f64::from(y))))
    }

    /// Move the selection a row down or up, stopping at the ends.
    pub fn step(&mut self, down: bool) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = Some(match (self.selected, down) {
            (None, true) => 0,
            (None, false) => last,
            (Some(i), true) => (i + 1).min(last),
            (Some(i), false) => i.saturating_sub(1),
        });
        self.hover = None;
    }

    pub fn draw(&self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, scene: &mut Scene, scale: f32) {
        let s = f64::from(scale);
        let r = Rect::new(self.rect.x0 * s, self.rect.y0 * s, self.rect.x1 * s, self.rect.y1 * s);
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 0, 0, 0x28),
            None,
            &RoundedRect::from_rect(Rect::new(r.x0 + 2.0 * s, r.y0 + 3.0 * s, r.x1 + 2.0 * s, r.y1 + 3.0 * s), 8.0 * s),
        );
        crate::input::rounded_box(scene, r, 8.0 * s, Color::from_rgb8(0xff, 0xff, 0xff), Color::from_rgb8(0xc4, 0xc4, 0xcc), s.max(1.0));
        let highlighted = self.hover.or(self.selected);
        for (i, row) in self.rows.iter().enumerate() {
            let rr = self.row_rect(i);
            let rr = Rect::new(rr.x0 * s, rr.y0 * s, rr.x1 * s, rr.y1 * s);
            if highlighted == Some(i) {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgb8(0xe8, 0xee, 0xfa),
                    None,
                    &RoundedRect::from_rect(Rect::new(rr.x0 + 4.0 * s, rr.y0, rr.x1 - 4.0 * s, rr.y1), 5.0 * s),
                );
            }
            // The title (or, for the typed row, the text itself), then the
            // address dimmed.
            let (main, rest) = if row.typed {
                (self.typed.clone(), "Visit".to_owned())
            } else if row.title.is_empty() {
                (row.url.to_string(), String::new())
            } else {
                (row.title.clone(), row.url.to_string())
            };
            let x = rr.x0 + 14.0 * s;
            let width = rr.width() - 28.0 * s;
            let main_width = if rest.is_empty() { width } else { width * 0.55 };
            draw_text(
                fonts,
                lcx,
                scene,
                TextLine {
                    text: &main,
                    font_size: FONT_SIZE,
                    scale,
                    color: [0x20, 0x20, 0x24, 0xff],
                    origin: (x, rr.y0),
                    max_width: main_width,
                    height: rr.height(),
                },
            );
            if !rest.is_empty() {
                draw_text(
                    fonts,
                    lcx,
                    scene,
                    TextLine {
                        text: &format!("— {rest}"),
                        font_size: FONT_SIZE - 1.0,
                        scale,
                        color: [0x1a, 0x5f, 0xb4, 0xff],
                        origin: (x + main_width + 8.0 * s, rr.y0),
                        max_width: width - main_width - 8.0 * s,
                        height: rr.height(),
                    },
                );
            }
        }
    }
}
