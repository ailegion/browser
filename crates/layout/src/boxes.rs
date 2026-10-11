//! Box tree construction: DOM + computed styles -> boxes that taffy lays
//! out, with inline content gathered into inline roots.

use std::ops::Range;
use std::sync::Arc;

use browser_dom::{Document, NodeId, NodeKind};
use browser_style::{
    ComputedStyle, Display, Float, FontVariant, ListStylePosition, ListStyleType, Rgba, StyleMap, TextTransform,
    WhiteSpace,
};
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
    /// A list item's `list-style-position: outside` marker: an inline
    /// root of its own, outside the flow (not a taffy child), laid out
    /// unbroken and placed left of the item's content edge on its first
    /// line's baseline (as browsers place the `::marker` box), so the
    /// item's lines are aligned and broken without it.
    pub marker: Option<Box<BoxNode>>,
    /// An outside marker laid out in flow because the item has nothing
    /// else to make a line (browsers give an empty item the marker's
    /// line); its fragment is moved left of the content edge by its
    /// width.
    pub hangs: bool,
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
            marker: None,
            hangs: false,
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
    /// The `text-overflow` marker (an ellipsis or the given string) laid
    /// out in the container's font, when the container asks for one and
    /// clips its overflow; set by the engine once the root's width is
    /// final. A line that overflows the root ends with it.
    pub ellipsis: Option<Ellipsis>,
    /// The list marker at the start of the text: its byte length (an
    /// image marker is an atomic at index 0 followed by a space). For an
    /// `inside` marker this root is the item's first line; for an
    /// `outside` one the root is the marker's own (`BoxNode::marker`).
    pub marker_len: usize,
}

/// The laid-out `text-overflow` marker of an inline root.
pub(crate) struct Ellipsis {
    pub text: Arc<str>,
    pub layout: parley::Layout<crate::Brush>,
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
            ellipsis: None,
            marker_len: 0,
        }
    }

    /// Whether lines of this root that overflow it end with the
    /// container's `text-overflow` marker: only when the container clips
    /// (CSS Overflow 3: `overflow` other than `visible`) and asks for one.
    pub fn text_overflow_marker(&self) -> Option<&str> {
        if matches!(self.container.overflow_x, browser_style::Overflow::Visible) {
            return None;
        }
        self.container.text_overflow.marker()
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

    /// Synthesize small caps over `ranges` of the text (sorted, disjoint
    /// byte ranges): every lowercase letter of a `font-variant:
    /// small-caps` span inside them becomes its capital in a run of its
    /// own at 0.7 times the size (Blink's ratio), the run a span of the
    /// same node. Called by the engine after shaping for the ranges whose
    /// font has no `smcp` feature, as browsers synthesize per shaped run.
    /// Span, atomic and marker offsets follow the text; the layout cache
    /// is dropped. Returns whether anything changed.
    pub(crate) fn synthesize_small_caps(&mut self, ranges: &[Range<usize>]) -> bool {
        let old = std::mem::take(&mut self.text);
        let old_spans = std::mem::take(&mut self.spans);
        // New offset of every old char boundary.
        let mut map = vec![0usize; old.len() + 1];
        let mut new = String::with_capacity(old.len() + 16);
        let mut spans = Vec::with_capacity(old_spans.len());
        let mut changed = false;
        let mut next_range = 0;
        let mut in_range = |pos: usize| {
            while next_range < ranges.len() && ranges[next_range].end <= pos {
                next_range += 1;
            }
            next_range < ranges.len() && ranges[next_range].start <= pos
        };
        let copy = |from: usize, to: usize, new: &mut String, map: &mut Vec<usize>| {
            for (i, ch) in old[from..to].char_indices() {
                map[from + i] = new.len();
                new.push(ch);
            }
        };
        let mut cursor = 0;
        for span in old_spans {
            copy(cursor, span.start, &mut new, &mut map);
            let synth = span.style.font_variant == FontVariant::SmallCaps;
            let mut small_style: Option<Arc<ComputedStyle>> = None;
            let mut run_start = new.len();
            let mut run_small = false;
            for (i, ch) in old[span.start..span.end].char_indices() {
                let o = span.start + i;
                let small = synth && ch.is_lowercase() && in_range(o);
                if small != run_small && new.len() > run_start {
                    let style = if run_small {
                        small_style.clone().unwrap_or_else(|| span.style.clone())
                    } else {
                        span.style.clone()
                    };
                    spans.push(TextSpan {
                        start: run_start,
                        end: new.len(),
                        style,
                        node: span.node,
                    });
                    run_start = new.len();
                }
                run_small = small;
                map[o] = new.len();
                if small {
                    if small_style.is_none() {
                        let mut s = (*span.style).clone();
                        s.font_size *= 0.7;
                        s.font_variant = FontVariant::Normal;
                        small_style = Some(Arc::new(s));
                    }
                    new.extend(ch.to_uppercase());
                    changed = true;
                } else {
                    new.push(ch);
                }
            }
            if new.len() > run_start {
                let style = if run_small {
                    small_style.clone().unwrap_or_else(|| span.style.clone())
                } else {
                    span.style.clone()
                };
                spans.push(TextSpan {
                    start: run_start,
                    end: new.len(),
                    style,
                    node: span.node,
                });
            }
            cursor = span.end;
        }
        copy(cursor, old.len(), &mut new, &mut map);
        map[old.len()] = new.len();
        for a in &mut self.atomics {
            a.index = map[a.index];
        }
        self.marker_len = map[self.marker_len];
        self.text = new;
        self.spans = spans;
        if changed {
            self.cache.clear();
        }
        changed
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

/// How an inline-level element joins its inline root.
enum InlineKind {
    /// A `<br>`.
    Break,
    /// Laid out on its own, placed in the line as a box.
    Atomic,
    /// Its children join the run.
    Plain,
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

    /// What an inline-level element contributes to its inline root.
    fn inline_kind(&self, id: NodeId, style: &ComputedStyle) -> InlineKind {
        let element = self.doc.element(id);
        if element.is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("br")) {
            return InlineKind::Break;
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
        if atomic { InlineKind::Atomic } else { InlineKind::Plain }
    }

    /// A `<br>`: a forced line break in the run.
    fn push_break(id: NodeId, style: Arc<ComputedStyle>, content: &mut InlineContent, ws: &mut WsState) {
        content.text.push('\n');
        content.spans.push(TextSpan {
            start: content.text.len() - 1,
            end: content.text.len(),
            style,
            node: Some(id),
        });
        ws.last_was_space = true;
        ws.at_line_start = true;
    }

    /// An element laid out on its own and placed in the line as a box:
    /// an inline-block, image or control, or a block inside an inline
    /// (laid out as an atomic so its content still shows; proper
    /// splitting is later).
    fn push_atomic(&self, id: NodeId, style: Arc<ComputedStyle>, content: &mut InlineContent, ws: &mut WsState, depth: usize) {
        let node = self.build_element(id, style, depth + 1);
        content.atomics.push(Atomic {
            index: content.text.len(),
            node,
            result: None,
        });
        ws.last_was_space = false;
        ws.at_line_start = false;
    }

    /// Gather an inline-level element into the current inline root. This
    /// recurses per nesting level (bounded by `MAX_DEPTH`), so it keeps
    /// its own frame small and leaves the per-element work to helpers.
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
        match self.inline_kind(id, &style) {
            InlineKind::Break => return Self::push_break(id, style, content, ws),
            InlineKind::Atomic => return self.push_atomic(id, style, content, ws, depth),
            InlineKind::Plain => {}
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
                        self.push_atomic(child, cs, content, ws, depth);
                    }
                }
                _ => {}
            }
        }
    }

    /// The list item's marker: the `list-style-image` when the image is
    /// known (followed by a space), else the `list-style-type`'s text.
    /// `inside` puts it at the start of the item's first line;
    /// `outside` (the default) makes it a box of its own that the engine
    /// places left of the content edge, on the first line's baseline,
    /// outside the flow (`BoxNode::marker`), or, when the item has
    /// nothing else to make a line, as the item's one line moved left by
    /// its width (`BoxNode::hangs`).
    fn add_marker(&self, id: NodeId, style: &Arc<ComputedStyle>, b: &mut BoxNode) {
        let image = style
            .list_style_image
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .and_then(|url| self.images.intrinsic_size(&url).map(|size| (url, size)));
        let text = if image.is_some() {
            " ".to_owned()
        } else {
            match style.list_style_type {
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
            }
        };
        let marker_style = Arc::new(ComputedStyle::anonymous_from(style));
        let image_box = |marker_style: Arc<ComputedStyle>| {
            image.map(|(url, intrinsic)| {
                let node = BoxNode::new(
                    BoxKind::Image {
                        url: Some(url),
                        intrinsic: Some(intrinsic),
                    },
                    None,
                    marker_style,
                );
                Atomic {
                    index: 0,
                    node,
                    result: None,
                }
            })
        };
        if style.list_style_position == ListStylePosition::Inside {
            if !b.children.first().is_some_and(BoxNode::is_inline_root) {
                let mut m = BoxNode::new(BoxKind::Inline(InlineContent::new(style.clone())), None, marker_style.clone());
                m.anonymous = true;
                b.children.insert(0, m);
            }
            let Some(first) = b.children.first_mut() else { return };
            let BoxKind::Inline(root) = &mut first.kind else { return };
            root.prepend(&text, marker_style.clone());
            if let Some(atomic) = image_box(marker_style) {
                root.atomics.insert(0, atomic);
            }
            root.marker_len = text.len();
            return;
        }
        let mut content = InlineContent::new(style.clone());
        content.text = text.clone();
        content.spans.push(TextSpan {
            start: 0,
            end: text.len(),
            style: marker_style.clone(),
            node: None,
        });
        if let Some(atomic) = image_box(marker_style.clone()) {
            content.atomics.push(atomic);
        }
        content.marker_len = text.len();
        let mut m = BoxNode::new(BoxKind::Inline(content), None, marker_style);
        m.anonymous = true;
        if b.children.is_empty() {
            m.hangs = true;
            b.children.push(m);
        } else {
            b.marker = Some(Box::new(m));
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
/// text-transform applied, recording a style span for it. Small caps are
/// not synthesized here: the engine does that per shaped run
/// (`InlineContent::synthesize_small_caps`).
fn append_text(content: &mut InlineContent, text: &str, style: Arc<ComputedStyle>, node: Option<NodeId>, ws: &mut WsState) {
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
                        push_char(content, ' ');
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
                    push_char(content, '\n');
                    ws.last_was_space = true;
                    ws.at_line_start = true;
                } else if is_space {
                    if !ws.last_was_space && !ws.at_line_start {
                        push_char(content, ' ');
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
                    push_char(content, '\n');
                    ws.at_line_start = true;
                } else if ch == '\t' {
                    // Tabs render as spaces to the next multiple of 8 columns is
                    // not tracked; four spaces is the common approximation.
                    for _ in 0..4 {
                        push_char(content, ' ');
                    }
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
        content.spans.push(TextSpan { start, end, style, node });
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

    /// Twelve-pixel squares for every URL.
    struct Squares;
    impl ImageSizes for Squares {
        fn intrinsic_size(&self, _url: &Url) -> Option<(f32, f32)> {
            Some((12.0, 12.0))
        }
    }

    #[test]
    fn small_caps_are_synthesized_over_the_given_ranges() {
        // Built as written; the engine asks for synthesis per shaped run.
        let (doc, styles) = build("<p style='font-variant: small-caps; font-size: 20px'>Hello <b>ab</b>!<img src=x.png> stra\u{df}e</p>");
        let builder = BoxBuilder { doc: &doc, styles: &styles, images: &Squares };
        let root = builder.build_root().expect("root");
        let p = &root.children[0].children[0];
        let BoxKind::Inline(c) = &p.children[0].kind else { panic!("inline root") };
        assert_eq!(c.text, "Hello ab! stra\u{df}e");
        assert_eq!(c.spans.len(), 4);
        assert_eq!(c.atomics[0].index, 9);
        let mut c = InlineContent {
            text: c.text.clone(),
            spans: c.spans.iter().map(|s| TextSpan { start: s.start, end: s.end, style: s.style.clone(), node: s.node }).collect(),
            atomics: Vec::new(),
            container: c.container.clone(),
            cache: Vec::new(),
            floats: Vec::new(),
            ellipsis: None,
            marker_len: 0,
        };
        let img_index = 9;
        c.atomics.push(Atomic {
            index: img_index,
            node: BoxNode::new(BoxKind::Image { url: None, intrinsic: None }, None, c.container.clone()),
            result: None,
        });
        // Only "Hello " and "straße" need synthesis (their runs' font has
        // no `smcp`); "ab" is left to its font. Lowercase letters become
        // capitals in runs 0.7 times the size, each a span of its node;
        // `ß` grows to "SS" and the image after it moves along.
        assert!(c.synthesize_small_caps(&[0..6, 9..17]));
        assert_eq!(c.text, "HELLO ab! STRASSE");
        let runs: Vec<(usize, usize, f32)> = c.spans.iter().map(|s| (s.start, s.end, s.style.font_size)).collect();
        assert_eq!(
            runs,
            vec![(0, 1, 20.0), (1, 5, 14.0), (5, 6, 20.0), (6, 8, 20.0), (8, 9, 20.0), (9, 10, 20.0), (10, 17, 14.0)]
        );
        assert_eq!(c.spans[1].node, c.spans[0].node, "both runs belong to the text node");
        assert_eq!(c.spans[1].style.font_variant, FontVariant::Normal, "a synthesized run is not small caps again");
        assert_eq!(c.atomics[0].index, img_index);
        assert!(c.cache.is_empty());
        // Nothing lowercase in the ranges: no change.
        assert!(!c.synthesize_small_caps(std::slice::from_ref(&(0..1))));
    }

    #[test]
    fn list_markers_hang_outside_or_sit_inside_and_images_replace_the_type() {
        let (doc, styles) = build(
            "<ul><li>one</li><li style='list-style-position: inside'>two</li>\
             <li style='list-style-image: url(http://x/m.png)'>three</li>\
             <li style='list-style-type: none; list-style-image: url(http://x/m.png)'>four</li>\
             <li style='list-style-type: none'>five</li></ul>",
        );
        let builder = BoxBuilder { doc: &doc, styles: &styles, images: &Squares };
        let root = builder.build_root().expect("root");
        let ul = &root.children[0].children[0];
        let inline = |b: &BoxNode| match &b.kind {
            BoxKind::Inline(c) => c.text.clone(),
            _ => panic!("inline root"),
        };
        let marker = |li: &BoxNode| match &li.marker {
            Some(m) => match &m.kind {
                BoxKind::Inline(c) => Some((c.text.clone(), c.marker_len, c.atomics.len())),
                _ => panic!("marker root"),
            },
            None => None,
        };
        let items: Vec<&BoxNode> = ul.children.iter().collect();
        // Outside: a root of its own, the item's text untouched.
        assert_eq!(marker(items[0]), Some(("\u{2022} ".to_owned(), 4, 0)));
        assert_eq!(inline(&items[0].children[0]), "one");
        assert!(!items[0].children[0].hangs);
        // Inside: at the start of the item's first line.
        assert!(items[1].marker.is_none());
        let BoxKind::Inline(two) = &items[1].children[0].kind else { panic!("inline root") };
        assert_eq!((two.text.as_str(), two.marker_len), ("\u{2022} two", 4));
        // An image marker: the atomic at index 0, then a space.
        assert_eq!(marker(items[2]), Some((" ".to_owned(), 1, 1)));
        let Some(m) = &items[2].marker else { panic!("marker") };
        let BoxKind::Inline(c) = &m.kind else { panic!("marker root") };
        assert!(matches!(c.atomics.first(), Some(a) if a.index == 0 && matches!(a.node.kind, BoxKind::Image { intrinsic: Some((12.0, 12.0)), .. })));
        // The image wins over `none`; `none` without an image: no marker.
        assert_eq!(marker(items[3]).map(|m| m.1), Some(1));
        assert!(marker(items[4]).is_none());
        assert_eq!(inline(&items[4].children[0]), "five");
    }

    #[test]
    fn an_empty_item_gets_its_marker_as_its_one_line() {
        let (doc, styles) = build("<ul><li></li><li><div></div></li></ul>");
        let builder = BoxBuilder { doc: &doc, styles: &styles, images: &() };
        let root = builder.build_root().expect("root");
        let ul = &root.children[0].children[0];
        // Nothing else to make a line: the marker root is in flow, hanging.
        let empty = &ul.children[0];
        assert!(empty.marker.is_none());
        assert_eq!(empty.children.len(), 1);
        assert!(empty.children[0].hangs && empty.children[0].is_inline_root());
        // A block child, even an empty one: the marker is out of flow.
        let with_div = &ul.children[1];
        assert!(with_div.marker.is_some());
        assert_eq!(with_div.children.len(), 1);
        assert!(!with_div.children[0].hangs);
    }

    fn inline_text(b: &BoxNode) -> Vec<String> {
        let mut out = Vec::new();
        if let BoxKind::Inline(c) = &b.kind {
            out.push(c.text.clone());
        }
        if let Some(m) = &b.marker {
            out.extend(inline_text(m));
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
        assert_eq!(texts, vec!["3. ", "a", "7. ", "b", "8. ", "c", "\u{2022} ", "d"]);
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
