//! Image values beyond `url()`: gradients (CSS Images 3 §3, CSS Images 4
//! §3 for conic gradients and double-position stops) and `image-set()`
//! (CSS Images 4 §2.5). Specified values keep what was written, for the
//! CSSOM; computed values have colours resolved and lengths in pixels,
//! for painting and `getComputedStyle`.

use std::sync::Arc;

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;
use crate::computed::ComputedPosition;
use crate::properties::{BackgroundPosition, ImageValue, PositionEdge, css_url, parse_background_position};
use crate::values::*;

// ----- specified -----

/// A gradient as written.
#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    pub shape: GradientShape,
    pub repeating: bool,
    /// Colour stops and hints, in order; at least two stops, the first
    /// and last items stops, hints never adjacent.
    pub items: Vec<GradientItem>,
    /// `-webkit-` or `-moz-`: the legacy syntax, serialized in it.
    pub prefix: Option<&'static str>,
    /// `in <color-space> [<hue-interpolation-method>]` (CSS Color 4 §12):
    /// where the colours between stops are interpolated; `None` is
    /// premultiplied sRGB, what browsers use for these colour syntaxes.
    pub interpolation: Option<ColorInterpolation>,
}

/// `<color-interpolation-method>`: `in <rectangular-color-space> | in
/// <polar-color-space> <hue-interpolation-method>?` (CSS Color 4 §12.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInterpolation {
    pub space: InterpolationSpace,
    /// Only for the polar spaces; `shorter` by default.
    pub hue: HueMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpolationSpace {
    Srgb,
    SrgbLinear,
    DisplayP3,
    A98Rgb,
    ProphotoRgb,
    Rec2020,
    Lab,
    Oklab,
    Xyz,
    XyzD50,
    XyzD65,
    Hsl,
    Hwb,
    Lch,
    Oklch,
}

impl InterpolationSpace {
    pub fn keyword(self) -> &'static str {
        match self {
            InterpolationSpace::Srgb => "srgb",
            InterpolationSpace::SrgbLinear => "srgb-linear",
            InterpolationSpace::DisplayP3 => "display-p3",
            InterpolationSpace::A98Rgb => "a98-rgb",
            InterpolationSpace::ProphotoRgb => "prophoto-rgb",
            InterpolationSpace::Rec2020 => "rec2020",
            InterpolationSpace::Lab => "lab",
            InterpolationSpace::Oklab => "oklab",
            InterpolationSpace::Xyz => "xyz",
            InterpolationSpace::XyzD50 => "xyz-d50",
            InterpolationSpace::XyzD65 => "xyz-d65",
            InterpolationSpace::Hsl => "hsl",
            InterpolationSpace::Hwb => "hwb",
            InterpolationSpace::Lch => "lch",
            InterpolationSpace::Oklch => "oklch",
        }
    }

    /// Whether the space has a hue angle (the hue method applies).
    pub fn is_polar(self) -> bool {
        matches!(
            self,
            InterpolationSpace::Hsl | InterpolationSpace::Hwb | InterpolationSpace::Lch | InterpolationSpace::Oklch
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HueMethod {
    #[default]
    Shorter,
    Longer,
    Increasing,
    Decreasing,
}

impl HueMethod {
    pub fn keyword(self) -> &'static str {
        match self {
            HueMethod::Shorter => "shorter",
            HueMethod::Longer => "longer",
            HueMethod::Increasing => "increasing",
            HueMethod::Decreasing => "decreasing",
        }
    }
}

impl ColorInterpolation {
    /// `in oklch longer hue`; the default `shorter hue` is left out.
    pub fn to_css(self) -> String {
        if self.space.is_polar() && self.hue != HueMethod::Shorter {
            format!("in {} {} hue", self.space.keyword(), self.hue.keyword())
        } else {
            format!("in {}", self.space.keyword())
        }
    }
}

/// A gradient's `at <position>` as computed: the keywords stay (the
/// computed value of a gradient keeps them, CSS Images 3 §3.1.3), lengths
/// become pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComputedGradientPosition {
    pub x: ComputedPositionComponent,
    pub y: ComputedPositionComponent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ComputedPositionComponent {
    Length(ComputedLp),
    Keyword(PositionEdge, Option<ComputedLp>),
}

impl ComputedPositionComponent {
    /// As a percentage of the box plus pixels, for painting.
    pub fn to_offset(self) -> crate::computed::PositionOffset {
        use crate::computed::PositionOffset;
        match self {
            ComputedPositionComponent::Length(v) => PositionOffset::from_lp(v),
            ComputedPositionComponent::Keyword(edge, offset) => {
                PositionOffset::from_edge(edge, offset.map(PositionOffset::from_lp).unwrap_or_default())
            }
        }
    }

    fn to_css(self, horizontal: bool) -> String {
        match self {
            ComputedPositionComponent::Length(v) => computed_lp_css(v),
            ComputedPositionComponent::Keyword(edge, offset) => {
                let kw = match (edge, horizontal) {
                    (PositionEdge::Start, true) => "left",
                    (PositionEdge::Start, false) => "top",
                    (PositionEdge::Center, _) => "center",
                    (PositionEdge::End, true) => "right",
                    (PositionEdge::End, false) => "bottom",
                };
                match offset {
                    Some(o) => format!("{kw} {}", computed_lp_css(o)),
                    None => kw.to_owned(),
                }
            }
        }
    }
}

impl ComputedGradientPosition {
    pub fn to_offsets(&self) -> ComputedPosition {
        ComputedPosition {
            x: self.x.to_offset(),
            y: self.y.to_offset(),
        }
    }

    pub fn to_css(&self) -> String {
        format!("{} {}", self.x.to_css(true), self.y.to_css(false))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GradientShape {
    Linear(LinearDirection),
    Radial {
        shape: Option<RadialShape>,
        size: Option<RadialSize>,
        position: Option<BackgroundPosition>,
    },
    Conic {
        /// `from <angle>`, in degrees.
        from: Option<f32>,
        position: Option<BackgroundPosition>,
    },
}

/// Where a linear gradient line points (the standard syntax's meaning:
/// the side or corner it goes *to*).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinearDirection {
    /// Nothing given: `to bottom`.
    Default,
    /// Degrees, `0deg` up and clockwise.
    Angle(f32),
    /// `to <side-or-corner>`: `Start` is left or top, `End` right or
    /// bottom; one of the two may be absent.
    Corner {
        horizontal: Option<PositionEdge>,
        vertical: Option<PositionEdge>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadialShape {
    Circle,
    Ellipse,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RadialSize {
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
    /// A circle's radius (a length, not a percentage).
    Circle(LengthPercentage),
    /// An ellipse's radii.
    Ellipse(LengthPercentage, LengthPercentage),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GradientItem {
    /// A colour at zero, one or two positions (two make two stops).
    Stop {
        color: Color,
        positions: Vec<GradientPos>,
    },
    /// A colour hint: where the midpoint between two stops lies.
    Hint(GradientPos),
}

/// A position along a gradient: a length or percentage of the gradient
/// line, or an angle (conic gradients).
#[derive(Debug, Clone, PartialEq)]
pub enum GradientPos {
    Lp(LengthPercentage),
    Angle(f32),
}

/// One `image-set()` candidate: an image for a device resolution (dots
/// per CSS pixel). Candidates with a `type()` this engine cannot decode
/// are dropped at parse time, as browsers drop them.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageSetCandidate {
    pub image: ImageValue,
    pub resolution: f32,
}

// ----- computed -----

/// An image as computed: colours resolved, lengths in pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum ComputedImage {
    None,
    Url(Arc<str>),
    Gradient(Arc<ComputedGradient>),
    ImageSet(Arc<[ComputedCandidate]>),
    /// An image function this engine does not paint (`cross-fade()`,
    /// `element()`, `paint()`), kept as written.
    Function(Arc<str>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComputedCandidate {
    pub image: ComputedImage,
    pub resolution: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComputedGradient {
    pub shape: ComputedGradientShape,
    pub repeating: bool,
    pub items: Vec<ComputedGradientItem>,
    pub prefix: Option<&'static str>,
    pub interpolation: Option<ColorInterpolation>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComputedGradientShape {
    Linear(LinearDirection),
    Radial {
        shape: Option<RadialShape>,
        size: Option<ComputedRadialSize>,
        position: Option<ComputedGradientPosition>,
    },
    Conic {
        from: Option<f32>,
        position: Option<ComputedGradientPosition>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ComputedRadialSize {
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
    Circle(ComputedLp),
    Ellipse(ComputedLp, ComputedLp),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComputedGradientItem {
    Stop {
        color: Rgba,
        positions: Vec<ComputedGradientPos>,
    },
    Hint(ComputedGradientPos),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ComputedGradientPos {
    Lp(ComputedLp),
    Angle(f32),
}

impl ComputedImage {
    /// Whether the layer paints something.
    pub fn is_none(&self) -> bool {
        matches!(self, ComputedImage::None)
    }

    /// The image an `image-set()` chooses for a device of `scale` dots per
    /// CSS pixel: the candidate with the smallest resolution not below the
    /// scale, else the largest (as Chrome chooses); any other image is
    /// itself.
    pub fn chosen(&self, scale: f32) -> &ComputedImage {
        match self {
            ComputedImage::ImageSet(candidates) => {
                let best = candidates
                    .iter()
                    .filter(|c| c.resolution >= scale - 1e-6)
                    .min_by(|a, b| a.resolution.total_cmp(&b.resolution))
                    .or_else(|| candidates.iter().max_by(|a, b| a.resolution.total_cmp(&b.resolution)));
                match best {
                    Some(c) => c.image.chosen(scale),
                    None => &ComputedImage::None,
                }
            }
            other => other,
        }
    }

    /// The URL to fetch for this layer on a device of `scale`, if any.
    pub fn url_to_fetch(&self, scale: f32) -> Option<&Arc<str>> {
        match self.chosen(scale) {
            ComputedImage::Url(u) => Some(u),
            _ => None,
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            ComputedImage::None => "none".to_owned(),
            ComputedImage::Url(u) => css_url(u),
            ComputedImage::Gradient(g) => g.to_css(),
            ComputedImage::ImageSet(c) => format!(
                "image-set({})",
                c.iter()
                    .map(|c| format!("{} {}x", c.image.to_css(), css_number(c.resolution)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ComputedImage::Function(f) => f.to_string(),
        }
    }
}

// ----- computation -----

/// Compute an image value: colours against `current`, lengths in `ctx`.
pub(crate) fn compute_image(v: &ImageValue, ctx: &LengthContext, current: Rgba) -> ComputedImage {
    match v {
        ImageValue::None => ComputedImage::None,
        ImageValue::Url(u) => ComputedImage::Url(u.clone()),
        ImageValue::Function(f) => ComputedImage::Function(f.clone()),
        ImageValue::ImageSet(candidates) => ComputedImage::ImageSet(
            candidates
                .iter()
                .map(|c| ComputedCandidate {
                    image: compute_image(&c.image, ctx, current),
                    resolution: c.resolution,
                })
                .collect(),
        ),
        ImageValue::Gradient(g) => {
            let lp = |v: &LengthPercentage| v.to_computed(ctx);
            let pos = |p: &GradientPos| match p {
                GradientPos::Lp(v) => ComputedGradientPos::Lp(lp(v)),
                GradientPos::Angle(a) => ComputedGradientPos::Angle(*a),
            };
            let component = |c: &crate::properties::PositionComponent| match c {
                crate::properties::PositionComponent::Length(v) => ComputedPositionComponent::Length(lp(v)),
                crate::properties::PositionComponent::Keyword(edge, offset) => {
                    ComputedPositionComponent::Keyword(*edge, offset.as_ref().map(lp))
                }
            };
            let position = |p: &Option<BackgroundPosition>| {
                p.as_ref().map(|p| ComputedGradientPosition {
                    x: component(&p.x),
                    y: component(&p.y),
                })
            };
            let shape = match &g.shape {
                GradientShape::Linear(d) => ComputedGradientShape::Linear(*d),
                GradientShape::Radial {
                    shape,
                    size,
                    position: p,
                } => ComputedGradientShape::Radial {
                    shape: *shape,
                    size: size.as_ref().map(|s| match s {
                        RadialSize::ClosestSide => ComputedRadialSize::ClosestSide,
                        RadialSize::FarthestSide => ComputedRadialSize::FarthestSide,
                        RadialSize::ClosestCorner => ComputedRadialSize::ClosestCorner,
                        RadialSize::FarthestCorner => ComputedRadialSize::FarthestCorner,
                        RadialSize::Circle(r) => ComputedRadialSize::Circle(lp(r)),
                        RadialSize::Ellipse(x, y) => ComputedRadialSize::Ellipse(lp(x), lp(y)),
                    }),
                    position: position(p),
                },
                GradientShape::Conic { from, position: p } => ComputedGradientShape::Conic {
                    from: *from,
                    position: position(p),
                },
            };
            let items = g
                .items
                .iter()
                .map(|item| match item {
                    GradientItem::Stop { color, positions } => ComputedGradientItem::Stop {
                        color: color.resolve(current),
                        positions: positions.iter().map(pos).collect(),
                    },
                    GradientItem::Hint(p) => ComputedGradientItem::Hint(pos(p)),
                })
                .collect();
            ComputedImage::Gradient(Arc::new(ComputedGradient {
                shape,
                repeating: g.repeating,
                items,
                prefix: g.prefix,
                interpolation: g.interpolation,
            }))
        }
    }
}

// ----- serialization -----

fn side_words(horizontal: Option<PositionEdge>, vertical: Option<PositionEdge>) -> String {
    let mut words = Vec::new();
    if let Some(h) = horizontal {
        words.push(if h == PositionEdge::Start { "left" } else { "right" });
    }
    if let Some(v) = vertical {
        words.push(if v == PositionEdge::Start { "top" } else { "bottom" });
    }
    words.join(" ")
}

/// The opposite sides: the legacy syntax names where the line starts.
fn legacy_side_words(horizontal: Option<PositionEdge>, vertical: Option<PositionEdge>) -> String {
    let flip = |e: Option<PositionEdge>| {
        e.map(|e| match e {
            PositionEdge::Start => PositionEdge::End,
            PositionEdge::End => PositionEdge::Start,
            PositionEdge::Center => PositionEdge::Center,
        })
    };
    side_words(flip(horizontal), flip(vertical))
}

fn angle_css(deg: f32) -> String {
    format!("{}deg", css_number(deg))
}

/// The parts before the colour stops, joined for the function's head;
/// shared by the specified and computed forms through the closures that
/// print a position and a size. The interpolation method comes last in
/// the head, as Chrome writes it.
fn gradient_head(
    prefix: Option<&'static str>,
    direction: Option<LinearDirection>,
    radial: Option<(Option<RadialShape>, Option<String>, Option<String>)>,
    conic: Option<(Option<f32>, Option<String>)>,
    interpolation: Option<ColorInterpolation>,
) -> Vec<String> {
    let mut head = gradient_head_parts(prefix, direction, radial, conic);
    if let Some(i) = interpolation {
        match head.last_mut() {
            Some(last) => {
                last.push(' ');
                last.push_str(&i.to_css());
            }
            None => head.push(i.to_css()),
        }
    }
    head
}

fn gradient_head_parts(
    prefix: Option<&'static str>,
    direction: Option<LinearDirection>,
    radial: Option<(Option<RadialShape>, Option<String>, Option<String>)>,
    conic: Option<(Option<f32>, Option<String>)>,
) -> Vec<String> {
    let mut head = Vec::new();
    if let Some(d) = direction {
        match d {
            LinearDirection::Default => {}
            LinearDirection::Angle(a) => {
                if prefix.is_some() {
                    head.push(angle_css(90.0 - a));
                } else {
                    head.push(angle_css(a));
                }
            }
            LinearDirection::Corner {
                horizontal,
                vertical,
            } => {
                if prefix.is_some() {
                    head.push(legacy_side_words(horizontal, vertical));
                } else if horizontal.is_none() && vertical == Some(PositionEdge::End) {
                    // `to bottom` is the default and is left out.
                } else {
                    head.push(format!("to {}", side_words(horizontal, vertical)));
                }
            }
        }
    }
    if let Some((shape, size, position)) = radial {
        let mut parts = Vec::new();
        if prefix.is_some() {
            // Legacy: the position first, without `at`.
            if let Some(p) = &position {
                head.push(p.clone());
            }
        }
        if let Some(s) = shape {
            parts.push(if s == RadialShape::Circle { "circle".to_owned() } else { "ellipse".to_owned() });
        }
        if let Some(s) = size {
            parts.push(s);
        }
        if prefix.is_none()
            && let Some(p) = position
        {
            parts.push(format!("at {p}"));
        }
        if !parts.is_empty() {
            head.push(parts.join(" "));
        }
    }
    if let Some((from, position)) = conic {
        let mut parts = Vec::new();
        if let Some(a) = from {
            parts.push(format!("from {}", angle_css(a)));
        }
        if let Some(p) = position {
            parts.push(format!("at {p}"));
        }
        if !parts.is_empty() {
            head.push(parts.join(" "));
        }
    }
    head
}

fn gradient_name(prefix: Option<&'static str>, repeating: bool, kind: &str) -> String {
    format!("{}{}{kind}-gradient", prefix.unwrap_or(""), if repeating { "repeating-" } else { "" })
}

impl Gradient {
    pub fn to_css(&self) -> String {
        let pos = |p: &BackgroundPosition| p.to_css();
        let (kind, head) = match &self.shape {
            GradientShape::Linear(d) => ("linear", gradient_head(self.prefix, Some(*d), None, None, self.interpolation)),
            GradientShape::Radial {
                shape,
                size,
                position,
            } => {
                let size = size.as_ref().map(|s| match s {
                    RadialSize::ClosestSide => "closest-side".to_owned(),
                    RadialSize::FarthestSide => "farthest-side".to_owned(),
                    RadialSize::ClosestCorner => "closest-corner".to_owned(),
                    RadialSize::FarthestCorner => "farthest-corner".to_owned(),
                    RadialSize::Circle(r) => r.to_css(),
                    RadialSize::Ellipse(x, y) => format!("{} {}", x.to_css(), y.to_css()),
                });
                (
                    "radial",
                    gradient_head(self.prefix, None, Some((*shape, size, position.as_ref().map(pos))), None, self.interpolation),
                )
            }
            GradientShape::Conic { from, position } => (
                "conic",
                gradient_head(self.prefix, None, None, Some((*from, position.as_ref().map(pos))), self.interpolation),
            ),
        };
        let gp = |p: &GradientPos| match p {
            GradientPos::Lp(v) => v.to_css(),
            GradientPos::Angle(a) => angle_css(*a),
        };
        let mut parts = head;
        for item in &self.items {
            parts.push(match item {
                GradientItem::Stop { color, positions } => {
                    let mut s = color.to_css();
                    for p in positions {
                        s.push(' ');
                        s.push_str(&gp(p));
                    }
                    s
                }
                GradientItem::Hint(p) => gp(p),
            });
        }
        format!("{}({})", gradient_name(self.prefix, self.repeating, kind), parts.join(", "))
    }
}

fn computed_lp_css(v: ComputedLp) -> String {
    match v {
        ComputedLp::Px(p) => format!("{}px", css_number(p)),
        ComputedLp::Percent(p) => format!("{}%", css_number(p)),
        ComputedLp::Calc(h) => h.to_css(),
    }
}

/// A computed `background-position` axis as the CSSOM prints it.
pub fn position_offset_css(o: crate::computed::PositionOffset) -> String {
    if let Some(h) = o.calc {
        return h.to_css();
    }
    if o.px == 0.0 {
        format!("{}%", css_number(o.percent))
    } else if o.percent == 0.0 {
        format!("{}px", css_number(o.px))
    } else if o.px < 0.0 {
        format!("calc({}% - {}px)", css_number(o.percent), css_number(-o.px))
    } else {
        format!("calc({}% + {}px)", css_number(o.percent), css_number(o.px))
    }
}

impl ComputedGradient {
    pub fn to_css(&self) -> String {
        let pos = ComputedGradientPosition::to_css;
        let (kind, head) = match &self.shape {
            ComputedGradientShape::Linear(d) => ("linear", gradient_head(self.prefix, Some(*d), None, None, self.interpolation)),
            ComputedGradientShape::Radial {
                shape,
                size,
                position,
            } => {
                let size = size.map(|s| match s {
                    ComputedRadialSize::ClosestSide => "closest-side".to_owned(),
                    ComputedRadialSize::FarthestSide => "farthest-side".to_owned(),
                    ComputedRadialSize::ClosestCorner => "closest-corner".to_owned(),
                    ComputedRadialSize::FarthestCorner => "farthest-corner".to_owned(),
                    ComputedRadialSize::Circle(r) => computed_lp_css(r),
                    ComputedRadialSize::Ellipse(x, y) => format!("{} {}", computed_lp_css(x), computed_lp_css(y)),
                });
                (
                    "radial",
                    gradient_head(self.prefix, None, Some((*shape, size, position.as_ref().map(pos))), None, self.interpolation),
                )
            }
            ComputedGradientShape::Conic { from, position } => (
                "conic",
                gradient_head(self.prefix, None, None, Some((*from, position.as_ref().map(pos))), self.interpolation),
            ),
        };
        let gp = |p: &ComputedGradientPos| match p {
            ComputedGradientPos::Lp(v) => computed_lp_css(*v),
            ComputedGradientPos::Angle(a) => angle_css(*a),
        };
        let mut parts = head;
        for item in &self.items {
            parts.push(match item {
                ComputedGradientItem::Stop { color, positions } => {
                    let mut s = color.to_css();
                    for p in positions {
                        s.push(' ');
                        s.push_str(&gp(p));
                    }
                    s
                }
                ComputedGradientItem::Hint(p) => gp(p),
            });
        }
        format!("{}({})", gradient_name(self.prefix, self.repeating, kind), parts.join(", "))
    }
}

// ----- parsing -----

/// The image function `name` whose arguments `input` is positioned at (the
/// function token consumed): a gradient or `image-set()`, or `None` for a
/// function this module does not know. `Err` for a malformed one.
pub(crate) fn parse_image_function<'i>(name: &str, input: &mut Parser<'i, '_>) -> Result<Option<ImageValue>, ParseErr<'i>> {
    let lower = name.to_ascii_lowercase();
    let (prefix, rest): (Option<&'static str>, &str) = if let Some(r) = lower.strip_prefix("-webkit-") {
        (Some("-webkit-"), r)
    } else if let Some(r) = lower.strip_prefix("-moz-") {
        (Some("-moz-"), r)
    } else {
        (None, lower.as_str())
    };
    let (repeating, rest) = match rest.strip_prefix("repeating-") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let kind = match rest {
        "linear-gradient" => Kind::Linear,
        "radial-gradient" => Kind::Radial,
        "conic-gradient" if prefix.is_none() => Kind::Conic,
        "image-set" if !repeating && prefix.is_none_or(|p| p == "-webkit-") => {
            return input.parse_nested_block(parse_image_set).map(Some);
        }
        _ => return Ok(None),
    };
    let gradient = input.parse_nested_block(|i| parse_gradient(i, kind, repeating, prefix))?;
    Ok(Some(ImageValue::Gradient(Arc::new(gradient))))
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Linear,
    Radial,
    Conic,
}

fn parse_gradient<'i>(
    input: &mut Parser<'i, '_>,
    kind: Kind,
    repeating: bool,
    prefix: Option<&'static str>,
) -> Result<Gradient, ParseErr<'i>> {
    let legacy = prefix.is_some();
    // The interpolation method may come before or after the other head
    // parts (`||`); never in the legacy syntax.
    let mut interpolation = if legacy { None } else { input.try_parse(parse_color_interpolation).ok() };
    let (shape, any) = match kind {
        Kind::Linear => {
            let (d, any) = parse_linear_direction(input, legacy)?;
            (GradientShape::Linear(d), any)
        }
        Kind::Radial => parse_radial_head(input, legacy)?,
        Kind::Conic => parse_conic_head(input)?,
    };
    if !legacy && interpolation.is_none() {
        interpolation = input.try_parse(parse_color_interpolation).ok();
    }
    if any || interpolation.is_some() {
        input.expect_comma()?;
    }
    let items = parse_color_stops(input, kind == Kind::Conic)?;
    Ok(Gradient {
        shape,
        repeating,
        items,
        prefix,
        interpolation,
    })
}

/// `in <color-space> [ <hue-interpolation-method> hue ]?` (CSS Color 4
/// §12.1); the hue method only with a polar space.
fn parse_color_interpolation<'i>(input: &mut Parser<'i, '_>) -> Result<ColorInterpolation, ParseErr<'i>> {
    input.expect_ident_matching("in")?;
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    let space = match_ignore_ascii_case! { &ident,
        "srgb" => InterpolationSpace::Srgb,
        "srgb-linear" => InterpolationSpace::SrgbLinear,
        "display-p3" => InterpolationSpace::DisplayP3,
        "a98-rgb" => InterpolationSpace::A98Rgb,
        "prophoto-rgb" => InterpolationSpace::ProphotoRgb,
        "rec2020" => InterpolationSpace::Rec2020,
        "lab" => InterpolationSpace::Lab,
        "oklab" => InterpolationSpace::Oklab,
        "xyz" => InterpolationSpace::Xyz,
        "xyz-d50" => InterpolationSpace::XyzD50,
        "xyz-d65" => InterpolationSpace::XyzD65,
        "hsl" => InterpolationSpace::Hsl,
        "hwb" => InterpolationSpace::Hwb,
        "lch" => InterpolationSpace::Lch,
        "oklch" => InterpolationSpace::Oklch,
        _ => return Err(location.new_unexpected_token_error(Token::Ident(ident))),
    };
    let mut hue = HueMethod::Shorter;
    if space.is_polar()
        && let Ok(h) = input.try_parse(|i| {
            let location = i.current_source_location();
            let word = i.expect_ident()?.clone();
            let h = match_ignore_ascii_case! { &word,
                "shorter" => HueMethod::Shorter,
                "longer" => HueMethod::Longer,
                "increasing" => HueMethod::Increasing,
                "decreasing" => HueMethod::Decreasing,
                _ => return Err(location.new_unexpected_token_error(Token::Ident(word))),
            };
            i.expect_ident_matching("hue")?;
            Ok::<_, ParseErr<'i>>(h)
        })
    {
        hue = h;
    }
    Ok(ColorInterpolation { space, hue })
}

/// `[ <angle> | to <side-or-corner> ]?` (standard) or `[ <angle> |
/// <side-or-corner> ]?` (legacy, where the sides are where the line
/// starts and angles are polar: `0deg` right, counter-clockwise), and
/// whether anything was there (the comma then follows).
fn parse_linear_direction<'i>(input: &mut Parser<'i, '_>, legacy: bool) -> Result<(LinearDirection, bool), ParseErr<'i>> {
    if let Ok(a) = input.try_parse(parse_angle) {
        return Ok((LinearDirection::Angle(if legacy { (90.0 - a).rem_euclid(360.0) } else { a }), true));
    }
    let sides = |i: &mut Parser<'i, '_>| -> Result<LinearDirection, ParseErr<'i>> {
        let mut horizontal = None;
        let mut vertical = None;
        for _ in 0..2 {
            // A word that is not a side (`in`, a colour) is left for what
            // follows.
            let Ok((h, v)) = i.try_parse(|i| {
                let location = i.current_source_location();
                let word = i.expect_ident()?.clone();
                Ok::<_, ParseErr<'i>>(match_ignore_ascii_case! { &word,
                    "left" => (Some(PositionEdge::Start), None),
                    "right" => (Some(PositionEdge::End), None),
                    "top" => (None, Some(PositionEdge::Start)),
                    "bottom" => (None, Some(PositionEdge::End)),
                    _ => return Err(location.new_unexpected_token_error(Token::Ident(word))),
                })
            }) else {
                break;
            };
            if (h.is_some() && horizontal.is_some()) || (v.is_some() && vertical.is_some()) {
                return Err(i.new_custom_error(()));
            }
            horizontal = horizontal.or(h);
            vertical = vertical.or(v);
        }
        if horizontal.is_none() && vertical.is_none() {
            return Err(i.new_custom_error(()));
        }
        Ok(LinearDirection::Corner {
            horizontal,
            vertical,
        })
    };
    if legacy {
        if let Ok(LinearDirection::Corner {
            horizontal,
            vertical,
        }) = input.try_parse(sides)
        {
            // The legacy words say where the line starts.
            let flip = |e: Option<PositionEdge>| {
                e.map(|e| if e == PositionEdge::Start { PositionEdge::End } else { PositionEdge::Start })
            };
            return Ok((
                LinearDirection::Corner {
                    horizontal: flip(horizontal),
                    vertical: flip(vertical),
                },
                true,
            ));
        }
        return Ok((LinearDirection::Default, false));
    }
    if input.try_parse(|i| i.expect_ident_matching("to")).is_ok() {
        return Ok((sides(input)?, true));
    }
    Ok((LinearDirection::Default, false))
}

fn parse_radial_shape<'i>(input: &mut Parser<'i, '_>) -> Result<RadialShape, ParseErr<'i>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    match_ignore_ascii_case! { &ident,
        "circle" => Ok(RadialShape::Circle),
        "ellipse" => Ok(RadialShape::Ellipse),
        _ => Err(location.new_unexpected_token_error(Token::Ident(ident))),
    }
}

/// A size keyword; the legacy syntax also has `contain` and `cover`.
fn parse_radial_extent<'i>(input: &mut Parser<'i, '_>, legacy: bool) -> Result<RadialSize, ParseErr<'i>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    match_ignore_ascii_case! { &ident,
        "closest-side" => Ok(RadialSize::ClosestSide),
        "farthest-side" => Ok(RadialSize::FarthestSide),
        "closest-corner" => Ok(RadialSize::ClosestCorner),
        "farthest-corner" => Ok(RadialSize::FarthestCorner),
        "contain" if legacy => Ok(RadialSize::ClosestSide),
        "cover" if legacy => Ok(RadialSize::FarthestCorner),
        _ => Err(location.new_unexpected_token_error(Token::Ident(ident))),
    }
}

/// A length-percentage that is not negative.
fn parse_non_negative_lp<'i>(input: &mut Parser<'i, '_>) -> Result<LengthPercentage, ParseErr<'i>> {
    let v = parse_length_percentage(input)?;
    let negative = match &v {
        LengthPercentage::Length(l) => l.to_px(&LengthContext {
            font_size: 16.0,
            root_font_size: 16.0,
            viewport_width: 1000.0,
            viewport_height: 1000.0,
        }) < 0.0,
        LengthPercentage::Percent(p) => *p < 0.0,
        LengthPercentage::Calc(_) => false,
    };
    if negative {
        return Err(input.new_custom_error(()));
    }
    Ok(v)
}

/// `[ [ <ending-shape> || <size> ]? [ at <position> ]? ]` (standard) or
/// `[ <position> , ]? [ <shape> || <size> ]?` (legacy), and whether
/// anything was there.
fn parse_radial_head<'i>(input: &mut Parser<'i, '_>, legacy: bool) -> Result<(GradientShape, bool), ParseErr<'i>> {
    let mut shape = None;
    let mut size: Option<RadialSize> = None;
    let mut position = None;
    if legacy
        && let Ok(p) = input.try_parse(|i| {
            let p = parse_background_position(i)?;
            i.expect_comma()?;
            Ok::<_, ParseErr<'i>>(p)
        })
    {
        position = Some(p);
    }
    let mut any = false;
    loop {
        if shape.is_none()
            && let Ok(s) = input.try_parse(parse_radial_shape)
        {
            shape = Some(s);
            any = true;
            continue;
        }
        if size.is_none() {
            if let Ok(s) = input.try_parse(|i| parse_radial_extent(i, legacy)) {
                size = Some(s);
                any = true;
                continue;
            }
            if let Ok(first) = input.try_parse(parse_non_negative_lp) {
                size = Some(match input.try_parse(parse_non_negative_lp) {
                    Ok(second) => RadialSize::Ellipse(first, second),
                    Err(_) => RadialSize::Circle(first),
                });
                any = true;
                continue;
            }
        }
        break;
    }
    // A circle's radius is a length, not a percentage; the shape and the
    // number of radii must agree.
    match (&shape, &size) {
        (Some(RadialShape::Circle), Some(RadialSize::Ellipse(..))) | (Some(RadialShape::Ellipse), Some(RadialSize::Circle(_))) => {
            return Err(input.new_custom_error(()));
        }
        (_, Some(RadialSize::Circle(LengthPercentage::Percent(_)))) => return Err(input.new_custom_error(())),
        _ => {}
    }
    if !legacy && input.try_parse(|i| i.expect_ident_matching("at")).is_ok() {
        position = Some(parse_background_position(input)?);
        any = true;
    }
    Ok((
        GradientShape::Radial {
            shape,
            size,
            position,
        },
        any,
    ))
}

/// `[ from <angle> ]? [ at <position> ]?`, and whether anything was there.
fn parse_conic_head<'i>(input: &mut Parser<'i, '_>) -> Result<(GradientShape, bool), ParseErr<'i>> {
    let mut from = None;
    let mut position = None;
    if input.try_parse(|i| i.expect_ident_matching("from")).is_ok() {
        from = Some(parse_angle(input)?);
    }
    if input.try_parse(|i| i.expect_ident_matching("at")).is_ok() {
        position = Some(parse_background_position(input)?);
    }
    let any = from.is_some() || position.is_some();
    Ok((GradientShape::Conic { from, position }, any))
}

fn parse_gradient_pos<'i>(input: &mut Parser<'i, '_>, angular: bool) -> Result<GradientPos, ParseErr<'i>> {
    if angular {
        if let Ok(a) = input.try_parse(parse_angle) {
            return Ok(GradientPos::Angle(a));
        }
        let location = input.current_source_location();
        return match input.next()?.clone() {
            Token::Percentage { unit_value, .. } => Ok(GradientPos::Lp(LengthPercentage::Percent(unit_value * 100.0))),
            t => Err(location.new_unexpected_token_error(t)),
        };
    }
    parse_length_percentage(input).map(GradientPos::Lp)
}

/// The comma-separated colour stops and hints: a stop is a colour with up
/// to two positions (or one position then the colour), a hint a lone
/// position between two stops. At least two stops; the list starts and
/// ends with a stop; no two hints together.
fn parse_color_stops<'i>(input: &mut Parser<'i, '_>, angular: bool) -> Result<Vec<GradientItem>, ParseErr<'i>> {
    let items = input.parse_comma_separated(|i| {
        if let Ok(color) = i.try_parse(parse_color) {
            let mut positions = Vec::new();
            while positions.len() < 2 {
                match i.try_parse(|i| parse_gradient_pos(i, angular)) {
                    Ok(p) => positions.push(p),
                    Err(_) => break,
                }
            }
            return Ok(GradientItem::Stop { color, positions });
        }
        let p = parse_gradient_pos(i, angular)?;
        match i.try_parse(parse_color) {
            Ok(color) => Ok(GradientItem::Stop {
                color,
                positions: vec![p],
            }),
            Err(_) => Ok(GradientItem::Hint(p)),
        }
    })?;
    let stops = items.iter().filter(|i| matches!(i, GradientItem::Stop { .. })).count();
    let ends_ok = matches!(items.first(), Some(GradientItem::Stop { .. })) && matches!(items.last(), Some(GradientItem::Stop { .. }));
    let hints_apart = items.windows(2).all(|w| !matches!(w, [GradientItem::Hint(_), GradientItem::Hint(_)]));
    if stops < 2 || !ends_ok || !hints_apart {
        return Err(input.new_custom_error(()));
    }
    Ok(items)
}

/// `image-set( <image-set-option># )`, `<image-set-option> = [ <image> |
/// <string> ] [ <resolution> || type(<string>) ]?`.
fn parse_image_set<'i>(input: &mut Parser<'i, '_>) -> Result<ImageValue, ParseErr<'i>> {
    let candidates = input.parse_comma_separated(|i| {
        let image = if let Ok(s) = i.try_parse(|i| i.expect_string().map(|s| s.to_string())) {
            ImageValue::Url(Arc::from(s.as_str()))
        } else {
            let img = parse_image(i)?;
            if matches!(img, ImageValue::None | ImageValue::ImageSet(_)) {
                return Err(i.new_custom_error(()));
            }
            img
        };
        let mut resolution = None;
        let mut mime: Option<String> = None;
        loop {
            if resolution.is_none()
                && let Ok(r) = i.try_parse(parse_resolution)
            {
                resolution = Some(r);
                continue;
            }
            if mime.is_none()
                && let Ok(t) = i.try_parse(|i| {
                    i.expect_function_matching("type")?;
                    i.parse_nested_block(|i| Ok::<_, ParseErr<'i>>(i.expect_string()?.to_string()))
                })
            {
                mime = Some(t);
                continue;
            }
            break;
        }
        Ok((image, resolution.unwrap_or(1.0), mime))
    })?;
    // A candidate of a type this engine cannot decode is dropped.
    let supported = |m: &Option<String>| {
        m.as_deref().is_none_or(|m| {
            matches!(m.trim().to_ascii_lowercase().as_str(), "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp")
        })
    };
    let kept: Vec<ImageSetCandidate> = candidates
        .into_iter()
        .filter(|(_, _, m)| supported(m))
        .map(|(image, resolution, _)| ImageSetCandidate { image, resolution })
        .collect();
    if kept.is_empty() {
        return Err(input.new_custom_error(()));
    }
    Ok(ImageValue::ImageSet(Arc::new(kept)))
}

/// `<resolution>`: `x`/`dppx`, `dpi` or `dpcm`, as dots per CSS pixel;
/// must be positive.
fn parse_resolution<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseErr<'i>> {
    let location = input.current_source_location();
    let r = match input.next()?.clone() {
        Token::Dimension { value, unit, .. } => match_ignore_ascii_case! { &unit,
            "x" | "dppx" => value,
            "dpi" => value / 96.0,
            "dpcm" => value * 2.54 / 96.0,
            _ => return Err(location.new_unexpected_token_error(Token::Ident(unit))),
        },
        t => return Err(location.new_unexpected_token_error(t)),
    };
    if r <= 0.0 {
        return Err(input.new_custom_error(()));
    }
    Ok(r)
}

/// `none`, a `url()`, a gradient, an `image-set()`, or another image
/// function kept as its source text.
pub(crate) fn parse_image<'i>(input: &mut Parser<'i, '_>) -> Result<ImageValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(ImageValue::None);
    }
    let location = input.current_source_location();
    let start = input.position();
    match input.next()?.clone() {
        Token::UnquotedUrl(u) => Ok(ImageValue::Url(Arc::from(&*u))),
        Token::Function(name) => {
            if name.eq_ignore_ascii_case("url") {
                let url = input.parse_nested_block(|i| Ok(i.expect_string()?.to_string()))?;
                return Ok(ImageValue::Url(Arc::from(url.as_str())));
            }
            if let Some(image) = parse_image_function(&name, input)? {
                return Ok(image);
            }
            // `cross-fade()`, `element()`, `paint()`: kept as written so
            // the CSSOM can give them back; nothing paints them.
            input.parse_nested_block(|i| {
                while i.next().is_ok() {}
                Ok(())
            })?;
            Ok(ImageValue::Function(Arc::from(input.slice_from(start).trim())))
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

/// `<angle>` in degrees.
pub(crate) fn parse_angle<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseErr<'i>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Dimension { value, unit, .. } => Ok(match_ignore_ascii_case! { &unit,
            "deg" => value,
            "grad" => value * 0.9,
            "rad" => value.to_degrees(),
            "turn" => value * 360.0,
            _ => return Err(location.new_unexpected_token_error(Token::Ident(unit))),
        }),
        // A unitless zero is the one bare number an angle accepts.
        Token::Number { value, .. } => {
            if value == 0.0 {
                Ok(0.0)
            } else {
                Err(location.new_unexpected_token_error(Token::Number {
                    has_sign: false,
                    value,
                    int_value: None,
                }))
            }
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cssparser::ParserInput;

    fn image(s: &str) -> Option<ImageValue> {
        let mut input = ParserInput::new(s);
        let mut parser = Parser::new(&mut input);
        let v = parse_image(&mut parser).ok()?;
        parser.expect_exhausted().ok()?;
        Some(v)
    }

    fn css(s: &str) -> String {
        image(s).map(|v| v.to_css()).unwrap_or_else(|| "invalid".to_owned())
    }

    #[test]
    fn gradients_parse_and_serialize() {
        assert_eq!(css("linear-gradient(red, blue)"), "linear-gradient(red, blue)");
        assert_eq!(css("linear-gradient(to bottom, red, blue)"), "linear-gradient(red, blue)");
        assert_eq!(css("linear-gradient(to top right, red 10%, 50%, blue 90%)"), "linear-gradient(to right top, red 10%, 50%, blue 90%)");
        assert_eq!(css("linear-gradient(0.25turn, red, blue)"), "linear-gradient(90deg, red, blue)");
        assert_eq!(css("repeating-linear-gradient(45deg, red 0 10px, blue 10px 20px)"), "repeating-linear-gradient(45deg, red 0px 10px, blue 10px 20px)");
        assert_eq!(css("radial-gradient(circle closest-side at 10px 20%, red, blue)"), "radial-gradient(circle closest-side at 10px 20%, red, blue)");
        assert_eq!(css("radial-gradient(20px 30px at center, red, blue)"), "radial-gradient(20px 30px at center center, red, blue)");
        assert_eq!(css("conic-gradient(from 90deg at 25% 75%, red, 50%, blue 270deg)"), "conic-gradient(from 90deg at 25% 75%, red, 50%, blue 270deg)");
        assert_eq!(css("conic-gradient(red, blue)"), "conic-gradient(red, blue)");
        // The interpolation method, before or after the other head parts;
        // the hue method only for a polar space; `shorter hue` left out.
        assert_eq!(css("linear-gradient(in oklab, red, blue)"), "linear-gradient(in oklab, red, blue)");
        assert_eq!(css("linear-gradient(to right in oklch longer hue, red, blue)"), "linear-gradient(to right in oklch longer hue, red, blue)");
        assert_eq!(css("linear-gradient(in hsl shorter hue 90deg, red, blue)"), "linear-gradient(90deg in hsl, red, blue)");
        assert_eq!(css("radial-gradient(in lab circle, red, blue)"), "radial-gradient(circle in lab, red, blue)");
        assert_eq!(css("conic-gradient(from 1turn in srgb-linear, red, blue)"), "conic-gradient(from 360deg in srgb-linear, red, blue)");
        assert_eq!(css("linear-gradient(in oklab longer hue, red, blue)"), "invalid", "no hue method for a rectangular space");
        assert_eq!(css("linear-gradient(in foo, red, blue)"), "invalid");
        // Legacy prefixed syntax: the side is where the line starts, the
        // angle polar; serialized back in its own form.
        assert_eq!(css("-webkit-linear-gradient(left, red, blue)"), "-webkit-linear-gradient(left, red, blue)");
        assert!(matches!(image("-webkit-linear-gradient(left, red, blue)"), Some(ImageValue::Gradient(g)) if g.shape == GradientShape::Linear(LinearDirection::Corner { horizontal: Some(PositionEdge::End), vertical: None })));
        assert!(matches!(image("-moz-linear-gradient(0deg, red, blue)"), Some(ImageValue::Gradient(g)) if g.shape == GradientShape::Linear(LinearDirection::Angle(90.0))));
        assert_eq!(css("-webkit-radial-gradient(center, circle cover, red, blue)"), "-webkit-radial-gradient(center center, circle farthest-corner, red, blue)");
        // Invalid: one stop, a hint at the end, two hints, a circle with a
        // percentage radius, a negative radius, `to` without a side.
        for bad in [
            "linear-gradient(red)",
            "linear-gradient(red, blue, 50%)",
            "linear-gradient(red, 20%, 30%, blue)",
            "radial-gradient(circle 50%, red, blue)",
            "radial-gradient(-5px, red, blue)",
            "linear-gradient(to, red, blue)",
            "linear-gradient(to left left, red, blue)",
            "conic-gradient(red 10px, blue)",
        ] {
            assert_eq!(css(bad), "invalid", "{bad}");
        }
        // Other image functions are kept as written.
        assert_eq!(css("cross-fade(url(a.png), url(b.png), 50%)"), "cross-fade(url(a.png), url(b.png), 50%)");
    }

    #[test]
    fn image_set_parses_and_chooses_by_resolution() {
        let v = image("image-set(\"a.png\" 1x, url(b.png) 2x, url(c.avif) type(\"image/avif\") 3x)").expect("image-set");
        assert_eq!(v.to_css(), "image-set(url(\"a.png\") 1x, url(\"b.png\") 2x)");
        let ctx = LengthContext {
            font_size: 16.0,
            root_font_size: 16.0,
            viewport_width: 800.0,
            viewport_height: 600.0,
        };
        let c = compute_image(&v, &ctx, Rgba::BLACK);
        assert_eq!(c.url_to_fetch(1.0).map(|u| &**u), Some("a.png"));
        assert_eq!(c.url_to_fetch(1.5).map(|u| &**u), Some("b.png"));
        assert_eq!(c.url_to_fetch(3.0).map(|u| &**u), Some("b.png"), "nothing high enough: the largest");
        assert_eq!(css("-webkit-image-set(url(a.png) 96dpi)"), "image-set(url(\"a.png\") 1x)");
        assert_eq!(css("image-set(url(a.avif) type(\"image/avif\"))"), "invalid", "no decodable candidate");
        assert_eq!(css("image-set(url(a.png) 0x)"), "invalid");
    }

    #[test]
    fn computed_gradients_resolve_colours_and_lengths() {
        let v = image("linear-gradient(to right, currentcolor 1em, #00f)").expect("gradient");
        let ctx = LengthContext {
            font_size: 20.0,
            root_font_size: 16.0,
            viewport_width: 800.0,
            viewport_height: 600.0,
        };
        let c = compute_image(&v, &ctx, Rgba::rgb8(255, 0, 0));
        assert_eq!(c.to_css(), "linear-gradient(to right, rgb(255, 0, 0) 20px, rgb(0, 0, 255))");
        // A gradient's position keeps its keywords in the computed value,
        // its lengths in pixels.
        let v = image("radial-gradient(at center top 1em, red, blue)").expect("gradient");
        let c = compute_image(&v, &ctx, Rgba::BLACK);
        assert_eq!(c.to_css(), "radial-gradient(at center top 20px, rgb(255, 0, 0), rgb(0, 0, 255))");
        let ComputedImage::Gradient(g) = &c else { panic!("gradient") };
        let ComputedGradientShape::Radial { position: Some(p), .. } = &g.shape else { panic!("radial") };
        let o = p.to_offsets();
        assert_eq!((o.x.percent, o.x.px, o.y.percent, o.y.px), (50.0, 0.0, 0.0, 20.0));
    }
}
