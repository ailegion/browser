//! Responsive images ("Working sites first" 6m; HTML §4.8.4.3): parsing
//! `srcset` and `sizes`, building and normalizing a source set (an
//! `<img>`'s own attributes or the first matching `<source>` of its
//! `<picture>`), selecting a source for the device pixel ratio, and the
//! image request algorithms that drive an `<img>`'s state
//! (`browser_dom::image`): "update the image data" (its synchronous part
//! and the part a microtask runs), "react to environment changes", and
//! what a finished fetch does. The tab and the script bindings call
//! these; the tab fetches, runs the queued tasks and lays out.

use browser_dom::image::{ImageData, ImageRequest, ImageTask, PendingKind, RequestState};
use browser_dom::{Document, NodeId};
use cssparser::{Delimiter, Parser, ParserInput, Token};
use html5ever::{local_name, ns};
use url::Url;

use crate::computed::Viewport;
use crate::media::{MediaCondition, MediaQueryList};
use crate::values::{LengthContext, LengthPercentage, parse_length_percentage};

/// One image candidate of a `srcset` (HTML §4.8.4.3.10): its URL as
/// written, and its width or pixel density descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub url: String,
    pub width: Option<u64>,
    pub density: Option<f64>,
}

fn is_ascii_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ')
}

/// HTML's "parse a srcset attribute" (§4.8.4.3.10), with its descriptor
/// tokenizer and parser; candidates in error are dropped.
pub fn parse_srcset(input: &str) -> Vec<Candidate> {
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0;
    let mut candidates = Vec::new();
    loop {
        // Splitting loop.
        while pos < chars.len() && (is_ascii_ws(chars[pos]) || chars[pos] == ',') {
            pos += 1;
        }
        if pos >= chars.len() {
            return candidates;
        }
        let start = pos;
        while pos < chars.len() && !is_ascii_ws(chars[pos]) {
            pos += 1;
        }
        let mut url: String = chars[start..pos].iter().collect();
        let mut descriptors: Vec<String> = Vec::new();
        if url.ends_with(',') {
            while url.ends_with(',') {
                url.pop();
            }
        } else {
            // Descriptor tokenizer.
            while pos < chars.len() && is_ascii_ws(chars[pos]) {
                pos += 1;
            }
            let mut current = String::new();
            #[derive(PartialEq)]
            enum State {
                InDescriptor,
                InParens,
                AfterDescriptor,
            }
            let mut state = State::InDescriptor;
            loop {
                let c = chars.get(pos).copied();
                match state {
                    State::InDescriptor => match c {
                        Some(c) if is_ascii_ws(c) => {
                            if !current.is_empty() {
                                descriptors.push(std::mem::take(&mut current));
                            }
                            state = State::AfterDescriptor;
                        }
                        Some(',') => {
                            pos += 1;
                            if !current.is_empty() {
                                descriptors.push(std::mem::take(&mut current));
                            }
                            break;
                        }
                        Some('(') => {
                            current.push('(');
                            state = State::InParens;
                        }
                        None => {
                            if !current.is_empty() {
                                descriptors.push(std::mem::take(&mut current));
                            }
                            break;
                        }
                        Some(c) => current.push(c),
                    },
                    State::InParens => match c {
                        Some(')') => {
                            current.push(')');
                            state = State::InDescriptor;
                        }
                        None => {
                            descriptors.push(std::mem::take(&mut current));
                            break;
                        }
                        Some(c) => current.push(c),
                    },
                    State::AfterDescriptor => match c {
                        Some(c) if is_ascii_ws(c) => {}
                        None => break,
                        Some(_) => {
                            // Reconsume in the descriptor state.
                            state = State::InDescriptor;
                            continue;
                        }
                    },
                }
                pos += 1;
            }
        }
        // Descriptor parser.
        let mut error = false;
        let mut width: Option<u64> = None;
        let mut density: Option<f64> = None;
        let mut future_h: Option<u64> = None;
        for d in &descriptors {
            let (body, last) = match d.char_indices().last() {
                Some((i, c)) => (&d[..i], c),
                None => {
                    error = true;
                    continue;
                }
            };
            if last == 'w' && is_non_negative_integer(body) {
                if width.is_some() || density.is_some() {
                    error = true;
                }
                match parse_non_negative_integer(body) {
                    0 => error = true,
                    n => width = Some(n),
                }
            } else if last == 'x' && is_float(body) {
                if width.is_some() || density.is_some() || future_h.is_some() {
                    error = true;
                }
                match body.parse::<f64>() {
                    Ok(v) if v >= 0.0 => density = Some(v),
                    _ => error = true,
                }
            } else if last == 'h' && is_non_negative_integer(body) {
                if future_h.is_some() || density.is_some() {
                    error = true;
                }
                match parse_non_negative_integer(body) {
                    0 => error = true,
                    n => future_h = Some(n),
                }
            } else {
                error = true;
            }
        }
        if future_h.is_some() && width.is_none() {
            error = true;
        }
        if !error {
            candidates.push(Candidate { url, width, density });
        }
    }
}

/// HTML's valid non-negative integer: one or more ASCII digits.
fn is_non_negative_integer(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_non_negative_integer(s: &str) -> u64 {
    s.parse::<u64>().unwrap_or(u64::MAX)
}

/// HTML's valid floating-point number: `-`? (digits (`.` digits)? |
/// `.` digits) ((`e`|`E`) (`-`|`+`)? digits)?.
fn is_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let has_int = i > int_start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return false;
        }
    } else if !has_int {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

/// Whether and how the `auto` keyword of `sizes` applies: the `<img>`
/// allows auto-sizes (`loading=lazy` and `sizes` starting with `auto`),
/// with its concrete object width when it is rendered and its last
/// auto-sizes width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AutoSizes {
    No,
    Allowed { rendered: Option<f32>, last: Option<f32> },
}

/// A `<source-size-value>`: a non-negative length in px, or `auto`.
enum SizeValue {
    Px(f32),
    Auto,
}

/// Lengths in `sizes` resolve as in media queries: font-relative units
/// against the initial font size, viewport units against the viewport.
fn sizes_context(vp: &Viewport) -> LengthContext {
    LengthContext {
        font_size: 16.0,
        root_font_size: 16.0,
        viewport_width: vp.width,
        viewport_height: vp.height,
    }
}

/// A component value as a `<source-size-value>`, if it is one: `auto`, or
/// a length (a math function of lengths, no percentage) not negative.
fn source_size_value(text: &str, vp: &Viewport) -> Option<SizeValue> {
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    if parser.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return parser.expect_exhausted().ok().map(|_| SizeValue::Auto);
    }
    let lp = parse_length_percentage(&mut parser).ok()?;
    parser.expect_exhausted().ok()?;
    let ctx = sizes_context(vp);
    match lp {
        LengthPercentage::Length(l) => {
            let px = l.to_px(&ctx);
            (px >= 0.0).then_some(SizeValue::Px(px))
        }
        LengthPercentage::Percent(_) => None,
        // A math function is clamped to the allowed range, not refused.
        LengthPercentage::Calc(c) => (!c.node.has_percent()).then(|| SizeValue::Px(c.node.eval(Some(&ctx), None).unwrap_or(0.0).max(0.0))),
    }
}

/// HTML's "parse a sizes attribute" (§4.8.4.3.11): the source size in
/// CSS px, and the rendered width it used for `auto` (the caller records
/// it as the last auto-sizes width).
pub fn parse_sizes(value: &str, auto: AutoSizes, vp: &Viewport) -> (f32, Option<f32>) {
    let entries = comma_entries(value);
    let mut used_rendered = None;
    for entry in &entries {
        let mut comps = entry_components(entry);
        while comps.last().is_some_and(|c| c.1) {
            comps.pop();
        }
        let Some((last, _)) = comps.pop() else { continue };
        let Some(mut size) = source_size_value(&last, vp) else { continue };
        if let (SizeValue::Auto, AutoSizes::Allowed { rendered, last }) = (&size, auto) {
            if let Some(w) = rendered {
                size = SizeValue::Px(w);
                used_rendered = Some(w);
            } else if let Some(w) = last {
                size = SizeValue::Px(w);
            }
        }
        while comps.last().is_some_and(|c| c.1) {
            comps.pop();
        }
        if comps.is_empty() {
            // A bare size (a parse error when not the last entry, used
            // all the same).
            match size {
                SizeValue::Px(px) => return (px, used_rendered),
                SizeValue::Auto => continue,
            }
        }
        let condition: String = comps.iter().map(|c| c.0.as_str()).collect();
        let mut input = ParserInput::new(&condition);
        let mut parser = Parser::new(&mut input);
        let holds = MediaCondition::parse_all(&mut parser).is_ok_and(|c| c.matches(vp));
        if !holds {
            continue;
        }
        if let SizeValue::Px(px) = size {
            return (px, used_rendered);
        }
    }
    (vp.width, used_rendered)
}

/// The component values of one comma-separated entry, as text, with
/// whether each is whitespace (a function or block is one component).
fn entry_components(entry: &str) -> Vec<(String, bool)> {
    let mut input = ParserInput::new(entry);
    let mut parser = Parser::new(&mut input);
    let mut out = Vec::new();
    loop {
        let start = parser.position();
        let (is_ws, block) = match parser.next_including_whitespace() {
            Ok(t) => (
                matches!(t, Token::WhiteSpace(_)),
                matches!(t, Token::Function(_) | Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock),
            ),
            Err(_) => break,
        };
        if block {
            // Consume the block so the component ends after it.
            let _ = parser.parse_nested_block(|i| {
                while i.next_including_whitespace_and_comments().is_ok() {}
                Ok::<_, crate::ParseErr<'_>>(())
            });
        }
        out.push((parser.slice_from(start).to_owned(), is_ws));
    }
    out
}

/// The entries of a comma-separated list of component values, as text.
fn comma_entries(value: &str) -> Vec<String> {
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    let mut out = Vec::new();
    loop {
        let start = parser.position();
        let _ = parser.parse_until_before(Delimiter::Comma, |i| {
            while i.next_including_whitespace().is_ok() {}
            Ok::<_, crate::ParseErr<'_>>(())
        });
        out.push(parser.slice_from(start).to_owned());
        if parser.next().is_err() {
            break;
        }
    }
    out
}

/// A normalized source set: each image source with its pixel density.
pub type SourceSet = Vec<(String, f64)>;

/// HTML's "create a source set" (§4.8.4.3.8) from parsed candidates and
/// a source size, with "normalize the source densities" (§4.8.4.3.12).
pub fn normalize(candidates: Vec<Candidate>, default_source: &str, source_size: f32) -> SourceSet {
    let mut set = candidates;
    if !default_source.is_empty() && !set.iter().any(|c| c.density == Some(1.0)) && !set.iter().any(|c| c.width.is_some()) {
        set.push(Candidate {
            url: default_source.to_owned(),
            width: None,
            density: None,
        });
    }
    set.into_iter()
        .map(|c| {
            let density = match (c.density, c.width) {
                (Some(d), _) => d,
                (None, Some(w)) => w as f64 / source_size as f64,
                (None, None) => 1.0,
            };
            (c.url, density)
        })
        .collect()
}

/// HTML's "select an image source from a source set" (§4.8.4.3.7): the
/// first source, by ascending density (the earliest of equal ones), whose
/// density reaches the device pixel ratio, else the densest. The
/// optional step 8 (taking a denser source already available) is not
/// taken.
pub fn select_from_set(set: &SourceSet, device_pixel_ratio: f64) -> Option<(String, f64)> {
    let mut candidates: Vec<&(String, f64)> = Vec::new();
    for entry in set {
        if !candidates.iter().any(|c| c.1 == entry.1) {
            candidates.push(entry);
        }
    }
    // A stable sort keeps the earliest first among equals (none remain).
    candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let chosen = candidates.iter().find(|c| c.1 >= device_pixel_ratio).or(candidates.last())?;
    Some(((*chosen).0.clone(), chosen.1))
}

/// Whether a `<source type>` names an image format this browser shows
/// (MIME Sniffing §4.4: the essence counts; parameters do not).
pub fn supported_image_type(value: &str) -> bool {
    let trimmed = value.trim_matches(|c: char| matches!(c, '\t' | '\n' | '\r' | ' '));
    let essence = trimmed.split(';').next().unwrap_or("").trim_end_matches(|c: char| matches!(c, '\t' | '\n' | '\r' | ' '));
    let Some((ty, sub)) = essence.split_once('/') else { return false };
    let token = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !token(ty) || !token(sub) {
        return false;
    }
    matches!(
        (ty.to_ascii_lowercase().as_str(), sub.to_ascii_lowercase().as_str()),
        ("image", "png" | "apng" | "jpeg" | "gif" | "webp" | "svg+xml")
    )
}

fn is_html(doc: &Document, id: NodeId, local: html5ever::LocalName) -> bool {
    doc.element(id).is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local)
}

fn attr(doc: &Document, id: NodeId, name: &str) -> Option<String> {
    doc.element(id).and_then(|e| e.attr(name)).map(str::to_owned)
}

/// The `<picture>` an `<img>` (or `<source>`) is a child of.
pub fn picture_parent(doc: &Document, id: NodeId) -> Option<NodeId> {
    doc.parent(id).filter(|&p| is_html(doc, p, local_name!("picture")))
}

/// HTML: an `<img>` uses `srcset` or `picture` when it has a `srcset`
/// attribute or its parent is a `<picture>`.
pub fn uses_srcset_or_picture(doc: &Document, img: NodeId) -> bool {
    attr(doc, img, "srcset").is_some() || picture_parent(doc, img).is_some()
}

/// HTML: an `<img>` allows auto-sizes when its `loading` is `lazy` and its
/// `sizes` is `auto` or starts with `auto,` (ASCII case-insensitive).
pub fn allows_auto_sizes(doc: &Document, img: NodeId) -> bool {
    let lazy = attr(doc, img, "loading").is_some_and(|l| l.eq_ignore_ascii_case("lazy"));
    let sizes = attr(doc, img, "sizes").unwrap_or_default();
    let lower = sizes.to_ascii_lowercase();
    lazy && (lower == "auto" || lower.starts_with("auto,"))
}

/// What the image algorithms need from outside the document.
#[allow(missing_debug_implementations)]
pub struct ImageEnv<'a> {
    pub viewport: &'a Viewport,
    /// The list of available images: decoded already, no fetch needed.
    pub available: &'a dyn Fn(&Url) -> bool,
    /// The concrete object width of an `<img>` that is being rendered.
    pub rendered_width: &'a dyn Fn(NodeId) -> Option<f32>,
}

/// HTML's "update the source set" (§4.8.4.3.9) for an `<img>`: its
/// normalized source set, and its dimension attribute source recorded
/// (a change listed in `images_changed`), with the last auto-sizes width.
pub fn update_source_set(doc: &mut Document, img: NodeId, env: &ImageEnv<'_>) -> SourceSet {
    let mut dimension_source = img;
    let elements: Vec<NodeId> = match picture_parent(doc, img) {
        Some(p) => doc.children(p).filter(|&c| doc.get(c).is_element()).collect(),
        None => vec![img],
    };
    let auto = if allows_auto_sizes(doc, img) {
        let data = doc.image_data(img);
        AutoSizes::Allowed {
            rendered: (env.rendered_width)(img),
            last: data.and_then(|d| d.last_auto_width),
        }
    } else {
        AutoSizes::No
    };
    let mut set = SourceSet::new();
    let mut used_rendered = None;
    for child in elements {
        if child == img {
            let srcset = attr(doc, img, "srcset").unwrap_or_default();
            let sizes = attr(doc, img, "sizes").unwrap_or_default();
            let src = attr(doc, img, "src").unwrap_or_default();
            let candidates = if srcset.is_empty() { Vec::new() } else { parse_srcset(&srcset) };
            let (size, used) = parse_sizes(&sizes, auto, env.viewport);
            used_rendered = used;
            set = normalize(candidates, &src, size);
            break;
        }
        if !is_html(doc, child, local_name!("source")) {
            continue;
        }
        let Some(srcset) = attr(doc, child, "srcset") else { continue };
        let candidates = parse_srcset(&srcset);
        if candidates.is_empty() {
            continue;
        }
        if let Some(media) = attr(doc, child, "media")
            && !MediaQueryList::parse_str(&media).evaluate(env.viewport)
        {
            continue;
        }
        // A `<source>` without `sizes` before an `<img>` that allows
        // auto-sizes stands for `auto` (HTML §4.8.2).
        let sizes = attr(doc, child, "sizes").unwrap_or_else(|| if matches!(auto, AutoSizes::Allowed { .. }) { "auto".to_owned() } else { String::new() });
        let (size, used) = parse_sizes(&sizes, auto, env.viewport);
        if let Some(ty) = attr(doc, child, "type")
            && !supported_image_type(&ty)
        {
            continue;
        }
        if attr(doc, child, "width").is_some() || attr(doc, child, "height").is_some() {
            dimension_source = child;
        }
        used_rendered = used;
        set = normalize(candidates, "", size);
        break;
    }
    let data = doc.image_data_mut(img);
    if let Some(w) = used_rendered {
        data.last_auto_width = Some(w);
    }
    if data.dimension_source != Some(dimension_source) {
        let before = data.dimension_source;
        data.dimension_source = Some(dimension_source);
        // The first time with the `<img>` itself changes nothing.
        if before.is_some() || dimension_source != img {
            doc.images_changed.push(img);
        }
    }
    set
}

/// HTML's "select an image source" (§4.8.4.3.7) for an `<img>`.
pub fn select_image_source(doc: &mut Document, img: NodeId, env: &ImageEnv<'_>) -> Option<(String, f64)> {
    let set = update_source_set(doc, img, env);
    select_from_set(&set, env.viewport.scale_factor as f64)
}

fn queue(doc: &mut Document, element: NodeId, set_current_url: Option<String>, event: Option<&'static str>) {
    doc.image_tasks.push(ImageTask { element, set_current_url, event });
}

/// The first half of HTML's "update the image data" (§4.8.4.3.5, steps
/// 2 to 9), run when the mutation happens: the source set (and so the
/// dimension attribute source) brought up to date, and an `<img>` that
/// uses neither `srcset` nor `<picture>` and whose `src` is already
/// available shown at once. Returns the run's number when the rest
/// (`update_image_data_rest`) must run in a microtask.
pub fn update_image_data(doc: &mut Document, img: NodeId, env: &ImageEnv<'_>, omit_events: bool) -> Option<u64> {
    let _ = update_source_set(doc, img, env);
    let uses = uses_srcset_or_picture(doc, img);
    let src = attr(doc, img, "src").unwrap_or_default();
    let data = doc.image_data_mut(img);
    data.generation += 1;
    data.mutations += 1;
    data.omit_events = omit_events;
    data.previous_url = data.current.current_url.clone();
    let generation = data.generation;
    let previous = data.previous_url.clone();
    // Step 6.
    let selected = (!uses && !src.is_empty()).then_some(src);
    data.last_selected = selected.clone();
    if let Some(selected) = selected
        && let Some(url) = doc.resolve_url(&selected)
        && (env.available)(&url)
    {
        let url_string = url.as_str().to_owned();
        let data = doc.image_data_mut(img);
        data.pending = None;
        data.current = ImageRequest {
            current_url: data.current.current_url.clone(),
            density: 1.0,
            state: RequestState::CompletelyAvailable,
            url: Some(url),
            deferred: false,
        };
        doc.images_changed.push(img);
        let fire = !omit_events || previous != url_string;
        queue(doc, img, Some(url_string), fire.then_some("load"));
        return None;
    }
    Some(generation)
}

/// The rest of HTML's "update the image data" (steps 10 to 20), from the
/// microtask the first half queued; a run a later one superseded
/// returns. A new request's URL goes on `image_fetches`.
pub fn update_image_data_rest(doc: &mut Document, img: NodeId, generation: u64, env: &ImageEnv<'_>) {
    if !doc.contains(img) || doc.image_data(img).is_none_or(|d| d.generation != generation) {
        return;
    }
    let selected = select_image_source(doc, img, env);
    let uses = uses_srcset_or_picture(doc, img);
    let has_src = attr(doc, img, "src").is_some();
    let deferred = allows_auto_sizes(doc, img);
    let data = doc.image_data_mut(img);
    let omit = data.omit_events;
    let previous = data.previous_url.clone();
    // The source selected is the last one, which an environment change
    // compares against (so a resize that selects the same source fetches
    // nothing and fires no event, as WPT's `viewport-change.html` and
    // `avoid-reload-on-resize.html` expect).
    data.last_selected = selected.as_ref().map(|s| s.0.clone());
    let Some((source, density)) = selected else {
        // Step 12.
        data.current.state = RequestState::Broken;
        data.current.url = None;
        data.pending = None;
        doc.images_changed.push(img);
        let fire = (has_src || uses) && (!omit || !previous.is_empty());
        queue(doc, img, Some(String::new()), fire.then_some("error"));
        return;
    };
    let Some(url) = doc.resolve_url(&source) else {
        // Step 15.
        let data = doc.image_data_mut(img);
        data.current.state = RequestState::Broken;
        data.current.url = None;
        data.pending = None;
        doc.images_changed.push(img);
        let fire = !omit || previous != source;
        queue(doc, img, Some(source), fire.then_some("error"));
        return;
    };
    let url_string = url.as_str().to_owned();
    let data = doc.image_data_mut(img);
    // Step 16.
    if data.pending.as_ref().is_some_and(|(p, _)| p.current_url == url_string) {
        return;
    }
    // Step 17.
    if data.current.current_url == url_string && data.current.state == RequestState::PartiallyAvailable {
        data.pending = None;
        return;
    }
    // Steps 18 to 20.
    data.pending = None;
    let request = ImageRequest {
        current_url: url_string,
        density,
        state: RequestState::Unavailable,
        url: Some(url.clone()),
        deferred,
    };
    if matches!(data.current.state, RequestState::Unavailable | RequestState::Broken) {
        data.current = request;
        doc.images_changed.push(img);
    } else {
        data.pending = Some((request, PendingKind::Update));
    }
    doc.image_fetches.push((url, deferred));
}

/// What happened to the document's images since the last call: each
/// `<img>` created (it updates its image data on creation, HTML
/// §4.8.4.3.1) and each that saw a relevant mutation, once, runs the first
/// half of "update the image data". Returns the runs whose rest a
/// microtask must perform.
pub fn run_image_mutations(doc: &mut Document, env: &ImageEnv<'_>) -> Vec<(NodeId, u64)> {
    let mut list = doc.take_created_images();
    for id in doc.take_image_mutations() {
        if !list.contains(&id) {
            list.push(id);
        }
    }
    list.into_iter().filter_map(|img| update_image_data(doc, img, env, false).map(|g| (img, g))).collect()
}

/// HTML's "react to environment changes" (§4.8.4.3.13) for one `<img>`:
/// when the source selected now differs from the last one, a pending
/// request for it (its URL on `image_fetches`).
pub fn react_to_environment_changes(doc: &mut Document, img: NodeId, env: &ImageEnv<'_>) {
    if !doc.contains(img) || !uses_srcset_or_picture(doc, img) {
        return;
    }
    if doc.image_data(img).is_some_and(|d| d.pending.is_some()) {
        return;
    }
    let Some((source, density)) = select_image_source(doc, img, env) else { return };
    let data = doc.image_data_mut(img);
    if data.last_selected.as_deref() == Some(source.as_str()) && data.current.density == density {
        return;
    }
    let Some(url) = doc.resolve_url(&source) else { return };
    let data = doc.image_data_mut(img);
    data.env_started_at = data.mutations;
    data.pending = Some((
        ImageRequest {
            current_url: url.as_str().to_owned(),
            density,
            state: RequestState::Unavailable,
            url: Some(url.clone()),
            deferred: false,
        },
        PendingKind::Environment { selected: source, density },
    ));
    doc.image_fetches.push((url, false));
}

/// A fetch of `url` ended: decoded (`ok`) or not (a network error, an
/// unsupported or broken image). Every `<img>` request waiting on it
/// completes as HTML's fetch steps say (§4.8.4.3.5 step 29, §4.8.4.3.13
/// step 18): a current request becomes completely available or broken; a
/// pending one of "update the image data" replaces the current one
/// either way; a pending one of an environment change replaces it only
/// when it loaded and no relevant mutation came meanwhile.
pub fn image_fetch_done(doc: &mut Document, url: &Url, ok: bool) {
    for img in doc.images_with_data() {
        let data = doc.image_data_mut(img);
        let omit = data.omit_events;
        let previous = data.previous_url.clone();
        let current_waits = data.current.state == RequestState::Unavailable && data.current.url.as_ref() == Some(url);
        let pending_waits = data.pending.as_ref().is_some_and(|(p, _)| p.url.as_ref() == Some(url));
        if current_waits {
            if ok {
                data.current.state = RequestState::CompletelyAvailable;
                let fire = !omit || previous != data.current.current_url;
                doc.images_changed.push(img);
                queue(doc, img, None, fire.then_some("load"));
            } else {
                data.current.state = RequestState::Broken;
                data.pending = None;
                doc.images_changed.push(img);
                queue(doc, img, None, Some("error"));
            }
            continue;
        }
        if !pending_waits {
            continue;
        }
        let Some((mut request, kind)) = data.pending.take() else { continue };
        match kind {
            PendingKind::Update => {
                request.state = if ok { RequestState::CompletelyAvailable } else { RequestState::Broken };
                let fire = !ok || !omit || previous != request.current_url;
                data.current = request;
                doc.images_changed.push(img);
                queue(doc, img, None, fire.then_some(if ok { "load" } else { "error" }));
            }
            PendingKind::Environment { selected, density } => {
                if !ok || data.mutations != data.env_started_at {
                    continue;
                }
                data.last_selected = Some(selected);
                request.density = density;
                request.state = RequestState::CompletelyAvailable;
                data.current = request;
                doc.images_changed.push(img);
                queue(doc, img, None, Some("load"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(width: f32, scale: f32) -> Viewport {
        Viewport {
            width,
            height: 600.0,
            scale_factor: scale,
            prefers_dark: false,
        }
    }

    /// The URL an `<img srcset>` alone selects at 1x, as WPT's
    /// `parse-a-srcset-attribute.html` checks it.
    fn chosen(srcset: &str) -> Option<String> {
        let set = normalize(parse_srcset(srcset), "", 1000.0);
        select_from_set(&set, 1.0).map(|s| s.0)
    }

    #[test]
    fn srcset_parses_as_wpt_has_it() {
        let cases: &[(&str, Option<&str>)] = &[
            ("", None),
            (",", None),
            (",,,", None),
            ("  data:,a  1x  ", Some("data:,a")),
            ("\tdata:,a 1x\t", Some("data:,a")),
            ("\ndata:,a 1x\n", Some("data:,a")),
            ("\x0Cdata:,a 1x\x0C", Some("data:,a")),
            ("\rdata:,a 1x\r", Some("data:,a")),
            ("data:,a", Some("data:,a")),
            ("data:,a ", Some("data:,a")),
            ("data:,a ,", Some("data:,a")),
            ("data:,a,", Some("data:,a")),
            ("data:,a, ", Some("data:,a")),
            ("data:,a,,,", Some("data:,a")),
            ("data:,a,, , ", Some("data:,a")),
            (" data:,a", Some("data:,a")),
            (",,,data:,a", Some("data:,a")),
            (" , ,,data:,a", Some("data:,a")),
            ("\u{a0}data:,a", Some("\u{a0}data:,a")),
            ("data:,a\u{a0}", Some("data:,a\u{a0}")),
            ("data:,a 1x", Some("data:,a")),
            ("data:,a 1x ", Some("data:,a")),
            ("data:,a 1x,", Some("data:,a")),
            ("data:,a ( , data:,b 1x, ), data:,c", Some("data:,c")),
            ("data:,a ((( , data:,b 1x, ), data:,c", Some("data:,c")),
            ("data:,a [ , data:,b 1x, ], data:,c", Some("data:,b")),
            ("data:,a { , data:,b 1x, }, data:,c", Some("data:,b")),
            ("data:,a \" , data:,b 1x, \", data:,c", Some("data:,b")),
            ("data:,a \\,data:;\\,b, data:,c", Some("data:;\\,b")),
            ("data:,a, data:,b (", Some("data:,a")),
            ("data:,a, data:,b (  ", Some("data:,a")),
            ("data:,a, data:,b (,", Some("data:,a")),
            ("data:,a, data:,b (x", Some("data:,a")),
            ("data:,a, data:,b ()", Some("data:,a")),
            ("data:,a (, data:,b", None),
            ("data:,a /*, data:,b, data:,c */", Some("data:,b")),
            ("data:,a //, data:,b", Some("data:,b")),
            ("data:,a foo", None),
            ("data:,a foo foo", None),
            ("data:,a foo 1x", None),
            ("data:,a foo 1x foo", None),
            ("data:,a foo 1w", None),
            ("data:,a foo 1w foo", None),
            ("data:,a 1x 1x", None),
            ("data:,a 1w 1w", None),
            ("data:,a 1w 1x", None),
            ("data:,a 1x 1w", None),
            ("data:,a 1w 1h", Some("data:,a")),
            ("data:,a 1h 1w", Some("data:,a")),
            ("data:,a 1h 1h", None),
            ("data:,a 1h 1x", None),
            ("data:,a 1h 1w 1x", None),
            ("data:,a 1x 1w 1h", None),
            ("data:,a 1w", Some("data:,a")),
            ("data:,a 1h", None),
            ("data:,a 1h foo", None),
            ("data:,a foo 1h", None),
            ("data:,a 0w", None),
            ("data:,a -1w", None),
            ("data:,a 1w -1w", None),
            ("data:,a 1.0w", None),
            ("data:,a 1w 1.0w", None),
            ("data:,a 1e0w", None),
            ("data:,a 1w 1e0w", None),
            ("data:,a 1www", None),
            ("data:,a 1w 1www", None),
            ("data:,a +1w", None),
            ("data:,a 1w +1w", None),
            ("data:,a 1W", None),
            ("data:,a 1w 1W", None),
            ("data:,a Infinityw", None),
            ("data:,a 1w Infinityw", None),
            ("data:,a NaNw", None),
            ("data:,a 1w NaNw", None),
            ("data:,a 0x1w", None),
            ("data:,a 0X1w", None),
            ("data:,a 0x", Some("data:,a")),
            ("data:,a -0x", Some("data:,a")),
            ("data:,a 1x -0x", None),
            ("data:,a -1x", None),
            ("data:,a 1x -1x", None),
            ("data:,a 1e0x", Some("data:,a")),
            ("data:,a 1E0x", Some("data:,a")),
            ("data:,a 1e-1x", Some("data:,a")),
            ("data:,a 1.5e1x", Some("data:,a")),
            ("data:,a -x", None),
            ("data:,a .x", None),
            ("data:,a -.x", None),
            ("data:,a 1.x", None),
            ("data:,a .5x", Some("data:,a")),
            ("data:,a .5e1x", Some("data:,a")),
            ("data:,a 1.0x", Some("data:,a")),
            ("data:,a 1x 1.5e1x", None),
            ("data:,a 1x 1e1.5x", None),
            ("data:,a 1x 1.0x", None),
            ("data:,a +1x", None),
            ("data:,a 1X", None),
            ("data:,a Infinityx", None),
            ("data:,a NaNx", None),
            ("data:,a 0x1x", None),
            ("data:,a 0X1x", None),
            ("data:,a 1w 0h", None),
            ("data:,a 1w -1h", None),
            ("data:,a 1w 1.0h", None),
            ("data:,a 1w 1e0h", None),
            ("data:,a 1w 1hhh", None),
            ("data:,a 1w +1h", None),
            ("data:,a 1w 1H", None),
            ("data:,a 1w Infinityh", None),
            ("data:,a 1w NaNh", None),
            ("data:,a 0x1h", None),
            ("data:,a 0X1h", None),
        ];
        for (input, want) in cases {
            assert_eq!(chosen(input).as_deref(), *want, "{input:?}");
        }
        // The whitespace that is not ASCII whitespace, around a descriptor.
        for ws in ["\u{1}", "\u{a0}", "\u{1680}", "\u{2000}", "\u{2005}", "\u{200a}", "\u{200c}", "\u{200d}", "\u{202f}", "\u{205f}", "\u{3000}", "\u{feff}"] {
            for d in ["1{}w", "{}1w", "1{}x", "{}1x"] {
                let s = format!("data:,a {}", d.replace("{}", ws));
                assert_eq!(chosen(&s), None, "{s:?}");
            }
            for d in ["1{}h", "{}1h"] {
                let s = format!("data:,a 1w {}", d.replace("{}", ws));
                assert_eq!(chosen(&s), None, "{s:?}");
            }
        }
    }

    #[test]
    fn selection_takes_the_first_density_reaching_the_ratio() {
        let set = |s: &str, src: &str, size: f32| normalize(parse_srcset(s), src, size);
        let pick = |s: SourceSet, dpr: f64| select_from_set(&s, dpr).map(|c| c.0);
        // Any order; the first at or above the ratio, else the densest.
        assert_eq!(pick(set("a 2x, b 1x, c 3x", "", 100.0), 1.0).as_deref(), Some("b"));
        assert_eq!(pick(set("a 2x, b 1x, c 3x", "", 100.0), 1.5).as_deref(), Some("a"));
        assert_eq!(pick(set("a 2x, b 1x, c 3x", "", 100.0), 4.0).as_deref(), Some("c"));
        // Equal densities: the earliest.
        assert_eq!(pick(set("a 1x, b 1x", "", 100.0), 1.0).as_deref(), Some("a"));
        // `src` joins as 1x unless a 1x or a width descriptor is there.
        assert_eq!(pick(set("a 2x", "s", 100.0), 1.0).as_deref(), Some("s"));
        assert_eq!(pick(set("a 1x", "s", 100.0), 1.0).as_deref(), Some("a"));
        assert_eq!(pick(set("a 200w", "s", 100.0), 1.0).as_deref(), Some("a"));
        // Widths become densities by the source size.
        assert_eq!(pick(set("a 200w, b 400w, c 800w", "", 300.0), 1.0).as_deref(), Some("b"));
        assert_eq!(pick(set("a 200w, b 400w, c 800w", "", 300.0), 2.0).as_deref(), Some("c"));
        // A zero source size makes every density infinite: the earliest.
        assert_eq!(pick(set("a 200w, b 400w", "", 0.0), 1.0).as_deref(), Some("a"));
        // 0x only as the last resort.
        assert_eq!(pick(set("a 0x, b 1x", "", 100.0), 1.0).as_deref(), Some("b"));
        assert_eq!(pick(set("a 0x", "", 100.0), 1.0).as_deref(), Some("a"));
        assert_eq!(pick(set("", "", 100.0), 1.0), None);
    }

    #[test]
    fn sizes_parse_as_html_says() {
        let v = vp(1000.0, 1.0);
        let px = |s: &str| parse_sizes(s, AutoSizes::No, &v).0;
        assert_eq!(px(""), 1000.0, "default 100vw");
        assert_eq!(px("500px"), 500.0);
        assert_eq!(px("50vw"), 500.0);
        assert_eq!(px("2em"), 32.0, "initial font size");
        assert_eq!(px("calc(50vw - 100px)"), 400.0);
        assert_eq!(px("calc(10px - 50px)"), 0.0, "a math function is clamped");
        assert_eq!(px("0"), 0.0);
        assert_eq!(px("-1px, 300px"), 300.0, "negative refused");
        assert_eq!(px("50%, 300px"), 300.0, "percentage refused");
        assert_eq!(px("calc(50% + 1px), 300px"), 300.0);
        assert_eq!(px("attr(x), 300px"), 300.0, "other functions refused");
        assert_eq!(px("(max-width: 600px) 100px, 300px"), 300.0);
        assert_eq!(px("(min-width: 600px) 100px, 300px"), 100.0);
        assert_eq!(px("(min-width: 600px) and (max-width: 900px) 100px, 300px"), 300.0);
        assert_eq!(px("not (max-width: 600px) 100px, 300px"), 100.0);
        assert_eq!(px("screen 100px, 300px"), 300.0, "a media type is not a condition");
        assert_eq!(px("(min-width: 600px) 100px 200px, 300px"), 300.0);
        assert_eq!(px("(unknown: 1) 100px, 300px"), 300.0);
        assert_eq!(px("100px, 300px"), 100.0, "a bare size is used where it stands");
        assert_eq!(px("(min-width: 600px), 300px"), 300.0);
        assert_eq!(px("  (min-width: 600px)   100px  ,  300px  "), 100.0);
        assert_eq!(px("auto, 300px"), 300.0, "auto ignored without auto-sizes");
        // `auto` with auto-sizes: the rendered width, else the last one,
        // else ignored.
        let allowed = |rendered, last| AutoSizes::Allowed { rendered, last };
        assert_eq!(parse_sizes("auto", allowed(Some(120.0), None), &v), (120.0, Some(120.0)));
        assert_eq!(parse_sizes("auto, 300px", allowed(None, Some(80.0)), &v), (80.0, None));
        assert_eq!(parse_sizes("auto, 300px", allowed(None, None), &v), (300.0, None));
        assert_eq!(parse_sizes("auto", allowed(None, None), &v), (1000.0, None));
    }

    #[test]
    fn source_types_by_their_essence() {
        for t in ["image/png", "image/jpeg", "IMAGE/WEBP", "image/gif", "image/svg+xml", "image/apng", " image/webp ", "image/webp; codecs=x", "image/webp;"] {
            assert!(supported_image_type(t), "{t}");
        }
        for t in ["image/avif", "image/jxl", "image/bmp", "", "image", "image/", "/png", "text/html", "image/jpg x"] {
            assert!(!supported_image_type(t), "{t}");
        }
    }
}

/// What `currentSrc` returns: the current request's current URL.
pub fn current_src(doc: &Document, img: NodeId) -> String {
    doc.image_data(img).map(|d| d.current.current_url.clone()).unwrap_or_default()
}

/// The image an `<img>` shows and its density, for layout: the current
/// request's URL unless it is broken.
pub fn shown_image(data: &ImageData) -> Option<(&Url, f64)> {
    if data.current.state == RequestState::Broken {
        return None;
    }
    data.current.url.as_ref().map(|u| (u, data.current.density))
}
