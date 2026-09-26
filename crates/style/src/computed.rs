//! Computed style: every longhand resolved to a concrete value, with
//! inheritance applied and lengths in pixels.

use std::sync::Arc;

use crate::custom::CustomMap;
use crate::properties::*;
use crate::values::*;

/// Line height after computation. Numbers stay numbers so they inherit
/// correctly; layout multiplies by the element's font size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeight {
    Normal,
    Number(f32),
    Px(f32),
}

impl LineHeight {
    /// Resolve to pixels for a given font size. `normal` uses 1.2.
    pub fn to_px(self, font_size: f32) -> f32 {
        match self {
            LineHeight::Normal => font_size * 1.2,
            LineHeight::Number(n) => n * font_size,
            LineHeight::Px(px) => px,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub position: Position,
    pub float: Float,
    pub clear: Clear,
    pub box_sizing: BoxSizing,
    pub inset: Sides<ComputedLpAuto>,
    pub margin: Sides<ComputedLpAuto>,
    pub padding: Sides<ComputedLp>,
    /// Border widths in px, zero when the side's style is none/hidden.
    pub border_width: Sides<f32>,
    pub border_style: Sides<BorderStyle>,
    pub border_color: Sides<Rgba>,
    /// Corner radii: top-left, top-right, bottom-right, bottom-left.
    pub border_radius: Sides<ComputedLp>,
    pub width: ComputedSize,
    pub height: ComputedSize,
    pub min_width: ComputedSize,
    pub min_height: ComputedSize,
    pub max_width: ComputedSize,
    pub max_height: ComputedSize,
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    pub visibility: Visibility,
    pub color: Rgba,
    pub background_color: Rgba,
    pub background_image: Option<Arc<str>>,
    pub font_family: Arc<str>,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_style: FontStyle,
    pub line_height: LineHeight,
    pub text_align: TextAlign,
    pub text_decoration: TextDecorationLine,
    pub text_transform: TextTransform,
    pub white_space: WhiteSpace,
    pub list_style_type: ListStyleType,
    pub vertical_align: VerticalAlign,
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub justify_content: AlignValue,
    pub align_items: AlignValue,
    pub align_self: AlignValue,
    pub align_content: AlignValue,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: ComputedSize,
    pub row_gap: ComputedLp,
    pub column_gap: ComputedLp,
    pub opacity: f32,
    /// Custom properties in effect, inherited; shared with the parent when
    /// this element declares none.
    pub custom: Arc<CustomMap>,
}

/// Viewport information the cascade needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub scale_factor: f32,
    pub prefers_dark: bool,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            scale_factor: 1.0,
            prefers_dark: false,
        }
    }
}

pub const DEFAULT_FONT_SIZE: f32 = 16.0;
pub const DEFAULT_FONT_FAMILY: &str = "serif";

impl ComputedStyle {
    /// The initial value of every property; used for the root's parent.
    pub fn initial() -> Self {
        Self {
            display: Display::Inline,
            position: Position::Static,
            float: Float::None,
            clear: Clear::None,
            box_sizing: BoxSizing::ContentBox,
            inset: Sides::all(ComputedLpAuto::Auto),
            margin: Sides::all(ComputedLpAuto::Px(0.0)),
            padding: Sides::all(ComputedLp::ZERO),
            border_width: Sides::all(0.0),
            border_style: Sides::all(BorderStyle::None),
            border_color: Sides::all(Rgba::BLACK),
            border_radius: Sides::all(ComputedLp::ZERO),
            width: ComputedSize::Auto,
            height: ComputedSize::Auto,
            min_width: ComputedSize::Auto,
            min_height: ComputedSize::Auto,
            max_width: ComputedSize::None,
            max_height: ComputedSize::None,
            overflow_x: Overflow::Visible,
            overflow_y: Overflow::Visible,
            visibility: Visibility::Visible,
            color: Rgba::BLACK,
            background_color: Rgba::TRANSPARENT,
            background_image: None,
            font_family: Arc::from(DEFAULT_FONT_FAMILY),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: 400,
            font_style: FontStyle::Normal,
            line_height: LineHeight::Normal,
            text_align: TextAlign::Start,
            text_decoration: TextDecorationLine::default(),
            text_transform: TextTransform::None,
            white_space: WhiteSpace::Normal,
            list_style_type: ListStyleType::Disc,
            vertical_align: VerticalAlign::Baseline,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            justify_content: AlignValue::Normal,
            align_items: AlignValue::Normal,
            align_self: AlignValue::Auto,
            align_content: AlignValue::Normal,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: ComputedSize::Auto,
            row_gap: ComputedLp::ZERO,
            column_gap: ComputedLp::ZERO,
            opacity: 1.0,
            custom: Arc::new(CustomMap::new()),
        }
    }

    /// Style for a text node or anonymous box: inherits from `parent`,
    /// everything else initial.
    pub fn anonymous_from(parent: &ComputedStyle) -> Self {
        let mut s = Self::initial();
        s.inherit_from(parent);
        s
    }

    fn inherit_from(&mut self, p: &ComputedStyle) {
        self.visibility = p.visibility;
        self.color = p.color;
        self.font_family = p.font_family.clone();
        self.font_size = p.font_size;
        self.font_weight = p.font_weight;
        self.font_style = p.font_style;
        self.line_height = p.line_height;
        self.text_align = p.text_align;
        self.text_transform = p.text_transform;
        self.white_space = p.white_space;
        self.list_style_type = p.list_style_type;
        self.custom = p.custom.clone();
        // Not inherited by spec, but the decoration propagates to all
        // descendant text, which is what this achieves for now.
        self.text_decoration = p.text_decoration;
    }

    pub fn is_block_container(&self) -> bool {
        matches!(
            self.display,
            Display::Block | Display::ListItem | Display::InlineBlock
        )
    }

    pub fn is_positioned(&self) -> bool {
        self.position != Position::Static
    }

    pub fn has_visible_border(&self) -> bool {
        self.border_width.top > 0.0
            || self.border_width.right > 0.0
            || self.border_width.bottom > 0.0
            || self.border_width.left > 0.0
    }
}

/// Declared values for one element, indexed by `PropertyId`, plus the
/// element's resolved custom properties.
#[derive(Debug)]
pub struct DeclaredValues<'a> {
    slots: Vec<Option<&'a DeclaredValue>>,
    pub custom: Arc<CustomMap>,
}

impl<'a> DeclaredValues<'a> {
    pub fn new() -> Self {
        Self {
            slots: vec![None; PropertyId::COUNT],
            custom: Arc::new(CustomMap::new()),
        }
    }

    /// Later calls override earlier ones; apply in cascade order. Custom
    /// and pending values are ignored: the cascade resolves those first.
    pub fn set(&mut self, v: &'a DeclaredValue) {
        if let Some(id) = v.id() {
            self.slots[id.index()] = Some(v);
        }
    }

    pub fn get(&self, id: PropertyId) -> Option<&'a DeclaredValue> {
        self.slots[id.index()]
    }
}

impl Default for DeclaredValues<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// How a declared value resolves for one property.
enum Resolved<'a> {
    Specified(&'a PropertyDeclaration),
    Inherit,
    Initial,
}

fn resolve<'a>(decls: &DeclaredValues<'a>, id: PropertyId) -> Resolved<'a> {
    match decls.get(id) {
        Some(DeclaredValue::Value(v)) => Resolved::Specified(v),
        Some(DeclaredValue::Inherit(_)) => Resolved::Inherit,
        Some(DeclaredValue::Initial(_)) => Resolved::Initial,
        // Custom and pending never reach a slot (`set` skips them).
        Some(DeclaredValue::Unset(_) | DeclaredValue::Custom { .. } | DeclaredValue::Pending { .. }) | None => {
            if id.is_inherited() {
                Resolved::Inherit
            } else {
                Resolved::Initial
            }
        }
    }
}

/// Compute the style of an element from its declared values and its parent.
pub fn compute(
    decls: &DeclaredValues<'_>,
    parent: &ComputedStyle,
    root_font_size: Option<f32>,
    viewport: &Viewport,
    is_root: bool,
) -> ComputedStyle {
    let initial = ComputedStyle::initial();
    let mut out = ComputedStyle::initial();
    out.custom = decls.custom.clone();

    // Font size first: em units on everything else depend on it.
    out.font_size = match resolve(decls, PropertyId::FontSize) {
        Resolved::Specified(PropertyDeclaration::FontSize(fs)) => {
            let parent_ctx = LengthContext {
                font_size: parent.font_size,
                root_font_size: root_font_size.unwrap_or(parent.font_size),
                viewport_width: viewport.width,
                viewport_height: viewport.height,
            };
            match *fs {
                FontSize::Length(l) => l.to_px(&parent_ctx),
                FontSize::Percent(p) => parent.font_size * p / 100.0,
                FontSize::Keyword(k) => {
                    const SIZES: [f32; 8] = [9.0, 10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0];
                    SIZES[(k as usize).min(7)]
                }
                FontSize::Smaller => parent.font_size / 1.2,
                FontSize::Larger => parent.font_size * 1.2,
            }
        }
        Resolved::Inherit => parent.font_size,
        _ => initial.font_size,
    }
    .max(0.0);

    let root_font_size = if is_root {
        out.font_size
    } else {
        root_font_size.unwrap_or(out.font_size)
    };
    let ctx = LengthContext {
        font_size: out.font_size,
        root_font_size,
        viewport_width: viewport.width,
        viewport_height: viewport.height,
    };

    macro_rules! pick {
        ($field:ident, $id:ident, $variant:ident, |$v:ident| $convert:expr) => {
            out.$field = match resolve(decls, PropertyId::$id) {
                Resolved::Specified(PropertyDeclaration::$variant($v)) => $convert,
                Resolved::Specified(_) => initial.$field.clone(),
                Resolved::Inherit => parent.$field.clone(),
                Resolved::Initial => initial.$field.clone(),
            };
        };
    }

    let lpa = |v: &LengthPercentageAuto| match *v {
        LengthPercentageAuto::Length(l) => ComputedLpAuto::Px(l.to_px(&ctx)),
        LengthPercentageAuto::Percent(p) => ComputedLpAuto::Percent(p),
        LengthPercentageAuto::Auto => ComputedLpAuto::Auto,
    };
    let lp = |v: &LengthPercentage| match *v {
        LengthPercentage::Length(l) => ComputedLp::Px(l.to_px(&ctx)),
        LengthPercentage::Percent(p) => ComputedLp::Percent(p),
    };
    let size = |v: &SizeValue| match *v {
        SizeValue::Auto => ComputedSize::Auto,
        SizeValue::Length(l) => ComputedSize::Px(l.to_px(&ctx)),
        SizeValue::Percent(p) => ComputedSize::Percent(p),
        SizeValue::MinContent => ComputedSize::MinContent,
        SizeValue::MaxContent => ComputedSize::MaxContent,
        SizeValue::FitContent => ComputedSize::FitContent,
        SizeValue::None => ComputedSize::None,
    };

    pick!(display, Display, Display, |v| *v);
    pick!(position, Position, Position, |v| *v);
    pick!(float, Float, Float, |v| *v);
    pick!(clear, Clear, Clear, |v| *v);
    pick!(box_sizing, BoxSizing, BoxSizing, |v| *v);

    pick!(color, Color, Color, |v| v.resolve(parent.color));

    // Everything below may reference currentcolor, resolved against out.color.
    let current = out.color;

    macro_rules! side {
        ($field:ident . $side:ident, $id:ident, $variant:ident, |$v:ident| $convert:expr) => {
            out.$field.$side = match resolve(decls, PropertyId::$id) {
                Resolved::Specified(PropertyDeclaration::$variant($v)) => $convert,
                Resolved::Specified(_) => initial.$field.$side,
                Resolved::Inherit => parent.$field.$side,
                Resolved::Initial => initial.$field.$side,
            };
        };
    }

    side!(inset.top, Top, Top, |v| lpa(v));
    side!(inset.right, Right, Right, |v| lpa(v));
    side!(inset.bottom, Bottom, Bottom, |v| lpa(v));
    side!(inset.left, Left, Left, |v| lpa(v));
    side!(margin.top, MarginTop, MarginTop, |v| lpa(v));
    side!(margin.right, MarginRight, MarginRight, |v| lpa(v));
    side!(margin.bottom, MarginBottom, MarginBottom, |v| lpa(v));
    side!(margin.left, MarginLeft, MarginLeft, |v| lpa(v));
    side!(padding.top, PaddingTop, PaddingTop, |v| lp(v));
    side!(padding.right, PaddingRight, PaddingRight, |v| lp(v));
    side!(padding.bottom, PaddingBottom, PaddingBottom, |v| lp(v));
    side!(padding.left, PaddingLeft, PaddingLeft, |v| lp(v));

    side!(border_style.top, BorderTopStyle, BorderTopStyle, |v| *v);
    side!(border_style.right, BorderRightStyle, BorderRightStyle, |v| *v);
    side!(border_style.bottom, BorderBottomStyle, BorderBottomStyle, |v| *v);
    side!(border_style.left, BorderLeftStyle, BorderLeftStyle, |v| *v);

    // Border widths: the initial value is `medium` (3px); the computed
    // value is 0 whenever the side's style is none or hidden.
    let bw = |v: &BorderWidth| match *v {
        BorderWidth::Thin => 1.0,
        BorderWidth::Medium => 3.0,
        BorderWidth::Thick => 5.0,
        BorderWidth::Length(l) => l.to_px(&ctx).max(0.0),
    };
    let border_width = |id: PropertyId, parent_width: f32, style: BorderStyle| -> f32 {
        if !style.is_visible() {
            return 0.0;
        }
        match resolve(decls, id) {
            Resolved::Specified(PropertyDeclaration::BorderTopWidth(v))
            | Resolved::Specified(PropertyDeclaration::BorderRightWidth(v))
            | Resolved::Specified(PropertyDeclaration::BorderBottomWidth(v))
            | Resolved::Specified(PropertyDeclaration::BorderLeftWidth(v)) => bw(v),
            Resolved::Specified(_) => 3.0,
            Resolved::Inherit => parent_width,
            Resolved::Initial => 3.0,
        }
    };
    out.border_width.top = border_width(PropertyId::BorderTopWidth, parent.border_width.top, out.border_style.top);
    out.border_width.right = border_width(PropertyId::BorderRightWidth, parent.border_width.right, out.border_style.right);
    out.border_width.bottom = border_width(PropertyId::BorderBottomWidth, parent.border_width.bottom, out.border_style.bottom);
    out.border_width.left = border_width(PropertyId::BorderLeftWidth, parent.border_width.left, out.border_style.left);

    side!(border_color.top, BorderTopColor, BorderTopColor, |v| v.resolve(current));
    side!(border_color.right, BorderRightColor, BorderRightColor, |v| v.resolve(current));
    side!(border_color.bottom, BorderBottomColor, BorderBottomColor, |v| v.resolve(current));
    side!(border_color.left, BorderLeftColor, BorderLeftColor, |v| v.resolve(current));
    // Initial border color is currentcolor.
    for (c, id) in [
        (&mut out.border_color.top, PropertyId::BorderTopColor),
        (&mut out.border_color.right, PropertyId::BorderRightColor),
        (&mut out.border_color.bottom, PropertyId::BorderBottomColor),
        (&mut out.border_color.left, PropertyId::BorderLeftColor),
    ] {
        if matches!(resolve(decls, id), Resolved::Initial) {
            *c = current;
        }
    }

    side!(border_radius.top, BorderTopLeftRadius, BorderTopLeftRadius, |v| lp(v));
    side!(border_radius.right, BorderTopRightRadius, BorderTopRightRadius, |v| lp(v));
    side!(border_radius.bottom, BorderBottomRightRadius, BorderBottomRightRadius, |v| lp(v));
    side!(border_radius.left, BorderBottomLeftRadius, BorderBottomLeftRadius, |v| lp(v));

    pick!(width, Width, Width, |v| size(v));
    pick!(height, Height, Height, |v| size(v));
    pick!(min_width, MinWidth, MinWidth, |v| size(v));
    pick!(min_height, MinHeight, MinHeight, |v| size(v));
    pick!(max_width, MaxWidth, MaxWidth, |v| size(v));
    pick!(max_height, MaxHeight, MaxHeight, |v| size(v));
    pick!(overflow_x, OverflowX, OverflowX, |v| *v);
    pick!(overflow_y, OverflowY, OverflowY, |v| *v);
    pick!(visibility, Visibility, Visibility, |v| *v);
    pick!(background_color, BackgroundColor, BackgroundColor, |v| v.resolve(current));
    pick!(background_image, BackgroundImage, BackgroundImage, |v| v.clone());
    pick!(font_family, FontFamily, FontFamily, |v| v.clone());
    pick!(font_style, FontStyle, FontStyle, |v| *v);

    out.font_weight = match resolve(decls, PropertyId::FontWeight) {
        Resolved::Specified(PropertyDeclaration::FontWeight(w)) => match *w {
            FontWeight::Absolute(n) => n,
            FontWeight::Bolder => match parent.font_weight {
                w if w < 350 => 400,
                w if w < 550 => 700,
                _ => 900,
            },
            FontWeight::Lighter => match parent.font_weight {
                w if w < 550 => 100,
                w if w < 750 => 400,
                _ => 700,
            },
        },
        Resolved::Inherit => parent.font_weight,
        _ => 400,
    };

    out.line_height = match resolve(decls, PropertyId::LineHeight) {
        Resolved::Specified(PropertyDeclaration::LineHeight(lh)) => match *lh {
            LineHeightValue::Normal => LineHeight::Normal,
            LineHeightValue::Number(n) => LineHeight::Number(n),
            LineHeightValue::Length(l) => LineHeight::Px(l.to_px(&ctx)),
            LineHeightValue::Percent(p) => LineHeight::Px(out.font_size * p / 100.0),
        },
        Resolved::Inherit => parent.line_height,
        _ => LineHeight::Normal,
    };

    pick!(text_align, TextAlign, TextAlign, |v| *v);
    pick!(text_decoration, TextDecorationLine, TextDecorationLine, |v| *v);
    // Decoration propagates: union with the parent's.
    out.text_decoration.underline |= parent.text_decoration.underline;
    out.text_decoration.overline |= parent.text_decoration.overline;
    out.text_decoration.line_through |= parent.text_decoration.line_through;
    pick!(text_transform, TextTransform, TextTransform, |v| *v);
    pick!(white_space, WhiteSpace, WhiteSpace, |v| *v);
    pick!(list_style_type, ListStyleType, ListStyleType, |v| *v);
    pick!(vertical_align, VerticalAlign, VerticalAlign, |v| *v);
    pick!(flex_direction, FlexDirection, FlexDirection, |v| *v);
    pick!(flex_wrap, FlexWrap, FlexWrap, |v| *v);
    pick!(justify_content, JustifyContent, JustifyContent, |v| *v);
    pick!(align_items, AlignItems, AlignItems, |v| *v);
    pick!(align_self, AlignSelf, AlignSelf, |v| *v);
    pick!(align_content, AlignContent, AlignContent, |v| *v);
    pick!(flex_grow, FlexGrow, FlexGrow, |v| *v);
    pick!(flex_shrink, FlexShrink, FlexShrink, |v| *v);
    pick!(flex_basis, FlexBasis, FlexBasis, |v| size(v));
    pick!(row_gap, RowGap, RowGap, |v| lp(v));
    pick!(column_gap, ColumnGap, ColumnGap, |v| lp(v));
    pick!(opacity, Opacity, Opacity, |v| *v);

    // Display fixups (CSS Display 3, "blockification").
    if is_root && !out.display.is_none() {
        out.display = blockify(out.display);
    }
    if matches!(out.position, Position::Absolute | Position::Fixed) || out.float != Float::None {
        out.display = blockify(out.display);
    }
    if out.display == Display::None {
        // Nothing else matters, but keep values sane.
        out.float = Float::None;
    }

    out
}

fn blockify(d: Display) -> Display {
    match d {
        Display::Inline | Display::InlineBlock | Display::Contents => Display::Block,
        Display::InlineFlex => Display::Flex,
        other => other,
    }
}
