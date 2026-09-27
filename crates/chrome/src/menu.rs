//! A popup menu: a list of rows below an anchor, one hovered or keyed.

use parley::{FontContext, LayoutContext};
use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};

use crate::Brush;
use crate::widgets::{TextLine, draw_text};

const WIDTH: f32 = 240.0;
const ROW: f32 = 30.0;
const SEPARATOR: f32 = 9.0;
const PAD: f32 = 6.0;
const FONT_SIZE: f32 = 13.0;

/// What a menu row does when chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuAction {
    NewTab,
    CloseTab,
    Reload,
}

#[derive(Debug, Clone)]
pub(crate) struct MenuItem {
    /// Empty for a separator.
    pub label: &'static str,
    pub shortcut: Option<&'static str>,
    pub action: Option<MenuAction>,
    pub enabled: bool,
}

impl MenuItem {
    pub const SEPARATOR: MenuItem = MenuItem {
        label: "",
        shortcut: None,
        action: None,
        enabled: false,
    };

    pub fn new(label: &'static str, shortcut: Option<&'static str>, action: MenuAction) -> Self {
        Self {
            label,
            shortcut,
            action: Some(action),
            enabled: true,
        }
    }

    /// A row that only shows something.
    pub fn note(label: &'static str) -> Self {
        Self {
            label,
            shortcut: None,
            action: None,
            enabled: false,
        }
    }

    fn is_separator(&self) -> bool {
        self.label.is_empty()
    }

    fn height(&self) -> f32 {
        if self.is_separator() { SEPARATOR } else { ROW }
    }
}

#[derive(Debug)]
pub(crate) struct Menu {
    pub items: Vec<MenuItem>,
    /// Logical pixels in the window.
    pub rect: Rect,
    pub hover: Option<usize>,
    /// The row keyboard navigation is on.
    pub keyed: Option<usize>,
}

impl Menu {
    /// Open below `anchor`, right-aligned to it, kept inside `window_width`.
    pub fn open(items: Vec<MenuItem>, anchor: Rect, window_width: f32) -> Self {
        let height: f32 = items.iter().map(MenuItem::height).sum::<f32>() + PAD * 2.0;
        let x1 = (anchor.x1 as f32).min(window_width - 4.0);
        let x0 = (x1 - WIDTH).max(0.0);
        let y0 = anchor.y1 as f32 + 4.0;
        Self {
            items,
            rect: Rect::new(f64::from(x0), f64::from(y0), f64::from(x1), f64::from(y0 + height)),
            hover: None,
            keyed: None,
        }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains((f64::from(x), f64::from(y)))
    }

    fn row_rect(&self, index: usize) -> Rect {
        let mut y = self.rect.y0 as f32 + PAD;
        for (i, item) in self.items.iter().enumerate() {
            if i == index {
                return Rect::new(self.rect.x0, f64::from(y), self.rect.x1, f64::from(y + item.height()));
            }
            y += item.height();
        }
        Rect::ZERO
    }

    /// The enabled row at a point.
    pub fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.contains(x, y) {
            return None;
        }
        (0..self.items.len())
            .find(|&i| self.items[i].enabled && self.row_rect(i).contains((f64::from(x), f64::from(y))))
    }

    /// Move the keyed row up or down over the enabled rows.
    pub fn step(&mut self, down: bool) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        let mut i = self.keyed.or(self.hover);
        for _ in 0..n {
            i = Some(match (i, down) {
                (None, true) => 0,
                (None, false) => n - 1,
                (Some(i), true) => (i + 1) % n,
                (Some(i), false) => (i + n - 1) % n,
            });
            if i.is_some_and(|i| self.items[i].enabled) {
                break;
            }
        }
        self.keyed = i.filter(|&i| self.items[i].enabled);
        self.hover = None;
    }

    pub fn draw(&self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, scene: &mut Scene, scale: f32) {
        let s = f64::from(scale);
        let r = Rect::new(self.rect.x0 * s, self.rect.y0 * s, self.rect.x1 * s, self.rect.y1 * s);
        // Shadow, then the panel.
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 0, 0, 0x28),
            None,
            &RoundedRect::from_rect(Rect::new(r.x0 + 2.0 * s, r.y0 + 3.0 * s, r.x1 + 2.0 * s, r.y1 + 3.0 * s), 8.0 * s),
        );
        crate::input::rounded_box(
            scene,
            r,
            8.0 * s,
            Color::from_rgb8(0xff, 0xff, 0xff),
            Color::from_rgb8(0xc4, 0xc4, 0xcc),
            s.max(1.0),
        );
        let selected = self.hover.or(self.keyed);
        for (i, item) in self.items.iter().enumerate() {
            let row = self.row_rect(i);
            let row = Rect::new(row.x0 * s, row.y0 * s, row.x1 * s, row.y1 * s);
            if item.is_separator() {
                let y = (row.y0 + row.y1) / 2.0;
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgb8(0xe0, 0xe0, 0xe6),
                    None,
                    &Rect::new(row.x0 + 8.0 * s, y, row.x1 - 8.0 * s, y + s.max(1.0)),
                );
                continue;
            }
            if selected == Some(i) && item.enabled {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgb8(0xe8, 0xee, 0xfa),
                    None,
                    &RoundedRect::from_rect(Rect::new(row.x0 + 4.0 * s, row.y0, row.x1 - 4.0 * s, row.y1), 5.0 * s),
                );
            }
            let color = if item.enabled { [0x20, 0x20, 0x24, 0xff] } else { [0x90, 0x90, 0x98, 0xff] };
            draw_text(
                fonts,
                lcx,
                scene,
                TextLine {
                    text: item.label,
                    font_size: FONT_SIZE,
                    scale,
                    color,
                    origin: (row.x0 + 14.0 * s, row.y0),
                    max_width: row.width() - 100.0 * s,
                    height: row.height(),
                },
            );
            if let Some(shortcut) = item.shortcut {
                draw_text(
                    fonts,
                    lcx,
                    scene,
                    TextLine {
                        text: shortcut,
                        font_size: FONT_SIZE - 1.0,
                        scale,
                        color: [0x80, 0x80, 0x88, 0xff],
                        origin: (row.x1 - 90.0 * s, row.y0),
                        max_width: 80.0 * s,
                        height: row.height(),
                    },
                );
            }
        }
    }
}
