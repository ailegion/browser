//! Specified and computed value types, and their parsers.

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;

/// A CSS length before computation, in the unit it was written in (the
/// CSSOM serializes a specified value as written). Percentages are kept
/// separate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Em(f32),
    Rem(f32),
    Vw(f32),
    Vh(f32),
    Vmin(f32),
    Vmax(f32),
    /// `ch`, `ex`: approximated from font size at compute time.
    Ch(f32),
    Ex(f32),
    /// The absolute units: fixed ratios to `px`.
    Pt(f32),
    Pc(f32),
    In(f32),
    Cm(f32),
    Mm(f32),
    Q(f32),
}

impl Length {
    pub const ZERO: Length = Length::Px(0.0);

    /// Resolve to pixels.
    pub fn to_px(self, ctx: &LengthContext) -> f32 {
        match self {
            Length::Px(v) => v,
            Length::Em(v) => v * ctx.font_size,
            Length::Rem(v) => v * ctx.root_font_size,
            Length::Vw(v) => v * ctx.viewport_width / 100.0,
            Length::Vh(v) => v * ctx.viewport_height / 100.0,
            Length::Vmin(v) => v * ctx.viewport_width.min(ctx.viewport_height) / 100.0,
            Length::Vmax(v) => v * ctx.viewport_width.max(ctx.viewport_height) / 100.0,
            Length::Ch(v) => v * ctx.font_size * 0.5,
            Length::Ex(v) => v * ctx.font_size * 0.5,
            Length::Pt(v) => v * 96.0 / 72.0,
            Length::Pc(v) => v * 16.0,
            Length::In(v) => v * 96.0,
            Length::Cm(v) => v * 96.0 / 2.54,
            Length::Mm(v) => v * 96.0 / 25.4,
            Length::Q(v) => v * 96.0 / 25.4 / 4.0,
        }
    }

    /// The number and the unit, as written.
    pub fn parts(self) -> (f32, &'static str) {
        match self {
            Length::Px(v) => (v, "px"),
            Length::Em(v) => (v, "em"),
            Length::Rem(v) => (v, "rem"),
            Length::Vw(v) => (v, "vw"),
            Length::Vh(v) => (v, "vh"),
            Length::Vmin(v) => (v, "vmin"),
            Length::Vmax(v) => (v, "vmax"),
            Length::Ch(v) => (v, "ch"),
            Length::Ex(v) => (v, "ex"),
            Length::Pt(v) => (v, "pt"),
            Length::Pc(v) => (v, "pc"),
            Length::In(v) => (v, "in"),
            Length::Cm(v) => (v, "cm"),
            Length::Mm(v) => (v, "mm"),
            Length::Q(v) => (v, "q"),
        }
    }

    /// Serialize as written: `<number><unit>`.
    pub fn to_css(self) -> String {
        let (v, unit) = self.parts();
        format!("{}{unit}", css_number(v))
    }
}

/// Serialize a CSS number: the shortest form with at most six decimals,
/// no trailing zeros, no negative zero (what browsers print).
pub fn css_number(v: f32) -> String {
    if !v.is_finite() {
        return "0".to_owned();
    }
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    let s = if s.is_empty() || s == "-" || s == "-0" { "0" } else { s };
    s.to_owned()
}

/// A `calc()` sum of lengths and percentages, kept as its terms so it can
/// be serialized; `+`/`-` are folded into the signs.
#[derive(Debug, Clone, PartialEq)]
pub struct Calc {
    pub terms: Vec<CalcTerm>,
}

/// One operand of a `calc()` sum, with the sign of the `+`/`-` before it.
#[derive(Debug, Clone, PartialEq)]
pub struct CalcTerm {
    pub negative: bool,
    pub value: CalcValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalcValue {
    Length(Length),
    Percent(f32),
    /// A nested `calc()`, folded on its own before it joins the sum.
    Nested(Calc),
}

impl Calc {
    /// The value the engine uses: `px` terms sum with `px`, `%` with `%`;
    /// anything else keeps what was accumulated so far.
    fn fold(&self) -> LengthPercentage {
        let value = |t: &CalcTerm| match &t.value {
            CalcValue::Length(l) => LengthPercentage::Length(*l),
            CalcValue::Percent(p) => LengthPercentage::Percent(*p),
            CalcValue::Nested(c) => c.fold(),
        };
        let mut iter = self.terms.iter();
        let mut acc = match iter.next() {
            Some(t) => value(t),
            None => LengthPercentage::ZERO,
        };
        for next in iter {
            let op = if next.negative { -1.0 } else { 1.0 };
            acc = match (acc, value(next)) {
                (LengthPercentage::Length(Length::Px(a)), LengthPercentage::Length(Length::Px(b))) => {
                    LengthPercentage::Length(Length::Px(a + op * b))
                }
                (LengthPercentage::Percent(a), LengthPercentage::Percent(b)) => LengthPercentage::Percent(a + op * b),
                // Mixed units are not supported yet; keep the first operand.
                (a, _) => a,
            };
        }
        acc
    }

    /// Sum every term into `percent` and `lengths` by unit, nested sums
    /// flattened, signs applied.
    fn collect(&self, sign: f32, percent: &mut Option<f32>, lengths: &mut Vec<(&'static str, f32)>) {
        for t in &self.terms {
            let s = if t.negative { -sign } else { sign };
            match &t.value {
                CalcValue::Percent(p) => *percent = Some(percent.unwrap_or(0.0) + s * p),
                CalcValue::Length(l) => {
                    let (v, unit) = l.parts();
                    match lengths.iter_mut().find(|(u, _)| *u == unit) {
                        Some(slot) => slot.1 += s * v,
                        None => lengths.push((unit, s * v)),
                    }
                }
                CalcValue::Nested(c) => c.collect(s, percent, lengths),
            }
        }
    }

    /// Serialize per CSS Values 4: like terms summed, percentages first,
    /// then lengths by unit, inside `calc()`.
    pub fn to_css(&self) -> String {
        let mut percent: Option<f32> = None;
        let mut lengths: Vec<(&'static str, f32)> = Vec::new();
        self.collect(1.0, &mut percent, &mut lengths);
        lengths.sort_by(|a, b| a.0.cmp(b.0));
        let mut parts: Vec<String> = Vec::new();
        if let Some(p) = percent {
            parts.push(format!("{}%", css_number(p)));
        }
        for (unit, v) in lengths {
            parts.push(format!("{}{unit}", css_number(v)));
        }
        let mut out = String::from("calc(");
        for (i, p) in parts.iter().enumerate() {
            if i == 0 {
                out.push_str(p);
            } else if let Some(rest) = p.strip_prefix('-') {
                out.push_str(" - ");
                out.push_str(rest);
            } else {
                out.push_str(" + ");
                out.push_str(p);
            }
        }
        out.push(')');
        out
    }
}

/// What a length needs to resolve.
#[derive(Debug, Clone, Copy)]
pub struct LengthContext {
    pub font_size: f32,
    pub root_font_size: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LengthPercentage {
    Length(Length),
    Percent(f32),
    /// A `calc()`, kept as written; `fold` gives what the engine uses.
    Calc(Calc),
}

impl LengthPercentage {
    pub const ZERO: LengthPercentage = LengthPercentage::Length(Length::ZERO);

    /// The engine's value: a plain length or percentage.
    pub fn folded(&self) -> LengthPercentage {
        match self {
            LengthPercentage::Calc(c) => c.fold(),
            other => other.clone(),
        }
    }

    pub fn to_computed(&self, ctx: &LengthContext) -> ComputedLp {
        match self.folded() {
            LengthPercentage::Length(l) => ComputedLp::Px(l.to_px(ctx)),
            LengthPercentage::Percent(p) => ComputedLp::Percent(p),
            LengthPercentage::Calc(_) => ComputedLp::ZERO,
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            LengthPercentage::Length(l) => l.to_css(),
            LengthPercentage::Percent(p) => format!("{}%", css_number(*p)),
            LengthPercentage::Calc(c) => c.to_css(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LengthPercentageAuto {
    Length(Length),
    Percent(f32),
    Calc(Calc),
    Auto,
}

impl LengthPercentageAuto {
    pub fn to_computed(&self, ctx: &LengthContext) -> ComputedLpAuto {
        match self {
            LengthPercentageAuto::Auto => ComputedLpAuto::Auto,
            LengthPercentageAuto::Length(l) => ComputedLpAuto::Px(l.to_px(ctx)),
            LengthPercentageAuto::Percent(p) => ComputedLpAuto::Percent(*p),
            LengthPercentageAuto::Calc(c) => match c.fold() {
                LengthPercentage::Length(l) => ComputedLpAuto::Px(l.to_px(ctx)),
                LengthPercentage::Percent(p) => ComputedLpAuto::Percent(p),
                LengthPercentage::Calc(_) => ComputedLpAuto::Px(0.0),
            },
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            LengthPercentageAuto::Auto => "auto".to_owned(),
            LengthPercentageAuto::Length(l) => l.to_css(),
            LengthPercentageAuto::Percent(p) => format!("{}%", css_number(*p)),
            LengthPercentageAuto::Calc(c) => c.to_css(),
        }
    }
}

/// Computed length-or-percentage: lengths are in px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ComputedLp {
    Px(f32),
    Percent(f32),
}

impl ComputedLp {
    pub const ZERO: ComputedLp = ComputedLp::Px(0.0);
    pub fn resolve(self, basis: f32) -> f32 {
        match self {
            ComputedLp::Px(v) => v,
            ComputedLp::Percent(p) => p / 100.0 * basis,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ComputedLpAuto {
    Px(f32),
    Percent(f32),
    #[default]
    Auto,
}

impl ComputedLpAuto {
    pub fn resolve(self, basis: f32) -> Option<f32> {
        match self {
            ComputedLpAuto::Px(v) => Some(v),
            ComputedLpAuto::Percent(p) => Some(p / 100.0 * basis),
            ComputedLpAuto::Auto => None,
        }
    }
}

/// Size value: auto, length, percentage, or the keywords taffy understands.
#[derive(Debug, Clone, PartialEq)]
pub enum SizeValue {
    Auto,
    Length(Length),
    Percent(f32),
    Calc(Calc),
    MinContent,
    MaxContent,
    FitContent,
    /// `none` for max-width/max-height.
    None,
}

impl SizeValue {
    fn from_lp(lp: LengthPercentage) -> SizeValue {
        match lp {
            LengthPercentage::Length(l) => SizeValue::Length(l),
            LengthPercentage::Percent(p) => SizeValue::Percent(p),
            LengthPercentage::Calc(c) => SizeValue::Calc(c),
        }
    }

    pub fn to_computed(&self, ctx: &LengthContext) -> ComputedSize {
        match self {
            SizeValue::Auto => ComputedSize::Auto,
            SizeValue::Length(l) => ComputedSize::Px(l.to_px(ctx)),
            SizeValue::Percent(p) => ComputedSize::Percent(*p),
            SizeValue::Calc(c) => match c.fold() {
                LengthPercentage::Length(l) => ComputedSize::Px(l.to_px(ctx)),
                LengthPercentage::Percent(p) => ComputedSize::Percent(p),
                LengthPercentage::Calc(_) => ComputedSize::Px(0.0),
            },
            SizeValue::MinContent => ComputedSize::MinContent,
            SizeValue::MaxContent => ComputedSize::MaxContent,
            SizeValue::FitContent => ComputedSize::FitContent,
            SizeValue::None => ComputedSize::None,
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            SizeValue::Auto => "auto".to_owned(),
            SizeValue::Length(l) => l.to_css(),
            SizeValue::Percent(p) => format!("{}%", css_number(*p)),
            SizeValue::Calc(c) => c.to_css(),
            SizeValue::MinContent => "min-content".to_owned(),
            SizeValue::MaxContent => "max-content".to_owned(),
            SizeValue::FitContent => "fit-content".to_owned(),
            SizeValue::None => "none".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ComputedSize {
    #[default]
    Auto,
    Px(f32),
    Percent(f32),
    MinContent,
    MaxContent,
    FitContent,
    None,
}

/// sRGB color with alpha, all 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    pub const fn rgb8(r: u8, g: u8, b: u8) -> Rgba {
        Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    pub fn is_transparent(&self) -> bool {
        self.a <= 0.0
    }

    pub fn to_rgba8(&self) -> [u8; 4] {
        let f = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [f(self.r), f(self.g), f(self.b), f(self.a)]
    }

    /// Serialize per CSS Color 4: `rgb(r, g, b)`, or `rgba(r, g, b, a)`
    /// when not opaque (alpha with up to three decimals).
    pub fn to_css(&self) -> String {
        let [r, g, b, _] = self.to_rgba8();
        let a = self.a.clamp(0.0, 1.0);
        if a >= 1.0 {
            format!("rgb({r}, {g}, {b})")
        } else {
            let s = format!("{:.3}", a);
            let s = s.trim_end_matches('0').trim_end_matches('.');
            let s = if s.is_empty() { "0" } else { s };
            format!("rgba({r}, {g}, {b}, {s})")
        }
    }
}

/// Specified color: a concrete color, a named color (kept by name, as the
/// CSSOM serializes it), `transparent` or `currentcolor`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    Rgba(Rgba),
    /// Index into `NAMED_COLORS`.
    Named(u8),
    Transparent,
    CurrentColor,
}

impl Color {
    pub fn resolve(self, current: Rgba) -> Rgba {
        match self {
            Color::Rgba(c) => c,
            Color::Named(i) => named_rgba(i),
            Color::Transparent => Rgba::TRANSPARENT,
            Color::CurrentColor => current,
        }
    }

    /// Serialize the specified value as written (keywords stay keywords).
    pub fn to_css(self) -> String {
        match self {
            Color::Rgba(c) => c.to_css(),
            Color::Named(i) => NAMED_COLORS[i as usize].0.to_owned(),
            Color::Transparent => "transparent".to_owned(),
            Color::CurrentColor => "currentcolor".to_owned(),
        }
    }
}

fn named_rgba(i: u8) -> Rgba {
    let v = NAMED_COLORS[i as usize].1;
    Rgba::rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

// ----- parsing -----

pub fn parse_length<'i>(input: &mut Parser<'i, '_>) -> Result<Length, ParseErr<'i>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        Token::Dimension { value, unit, .. } => length_from_unit(value, &unit)
            .ok_or_else(|| location.new_unexpected_token_error(Token::Ident(unit))),
        Token::Number { value, .. } if value == 0.0 => Ok(Length::Px(0.0)),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn length_from_unit(value: f32, unit: &str) -> Option<Length> {
    Some(match_ignore_ascii_case! { unit,
        "px" => Length::Px(value),
        "em" => Length::Em(value),
        "rem" => Length::Rem(value),
        "vw" => Length::Vw(value),
        "vh" => Length::Vh(value),
        "vmin" => Length::Vmin(value),
        "vmax" => Length::Vmax(value),
        "ch" => Length::Ch(value),
        "ex" => Length::Ex(value),
        "pt" => Length::Pt(value),
        "pc" => Length::Pc(value),
        "in" => Length::In(value),
        "cm" => Length::Cm(value),
        "mm" => Length::Mm(value),
        "q" => Length::Q(value),
        _ => return None,
    })
}

pub fn parse_length_percentage<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<LengthPercentage, ParseErr<'i>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        Token::Dimension { value, unit, .. } => length_from_unit(value, &unit)
            .map(LengthPercentage::Length)
            .ok_or_else(|| location.new_unexpected_token_error(Token::Ident(unit))),
        Token::Percentage { unit_value, .. } => Ok(LengthPercentage::Percent(unit_value * 100.0)),
        Token::Number { value, .. } if value == 0.0 => Ok(LengthPercentage::ZERO),
        Token::Function(ref name) if name.eq_ignore_ascii_case("calc") => {
            // Minimal calc: lengths and percentages joined by `+`/`-`, kept
            // as written (`Calc::fold` gives the engine's value). Anything
            // else is rejected.
            let terms = input.parse_nested_block(parse_simple_calc)?;
            Ok(LengthPercentage::Calc(Calc { terms }))
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn parse_simple_calc<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<CalcTerm>, ParseErr<'i>> {
    fn term(lp: LengthPercentage, negative: bool) -> CalcTerm {
        let value = match lp {
            LengthPercentage::Length(l) => CalcValue::Length(l),
            LengthPercentage::Percent(p) => CalcValue::Percent(p),
            LengthPercentage::Calc(c) => CalcValue::Nested(c),
        };
        CalcTerm { negative, value }
    }
    let mut terms = vec![term(parse_length_percentage(input)?, false)];
    while !input.is_exhausted() {
        let location = input.current_source_location();
        let negative = match input.next()?.clone() {
            Token::Delim('+') => false,
            Token::Delim('-') => true,
            t => return Err(location.new_unexpected_token_error(t)),
        };
        terms.push(term(parse_length_percentage(input)?, negative));
    }
    Ok(terms)
}

pub fn parse_length_percentage_auto<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<LengthPercentageAuto, ParseErr<'i>> {
    if input
        .try_parse(|i| i.expect_ident_matching("auto"))
        .is_ok()
    {
        return Ok(LengthPercentageAuto::Auto);
    }
    Ok(match parse_length_percentage(input)? {
        LengthPercentage::Length(l) => LengthPercentageAuto::Length(l),
        LengthPercentage::Percent(p) => LengthPercentageAuto::Percent(p),
        LengthPercentage::Calc(c) => LengthPercentageAuto::Calc(c),
    })
}

pub fn parse_size<'i>(input: &mut Parser<'i, '_>, allow_none: bool) -> Result<SizeValue, ParseErr<'i>> {
    if let Ok(ident) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        return Ok(match_ignore_ascii_case! { &ident,
            "auto" => SizeValue::Auto,
            "min-content" => SizeValue::MinContent,
            "max-content" => SizeValue::MaxContent,
            "fit-content" => SizeValue::FitContent,
            "none" if allow_none => SizeValue::None,
            _ => return Err(input.new_custom_error(())),
        });
    }
    Ok(SizeValue::from_lp(parse_length_percentage(input)?))
}

pub fn parse_number<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseErr<'i>> {
    Ok(input.expect_number()?)
}

/// `<number>` or `<percentage>` returning 0..=1 style factor for opacity.
pub fn parse_opacity<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseErr<'i>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Number { value, .. } => Ok(value.clamp(0.0, 1.0)),
        Token::Percentage { unit_value, .. } => Ok(unit_value.clamp(0.0, 1.0)),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

pub fn parse_color<'i>(input: &mut Parser<'i, '_>) -> Result<Color, ParseErr<'i>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        Token::Ident(name) => {
            if name.eq_ignore_ascii_case("currentcolor") {
                return Ok(Color::CurrentColor);
            }
            if name.eq_ignore_ascii_case("transparent") {
                return Ok(Color::Transparent);
            }
            named_color(&name)
                .map(Color::Named)
                .ok_or_else(|| location.new_unexpected_token_error(Token::Ident(name)))
        }
        Token::IDHash(hex) | Token::Hash(hex) => parse_hex_color(&hex)
            .map(Color::Rgba)
            .ok_or_else(|| location.new_unexpected_token_error(Token::Hash(hex))),
        Token::Function(name) => {
            let name = name.to_string();
            input.parse_nested_block(|i| parse_color_function(&name, i))
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn parse_hex_color(hex: &str) -> Option<Rgba> {
    let h = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
    let hh = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    if !hex.is_ascii() {
        return None;
    }
    let (r, g, b, a) = match hex.len() {
        3 => (h(0)? * 17, h(1)? * 17, h(2)? * 17, 255),
        4 => (h(0)? * 17, h(1)? * 17, h(2)? * 17, h(3)? * 17),
        6 => (hh(0)?, hh(2)?, hh(4)?, 255),
        8 => (hh(0)?, hh(2)?, hh(4)?, hh(6)?),
        _ => return None,
    };
    Some(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    })
}

/// rgb()/rgba()/hsl()/hsla() in both legacy comma and modern space syntax.
fn parse_color_function<'i>(name: &str, input: &mut Parser<'i, '_>) -> Result<Color, ParseErr<'i>> {
    let is_rgb = match_ignore_ascii_case! { name,
        "rgb" | "rgba" => true,
        "hsl" | "hsla" => false,
        _ => return Err(input.new_custom_error(())),
    };

    // Component 1
    let c1 = parse_color_component(input, is_rgb, true)?;
    let legacy = input.try_parse(|i| i.expect_comma()).is_ok();
    let c2 = parse_color_component(input, is_rgb, false)?;
    if legacy {
        input.expect_comma()?;
    }
    let c3 = parse_color_component(input, is_rgb, false)?;

    let mut alpha = 1.0;
    if !input.is_exhausted() {
        if legacy {
            input.expect_comma()?;
        } else {
            input.expect_delim('/')?;
        }
        alpha = parse_opacity(input)?;
    }
    input.expect_exhausted()?;

    let (r, g, b) = if is_rgb {
        (c1, c2, c3)
    } else {
        hsl_to_rgb(c1, c2, c3)
    };
    Ok(Color::Rgba(Rgba {
        r: r.clamp(0.0, 1.0),
        g: g.clamp(0.0, 1.0),
        b: b.clamp(0.0, 1.0),
        a: alpha,
    }))
}

/// For rgb: returns 0..=1. For hsl: first is hue in degrees, others 0..=1.
fn parse_color_component<'i>(
    input: &mut Parser<'i, '_>,
    is_rgb: bool,
    first: bool,
) -> Result<f32, ParseErr<'i>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Number { value, .. } => {
            if is_rgb {
                Ok(value / 255.0)
            } else if first {
                Ok(value)
            } else {
                Ok(value / 100.0)
            }
        }
        Token::Percentage { unit_value, .. } => Ok(unit_value),
        Token::Dimension { value, unit, .. } if !is_rgb && first => {
            Ok(match_ignore_ascii_case! { &unit,
                "deg" => value,
                "grad" => value * 0.9,
                "rad" => value.to_degrees(),
                "turn" => value * 360.0,
                _ => return Err(location.new_unexpected_token_error(Token::Ident(unit))),
            })
        }
        Token::Ident(ref i) if i.eq_ignore_ascii_case("none") => Ok(0.0),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(360.0) / 360.0;
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

/// The index of a named color (case-insensitive), if there is one.
fn named_color(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    NAMED_COLORS
        .binary_search_by(|(n, _)| n.cmp(&lower.as_str()))
        .ok()
        .map(|i| i as u8)
}

/// CSS Color Level 4 named colors, sorted by name for binary search.
pub static NAMED_COLORS: [(&str, u32); 148] = [
    ("aliceblue", 0xF0F8FF),
    ("antiquewhite", 0xFAEBD7),
    ("aqua", 0x00FFFF),
    ("aquamarine", 0x7FFFD4),
    ("azure", 0xF0FFFF),
    ("beige", 0xF5F5DC),
    ("bisque", 0xFFE4C4),
    ("black", 0x000000),
    ("blanchedalmond", 0xFFEBCD),
    ("blue", 0x0000FF),
    ("blueviolet", 0x8A2BE2),
    ("brown", 0xA52A2A),
    ("burlywood", 0xDEB887),
    ("cadetblue", 0x5F9EA0),
    ("chartreuse", 0x7FFF00),
    ("chocolate", 0xD2691E),
    ("coral", 0xFF7F50),
    ("cornflowerblue", 0x6495ED),
    ("cornsilk", 0xFFF8DC),
    ("crimson", 0xDC143C),
    ("cyan", 0x00FFFF),
    ("darkblue", 0x00008B),
    ("darkcyan", 0x008B8B),
    ("darkgoldenrod", 0xB8860B),
    ("darkgray", 0xA9A9A9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xA9A9A9),
    ("darkkhaki", 0xBDB76B),
    ("darkmagenta", 0x8B008B),
    ("darkolivegreen", 0x556B2F),
    ("darkorange", 0xFF8C00),
    ("darkorchid", 0x9932CC),
    ("darkred", 0x8B0000),
    ("darksalmon", 0xE9967A),
    ("darkseagreen", 0x8FBC8F),
    ("darkslateblue", 0x483D8B),
    ("darkslategray", 0x2F4F4F),
    ("darkslategrey", 0x2F4F4F),
    ("darkturquoise", 0x00CED1),
    ("darkviolet", 0x9400D3),
    ("deeppink", 0xFF1493),
    ("deepskyblue", 0x00BFFF),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1E90FF),
    ("firebrick", 0xB22222),
    ("floralwhite", 0xFFFAF0),
    ("forestgreen", 0x228B22),
    ("fuchsia", 0xFF00FF),
    ("gainsboro", 0xDCDCDC),
    ("ghostwhite", 0xF8F8FF),
    ("gold", 0xFFD700),
    ("goldenrod", 0xDAA520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xADFF2F),
    ("grey", 0x808080),
    ("honeydew", 0xF0FFF0),
    ("hotpink", 0xFF69B4),
    ("indianred", 0xCD5C5C),
    ("indigo", 0x4B0082),
    ("ivory", 0xFFFFF0),
    ("khaki", 0xF0E68C),
    ("lavender", 0xE6E6FA),
    ("lavenderblush", 0xFFF0F5),
    ("lawngreen", 0x7CFC00),
    ("lemonchiffon", 0xFFFACD),
    ("lightblue", 0xADD8E6),
    ("lightcoral", 0xF08080),
    ("lightcyan", 0xE0FFFF),
    ("lightgoldenrodyellow", 0xFAFAD2),
    ("lightgray", 0xD3D3D3),
    ("lightgreen", 0x90EE90),
    ("lightgrey", 0xD3D3D3),
    ("lightpink", 0xFFB6C1),
    ("lightsalmon", 0xFFA07A),
    ("lightseagreen", 0x20B2AA),
    ("lightskyblue", 0x87CEFA),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xB0C4DE),
    ("lightyellow", 0xFFFFE0),
    ("lime", 0x00FF00),
    ("limegreen", 0x32CD32),
    ("linen", 0xFAF0E6),
    ("magenta", 0xFF00FF),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66CDAA),
    ("mediumblue", 0x0000CD),
    ("mediumorchid", 0xBA55D3),
    ("mediumpurple", 0x9370DB),
    ("mediumseagreen", 0x3CB371),
    ("mediumslateblue", 0x7B68EE),
    ("mediumspringgreen", 0x00FA9A),
    ("mediumturquoise", 0x48D1CC),
    ("mediumvioletred", 0xC71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xF5FFFA),
    ("mistyrose", 0xFFE4E1),
    ("moccasin", 0xFFE4B5),
    ("navajowhite", 0xFFDEAD),
    ("navy", 0x000080),
    ("oldlace", 0xFDF5E6),
    ("olive", 0x808000),
    ("olivedrab", 0x6B8E23),
    ("orange", 0xFFA500),
    ("orangered", 0xFF4500),
    ("orchid", 0xDA70D6),
    ("palegoldenrod", 0xEEE8AA),
    ("palegreen", 0x98FB98),
    ("paleturquoise", 0xAFEEEE),
    ("palevioletred", 0xDB7093),
    ("papayawhip", 0xFFEFD5),
    ("peachpuff", 0xFFDAB9),
    ("peru", 0xCD853F),
    ("pink", 0xFFC0CB),
    ("plum", 0xDDA0DD),
    ("powderblue", 0xB0E0E6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xFF0000),
    ("rosybrown", 0xBC8F8F),
    ("royalblue", 0x4169E1),
    ("saddlebrown", 0x8B4513),
    ("salmon", 0xFA8072),
    ("sandybrown", 0xF4A460),
    ("seagreen", 0x2E8B57),
    ("seashell", 0xFFF5EE),
    ("sienna", 0xA0522D),
    ("silver", 0xC0C0C0),
    ("skyblue", 0x87CEEB),
    ("slateblue", 0x6A5ACD),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xFFFAFA),
    ("springgreen", 0x00FF7F),
    ("steelblue", 0x4682B4),
    ("tan", 0xD2B48C),
    ("teal", 0x008080),
    ("thistle", 0xD8BFD8),
    ("tomato", 0xFF6347),
    ("turquoise", 0x40E0D0),
    ("violet", 0xEE82EE),
    ("wheat", 0xF5DEB3),
    ("white", 0xFFFFFF),
    ("whitesmoke", 0xF5F5F5),
    ("yellow", 0xFFFF00),
    ("yellowgreen", 0x9ACD32),
];

#[cfg(test)]
mod tests {
    use super::*;
    use cssparser::ParserInput;

    fn color(s: &str) -> Option<Rgba> {
        let mut input = ParserInput::new(s);
        let mut parser = Parser::new(&mut input);
        match parse_color(&mut parser) {
            Ok(Color::CurrentColor) | Err(_) => None,
            Ok(c) => Some(c.resolve(Rgba::BLACK)),
        }
    }

    fn lp(s: &str) -> Option<LengthPercentage> {
        let mut input = ParserInput::new(s);
        let mut parser = Parser::new(&mut input);
        parse_length_percentage(&mut parser).ok()
    }

    #[test]
    fn serializes_numbers_lengths_calc_and_colors() {
        assert_eq!(css_number(10.0), "10");
        assert_eq!(css_number(0.5), "0.5");
        assert_eq!(css_number(-0.0), "0");
        assert_eq!(css_number(0.123_456_8), "0.123457");
        assert_eq!(css_number(1.10), "1.1");
        assert_eq!(lp("10PT").map(|v| v.to_css()).as_deref(), Some("10pt"));
        assert_eq!(lp("0").map(|v| v.to_css()).as_deref(), Some("0px"));
        assert_eq!(lp("1.50%").map(|v| v.to_css()).as_deref(), Some("1.5%"));
        assert_eq!(lp("calc(1px + 2px)").map(|v| v.to_css()).as_deref(), Some("calc(3px)"));
        assert_eq!(lp("calc(10px - 5%)").map(|v| v.to_css()).as_deref(), Some("calc(-5% + 10px)"));
        assert_eq!(lp("calc(1em + 2px - 1px)").map(|v| v.to_css()).as_deref(), Some("calc(1em + 1px)"));
        // The engine's value is unchanged: like kinds sum, mixed keeps the first.
        let ctx = LengthContext { font_size: 10.0, root_font_size: 10.0, viewport_width: 100.0, viewport_height: 100.0 };
        assert_eq!(lp("calc(1px + 2px)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(3.0)));
        assert_eq!(lp("calc(10px + 5%)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(10.0)));
        assert_eq!(lp("calc(5% + 10px)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Percent(5.0)));
        assert_eq!(lp("calc(1em + 1em)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(10.0)));
        let c = |s: &str| {
            let mut input = ParserInput::new(s);
            let mut parser = Parser::new(&mut input);
            parse_color(&mut parser).map(|c| c.to_css()).ok()
        };
        assert_eq!(c("Red").as_deref(), Some("red"));
        assert_eq!(c("#ff0000").as_deref(), Some("rgb(255, 0, 0)"));
        assert_eq!(c("#00ff0080").as_deref(), Some("rgba(0, 255, 0, 0.502)"));
        assert_eq!(c("rgba(1, 2, 3, 0.5)").as_deref(), Some("rgba(1, 2, 3, 0.5)"));
        assert_eq!(c("transparent").as_deref(), Some("transparent"));
        assert_eq!(c("CurrentColor").as_deref(), Some("currentcolor"));
        assert_eq!(c("hsl(120, 100%, 50%)").as_deref(), Some("rgb(0, 255, 0)"));
    }

    #[test]
    fn named_colors_sorted() {
        for w in NAMED_COLORS.windows(2) {
            assert!(w[0].0 < w[1].0, "{} before {}", w[0].0, w[1].0);
        }
    }

    #[test]
    fn parses_colors() {
        assert_eq!(color("red").map(|c| c.to_rgba8()), Some([255, 0, 0, 255]));
        assert_eq!(color("#0f0").map(|c| c.to_rgba8()), Some([0, 255, 0, 255]));
        assert_eq!(color("#00ff0080").map(|c| c.to_rgba8()), Some([0, 255, 0, 128]));
        assert_eq!(color("rgb(1, 2, 3)").map(|c| c.to_rgba8()), Some([1, 2, 3, 255]));
        assert_eq!(color("rgba(1, 2, 3, 0.5)").map(|c| c.to_rgba8()), Some([1, 2, 3, 128]));
        assert_eq!(color("rgb(1 2 3 / 50%)").map(|c| c.to_rgba8()), Some([1, 2, 3, 128]));
        assert_eq!(color("hsl(120, 100%, 50%)").map(|c| c.to_rgba8()), Some([0, 255, 0, 255]));
        assert_eq!(color("hsl(0 0% 100%)").map(|c| c.to_rgba8()), Some([255, 255, 255, 255]));
        assert_eq!(color("transparent").map(|c| c.a), Some(0.0));
        assert!(color("notacolor").is_none());
    }
}
