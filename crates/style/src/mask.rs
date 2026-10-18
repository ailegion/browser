//! CSS Masking 1 §6 ("Working sites first" line 6c): the `mask-*`
//! longhands, the `mask` shorthand and their `-webkit-` aliases. The layer
//! values mirror the background ones (`mask-repeat`, `mask-position`,
//! `mask-size` share their types and parsers).

use cssparser::{Parser, match_ignore_ascii_case};

use crate::ParseErr;
use crate::image::parse_image;
use crate::properties::{
    BackgroundBox, BackgroundPosition, BackgroundRepeat, BackgroundSize, ImageValue, PropertyDeclaration, parse_background_position,
    parse_background_repeat, parse_background_size,
};

/// `mask-mode`: how a mask image becomes a mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaskMode {
    /// An image's alpha (an SVG `<mask>`'s luminance).
    #[default]
    MatchSource,
    Alpha,
    Luminance,
}

impl MaskMode {
    pub fn keyword(self) -> &'static str {
        match self {
            MaskMode::MatchSource => "match-source",
            MaskMode::Alpha => "alpha",
            MaskMode::Luminance => "luminance",
        }
    }
}

/// `mask-composite`: how a layer combines with the layers below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaskComposite {
    #[default]
    Add,
    Subtract,
    Intersect,
    Exclude,
}

impl MaskComposite {
    pub fn keyword(self) -> &'static str {
        match self {
            MaskComposite::Add => "add",
            MaskComposite::Subtract => "subtract",
            MaskComposite::Intersect => "intersect",
            MaskComposite::Exclude => "exclude",
        }
    }
}

/// `<geometry-box>`: the boxes `mask-origin` and `mask-clip` name. The SVG
/// boxes compute to `border-box` on a CSS box (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeometryBox {
    #[default]
    BorderBox,
    PaddingBox,
    ContentBox,
    FillBox,
    StrokeBox,
    ViewBox,
}

impl GeometryBox {
    pub fn keyword(self) -> &'static str {
        match self {
            GeometryBox::BorderBox => "border-box",
            GeometryBox::PaddingBox => "padding-box",
            GeometryBox::ContentBox => "content-box",
            GeometryBox::FillBox => "fill-box",
            GeometryBox::StrokeBox => "stroke-box",
            GeometryBox::ViewBox => "view-box",
        }
    }

    /// The box on a CSS box.
    pub fn computed(self) -> BackgroundBox {
        match self {
            GeometryBox::PaddingBox => BackgroundBox::PaddingBox,
            GeometryBox::ContentBox => BackgroundBox::ContentBox,
            _ => BackgroundBox::BorderBox,
        }
    }
}

/// `mask-clip`: a box, or `no-clip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskClip {
    Box(GeometryBox),
    NoClip,
}

impl Default for MaskClip {
    fn default() -> Self {
        MaskClip::Box(GeometryBox::BorderBox)
    }
}

impl MaskClip {
    pub fn keyword(self) -> &'static str {
        match self {
            MaskClip::Box(b) => b.keyword(),
            MaskClip::NoClip => "no-clip",
        }
    }

    /// The clipping box on a CSS box; `None` is `no-clip`.
    pub fn computed(self) -> Option<BackgroundBox> {
        match self {
            MaskClip::Box(b) => Some(b.computed()),
            MaskClip::NoClip => None,
        }
    }
}

pub fn parse_mode<'i>(input: &mut Parser<'i, '_>) -> Result<MaskMode, ParseErr<'i>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    Ok(match_ignore_ascii_case! { &ident,
        "match-source" => MaskMode::MatchSource,
        "alpha" => MaskMode::Alpha,
        "luminance" => MaskMode::Luminance,
        _ => return Err(location.new_custom_error(())),
    })
}

pub fn parse_composite<'i>(input: &mut Parser<'i, '_>) -> Result<MaskComposite, ParseErr<'i>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    Ok(match_ignore_ascii_case! { &ident,
        "add" => MaskComposite::Add,
        "subtract" => MaskComposite::Subtract,
        "intersect" => MaskComposite::Intersect,
        "exclude" => MaskComposite::Exclude,
        _ => return Err(location.new_custom_error(())),
    })
}

/// `-webkit-mask-composite`: the old Porter-Duff names, as the operators
/// they are.
pub fn parse_webkit_composite<'i>(input: &mut Parser<'i, '_>) -> Result<MaskComposite, ParseErr<'i>> {
    if let Ok(c) = input.try_parse(parse_composite) {
        return Ok(c);
    }
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    Ok(match_ignore_ascii_case! { &ident,
        "source-over" => MaskComposite::Add,
        "source-out" => MaskComposite::Subtract,
        "source-in" => MaskComposite::Intersect,
        "xor" => MaskComposite::Exclude,
        _ => return Err(location.new_custom_error(())),
    })
}

pub fn parse_geometry_box<'i>(input: &mut Parser<'i, '_>) -> Result<GeometryBox, ParseErr<'i>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    Ok(match_ignore_ascii_case! { &ident,
        "border-box" => GeometryBox::BorderBox,
        "padding-box" => GeometryBox::PaddingBox,
        "content-box" => GeometryBox::ContentBox,
        "fill-box" => GeometryBox::FillBox,
        "stroke-box" => GeometryBox::StrokeBox,
        "view-box" => GeometryBox::ViewBox,
        _ => return Err(location.new_custom_error(())),
    })
}

pub fn parse_clip<'i>(input: &mut Parser<'i, '_>) -> Result<MaskClip, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("no-clip")).is_ok() {
        return Ok(MaskClip::NoClip);
    }
    Ok(MaskClip::Box(parse_geometry_box(input)?))
}

/// One `<mask-layer>` of the shorthand: `<mask-reference> || <position> [
/// / <bg-size> ]? || <repeat-style> || <geometry-box> || [ <geometry-box> |
/// no-clip ] || <compositing-operator> || <masking-mode>`.
#[derive(Default)]
struct Layer {
    image: Option<ImageValue>,
    position: Option<BackgroundPosition>,
    size: Option<BackgroundSize>,
    repeat: Option<BackgroundRepeat>,
    origin: Option<GeometryBox>,
    clip: Option<MaskClip>,
    composite: Option<MaskComposite>,
    mode: Option<MaskMode>,
}

fn parse_layer<'i>(input: &mut Parser<'i, '_>, webkit: bool) -> Result<Layer, ParseErr<'i>> {
    let mut layer = Layer::default();
    let mut any = false;
    loop {
        if layer.image.is_none() && let Ok(img) = input.try_parse(parse_image) {
            layer.image = Some(img);
        } else if layer.position.is_none() && let Ok(pos) = input.try_parse(parse_background_position) {
            layer.position = Some(pos);
            if input.try_parse(|i| i.expect_delim('/')).is_ok() {
                layer.size = Some(parse_background_size(input)?);
            }
        } else if layer.repeat.is_none() && let Ok(r) = input.try_parse(parse_background_repeat) {
            layer.repeat = Some(r);
        } else if layer.origin.is_none() && let Ok(b) = input.try_parse(parse_geometry_box) {
            layer.origin = Some(b);
        } else if layer.origin.is_some() && layer.clip.is_none() && let Ok(c) = input.try_parse(parse_clip) {
            layer.clip = Some(c);
        } else if layer.origin.is_none() && layer.clip.is_none() && input.try_parse(|i| i.expect_ident_matching("no-clip")).is_ok() {
            layer.clip = Some(MaskClip::NoClip);
        } else if layer.composite.is_none()
            && let Ok(c) = input.try_parse(|i| if webkit { parse_webkit_composite(i) } else { parse_composite(i) })
        {
            layer.composite = Some(c);
        } else if layer.mode.is_none() && let Ok(m) = input.try_parse(parse_mode) {
            layer.mode = Some(m);
        } else {
            break;
        }
        any = true;
    }
    if !any {
        return Err(input.new_custom_error(()));
    }
    Ok(layer)
}

/// The `mask` shorthand (and `-webkit-mask`): its layers, each longhand
/// one value per layer, the parts a layer leaves out initial. One box sets
/// `mask-origin` and `mask-clip`; a second sets `mask-clip`.
pub fn parse_shorthand<'i>(input: &mut Parser<'i, '_>, webkit: bool) -> Result<Vec<PropertyDeclaration>, ParseErr<'i>> {
    use PropertyDeclaration as P;
    let layers = input.parse_comma_separated(|i| parse_layer(i, webkit))?;
    let mut images = Vec::new();
    let mut positions = Vec::new();
    let mut sizes = Vec::new();
    let mut repeats = Vec::new();
    let mut origins = Vec::new();
    let mut clips = Vec::new();
    let mut composites = Vec::new();
    let mut modes = Vec::new();
    for l in layers {
        images.push(l.image.unwrap_or(ImageValue::None));
        positions.push(l.position.unwrap_or(BackgroundPosition::INITIAL));
        sizes.push(l.size.unwrap_or(BackgroundSize::Explicit(None, None)));
        repeats.push(l.repeat.unwrap_or_default());
        let origin = l.origin.unwrap_or_default();
        origins.push(origin);
        clips.push(l.clip.unwrap_or(match l.origin {
            Some(o) => MaskClip::Box(o),
            None => MaskClip::default(),
        }));
        composites.push(l.composite.unwrap_or_default());
        modes.push(l.mode.unwrap_or_default());
    }
    Ok(vec![
        P::MaskImage(images),
        P::MaskPosition(positions),
        P::MaskSize(sizes),
        P::MaskRepeat(repeats),
        P::MaskOrigin(origins),
        P::MaskClip(clips),
        P::MaskComposite(composites),
        P::MaskMode(modes),
    ])
}
