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

/// Computed `text-decoration-thickness`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum DecorationThickness {
    #[default]
    Auto,
    FromFont,
    Length(ComputedLp),
}

/// One axis of a computed `background-position`: a percentage of the
/// box plus a pixel offset (`right 10px` is 100% and -10px).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PositionOffset {
    pub percent: f32,
    pub px: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ComputedPosition {
    pub x: PositionOffset,
    pub y: PositionOffset,
}

/// Computed `background-size`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ComputedBackgroundSize {
    Cover,
    Contain,
    Explicit(ComputedLpAuto, ComputedLpAuto),
}

impl Default for ComputedBackgroundSize {
    fn default() -> Self {
        ComputedBackgroundSize::Explicit(ComputedLpAuto::Auto, ComputedLpAuto::Auto)
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
    /// The image the painter draws: the last layer with a `url()` (one
    /// layer is painted until roadmap item 4a paints them all).
    pub background_image: Option<Arc<str>>,
    /// The background layers, one entry per layer for each longhand
    /// (`getComputedStyle` reports them all).
    pub background_images: Vec<ImageValue>,
    pub background_position: Vec<ComputedPosition>,
    pub background_size: Vec<ComputedBackgroundSize>,
    pub background_repeat: Vec<BackgroundRepeat>,
    pub background_attachment: Vec<BackgroundAttachment>,
    pub background_origin: Vec<BackgroundBox>,
    pub background_clip: Vec<BackgroundBox>,
    pub font_family: Arc<str>,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_style: FontStyle,
    pub font_variant: FontVariant,
    /// `font-stretch` as a percentage.
    pub font_stretch: f32,
    pub line_height: LineHeight,
    pub text_align: TextAlign,
    /// The decoration lines in effect on this element's text: its own
    /// and those propagated from its ancestors.
    pub text_decoration: TextDecorationLine,
    /// The element's own `text-decoration-line` (what `getComputedStyle`
    /// reports).
    pub text_decoration_line: TextDecorationLine,
    pub text_decoration_style: TextDecorationStyle,
    pub text_decoration_color: Rgba,
    pub text_decoration_thickness: DecorationThickness,
    pub text_transform: TextTransform,
    pub white_space: WhiteSpace,
    pub list_style_type: ListStyleType,
    pub list_style_position: ListStylePosition,
    pub list_style_image: Option<Arc<str>>,
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
            background_images: vec![ImageValue::None],
            background_position: vec![ComputedPosition::default()],
            background_size: vec![ComputedBackgroundSize::default()],
            background_repeat: vec![BackgroundRepeat::default()],
            background_attachment: vec![BackgroundAttachment::Scroll],
            background_origin: vec![BackgroundBox::PaddingBox],
            background_clip: vec![BackgroundBox::BorderBox],
            font_family: Arc::from(DEFAULT_FONT_FAMILY),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: 400,
            font_style: FontStyle::Normal,
            font_variant: FontVariant::Normal,
            font_stretch: 100.0,
            line_height: LineHeight::Normal,
            text_align: TextAlign::Start,
            text_decoration: TextDecorationLine::default(),
            text_decoration_line: TextDecorationLine::default(),
            text_decoration_style: TextDecorationStyle::Solid,
            text_decoration_color: Rgba::BLACK,
            text_decoration_thickness: DecorationThickness::Auto,
            text_transform: TextTransform::None,
            white_space: WhiteSpace::Normal,
            list_style_type: ListStyleType::Disc,
            list_style_position: ListStylePosition::Outside,
            list_style_image: None,
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
        self.font_variant = p.font_variant;
        self.font_stretch = p.font_stretch;
        self.line_height = p.line_height;
        self.text_align = p.text_align;
        self.text_transform = p.text_transform;
        self.white_space = p.white_space;
        self.list_style_type = p.list_style_type;
        self.list_style_position = p.list_style_position;
        self.list_style_image = p.list_style_image.clone();
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

    /// Whether going from `self` to `other` changes anything layout
    /// reads: box, size, font, text flow, flex and gap properties. A
    /// change in colours, backgrounds, decorations, opacity, visibility
    /// or custom properties alone is paint-only, as browsers classify it
    /// (repaint without reflow).
    pub fn layout_differs(&self, other: &ComputedStyle) -> bool {
        self.display != other.display
            || self.position != other.position
            || self.float != other.float
            || self.clear != other.clear
            || self.box_sizing != other.box_sizing
            || self.inset != other.inset
            || self.margin != other.margin
            || self.padding != other.padding
            || self.border_width != other.border_width
            || self.width != other.width
            || self.height != other.height
            || self.min_width != other.min_width
            || self.min_height != other.min_height
            || self.max_width != other.max_width
            || self.max_height != other.max_height
            || self.overflow_x != other.overflow_x
            || self.overflow_y != other.overflow_y
            || self.font_family != other.font_family
            || self.font_size != other.font_size
            || self.font_weight != other.font_weight
            || self.font_style != other.font_style
            || self.font_variant != other.font_variant
            || self.font_stretch != other.font_stretch
            || self.line_height != other.line_height
            || self.text_align != other.text_align
            || self.text_transform != other.text_transform
            || self.white_space != other.white_space
            || self.list_style_type != other.list_style_type
            || self.list_style_position != other.list_style_position
            || self.list_style_image != other.list_style_image
            || self.vertical_align != other.vertical_align
            || self.flex_direction != other.flex_direction
            || self.flex_wrap != other.flex_wrap
            || self.justify_content != other.justify_content
            || self.align_items != other.align_items
            || self.align_self != other.align_self
            || self.align_content != other.align_content
            || self.flex_grow != other.flex_grow
            || self.flex_shrink != other.flex_shrink
            || self.flex_basis != other.flex_basis
            || self.row_gap != other.row_gap
            || self.column_gap != other.column_gap
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

    /// Forget the value of `id` (it resolves as `unset`).
    pub fn clear(&mut self, id: PropertyId) {
        self.slots[id.index()] = None;
    }

    /// Take `id`'s value from `other` (none if it has none).
    pub fn copy_from(&mut self, other: &DeclaredValues<'a>, id: PropertyId) {
        self.slots[id.index()] = other.slots[id.index()];
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
        // Revert is replaced by the cascade; an unreplaced one is unset.
        Some(
            DeclaredValue::Unset(_)
            | DeclaredValue::Revert(_)
            | DeclaredValue::RevertLayer(_)
            | DeclaredValue::Custom { .. }
            | DeclaredValue::Pending { .. },
        )
        | None => {
            if id.is_inherited() {
                Resolved::Inherit
            } else {
                Resolved::Initial
            }
        }
    }
}

/// A `background-position` axis as computed: keywords become
/// percentages, an offset from the end edge is subtracted.
fn position_offset(c: &PositionComponent, ctx: &LengthContext) -> PositionOffset {
    let lp = |v: &LengthPercentage| match v.to_computed(ctx) {
        ComputedLp::Px(px) => PositionOffset { percent: 0.0, px },
        ComputedLp::Percent(percent) => PositionOffset { percent, px: 0.0 },
    };
    match c {
        PositionComponent::Length(v) => lp(v),
        PositionComponent::Keyword(edge, offset) => {
            let off = offset.as_ref().map(lp).unwrap_or_default();
            match edge {
                PositionEdge::Start => off,
                PositionEdge::Center => PositionOffset {
                    percent: 50.0 + off.percent,
                    px: off.px,
                },
                PositionEdge::End => PositionOffset {
                    percent: 100.0 - off.percent,
                    px: -off.px,
                },
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
            match fs {
                FontSize::Length(l) => l.to_px(&parent_ctx),
                FontSize::Percent(p) => parent.font_size * p / 100.0,
                FontSize::Calc(c) => match LengthPercentage::Calc(c.clone()).to_computed(&parent_ctx) {
                    ComputedLp::Px(px) => px,
                    ComputedLp::Percent(p) => parent.font_size * p / 100.0,
                },
                FontSize::Keyword(k) => {
                    const SIZES: [f32; 8] = [9.0, 10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0];
                    SIZES[(*k as usize).min(7)]
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

    let lpa = |v: &LengthPercentageAuto| v.to_computed(&ctx);
    let lp = |v: &LengthPercentage| v.to_computed(&ctx);
    let size = |v: &SizeValue| v.to_computed(&ctx);

    pick!(display, Display, Display, |v| v.computed());
    pick!(position, Position, Position, |v| *v);
    pick!(float, Float, Float, |v| v.computed());
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

    side!(border_radius.top, BorderTopLeftRadius, BorderTopLeftRadius, |v| lp(&v.x));
    side!(border_radius.right, BorderTopRightRadius, BorderTopRightRadius, |v| lp(&v.x));
    side!(border_radius.bottom, BorderBottomRightRadius, BorderBottomRightRadius, |v| lp(&v.x));
    side!(border_radius.left, BorderBottomLeftRadius, BorderBottomLeftRadius, |v| lp(&v.x));

    pick!(width, Width, Width, |v| size(v));
    pick!(height, Height, Height, |v| size(v));
    pick!(min_width, MinWidth, MinWidth, |v| size(v));
    pick!(min_height, MinHeight, MinHeight, |v| size(v));
    pick!(max_width, MaxWidth, MaxWidth, |v| size(v));
    pick!(max_height, MaxHeight, MaxHeight, |v| size(v));
    pick!(overflow_x, OverflowX, OverflowX, |v| v.computed());
    pick!(overflow_y, OverflowY, OverflowY, |v| v.computed());
    pick!(visibility, Visibility, Visibility, |v| *v);
    pick!(background_color, BackgroundColor, BackgroundColor, |v| v.resolve(current));
    pick!(background_images, BackgroundImage, BackgroundImage, |v| v.clone());
    // The painted image: the last layer with a URL.
    out.background_image = out.background_images.iter().rev().find_map(ImageValue::url);
    pick!(background_position, BackgroundPosition, BackgroundPosition, |v| v
        .iter()
        .map(|p| ComputedPosition {
            x: position_offset(&p.x, &ctx),
            y: position_offset(&p.y, &ctx),
        })
        .collect());
    pick!(background_size, BackgroundSize, BackgroundSize, |v| v
        .iter()
        .map(|s| match s {
            BackgroundSize::Cover => ComputedBackgroundSize::Cover,
            BackgroundSize::Contain => ComputedBackgroundSize::Contain,
            BackgroundSize::Explicit(x, y) => {
                let one = |v: &Option<LengthPercentage>| match v {
                    None => ComputedLpAuto::Auto,
                    Some(v) => match lp(v) {
                        ComputedLp::Px(px) => ComputedLpAuto::Px(px),
                        ComputedLp::Percent(p) => ComputedLpAuto::Percent(p),
                    },
                };
                ComputedBackgroundSize::Explicit(one(x), one(y))
            }
        })
        .collect());
    pick!(background_repeat, BackgroundRepeat, BackgroundRepeat, |v| v.clone());
    pick!(background_attachment, BackgroundAttachment, BackgroundAttachment, |v| v.clone());
    pick!(background_origin, BackgroundOrigin, BackgroundOrigin, |v| v.clone());
    pick!(background_clip, BackgroundClip, BackgroundClip, |v| v.clone());
    pick!(font_family, FontFamily, FontFamily, |v| font_family_computed(v));
    pick!(font_style, FontStyle, FontStyle, |v| v.computed());
    pick!(font_variant, FontVariant, FontVariant, |v| *v);
    pick!(font_stretch, FontStretch, FontStretch, |v| v.percent());

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
        Resolved::Specified(PropertyDeclaration::LineHeight(lh)) => match lh {
            LineHeightValue::Normal => LineHeight::Normal,
            LineHeightValue::Number(n) => LineHeight::Number(*n),
            LineHeightValue::Length(l) => LineHeight::Px(l.to_px(&ctx)),
            LineHeightValue::Percent(p) => LineHeight::Px(out.font_size * p / 100.0),
            LineHeightValue::Calc(c) => match LengthPercentage::Calc(c.clone()).to_computed(&ctx) {
                ComputedLp::Px(px) => LineHeight::Px(px),
                ComputedLp::Percent(p) => LineHeight::Px(out.font_size * p / 100.0),
            },
        },
        Resolved::Inherit => parent.line_height,
        _ => LineHeight::Normal,
    };

    pick!(text_align, TextAlign, TextAlign, |v| v.computed());
    pick!(text_decoration, TextDecorationLine, TextDecorationLine, |v| *v);
    out.text_decoration_line = out.text_decoration;
    // Decoration propagates: union with the parent's.
    out.text_decoration.underline |= parent.text_decoration.underline;
    out.text_decoration.overline |= parent.text_decoration.overline;
    out.text_decoration.line_through |= parent.text_decoration.line_through;
    pick!(text_decoration_style, TextDecorationStyle, TextDecorationStyle, |v| *v);
    pick!(text_decoration_color, TextDecorationColor, TextDecorationColor, |v| v.resolve(current));
    if matches!(resolve(decls, PropertyId::TextDecorationColor), Resolved::Initial) {
        out.text_decoration_color = current;
    }
    pick!(text_decoration_thickness, TextDecorationThickness, TextDecorationThickness, |v| match v {
        TextDecorationThickness::Auto => DecorationThickness::Auto,
        TextDecorationThickness::FromFont => DecorationThickness::FromFont,
        TextDecorationThickness::Length(l) => DecorationThickness::Length(lp(l)),
    });
    pick!(text_transform, TextTransform, TextTransform, |v| *v);
    pick!(white_space, WhiteSpace, WhiteSpace, |v| *v);
    pick!(list_style_type, ListStyleType, ListStyleType, |v| v.computed());
    pick!(list_style_position, ListStylePosition, ListStylePosition, |v| *v);
    pick!(list_style_image, ListStyleImage, ListStyleImage, |v| v.url());
    pick!(vertical_align, VerticalAlign, VerticalAlign, |v| v.computed());
    pick!(flex_direction, FlexDirection, FlexDirection, |v| *v);
    pick!(flex_wrap, FlexWrap, FlexWrap, |v| *v);
    pick!(justify_content, JustifyContent, JustifyContent, |v| v.keyword.computed());
    pick!(align_items, AlignItems, AlignItems, |v| v.keyword.computed());
    pick!(align_self, AlignSelf, AlignSelf, |v| v.keyword.computed());
    pick!(align_content, AlignContent, AlignContent, |v| v.keyword.computed());
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
