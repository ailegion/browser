//! An overlay scrollbar for a scrolling area: geometry, hit testing and
//! drawing. The owner keeps the scroll offset (the tab, for the page) and
//! builds one of these from it whenever it needs to draw or hit test.

use vello::Scene;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};

/// Width of the track in logical pixels.
pub const SCROLLBAR_WIDTH: f32 = 12.0;
const MIN_THUMB: f32 = 24.0;
const INSET: f64 = 3.0;

/// A vertical scrollbar at the right edge of a viewport. Coordinates are
/// logical pixels within the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scrollbar {
    pub track: Rect,
    pub content: f32,
    pub viewport: f32,
    pub offset: f32,
}

/// Where a point on the scrollbar landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarHit {
    Thumb,
    /// On the track above the thumb: a page up.
    Before,
    /// On the track below the thumb: a page down.
    After,
}

impl Scrollbar {
    /// The bar for a viewport showing `content_height` of content at
    /// `offset`, or `None` if it all fits.
    pub fn vertical(viewport_width: f32, viewport_height: f32, content_height: f32, offset: f32) -> Option<Self> {
        if viewport_height <= 0.0 || content_height <= viewport_height + 0.5 {
            return None;
        }
        Some(Self {
            track: Rect::new(
                (viewport_width - SCROLLBAR_WIDTH).max(0.0) as f64,
                0.0,
                viewport_width as f64,
                viewport_height as f64,
            ),
            content: content_height,
            viewport: viewport_height,
            offset,
        })
    }

    pub fn max_offset(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    fn thumb_len(&self) -> f32 {
        let track_len = self.track.height() as f32;
        (track_len * self.viewport / self.content).max(MIN_THUMB).min(track_len)
    }

    pub fn thumb(&self) -> Rect {
        let track_len = self.track.height() as f32;
        let len = self.thumb_len();
        let range = track_len - len;
        let top = if self.max_offset() > 0.0 {
            range * (self.offset / self.max_offset()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Rect::new(
            self.track.x0 + INSET,
            self.track.y0 + f64::from(top),
            self.track.x1 - INSET,
            self.track.y0 + f64::from(top + len),
        )
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.track.contains((f64::from(x), f64::from(y)))
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<ScrollbarHit> {
        if !self.contains(x, y) {
            return None;
        }
        let thumb = self.thumb();
        let y = f64::from(y);
        Some(if y < thumb.y0 {
            ScrollbarHit::Before
        } else if y >= thumb.y1 {
            ScrollbarHit::After
        } else {
            ScrollbarHit::Thumb
        })
    }

    /// The offset that puts the thumb's top at `y`.
    pub fn offset_for_thumb_top(&self, y: f32) -> f32 {
        let range = self.track.height() as f32 - self.thumb_len();
        if range <= 0.0 {
            return 0.0;
        }
        ((y - self.track.y0 as f32) / range * self.max_offset()).clamp(0.0, self.max_offset())
    }

    /// Draw into a scene in physical pixels. `active` is hover or drag.
    pub fn draw(&self, scene: &mut Scene, scale: f32, active: bool) {
        let s = f64::from(scale);
        let scaled = |r: Rect| Rect::new(r.x0 * s, r.y0 * s, r.x1 * s, r.y1 * s);
        if active {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                Color::from_rgba8(0x80, 0x80, 0x88, 0x30),
                None,
                &scaled(self.track),
            );
        }
        let thumb = scaled(self.thumb());
        let color = if active {
            Color::from_rgba8(0x50, 0x50, 0x58, 0xc0)
        } else {
            Color::from_rgba8(0x60, 0x60, 0x68, 0x80)
        };
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            color,
            None,
            &RoundedRect::from_rect(thumb, thumb.width() / 2.0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_means_no_bar_and_geometry_follows_the_offset() {
        assert!(Scrollbar::vertical(800.0, 600.0, 500.0, 0.0).is_none());
        let sb = Scrollbar::vertical(800.0, 600.0, 2400.0, 0.0).expect("bar");
        assert_eq!(sb.track.x0 as f32, 800.0 - SCROLLBAR_WIDTH);
        let thumb = sb.thumb();
        assert_eq!(thumb.y0, 0.0);
        assert_eq!(thumb.height() as f32, 150.0, "a quarter of the track");
        let end = Scrollbar::vertical(800.0, 600.0, 2400.0, 1800.0).expect("bar");
        assert_eq!(end.thumb().y1 as f32, 600.0);
        assert_eq!(sb.hit(795.0, 100.0), Some(ScrollbarHit::Thumb));
        assert_eq!(sb.hit(795.0, 300.0), Some(ScrollbarHit::After));
        assert_eq!(end.hit(795.0, 100.0), Some(ScrollbarHit::Before));
        assert_eq!(sb.hit(700.0, 100.0), None);
        assert_eq!(sb.offset_for_thumb_top(0.0), 0.0);
        assert_eq!(sb.offset_for_thumb_top(450.0), 1800.0);
        assert_eq!(sb.offset_for_thumb_top(225.0), 900.0);
        assert_eq!(sb.offset_for_thumb_top(9999.0), 1800.0);
    }

    #[test]
    fn a_huge_page_keeps_a_grabbable_thumb() {
        let sb = Scrollbar::vertical(800.0, 600.0, 1.0e6, 0.0).expect("bar");
        assert_eq!(sb.thumb().height() as f32, MIN_THUMB);
        let mut scene = Scene::new();
        sb.draw(&mut scene, 2.0, true);
        sb.draw(&mut scene, 1.0, false);
    }
}
