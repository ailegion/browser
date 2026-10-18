//! Text decoration geometry: where each decorating box's lines go on a
//! text fragment, and where an underline or overline skips glyph ink.
//!
//! CSS Text Decoration 3 §2.1: a line propagated from a decorating box is
//! drawn with that box's colour, style and thickness, at the position the
//! box's own first available font and baseline give it, whatever font the
//! text under it is shaped in. `text-decoration-skip-ink` (default `auto`)
//! interrupts underlines and overlines where glyph outlines cross them.

use std::collections::HashMap;
use std::sync::Arc;

use browser_style::{
    ComputedStyle, DecorationFont, DecorationThickness, FontStyle, TextDecorationSkipInk, UnderlineOffset,
};
use fontique::{Attributes, FontStyle as QFontStyle, FontWeight, FontWidth, GenericFamily, QueryFamily, QueryStatus};
use parley::FontContext;
use parley::style::FontFamilyName;
use skrifa::instance::{LocationRef, NormalizedCoord, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{GlyphId, MetadataProvider};

use crate::{Decoration, DecorationLine, TextFragment};

/// One family of a `font-family` list, as fontique queries it.
pub(crate) enum Family {
    Named(String),
    Generic(GenericFamily),
}

impl Family {
    /// The families of a CSS `font-family` list, in order.
    pub(crate) fn parse_list(list: &str) -> Vec<Family> {
        FontFamilyName::parse_css_list(list)
            .map_while(Result::ok)
            .map(|f| match f {
                FontFamilyName::Named(name) => Family::Named(name.to_string()),
                FontFamilyName::Generic(generic) => Family::Generic(generic),
            })
            .collect()
    }

    pub(crate) fn query(&self) -> QueryFamily<'_> {
        match self {
            Family::Named(name) => QueryFamily::Named(name.as_str()),
            Family::Generic(generic) => QueryFamily::Generic(*generic),
        }
    }
}

/// A style's first available font's metrics at the style's font size, as
/// distances from the baseline in px, positive away from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    /// The font's line gap.
    pub leading: f32,
    /// Height of the lowercase x: the font's own, else the `x` glyph's
    /// measured, else half the ascent (as browsers fall back).
    pub x_height: f32,
    /// Top of the underline, below the baseline (never above it).
    pub underline_top: f32,
    pub underline_thickness: f32,
    /// Top of the line-through, above the baseline.
    pub strikeout_top: f32,
    pub strikeout_thickness: f32,
}

/// The font a style asks for, as the key its metrics are looked up by.
pub(crate) fn font_of(style: &ComputedStyle) -> DecorationFont {
    DecorationFont {
        family: style.font_family.clone(),
        size: style.font_size,
        weight: style.font_weight,
        style: style.font_style,
        stretch: style.font_stretch,
        optical_sizing: style.font_optical_sizing,
        variations: style.font_variation_settings.clone(),
    }
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct FontKey {
    family: Arc<str>,
    size: u32,
    weight: u16,
    style: u8,
    stretch: u32,
    optical_sizing: bool,
    variations: Box<[([u8; 4], u32)]>,
}

impl FontKey {
    fn of(f: &DecorationFont) -> Self {
        Self {
            family: f.family.clone(),
            size: f.size.to_bits(),
            weight: f.weight,
            style: match f.style {
                FontStyle::Normal => 0,
                FontStyle::Italic => 1,
                FontStyle::Oblique => 2,
            },
            stretch: f.stretch.to_bits(),
            optical_sizing: f.optical_sizing == browser_style::FontOpticalSizing::Auto,
            variations: f.variations.iter().map(|(t, v)| (*t, v.to_bits())).collect(),
        }
    }
}

/// The variations a font of `font` is set with: those font matching asks
/// for, then the optical size (`font-optical-sizing: auto`, unless the
/// settings name `opsz`), then `font-variation-settings`, a later one
/// winning per axis (as the text's runs).
fn variations_of(font: &DecorationFont, synthesis: &[(skrifa::Tag, f32)]) -> Vec<(skrifa::Tag, f32)> {
    let mut out: Vec<(skrifa::Tag, f32)> = synthesis.to_vec();
    if font.optical_sizing == browser_style::FontOpticalSizing::Auto && !font.variations.iter().any(|(t, _)| t == b"opsz") {
        out.push((skrifa::Tag::new(b"opsz"), font.size.max(0.0)));
    }
    out.extend(font.variations.iter().map(|(t, v)| (skrifa::Tag::new(t), *v)));
    out
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct GlyphKey {
    font: u64,
    index: u32,
    glyph: u32,
    size: u32,
    coords: Box<[i16]>,
}

/// How far an ink gap extends past the ink on each side: the line's
/// thickness, at most this (Blink's `kDecorationClipMaxDilation`).
const MAX_DILATION: f32 = 13.0;

/// Flattened outlines kept before the cache is emptied.
const OUTLINE_CACHE_LIMIT: usize = 4096;

/// Flattening tolerance for glyph outlines, in px; the gaps are widened by
/// at least a pixel on each side, so this is well under what shows.
const FLATTEN_TOLERANCE: f32 = 0.1;

/// Computes and caches decoration geometry for a `LayoutEngine`.
#[derive(Default)]
pub(crate) struct Decorator {
    metrics: HashMap<FontKey, FontMetrics>,
    /// Flattened glyph outlines as line segments in px, y up, origin at
    /// the glyph's baseline point.
    outlines: HashMap<GlyphKey, Arc<[[f32; 4]]>>,
}

impl Decorator {
    /// Forget the cached font metrics: the fonts a family resolves to
    /// changed (a web font registered or cleared).
    pub(crate) fn forget_metrics(&mut self) {
        self.metrics.clear();
    }

    /// The lines on `text`, whose element's style is `style`: one per
    /// line of every decorating box in `style.decorations`, outermost
    /// box first. `text.baseline` must be set.
    pub(crate) fn decorations(
        &mut self,
        fonts: &mut FontContext,
        style: &ComputedStyle,
        text: &TextFragment,
    ) -> Vec<Decoration> {
        let mut out = Vec::new();
        if style.decorations.is_empty() {
            return out;
        }
        // `auto` and `all` both skip ink (both browsers skip for `auto`
        // in every script); line-throughs never skip.
        let skip_ink = style.text_decoration_skip_ink != TextDecorationSkipInk::None;
        for d in style.decorations.iter() {
            let m = self.font_metrics(fonts, &d.font);
            // `auto` and `from-font` take the decorating font's own
            // thickness; a length is as given, a percentage of the
            // decorating box's font size.
            let thickness = |from_font: f32| {
                match d.thickness {
                    DecorationThickness::Auto | DecorationThickness::FromFont => from_font,
                    DecorationThickness::Length(lp) => lp.resolve(d.font.size),
                }
                .max(0.0)
            };
            if d.lines.underline {
                let t = thickness(m.underline_thickness);
                // `under` sits below the decorating font's descent (CSS
                // Text Decoration 4); `auto` and `from-font` at the font's
                // underline position (as Firefox; Chrome's `auto` leaves a
                // gap under the baseline instead). `text-underline-offset`
                // moves it outward.
                let base = if d.underline_position.under { m.descent } else { m.underline_top };
                let offset = match d.underline_offset {
                    UnderlineOffset::Auto => 0.0,
                    UnderlineOffset::Px(v) => v,
                };
                let y = text.baseline + base + offset;
                let gaps = if skip_ink { self.ink_gaps(text, y, t) } else { Vec::new() };
                out.push(Decoration {
                    line: DecorationLine::Underline,
                    y,
                    thickness: t,
                    color: d.color,
                    style: d.style,
                    gaps,
                });
            }
            if d.lines.overline {
                // At the decorating font's ascent, running down into the
                // text's box, with the underline's thickness.
                let t = thickness(m.underline_thickness);
                let y = text.baseline - m.ascent;
                let gaps = if skip_ink { self.ink_gaps(text, y, t) } else { Vec::new() };
                out.push(Decoration {
                    line: DecorationLine::Overline,
                    y,
                    thickness: t,
                    color: d.color,
                    style: d.style,
                    gaps,
                });
            }
            if d.lines.line_through {
                // At the decorating font's strikeout position, with its
                // strikeout thickness (as Firefox).
                out.push(Decoration {
                    line: DecorationLine::LineThrough,
                    y: text.baseline - m.strikeout_top,
                    thickness: thickness(m.strikeout_thickness),
                    color: d.color,
                    style: d.style,
                    gaps: Vec::new(),
                });
            }
        }
        out
    }

    /// The metrics of the first available font of `font`'s family list
    /// at `font`'s size, weight, style and stretch; cached per font.
    pub(crate) fn font_metrics(&mut self, fonts: &mut FontContext, font: &DecorationFont) -> FontMetrics {
        let key = FontKey::of(font);
        if let Some(m) = self.metrics.get(&key) {
            return *m;
        }
        let families = Family::parse_list(&font.family);
        let mut found = None;
        {
            let mut query = fonts.collection.query(&mut fonts.source_cache);
            query.set_families(families.iter().map(Family::query));
            query.set_attributes(Attributes::new(
                FontWidth::from_percentage(font.stretch),
                match font.style {
                    FontStyle::Normal => QFontStyle::Normal,
                    FontStyle::Italic => QFontStyle::Italic,
                    FontStyle::Oblique => QFontStyle::Oblique(None),
                },
                FontWeight::new(font.weight as f32),
            ));
            // The first available font: the first whose `unicode-range`
            // does not exclude U+0020 (CSS Fonts 4 §5.5), at its face's
            // `size-adjust` and with its metric overrides.
            query.matches_with(|f| {
                if f.unicode_range.as_ref().is_some_and(|r| !r.iter().any(|&(a, b)| a <= 0x20 && 0x20 <= b)) {
                    return QueryStatus::Continue;
                }
                let fm = f.face_metrics;
                let size = font.size * fm.size_adjust;
                let vars = variations_of(font, f.synthesis.variation_settings());
                found = font_metrics(f.blob.as_ref(), f.index, &vars, size).map(|mut m| {
                    if let Some(a) = fm.ascent {
                        m.ascent = a * size;
                    }
                    if let Some(d) = fm.descent {
                        m.descent = d * size;
                    }
                    if let Some(g) = fm.line_gap {
                        m.leading = g * size;
                    }
                    m
                });
                QueryStatus::Stop
            });
        }
        let m = found.unwrap_or_else(|| fallback_metrics(font.size));
        self.metrics.insert(key, m);
        m
    }

    /// Where a line from `top` to `top + thickness` (fragment
    /// coordinates, y down) crosses the glyphs' ink: x ranges relative to
    /// the fragment's left, each widened by the thickness (at most
    /// `MAX_DILATION`) on both sides, merged and sorted.
    fn ink_gaps(&mut self, text: &TextFragment, top: f32, thickness: f32) -> Vec<(f32, f32)> {
        let bottom = top + thickness.max(1.0);
        let dilation = thickness.clamp(1.0, MAX_DILATION);
        // A synthetic italic skews the painted outline: x' = x + tan·y.
        let skew = text.skew.map(|deg| deg.to_radians().tan()).unwrap_or(0.0);
        let mut ranges: Vec<(f32, f32)> = Vec::new();
        for g in &text.glyphs {
            let outline = self.outline(text, g.id);
            // The band in the glyph's outline space: y up, origin at the
            // glyph's baseline point.
            let (lo, hi) = (g.y - bottom, g.y - top);
            for seg in outline.iter() {
                let seg = [seg[0] + skew * seg[1], seg[1], seg[2] + skew * seg[3], seg[3]];
                if let Some((x0, x1)) = segment_span(&seg, lo, hi) {
                    ranges.push((g.x + x0 - dilation, g.x + x1 + dilation));
                }
            }
        }
        merge_ranges(ranges)
    }

    /// The flattened outline of `glyph` in `text`'s font at its size and
    /// variation coordinates.
    fn outline(&mut self, text: &TextFragment, glyph: u32) -> Arc<[[f32; 4]]> {
        let key = GlyphKey {
            font: text.font.data.id(),
            index: text.font.index,
            glyph,
            size: text.font_size.to_bits(),
            coords: text.coords.clone().into_boxed_slice(),
        };
        if let Some(o) = self.outlines.get(&key) {
            return o.clone();
        }
        if self.outlines.len() >= OUTLINE_CACHE_LIMIT {
            self.outlines.clear();
        }
        let mut pen = Flattener::default();
        if let Ok(font) = skrifa::FontRef::from_index(text.font.data.as_ref(), text.font.index)
            && let Some(g) = font.outline_glyphs().get(GlyphId::new(glyph))
        {
            let coords: Vec<NormalizedCoord> = text.coords.iter().map(|&c| NormalizedCoord::from_bits(c)).collect();
            let settings = DrawSettings::unhinted(Size::new(text.font_size), LocationRef::new(&coords));
            let _ = g.draw(settings, &mut pen);
        }
        let out: Arc<[[f32; 4]]> = pen.segments.into();
        self.outlines.insert(key, out.clone());
        out
    }
}

/// The metrics of the font at `index` in `data`, at `size` and the
/// variation `vars` the query chose for it. Follows skrifa's `Metrics`
/// (the same tables and precedence parley uses for its runs); a font
/// without underline or strikeout metrics gets HarfBuzz's defaults.
fn font_metrics(data: &[u8], index: u32, vars: &[(skrifa::Tag, f32)], size: f32) -> Option<FontMetrics> {
    let font = skrifa::FontRef::from_index(data, index).ok()?;
    let location = font.axes().location(vars.iter().copied());
    let m = skrifa::metrics::Metrics::new(&font, Size::new(size), &location);
    let x_height = match m.x_height {
        Some(x) if x > 0.0 => x,
        _ => measured_x_height(&font, size, &location).unwrap_or(m.ascent * 0.5),
    };
    let (underline_top, underline_thickness) = match m.underline {
        // `post` gives the top of the underline from the baseline, negative
        // below it; a line above the baseline is clamped to it.
        Some(u) => ((-u.offset).max(0.0), u.thickness.max(0.0)),
        None => (size / 18.0, size / 18.0),
    };
    let (strikeout_top, strikeout_thickness) = match m.strikeout {
        Some(s) => (s.offset, s.thickness.max(0.0)),
        None => (m.ascent / 2.0, size / 18.0),
    };
    Some(FontMetrics {
        ascent: m.ascent,
        descent: (-m.descent).max(0.0),
        leading: m.leading.max(0.0),
        x_height,
        underline_top,
        underline_thickness,
        strikeout_top,
        strikeout_thickness,
    })
}

/// The height of the font's `x` glyph above the baseline, for a font
/// whose `OS/2` table gives no x-height.
fn measured_x_height(font: &skrifa::FontRef<'_>, size: f32, location: &skrifa::instance::Location) -> Option<f32> {
    let glyph = font.charmap().map('x')?;
    let outline = font.outline_glyphs().get(glyph)?;
    let mut pen = Flattener::default();
    outline.draw(DrawSettings::unhinted(Size::new(size), location), &mut pen).ok()?;
    let top = pen.segments.iter().map(|s| s[1].max(s[3])).fold(f32::NEG_INFINITY, f32::max);
    (top > 0.0).then_some(top)
}

/// Metrics when no font at all answers the query (nothing shapes then
/// either).
fn fallback_metrics(size: f32) -> FontMetrics {
    FontMetrics {
        ascent: size * 0.8,
        descent: size * 0.2,
        leading: 0.0,
        x_height: size * 0.5,
        underline_top: size / 18.0,
        underline_thickness: size / 18.0,
        strikeout_top: size * 0.3,
        strikeout_thickness: size / 18.0,
    }
}

/// The x extent of the part of a line segment whose y lies in `lo..=hi`.
fn segment_span(seg: &[f32; 4], lo: f32, hi: f32) -> Option<(f32, f32)> {
    let [x0, y0, x1, y1] = *seg;
    if y0.max(y1) < lo || y0.min(y1) > hi {
        return None;
    }
    if (y1 - y0).abs() < 1e-6 {
        return Some((x0.min(x1), x0.max(x1)));
    }
    let t_at = |y: f32| ((y - y0) / (y1 - y0)).clamp(0.0, 1.0);
    let (ta, tb) = (t_at(lo), t_at(hi));
    let xa = x0 + (x1 - x0) * ta;
    let xb = x0 + (x1 - x0) * tb;
    Some((xa.min(xb), xa.max(xb)))
}

/// Sort ranges and merge the ones that overlap or touch.
fn merge_ranges(mut ranges: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f32, f32)> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

/// An `OutlinePen` that flattens curves into line segments.
#[derive(Default)]
struct Flattener {
    segments: Vec<[f32; 4]>,
    start: (f32, f32),
    current: (f32, f32),
}

impl Flattener {
    fn push(&mut self, x: f32, y: f32) {
        let (cx, cy) = self.current;
        if cx != x || cy != y {
            self.segments.push([cx, cy, x, y]);
        }
        self.current = (x, y);
    }

    /// Segments for a curve whose second difference (a bound on its
    /// deviation from the chord) is `dd`.
    fn steps(dd: f32) -> usize {
        ((dd / (4.0 * FLATTEN_TOLERANCE)).sqrt().ceil() as usize).clamp(1, 32)
    }
}

impl OutlinePen for Flattener {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = (x, y);
        self.current = (x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.push(x, y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (x0, y0) = self.current;
        let dd = ((x0 - 2.0 * cx0 + x).powi(2) + (y0 - 2.0 * cy0 + y).powi(2)).sqrt();
        let n = Self::steps(dd);
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let px = u * u * x0 + 2.0 * u * t * cx0 + t * t * x;
            let py = u * u * y0 + 2.0 * u * t * cy0 + t * t * y;
            self.push(px, py);
        }
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (x0, y0) = self.current;
        let d1 = ((x0 - 2.0 * cx0 + cx1).powi(2) + (y0 - 2.0 * cy0 + cy1).powi(2)).sqrt();
        let d2 = ((cx0 - 2.0 * cx1 + x).powi(2) + (cy0 - 2.0 * cy1 + y).powi(2)).sqrt();
        let n = Self::steps(1.5 * d1.max(d2));
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let px = u * u * u * x0 + 3.0 * u * u * t * cx0 + 3.0 * u * t * t * cx1 + t * t * t * x;
            let py = u * u * u * y0 + 3.0 * u * u * t * cy0 + 3.0 * u * t * t * cy1 + t * t * t * y;
            self.push(px, py);
        }
    }

    fn close(&mut self) {
        let (sx, sy) = self.start;
        self.push(sx, sy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_spans_and_merging() {
        // A vertical stem crossing the band: its x; a horizontal bar inside
        // it: its whole width; a bar outside: nothing; a diagonal: the x
        // range of the part inside the band.
        assert_eq!(segment_span(&[5.0, -10.0, 5.0, 10.0], -2.0, -1.0), Some((5.0, 5.0)));
        assert_eq!(segment_span(&[0.0, -1.5, 8.0, -1.5], -2.0, -1.0), Some((0.0, 8.0)));
        assert_eq!(segment_span(&[0.0, 3.0, 8.0, 3.0], -2.0, -1.0), None);
        assert_eq!(segment_span(&[0.0, -4.0, 4.0, 0.0], -2.0, -1.0), Some((2.0, 3.0)));
        assert_eq!(
            merge_ranges(vec![(5.0, 6.0), (1.0, 2.0), (1.5, 3.0), (3.0, 4.0)]),
            vec![(1.0, 4.0), (5.0, 6.0)]
        );
    }

    #[test]
    fn flattener_closes_contours_and_subdivides_curves() {
        let mut pen = Flattener::default();
        pen.move_to(0.0, 0.0);
        pen.line_to(10.0, 0.0);
        pen.quad_to(10.0, 10.0, 0.0, 10.0);
        pen.close();
        let n = pen.segments.len();
        assert!(n > 3, "{n} segments");
        let last = pen.segments[n - 1];
        assert_eq!((last[2], last[3]), (0.0, 0.0), "closed back to the start");
    }
}
