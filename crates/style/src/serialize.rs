//! CSSOM serialization: specified values as the `style` attribute and
//! `element.style` give them back, declaration blocks with shorthands
//! collapsed, and resolved values for `getComputedStyle`.
//!
//! Specified values serialize the way browsers normalize them: keywords
//! lower-case, numbers trimmed, hex colors as `rgb()`, named colors by
//! name, lengths in the unit written. Shorthands are rebuilt from their
//! longhands and omit the parts that are at their initial value.

use crate::computed::{
    ComputedBackgroundSize, ComputedStyle, ComputedVerticalAlign, DecorationThickness, LetterSpacing, LineHeight,
    UnderlineOffset,
};
use crate::image::{ComputedImage, position_offset_css};
use crate::properties::*;
use crate::values::*;

// ----- specified values -----

/// Serialize a longhand's specified value.
pub fn serialize_longhand(v: &PropertyDeclaration) -> String {
    use PropertyDeclaration as P;
    match v {
        P::Display(d) => d.keyword().to_owned(),
        P::Position(p) => p.keyword().to_owned(),
        P::Float(f) => f.keyword().to_owned(),
        P::Clear(c) => c.keyword().to_owned(),
        P::BoxSizing(b) => b.keyword().to_owned(),
        P::Top(v) | P::Right(v) | P::Bottom(v) | P::Left(v) => v.to_css(),
        P::MarginTop(v) | P::MarginRight(v) | P::MarginBottom(v) | P::MarginLeft(v) => v.to_css(),
        P::PaddingTop(v) | P::PaddingRight(v) | P::PaddingBottom(v) | P::PaddingLeft(v) => v.to_css(),
        P::BorderTopWidth(w) | P::BorderRightWidth(w) | P::BorderBottomWidth(w) | P::BorderLeftWidth(w) => {
            border_width_css(w)
        }
        P::BorderTopStyle(s) | P::BorderRightStyle(s) | P::BorderBottomStyle(s) | P::BorderLeftStyle(s) => {
            s.keyword().to_owned()
        }
        P::BorderTopColor(c) | P::BorderRightColor(c) | P::BorderBottomColor(c) | P::BorderLeftColor(c) => c.to_css(),
        P::BorderTopLeftRadius(r) | P::BorderTopRightRadius(r) | P::BorderBottomRightRadius(r) | P::BorderBottomLeftRadius(r) => {
            r.to_css()
        }
        P::Width(s) | P::Height(s) | P::MinWidth(s) | P::MinHeight(s) | P::MaxWidth(s) | P::MaxHeight(s) => s.to_css(),
        P::OverflowX(o) | P::OverflowY(o) => o.keyword().to_owned(),
        P::Visibility(v) => v.keyword().to_owned(),
        P::Color(c) | P::BackgroundColor(c) | P::TextDecorationColor(c) => c.to_css(),
        P::ListStyleImage(i) => i.to_css(),
        P::BackgroundImage(list) => layers(list.iter().map(ImageValue::to_css)),
        P::BackgroundPositionX(list) => layers(list.iter().map(|p| p.to_css(true))),
        P::BackgroundPositionY(list) => layers(list.iter().map(|p| p.to_css(false))),
        P::BackgroundBlendMode(list) => layers(list.iter().map(|m| m.keyword().to_owned())),
        P::BackgroundSize(list) => layers(list.iter().map(BackgroundSize::to_css)),
        P::BackgroundRepeat(list) => layers(list.iter().map(|r| r.to_css())),
        P::BackgroundAttachment(list) => layers(list.iter().map(|a| a.keyword().to_owned())),
        P::BackgroundOrigin(list) | P::BackgroundClip(list) => layers(list.iter().map(|b| b.keyword().to_owned())),
        P::FontFamily(f) => font_family_css(f),
        P::FontSize(s) => font_size_css(s),
        P::FontWeight(w) => match w {
            FontWeight::Absolute(n) => n.to_string(),
            FontWeight::Bolder => "bolder".to_owned(),
            FontWeight::Lighter => "lighter".to_owned(),
        },
        P::FontStyle(s) => match s {
            FontStyleValue::Normal => "normal".to_owned(),
            FontStyleValue::Italic => "italic".to_owned(),
            FontStyleValue::Oblique(None) => "oblique".to_owned(),
            FontStyleValue::Oblique(Some(deg)) => format!("oblique {}deg", css_number(*deg)),
        },
        P::FontVariant(v) => v.keyword().to_owned(),
        P::FontOpticalSizing(o) => o.keyword().to_owned(),
        P::FontVariationSettings(v) => variation_settings_css(v),
        P::FontStretch(s) => s.to_css(),
        P::LineHeight(l) => line_height_css(l),
        P::TextAlign(t) => t.keyword().to_owned(),
        P::Direction(d) => d.keyword().to_owned(),
        P::UnicodeBidi(u) => u.keyword().to_owned(),
        P::TextDecorationLine(l) => l.to_css(),
        P::TextDecorationStyle(s) => s.keyword().to_owned(),
        P::TextDecorationThickness(t) => match t {
            TextDecorationThickness::Auto => "auto".to_owned(),
            TextDecorationThickness::FromFont => "from-font".to_owned(),
            TextDecorationThickness::Length(l) => l.to_css(),
        },
        P::TextUnderlinePosition(p) => p.to_css(),
        P::TextUnderlineOffset(o) => match o {
            TextUnderlineOffsetValue::Auto => "auto".to_owned(),
            TextUnderlineOffsetValue::Length(l) => l.to_css(),
        },
        P::TextDecorationSkipInk(s) => s.keyword().to_owned(),
        P::TextTransform(t) => t.keyword().to_owned(),
        P::WhiteSpace(w) => w.keyword().to_owned(),
        P::LetterSpacing(l) => l.to_css(),
        P::WordBreak(w) => w.keyword().to_owned(),
        P::OverflowWrap(w) => w.keyword().to_owned(),
        P::TextOverflow(t) => t.to_css(),
        P::BoxDecorationBreak(b) => b.keyword().to_owned(),
        P::ListStyleType(t) => t.to_css(),
        P::ListStylePosition(p) => p.keyword().to_owned(),
        P::VerticalAlign(v) => match v {
            VerticalAlignValue::Keyword(k) => k.keyword().to_owned(),
            VerticalAlignValue::Length(l) => l.to_css(),
        },
        P::FlexDirection(d) => d.keyword().to_owned(),
        P::FlexWrap(w) => w.keyword().to_owned(),
        P::JustifyContent(a) | P::AlignItems(a) | P::AlignSelf(a) | P::AlignContent(a) => a.to_css(),
        P::FlexGrow(n) | P::FlexShrink(n) | P::Opacity(n) => css_number(*n),
        P::FlexBasis(b) => b.to_css(),
        P::RowGap(g) | P::ColumnGap(g) => g.to_css(),
        P::ScrollbarWidth(w) => w.keyword().to_owned(),
        P::ScrollbarColor(c) => match c {
            ScrollbarColorValue::Auto => "auto".to_owned(),
            ScrollbarColorValue::Colors { thumb, track } => format!("{} {}", thumb.to_css(), track.to_css()),
        },
        P::ScrollbarGutter(g) => g.keyword().to_owned(),
        P::OverscrollBehaviorX(b) | P::OverscrollBehaviorY(b) => b.keyword().to_owned(),
        P::ScrollBehavior(b) => b.keyword().to_owned(),
        P::ScrollMarginTop(m) | P::ScrollMarginRight(m) | P::ScrollMarginBottom(m) | P::ScrollMarginLeft(m) => m.to_css(),
        P::ScrollPaddingTop(p) | P::ScrollPaddingRight(p) | P::ScrollPaddingBottom(p) | P::ScrollPaddingLeft(p) => {
            p.to_css()
        }
        P::ScrollSnapType(t) => t.to_css(),
        P::ScrollSnapAlign(a) => a.to_css(),
        P::ScrollSnapStop(s) => s.keyword().to_owned(),
        P::OverflowAnchor(a) => a.keyword().to_owned(),
        P::BoxShadow(list) => {
            if list.is_empty() {
                "none".to_owned()
            } else {
                layers(list.iter().map(BoxShadow::to_css))
            }
        }
        P::OutlineWidth(w) => border_width_css(w),
        P::OutlineStyle(s) => s.keyword().to_owned(),
        P::OutlineColor(c) => c.to_css(),
        P::OutlineOffset(l) => l.to_css(),
        P::Cursor(c) => c.to_css(),
        P::PointerEvents(p) => p.keyword().to_owned(),
        P::UserSelect(u) => u.keyword().to_owned(),
        P::ZIndex(z) => z.to_css(),
        P::Isolation(i) => match i {
            Isolation::Auto => "auto".to_owned(),
            Isolation::Isolate => "isolate".to_owned(),
        },
        P::WillChange(w) => w.to_css(),
        P::Transform(list) => transform_list_css(list),
        P::TransformOrigin(o) => {
            let mut s = format!("{} {}", o.x.to_css(true), o.y.to_css(false));
            if o.z != Length::ZERO {
                s.push(' ');
                s.push_str(&o.z.to_css());
            }
            s
        }
        P::TransformBox(b) => b.keyword().to_owned(),
        P::TransformStyle(t) => match t {
            TransformStyle::Flat => "flat".to_owned(),
            TransformStyle::Preserve3d => "preserve-3d".to_owned(),
        },
        P::BackfaceVisibility(b) => match b {
            BackfaceVisibility::Visible => "visible".to_owned(),
            BackfaceVisibility::Hidden => "hidden".to_owned(),
        },
        P::Perspective(p) => match p {
            Perspective::None => "none".to_owned(),
            Perspective::Length(l) => l.to_css(),
        },
        P::PerspectiveOrigin(p) => format!("{} {}", p.x.to_css(true), p.y.to_css(false)),
        P::Translate(t) => match t {
            TranslateValue::None => "none".to_owned(),
            TranslateValue::Values(x, y, z) => {
                let mut parts = vec![x.to_css()];
                if let Some(y) = y {
                    parts.push(y.to_css());
                }
                if let Some(z) = z {
                    parts.push(z.to_css());
                }
                parts.join(" ")
            }
        },
        P::Rotate(r) => match r {
            RotateValue::None => "none".to_owned(),
            RotateValue::Rotate(x, y, z, a) => rotate_css(*x, *y, *z, *a),
        },
        P::Scale(s) => match s {
            ScaleValue::None => "none".to_owned(),
            ScaleValue::Values(x, y, z) => scale_css(*x, *y, *z),
        },
        P::BorderSpacing(b) => match &b.1 {
            Some(v) if *v != b.0 => format!("{} {}", b.0.to_css(), v.to_css()),
            _ => b.0.to_css(),
        },
        P::BorderCollapse(b) => match b {
            BorderCollapse::Separate => "separate".to_owned(),
            BorderCollapse::Collapse => "collapse".to_owned(),
        },
        P::EmptyCells(e) => match e {
            EmptyCells::Show => "show".to_owned(),
            EmptyCells::Hide => "hide".to_owned(),
        },
        P::GridTemplateColumns(t) | P::GridTemplateRows(t) => t.to_css(),
        P::GridAutoColumns(t) | P::GridAutoRows(t) => crate::grid::auto_tracks_css(t),
        P::GridAutoFlow(f) => f.to_css(),
        P::GridRowStart(l) | P::GridRowEnd(l) | P::GridColumnStart(l) | P::GridColumnEnd(l) => l.to_css(),
    }
}

/// A `rotate` value: the angle alone about z, `x`/`y` for those axes,
/// else the three numbers.
fn rotate_css(x: f32, y: f32, z: f32, a: f32) -> String {
    let deg = format!("{}deg", css_number(a));
    match (x, y, z) {
        (0.0, 0.0, 1.0) => deg,
        (1.0, 0.0, 0.0) => format!("x {deg}"),
        (0.0, 1.0, 0.0) => format!("y {deg}"),
        _ => format!("{} {} {} {deg}", css_number(x), css_number(y), css_number(z)),
    }
}

/// A `scale` value: one number when they agree, two when z is 1, else
/// three.
fn scale_css(x: f32, y: f32, z: f32) -> String {
    if z == 1.0 {
        if x == y { css_number(x) } else { format!("{} {}", css_number(x), css_number(y)) }
    } else {
        format!("{} {} {}", css_number(x), css_number(y), css_number(z))
    }
}

/// A `transform` list as written (`none` when empty).
fn transform_list_css(list: &[TransformFunction]) -> String {
    use TransformFunction as T;
    if list.is_empty() {
        return "none".to_owned();
    }
    let deg = |a: f32| format!("{}deg", css_number(a));
    list.iter()
        .map(|f| match f {
            T::Matrix(a) => format!("matrix({})", a.iter().map(|v| css_number(*v)).collect::<Vec<_>>().join(", ")),
            T::Matrix3d(a) => format!("matrix3d({})", a.iter().map(|v| css_number(*v)).collect::<Vec<_>>().join(", ")),
            T::Translate(x, y) => format!("translate({}, {})", x.to_css(), y.to_css()),
            T::TranslateX(x) => format!("translateX({})", x.to_css()),
            T::TranslateY(y) => format!("translateY({})", y.to_css()),
            T::TranslateZ(z) => format!("translateZ({})", z.to_css()),
            T::Translate3d(x, y, z) => format!("translate3d({}, {}, {})", x.to_css(), y.to_css(), z.to_css()),
            T::Scale(x, y) => format!("scale({}, {})", css_number(*x), css_number(*y)),
            T::ScaleX(x) => format!("scaleX({})", css_number(*x)),
            T::ScaleY(y) => format!("scaleY({})", css_number(*y)),
            T::ScaleZ(z) => format!("scaleZ({})", css_number(*z)),
            T::Scale3d(x, y, z) => format!("scale3d({}, {}, {})", css_number(*x), css_number(*y), css_number(*z)),
            T::Rotate(a) => format!("rotate({})", deg(*a)),
            T::RotateX(a) => format!("rotateX({})", deg(*a)),
            T::RotateY(a) => format!("rotateY({})", deg(*a)),
            T::RotateZ(a) => format!("rotateZ({})", deg(*a)),
            T::Rotate3d(x, y, z, a) => format!("rotate3d({}, {}, {}, {})", css_number(*x), css_number(*y), css_number(*z), deg(*a)),
            T::Skew(x, y) => format!("skew({}, {})", deg(*x), deg(*y)),
            T::SkewX(x) => format!("skewX({})", deg(*x)),
            T::SkewY(y) => format!("skewY({})", deg(*y)),
            T::Perspective(None) => "perspective(none)".to_owned(),
            T::Perspective(Some(l)) => format!("perspective({})", l.to_css()),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Serialize a declared value: a specified value, a CSS-wide keyword, or
/// the raw text of a custom property or a `var()` declaration.
pub fn serialize_declared(v: &DeclaredValue) -> String {
    match v {
        DeclaredValue::Value(p) => serialize_longhand(p),
        DeclaredValue::Inherit(_) => "inherit".to_owned(),
        DeclaredValue::Initial(_) => "initial".to_owned(),
        DeclaredValue::Unset(_) => "unset".to_owned(),
        DeclaredValue::Revert(_) => "revert".to_owned(),
        DeclaredValue::RevertLayer(_) => "revert-layer".to_owned(),
        DeclaredValue::Custom { value, .. } => match value {
            CustomValue::Raw(raw) => raw.to_string(),
            CustomValue::Initial => "initial".to_owned(),
            CustomValue::Inherit => "inherit".to_owned(),
        },
        DeclaredValue::Pending { raw, .. } => raw.to_string(),
    }
}

/// A comma-separated layer list.
fn layers(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(", ")
}

/// Split a serialized layer list at its top-level commas (not those
/// inside functions or strings).
fn split_layers(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

fn border_width_css(w: &BorderWidth) -> String {
    match w {
        BorderWidth::Thin => "thin".to_owned(),
        BorderWidth::Medium => "medium".to_owned(),
        BorderWidth::Thick => "thick".to_owned(),
        BorderWidth::Length(l) => l.to_css(),
    }
}

fn font_size_css(s: &FontSize) -> String {
    match s {
        FontSize::Length(l) => l.to_css(),
        FontSize::Percent(p) => format!("{}%", css_number(*p)),
        FontSize::Calc(c) => c.to_css(),
        FontSize::Keyword(k) => FONT_SIZE_KEYWORDS[(*k as usize).min(7)].to_owned(),
        FontSize::Smaller => "smaller".to_owned(),
        FontSize::Larger => "larger".to_owned(),
    }
}

/// `font-variation-settings`: `normal`, or `"tag" value` pairs.
fn variation_settings_css(v: &[([u8; 4], f32)]) -> String {
    if v.is_empty() {
        return "normal".to_owned();
    }
    v.iter()
        .map(|(tag, value)| {
            let mut s = String::from("\"");
            for &b in tag {
                if b == b'"' || b == b'\\' {
                    s.push('\\');
                }
                s.push(b as char);
            }
            format!("{s}\" {}", css_number(*value))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn line_height_css(l: &LineHeightValue) -> String {
    match l {
        LineHeightValue::Normal => "normal".to_owned(),
        LineHeightValue::Number(n) => css_number(*n),
        LineHeightValue::Length(l) => l.to_css(),
        LineHeightValue::Percent(p) => format!("{}%", css_number(*p)),
        LineHeightValue::Calc(c) => c.to_css(),
    }
}

/// Whether `s` can stand unquoted as a family name: one identifier that
/// is not a keyword with a meaning of its own there.
fn is_plain_family_ident(s: &str) -> bool {
    if s.is_empty() || s.contains(char::is_whitespace) {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if GENERIC_FAMILIES.contains(&lower.as_str())
        || matches!(lower.as_str(), "inherit" | "initial" | "unset" | "revert" | "revert-layer" | "default" | "none")
    {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap_or(' ');
    let ident_start = |c: char| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii();
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii();
    let ok_first = ident_start(first) || (first == '-' && chars.clone().next().is_some_and(|c| ident_start(c) || c == '-'));
    ok_first && chars.all(ident_char)
}

/// A family name as the CSSOM gives it: generic keywords and single
/// identifiers bare, everything else quoted.
fn family_css(name: &str, quoted: bool) -> String {
    let lower = name.to_ascii_lowercase();
    if !quoted && GENERIC_FAMILIES.contains(&lower.as_str()) {
        return lower;
    }
    if is_plain_family_ident(name) {
        name.to_owned()
    } else {
        css_string(name)
    }
}

fn font_family_css(families: &[FamilyName]) -> String {
    families
        .iter()
        .map(|f| match f {
            FamilyName::Quoted(s) => family_css(s, true),
            FamilyName::Ident(s) => family_css(s, false),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The computed `font-family` string (`"Name", serif`) as the CSSOM
/// gives it.
fn computed_font_family_css(computed: &str) -> String {
    computed
        .split(", ")
        .map(|part| match part.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
            Some(inner) => family_css(inner, true),
            None => part.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ----- shorthands -----

/// The serialized value of shorthand `name` from the serialized values of
/// its longhands, in `shorthand_longhands` order. `None` when the values
/// cannot be expressed as one shorthand value.
pub fn serialize_shorthand(name: &str, values: &[String]) -> Option<String> {
    let longhands = shorthand_longhands(name)?;
    if values.len() != longhands.len() {
        return None;
    }
    let v = |i: usize| values[i].as_str();
    Some(match name.to_ascii_lowercase().as_str() {
        "margin" | "padding" | "inset" | "scroll-margin" | "scroll-padding" | "border-width" | "border-style"
        | "border-color" => sides_css(values),
        "border-radius" => {
            // Each longhand is `x` or `x y`; the shorthand is the four
            // horizontal radii, then `/` and the four vertical ones.
            let split: Vec<(&str, &str)> = values
                .iter()
                .map(|s| s.split_once(' ').unwrap_or((s.as_str(), s.as_str())))
                .collect();
            let xs: Vec<String> = split.iter().map(|(x, _)| (*x).to_owned()).collect();
            let ys: Vec<String> = split.iter().map(|(_, y)| (*y).to_owned()).collect();
            if xs == ys {
                sides_css(&xs)
            } else {
                format!("{} / {}", sides_css(&xs), sides_css(&ys))
            }
        }
        "border" => {
            // One value only when all four sides agree.
            for (a, b, c) in [(0, 1, 2), (4, 5, 6), (8, 9, 10)] {
                if v(a) != v(b) || v(b) != v(c) || v(c) != v(a + 3) {
                    return None;
                }
            }
            border_side_css(v(0), v(4), v(8))
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => border_side_css(v(0), v(1), v(2)),
        "overflow" | "gap" | "grid-gap" | "place-content" | "overscroll-behavior" => {
            if v(0) == v(1) { v(0).to_owned() } else { format!("{} {}", v(0), v(1)) }
        }
        "background-position" => {
            // One `x y` per layer; the two lists must agree in length.
            let (xs, ys) = (split_layers(v(0)), split_layers(v(1)));
            if xs.len() != ys.len() || xs.is_empty() {
                return None;
            }
            xs.iter().zip(&ys).map(|(x, y)| format!("{x} {y}")).collect::<Vec<_>>().join(", ")
        }
        "-webkit-background-clip" | "-webkit-background-origin" | "-webkit-background-size" => v(0).to_owned(),
        "background" => {
            // One layer per entry of the lists, which must agree in
            // length; the color goes with the last layer.
            let color = v(0);
            let lists: Vec<Vec<&str>> = (1..9).map(|i| split_layers(v(i))).collect();
            let count = lists[0].len();
            if lists.iter().any(|l| l.len() != count) || count == 0 {
                return None;
            }
            let mut out: Vec<String> = Vec::with_capacity(count);
            for (layer, &image) in lists[0].iter().enumerate() {
                let (x, y, size, repeat, attachment, origin, clip) = (
                    lists[1][layer],
                    lists[2][layer],
                    lists[3][layer],
                    lists[4][layer],
                    lists[5][layer],
                    lists[6][layer],
                    lists[7][layer],
                );
                let position = format!("{x} {y}");
                let mut parts: Vec<String> = Vec::new();
                if layer == count - 1 && color != "transparent" {
                    parts.push(color.to_owned());
                }
                if image != "none" {
                    parts.push(image.to_owned());
                }
                let size_set = size != "auto";
                if position != "0% 0%" || size_set {
                    parts.push(if size_set { format!("{position} / {size}") } else { position });
                }
                if repeat != "repeat" {
                    parts.push(repeat.to_owned());
                }
                if attachment != "scroll" {
                    parts.push(attachment.to_owned());
                }
                if origin != "padding-box" || clip != "border-box" {
                    parts.push(origin.to_owned());
                    if clip != origin {
                        parts.push(clip.to_owned());
                    }
                }
                out.push(if parts.is_empty() { "none".to_owned() } else { parts.join(" ") });
            }
            out.join(", ")
        }
        "font" => {
            let (style, variant, weight, stretch, size, line_height, family) =
                (v(0), v(1), v(2), v(3), v(4), v(5), v(6));
            // The longhands the shorthand only resets: other values have
            // no shorthand form.
            if v(7) != "auto" || v(8) != "normal" {
                return None;
            }
            // The shorthand takes only the keyword stretches.
            if stretch.ends_with('%') && stretch != "100%" {
                return None;
            }
            let mut parts: Vec<String> = Vec::new();
            if style != "normal" {
                parts.push(style.to_owned());
            }
            if variant != "normal" {
                parts.push(variant.to_owned());
            }
            if weight != "400" && weight != "normal" {
                parts.push(weight.to_owned());
            }
            if stretch != "normal" && stretch != "100%" {
                parts.push(stretch.to_owned());
            }
            if line_height == "normal" {
                parts.push(size.to_owned());
            } else {
                parts.push(format!("{size} / {line_height}"));
            }
            parts.push(family.to_owned());
            parts.join(" ")
        }
        "flex" => format!("{} {} {}", v(0), v(1), v(2)),
        "flex-flow" => {
            let mut parts: Vec<&str> = Vec::new();
            if v(0) != "row" {
                parts.push(v(0));
            }
            if v(1) != "nowrap" {
                parts.push(v(1));
            }
            if parts.is_empty() { "row".to_owned() } else { parts.join(" ") }
        }
        "text-decoration" => {
            let (line, thickness, style, color) = (v(0), v(1), v(2), v(3));
            let mut parts: Vec<&str> = Vec::new();
            if line != "none" {
                parts.push(line);
            }
            if thickness != "auto" {
                parts.push(thickness);
            }
            if style != "solid" {
                parts.push(style);
            }
            if color != "currentcolor" {
                parts.push(color);
            }
            if parts.is_empty() { "none".to_owned() } else { parts.join(" ") }
        }
        "list-style" => {
            let (ty, position, image) = (v(0), v(1), v(2));
            let mut parts: Vec<&str> = Vec::new();
            if position != "outside" {
                parts.push(position);
            }
            if image != "none" {
                parts.push(image);
            }
            if ty != "disc" || parts.is_empty() {
                parts.push(ty);
            }
            parts.join(" ")
        }
        "place-items" | "word-wrap" | "-webkit-box-decoration-break" | "-webkit-user-select" | "-webkit-transform"
        | "-webkit-transform-origin" | "-webkit-backface-visibility" => v(0).to_owned(),
        // A grid line part is left out when it is what the shorthand would
        // give it: a named start line repeated, else `auto`.
        "grid-row" | "grid-column" => {
            if v(1) == grid_line_default_css(v(0)) {
                v(0).to_owned()
            } else {
                format!("{} / {}", v(0), v(1))
            }
        }
        "grid-area" => {
            let (rs, cs, re, ce) = (v(0), v(1), v(2), v(3));
            let mut parts = vec![rs, cs, re, ce];
            if ce == grid_line_default_css(cs) {
                parts.pop();
                if re == grid_line_default_css(rs) {
                    parts.pop();
                    if cs == grid_line_default_css(rs) {
                        parts.pop();
                    }
                }
            }
            parts.join(" / ")
        }
        "grid-template" => {
            if v(0) == "none" && v(1) == "none" {
                "none".to_owned()
            } else {
                format!("{} / {}", v(0), v(1))
            }
        }
        "grid" => {
            let (rows, columns, auto_rows, auto_columns, flow) = (v(0), v(1), v(2), v(3), v(4));
            if auto_rows == "auto" && auto_columns == "auto" && flow == "row" {
                if rows == "none" && columns == "none" {
                    "none".to_owned()
                } else {
                    format!("{rows} / {columns}")
                }
            } else if rows == "none" && auto_columns == "auto" && (flow == "row" || flow == "dense") {
                let keyword = if flow == "dense" { "auto-flow dense" } else { "auto-flow" };
                if auto_rows == "auto" {
                    format!("{keyword} / {columns}")
                } else {
                    format!("{keyword} {auto_rows} / {columns}")
                }
            } else if columns == "none" && auto_rows == "auto" && (flow == "column" || flow == "column dense") {
                let keyword = if flow == "column dense" { "auto-flow dense" } else { "auto-flow" };
                if auto_columns == "auto" {
                    format!("{rows} / {keyword}")
                } else {
                    format!("{rows} / {keyword} {auto_columns}")
                }
            } else {
                return None;
            }
        }
        "outline" => {
            // Chrome's order: colour, style, width; the initial parts left
            // out, `none` alone for the initial value.
            let (width, style, color) = (v(0), v(1), v(2));
            let mut parts: Vec<&str> = Vec::new();
            if color != "currentcolor" {
                parts.push(color);
            }
            if style != "none" {
                parts.push(style);
            }
            if width != "medium" {
                parts.push(width);
            }
            if parts.is_empty() { "none".to_owned() } else { parts.join(" ") }
        }
        _ => return None,
    })
}

/// The serialized grid line a shorthand gives an omitted part after the
/// line `start`: the same name when `start` is a lone name, else `auto`.
fn grid_line_default_css(start: &str) -> &str {
    let mut chars = start.chars();
    let ident_start = match (chars.next(), chars.next()) {
        (Some(c), _) if c.is_alphabetic() || c == '_' => true,
        (Some('-'), Some(d)) => !d.is_ascii_digit(),
        _ => false,
    };
    let lone_name = !start.contains(' ') && start != "auto" && ident_start;
    if lone_name { start } else { "auto" }
}

/// Four side values collapsed to one, two, three or four.
fn sides_css(values: &[String]) -> String {
    let (t, r, b, l) = (&values[0], &values[1], &values[2], &values[3]);
    if t == r && r == b && b == l {
        t.clone()
    } else if t == b && r == l {
        format!("{t} {r}")
    } else if r == l {
        format!("{t} {r} {b}")
    } else {
        format!("{t} {r} {b} {l}")
    }
}

/// `border-*`: width, style and color, the initial parts left out.
fn border_side_css(width: &str, style: &str, color: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if width != "medium" {
        parts.push(width);
    }
    if style != "none" {
        parts.push(style);
    }
    if color != "currentcolor" {
        parts.push(color);
    }
    if parts.is_empty() { "none".to_owned() } else { parts.join(" ") }
}

/// The shorthands a longhand belongs to, in the order the CSSOM tries
/// them (`SHORTHANDS`).
fn shorthands_of(id: PropertyId) -> impl Iterator<Item = &'static str> {
    // A shorthand that expands to a single longhand here (`place-items`,
    // whose other longhand the table lacks; the `word-wrap` alias) is not
    // written in its place.
    SHORTHANDS
        .iter()
        .copied()
        .filter(move |s| shorthand_longhands(s).is_some_and(|l| l.len() > 1 && l.contains(&id)))
}

/// Serialize a declaration block per CSSOM "serialize a CSS declaration
/// block": each declaration as `name: value;` (` !important` kept), with
/// every run of longhands that makes up a whole shorthand of the same
/// importance written as that shorthand once, in the position of its
/// first longhand.
pub fn serialize_block(decls: &[Declaration]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut serialized = vec![false; decls.len()];
    for (i, d) in decls.iter().enumerate() {
        if serialized[i] {
            continue;
        }
        let Some(id) = d.value.id() else {
            // A custom property, or a `var()` declaration under its name.
            out.push(declaration_text(d.value.property_name(), &serialize_declared(&d.value), d.important));
            serialized[i] = true;
            continue;
        };
        let mut done = false;
        'shorthands: for sh in shorthands_of(id) {
            let Some(longhands) = shorthand_longhands(sh) else { continue };
            // Every longhand must be declared, not yet serialized, and
            // all of the same importance.
            let mut positions: Vec<usize> = Vec::with_capacity(longhands.len());
            for lh in longhands {
                let Some(pos) = decls.iter().position(|x| x.value.id() == Some(*lh)) else {
                    continue 'shorthands;
                };
                if serialized[pos] || decls[pos].important != d.important {
                    continue 'shorthands;
                }
                positions.push(pos);
            }
            let value = shorthand_value(sh, &positions.iter().map(|&p| &decls[p].value).collect::<Vec<_>>());
            let Some(value) = value else { continue };
            for p in positions {
                serialized[p] = true;
            }
            out.push(declaration_text(sh, &value, d.important));
            done = true;
            break;
        }
        if !done {
            out.push(declaration_text(id.name(), &serialize_declared(&d.value), d.important));
            serialized[i] = true;
        }
    }
    out.join(" ")
}

/// The value a shorthand serializes to from its longhands' declared
/// values: one CSS-wide keyword they all share, or their values combined;
/// `None` when it has no single value.
pub fn shorthand_value(name: &str, values: &[&DeclaredValue]) -> Option<String> {
    let keyword = |v: &DeclaredValue| match v {
        DeclaredValue::Inherit(_) => Some("inherit"),
        DeclaredValue::Initial(_) => Some("initial"),
        DeclaredValue::Unset(_) => Some("unset"),
        DeclaredValue::Revert(_) => Some("revert"),
        DeclaredValue::RevertLayer(_) => Some("revert-layer"),
        _ => None,
    };
    let first = values.first()?;
    if let Some(k) = keyword(first) {
        return values.iter().all(|v| keyword(v) == Some(k)).then(|| k.to_owned());
    }
    let mut strings = Vec::with_capacity(values.len());
    for v in values {
        match v {
            DeclaredValue::Value(p) => strings.push(serialize_longhand(p)),
            _ => return None,
        }
    }
    serialize_shorthand(name, &strings)
}

fn declaration_text(name: &str, value: &str, important: bool) -> String {
    let mut s = String::with_capacity(name.len() + value.len() + 14);
    s.push_str(name);
    s.push_str(": ");
    s.push_str(value);
    if important {
        s.push_str(" !important");
    }
    s.push(';');
    s
}

// ----- resolved values (getComputedStyle) -----

/// What layout decided for an element with a box: its border box size,
/// used margins and paddings, and used insets when positioned. Resolved
/// values use these where the CSSOM says the used value is reported.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsedValues {
    pub width: f32,
    pub height: f32,
    pub margin: Sides<f32>,
    pub padding: Sides<f32>,
    /// Used `top`, `right`, `bottom`, `left` for an absolutely or fixed
    /// positioned box.
    pub inset: Option<Sides<f32>>,
    /// A grid container's tracks as laid out.
    pub grid: Option<std::sync::Arc<UsedGrid>>,
}

/// A grid container's used columns and rows (CSS Grid 1 §7.2.6: the
/// resolved value of `grid-template-columns`/`-rows` lists every track).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsedGrid {
    pub columns: UsedTracks,
    pub rows: UsedTracks,
}

/// The tracks of one axis: how many implicit ones come before the
/// explicit grid, how many are explicit, and every track's size.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsedTracks {
    pub leading_implicit: usize,
    pub explicit: usize,
    pub sizes: Vec<f32>,
}

/// The resolved track list of a grid container: each track's used size,
/// the explicit grid's line names where they are.
fn used_tracks_css(tracks: &UsedTracks, list: &crate::grid::TrackList<ComputedLp>) -> String {
    if tracks.sizes.is_empty() {
        return "none".to_owned();
    }
    let (_, names) = list.expanded();
    let mut parts: Vec<String> = Vec::new();
    let named = |line: usize, parts: &mut Vec<String>| {
        if line >= tracks.leading_implicit
            && let Some(n) = names.get(line - tracks.leading_implicit)
            && !n.is_empty()
        {
            parts.push(crate::grid::names_css(n));
        }
    };
    for (i, size) in tracks.sizes.iter().enumerate() {
        named(i, &mut parts);
        parts.push(px(*size));
    }
    named(tracks.sizes.len(), &mut parts);
    parts.join(" ")
}

fn px(v: f32) -> String {
    format!("{}px", css_number(v))
}

fn lp_css(v: ComputedLp) -> String {
    match v {
        ComputedLp::Px(p) => px(p),
        ComputedLp::Percent(p) => format!("{}%", css_number(p)),
    }
}

fn lpa_css(v: ComputedLpAuto) -> String {
    match v {
        ComputedLpAuto::Px(p) => px(p),
        ComputedLpAuto::Percent(p) => format!("{}%", css_number(p)),
        ComputedLpAuto::Auto => "auto".to_owned(),
    }
}

fn size_css(v: ComputedSize) -> String {
    match v {
        ComputedSize::Auto => "auto".to_owned(),
        ComputedSize::Px(p) => px(p),
        ComputedSize::Percent(p) => format!("{}%", css_number(p)),
        ComputedSize::MinContent => "min-content".to_owned(),
        ComputedSize::MaxContent => "max-content".to_owned(),
        ComputedSize::FitContent => "fit-content".to_owned(),
        ComputedSize::None => "none".to_owned(),
    }
}


/// The resolved value of longhand `id` per CSSOM §resolved values: the
/// used value (from `used`) for the sizes, margins, paddings and insets
/// of a rendered box, `line-height` in pixels unless `normal`, colors as
/// `rgb()`/`rgba()`, and the computed value for everything else.
pub fn resolved_value(style: &ComputedStyle, id: PropertyId, used: Option<&UsedValues>) -> String {
    use PropertyId as Id;
    let rendered = used.filter(|_| !matches!(style.display, Display::None | Display::Contents));
    let used_side = |side: fn(&Sides<f32>) -> f32, which: fn(&UsedValues) -> &Sides<f32>| {
        rendered.map(|u| px(side(which(u))))
    };
    let inset = |computed: ComputedLpAuto, side: fn(&Sides<f32>) -> f32| match rendered.and_then(|u| u.inset.as_ref()) {
        Some(i) => px(side(i)),
        None => lpa_css(computed),
    };
    match id {
        Id::Display => style.display.keyword().to_owned(),
        Id::Position => style.position.keyword().to_owned(),
        Id::Float => style.float.keyword().to_owned(),
        Id::Clear => style.clear.keyword().to_owned(),
        Id::BoxSizing => style.box_sizing.keyword().to_owned(),
        Id::Top => inset(style.inset.top, |s| s.top),
        Id::Right => inset(style.inset.right, |s| s.right),
        Id::Bottom => inset(style.inset.bottom, |s| s.bottom),
        Id::Left => inset(style.inset.left, |s| s.left),
        Id::MarginTop => used_side(|s| s.top, |u| &u.margin).unwrap_or_else(|| lpa_css(style.margin.top)),
        Id::MarginRight => used_side(|s| s.right, |u| &u.margin).unwrap_or_else(|| lpa_css(style.margin.right)),
        Id::MarginBottom => used_side(|s| s.bottom, |u| &u.margin).unwrap_or_else(|| lpa_css(style.margin.bottom)),
        Id::MarginLeft => used_side(|s| s.left, |u| &u.margin).unwrap_or_else(|| lpa_css(style.margin.left)),
        Id::PaddingTop => used_side(|s| s.top, |u| &u.padding).unwrap_or_else(|| lp_css(style.padding.top)),
        Id::PaddingRight => used_side(|s| s.right, |u| &u.padding).unwrap_or_else(|| lp_css(style.padding.right)),
        Id::PaddingBottom => used_side(|s| s.bottom, |u| &u.padding).unwrap_or_else(|| lp_css(style.padding.bottom)),
        Id::PaddingLeft => used_side(|s| s.left, |u| &u.padding).unwrap_or_else(|| lp_css(style.padding.left)),
        Id::BorderTopWidth => px(style.border_width.top),
        Id::BorderRightWidth => px(style.border_width.right),
        Id::BorderBottomWidth => px(style.border_width.bottom),
        Id::BorderLeftWidth => px(style.border_width.left),
        Id::BorderTopStyle => style.border_style.top.keyword().to_owned(),
        Id::BorderRightStyle => style.border_style.right.keyword().to_owned(),
        Id::BorderBottomStyle => style.border_style.bottom.keyword().to_owned(),
        Id::BorderLeftStyle => style.border_style.left.keyword().to_owned(),
        Id::BorderTopColor => style.border_color.top.to_css(),
        Id::BorderRightColor => style.border_color.right.to_css(),
        Id::BorderBottomColor => style.border_color.bottom.to_css(),
        Id::BorderLeftColor => style.border_color.left.to_css(),
        Id::BorderTopLeftRadius => lp_css(style.border_radius.top),
        Id::BorderTopRightRadius => lp_css(style.border_radius.right),
        Id::BorderBottomRightRadius => lp_css(style.border_radius.bottom),
        Id::BorderBottomLeftRadius => lp_css(style.border_radius.left),
        Id::Width | Id::Height => {
            // The used size, as the box-sizing says it: the border box,
            // or the content box inside the padding and borders.
            let is_inline = matches!(style.display, Display::Inline);
            match rendered.filter(|_| !is_inline) {
                Some(u) => {
                    let (border_box, pad, border) = if id == Id::Width {
                        (u.width, u.padding.left + u.padding.right, style.border_width.left + style.border_width.right)
                    } else {
                        (u.height, u.padding.top + u.padding.bottom, style.border_width.top + style.border_width.bottom)
                    };
                    px(match style.box_sizing {
                        BoxSizing::BorderBox => border_box,
                        BoxSizing::ContentBox => (border_box - pad - border).max(0.0),
                    })
                }
                None => size_css(if id == Id::Width { style.width } else { style.height }),
            }
        }
        Id::MinWidth => size_css(style.min_width),
        Id::MinHeight => size_css(style.min_height),
        Id::MaxWidth => size_css(style.max_width),
        Id::MaxHeight => size_css(style.max_height),
        Id::OverflowX => style.overflow_x.keyword().to_owned(),
        Id::OverflowY => style.overflow_y.keyword().to_owned(),
        Id::Visibility => style.visibility.keyword().to_owned(),
        Id::Color => style.color.to_css(),
        Id::BackgroundColor => style.background_color.to_css(),
        Id::BackgroundImage => layers(style.background_images.iter().map(ComputedImage::to_css)),
        Id::BackgroundPositionX => layers(style.background_position_x.iter().copied().map(position_offset_css)),
        Id::BackgroundPositionY => layers(style.background_position_y.iter().copied().map(position_offset_css)),
        Id::BackgroundBlendMode => layers(style.background_blend_mode.iter().map(|m| m.keyword().to_owned())),
        Id::BackgroundSize => layers(style.background_size.iter().map(|s| match *s {
            ComputedBackgroundSize::Cover => "cover".to_owned(),
            ComputedBackgroundSize::Contain => "contain".to_owned(),
            ComputedBackgroundSize::Explicit(x, y) => match y {
                ComputedLpAuto::Auto => lpa_css(x),
                y => format!("{} {}", lpa_css(x), lpa_css(y)),
            },
        })),
        Id::BackgroundRepeat => layers(style.background_repeat.iter().map(|r| r.to_css())),
        Id::BackgroundAttachment => layers(style.background_attachment.iter().map(|a| a.keyword().to_owned())),
        Id::BackgroundOrigin => layers(style.background_origin.iter().map(|b| b.keyword().to_owned())),
        Id::BackgroundClip => layers(style.background_clip.iter().map(|b| b.keyword().to_owned())),
        Id::FontFamily => computed_font_family_css(&style.font_family),
        Id::FontSize => px(style.font_size),
        Id::FontWeight => style.font_weight.to_string(),
        Id::FontStyle => style.font_style.keyword().to_owned(),
        Id::FontVariant => style.font_variant.keyword().to_owned(),
        Id::FontOpticalSizing => style.font_optical_sizing.keyword().to_owned(),
        Id::FontVariationSettings => variation_settings_css(&style.font_variation_settings),
        Id::FontStretch => format!("{}%", css_number(style.font_stretch)),
        Id::LineHeight => match style.line_height {
            LineHeight::Normal => "normal".to_owned(),
            lh => px(lh.to_px(style.font_size)),
        },
        Id::TextAlign => style.text_align.keyword().to_owned(),
        Id::Direction => style.direction.keyword().to_owned(),
        Id::UnicodeBidi => style.unicode_bidi.keyword().to_owned(),
        Id::TextDecorationLine => style.text_decoration_line.to_css(),
        Id::TextDecorationStyle => style.text_decoration_style.keyword().to_owned(),
        Id::TextDecorationColor => style.text_decoration_color.to_css(),
        Id::TextDecorationThickness => match style.text_decoration_thickness {
            DecorationThickness::Auto => "auto".to_owned(),
            DecorationThickness::FromFont => "from-font".to_owned(),
            DecorationThickness::Length(l) => lp_css(l),
        },
        Id::TextUnderlinePosition => style.text_underline_position.to_css(),
        Id::TextUnderlineOffset => match style.text_underline_offset {
            UnderlineOffset::Auto => "auto".to_owned(),
            UnderlineOffset::Px(p) => px(p),
        },
        Id::TextDecorationSkipInk => style.text_decoration_skip_ink.keyword().to_owned(),
        Id::TextTransform => style.text_transform.keyword().to_owned(),
        Id::WhiteSpace => style.white_space.keyword().to_owned(),
        Id::LetterSpacing => match style.letter_spacing {
            LetterSpacing::Normal => "normal".to_owned(),
            LetterSpacing::Px(p) => px(p),
        },
        Id::WordBreak => style.word_break.keyword().to_owned(),
        Id::OverflowWrap => style.overflow_wrap.keyword().to_owned(),
        Id::TextOverflow => style.text_overflow.to_css(),
        Id::BoxDecorationBreak => style.box_decoration_break.keyword().to_owned(),
        Id::ListStyleType => style.list_style_type.keyword().to_owned(),
        Id::ListStylePosition => style.list_style_position.keyword().to_owned(),
        Id::ListStyleImage => style.list_style_image.as_deref().map_or("none".to_owned(), css_url),
        Id::VerticalAlign => match style.vertical_align {
            ComputedVerticalAlign::Keyword(k) => k.keyword().to_owned(),
            ComputedVerticalAlign::Length(l) => lp_css(l),
        },
        Id::FlexDirection => style.flex_direction.keyword().to_owned(),
        Id::FlexWrap => style.flex_wrap.keyword().to_owned(),
        Id::JustifyContent => style.justify_content.keyword().to_owned(),
        Id::AlignItems => style.align_items.keyword().to_owned(),
        Id::AlignSelf => style.align_self.keyword().to_owned(),
        Id::AlignContent => style.align_content.keyword().to_owned(),
        Id::FlexGrow => css_number(style.flex_grow),
        Id::FlexShrink => css_number(style.flex_shrink),
        Id::FlexBasis => size_css(style.flex_basis),
        Id::RowGap => lp_css(style.row_gap),
        Id::ColumnGap => lp_css(style.column_gap),
        Id::Opacity => css_number(style.opacity),
        Id::ScrollbarWidth => style.scrollbar_width.keyword().to_owned(),
        Id::ScrollbarColor => match style.scrollbar_color {
            ScrollbarColor::Auto => "auto".to_owned(),
            ScrollbarColor::Colors { thumb, track } => format!("{} {}", thumb.to_css(), track.to_css()),
        },
        Id::ScrollbarGutter => style.scrollbar_gutter.keyword().to_owned(),
        Id::OverscrollBehaviorX => style.overscroll_behavior_x.keyword().to_owned(),
        Id::OverscrollBehaviorY => style.overscroll_behavior_y.keyword().to_owned(),
        Id::ScrollBehavior => style.scroll_behavior.keyword().to_owned(),
        Id::ScrollMarginTop => px(style.scroll_margin.top),
        Id::ScrollMarginRight => px(style.scroll_margin.right),
        Id::ScrollMarginBottom => px(style.scroll_margin.bottom),
        Id::ScrollMarginLeft => px(style.scroll_margin.left),
        Id::ScrollPaddingTop => lpa_css(style.scroll_padding.top),
        Id::ScrollPaddingRight => lpa_css(style.scroll_padding.right),
        Id::ScrollPaddingBottom => lpa_css(style.scroll_padding.bottom),
        Id::ScrollPaddingLeft => lpa_css(style.scroll_padding.left),
        Id::ScrollSnapType => style.scroll_snap_type.resolved_css(),
        Id::ScrollSnapAlign => style.scroll_snap_align.to_css(),
        Id::ScrollSnapStop => style.scroll_snap_stop.keyword().to_owned(),
        Id::OverflowAnchor => style.overflow_anchor.keyword().to_owned(),
        Id::BoxShadow => {
            if style.box_shadow.is_empty() {
                "none".to_owned()
            } else {
                layers(style.box_shadow.iter().map(ComputedBoxShadow::to_css))
            }
        }
        Id::OutlineWidth => px(style.outline_width),
        Id::OutlineStyle => style.outline_style.keyword().to_owned(),
        Id::OutlineColor => style.outline_color.to_css(),
        Id::OutlineOffset => px(style.outline_offset),
        Id::Cursor => style.cursor.to_css(),
        Id::PointerEvents => style.pointer_events.keyword().to_owned(),
        Id::UserSelect => style.user_select.keyword().to_owned(),
        Id::ZIndex => style.z_index.to_css(),
        Id::Isolation => match style.isolation {
            Isolation::Auto => "auto".to_owned(),
            Isolation::Isolate => "isolate".to_owned(),
        },
        Id::WillChange => style.will_change.to_css(),
        // The resolved `transform` is the matrix, its percentages
        // resolved against the box (CSSOM View; `none` without one).
        Id::Transform => {
            let (w, h) = rendered.map_or((0.0, 0.0), |u| (u.width, u.height));
            match crate::transform::transform_matrix(style, w, h) {
                Some(m) => m.to_css(),
                None => "none".to_owned(),
            }
        }
        Id::TransformOrigin => {
            let (w, h) = rendered.map_or((0.0, 0.0), |u| (u.width, u.height));
            let (x, y, z) = style.transform_origin;
            let mut s = format!("{} {}", px(x.px + x.percent / 100.0 * w), px(y.px + y.percent / 100.0 * h));
            if z != 0.0 {
                s.push(' ');
                s.push_str(&px(z));
            }
            s
        }
        Id::TransformBox => style.transform_box.keyword().to_owned(),
        Id::TransformStyle => match style.transform_style {
            TransformStyle::Flat => "flat".to_owned(),
            TransformStyle::Preserve3d => "preserve-3d".to_owned(),
        },
        Id::BackfaceVisibility => match style.backface_visibility {
            BackfaceVisibility::Visible => "visible".to_owned(),
            BackfaceVisibility::Hidden => "hidden".to_owned(),
        },
        Id::Perspective => style.perspective.map_or("none".to_owned(), px),
        Id::PerspectiveOrigin => {
            let (w, h) = rendered.map_or((0.0, 0.0), |u| (u.width, u.height));
            let (x, y) = style.perspective_origin;
            format!("{} {}", px(x.px + x.percent / 100.0 * w), px(y.px + y.percent / 100.0 * h))
        }
        Id::Translate => match style.translate {
            None => "none".to_owned(),
            Some((x, y, z)) => {
                let mut s = format!("{} {}", lp_css(x), lp_css(y));
                if z != 0.0 {
                    s.push(' ');
                    s.push_str(&px(z));
                }
                s
            }
        },
        Id::Rotate => match style.rotate {
            None => "none".to_owned(),
            Some((x, y, z, a)) => rotate_css(x, y, z, a),
        },
        Id::Scale => match style.scale {
            None => "none".to_owned(),
            Some((x, y, z)) => scale_css(x, y, z),
        },
        Id::BorderSpacing => {
            let (h, v) = style.border_spacing;
            if h == v { px(h) } else { format!("{} {}", px(h), px(v)) }
        }
        Id::BorderCollapse => match style.border_collapse {
            BorderCollapse::Separate => "separate".to_owned(),
            BorderCollapse::Collapse => "collapse".to_owned(),
        },
        Id::EmptyCells => match style.empty_cells {
            EmptyCells::Show => "show".to_owned(),
            EmptyCells::Hide => "hide".to_owned(),
        },
        // A grid container's tracks are its used sizes, every one listed;
        // anything else gives the computed list.
        Id::GridTemplateColumns => match rendered.and_then(|u| u.grid.as_ref()) {
            Some(g) => used_tracks_css(&g.columns, &style.grid_template_columns),
            None => style.grid_template_columns.to_css(),
        },
        Id::GridTemplateRows => match rendered.and_then(|u| u.grid.as_ref()) {
            Some(g) => used_tracks_css(&g.rows, &style.grid_template_rows),
            None => style.grid_template_rows.to_css(),
        },
        Id::GridAutoColumns => crate::grid::auto_tracks_css(&style.grid_auto_columns),
        Id::GridAutoRows => crate::grid::auto_tracks_css(&style.grid_auto_rows),
        Id::GridAutoFlow => style.grid_auto_flow.to_css(),
        Id::GridRowStart => style.grid_row_start.to_css(),
        Id::GridRowEnd => style.grid_row_end.to_css(),
        Id::GridColumnStart => style.grid_column_start.to_css(),
        Id::GridColumnEnd => style.grid_column_end.to_css(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stylesheet::parse_cssom_block;

    fn block(css: &str) -> String {
        serialize_block(&parse_cssom_block(css))
    }

    /// "Working sites first" line 6: `font-optical-sizing` and
    /// `font-variation-settings` parse, refuse bad tags, keep the last of a
    /// repeated tag, serialize, and are reset by `font` (which then only
    /// collapses back while they are initial).
    #[test]
    fn font_variation_properties_parse_and_serialize() {
        let cases = [
            ("font-optical-sizing: NONE", "font-optical-sizing: none;"),
            ("font-optical-sizing: auto", "font-optical-sizing: auto;"),
            ("font-optical-sizing: 1", ""),
            ("font-variation-settings: normal", "font-variation-settings: normal;"),
            ("font-variation-settings: \"wght\" 650, 'wdth' 80.5", "font-variation-settings: \"wght\" 650, \"wdth\" 80.5;"),
            ("font-variation-settings: \"wght\" 300, \"wdth\" 80, \"wght\" 700", "font-variation-settings: \"wdth\" 80, \"wght\" 700;"),
            ("font-variation-settings: \"wgh\" 300", ""),
            ("font-variation-settings: \"wght\"", ""),
            ("font-variation-settings: wght 300", ""),
            ("font: 12px serif", "font: 12px serif;"),
        ];
        for (css, want) in cases {
            assert_eq!(block(css), want, "{css}");
        }
        assert_eq!(
            block("font: 12px serif; font-variation-settings: \"wght\" 500"),
            "font-style: normal; font-variant: normal; font-weight: 400; font-stretch: normal; font-size: 12px; line-height: normal; font-family: serif; font-optical-sizing: auto; font-variation-settings: \"wght\" 500;",
            "not initial: no shorthand"
        );
        // Computed: inherited.
        use crate::{Stylist, Viewport, cascade::compute_styles, ua::ua_stylesheet};
        let doc = browser_dom::parse_html(b"<div style='font-optical-sizing: none; font-variation-settings: \"slnt\" -10'><p id=p>x</p></div>");
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let p = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "p")).expect("p");
        let s = styles.get(p).expect("style");
        assert_eq!(resolved_value(s, PropertyId::FontOpticalSizing, None), "none");
        assert_eq!(resolved_value(s, PropertyId::FontVariationSettings, None), "\"slnt\" -10");
    }

    /// "Working sites first" line 5: the grid properties parse, refuse
    /// what the grammar refuses, serialize as written (shortest forms),
    /// and the shorthands expand and collapse back.
    #[test]
    fn grid_properties_parse_and_serialize() {
        let cases = [
            ("grid-template-columns: 100px 1fr 2FR", "grid-template-columns: 100px 1fr 2fr;"),
            ("grid-template-columns: [a] repeat(2, [b] 50px) [c]", "grid-template-columns: [a] repeat(2, [b] 50px) [c];"),
            ("grid-template-rows: minmax(100px, 1fr) fit-content(20%) auto min-content", "grid-template-rows: minmax(100px, 1fr) fit-content(20%) auto min-content;"),
            ("grid-template-columns: none", "grid-template-columns: none;"),
            ("grid-auto-rows: 50px minmax(10px, auto)", "grid-auto-rows: 50px minmax(10px, auto);"),
            ("grid-auto-flow: dense row", "grid-auto-flow: dense;"),
            ("grid-auto-flow: column dense", "grid-auto-flow: column dense;"),
            ("grid-auto-flow: column", "grid-auto-flow: column;"),
            ("grid-row-start: span 2", "grid-row-start: span 2;"),
            ("grid-column-end: 2 foo", "grid-column-end: 2 foo;"),
            ("grid-column-start: span foo", "grid-column-start: span foo;"),
            ("grid-row-end: -1", "grid-row-end: -1;"),
            ("grid-row: 2 / span 3", "grid-row: 2 / span 3;"),
            ("grid-row: 2", "grid-row: 2;"),
            ("grid-column: foo", "grid-column: foo;"),
            ("grid-column: 1 / auto", "grid-column: 1;"),
            ("grid-area: 1 / 2", "grid-area: 1 / 2;"),
            ("grid-area: a", "grid-area: a;"),
            ("grid-area: 1 / 2 / 3 / 4", "grid-area: 1 / 2 / 3 / 4;"),
            ("grid-template: 100px auto / 1fr 2fr", "grid-template: 100px auto / 1fr 2fr;"),
            ("grid-template: none", "grid-template: none;"),
            ("grid: auto-flow 50px / 1fr 1fr", "grid: auto-flow 50px / 1fr 1fr;"),
            ("grid: 100px / auto-flow dense 20px", "grid: 100px / auto-flow dense 20px;"),
            ("grid: 10px / 20px", "grid: 10px / 20px;"),
        ];
        for (css, want) in cases {
            assert_eq!(block(css), want, "{css}");
        }
        // Refused: auto-repeat (a later part of grid), flexible minima,
        // two name sets in a row, zero lines, a zero span, `span` as a
        // name, negative sizes, an empty list.
        for css in [
            "grid-template-columns: repeat(auto-fill, 100px)",
            "grid-template-columns: minmax(1fr, 100px)",
            "grid-template-columns: [a] [b] 10px",
            "grid-template-columns: -10px",
            "grid-template-columns: [a]",
            "grid-row-start: 0",
            "grid-row-start: span 0",
            "grid-row-start: span",
            "grid-auto-flow: row column",
            "grid-template: \"a b\" / 1fr",
        ] {
            assert_eq!(block(css), "", "{css} refused");
        }
    }

    #[test]
    fn longhands_serialize_as_written() {
        let cases = [
            ("color: RED", "color: red;"),
            ("color: #ff0000", "color: rgb(255, 0, 0);"),
            ("background-color: transparent", "background-color: transparent;"),
            ("width: 10PX", "width: 10px;"),
            ("width: 1.50em", "width: 1.5em;"),
            ("width: 10pt", "width: 10pt;"),
            ("width: calc(1px + 2px)", "width: calc(3px);"),
            ("width: calc(100% - 10px)", "width: calc(100% - 10px);"),
            ("margin-top: 0", "margin-top: 0px;"),
            ("display: TABLE-CELL", "display: table-cell;"),
            ("display: block flow", "display: block;"),
            ("float: inline-start", "float: inline-start;"),
            ("overflow-x: overlay", "overflow-x: overlay;"),
            ("vertical-align: 5px", "vertical-align: 5px;"),
            ("border-top-left-radius: 1px 2px", "border-top-left-radius: 1px 2px;"),
            ("font-family: \"Helvetica Neue\", Arial, sans-serif", "font-family: \"Helvetica Neue\", Arial, sans-serif;"),
            ("font-family: Helvetica Neue", "font-family: \"Helvetica Neue\";"),
            ("font-family: \"serif\"", "font-family: \"serif\";"),
            ("font-size: LARGE", "font-size: large;"),
            ("font-weight: bold", "font-weight: 700;"),
            ("line-height: 1.20", "line-height: 1.2;"),
            ("opacity: 50%", "opacity: 0.5;"),
            ("background-image: url(a.png)", "background-image: url(\"a.png\");"),
            ("background-image: linear-gradient(red, blue)", "background-image: linear-gradient(red, blue);"),
            ("list-style-type: \"- \"", "list-style-type: \"- \";"),
            ("align-items: safe center", "align-items: safe center;"),
            ("text-decoration-line: overline underline", "text-decoration-line: underline overline;"),
            ("--x: 1px", "--x: 1px;"),
            ("color: var(--c, red)", "color: var(--c, red);"),
            ("margin: var(--m) !important", "margin: var(--m) !important;"),
            ("color: inherit", "color: inherit;"),
            ("margin: unset", "margin: unset;"),
            ("color: revert", "color: revert;"),
            ("margin: revert-layer", "margin: revert-layer;"),
            ("font-style: oblique 10deg", "font-style: oblique 10deg;"),
            ("align-items: first baseline", "align-items: first baseline;"),
            ("background-image: url(a.png), none, linear-gradient(red, blue)", "background-image: url(\"a.png\"), none, linear-gradient(red, blue);"),
            ("background-position: left 10px top, center", "background-position: left 10px top, center center;"),
            ("color: red !IMPORTANT", "color: red !important;"),
            ("zzz: 1; color: blue", "color: blue;"),
            ("letter-spacing: normal", "letter-spacing: normal;"),
            ("letter-spacing: 0.10EM", "letter-spacing: 0.1em;"),
            ("letter-spacing: calc(1px + 1px)", "letter-spacing: calc(2px);"),
            ("letter-spacing: 10%", ""),
            ("word-break: BREAK-ALL", "word-break: break-all;"),
            ("word-break: break-word", "word-break: break-word;"),
            ("overflow-wrap: anywhere", "overflow-wrap: anywhere;"),
            ("word-wrap: break-word", "overflow-wrap: break-word;"),
            ("word-wrap: inherit", "overflow-wrap: inherit;"),
            ("text-overflow: ELLIPSIS", "text-overflow: ellipsis;"),
            ("text-overflow: \"--\"", "text-overflow: \"--\";"),
            ("text-overflow: clip ellipsis", ""),
            ("scrollbar-width: THIN", "scrollbar-width: thin;"),
            ("scrollbar-color: red #000", "scrollbar-color: red rgb(0, 0, 0);"),
            ("scrollbar-color: red", ""),
            ("scrollbar-gutter: both-edges stable", "scrollbar-gutter: stable both-edges;"),
            ("scroll-behavior: smooth", "scroll-behavior: smooth;"),
            ("scroll-margin-top: -0.5em", "scroll-margin-top: -0.5em;"),
            ("scroll-margin-top: calc(1px + 1em)", "scroll-margin-top: calc(1em + 1px);"),
            ("scroll-margin-top: 10%", ""),
            ("scroll-padding-top: 10%", "scroll-padding-top: 10%;"),
            ("scroll-padding-top: auto", "scroll-padding-top: auto;"),
            ("scroll-padding-top: -1px", ""),
            ("scroll-snap-type: x proximity", "scroll-snap-type: x proximity;"),
            ("scroll-snap-type: Y", "scroll-snap-type: y;"),
            ("scroll-snap-align: start start", "scroll-snap-align: start;"),
            ("scroll-snap-align: start end", "scroll-snap-align: start end;"),
            ("scroll-snap-stop: always", "scroll-snap-stop: always;"),
            ("overflow-anchor: none", "overflow-anchor: none;"),
            ("box-shadow: 1px 2px red inset", "box-shadow: red 1px 2px inset;"),
            ("box-shadow: 0 0 10px", "box-shadow: 0px 0px 10px;"),
            ("box-shadow: none", "box-shadow: none;"),
            ("outline-style: AUTO", "outline-style: auto;"),
            ("outline-offset: 3px", "outline-offset: 3px;"),
            ("cursor: url(a.cur) 1 2, pointer", "cursor: url(\"a.cur\") 1 2, pointer;"),
            ("pointer-events: none", "pointer-events: none;"),
            ("-webkit-user-select: none", "user-select: none;"),
            ("z-index: 03", "z-index: 3;"),
            ("transform: translateX(10PX) rotate(45DEG) scale(2)", "transform: translateX(10px) rotate(45deg) scale(2, 2);"),
            ("transform: none", "transform: none;"),
            ("-webkit-transform: translate(1px)", "transform: translate(1px, 0px);"),
            ("transform-origin: left top", "transform-origin: left top;"),
            ("transform-origin: 10px 20px 5px", "transform-origin: 10px 20px 5px;"),
            ("translate: 10px", "translate: 10px;"),
            ("rotate: x 90deg", "rotate: x 90deg;"),
            ("rotate: 1 0 0 90deg", "rotate: x 90deg;"),
            ("scale: 2 2", "scale: 2;"),
            ("scale: 1 2 3", "scale: 1 2 3;"),
            ("will-change: transform, Opacity", "will-change: transform, opacity;"),
            ("backface-visibility: hidden", "backface-visibility: hidden;"),
            ("perspective: 50em", "perspective: 50em;"),
        ];
        for (input, expected) in cases {
            assert_eq!(block(input), expected, "for {input}");
        }
    }

    #[test]
    fn shorthands_collapse_and_expand() {
        let cases = [
            ("margin: 1px", "margin: 1px;"),
            ("margin: 1px 2px", "margin: 1px 2px;"),
            ("margin: 1px 2px 3px", "margin: 1px 2px 3px;"),
            ("margin: 1px 2px 3px 4px", "margin: 1px 2px 3px 4px;"),
            ("margin-top: 1px; margin-right: 1px; margin-bottom: 1px; margin-left: 1px", "margin: 1px;"),
            ("margin-top: 1px; margin-left: 2px", "margin-top: 1px; margin-left: 2px;"),
            ("margin-top: 1px !important; margin-right: 1px; margin-bottom: 1px; margin-left: 1px", "margin-top: 1px !important; margin-right: 1px; margin-bottom: 1px; margin-left: 1px;"),
            ("border: 1px solid red", "border: 1px solid red;"),
            ("border: solid", "border: solid;"),
            ("border: 1px solid red; border-left-color: blue", "border-width: 1px; border-style: solid; border-color: red red red blue;"),
            ("border-top: 2px dashed", "border-top: 2px dashed;"),
            ("border-radius: 1px 2px / 3px", "border-radius: 1px 2px / 3px;"),
            ("border-radius: 4px", "border-radius: 4px;"),
            ("color: red; margin: 0; color: blue", "margin: 0px; color: blue;"),
            ("color: red !important; color: blue", "color: red !important;"),
            ("background: red", "background: red;"),
            ("background: url(a.png) no-repeat center / cover", "background: url(\"a.png\") center center / cover no-repeat;"),
            ("background: none", "background: none;"),
            ("background: url(a.png) no-repeat, url(b.png) center red", "background: url(\"a.png\") no-repeat, red url(\"b.png\") center center;"),
            // The later `background-image` replaces the shorthand's and
            // moves to the end; the lists no longer agree, so no shorthand.
            ("background: url(a.png); background-image: url(a.png), url(b.png)", "background-color: transparent; background-position: 0% 0%; background-size: auto; background-repeat: repeat; background-attachment: scroll; background-origin: padding-box; background-clip: border-box; background-image: url(\"a.png\"), url(\"b.png\");"),
            ("text-decoration: underline dotted red", "text-decoration: underline dotted red;"),
            ("grid-gap: 1px 2px", "gap: 1px 2px;"),
            ("font: oblique 10deg 12px serif", "font: oblique 10deg 12px serif;"),
            ("font: italic bold 12px/1.5 serif", "font: italic 700 12px / 1.5 serif;"),
            ("font: 12px Arial", "font: 12px Arial;"),
            ("font: menu", "font: 13px system-ui;"),
            ("overflow: hidden", "overflow: hidden;"),
            ("overflow: hidden auto", "overflow: hidden auto;"),
            ("flex: 1", "flex: 1 1 0px;"),
            ("flex: none", "flex: 0 0 auto;"),
            ("flex-flow: column wrap", "flex-flow: column wrap;"),
            ("flex-flow: row", "flex-flow: row;"),
            ("gap: 1px", "gap: 1px;"),
            ("gap: 1px 2px", "gap: 1px 2px;"),
            ("text-decoration: underline", "text-decoration: underline;"),
            ("text-decoration: red dotted underline", "text-decoration: underline dotted red;"),
            ("text-decoration: none", "text-decoration: none;"),
            ("list-style: none", "list-style: none;"),
            ("list-style: inside square", "list-style: inside square;"),
            ("list-style: disc", "list-style: disc;"),
            ("inset: 1px 2px", "inset: 1px 2px;"),
            ("place-content: center", "place-content: center;"),
            ("place-content: center start", "place-content: center start;"),
            ("margin: inherit", "margin: inherit;"),
            ("margin-top: inherit; margin-right: 1px; margin-bottom: inherit; margin-left: inherit", "margin-top: inherit; margin-right: 1px; margin-bottom: inherit; margin-left: inherit;"),
            ("scroll-margin: 1px 2px", "scroll-margin: 1px 2px;"),
            ("scroll-margin: 1px 2px 3px 4px", "scroll-margin: 1px 2px 3px 4px;"),
            ("scroll-padding: auto 1px", "scroll-padding: auto 1px;"),
            ("scroll-padding: inherit", "scroll-padding: inherit;"),
            ("overscroll-behavior: contain", "overscroll-behavior: contain;"),
            ("overscroll-behavior: auto none", "overscroll-behavior: auto none;"),
            ("overscroll-behavior-x: contain; overscroll-behavior-y: contain", "overscroll-behavior: contain;"),
            ("overscroll-behavior-y: contain", "overscroll-behavior-y: contain;"),
            ("outline: 2px dashed red", "outline: red dashed 2px;"),
            ("outline: none", "outline: none;"),
            ("outline: auto", "outline: auto;"),
        ];
        for (input, expected) in cases {
            assert_eq!(block(input), expected, "for {input}");
        }
    }

    #[test]
    fn padding_shorthand_with_auto_is_dropped() {
        // `padding: 0 auto` is invalid; nothing is declared.
        assert_eq!(block("padding: 0 auto"), "");
    }

    #[test]
    fn resolved_values_from_computed_and_used() {
        let mut style = ComputedStyle::initial();
        style.display = Display::Block;
        style.width = ComputedSize::Percent(50.0);
        style.padding.left = ComputedLp::Percent(10.0);
        style.margin.left = ComputedLpAuto::Auto;
        style.line_height = LineHeight::Number(1.5);
        style.font_size = 20.0;
        style.color = Rgba::rgb8(1, 2, 3);
        style.background_color = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.5 };
        style.font_family = std::sync::Arc::from("\"Helvetica Neue\", \"Arial\", sans-serif");
        style.border_width.top = 2.0;
        style.inset.top = ComputedLpAuto::Px(5.0);
        style.text_overflow = TextOverflow::String("~".into());
        // Not rendered: computed values.
        assert_eq!(resolved_value(&style, PropertyId::LetterSpacing, None), "normal");
        style.letter_spacing = LetterSpacing::Px(1.5);
        assert_eq!(resolved_value(&style, PropertyId::LetterSpacing, None), "1.5px");
        assert_eq!(resolved_value(&style, PropertyId::WordBreak, None), "normal");
        assert_eq!(resolved_value(&style, PropertyId::OverflowWrap, None), "normal");
        assert_eq!(resolved_value(&style, PropertyId::TextOverflow, None), "\"~\"");
        assert_eq!(resolved_value(&style, PropertyId::BoxDecorationBreak, None), "slice");
        assert_eq!(serialize_shorthand("-webkit-box-decoration-break", &["clone".to_owned()]), Some("clone".to_owned()));
        assert_eq!(resolved_value(&style, PropertyId::Width, None), "50%");
        assert_eq!(resolved_value(&style, PropertyId::PaddingLeft, None), "10%");
        assert_eq!(resolved_value(&style, PropertyId::MarginLeft, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::LineHeight, None), "30px");
        assert_eq!(resolved_value(&style, PropertyId::Color, None), "rgb(1, 2, 3)");
        assert_eq!(resolved_value(&style, PropertyId::BackgroundColor, None), "rgba(255, 0, 0, 0.5)");
        assert_eq!(resolved_value(&style, PropertyId::FontFamily, None), "\"Helvetica Neue\", Arial, sans-serif");
        assert_eq!(resolved_value(&style, PropertyId::FontSize, None), "20px");
        assert_eq!(resolved_value(&style, PropertyId::BorderTopWidth, None), "2px");
        assert_eq!(resolved_value(&style, PropertyId::Top, None), "5px");
        assert_eq!(resolved_value(&style, PropertyId::BackgroundPositionX, None), "0%");
        assert_eq!(resolved_value(&style, PropertyId::BackgroundPositionY, None), "0%");
        assert_eq!(
            serialize_shorthand("background-position", &["0%, 10px".to_owned(), "0%, calc(100% - 4px)".to_owned()]),
            Some("0% 0%, 10px calc(100% - 4px)".to_owned())
        );
        assert_eq!(resolved_value(&style, PropertyId::BackgroundBlendMode, None), "normal");
        // The scroll container properties: lengths in px, `proximity`
        // left out, colours as `rgb()`.
        style.scroll_margin.top = -4.0;
        style.scroll_padding.left = ComputedLpAuto::Percent(10.0);
        style.scroll_snap_type = ScrollSnapType::Axis {
            axis: SnapAxis::X,
            strictness: Some(SnapStrictness::Proximity),
        };
        style.scrollbar_color = ScrollbarColor::Colors {
            thumb: Rgba::rgb8(1, 2, 3),
            track: Rgba::BLACK,
        };
        assert_eq!(resolved_value(&style, PropertyId::ScrollMarginTop, None), "-4px");
        assert_eq!(resolved_value(&style, PropertyId::ScrollMarginLeft, None), "0px");
        assert_eq!(resolved_value(&style, PropertyId::ScrollPaddingLeft, None), "10%");
        assert_eq!(resolved_value(&style, PropertyId::ScrollPaddingTop, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::ScrollSnapType, None), "x");
        style.scroll_snap_type = ScrollSnapType::Axis {
            axis: SnapAxis::Y,
            strictness: Some(SnapStrictness::Mandatory),
        };
        assert_eq!(resolved_value(&style, PropertyId::ScrollSnapType, None), "y mandatory");
        assert_eq!(resolved_value(&style, PropertyId::ScrollSnapAlign, None), "none");
        assert_eq!(resolved_value(&style, PropertyId::ScrollbarColor, None), "rgb(1, 2, 3) rgb(0, 0, 0)");
        assert_eq!(resolved_value(&style, PropertyId::ScrollbarGutter, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::ScrollbarWidth, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::OverscrollBehaviorX, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::ScrollBehavior, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::ScrollSnapStop, None), "normal");
        assert_eq!(resolved_value(&style, PropertyId::OverflowAnchor, None), "auto");
        style.box_shadow = vec![ComputedBoxShadow {
            inset: true,
            x: 1.0,
            y: 2.0,
            blur: 0.0,
            spread: 0.0,
            color: Rgba::rgb8(255, 0, 0),
        }];
        assert_eq!(resolved_value(&style, PropertyId::BoxShadow, None), "rgb(255, 0, 0) 1px 2px 0px 0px inset");
        style.outline_style = OutlineStyle::Auto;
        style.outline_width = 1.0;
        assert_eq!(resolved_value(&style, PropertyId::OutlineStyle, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::OutlineWidth, None), "1px");
        assert_eq!(resolved_value(&style, PropertyId::Cursor, None), "auto");
        assert_eq!(resolved_value(&style, PropertyId::UserSelect, None), "auto");
        // Transforms resolve to the matrix (percentages against the
        // box), the origin to px, the individual properties as written.
        assert_eq!(resolved_value(&style, PropertyId::Transform, None), "none");
        assert_eq!(resolved_value(&style, PropertyId::ZIndex, None), "auto");
        style.z_index = ZIndex::Int(4);
        style.transform = vec![
            ComputedTransformFunction::Translate(ComputedLp::Percent(50.0), ComputedLp::Px(10.0), 0.0),
            ComputedTransformFunction::Scale(2.0, 3.0, 1.0),
        ];
        assert_eq!(resolved_value(&style, PropertyId::ZIndex, None), "4");
        assert_eq!(resolved_value(&style, PropertyId::Transform, None), "matrix(2, 0, 0, 3, 0, 10)", "unrendered: 0 for the percentage");
        // Rendered: used values.
        let used = UsedValues {
            width: 200.0,
            height: 100.0,
            margin: Sides { top: 0.0, right: 0.0, bottom: 0.0, left: 37.5 },
            padding: Sides { top: 0.0, right: 0.0, bottom: 0.0, left: 40.0 },
            inset: Some(Sides { top: 5.0, right: 1.0, bottom: 2.0, left: 3.0 }),
            grid: None,
        };
        assert_eq!(resolved_value(&style, PropertyId::Transform, Some(&used)), "matrix(2, 0, 0, 3, 100, 10)");
        assert_eq!(resolved_value(&style, PropertyId::TransformOrigin, Some(&used)), "100px 50px");
        style.transform = vec![ComputedTransformFunction::Rotate(0.0, 1.0, 0.0, 90.0)];
        assert!(resolved_value(&style, PropertyId::Transform, None).starts_with("matrix3d("), "a 3D function resolves to matrix3d()");
        style.transform = Vec::new();
        style.rotate = Some((0.0, 0.0, 1.0, 45.0));
        style.scale = Some((2.0, 2.0, 1.0));
        style.translate = Some((ComputedLp::Px(5.0), ComputedLp::Percent(10.0), 0.0));
        assert_eq!(resolved_value(&style, PropertyId::Rotate, None), "45deg");
        assert_eq!(resolved_value(&style, PropertyId::Scale, None), "2");
        assert_eq!(resolved_value(&style, PropertyId::Translate, None), "5px 10%");
        assert_eq!(resolved_value(&style, PropertyId::Perspective, None), "none");
        assert_eq!(resolved_value(&style, PropertyId::BackfaceVisibility, None), "visible");
        assert_eq!(resolved_value(&style, PropertyId::Width, Some(&used)), "160px");
        style.box_sizing = BoxSizing::BorderBox;
        assert_eq!(resolved_value(&style, PropertyId::Width, Some(&used)), "200px");
        assert_eq!(resolved_value(&style, PropertyId::PaddingLeft, Some(&used)), "40px");
        assert_eq!(resolved_value(&style, PropertyId::MarginLeft, Some(&used)), "37.5px");
        assert_eq!(resolved_value(&style, PropertyId::Right, Some(&used)), "1px");
        style.display = Display::None;
        assert_eq!(resolved_value(&style, PropertyId::Width, Some(&used)), "50%", "display none: computed");
    }
}
