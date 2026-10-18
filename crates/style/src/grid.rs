//! Grid layout values (CSS Grid 1; "Working sites first" line 5): track
//! lists with lengths, `fr`, `repeat()` with a count, `minmax()`,
//! `fit-content()`, the content keywords and line names; implicit track
//! sizes; `grid-auto-flow`; the grid lines items are placed on.
//!
//! The types are generic over the length so the specified value (as
//! written, for the CSSOM) and the computed one (pixels and percentages)
//! share them.

use std::sync::Arc;

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;
use crate::values::{ComputedLp, LengthContext, LengthPercentage, css_number, parse_length_percentage};

/// A track's breadth: a length or percentage, a flexible fraction, or a
/// content keyword.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackBreadth<L> {
    Length(L),
    Flex(f32),
    MinContent,
    MaxContent,
    Auto,
}

/// A track's size: one breadth, `minmax(min, max)`, or `fit-content()`.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackSize<L> {
    Breadth(TrackBreadth<L>),
    MinMax(TrackBreadth<L>, TrackBreadth<L>),
    FitContent(L),
}

/// One piece of a track list: a set of line names, a track, or a
/// `repeat()` of names and tracks.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackComponent<L> {
    Names(Vec<Arc<str>>),
    Size(TrackSize<L>),
    Repeat(u32, Vec<TrackComponent<L>>),
}

/// `grid-template-columns`/`-rows`: `none` or a track list.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TrackList<L> {
    #[default]
    None,
    List(Vec<TrackComponent<L>>),
}

/// `grid-template-areas` (CSS Grid 1 §7.3; "Working sites first" line
/// 6a): `none`, or rows of cells, each a name or `None` (a `.` cell). All
/// rows have the same number of cells and each name covers a rectangle.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TemplateAreas {
    #[default]
    None,
    Rows(Arc<[Vec<Option<Arc<str>>>]>),
}

/// A named area: its name and its lines, 1-based, end lines exclusive.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedArea {
    pub name: Arc<str>,
    pub row_start: u16,
    pub row_end: u16,
    pub column_start: u16,
    pub column_end: u16,
}

impl TemplateAreas {
    /// The rows as written: each a string of its cells (`.` for an
    /// unnamed one) separated by single spaces.
    pub fn to_css(&self) -> String {
        match self {
            TemplateAreas::None => "none".to_owned(),
            TemplateAreas::Rows(rows) => rows.iter().map(|r| row_css(r)).collect::<Vec<_>>().join(" "),
        }
    }

    /// The named areas, in the order their first cell appears.
    pub fn areas(&self) -> Vec<NamedArea> {
        let TemplateAreas::Rows(rows) = self else { return Vec::new() };
        let mut out: Vec<NamedArea> = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let Some(name) = cell else { continue };
                let (r, c) = (r as u16 + 1, c as u16 + 1);
                match out.iter_mut().find(|a| a.name == *name) {
                    Some(a) => {
                        a.row_end = a.row_end.max(r + 1);
                        a.column_end = a.column_end.max(c + 1);
                    }
                    None => out.push(NamedArea { name: name.clone(), row_start: r, row_end: r + 1, column_start: c, column_end: c + 1 }),
                }
            }
        }
        out
    }

    /// The number of rows and of columns of the template.
    pub fn size(&self) -> (u16, u16) {
        match self {
            TemplateAreas::None => (0, 0),
            TemplateAreas::Rows(rows) => (rows.len() as u16, rows.first().map_or(0, Vec::len) as u16),
        }
    }
}

/// One row of cells as a CSS string.
pub fn row_css(row: &[Option<Arc<str>>]) -> String {
    let cells: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or(".")).collect();
    format!("\"{}\"", cells.join(" "))
}

/// The cells of one area string (§7.3.1): runs of name code points (letters,
/// digits, `-`, `_`, non-ASCII) are named cells, runs of `.` unnamed ones,
/// white space separates; anything else makes the declaration invalid.
fn parse_area_row(s: &str) -> Option<Vec<Option<Arc<str>>>> {
    let name_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii();
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C') {
            chars.next();
        } else if c == '.' {
            while chars.peek() == Some(&'.') {
                chars.next();
            }
            out.push(None);
        } else if name_char(c) {
            let mut name = String::new();
            while let Some(&c) = chars.peek().filter(|c| name_char(**c)) {
                name.push(c);
                chars.next();
            }
            out.push(Some(Arc::from(name.as_str())));
        } else {
            return None;
        }
    }
    Some(out)
}

/// The template the rows make: `None` when a row has no cell, the rows
/// differ in length, or a name does not cover a rectangle.
fn areas_from_rows(rows: Vec<Vec<Option<Arc<str>>>>) -> Option<TemplateAreas> {
    let columns = rows.first()?.len();
    if columns == 0 || rows.iter().any(|r| r.len() != columns) {
        return None;
    }
    let template = TemplateAreas::Rows(Arc::from(rows));
    let TemplateAreas::Rows(cells) = &template else { return None };
    for a in template.areas() {
        for r in a.row_start..a.row_end {
            for c in a.column_start..a.column_end {
                if cells[r as usize - 1][c as usize - 1].as_ref() != Some(&a.name) {
                    return None;
                }
            }
        }
    }
    Some(template)
}

/// `none | <string>+`.
pub fn parse_template_areas<'i>(input: &mut Parser<'i, '_>) -> Result<TemplateAreas, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(TemplateAreas::None);
    }
    let mut rows = Vec::new();
    while let Ok(s) = input.try_parse(|i| i.expect_string().map(|s| s.to_string())) {
        rows.push(parse_area_row(&s).ok_or_else(|| input.new_custom_error(()))?);
    }
    areas_from_rows(rows).ok_or_else(|| input.new_custom_error(()))
}

/// The area form of `grid-template` (§7.4): `[ <line-names>? <string>
/// <track-size>? <line-names>? ]+ [ / <explicit-track-list> ]?`, a row's
/// trailing names and the next row's leading ones one set. Rows without a
/// size are `auto`; the columns are `none` without the `/` part.
pub fn parse_area_template<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<(TrackList<LengthPercentage>, TrackList<LengthPercentage>, TemplateAreas), ParseErr<'i>> {
    let mut rows: Vec<TrackComponent<LengthPercentage>> = Vec::new();
    let mut strings = Vec::new();
    loop {
        let leading = input.try_parse(parse_line_names).ok();
        let Ok(s) = input.try_parse(|i| i.expect_string().map(|s| s.to_string())) else {
            if leading.is_some() || strings.is_empty() {
                return Err(input.new_custom_error(()));
            }
            break;
        };
        if let Some(names) = leading {
            match rows.last_mut() {
                Some(TrackComponent::Names(prev)) => prev.extend(names),
                _ => rows.push(TrackComponent::Names(names)),
            }
        }
        strings.push(parse_area_row(&s).ok_or_else(|| input.new_custom_error(()))?);
        let size = input.try_parse(parse_track_size).unwrap_or(TrackSize::Breadth(TrackBreadth::Auto));
        rows.push(TrackComponent::Size(size));
        if let Ok(names) = input.try_parse(parse_line_names) {
            rows.push(TrackComponent::Names(names));
        }
        if input.is_exhausted() {
            break;
        }
        let state = input.state();
        if input.try_parse(|i| i.expect_delim('/')).is_ok() {
            input.reset(&state);
            break;
        }
    }
    let areas = areas_from_rows(strings).ok_or_else(|| input.new_custom_error(()))?;
    let columns = if input.try_parse(|i| i.expect_delim('/')).is_ok() {
        // `<explicit-track-list>`: no `repeat()`.
        TrackList::List(parse_components(input, false)?)
    } else {
        TrackList::None
    };
    Ok((TrackList::List(rows), columns, areas))
}

/// `grid-auto-flow`: the axis auto-placed items fill, and whether holes
/// are filled (`dense`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GridAutoFlow {
    pub column: bool,
    pub dense: bool,
}

/// A grid line an item's edge is placed on (`grid-row-start` and the
/// like): `auto`, a named line, the `n`th line (of a name), or a span.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum GridLine {
    #[default]
    Auto,
    Ident(Arc<str>),
    Line(i32, Option<Arc<str>>),
    Span(u32, Option<Arc<str>>),
}

impl GridLine {
    pub fn is_auto(&self) -> bool {
        matches!(self, GridLine::Auto)
    }

    pub fn to_css(&self) -> String {
        match self {
            GridLine::Auto => "auto".to_owned(),
            GridLine::Ident(n) => n.to_string(),
            GridLine::Line(i, None) => i.to_string(),
            GridLine::Line(i, Some(n)) => format!("{i} {n}"),
            GridLine::Span(c, None) => format!("span {c}"),
            GridLine::Span(1, Some(n)) => format!("span {n}"),
            GridLine::Span(c, Some(n)) => format!("span {c} {n}"),
        }
    }
}

impl GridAutoFlow {
    /// The shortest form: `row` and `dense` stand alone, `column` and
    /// `dense` together.
    pub fn to_css(self) -> String {
        match (self.column, self.dense) {
            (false, false) => "row".to_owned(),
            (false, true) => "dense".to_owned(),
            (true, false) => "column".to_owned(),
            (true, true) => "column dense".to_owned(),
        }
    }
}

// ----- serialization -----

/// How a length of `L` is written.
pub trait GridLength {
    fn length_css(&self) -> String;
}

impl GridLength for LengthPercentage {
    fn length_css(&self) -> String {
        self.to_css()
    }
}

impl GridLength for ComputedLp {
    fn length_css(&self) -> String {
        match self {
            ComputedLp::Px(v) => format!("{}px", css_number(*v)),
            ComputedLp::Percent(p) => format!("{}%", css_number(*p)),
        }
    }
}

impl<L: GridLength> TrackBreadth<L> {
    pub fn to_css(&self) -> String {
        match self {
            TrackBreadth::Length(l) => l.length_css(),
            TrackBreadth::Flex(f) => format!("{}fr", css_number(*f)),
            TrackBreadth::MinContent => "min-content".to_owned(),
            TrackBreadth::MaxContent => "max-content".to_owned(),
            TrackBreadth::Auto => "auto".to_owned(),
        }
    }
}

impl<L: GridLength> TrackSize<L> {
    pub fn to_css(&self) -> String {
        match self {
            TrackSize::Breadth(b) => b.to_css(),
            TrackSize::MinMax(a, b) => format!("minmax({}, {})", a.to_css(), b.to_css()),
            TrackSize::FitContent(l) => format!("fit-content({})", l.length_css()),
        }
    }
}

/// Line names as written: `[a b]`.
pub fn names_css(names: &[Arc<str>]) -> String {
    let list: Vec<&str> = names.iter().map(|n| &**n).collect();
    format!("[{}]", list.join(" "))
}

fn components_css<L: GridLength>(list: &[TrackComponent<L>]) -> String {
    list.iter()
        .map(|c| match c {
            TrackComponent::Names(n) => names_css(n),
            TrackComponent::Size(s) => s.to_css(),
            TrackComponent::Repeat(count, items) => format!("repeat({count}, {})", components_css(items)),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl<L: GridLength> TrackList<L> {
    pub fn to_css(&self) -> String {
        match self {
            TrackList::None => "none".to_owned(),
            TrackList::List(l) => components_css(l),
        }
    }
}

/// Implicit track sizes as written (an empty list is the initial
/// `auto`).
pub fn auto_tracks_css<L: GridLength>(tracks: &[TrackSize<L>]) -> String {
    if tracks.is_empty() {
        return "auto".to_owned();
    }
    tracks.iter().map(TrackSize::to_css).collect::<Vec<_>>().join(" ")
}

// ----- computation -----

impl TrackBreadth<LengthPercentage> {
    fn compute(&self, ctx: &LengthContext) -> TrackBreadth<ComputedLp> {
        match self {
            TrackBreadth::Length(l) => TrackBreadth::Length(l.to_computed(ctx)),
            TrackBreadth::Flex(f) => TrackBreadth::Flex(*f),
            TrackBreadth::MinContent => TrackBreadth::MinContent,
            TrackBreadth::MaxContent => TrackBreadth::MaxContent,
            TrackBreadth::Auto => TrackBreadth::Auto,
        }
    }
}

impl TrackSize<LengthPercentage> {
    pub fn compute(&self, ctx: &LengthContext) -> TrackSize<ComputedLp> {
        match self {
            TrackSize::Breadth(b) => TrackSize::Breadth(b.compute(ctx)),
            TrackSize::MinMax(a, b) => TrackSize::MinMax(a.compute(ctx), b.compute(ctx)),
            TrackSize::FitContent(l) => TrackSize::FitContent(l.to_computed(ctx)),
        }
    }
}

fn compute_components(list: &[TrackComponent<LengthPercentage>], ctx: &LengthContext) -> Vec<TrackComponent<ComputedLp>> {
    list.iter()
        .map(|c| match c {
            TrackComponent::Names(n) => TrackComponent::Names(n.clone()),
            TrackComponent::Size(s) => TrackComponent::Size(s.compute(ctx)),
            TrackComponent::Repeat(count, items) => TrackComponent::Repeat(*count, compute_components(items, ctx)),
        })
        .collect()
}

impl TrackList<LengthPercentage> {
    pub fn compute(&self, ctx: &LengthContext) -> TrackList<ComputedLp> {
        match self {
            TrackList::None => TrackList::None,
            TrackList::List(l) => TrackList::List(compute_components(l, ctx)),
        }
    }
}

impl<L: Clone> TrackList<L> {
    /// The explicit tracks with `repeat()` written out, and the line names
    /// of each line (one more than the tracks).
    pub fn expanded(&self) -> (Vec<TrackSize<L>>, Vec<Vec<Arc<str>>>) {
        let mut tracks = Vec::new();
        let mut names: Vec<Vec<Arc<str>>> = vec![Vec::new()];
        fn walk<L: Clone>(list: &[TrackComponent<L>], tracks: &mut Vec<TrackSize<L>>, names: &mut Vec<Vec<Arc<str>>>) {
            for c in list {
                match c {
                    TrackComponent::Names(n) => {
                        if let Some(last) = names.last_mut() {
                            last.extend(n.iter().cloned());
                        }
                    }
                    TrackComponent::Size(s) => {
                        tracks.push(s.clone());
                        names.push(Vec::new());
                    }
                    TrackComponent::Repeat(count, items) => {
                        for _ in 0..*count {
                            walk(items, tracks, names);
                        }
                    }
                }
            }
        }
        if let TrackList::List(l) = self {
            walk(l, &mut tracks, &mut names);
        }
        (tracks, names)
    }
}

// ----- parsing -----

/// A `<custom-ident>` that may name a grid line: not `span`, `auto` or a
/// CSS-wide keyword.
fn parse_line_ident<'i>(input: &mut Parser<'i, '_>) -> Result<Arc<str>, ParseErr<'i>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    let bad = match_ignore_ascii_case! { &ident,
        "span" | "auto" | "inherit" | "initial" | "unset" | "default" | "revert" | "revert-layer" => true,
        _ => false,
    };
    if bad {
        return Err(location.new_unexpected_token_error(Token::Ident(ident)));
    }
    Ok(Arc::from(&*ident))
}

/// `[ <custom-ident>* ]`.
fn parse_line_names<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<Arc<str>>, ParseErr<'i>> {
    input.expect_square_bracket_block()?;
    input.parse_nested_block(|i| {
        let mut names = Vec::new();
        while !i.is_exhausted() {
            names.push(parse_line_ident(i)?);
        }
        Ok(names)
    })
}

/// A non-negative length or percentage.
fn parse_nonnegative_lp<'i>(input: &mut Parser<'i, '_>) -> Result<LengthPercentage, ParseErr<'i>> {
    let location = input.current_source_location();
    let lp = parse_length_percentage(input)?;
    let negative = match &lp {
        LengthPercentage::Length(l) => l.parts().0 < 0.0,
        LengthPercentage::Percent(p) => *p < 0.0,
        LengthPercentage::Calc(_) => false,
    };
    if negative {
        return Err(location.new_custom_error(()));
    }
    Ok(lp)
}

/// `<track-breadth>`, or `<inflexible-breadth>` without `fr`.
fn parse_breadth<'i>(input: &mut Parser<'i, '_>, flexible: bool) -> Result<TrackBreadth<LengthPercentage>, ParseErr<'i>> {
    if let Ok(k) = input.try_parse(|i| {
        let ident = i.expect_ident()?.clone();
        Ok::<_, ParseErr<'i>>(match_ignore_ascii_case! { &ident,
            "min-content" => TrackBreadth::MinContent,
            "max-content" => TrackBreadth::MaxContent,
            "auto" => TrackBreadth::Auto,
            _ => return Err(i.new_custom_error(())),
        })
    }) {
        return Ok(k);
    }
    if flexible
        && let Ok(f) = input.try_parse(|i| {
            let location = i.current_source_location();
            match i.next()?.clone() {
                Token::Dimension { value, unit, .. } if unit.eq_ignore_ascii_case("fr") && value >= 0.0 => Ok(value),
                t => Err::<f32, ParseErr<'i>>(location.new_unexpected_token_error(t)),
            }
        })
    {
        return Ok(TrackBreadth::Flex(f));
    }
    Ok(TrackBreadth::Length(parse_nonnegative_lp(input)?))
}

/// `<track-size>`.
pub fn parse_track_size<'i>(input: &mut Parser<'i, '_>) -> Result<TrackSize<LengthPercentage>, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_function_matching("minmax")).is_ok() {
        return input.parse_nested_block(|i| {
            // The minimum can not be flexible.
            let min = parse_breadth(i, false)?;
            i.expect_comma()?;
            let max = parse_breadth(i, true)?;
            Ok(TrackSize::MinMax(min, max))
        });
    }
    if input.try_parse(|i| i.expect_function_matching("fit-content")).is_ok() {
        return input.parse_nested_block(|i| Ok(TrackSize::FitContent(parse_nonnegative_lp(i)?)));
    }
    Ok(TrackSize::Breadth(parse_breadth(input, true)?))
}

/// Line names and tracks (and, at the top level, `repeat()`s) up to the
/// end of the input: at least one track, no two name sets in a row.
fn parse_components<'i>(input: &mut Parser<'i, '_>, allow_repeat: bool) -> Result<Vec<TrackComponent<LengthPercentage>>, ParseErr<'i>> {
    let mut out: Vec<TrackComponent<LengthPercentage>> = Vec::new();
    let mut tracks = 0;
    while !input.is_exhausted() {
        // A `/` ends the list (the shorthands' separator).
        let state = input.state();
        if input.try_parse(|i| i.expect_delim('/')).is_ok() {
            input.reset(&state);
            break;
        }
        if let Ok(names) = input.try_parse(parse_line_names) {
            if matches!(out.last(), Some(TrackComponent::Names(_))) {
                return Err(input.new_custom_error(()));
            }
            out.push(TrackComponent::Names(names));
            continue;
        }
        if allow_repeat && input.try_parse(|i| i.expect_function_matching("repeat")).is_ok() {
            let r = input.parse_nested_block(|i| {
                let location = i.current_source_location();
                // A count of repetitions; `auto-fill`/`auto-fit` are a
                // later part of grid.
                let count = i.expect_integer()?;
                if count < 1 {
                    return Err(location.new_custom_error(()));
                }
                i.expect_comma()?;
                let items = parse_components(i, false)?;
                Ok(TrackComponent::Repeat(count as u32, items))
            })?;
            out.push(r);
            tracks += 1;
            continue;
        }
        out.push(TrackComponent::Size(parse_track_size(input)?));
        tracks += 1;
    }
    if tracks == 0 {
        return Err(input.new_custom_error(()));
    }
    Ok(out)
}

/// `none | <track-list>`.
pub fn parse_track_list<'i>(input: &mut Parser<'i, '_>) -> Result<TrackList<LengthPercentage>, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(TrackList::None);
    }
    Ok(TrackList::List(parse_components(input, true)?))
}

/// `<track-size>+`.
pub fn parse_auto_tracks<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<TrackSize<LengthPercentage>>, ParseErr<'i>> {
    let mut out = vec![parse_track_size(input)?];
    while let Ok(s) = input.try_parse(parse_track_size) {
        out.push(s);
    }
    Ok(out)
}

/// `[ row | column ] || dense`.
pub fn parse_auto_flow<'i>(input: &mut Parser<'i, '_>) -> Result<GridAutoFlow, ParseErr<'i>> {
    let mut axis: Option<bool> = None;
    let mut dense = false;
    while let Ok(ident) = input.try_parse(|i| i.expect_ident().map(|s| s.to_ascii_lowercase())) {
        match ident.as_str() {
            "row" if axis.is_none() => axis = Some(false),
            "column" if axis.is_none() => axis = Some(true),
            "dense" if !dense => dense = true,
            _ => return Err(input.new_custom_error(())),
        }
    }
    if axis.is_none() && !dense {
        return Err(input.new_custom_error(()));
    }
    Ok(GridAutoFlow {
        column: axis.unwrap_or(false),
        dense,
    })
}

/// `<grid-line> = auto | <custom-ident> | [ <integer> && <custom-ident>? ]
/// | [ span && [ <integer> || <custom-ident> ] ]` (the integer not zero,
/// positive after `span`).
pub fn parse_grid_line<'i>(input: &mut Parser<'i, '_>) -> Result<GridLine, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(GridLine::Auto);
    }
    let mut span = false;
    let mut number: Option<i32> = None;
    let mut name: Option<Arc<str>> = None;
    for _ in 0..3 {
        if !span && input.try_parse(|i| i.expect_ident_matching("span")).is_ok() {
            span = true;
            continue;
        }
        if number.is_none()
            && let Ok(n) = input.try_parse(|i| i.expect_integer())
        {
            number = Some(n);
            continue;
        }
        if name.is_none()
            && let Ok(n) = input.try_parse(parse_line_ident)
        {
            name = Some(n);
            continue;
        }
        break;
    }
    match (span, number, name) {
        (true, Some(n), name) if n > 0 => Ok(GridLine::Span(n as u32, name)),
        (true, None, Some(name)) => Ok(GridLine::Span(1, Some(name))),
        (false, Some(n), name) if n != 0 => Ok(GridLine::Line(n, name)),
        (false, None, Some(name)) => Ok(GridLine::Ident(name)),
        _ => Err(input.new_custom_error(())),
    }
}
