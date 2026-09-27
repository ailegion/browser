//! A single-line text input over parley's `PlainEditor`, which owns the
//! text, cursor, selection and IME preedit; this adds focus, a box to
//! draw in, horizontal scrolling and the drawing itself.

use parley::{FontContext, LayoutContext, PlainEditor, PositionedLayoutItem, StyleProperty};
use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};

use crate::{Brush, Key, KeyInput};

/// Logical padding between the box edge and the text.
const PAD_X: f32 = 10.0;
const CARET_WIDTH: f32 = 1.5;

pub(crate) struct TextInput {
    editor: PlainEditor<Brush>,
    pub focused: bool,
    /// Box in logical pixels, set by layout.
    pub rect: Rect,
    /// Logical space kept free at the left of the box, for an indicator.
    pub pad_left: f32,
    /// How far the text is shifted left so the caret stays visible, in
    /// physical pixels.
    scroll_x: f64,
    scale: f32,
    placeholder: String,
    dragging: bool,
    last_click: Option<(std::time::Instant, f32, f32)>,
}

/// What a key did that the owner may act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputEvent {
    /// Enter: the text as it stands.
    Submit(String),
    /// Escape: the owner restores the text and takes focus away.
    Cancel,
    /// Tab: focus moves on.
    Blur,
    /// Ctrl+C or Ctrl+X: this text goes to the clipboard.
    Copy(String),
    /// Ctrl+V: the owner fetches the clipboard and calls `paste`.
    RequestPaste,
}

impl TextInput {
    pub fn new(font_size: f32, placeholder: &str) -> Self {
        let mut editor = PlainEditor::new(font_size);
        editor.set_width(None);
        editor
            .edit_styles()
            .insert(StyleProperty::FontFamily(parley::FontFamily::Source(std::borrow::Cow::Borrowed("sans-serif"))));
        editor.edit_styles().insert(StyleProperty::Brush(Brush([0x20, 0x20, 0x24, 0xff])));
        Self {
            editor,
            focused: false,
            rect: Rect::ZERO,
            pad_left: PAD_X,
            scroll_x: 0.0,
            scale: 1.0,
            placeholder: placeholder.to_owned(),
            dragging: false,
            last_click: None,
        }
    }

    pub fn set_scale(&mut self, scale: f32) {
        if self.scale != scale {
            self.scale = scale;
            self.editor.set_scale(scale);
        }
    }

    pub fn text(&self) -> String {
        self.editor.text().to_string()
    }

    pub fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
        self.scroll_x = 0.0;
    }

    /// Text position of a logical point, in physical layout coordinates.
    fn layout_point(&self, x: f32, y: f32) -> (f32, f32) {
        let px = (x as f64 - self.rect.x0 - self.pad_left as f64) * self.scale as f64 + self.scroll_x;
        let py = (y as f64 - self.rect.y0) * self.scale as f64;
        (px as f32, py as f32)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains((x as f64, y as f64))
    }

    /// Primary button down inside the box.
    pub fn mouse_down(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, x: f32, y: f32, shift: bool) {
        let now = std::time::Instant::now();
        let double = self
            .last_click
            .is_some_and(|(at, lx, ly)| now.duration_since(at).as_millis() < 400 && (lx - x).abs() < 4.0 && (ly - y).abs() < 4.0);
        self.last_click = Some((now, x, y));
        let (px, py) = self.layout_point(x, y);
        let was_focused = self.focused;
        self.focused = true;
        let mut drv = self.editor.driver(fonts, lcx);
        if !was_focused {
            // A click into an unfocused address bar selects it all, as
            // browsers do: the common next action is typing a new one.
            drv.select_all();
        } else if double {
            drv.select_word_at_point(px, py);
        } else if shift {
            drv.shift_click_extension(px, py);
        } else {
            drv.move_to_point(px, py);
            self.dragging = true;
        }
    }

    pub fn mouse_move(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, x: f32, y: f32) {
        if self.dragging {
            let (px, py) = self.layout_point(x, y);
            self.editor.driver(fonts, lcx).extend_selection_to_point(px, py);
        }
    }

    pub fn mouse_up(&mut self) {
        self.dragging = false;
    }

    pub fn paste(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, text: &str) {
        // One line only.
        let text: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        self.editor.driver(fonts, lcx).insert_or_replace_selection(&text);
    }

    pub fn ime_preedit(
        &mut self,
        fonts: &mut FontContext,
        lcx: &mut LayoutContext<Brush>,
        text: &str,
        cursor: Option<(usize, usize)>,
    ) {
        let mut drv = self.editor.driver(fonts, lcx);
        if text.is_empty() {
            drv.clear_compose();
        } else {
            drv.set_compose(text, cursor);
        }
    }

    pub fn ime_commit(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, text: &str) {
        let mut drv = self.editor.driver(fonts, lcx);
        drv.clear_compose();
        drv.insert_or_replace_selection(text);
    }

    pub fn key(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, input: &KeyInput) -> Option<InputEvent> {
        let KeyInput { key, ctrl, shift, .. } = input;
        let (ctrl, shift) = (*ctrl, *shift);
        let mut drv = self.editor.driver(fonts, lcx);
        match key {
            Key::Character(s) if ctrl => match s.to_ascii_lowercase().as_str() {
                "a" => drv.select_all(),
                "c" => return drv.editor.selected_text().map(|t| InputEvent::Copy(t.to_owned())),
                "x" => {
                    let text = drv.editor.selected_text().map(str::to_owned)?;
                    drv.delete_selection();
                    return Some(InputEvent::Copy(text));
                }
                "v" => return Some(InputEvent::RequestPaste),
                _ => {}
            },
            Key::Character(s) => drv.insert_or_replace_selection(s),
            Key::Backspace if ctrl => drv.backdelete_word(),
            Key::Backspace => drv.backdelete(),
            Key::Delete if ctrl => drv.delete_word(),
            Key::Delete => drv.delete(),
            Key::ArrowLeft => match (ctrl, shift) {
                (true, true) => drv.select_word_left(),
                (true, false) => drv.move_word_left(),
                (false, true) => drv.select_left(),
                (false, false) => drv.move_left(),
            },
            Key::ArrowRight => match (ctrl, shift) {
                (true, true) => drv.select_word_right(),
                (true, false) => drv.move_word_right(),
                (false, true) => drv.select_right(),
                (false, false) => drv.move_right(),
            },
            Key::Home if shift => drv.select_to_text_start(),
            Key::Home => drv.move_to_text_start(),
            Key::End if shift => drv.select_to_text_end(),
            Key::End => drv.move_to_text_end(),
            Key::Enter => {
                drv.finish_compose();
                return Some(InputEvent::Submit(drv.editor.text().to_string()));
            }
            Key::Escape => return Some(InputEvent::Cancel),
            Key::Tab => return Some(InputEvent::Blur),
            // One line: nothing above or below.
            Key::ArrowUp | Key::ArrowDown => {}
        }
        None
    }

    pub fn select_all(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) {
        self.editor.driver(fonts, lcx).select_all();
    }

    pub fn blur(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) {
        self.focused = false;
        self.dragging = false;
        let mut drv = self.editor.driver(fonts, lcx);
        drv.finish_compose();
        drv.collapse_selection();
    }

    /// Where the IME should put its candidate window: logical pixels in
    /// the window.
    pub fn ime_cursor_area(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>) -> (f32, f32, f32, f32) {
        self.editor.refresh_layout(fonts, lcx);
        let area = self.editor.ime_cursor_area();
        let s = self.scale as f64;
        let x = self.rect.x0 + self.pad_left as f64 + (area.x0 - self.scroll_x) / s;
        let y = self.rect.y0 + self.text_top() / s;
        ((x as f32), (y as f32), ((area.x1 - area.x0) / s) as f32, ((area.y1 - area.y0) / s) as f32)
    }

    /// Physical y of the text's top inside the box, centering the line.
    fn text_top(&self) -> f64 {
        let box_h = self.rect.height() * self.scale as f64;
        let text_h = self
            .editor
            .try_layout()
            .map(|l| l.height() as f64)
            .unwrap_or(self.editor.get_font_size() as f64 * self.scale as f64 * 1.2);
        ((box_h - text_h) / 2.0).max(0.0).round()
    }

    /// Draw the box's text, selection and caret into `scene`, which is in
    /// physical pixels. The box itself is drawn by the owner.
    pub fn draw(&mut self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, scene: &mut Scene) {
        let s = self.scale as f64;
        let inner = Rect::new(
            (self.rect.x0 + self.pad_left as f64) * s,
            self.rect.y0 * s,
            (self.rect.x1 - PAD_X as f64) * s,
            self.rect.y1 * s,
        );
        if inner.width() <= 0.0 {
            return;
        }
        self.editor.refresh_layout(fonts, lcx);

        // Keep the caret in view.
        if let Some(caret) = self.editor.cursor_geometry(1.0) {
            let visible_w = inner.width();
            if caret.x0 - self.scroll_x > visible_w {
                self.scroll_x = caret.x0 - visible_w;
            } else if caret.x0 - self.scroll_x < 0.0 {
                self.scroll_x = caret.x0;
            }
        }
        let text_w = self.editor.try_layout().map(|l| l.full_width() as f64).unwrap_or(0.0);
        self.scroll_x = self.scroll_x.clamp(0.0, (text_w - inner.width()).max(0.0));

        let origin = Affine::translate((inner.x0 - self.scroll_x, inner.y0 + self.text_top()));
        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &inner);

        let empty = self.editor.raw_text().is_empty();
        if self.focused {
            for (rect, _) in self.editor.selection_geometry() {
                let r = Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
                scene.fill(Fill::NonZero, origin, Color::from_rgb8(0xb4, 0xd5, 0xfe), None, &r);
            }
        }
        if empty && !self.focused {
            self.draw_placeholder(fonts, lcx, scene, origin);
        } else {
            let composing = self.editor.raw_compose().clone();
            let layout = self.editor.layout(fonts, lcx);
            draw_layout(scene, layout, origin, None);
            // Underline the preedit text.
            if let Some(range) = composing {
                let sel = parley::Selection::new(
                    parley::Cursor::from_byte_index(layout, range.start, parley::Affinity::Downstream),
                    parley::Cursor::from_byte_index(layout, range.end, parley::Affinity::Upstream),
                );
                for (rect, _) in sel.geometry(layout) {
                    let r = Rect::new(rect.x0, rect.y1 - s, rect.x1, rect.y1);
                    scene.fill(Fill::NonZero, origin, Color::from_rgb8(0x20, 0x20, 0x24), None, &r);
                }
            }
        }
        if self.focused
            && let Some(caret) = self.editor.cursor_geometry((CARET_WIDTH * self.scale).max(1.0))
        {
            let r = Rect::new(caret.x0, caret.y0, caret.x1, caret.y1);
            scene.fill(Fill::NonZero, origin, Color::from_rgb8(0x20, 0x20, 0x24), None, &r);
        }
        scene.pop_layer();
    }

    fn draw_placeholder(&self, fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, scene: &mut Scene, origin: Affine) {
        let mut builder = lcx.ranged_builder(fonts, &self.placeholder, self.scale, true);
        builder.push_default(StyleProperty::FontFamily(parley::FontFamily::Source(std::borrow::Cow::Borrowed(
            "sans-serif",
        ))));
        builder.push_default(StyleProperty::FontSize(self.editor.get_font_size()));
        builder.push_default(StyleProperty::Brush(Brush([0x8a, 0x8a, 0x94, 0xff])));
        let mut layout = builder.build(&self.placeholder);
        layout.break_all_lines(None);
        layout.align(parley::Alignment::Start, parley::AlignmentOptions::default());
        draw_layout(scene, &layout, origin, None);
    }
}

/// Draw every glyph run of a layout at `origin`; `color` overrides the
/// runs' brushes when given.
pub(crate) fn draw_layout(scene: &mut Scene, layout: &parley::Layout<Brush>, origin: Affine, color: Option<Color>) {
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(run) = item else { continue };
            let r = run.run();
            let brush = color.unwrap_or_else(|| {
                let c = run.style().brush.0;
                Color::from_rgba8(c[0], c[1], c[2], c[3])
            });
            let synthesis = r.synthesis();
            let glyph_transform = synthesis
                .skew()
                .map(|deg| Affine::skew((-deg as f64).to_radians().tan(), 0.0));
            scene
                .draw_glyphs(r.font())
                .font_size(r.font_size())
                .brush(brush)
                .normalized_coords(r.normalized_coords())
                .transform(origin)
                .glyph_transform(glyph_transform)
                .hint(true)
                .draw(
                    Fill::NonZero,
                    run.positioned_glyphs().map(|g| vello::Glyph {
                        id: g.id,
                        x: g.x,
                        y: g.y,
                    }),
                );
        }
    }
}

/// A rounded box outline and fill, in physical pixels.
pub(crate) fn rounded_box(scene: &mut Scene, rect: Rect, radius: f64, fill: Color, border: Color, border_width: f64) {
    let outer = RoundedRect::from_rect(rect, radius);
    scene.fill(Fill::NonZero, Affine::IDENTITY, border, None, &outer);
    let inner = RoundedRect::from_rect(rect.inset(-border_width), (radius - border_width).max(0.0));
    scene.fill(Fill::NonZero, Affine::IDENTITY, fill, None, &inner);
}
