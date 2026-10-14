//! Classic scrollbars (roadmap 4a (4s)): the look of Chrome's non-overlay
//! scrollbars as recalled from Blink's Aura theme: a light track, a grey
//! rectangular thumb that darkens on hover and press, an arrow button at
//! each end that highlights the same way (its arrow greyed out when there
//! is nothing to scroll that way), and a square where two bars meet. The
//! geometry comes from `browser_layout::ScrollbarGeometry`; the tab draws
//! the page's bars with this and the painter draws every element's.

use browser_layout::{Rect, ScrollbarGeometry, ScrollbarPart};
use vello::Scene;
use vello::kurbo::{Affine, BezPath, Point, Rect as KRect};
use vello::peniko::{Color, Fill};

/// Which parts the pointer is on or holding, for the highlights, and the
/// `scrollbar-color` (thumb, track) when the page set one: then the
/// track, the buttons' background and the corner take the track colour
/// and the thumb and the arrows the thumb colour, in every state (Blink
/// keeps the shape and drops the hover and press shading).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollbarState {
    pub hover: Option<ScrollbarPart>,
    pub pressed: Option<ScrollbarPart>,
    pub colors: Option<(Color, Color)>,
}

const TRACK: Color = Color::from_rgb8(0xf1, 0xf1, 0xf1);
const THUMB: Color = Color::from_rgb8(0xc1, 0xc1, 0xc1);
const THUMB_HOVER: Color = Color::from_rgb8(0xa8, 0xa8, 0xa8);
const THUMB_PRESSED: Color = Color::from_rgb8(0x78, 0x78, 0x78);
const BUTTON_HOVER: Color = Color::from_rgb8(0xd2, 0xd2, 0xd2);
const BUTTON_PRESSED: Color = Color::from_rgb8(0x78, 0x78, 0x78);
const ARROW: Color = Color::from_rgb8(0x50, 0x50, 0x50);
const ARROW_DISABLED: Color = Color::from_rgb8(0xa3, 0xa3, 0xa3);
const ARROW_PRESSED: Color = Color::WHITE;
const CORNER: Color = Color::from_rgb8(0xdc, 0xdc, 0xdc);

fn krect(r: &Rect) -> KRect {
    KRect::new(r.x as f64, r.y as f64, r.right() as f64, r.bottom() as f64)
}

/// Draw one bar. `transform` maps the geometry's coordinates to the
/// scene's.
pub fn draw_scrollbar(scene: &mut Scene, transform: Affine, g: &ScrollbarGeometry, state: ScrollbarState) {
    let track = state.colors.map_or(TRACK, |(_, t)| t);
    scene.fill(Fill::NonZero, transform, track, None, &krect(&g.bar));
    let max = g.max_position();
    for (part, rect, enabled) in [
        (ScrollbarPart::ButtonStart, g.button_start_rect(), g.position > 0.0),
        (ScrollbarPart::ButtonEnd, g.button_end_rect(), g.position < max),
    ] {
        let Some(rect) = rect else { continue };
        let pressed = state.pressed == Some(part);
        let hovered = state.hover == Some(part) && state.pressed.is_none_or(|p| p == part);
        let (background, arrow) = match state.colors {
            Some((thumb, _)) => (None, thumb),
            None if !enabled => (None, ARROW_DISABLED),
            None if pressed => (Some(BUTTON_PRESSED), ARROW_PRESSED),
            None if hovered => (Some(BUTTON_HOVER), ARROW),
            None => (None, ARROW),
        };
        if let Some(bg) = background {
            scene.fill(Fill::NonZero, transform, bg, None, &krect(&rect));
        }
        scene.fill(Fill::NonZero, transform, arrow, None, &arrow_path(&rect, g.vertical, part == ScrollbarPart::ButtonStart));
    }
    if max > 0.0 {
        let thumb = g.thumb_rect();
        let color = match state.colors {
            Some((thumb, _)) => thumb,
            None if state.pressed == Some(ScrollbarPart::Thumb) => THUMB_PRESSED,
            None if state.hover == Some(ScrollbarPart::Thumb) && state.pressed.is_none() => THUMB_HOVER,
            None => THUMB,
        };
        scene.fill(Fill::NonZero, transform, color, None, &krect(&thumb));
    }
}

/// The square where a vertical and a horizontal bar meet (the track
/// colour under `scrollbar-color`).
pub fn draw_corner(scene: &mut Scene, transform: Affine, rect: &Rect) {
    draw_corner_colored(scene, transform, rect, None);
}

pub fn draw_corner_colored(scene: &mut Scene, transform: Affine, rect: &Rect, colors: Option<(Color, Color)>) {
    scene.fill(Fill::NonZero, transform, colors.map_or(CORNER, |(_, t)| t), None, &krect(rect));
}

/// A button's arrow: a triangle pointing out of the bar, half the button
/// wide and a quarter high, centred.
fn arrow_path(button: &Rect, vertical: bool, start: bool) -> BezPath {
    let cx = f64::from(button.x + button.width / 2.0);
    let cy = f64::from(button.y + button.height / 2.0);
    let size = f64::from(button.width.min(button.height));
    let half = (size / 4.0).round().max(2.0);
    let height = (half / 2.0).round().max(1.0);
    let mut path = BezPath::new();
    let (a, b, c) = match (vertical, start) {
        (true, true) => (Point::new(cx - half, cy + height / 2.0), Point::new(cx + half, cy + height / 2.0), Point::new(cx, cy - height / 2.0)),
        (true, false) => (Point::new(cx - half, cy - height / 2.0), Point::new(cx + half, cy - height / 2.0), Point::new(cx, cy + height / 2.0)),
        (false, true) => (Point::new(cx + height / 2.0, cy - half), Point::new(cx + height / 2.0, cy + half), Point::new(cx - height / 2.0, cy)),
        (false, false) => (Point::new(cx - height / 2.0, cy - half), Point::new(cx - height / 2.0, cy + half), Point::new(cx + height / 2.0, cy)),
    };
    path.move_to(a);
    path.line_to(b);
    path.line_to(c);
    path.close_path();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_draw_in_every_state() {
        let g = ScrollbarGeometry::new(Rect::new(783.0, 0.0, 17.0, 583.0), true, 17.0, 3000.0, 583.0, 100.0);
        let mut scene = Scene::new();
        for state in [
            ScrollbarState::default(),
            ScrollbarState { hover: Some(ScrollbarPart::Thumb), pressed: None, colors: None },
            ScrollbarState { hover: Some(ScrollbarPart::ButtonEnd), pressed: Some(ScrollbarPart::ButtonEnd), colors: None },
            ScrollbarState { hover: Some(ScrollbarPart::TrackAfter), pressed: Some(ScrollbarPart::Thumb), colors: None },
            ScrollbarState { hover: None, pressed: None, colors: Some((Color::from_rgb8(1, 2, 3), Color::WHITE)) },
        ] {
            draw_scrollbar(&mut scene, Affine::IDENTITY, &g, state);
        }
        let h = ScrollbarGeometry::new(Rect::new(0.0, 583.0, 783.0, 17.0), false, 17.0, 1000.0, 783.0, 0.0);
        draw_scrollbar(&mut scene, Affine::scale(2.0), &h, ScrollbarState::default());
        draw_corner(&mut scene, Affine::IDENTITY, &Rect::new(783.0, 583.0, 17.0, 17.0));
        assert_eq!(g.button, 17.0);
        assert_eq!(h.hit(5.0, 590.0), Some(ScrollbarPart::ButtonStart));
    }
}
