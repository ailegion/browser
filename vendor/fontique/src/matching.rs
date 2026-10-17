// Copyright 2024 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Implementation of the CSS font matching algorithm.

use super::font::FontInfo;
use crate::{FontStyle, FontWeight, FontWidth};
use smallvec::SmallVec;

const DEFAULT_OBLIQUE_ANGLE: f32 = 14.0;

pub fn match_font(
    set: &[FontInfo],
    width: FontWidth,
    style: FontStyle,
    weight: FontWeight,
    synthesize_style: bool,
) -> Option<usize> {
    const OBLIQUE_THRESHOLD: f32 = DEFAULT_OBLIQUE_ANGLE;
    match set.len() {
        0 => return None,
        1 => return Some(0),
        _ => {}
    }
    // Patch (plan/03-crates.md): web fonts match by their descriptors.
    if set.iter().any(|f| f.face().is_some()) {
        return match_faces(set, width, style, weight);
    }
    #[derive(Copy, Clone)]
    struct Candidate {
        index: usize,
        width: i32,
        style: FontStyle,
        weight: f32,
        has_slnt: bool,
    }
    let mut set: SmallVec<[Candidate; 16]> = set
        .iter()
        .enumerate()
        .map(|(i, font)| Candidate {
            index: i,
            width: (font.width().ratio() * 100.0) as i32,
            style: font.style(),
            weight: font.weight().value(),
            has_slnt: font.has_slant_axis(),
        })
        .collect();
    let width = (width.ratio() * 100.0) as i32;
    let weight = weight.value();
    // font-width is tried first:
    let mut use_width = set[0].width;
    if !set.iter().any(|f| f.width == width) {
        // If the desired width value is less than or equal to 100%...
        if width <= 100 {
            // width values below the desired width value are checked in
            // descending order...
            if let Some(found) = set
                .iter()
                .filter(|f| f.width < width)
                .max_by_key(|f| f.width)
            {
                use_width = found.width;
            }
            // followed by width values above the desired width value in
            // ascending order until a match is found.
            else if let Some(found) = set
                .iter()
                .filter(|f| f.width > width)
                .min_by_key(|f| f.width)
            {
                use_width = found.width;
            }
        }
        // Otherwise, ...
        else {
            // width values above the desired width value are checked in
            // ascending order...
            if let Some(found) = set
                .iter()
                .filter(|f| f.width > width)
                .min_by_key(|f| f.width)
            {
                use_width = found.width;
            }
            // followed by width values below the desired width value in
            // descending order until a match is found.
            else if let Some(found) = set
                .iter()
                .filter(|f| f.width < width)
                .max_by_key(|f| f.width)
            {
                use_width = found.width;
            }
        }
    } else {
        use_width = width;
    }
    set.retain(|f| f.width == use_width);
    use core::cmp::Ordering::*;
    let oblique_fonts = set.iter().filter_map(|f| oblique_style(f.style));
    // font-style is tried next:
    // NOTE: this code uses an oblique threshold of 14deg rather than
    // the current value of 20deg in the spec.
    // See: https://github.com/w3c/csswg-drafts/issues/2295
    let mut use_style = style;
    let mut _use_slnt = false;
    if !set.iter().any(|f| f.style == use_style) {
        // If the value of font-style is italic:
        if style == FontStyle::Italic {
            // oblique values greater than or equal to 14deg are checked in
            // ascending order
            if let Some(found) = oblique_fonts
                .clone()
                .filter(|(_, a)| *a >= OBLIQUE_THRESHOLD)
                .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
            {
                use_style = found.0;
            }
            // followed by positive oblique values below 14deg in descending order
            else if let Some(found) = oblique_fonts
                .clone()
                .filter(|(_, a)| *a > 0. && *a < OBLIQUE_THRESHOLD)
                .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
            {
                use_style = found.0;
            }
            // If no match is found, oblique values less than or equal to 0deg
            // are checked in descending order until a match is found.
            else if let Some(found) = oblique_fonts
                .clone()
                .filter(|(_, a)| *a < 0.)
                .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
            {
                use_style = found.0;
            } else {
                use_style = set[0].style;
            }
        }
        // If the value of font-style is oblique...
        else if let Some(angle) = oblique_angle(style) {
            // and the requested angle is greater than or equal to 14deg
            if angle >= OBLIQUE_THRESHOLD {
                // oblique values greater than or equal to angle are checked in
                // ascending order
                if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a >= angle)
                    .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                }
                // followed by positive oblique values below angle in descending order
                else if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a > 0. && *a < angle)
                    .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                } else {
                    // If font-synthesis-style has the value auto, then for variable
                    // fonts with a slnt axis a match is created by setting the slnt
                    // value with the specified oblique value; otherwise, a fallback
                    // match is produced by geometric shearing to the specified
                    // oblique value.
                    if synthesize_style {
                        if set.iter().any(|f| f.has_slnt) {
                            _use_slnt = true;
                        } else {
                            use_style = if set.iter().any(|f| f.style == FontStyle::Normal) {
                                FontStyle::Normal
                            } else {
                                set[0].style
                            };
                        }
                    } else {
                        // Choose an italic style
                        if set.iter().any(|f| f.style == FontStyle::Italic) {
                            use_style = FontStyle::Italic;
                        }
                        // oblique values less than or equal to 0deg are checked in descending order
                        else if let Some(found) = oblique_fonts
                            .clone()
                            .filter(|(_, a)| *a <= 0.)
                            .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                        {
                            use_style = found.0;
                        } else {
                            use_style = set[0].style;
                        }
                    }
                }
            }
            // if the requested angle is greater than or equal to 0deg
            // and less than 14deg
            else if angle >= 0. {
                // positive oblique values below angle in descending order
                if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a > 0. && *a < angle)
                    .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                }
                // followed by oblique values greater than or equal to angle in
                // ascending order
                else if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a >= angle)
                    .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                } else {
                    // If font-synthesis-style has the value auto, then for variable
                    // fonts with a slnt axis a match is created by setting the slnt
                    // value with the specified oblique value; otherwise, a fallback
                    // match is produced by geometric shearing to the specified
                    // oblique value.
                    if synthesize_style {
                        if set.iter().any(|f| f.has_slnt) {
                            _use_slnt = true;
                        } else {
                            use_style = if set.iter().any(|f| f.style == FontStyle::Normal) {
                                FontStyle::Normal
                            } else {
                                set[0].style
                            };
                        }
                    } else {
                        // Choose an italic style
                        if set.iter().any(|f| f.style == FontStyle::Italic) {
                            use_style = FontStyle::Italic;
                        }
                        // oblique values less than or equal to 0deg are checked in descending order
                        else if let Some(found) = oblique_fonts
                            .clone()
                            .filter(|(_, a)| *a <= 0.)
                            .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                        {
                            use_style = found.0;
                        } else {
                            use_style = set[0].style;
                        }
                    }
                }
            }
            // -14deg < angle < 0deg
            else if angle > -OBLIQUE_THRESHOLD {
                // negative oblique values above angle in ascending order
                if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a < 0. && *a > angle)
                    .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                }
                // followed by oblique values less than or equal to angle in
                // descending order
                else if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a <= angle)
                    .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                } else {
                    // If font-synthesis-style has the value auto, then for variable
                    // fonts with a slnt axis a match is created by setting the slnt
                    // value with the specified oblique value; otherwise, a fallback
                    // match is produced by geometric shearing to the specified
                    // oblique value.
                    if synthesize_style {
                        if set.iter().any(|f| f.has_slnt) {
                            _use_slnt = true;
                        } else {
                            use_style = if set.iter().any(|f| f.style == FontStyle::Normal) {
                                FontStyle::Normal
                            } else {
                                set[0].style
                            };
                        }
                    } else {
                        // Choose an italic style
                        if set.iter().any(|f| f.style == FontStyle::Italic) {
                            use_style = FontStyle::Italic;
                        }
                        // oblique values greater than or equal to 0deg are checked in ascending order
                        else if let Some(found) = oblique_fonts
                            .clone()
                            .filter(|(_, a)| *a >= 0.)
                            .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                        {
                            use_style = found.0;
                        } else {
                            use_style = set[0].style;
                        }
                    }
                }
            }
            // angle < -14 deg
            else {
                // oblique values less than or equal to angle are checked in
                // descending order
                if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a <= angle)
                    .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                }
                // followed by negative oblique values above angle in ascending order
                else if let Some(found) = oblique_fonts
                    .clone()
                    .filter(|(_, a)| *a < 0. && *a > angle)
                    .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                {
                    use_style = found.0;
                } else {
                    // If font-synthesis-style has the value auto, then for variable
                    // fonts with a slnt axis a match is created by setting the slnt
                    // value with the specified oblique value; otherwise, a fallback
                    // match is produced by geometric shearing to the specified
                    // oblique value.
                    if synthesize_style {
                        if set.iter().any(|f| f.has_slnt) {
                            _use_slnt = true;
                        } else {
                            use_style = if set.iter().any(|f| f.style == FontStyle::Normal) {
                                FontStyle::Normal
                            } else {
                                set[0].style
                            };
                        }
                    } else {
                        // Choose an italic style
                        if set.iter().any(|f| f.style == FontStyle::Italic) {
                            use_style = FontStyle::Italic;
                        }
                        // oblique values greater than or equal to 0deg are checked in ascending order
                        else if let Some(found) = oblique_fonts
                            .clone()
                            .filter(|(_, a)| *a >= 0.)
                            .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
                        {
                            use_style = found.0;
                        } else {
                            use_style = set[0].style;
                        }
                    }
                }
            }
        }
        // If the value of font-style is normal...
        else {
            // oblique values greater than or equal to 0deg are checked in
            // ascending order
            if let Some(found) = oblique_fonts
                .clone()
                .filter(|(_, a)| *a >= 0.)
                .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
            {
                use_style = found.0;
            }
            // followed by italic fonts
            else if let Some(found) = set.iter().find(|f| f.style == FontStyle::Italic) {
                use_style = found.style;
            }
            // followed by oblique values less than 0deg in descending order
            else if let Some(found) = oblique_fonts
                .clone()
                .filter(|(_, a)| *a < 0.)
                .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(Less))
            {
                use_style = found.0;
            } else {
                use_style = set[0].style;
            }
        }
    }
    set.retain(|f| f.style == use_style);
    // font-weight is matched next:
    if let Some(f) = set.iter().find(|f| f.weight == weight) {
        return Some(f.index);
    } else {
        // If the desired weight is inclusively between 400 and 500...
        if (400.0..=500.0).contains(&weight) {
            // weights greater than or equal to the target weight are checked in ascending
            // order until 500 is hit and checked
            if let Some(found) = set
                .iter()
                .enumerate()
                .filter(|f| f.1.weight >= weight && f.1.weight <= 500.0)
                .min_by(|x, y| x.1.weight.partial_cmp(&y.1.weight).unwrap_or(Less))
            {
                return Some(found.1.index);
            }
            // followed by weights less than the target weight in descending order
            if let Some(found) = set
                .iter()
                .enumerate()
                .filter(|f| f.1.weight < weight)
                .max_by(|x, y| x.1.weight.partial_cmp(&y.1.weight).unwrap_or(Less))
            {
                return Some(found.1.index);
            }
            // followed by weights greater than 500, until a match is found.
            if let Some(found) = set
                .iter()
                .enumerate()
                .filter(|f| f.1.weight > 500.0)
                .min_by(|x, y| x.1.weight.partial_cmp(&y.1.weight).unwrap_or(Less))
            {
                return Some(found.1.index);
            }
        }
        // If the desired weight is less than 400...
        else if weight < 400.0 {
            // weights less than or equal to the target weight are checked in descending
            if let Some(found) = set
                .iter()
                .filter(|f| f.weight <= weight)
                .max_by(|x, y| x.weight.partial_cmp(&y.weight).unwrap_or(Less))
            {
                return Some(found.index);
            }
            // followed by weights greater than the target weight in ascending order
            if let Some(found) = set
                .iter()
                .filter(|f| f.weight > weight)
                .min_by(|x, y| x.weight.partial_cmp(&y.weight).unwrap_or(Less))
            {
                return Some(found.index);
            }
        }
        // If the desired weight is greater than 500...
        else {
            // weights greater than or equal to the target weight are checked in ascending
            if let Some(found) = set
                .iter()
                .filter(|f| f.weight >= weight)
                .min_by(|x, y| x.weight.partial_cmp(&y.weight).unwrap_or(Less))
            {
                return Some(found.index);
            }
            // followed by weights less than the target weight in descending order
            if let Some(found) = set
                .iter()
                .filter(|f| f.weight < weight)
                .max_by(|x, y| x.weight.partial_cmp(&y.weight).unwrap_or(Less))
            {
                return Some(found.index);
            }
        }
    }
    None
}

/// The faces of a family registered from `@font-face` rules, matched by
/// their descriptor ranges (CSS Fonts 4 §5.2): width, then style, then
/// weight, each step keeping the faces nearest in its order of
/// preference. `auto` descriptors match as the normal value. Among equal
/// faces the last registered wins.
fn match_faces(set: &[FontInfo], width: FontWidth, style: FontStyle, weight: FontWeight) -> Option<usize> {
    use crate::FaceStyle;
    // The ranges a font is matched by.
    let ranges = |f: &FontInfo| {
        let face = f.face().copied().unwrap_or_default();
        (
            face.width.unwrap_or((100.0, 100.0)),
            face.style.unwrap_or(FaceStyle::Normal),
            face.weight.unwrap_or((400.0, 400.0)),
        )
    };
    let mut candidates: SmallVec<[usize; 16]> = (0..set.len()).collect();
    let keep_best = |candidates: &mut SmallVec<[usize; 16]>, key: &dyn Fn(usize) -> (u8, f32)| {
        let best = candidates.iter().map(|&i| key(i)).fold(None, |acc: Option<(u8, f32)>, k| match acc {
            Some(a) if a <= k => Some(a),
            _ => Some(k),
        });
        if let Some(b) = best {
            candidates.retain(|i| key(*i) == b);
        }
    };

    // Width: a range holding the wanted width; else at or below 100%
    // narrower faces nearest first, then wider; above 100% the reverse.
    let w = width.percentage();
    keep_best(&mut candidates, &|i| {
        let (lo, hi) = ranges(&set[i]).0;
        if lo <= w && w <= hi {
            (0, 0.0)
        } else if hi < w {
            (if w <= 100.0 { 1 } else { 2 }, w - hi)
        } else {
            (if w <= 100.0 { 2 } else { 1 }, lo - w)
        }
    });

    // Style. `normal` is an oblique range of 0deg; italic is asked as
    // itself first, then as an angle of `OBLIQUE_THRESHOLD`.
    const T: f32 = 11.0;
    let (italic, r) = match style {
        FontStyle::Normal => (false, 0.0),
        FontStyle::Italic => (true, T),
        FontStyle::Oblique(a) => (false, a.unwrap_or(DEFAULT_OBLIQUE_ANGLE)),
    };
    keep_best(&mut candidates, &|i| {
        let (lo, hi) = match ranges(&set[i]).1 {
            FaceStyle::Italic => {
                // An italic face: first for italic; otherwise after the
                // oblique faces on the asked side, before the other side.
                return if italic { (0, 0.0) } else { (4, 0.0) };
            }
            FaceStyle::Normal => (0.0, 0.0),
            FaceStyle::Oblique(lo, hi) => (lo, hi),
        };
        let tier = |t: u8, d: f32| (t + u8::from(italic), d);
        if lo <= r && r <= hi {
            return tier(0, 0.0);
        }
        if r >= T {
            // Larger angles first, then smaller positive, then the rest.
            if lo > r {
                tier(1, lo - r)
            } else if hi > 0.0 {
                tier(2, r - hi)
            } else {
                tier(5, -hi)
            }
        } else if r >= 0.0 {
            // Smaller angles down to 0 first, then larger, then negative.
            if hi < r && hi >= 0.0 {
                tier(1, r - hi)
            } else if lo > r {
                tier(2, lo - r)
            } else {
                tier(5, -hi)
            }
        } else if r > -T {
            if lo > r && lo <= 0.0 {
                tier(1, lo - r)
            } else if hi < r {
                tier(2, r - hi)
            } else {
                tier(5, lo)
            }
        } else if hi < r {
            tier(1, r - hi)
        } else if lo <= 0.0 {
            tier(2, lo - r)
        } else {
            tier(5, lo)
        }
    });

    // Weight: in range; else from 400 to 500 heavier up to 500, then
    // lighter, then heavier; below 400 lighter then heavier; above 500
    // heavier then lighter.
    let wt = weight.value();
    keep_best(&mut candidates, &|i| {
        let (lo, hi) = ranges(&set[i]).2;
        if lo <= wt && wt <= hi {
            (0, 0.0)
        } else if (400.0..=500.0).contains(&wt) {
            if lo > wt && lo <= 500.0 {
                (1, lo - wt)
            } else if hi < wt {
                (2, wt - hi)
            } else {
                (3, lo - wt)
            }
        } else if wt < 400.0 {
            if hi < wt { (1, wt - hi) } else { (2, lo - wt) }
        } else if lo > wt {
            (1, lo - wt)
        } else {
            (2, wt - hi)
        }
    });
    candidates.last().copied()
}

fn oblique_angle(style: FontStyle) -> Option<f32> {
    match style {
        FontStyle::Oblique(angle) => Some(angle.unwrap_or(DEFAULT_OBLIQUE_ANGLE)),
        _ => None,
    }
}

fn oblique_style(style: FontStyle) -> Option<(FontStyle, f32)> {
    match style {
        FontStyle::Oblique(angle) => Some((style, angle.unwrap_or(DEFAULT_OBLIQUE_ANGLE))),
        _ => None,
    }
}
