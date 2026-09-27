//! Box tree construction: DOM + computed styles -> boxes that taffy lays
//! out, with inline content gathered into inline roots.

use std::sync::Arc;

use browser_dom::{Document, NodeId, NodeKind};
use browser_style::{ComputedStyle, Display, Float, ListStyleType, Rgba, StyleMap, TextTransform, WhiteSpace};
use html5ever::{local_name, ns};
use url::Url;

use crate::{Control, Fragment, ImageSizes};

pub(crate) type TaffyId = taffy::NodeId;

pub(crate) enum BoxKind {
    Block,
    Flex,
    /// Inline formatting context; laid out by parley.
    Inline(InlineContent),
    /// Replaced image with an optional intrinsic size.
    Image {
        url: Option<Url>,
        intrinsic: Option<(f32, f32)>,
    },
}

pub(crate) struct BoxNode {
    pub kind: BoxKind,
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub children: Vec<BoxNode>,
    /// True for boxes the engine invented (anonymous inline roots, markers).
    pub anonymous: bool,
    pub taffy: Option<TaffyId>,
    /// A form control with its own drawing.
    pub control: Option<Control>,
}

impl BoxNode {
    fn new(kind: BoxKind, node: Option<NodeId>, style: Arc<ComputedStyle>) -> Self {
        Self {
            kind,
            node,
            style,
            children: Vec::new(),
            anonymous: false,
            taffy: None,
            control: None,
        }
    }

    pub fn is_inline_root(&self) -> bool {
        matches!(self.kind, BoxKind::Inline(_))
    }
}

/// A styled range of the inline root's text.
pub(crate) struct TextSpan {
    pub start: usize,
    pub end: usize,
    pub style: Arc<ComputedStyle>,
    pub node: Option<NodeId>,
}

/// An inline-level box that is laid out on its own and placed in the line
/// as an opaque rectangle (inline-block, image, form control).
pub(crate) struct Atomic {
    /// Byte offset in the text where the box sits.
    pub index: usize,
    pub node: BoxNode,
    /// Filled by the engine before the containing inline root is measured.
    pub result: Option<AtomicResult>,
}

pub(crate) struct AtomicResult {
    pub width: f32,
    pub height: f32,
    pub fragment: Fragment,
}

/// Cached parley layout for one width constraint.
pub(crate) struct InlineLayout {
    pub max_width: Option<f32>,
    pub layout: parley::Layout<crate::Brush>,
    /// Height of the laid-out lines, including gaps left when a line was
    /// pushed below a float. Equals `layout.height()` when there are none.
    pub height: f32,
}

/// The vertical band a float occupies next to an inline root, in the root's
/// own coordinates (origin at the root's top-left). Lines that overlap the
/// band vertically must keep clear of `edge` on the float's side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FloatBand {
    pub top: f32,
    pub bottom: f32,
    pub side: Float,
    /// For a left float, the x of its right margin edge; for a right float,
    /// the x of its left margin edge.
    pub edge: f32,
}

pub(crate) struct InlineContent {
    pub text: String,
    pub spans: Vec<TextSpan>,
    pub atomics: Vec<Atomic>,
    /// Style of the block container: alignment, white-space, defaults.
    pub container: Arc<ComputedStyle>,
    pub cache: Vec<InlineLayout>,
    /// Floats this root's lines must avoid; filled by the engine after the
    /// first layout pass, empty on float-free pages.
    pub floats: Vec<FloatBand>,
}

impl InlineContent {
    fn new(container: Arc<ComputedStyle>) -> Self {
        Self {
            text: String::new(),
            spans: Vec::new(),
            atomics: Vec::new(),
            container,
            cache: Vec::new(),
            floats: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.atomics.is_empty() && self.text.trim().is_empty()
    }

    /// Remove a collapsible space at the end of the run (CSS Text 3, step
    /// 4 of the white space processing rules).
    fn trim_trailing_space(&mut self) {
        if !self.text.ends_with(' ') {
            return;
        }
        let end = self.text.len();
        // Only if no atomic box sits after the space.
        if self.atomics.iter().any(|a| a.index >= end) {
            return;
        }
        self.text.pop();
        let new_end = self.text.len();
        if let Some(last) = self.spans.last_mut() {
            last.end = last.end.min(new_end);
            if last.end <= last.start {
                self.spans.pop();
            }
        }
    }

    fn prepend(&mut self, text: &str, style: Arc<ComputedStyle>) {
        let n = text.len();
        for s in &mut self.spans {
            s.start += n;
            s.end += n;
        }
        for a in &mut self.atomics {
            a.index += n;
        }
        self.text.insert_str(0, text);
        self.spans.insert(
            0,
            TextSpan {
                start: 0,
                end: n,
                style,
                node: None,
            },
        );
    }
}

/// The controls whose boxes are built specially.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Check { radio: bool },
    Text { password: bool },
    ButtonInput,
    Select,
}

/// Whitespace collapsing state while gathering one inline root.
struct WsState {
    /// The last emitted character was a collapsible space.
    last_was_space: bool,
    /// At the start of the root or right after a forced break.
    at_line_start: bool,
}

pub(crate) struct BoxBuilder<'a> {
    pub doc: &'a Document,
    pub styles: &'a StyleMap,
    pub images: &'a dyn ImageSizes,
}

/// Element nesting deeper than this is flattened: a stack overflow aborts
/// the process, which no page may be able to cause (plan D01). Real pages
/// stay well under this; only hostile or broken markup reaches it.
const MAX_DEPTH: usize = 400;

impl BoxBuilder<'_> {
    /// Build the box for the root element, if there is one that is displayed.
    pub fn build_root(&self) -> Option<BoxNode> {
        let root = self.doc.document_element()?;
        let style = self.styles.get(root)?.clone();
        if style.display.is_none() {
            return None;
        }
        Some(self.build_element(root, style, 0))
    }

    fn style_of(&self, id: NodeId) -> Option<Arc<ComputedStyle>> {
        self.styles.get(id).cloned()
    }

    fn is_image(&self, id: NodeId) -> bool {
        self.doc
            .element(id)
            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("img"))
    }

    /// What kind of form control an element is, if any.
    fn control_kind(&self, id: NodeId) -> Option<ControlKind> {
        let e = self.doc.element(id)?;
        if e.name.ns != ns!(html) {
            return None;
        }
        match e.name.local {
            local_name!("input") => {
                let kind = e.attr("type").map(|t| t.trim().to_ascii_lowercase()).unwrap_or_default();
                Some(match kind.as_str() {
                    "checkbox" => ControlKind::Check { radio: false },
                    "radio" => ControlKind::Check { radio: true },
                    "submit" | "button" | "reset" | "image" | "file" => ControlKind::ButtonInput,
                    _ => ControlKind::Text {
                        password: kind == "password",
                    },
                })
            }
            local_name!("select") => Some(ControlKind::Select),
            _ => None,
        }
    }

    /// A control's box with its contents made up here: a text input shows
    /// its value or placeholder, a button input its label, a select its
    /// chosen option; checkboxes and radios are drawn by the painter.
    fn build_control(&self, id: NodeId, style: Arc<ComputedStyle>, kind: ControlKind) -> BoxNode {
        let attr = |name: &str| self.doc.element(id).and_then(|e| e.attr(name));
        match kind {
            ControlKind::Check { radio } => {
                let checked = attr("checked").is_some();
                let mut b = BoxNode::new(BoxKind::Block, Some(id), style);
                b.control = Some(if radio {
                    Control::Radio { checked }
                } else {
                    Control::Checkbox { checked }
                });
                b
            }
            ControlKind::Text { password } => {
                let value = attr("value").unwrap_or("");
                let (text, dim) = if value.is_empty() {
                    (attr("placeholder").unwrap_or("").to_owned(), true)
                } else if password {
                    ("\u{2022}".repeat(value.chars().count()), false)
                } else {
                    (value.to_owned(), false)
                };
                self.control_box(id, style, &text, dim, None)
            }
            ControlKind::ButtonInput => {
                let kind = attr("type").map(|t| t.trim().to_ascii_lowercase()).unwrap_or_default();
                let label = match attr("value") {
                    Some(v) => v.to_owned(),
                    None => match kind.as_str() {
                        "submit" => "Submit".to_owned(),
                        "reset" => "Reset".to_owned(),
                        "file" => "Choose file".to_owned(),
                        _ => String::new(),
                    },
                };
                self.control_box(id, style, &label, false, None)
            }
            ControlKind::Select => {
                let options: Vec<NodeId> = self
                    .doc
                    .descendants(id)
                    .filter(|&n| {
                        self.doc
                            .element(n)
                            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("option"))
                    })
                    .collect();
                let chosen = options
                    .iter()
                    .find(|&&o| self.doc.element(o).is_some_and(|e| e.attr("selected").is_some()))
                    .or(options.first());
                let text = chosen.map(|&o| self.doc.text_content(o)).unwrap_or_default();
                self.control_box(id, style, text.trim(), false, Some(Control::Select))
            }
        }
    }

    /// A block box for a control with one line of made-up text inside,
    /// kept on one line with spaces intact so caret offsets match the
    /// value. `dim` draws it as a placeholder.
    fn control_box(&self, id: NodeId, style: Arc<ComputedStyle>, text: &str, dim: bool, control: Option<Control>) -> BoxNode {
        let mut b = BoxNode::new(BoxKind::Block, Some(id), style.clone());
        b.control = control;
        if text.is_empty() {
            return b;
        }
        let mut container = ComputedStyle::anonymous_from(&style);
        container.white_space = if control == Some(Control::Select) {
            WhiteSpace::Nowrap
        } else {
            WhiteSpace::Pre
        };
        let container = Arc::new(container);
        let mut text_style = (*container).clone();
        if dim {
            text_style.color = Rgba {
                r: 0.46,
                g: 0.46,
                b: 0.46,
                a: 1.0,
            };
        }
        let mut content = InlineContent::new(container.clone());
        let mut ws = WsState {
            last_was_space: false,
            at_line_start: true,
        };
        append_text(&mut content, text, Arc::new(text_style), Some(id), &mut ws);
        let mut inline = BoxNode::new(BoxKind::Inline(content), None, container);
        inline.anonymous = true;
        b.children.push(inline);
        b
    }

    /// Build a block-level or atomic box for an element.
    fn build_element(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> BoxNode {
        if let Some(kind) = self.control_kind(id) {
            return self.build_control(id, style, kind);
        }
        if self.is_image(id) {
            let url = self
                .doc
                .element(id)
                .and_then(|e| e.attr("src"))
                .and_then(|src| self.doc.resolve_url(src));
            let intrinsic = url.as_ref().and_then(|u| self.images.intrinsic_size(u));
            return BoxNode::new(BoxKind::Image { url, intrinsic }, Some(id), style);
        }

        let kind = match style.display {
            Display::Flex | Display::InlineFlex => BoxKind::Flex,
            _ => BoxKind::Block,
        };
        let mut b = BoxNode::new(kind, Some(id), style.clone());
        b.children = if depth < MAX_DEPTH {
            self.build_children(id, &style, depth + 1)
        } else {
            // Too deep: keep the text, drop the structure.
            let mut content = InlineContent::new(style.clone());
            let mut ws = WsState {
                last_was_space: false,
                at_line_start: true,
            };
            append_text(&mut content, &self.doc.text_content(id), style.clone(), None, &mut ws);
            let mut inline = BoxNode::new(BoxKind::Inline(content), None, Arc::new(ComputedStyle::anonymous_from(&style)));
            inline.anonymous = true;
            vec![inline]
        };

        if style.display == Display::ListItem {
            self.add_marker(id, &style, &mut b);
        }
        b
    }

    /// Children of a block or flex container: block-level children become
    /// boxes; runs of inline-level content become anonymous inline roots.
    fn build_children(&self, id: NodeId, container: &Arc<ComputedStyle>, depth: usize) -> Vec<BoxNode> {
        let mut out = Vec::new();
        let mut pending: Option<InlineContent> = None;
        let mut ws = WsState {
            last_was_space: false,
            at_line_start: true,
        };

        let is_flex = matches!(container.display, Display::Flex | Display::InlineFlex);

        let flush = |pending: &mut Option<InlineContent>, out: &mut Vec<BoxNode>, ws: &mut WsState| {
            if let Some(mut content) = pending.take()
                && !content.is_empty()
            {
                if ws.last_was_space {
                    content.trim_trailing_space();
                }
                let style = Arc::new(ComputedStyle::anonymous_from(&content.container));
                let mut b = BoxNode::new(BoxKind::Inline(content), None, style);
                b.anonymous = true;
                out.push(b);
            }
            ws.last_was_space = false;
            ws.at_line_start = true;
        };

        for child in self.doc.children(id) {
            match &self.doc.get(child).kind {
                NodeKind::Text(text) => {
                    let content = pending.get_or_insert_with(|| InlineContent::new(container.clone()));
                    append_text(content, text, container.clone(), Some(child), &mut ws);
                }
                NodeKind::Element(_) => {
                    let Some(style) = self.style_of(child) else { continue };
                    if style.display.is_none() {
                        continue;
                    }
                    // Children of a flex container are blockified: each
                    // element is its own flex item.
                    if is_inline_level(&style) && !is_flex {
                        let content = pending.get_or_insert_with(|| InlineContent::new(container.clone()));
                        self.collect_inline(child, style, content, &mut ws, depth);
                    } else {
                        flush(&mut pending, &mut out, &mut ws);
                        out.push(self.build_element(child, style, depth));
                    }
                }
                _ => {}
            }
        }
        flush(&mut pending, &mut out, &mut ws);
        out
    }

    /// Gather an inline-level element into the current inline root.
    fn collect_inline(
        &self,
        id: NodeId,
        style: Arc<ComputedStyle>,
        content: &mut InlineContent,
        ws: &mut WsState,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            append_text(content, &self.doc.text_content(id), style, Some(id), ws);
            return;
        }
        let element = self.doc.element(id);
        let is_br = element.is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("br"));
        if is_br {
            content.text.push('\n');
            content.spans.push(TextSpan {
                start: content.text.len() - 1,
                end: content.text.len(),
                style,
                node: Some(id),
            });
            ws.last_was_space = true;
            ws.at_line_start = true;
            return;
        }

        let atomic = self.is_image(id)
            || matches!(style.display, Display::InlineBlock | Display::InlineFlex)
            || element.is_some_and(|e| {
                e.name.ns == ns!(html)
                    && matches!(
                        e.name.local,
                        local_name!("input")
                            | local_name!("button")
                            | local_name!("select")
                            | local_name!("textarea")
                            | local_name!("video")
                            | local_name!("canvas")
                            | local_name!("iframe")
                            | local_name!("svg")
                    )
            });
        if atomic {
            let node = self.build_element(id, style, depth + 1);
            content.atomics.push(Atomic {
                index: content.text.len(),
                node,
                result: None,
            });
            ws.last_was_space = false;
            ws.at_line_start = false;
            return;
        }

        // A plain inline element: its text joins the run with its own style.
        for child in self.doc.children(id) {
            match &self.doc.get(child).kind {
                NodeKind::Text(text) => {
                    append_text(content, text, style.clone(), Some(child), ws);
                }
                NodeKind::Element(_) => {
                    let Some(cs) = self.style_of(child) else { continue };
                    if cs.display.is_none() {
                        continue;
                    }
                    if is_inline_level(&cs) {
                        self.collect_inline(child, cs, content, ws, depth + 1);
                    } else {
                        // Block inside inline: lay it out as an atomic box so
                        // its content still shows. Proper splitting is later.
                        let node = self.build_element(child, cs, depth + 1);
                        content.atomics.push(Atomic {
                            index: content.text.len(),
                            node,
                            result: None,
                        });
                        ws.last_was_space = false;
                        ws.at_line_start = false;
                    }
                }
                _ => {}
            }
        }
    }

    fn add_marker(&self, id: NodeId, style: &Arc<ComputedStyle>, b: &mut BoxNode) {
        let marker = match style.list_style_type {
            ListStyleType::None => return,
            ListStyleType::Disc => "\u{2022} ".to_owned(),
            ListStyleType::Circle => "\u{25E6} ".to_owned(),
            ListStyleType::Square => "\u{25AA} ".to_owned(),
            numbered => {
                let n = self.list_index(id);
                match numbered {
                    ListStyleType::LowerAlpha => format!("{}. ", alpha(n, false)),
                    ListStyleType::UpperAlpha => format!("{}. ", alpha(n, true)),
                    ListStyleType::LowerRoman => format!("{}. ", roman(n).to_ascii_lowercase()),
                    ListStyleType::UpperRoman => format!("{}. ", roman(n)),
                    _ => format!("{n}. "),
                }
            }
        };
        let marker_style = Arc::new(ComputedStyle::anonymous_from(style));
        match b.children.first_mut() {
            Some(first) if first.is_inline_root() => {
                if let BoxKind::Inline(content) = &mut first.kind {
                    content.prepend(&marker, marker_style);
                }
            }
            _ => {
                let mut content = InlineContent::new(style.clone());
                content.prepend(&marker, marker_style.clone());
                let mut m = BoxNode::new(BoxKind::Inline(content), None, marker_style);
                m.anonymous = true;
                b.children.insert(0, m);
            }
        }
    }

    /// 1-based position among list-item siblings, honoring `<ol start>` and
    /// `<li value>`.
    fn list_index(&self, id: NodeId) -> i64 {
        let mut n: i64 = 0;
        let mut explicit: Option<i64> = None;
        if let Some(parent) = self.doc.parent(id) {
            if let Some(start) = self.doc.element(parent).and_then(|e| e.attr("start")).and_then(|s| s.trim().parse::<i64>().ok()) {
                n = start - 1;
            }
            for sib in self.doc.children(parent) {
                let is_item = self.styles.get(sib).is_some_and(|s| s.display == Display::ListItem);
                if !is_item {
                    continue;
                }
                if let Some(v) = self.doc.element(sib).and_then(|e| e.attr("value")).and_then(|s| s.trim().parse::<i64>().ok()) {
                    explicit = Some(v);
                    n = v;
                } else {
                    n += 1;
                }
                if sib == id {
                    return explicit.unwrap_or(n);
                }
                explicit = None;
            }
        }
        n.max(1)
    }
}

fn is_inline_level(style: &ComputedStyle) -> bool {
    matches!(
        style.display,
        Display::Inline | Display::InlineBlock | Display::InlineFlex | Display::Contents
    )
}

/// Append a text node's content with CSS white-space processing and
/// text-transform applied, recording a style span for it.
fn append_text(
    content: &mut InlineContent,
    text: &str,
    style: Arc<ComputedStyle>,
    node: Option<NodeId>,
    ws: &mut WsState,
) {
    let start = content.text.len();
    let mode = style.white_space;
    let transform = style.text_transform;
    let mut capitalize_next = true;

    let mut push_char = |content: &mut InlineContent, ch: char| {
        match transform {
            TextTransform::None => content.text.push(ch),
            TextTransform::Uppercase => content.text.extend(ch.to_uppercase()),
            TextTransform::Lowercase => content.text.extend(ch.to_lowercase()),
            TextTransform::Capitalize => {
                if capitalize_next && ch.is_alphanumeric() {
                    content.text.extend(ch.to_uppercase());
                } else {
                    content.text.push(ch);
                }
                capitalize_next = !ch.is_alphanumeric();
            }
        }
    };

    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let is_newline = ch == '\n' || ch == '\r';
        if ch == '\r' && chars.peek() == Some(&'\n') {
            continue;
        }
        let is_space = matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{c}');
        match mode {
            WhiteSpace::Normal | WhiteSpace::Nowrap => {
                if is_space {
                    if !ws.last_was_space && !ws.at_line_start {
                        content.text.push(' ');
                        ws.last_was_space = true;
                    }
                } else {
                    push_char(content, ch);
                    ws.last_was_space = false;
                    ws.at_line_start = false;
                }
            }
            WhiteSpace::PreLine => {
                if is_newline {
                    // Drop a collapsible space before the break.
                    if ws.last_was_space && content.text.ends_with(' ') && content.text.len() > start {
                        content.text.pop();
                    }
                    content.text.push('\n');
                    ws.last_was_space = true;
                    ws.at_line_start = true;
                } else if is_space {
                    if !ws.last_was_space && !ws.at_line_start {
                        content.text.push(' ');
                        ws.last_was_space = true;
                    }
                } else {
                    push_char(content, ch);
                    ws.last_was_space = false;
                    ws.at_line_start = false;
                }
            }
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces => {
                if is_newline {
                    content.text.push('\n');
                    ws.at_line_start = true;
                } else if ch == '\t' {
                    // Tabs render as spaces to the next multiple of 8 columns is
                    // not tracked; four spaces is the common approximation.
                    content.text.push_str("    ");
                    ws.at_line_start = false;
                } else {
                    push_char(content, ch);
                    ws.at_line_start = false;
                }
                ws.last_was_space = is_space;
            }
        }
    }

    let end = content.text.len();
    if end > start {
        content.spans.push(TextSpan {
            start,
            end,
            style,
            node,
        });
    }
}

fn alpha(n: i64, upper: bool) -> String {
    let mut n = n.max(1) as u64;
    let mut s = Vec::new();
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        s.push(if upper { b'A' + r } else { b'a' + r });
        n = (n - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

fn roman(n: i64) -> String {
    if !(1..4000).contains(&n) {
        return n.to_string();
    }
    let mut n = n;
    let mut out = String::new();
    for (v, s) in [
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
        (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
    ] {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_style::{Stylist, Viewport, compute_styles, ua::ua_stylesheet};

    fn build(html: &str) -> (Document, StyleMap) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        (doc, styles)
    }

    fn inline_text(b: &BoxNode) -> Vec<String> {
        let mut out = Vec::new();
        if let BoxKind::Inline(c) = &b.kind {
            out.push(c.text.clone());
        }
        for c in &b.children {
            out.extend(inline_text(c));
        }
        out
    }

    #[test]
    fn collapses_whitespace_and_splits_blocks() {
        let (doc, styles) = build("<body>  Hello   <b>big</b>\n world <div>block</div> tail  </body>");
        let builder = BoxBuilder { doc: &doc, styles: &styles, images: &() };
        let root = builder.build_root().unwrap();
        let texts = inline_text(&root);
        assert_eq!(texts, vec!["Hello big world", "block", "tail"]);
    }

    #[test]
    fn pre_keeps_newlines_and_br_breaks() {
        let (doc, styles) = build("<pre>a\n  b</pre><p>x<br>y</p>");
        let builder = BoxBuilder { doc: &doc, styles: &styles, images: &() };
        let root = builder.build_root().unwrap();
        let texts = inline_text(&root);
        assert_eq!(texts, vec!["a\n  b", "x\ny"]);
    }

    #[test]
    fn list_markers() {
        let (doc, styles) = build("<ol start=3><li>a<li value=7>b<li>c</ol><ul><li>d</ul>");
        let builder = BoxBuilder { doc: &doc, styles: &styles, images: &() };
        let root = builder.build_root().unwrap();
        let texts = inline_text(&root);
        assert_eq!(texts, vec!["3. a", "7. b", "8. c", "\u{2022} d"]);
    }

    #[test]
    fn controls_get_their_contents_made_up() {
        let (doc, styles) = build(
            "<body><input value='hello'><input placeholder='Type here'><input type=password value='abc'>\
             <input type=checkbox checked><input type=radio><select><option>One<option selected>Two</select>\
             <input type=submit><input type=button value='Go'><textarea>text\nhere</textarea><button>Push</button></body>",
        );
        let builder = BoxBuilder { doc: &doc, styles: &styles, images: &() };
        let root = builder.build_root().expect("root");
        // Controls are inline-level, so they sit in the body's inline
        // root as atomics; visit those too.
        fn visit(b: &BoxNode, f: &mut impl FnMut(&BoxNode)) {
            f(b);
            if let BoxKind::Inline(c) = &b.kind {
                for a in &c.atomics {
                    visit(&a.node, f);
                }
            }
            for c in &b.children {
                visit(c, f);
            }
        }
        let mut texts = Vec::new();
        let mut found = Vec::new();
        let mut owners = Vec::new();
        visit(&root, &mut |b| {
            found.extend(b.control);
            if let BoxKind::Inline(c) = &b.kind {
                if !c.text.trim().is_empty() {
                    texts.push(c.text.clone());
                }
                // The made-up text belongs to the control, for hit testing
                // and the caret.
                owners.extend(
                    c.spans
                        .iter()
                        .filter(|s| !c.text[s.start..s.end].trim().is_empty())
                        .map(|s| s.node),
                );
            }
        });
        assert_eq!(
            texts,
            vec!["hello", "Type here", "\u{2022}\u{2022}\u{2022}", "Two", "Submit", "Go", "text\nhere", "Push"]
        );
        assert_eq!(
            found,
            vec![Control::Checkbox { checked: true }, Control::Radio { checked: false }, Control::Select]
        );
        let inputs: Vec<NodeId> = doc
            .descendants(doc.root())
            .filter(|&n| doc.element(n).is_some_and(|e| matches!(&*e.name.local, "input" | "select")))
            .collect();
        assert_eq!(owners[0], Some(inputs[0]));
        assert_eq!(owners[3], Some(inputs[5]), "the select's text is the select's");
    }

    #[test]
    fn roman_and_alpha() {
        assert_eq!(roman(1994), "MCMXCIV");
        assert_eq!(alpha(1, false), "a");
        assert_eq!(alpha(27, true), "AA");
    }
}
