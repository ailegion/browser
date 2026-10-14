//! Computed style: every longhand resolved to a concrete value, with
//! inheritance applied and lengths in pixels.

use std::sync::Arc;

use crate::custom::CustomMap;
use crate::image::ComputedImage;
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

/// Computed `letter-spacing`: `normal`, or an absolute length.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LetterSpacing {
    #[default]
    Normal,
    Px(f32),
}

impl LetterSpacing {
    /// What layout adds between characters; `normal` adds nothing.
    pub fn to_px(self) -> f32 {
        match self {
            LetterSpacing::Normal => 0.0,
            LetterSpacing::Px(px) => px,
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

/// Computed `vertical-align`: a keyword, or a length; a percentage stays
/// one, of the element's own line height, resolved at layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ComputedVerticalAlign {
    Keyword(VerticalAlign),
    Length(ComputedLp),
}

impl Default for ComputedVerticalAlign {
    fn default() -> Self {
        ComputedVerticalAlign::Keyword(VerticalAlign::Baseline)
    }
}

/// Computed `text-underline-offset`: `auto`, or an absolute length (a
/// percentage is of the element's `1em`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum UnderlineOffset {
    #[default]
    Auto,
    Px(f32),
}

/// The font of a decorating box: what layout needs to find the box's
/// first available font, whose metrics place the box's lines (CSS Text
/// Decoration 3 §2.1: a propagated line sits where the decorating box's
/// font and baseline put it, not the text's).
#[derive(Debug, Clone, PartialEq)]
pub struct DecorationFont {
    pub family: Arc<str>,
    pub size: f32,
    pub weight: u16,
    pub style: FontStyle,
    pub stretch: f32,
}

/// The lines one decorating box draws on the text inside it, with that
/// box's look (what Blink keeps as its applied-decoration list).
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedDecoration {
    pub lines: TextDecorationLine,
    pub style: TextDecorationStyle,
    pub color: Rgba,
    pub thickness: DecorationThickness,
    /// The decorating box's `text-underline-offset` and
    /// `text-underline-position`.
    pub underline_offset: UnderlineOffset,
    pub underline_position: TextUnderlinePosition,
    pub font: DecorationFont,
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
    /// The background layers, one entry per layer for each longhand; the
    /// lists repeat to the image list's length when painted (CSS
    /// Backgrounds 3 §2.3).
    pub background_images: Vec<ComputedImage>,
    pub background_position_x: Vec<PositionOffset>,
    pub background_position_y: Vec<PositionOffset>,
    pub background_size: Vec<ComputedBackgroundSize>,
    pub background_repeat: Vec<BackgroundRepeat>,
    pub background_attachment: Vec<BackgroundAttachment>,
    pub background_origin: Vec<BackgroundBox>,
    pub background_clip: Vec<BackgroundBox>,
    pub background_blend_mode: Vec<BlendMode>,
    pub font_family: Arc<str>,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_style: FontStyle,
    pub font_variant: FontVariant,
    /// `font-stretch` as a percentage.
    pub font_stretch: f32,
    pub line_height: LineHeight,
    pub text_align: TextAlign,
    pub direction: Direction,
    pub unicode_bidi: UnicodeBidi,
    /// The element's own `text-decoration-*` (what `getComputedStyle`
    /// reports).
    pub text_decoration_line: TextDecorationLine,
    pub text_decoration_style: TextDecorationStyle,
    pub text_decoration_color: Rgba,
    pub text_decoration_thickness: DecorationThickness,
    pub text_underline_position: TextUnderlinePosition,
    pub text_underline_offset: UnderlineOffset,
    pub text_decoration_skip_ink: TextDecorationSkipInk,
    /// The decorating boxes whose lines are drawn on this element's text,
    /// outermost first: every ancestor with lines of its own, up to the
    /// nearest atomic inline, float or out-of-flow box (which start
    /// afresh), then this element when it declares lines. Each entry is
    /// drawn with its own box's look and font (CSS Text Decoration 3
    /// §2.1). Shared with the parent when this element adds nothing.
    pub decorations: Arc<[AppliedDecoration]>,
    pub text_transform: TextTransform,
    pub white_space: WhiteSpace,
    pub letter_spacing: LetterSpacing,
    pub word_break: WordBreak,
    pub overflow_wrap: OverflowWrap,
    pub text_overflow: TextOverflow,
    pub box_decoration_break: BoxDecorationBreak,
    pub list_style_type: ListStyleType,
    pub list_style_position: ListStylePosition,
    pub list_style_image: Option<Arc<str>>,
    pub vertical_align: ComputedVerticalAlign,
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
    /// Scroll container properties (roadmap 4a (4s)).
    pub scrollbar_width: ScrollbarWidth,
    pub scrollbar_color: ScrollbarColor,
    pub scrollbar_gutter: ScrollbarGutter,
    pub overscroll_behavior_x: OverscrollBehavior,
    pub overscroll_behavior_y: OverscrollBehavior,
    pub scroll_behavior: ScrollBehavior,
    /// `scroll-margin-*` in px (negative allowed).
    pub scroll_margin: Sides<f32>,
    /// `scroll-padding-*`: `auto`, px, or a percentage of the scrollport.
    pub scroll_padding: Sides<ComputedLpAuto>,
    pub scroll_snap_type: ScrollSnapType,
    pub scroll_snap_align: ScrollSnapAlign,
    pub scroll_snap_stop: ScrollSnapStop,
    pub overflow_anchor: OverflowAnchor,
    /// 4a block 5.
    pub box_shadow: Vec<ComputedBoxShadow>,
    /// Outline width in px (0 when the style is `none`), style, colour,
    /// offset.
    pub outline_width: f32,
    pub outline_style: OutlineStyle,
    pub outline_color: Rgba,
    pub outline_offset: f32,
    pub cursor: CursorValue,
    pub pointer_events: PointerEvents,
    pub user_select: UserSelect,
    /// The used `user-select` (CSS Basic UI 4 §7.1): `auto` is `all` or
    /// `none` under a parent whose used value is that, else `text`; never
    /// `auto`.
    pub user_select_used: UserSelect,
    /// Custom properties in effect, inherited; shared with the parent when
    /// this element declares none.
    pub custom: Arc<CustomMap>,
}

/// The `overflow` the viewport scrolls and clips by (CSS Overflow 3 §3.3):
/// the root element's, or the body's when the root's is `visible` in both
/// axes; `visible` becomes `auto` and `clip` becomes `hidden` on the
/// viewport. `source` is the element the values were taken from, whose
/// own used `overflow` is then `visible` (it does not clip or scroll
/// itself); `None` when no element has a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportOverflow {
    pub x: Overflow,
    pub y: Overflow,
    pub source: Option<browser_dom::NodeId>,
}

impl Default for ViewportOverflow {
    fn default() -> Self {
        Self {
            x: Overflow::Auto,
            y: Overflow::Auto,
            source: None,
        }
    }
}

/// CSS Overflow 3 §3.3: the viewport takes the `overflow-x`/`-y` of the
/// root element when it is rendered; when the root is an HTML `html`
/// element with `visible` in both axes, the viewport takes the values of
/// its first `body` child instead, when that is rendered. Applied to the
/// viewport, `visible` means `auto` and `clip` means `hidden`. The
/// element propagated from behaves as `visible` itself.
pub fn viewport_overflow(doc: &browser_dom::Document, styles: &crate::cascade::StyleMap) -> ViewportOverflow {
    let Some(root) = doc.document_element() else { return ViewportOverflow::default() };
    let Some(root_style) = styles.get(root) else { return ViewportOverflow::default() };
    if root_style.display.is_none() {
        return ViewportOverflow::default();
    }
    let mut source = root;
    let (mut x, mut y) = (root_style.overflow_x, root_style.overflow_y);
    let is_html = doc
        .element(root)
        .is_some_and(|e| e.name.ns == html5ever::ns!(html) && e.name.local == html5ever::local_name!("html"));
    if is_html && x == Overflow::Visible && y == Overflow::Visible
        && let Some(body) = doc.body()
        && let Some(body_style) = styles.get(body)
        && !body_style.display.is_none()
    {
        source = body;
        x = body_style.overflow_x;
        y = body_style.overflow_y;
    }
    let on_viewport = |o: Overflow| match o {
        Overflow::Visible => Overflow::Auto,
        Overflow::Clip => Overflow::Hidden,
        other => other,
    };
    ViewportOverflow {
        x: on_viewport(x),
        y: on_viewport(y),
        source: Some(source),
    }
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
            background_images: vec![ComputedImage::None],
            background_position_x: vec![PositionOffset::default()],
            background_position_y: vec![PositionOffset::default()],
            background_size: vec![ComputedBackgroundSize::default()],
            background_repeat: vec![BackgroundRepeat::default()],
            background_attachment: vec![BackgroundAttachment::Scroll],
            background_origin: vec![BackgroundBox::PaddingBox],
            background_clip: vec![BackgroundBox::BorderBox],
            background_blend_mode: vec![BlendMode::Normal],
            font_family: Arc::from(DEFAULT_FONT_FAMILY),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: 400,
            font_style: FontStyle::Normal,
            font_variant: FontVariant::Normal,
            font_stretch: 100.0,
            line_height: LineHeight::Normal,
            text_align: TextAlign::Start,
            direction: Direction::Ltr,
            unicode_bidi: UnicodeBidi::Normal,
            text_decoration_line: TextDecorationLine::default(),
            text_decoration_style: TextDecorationStyle::Solid,
            text_decoration_color: Rgba::BLACK,
            text_decoration_thickness: DecorationThickness::Auto,
            text_underline_position: TextUnderlinePosition::default(),
            text_underline_offset: UnderlineOffset::Auto,
            text_decoration_skip_ink: TextDecorationSkipInk::Auto,
            decorations: Arc::from(Vec::new()),
            text_transform: TextTransform::None,
            white_space: WhiteSpace::Normal,
            letter_spacing: LetterSpacing::Normal,
            word_break: WordBreak::Normal,
            overflow_wrap: OverflowWrap::Normal,
            text_overflow: TextOverflow::Clip,
            box_decoration_break: BoxDecorationBreak::Slice,
            list_style_type: ListStyleType::Disc,
            list_style_position: ListStylePosition::Outside,
            list_style_image: None,
            vertical_align: ComputedVerticalAlign::default(),
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
            scrollbar_width: ScrollbarWidth::Auto,
            scrollbar_color: ScrollbarColor::Auto,
            scrollbar_gutter: ScrollbarGutter::Auto,
            overscroll_behavior_x: OverscrollBehavior::Auto,
            overscroll_behavior_y: OverscrollBehavior::Auto,
            scroll_behavior: ScrollBehavior::Auto,
            scroll_margin: Sides::all(0.0),
            scroll_padding: Sides::all(ComputedLpAuto::Auto),
            scroll_snap_type: ScrollSnapType::None,
            scroll_snap_align: ScrollSnapAlign::default(),
            scroll_snap_stop: ScrollSnapStop::Normal,
            overflow_anchor: OverflowAnchor::Auto,
            box_shadow: Vec::new(),
            outline_width: 0.0,
            outline_style: OutlineStyle::None,
            outline_color: Rgba::BLACK,
            outline_offset: 0.0,
            cursor: CursorValue {
                images: Vec::new(),
                keyword: CursorKeyword::Auto,
            },
            pointer_events: PointerEvents::Auto,
            user_select: UserSelect::Auto,
            user_select_used: UserSelect::Text,
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
        self.direction = p.direction;
        self.text_transform = p.text_transform;
        self.white_space = p.white_space;
        self.letter_spacing = p.letter_spacing;
        self.word_break = p.word_break;
        self.overflow_wrap = p.overflow_wrap;
        self.list_style_type = p.list_style_type;
        self.list_style_position = p.list_style_position;
        self.list_style_image = p.list_style_image.clone();
        self.text_underline_position = p.text_underline_position;
        self.text_underline_offset = p.text_underline_offset;
        self.text_decoration_skip_ink = p.text_decoration_skip_ink;
        self.scrollbar_color = p.scrollbar_color;
        self.cursor = p.cursor.clone();
        self.pointer_events = p.pointer_events;
        // Not inherited, but `auto` resolves through the parent's used value.
        self.user_select_used = match p.user_select_used {
            UserSelect::All => UserSelect::All,
            UserSelect::None => UserSelect::None,
            _ => UserSelect::Text,
        };
        self.custom = p.custom.clone();
        // Not inherited by spec, but the decorating boxes' lines reach
        // every in-flow descendant's text, anonymous boxes included.
        self.decorations = p.decorations.clone();
    }

    /// The decoration lines in effect on this element's text: the union
    /// of every decorating box's.
    pub fn decoration_lines(&self) -> TextDecorationLine {
        let mut out = TextDecorationLine::default();
        for d in self.decorations.iter() {
            out.underline |= d.lines.underline;
            out.overline |= d.lines.overline;
            out.line_through |= d.lines.line_through;
            out.blink |= d.lines.blink;
        }
        out
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
            || self.direction != other.direction
            || self.unicode_bidi != other.unicode_bidi
            || self.text_transform != other.text_transform
            || self.white_space != other.white_space
            || self.letter_spacing != other.letter_spacing
            || self.word_break != other.word_break
            || self.overflow_wrap != other.overflow_wrap
            || self.text_overflow != other.text_overflow
            || self.box_decoration_break != other.box_decoration_break
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
            // The scrollbar gutter is part of the box; the other scroll
            // properties change scrolling, not layout.
            || self.scrollbar_width != other.scrollbar_width
            || self.scrollbar_gutter != other.scrollbar_gutter
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
pub(crate) fn position_offset(c: &PositionComponent, ctx: &LengthContext) -> PositionOffset {
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
    // `float: inline-start/end` resolves against the element's direction.
    pick!(direction, Direction, Direction, |v| *v);
    pick!(float, Float, Float, |v| v.computed(out.direction));
    pick!(clear, Clear, Clear, |v| *v);
    pick!(unicode_bidi, UnicodeBidi, UnicodeBidi, |v| *v);
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
    // CSS Overflow 3 §3.1: `visible` computes to `auto` and `clip` to
    // `hidden` when the other axis is neither `visible` nor `clip`, since
    // a box cannot clip in one axis and overflow visibly in the other.
    let scrolls = |o: Overflow| !matches!(o, Overflow::Visible | Overflow::Clip);
    let paired = |o: Overflow, other: Overflow| match o {
        Overflow::Visible if scrolls(other) => Overflow::Auto,
        Overflow::Clip if scrolls(other) => Overflow::Hidden,
        o => o,
    };
    let (ox, oy) = (out.overflow_x, out.overflow_y);
    out.overflow_x = paired(ox, oy);
    out.overflow_y = paired(oy, ox);
    pick!(visibility, Visibility, Visibility, |v| *v);
    pick!(background_color, BackgroundColor, BackgroundColor, |v| v.resolve(current));
    pick!(background_images, BackgroundImage, BackgroundImage, |v| v
        .iter()
        .map(|i| crate::image::compute_image(i, &ctx, current))
        .collect());
    pick!(background_position_x, BackgroundPositionX, BackgroundPositionX, |v| v
        .iter()
        .map(|p| position_offset(p, &ctx))
        .collect());
    pick!(background_position_y, BackgroundPositionY, BackgroundPositionY, |v| v
        .iter()
        .map(|p| position_offset(p, &ctx))
        .collect());
    pick!(background_blend_mode, BackgroundBlendMode, BackgroundBlendMode, |v| v.clone());
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
    pick!(text_decoration_line, TextDecorationLine, TextDecorationLine, |v| *v);
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
    pick!(text_underline_position, TextUnderlinePosition, TextUnderlinePosition, |v| *v);
    pick!(text_underline_offset, TextUnderlineOffset, TextUnderlineOffset, |v| match v {
        TextUnderlineOffsetValue::Auto => UnderlineOffset::Auto,
        TextUnderlineOffsetValue::Length(l) => UnderlineOffset::Px(l.to_computed(&ctx).resolve(out.font_size)),
    });
    pick!(text_decoration_skip_ink, TextDecorationSkipInk, TextDecorationSkipInk, |v| *v);
    // The decorating boxes above reach this element's text (CSS Text
    // Decoration 3 §2.1), except across an atomic inline, a float or an
    // out-of-flow box, whose contents start afresh (what Blink's
    // `StopPropagateTextDecorations` lists). An element with lines of its
    // own is a decorating box too and adds its entry after theirs.
    let stops = matches!(out.display, Display::InlineBlock | Display::InlineFlex)
        || out.float != Float::None
        || matches!(out.position, Position::Absolute | Position::Fixed);
    out.decorations = match (stops, out.text_decoration_line.is_none()) {
        (false, true) => parent.decorations.clone(),
        (true, true) => Arc::from(Vec::new()),
        (stops, false) => {
            let mut list = if stops { Vec::new() } else { parent.decorations.to_vec() };
            list.push(AppliedDecoration {
                lines: out.text_decoration_line,
                style: out.text_decoration_style,
                color: out.text_decoration_color,
                thickness: out.text_decoration_thickness,
                underline_offset: out.text_underline_offset,
                underline_position: out.text_underline_position,
                font: DecorationFont {
                    family: out.font_family.clone(),
                    size: out.font_size,
                    weight: out.font_weight,
                    style: out.font_style,
                    stretch: out.font_stretch,
                },
            });
            Arc::from(list)
        }
    };
    pick!(text_transform, TextTransform, TextTransform, |v| *v);
    pick!(white_space, WhiteSpace, WhiteSpace, |v| *v);
    pick!(letter_spacing, LetterSpacing, LetterSpacing, |v| match v {
        LetterSpacingValue::Normal => LetterSpacing::Normal,
        LetterSpacingValue::Length(l) => LetterSpacing::Px(l.to_px(&ctx)),
        LetterSpacingValue::Calc(c) => match LengthPercentage::Calc(c.clone()).to_computed(&ctx) {
            ComputedLp::Px(px) => LetterSpacing::Px(px),
            ComputedLp::Percent(_) => LetterSpacing::Normal,
        },
    });
    pick!(word_break, WordBreak, WordBreak, |v| *v);
    pick!(overflow_wrap, OverflowWrap, OverflowWrap, |v| *v);
    pick!(text_overflow, TextOverflow, TextOverflow, |v| v.clone());
    pick!(box_decoration_break, BoxDecorationBreak, BoxDecorationBreak, |v| *v);
    pick!(list_style_type, ListStyleType, ListStyleType, |v| v.computed());
    pick!(list_style_position, ListStylePosition, ListStylePosition, |v| *v);
    pick!(list_style_image, ListStyleImage, ListStyleImage, |v| v.url());
    pick!(vertical_align, VerticalAlign, VerticalAlign, |v| match v {
        VerticalAlignValue::Keyword(k) => ComputedVerticalAlign::Keyword(*k),
        VerticalAlignValue::Length(l) => ComputedVerticalAlign::Length(lp(l)),
    });
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

    pick!(scrollbar_width, ScrollbarWidth, ScrollbarWidth, |v| *v);
    pick!(scrollbar_color, ScrollbarColor, ScrollbarColor, |v| match v {
        ScrollbarColorValue::Auto => ScrollbarColor::Auto,
        ScrollbarColorValue::Colors { thumb, track } => ScrollbarColor::Colors {
            thumb: thumb.resolve(current),
            track: track.resolve(current),
        },
    });
    pick!(scrollbar_gutter, ScrollbarGutter, ScrollbarGutter, |v| *v);
    pick!(overscroll_behavior_x, OverscrollBehaviorX, OverscrollBehaviorX, |v| *v);
    pick!(overscroll_behavior_y, OverscrollBehaviorY, OverscrollBehaviorY, |v| *v);
    pick!(scroll_behavior, ScrollBehavior, ScrollBehavior, |v| *v);
    side!(scroll_margin.top, ScrollMarginTop, ScrollMarginTop, |v| v.to_px(&ctx));
    side!(scroll_margin.right, ScrollMarginRight, ScrollMarginRight, |v| v.to_px(&ctx));
    side!(scroll_margin.bottom, ScrollMarginBottom, ScrollMarginBottom, |v| v.to_px(&ctx));
    side!(scroll_margin.left, ScrollMarginLeft, ScrollMarginLeft, |v| v.to_px(&ctx));
    side!(scroll_padding.top, ScrollPaddingTop, ScrollPaddingTop, |v| lpa(v));
    side!(scroll_padding.right, ScrollPaddingRight, ScrollPaddingRight, |v| lpa(v));
    side!(scroll_padding.bottom, ScrollPaddingBottom, ScrollPaddingBottom, |v| lpa(v));
    side!(scroll_padding.left, ScrollPaddingLeft, ScrollPaddingLeft, |v| lpa(v));
    pick!(scroll_snap_type, ScrollSnapType, ScrollSnapType, |v| *v);
    pick!(scroll_snap_align, ScrollSnapAlign, ScrollSnapAlign, |v| *v);
    pick!(scroll_snap_stop, ScrollSnapStop, ScrollSnapStop, |v| *v);
    pick!(overflow_anchor, OverflowAnchor, OverflowAnchor, |v| *v);

    pick!(box_shadow, BoxShadow, BoxShadow, |v| v
        .iter()
        .map(|s| ComputedBoxShadow {
            inset: s.inset,
            x: s.x.to_px(&ctx),
            y: s.y.to_px(&ctx),
            blur: s.blur.map_or(0.0, |b| b.to_px(&ctx).max(0.0)),
            spread: s.spread.map_or(0.0, |b| b.to_px(&ctx)),
            color: s.color.map_or(current, |c| c.resolve(current)),
        })
        .collect());
    pick!(outline_style, OutlineStyle, OutlineStyle, |v| *v);
    // The outline width is 0 when the style is `none`, as a border's is.
    out.outline_width = if out.outline_style.is_visible() {
        match resolve(decls, PropertyId::OutlineWidth) {
            Resolved::Specified(PropertyDeclaration::OutlineWidth(w)) => bw(w),
            Resolved::Inherit => parent.outline_width,
            _ => 3.0,
        }
    } else {
        0.0
    };
    pick!(outline_color, OutlineColor, OutlineColor, |v| v.resolve(current));
    if matches!(resolve(decls, PropertyId::OutlineColor), Resolved::Initial) {
        out.outline_color = current;
    }
    pick!(outline_offset, OutlineOffset, OutlineOffset, |v| v.to_px(&ctx));
    pick!(cursor, Cursor, Cursor, |v| v.clone());
    pick!(pointer_events, PointerEvents, PointerEvents, |v| *v);
    pick!(user_select, UserSelect, UserSelect, |v| *v);
    out.user_select_used = match out.user_select {
        UserSelect::Auto => match parent.user_select_used {
            UserSelect::All => UserSelect::All,
            UserSelect::None => UserSelect::None,
            _ => UserSelect::Text,
        },
        v => v,
    };

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
