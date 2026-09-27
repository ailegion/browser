//! The find bar: a panel under the toolbar at the window's right with a
//! text box, the match count, previous and next, and close. The search
//! itself runs in the tab; the shell relays the query and the results.

use parley::{FontContext, LayoutContext};
use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};

use crate::input::{TextInput, rounded_box};
use crate::widgets::{Button, Icon, TextLine, draw_text, scale_rect};
use crate::{Brush, FONT_SIZE, TEXT_DIM};

const WIDTH: f32 = 380.0;
const HEIGHT: f32 = 44.0;
const PAD: f32 = 8.0;
const INPUT_WIDTH: f32 = 190.0;
const BUTTON: f32 = 28.0;
const COUNT_WIDTH: f32 = 70.0;
/// Space between the panel and the window's right edge.
const MARGIN: f32 = 12.0;

/// Things the pointer can be on in the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FindPart {
    Input,
    Prev,
    Next,
    Close,
    /// The panel itself, between the parts.
    Panel,
}

pub(crate) struct FindBar {
    pub input: TextInput,
    /// Panel in logical pixels.
    pub rect: Rect,
    pub prev: Button,
    pub next: Button,
    pub close: Button,
    /// 1-based current match and the total, as the tab last reported.
    pub current: Option<usize>,
    pub total: usize,
    /// The query the results are for; the count shows only while it
    /// matches the box, so a stale count never shows for new text.
    pub reported_for: String,
}

impl FindBar {
    pub fn new(scale: f32) -> Self {
        let mut input = TextInput::new(FONT_SIZE, "Find in page");
        input.set_scale(scale);
        input.focused = true;
        Self {
            input,
            rect: Rect::ZERO,
            prev: Button::new(Icon::ChevronUp),
            next: Button::new(Icon::ChevronDown),
            close: Button::new(Icon::Close),
            current: None,
            total: 0,
            reported_for: String::new(),
        }
    }

    /// Place the panel under a chrome `top` pixels tall in a window
    /// `width` wide.
    pub fn layout(&mut self, width: f32, top: f32) {
        let x1 = (width - MARGIN).max(WIDTH);
        let x0 = x1 - WIDTH;
        let (y0, y1) = (top, top + HEIGHT);
        self.rect = Rect::new(f64::from(x0), f64::from(y0), f64::from(x1), f64::from(y1));
        let row_y0 = y0 + PAD;
        let row_y1 = y1 - PAD;
        self.input.rect = Rect::new(
            f64::from(x0 + PAD),
            f64::from(row_y0),
            f64::from(x0 + PAD + INPUT_WIDTH),
            f64::from(row_y1),
        );
        let mut right = x1 - PAD;
        for b in [&mut self.close, &mut self.next, &mut self.prev] {
            b.rect = Rect::new(f64::from(right - BUTTON), f64::from(row_y0), f64::from(right), f64::from(row_y1));
            right -= BUTTON + 2.0;
        }
        self.set_enabled();
    }

    fn set_enabled(&mut self) {
        let some = self.total > 0;
        self.prev.enabled = some;
        self.next.enabled = some;
    }

    /// The tab answered the query.
    pub fn set_result(&mut self, current: Option<usize>, total: usize) {
        self.current = current;
        self.total = total;
        self.reported_for = self.input.text();
        self.set_enabled();
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains((f64::from(x), f64::from(y)))
    }

    pub fn part_at(&self, x: f32, y: f32) -> Option<FindPart> {
        if !self.contains(x, y) {
            return None;
        }
        Some(if self.input.contains(x, y) {
            FindPart::Input
        } else if self.prev.contains(x, y) {
            FindPart::Prev
        } else if self.next.contains(x, y) {
            FindPart::Next
        } else if self.close.contains(x, y) {
            FindPart::Close
        } else {
            FindPart::Panel
        })
    }

    /// The count label: nothing until there is a query, then the current
    /// match over the total, or that there is none.
    pub fn count_text(&self) -> String {
        let text = self.input.text();
        if text.is_empty() || text != self.reported_for {
            return String::new();
        }
        match self.current {
            Some(c) if self.total > 0 => format!("{c}/{}", self.total),
            _ => "No results".to_owned(),
        }
    }

    pub fn draw(
        &mut self,
        fonts: &mut FontContext,
        lcx: &mut LayoutContext<Brush>,
        scene: &mut Scene,
        scale: f32,
        hover: Option<FindPart>,
        pressed: Option<FindPart>,
    ) {
        let s = f64::from(scale);
        let r = scale_rect(self.rect, s);
        // Shadow, then the panel; only its bottom corners are rounded, as
        // it hangs from the toolbar.
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 0, 0, 0x28),
            None,
            &RoundedRect::new(r.x0 + 2.0 * s, r.y0, r.x1 + 2.0 * s, r.y1 + 3.0 * s, 8.0 * s),
        );
        rounded_box(
            scene,
            Rect::new(r.x0, r.y0 - 8.0 * s, r.x1, r.y1),
            8.0 * s,
            Color::from_rgb8(0xf7, 0xf7, 0xfa),
            Color::from_rgb8(0xc4, 0xc4, 0xcc),
            s.max(1.0),
        );
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgb8(0xf7, 0xf7, 0xfa),
            None,
            &Rect::new(r.x0 + s, r.y0 - s, r.x1 - s, r.y0 + s),
        );

        let box_rect = scale_rect(self.input.rect, s);
        let (border, width) = if self.input.focused {
            (Color::from_rgb8(0x4a, 0x7b, 0xd8), 2.0 * s)
        } else {
            (Color::from_rgb8(0xc4, 0xc4, 0xcc), s.max(1.0))
        };
        rounded_box(scene, box_rect, 6.0 * s, Color::from_rgb8(0xff, 0xff, 0xff), border, width);
        self.input.draw(fonts, lcx, scene);

        let count = self.count_text();
        let color = if self.total == 0 && !count.is_empty() {
            [0xc0, 0x3a, 0x2b, 0xff]
        } else {
            TEXT_DIM
        };
        draw_text(
            fonts,
            lcx,
            scene,
            TextLine {
                text: &count,
                font_size: FONT_SIZE - 1.0,
                scale,
                color,
                origin: (box_rect.x1 + 8.0 * s, box_rect.y0),
                max_width: f64::from(COUNT_WIDTH) * s,
                height: box_rect.height(),
            },
        );
        for (button, part) in [(&self.prev, FindPart::Prev), (&self.next, FindPart::Next), (&self.close, FindPart::Close)] {
            button.draw(scene, s, hover == Some(part), pressed == Some(part));
        }
    }
}
