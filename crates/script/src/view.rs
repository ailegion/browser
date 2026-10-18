//! Geometry and the viewport (Phase 3 item 3.3.4, block 1): what a
//! script sees of layout (`getBoundingClientRect`, `offset*`,
//! `client*`, `scroll*`), the window's size and scroll position, the
//! screen, `visualViewport` and `matchMedia`.
//!
//! The tab lends the layout machinery with the document (`View`): the
//! engine, the computed styles, the layout tree, the loaded external
//! sheets and the image sizes. A geometry read after the script changed
//! the tree recomputes style and layout on the spot, as browsers force a
//! synchronous layout, including a `<style>` the script just added; the
//! tab takes the fresh result back so it is not computed twice. A
//! scroll a script asks for is applied by the tab after the script, as
//! the document's scroll position belongs to it.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::ObjectInitializer;
use boa_engine::property::{Attribute, PropertyDescriptor};
use boa_engine::{
    Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string,
};
use boa_gc::{Finalize, Trace};
use browser_dom::{Document, NodeId};
use browser_layout::{
    Fragment, IDENTITY, ImageSizes, LayoutEngine, LayoutTree, Rect, Space, affine_bounds, affine_invert, affine_mul,
};
use browser_style::media::MediaQueryList;
use browser_style::ua::ua_stylesheet;
use browser_style::{
    ElementStates, FeatureChange, InteractionDeps, Origin, Position, Rule, Sides, StyleMap, Stylesheet, Stylist, Viewport,
    compute_styles_with, restyle_mutations,
};
use html5ever::{local_name, ns};
use url::Url;

use crate::dom::{Dom, DomChange, DomNode, dom, illegal_invocation, wrap};
use crate::events::{EventTargetRef, PlainTarget, UiClass, UiEventInit, dispatch, new_event};
use crate::{getter, host_state, js_str, setter};

/// Nested `@import` depth followed, as the tab's loader.
const MAX_IMPORT_DEPTH: u8 = 6;

/// What a run of DOM attribute changes means for styles: the features
/// each element changed, for `restyle_mutations`, and whether an attribute
/// layout reads itself changed (an image's `src`, a cell's `colspan`, a
/// control's `value`: anything but `class`, `id`, `style`, `title`,
/// `role`, `tabindex`, `data-*` and `aria-*`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureDelta {
    pub changes: Vec<FeatureChange>,
    pub layout: bool,
}

/// The changes after generation `since` (all of them when `None`) as a
/// feature delta; `None` when one cannot be targeted and the whole
/// document must be restyled: a tree change, or an attribute of a
/// `<style>`, `<link>` or `<base>` (the sheets or the base URL), or of a
/// `<table>` whose attributes style its cells (`border`, `cellpadding`,
/// `rules`, `frame`).
pub fn feature_changes(doc: &Document, changes: &[DomChange], since: Option<u64>) -> Option<FeatureDelta> {
    use browser_style::Feature;
    let mut delta = FeatureDelta::default();
    for change in changes {
        let (element, name, old) = match change {
            DomChange::Tree { generation } if since.is_none_or(|s| *generation > s) => return None,
            DomChange::Tree { .. } => continue,
            DomChange::Attr { generation, .. } if since.is_some_and(|s| *generation <= s) => continue,
            DomChange::Attr { element, name, old, .. } => (*element, name.as_str(), old.as_deref()),
        };
        let Some(e) = doc.element(element) else { continue };
        if e.name.ns == ns!(html) {
            let tag = &e.name.local;
            if *tag == local_name!("style") || *tag == local_name!("link") || *tag == local_name!("base") {
                return None;
            }
            if *tag == local_name!("table") && matches!(name, "border" | "cellpadding" | "rules" | "frame") {
                return None;
            }
        }
        let new = e.attr(name);
        if new == old {
            continue;
        }
        let mut features = vec![Feature::Attr(name.to_owned())];
        match name {
            "class" => {
                let before: Vec<&str> = old.unwrap_or("").split_ascii_whitespace().collect();
                let after: Vec<&str> = new.unwrap_or("").split_ascii_whitespace().collect();
                for c in before.iter().filter(|c| !after.contains(c)).chain(after.iter().filter(|c| !before.contains(c))) {
                    features.push(Feature::Class((*c).to_owned()));
                }
            }
            "id" => {
                features.extend(old.map(|v| Feature::Id(v.to_owned())));
                features.extend(new.map(|v| Feature::Id(v.to_owned())));
            }
            _ => {}
        }
        let style_only = matches!(name, "class" | "id" | "style" | "title" | "role" | "tabindex")
            || name.starts_with("data-")
            || name.starts_with("aria-");
        delta.layout |= !style_only;
        match delta.changes.iter_mut().find(|c| c.element == element) {
            Some(c) => c.features.extend(features),
            None => delta.changes.push(FeatureChange { element, features }),
        }
    }
    Some(delta)
}

/// Intrinsic image sizes by URL, as the tab knows them.
#[derive(Debug, Clone, Default)]
pub struct ImageSizeMap(pub Arc<HashMap<Url, (f32, f32)>>);

impl ImageSizes for ImageSizeMap {
    fn intrinsic_size(&self, url: &Url) -> Option<(f32, f32)> {
        self.0.get(url).copied()
    }
}

/// The layout machinery and the viewport, lent by the tab around every
/// script run.
#[derive(Default)]
#[allow(missing_debug_implementations)]
pub struct View {
    /// The page's viewport in CSS pixels, and the device scale factor.
    pub viewport: (f32, f32),
    pub scale: f32,
    /// The scroll offset; a script may change it (`scroll_changed`).
    pub scroll: (f32, f32),
    /// The element scroll containers' offsets, as the tab keeps them, so
    /// a layout forced here keeps every scroller where it was. A script
    /// scroll of an element is applied to the tree at once (later reads
    /// see the moved content) and recorded here for the tab.
    pub element_scrolls: HashMap<NodeId, (f32, f32)>,
    /// A script moved an element scroll container.
    pub elements_scrolled: bool,
    /// The scrolls the script asked for, in order: the page (`None`) or
    /// an element, the target offset, and whether it is smooth
    /// (`behavior: smooth`, or `auto` on a box with `scroll-behavior:
    /// smooth`). Instant ones are applied here at once (later reads see
    /// them) and listed so the tab drops an animation they override;
    /// smooth ones the tab animates after the script, the offsets read
    /// meanwhile being the current ones, as in browsers.
    pub scroll_requests: Vec<(Option<NodeId>, (f32, f32), bool)>,
    /// The window's outer size and the screen's size, in CSS pixels.
    pub window: (f32, f32),
    pub screen: (f32, f32),
    pub engine: Option<LayoutEngine>,
    pub styles: StyleMap,
    pub layout: Option<LayoutTree>,
    pub deps: InteractionDeps,
    /// External sheets already fetched and parsed, by URL.
    pub loaded_sheets: HashMap<Url, Arc<Stylesheet>>,
    pub images: ImageSizeMap,
    /// The arena generation `styles` and `layout` are current for; none
    /// when the tab lent stale ones (a restyle or layout was pending).
    pub clean_generation: Option<u64>,
    /// A script forced a recomputation: the tab adopts the result when
    /// nothing changed after it.
    pub recomputed: bool,
    pub scroll_changed: bool,
    /// Inline `<style>` sheets parsed here, by text hash.
    inline_cache: HashMap<u64, Arc<Stylesheet>>,
    /// Fragment boxes by node, in tree order, for the current layout.
    rect_index: Option<HashMap<NodeId, Vec<FragmentBox>>>,
}

/// One fragment of an element in the current layout: its border box
/// and the used padding and margin layout gave it, and the space it
/// shows in (the transforms above it; fixed content).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FragmentBox {
    order: u32,
    pub(crate) rect: Rect,
    pub(crate) padding: Sides<f32>,
    pub(crate) margin: Sides<f32>,
    pub(crate) space: Space,
}

impl FragmentBox {
    /// Where the box shows, in page coordinates: its bounds through the
    /// transforms, and for fixed content moved by the page scroll.
    fn shown(&self, scroll: (f32, f32)) -> Rect {
        let r = affine_bounds(&self.space.transform, &self.rect);
        if self.space.fixed { r.translate(scroll.0, scroll.1) } else { r }
    }
}

fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl View {
    /// Make `styles` and `layout` current for the lent document, if
    /// they are not: the cascade with the document's sheets as they are
    /// now, then layout.
    pub fn ensure_layout(&mut self, doc: &Document, states: &ElementStates, generation: u64, changes: &[DomChange]) {
        if self.clean_generation == Some(generation) && self.layout.is_some() {
            return;
        }
        if self.engine.is_none() {
            return;
        }
        let stylist = self.stylist_for(doc);
        let vp = Viewport {
            width: self.viewport.0,
            height: self.viewport.1,
            scale_factor: self.scale,
            prefers_dark: false,
        };
        // Styles current before the changes, which are all attribute
        // changes: restyle what they reach, and lay out again only when
        // that moved something (or an attribute layout reads changed).
        let delta = match (self.clean_generation, &self.layout) {
            (Some(clean), Some(_)) => feature_changes(doc, changes, Some(clean)),
            _ => None,
        };
        if let Some(delta) = delta {
            let r = restyle_mutations(doc, &stylist, &vp, states, &mut self.styles, &self.deps, &delta.changes);
            if !r.moved && !delta.layout {
                if r.repaint
                    && let (Some(engine), Some(layout)) = (&mut self.engine, &mut self.layout)
                {
                    engine.refresh_styles(layout, doc, &self.styles);
                }
                self.clean_generation = Some(generation);
                self.recomputed = true;
                return;
            }
        } else {
            self.styles = compute_styles_with(doc, &stylist, &vp, states);
            self.deps = stylist.interaction_deps();
        }
        let Some(engine) = &mut self.engine else { return };
        let tree = engine.layout_with_scrolls(
            doc,
            &self.styles,
            self.viewport.0,
            self.viewport.1,
            &self.images,
            &self.element_scrolls,
            self.scroll,
        );
        self.layout = Some(tree);
        self.rect_index = None;
        self.clean_generation = Some(generation);
        self.recomputed = true;
        self.clamp_scroll();
    }

    /// The document's sheets in cascade order, as the tab's loader
    /// arranges them: the UA sheet, then each `<style>` (parsed here,
    /// cached by text) and `<link rel=stylesheet>` (from the loaded
    /// sheets; one not fetched yet is left out until the tab has it),
    /// each preceded by its `@import`s, all under the element's `media`.
    fn stylist_for(&mut self, doc: &Document) -> Stylist {
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let base = doc.base_url.clone();
        for id in doc.descendants(doc.root()) {
            let Some(e) = doc.element(id) else { continue };
            if e.name.ns != ns!(html) {
                continue;
            }
            let media = e.attr("media").map_or_else(MediaQueryList::all, MediaQueryList::parse_str);
            let sheet = if e.name.local == local_name!("style") {
                let css = doc.text_content(id);
                let key = hash_text(&css);
                self.inline_cache
                    .entry(key)
                    .or_insert_with(|| Arc::new(Stylesheet::parse_with_base(&css, Origin::Author, base.as_ref())))
                    .clone()
            } else if e.name.local == local_name!("link") {
                let rel = e.attr("rel").unwrap_or("");
                let is_sheet = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet"));
                let alternate = rel.split_ascii_whitespace().any(|r| r.eq_ignore_ascii_case("alternate"));
                let url = e.attr("href").and_then(|h| doc.resolve_url(h));
                match (is_sheet && !alternate, url) {
                    (true, Some(url)) => match self.loaded_sheets.get(&url) {
                        Some(s) => s.clone(),
                        None => continue,
                    },
                    _ => continue,
                }
            } else {
                continue;
            };
            self.add_with_imports(&mut stylist, &sheet, &media, 0);
        }
        stylist
    }

    fn add_with_imports(&self, stylist: &mut Stylist, sheet: &Arc<Stylesheet>, media: &MediaQueryList, depth: u8) {
        if depth < MAX_IMPORT_DEPTH {
            for (href, import_media) in &sheet.imports {
                let Ok(url) = Url::parse(href) else { continue };
                let Some(imported) = self.loaded_sheets.get(&url) else { continue };
                // An import under the sheet's media and its own.
                let both = if *media == MediaQueryList::all() {
                    import_media.clone()
                } else {
                    media.clone()
                };
                self.add_with_imports(stylist, imported, &both, depth + 1);
            }
        }
        if *media == MediaQueryList::all() {
            stylist.add_sheet(sheet.clone());
        } else {
            stylist.add_sheet(Arc::new(Stylesheet {
                origin: Origin::Author,
                rules: vec![Rule::Media(media.clone(), sheet.rules.clone())],
                imports: Vec::new(),
            }));
        }
    }

    /// The content size the page scrolls over.
    pub fn content_size(&self) -> (f32, f32) {
        self.layout
            .as_ref()
            .map_or(self.viewport, |l| (l.content_width.max(self.viewport.0), l.content_height.max(self.viewport.1)))
    }

    fn clamp_scroll(&mut self) {
        // The page scrolls over the viewport's scrollable overflow within
        // its scrollport (the viewport less the page scrollbars).
        match &self.layout {
            Some(tree) => self.scroll = tree.viewport.clamp(self.scroll),
            None => {
                let (cw, ch) = self.content_size();
                self.scroll.0 = self.scroll.0.clamp(0.0, (cw - self.viewport.0).max(0.0));
                self.scroll.1 = self.scroll.1.clamp(0.0, (ch - self.viewport.1).max(0.0));
            }
        }
    }

    /// Scroll the page to `(x, y)` (clamped); the tab applies it after
    /// the script.
    pub fn set_scroll(&mut self, x: f32, y: f32) {
        let before = self.scroll;
        self.scroll = (if x.is_finite() { x } else { 0.0 }, if y.is_finite() { y } else { 0.0 });
        self.clamp_scroll();
        if self.scroll != before {
            self.scroll_changed = true;
        }
    }

    /// Scroll an element scroll container to `(x, y)` (clamped), moving
    /// its content in the tree now. False when it is not a scroll
    /// container.
    pub fn set_element_scroll(&mut self, id: NodeId, x: f32, y: f32) -> bool {
        let Some(tree) = self.layout.as_mut() else { return false };
        let before = tree.scroller(id).map(|i| i.offset);
        let Some(new) = tree.set_scroll_offset(id, (x, y)) else { return false };
        self.element_scrolls.insert(id, new);
        if before != Some(new) {
            self.elements_scrolled = true;
            self.rect_index = None;
        }
        true
    }

    /// Whether a scroll of `target` (the page for `None`) with `behavior`
    /// is smooth: `smooth` is, `instant` is not, `auto` follows the box's
    /// `scroll-behavior` (the root element's for the page; CSSOM View
    /// §5.1).
    pub fn smooth(&self, doc: &Document, target: Option<NodeId>, behavior: ScrollBehaviorArg) -> bool {
        match behavior {
            ScrollBehaviorArg::Smooth => true,
            ScrollBehaviorArg::Instant => false,
            ScrollBehaviorArg::Auto => target
                .or_else(|| doc.document_element())
                .and_then(|n| self.styles.get(n))
                .is_some_and(|s| s.scroll_behavior == browser_style::ScrollBehavior::Smooth),
        }
    }

    /// Scroll the page (`None`) or an element scroller to `(x, y)`: at
    /// once, or as a smooth request for the tab. False for a box that is
    /// no scroll container.
    pub fn request_scroll(&mut self, doc: &Document, target: Option<NodeId>, x: f32, y: f32, behavior: ScrollBehaviorArg) -> bool {
        if target.is_some_and(|n| self.scroller(n).is_none()) {
            return false;
        }
        if self.smooth(doc, target, behavior) {
            self.scroll_requests.push((target, (x, y), true));
            return true;
        }
        self.scroll_requests.push((target, (x, y), false));
        match target {
            None => {
                self.set_scroll(x, y);
                true
            }
            Some(n) => self.set_element_scroll(n, x, y),
        }
    }

    /// The scroll container state of an element, if it is one.
    pub fn scroller(&self, id: NodeId) -> Option<browser_layout::ScrollInfo> {
        self.layout.as_ref().and_then(|t| t.scroller(id)).cloned()
    }

    /// The scroll offsets of the scroll containers among `id`'s
    /// ancestors up to and including `until` (every one when `None`),
    /// summed: what the layout positions of `id` differ from its page
    /// coordinates by.
    pub fn ancestor_scroll(&self, doc: &Document, id: NodeId, until: Option<NodeId>) -> (f32, f32) {
        let Some(tree) = &self.layout else { return (0.0, 0.0) };
        let mut sum = (0.0, 0.0);
        for a in doc.ancestors(id) {
            if let Some(info) = tree.scroller(a) {
                sum.0 += info.offset.0;
                sum.1 += info.offset.1;
            }
            if Some(a) == until {
                break;
            }
        }
        sum
    }

    fn index(&mut self) -> &HashMap<NodeId, Vec<FragmentBox>> {
        if self.rect_index.is_none() {
            let mut index: HashMap<NodeId, Vec<FragmentBox>> = HashMap::new();
            let mut order = 0u32;
            fn visit(
                f: &Fragment,
                order: &mut u32,
                index: &mut HashMap<NodeId, Vec<FragmentBox>>,
                spaces: &HashMap<NodeId, Space>,
            ) {
                if let Some(n) = f.node {
                    index.entry(n).or_default().push(FragmentBox {
                        order: *order,
                        rect: f.rect,
                        padding: f.padding,
                        margin: f.margin,
                        space: spaces.get(&n).copied().unwrap_or(Space::ROOT),
                    });
                    *order += 1;
                }
                for c in &f.children {
                    visit(c, order, index, spaces);
                }
            }
            if let Some(tree) = &self.layout {
                visit(&tree.root, &mut order, &mut index, tree.spaces());
            }
            self.rect_index = Some(index);
        }
        self.rect_index.as_ref().expect("built")
    }

    /// The boxes of `id`, in tree order: its own fragments, or, for an
    /// inline element (which has no box of its own), the fragments of
    /// what it contains, one per line.
    fn boxes_of(&mut self, doc: &Document, id: NodeId) -> Vec<FragmentBox> {
        let index = self.index();
        if let Some(own) = index.get(&id) {
            return own.clone();
        }
        if !doc.contains(id) {
            return Vec::new();
        }
        let mut found: Vec<FragmentBox> = Vec::new();
        for d in doc.descendants(id) {
            if let Some(boxes) = index.get(&d) {
                found.extend(boxes.iter().copied());
            }
        }
        found.sort_by_key(|b| b.order);
        // A box fragment covers the fragments inside it: keep the outer.
        let mut out: Vec<FragmentBox> = Vec::new();
        for b in found {
            let r = b.rect;
            if out
                .iter()
                .any(|o| o.rect.x <= r.x && o.rect.y <= r.y && o.rect.right() >= r.right() && o.rect.bottom() >= r.bottom())
            {
                continue;
            }
            out.push(b);
        }
        out
    }

    /// The boxes of `id` in page coordinates as they show (through the
    /// transforms above them; CSSOM View's client rectangles), in tree
    /// order.
    pub fn rects_of(&mut self, doc: &Document, id: NodeId) -> Vec<Rect> {
        let scroll = self.scroll;
        self.boxes_of(doc, id).iter().map(|b| b.shown(scroll)).collect()
    }

    /// The boxes of `id` as layout placed them (no transforms; fixed
    /// content in viewport coordinates), for the `offset*` geometry.
    fn layout_rects_of(&mut self, doc: &Document, id: NodeId) -> Vec<Rect> {
        self.boxes_of(doc, id).iter().map(|b| b.rect).collect()
    }

    /// The first box of `id`, if it has one.
    pub fn own_rect(&mut self, id: NodeId) -> Option<Rect> {
        self.own_box(id).map(|b| b.rect)
    }

    /// The first fragment of `id` with its used padding and margin, if
    /// it has a box of its own.
    pub(crate) fn own_box(&mut self, id: NodeId) -> Option<FragmentBox> {
        self.index().get(&id).and_then(|v| v.first()).copied()
    }

    /// A grid container's used tracks, from its box.
    pub(crate) fn grid_of(&mut self, id: NodeId) -> Option<std::sync::Arc<browser_style::UsedGrid>> {
        let tree = self.layout.as_ref()?;
        let mut found = None;
        tree.root.walk(&mut |f| {
            if found.is_none()
                && f.node == Some(id)
                && let Some(g) = &f.grid
            {
                found = Some(std::sync::Arc::new(g.used()));
            }
        });
        found
    }

    /// The containing block of an absolutely positioned (or fixed) box
    /// as layout used it (CSS 2.1 §10.1): the padding box of the element
    /// `cb`, an inline element's from the start of its first fragment to
    /// the end of its last; with no element, the initial containing block
    /// (the viewport less the page's scrollbar gutters, in the
    /// coordinates fixed boxes have too).
    pub(crate) fn containing_rect(&mut self, cb: Option<NodeId>) -> Rect {
        let initial = self
            .layout
            .as_ref()
            .map(|t| {
                let p = t.viewport.scrollport;
                Rect::new(p.x, 0.0, p.width, p.height)
            })
            .unwrap_or(Rect::new(0.0, 0.0, self.viewport.0, self.viewport.1));
        let Some(a) = cb else { return initial };
        let style = self.styles.get(a).cloned();
        let Some(boxes) = self.index().get(&a).cloned() else { return initial };
        let (Some(first), Some(last)) = (boxes.first(), boxes.last()) else { return initial };
        let bw = style.as_ref().map(|s| s.border_width).unwrap_or(Sides::all(0.0));
        let pad = |r: &Rect| Rect::new(r.x + bw.left, r.y + bw.top, (r.width - bw.left - bw.right).max(0.0), (r.height - bw.top - bw.bottom).max(0.0));
        let inline = style.as_ref().is_some_and(|s| matches!(s.display, browser_style::Display::Inline));
        if !inline || boxes.len() == 1 {
            return pad(&first.rect);
        }
        let (f, l) = (pad(&first.rect), pad(&last.rect));
        let rtl = style.as_ref().is_some_and(|s| s.direction == browser_style::Direction::Rtl);
        if rtl {
            Rect::new(l.x, f.y, (f.right() - l.x).max(0.0), (l.bottom() - f.y).max(0.0))
        } else {
            Rect::new(f.x, f.y, (l.right() - f.x).max(0.0), (l.bottom() - f.y).max(0.0))
        }
    }

    /// The smallest rectangle around all of an element's boxes as they
    /// show; zero when it has none (not rendered).
    pub fn bounding(&mut self, doc: &Document, id: NodeId) -> Rect {
        union(&self.rects_of(doc, id))
    }

    /// The smallest rectangle around all of an element's boxes as layout
    /// placed them.
    fn layout_bounding(&mut self, doc: &Document, id: NodeId) -> Rect {
        union(&self.layout_rects_of(doc, id))
    }

    /// How far the boxes inside `id` reach past its padding box origin:
    /// `scrollWidth`/`scrollHeight` without a scroll container. A
    /// transformed box inside reaches as far as its transformed bounds,
    /// taken in `id`'s own coordinates.
    fn content_extent(&mut self, doc: &Document, id: NodeId, padding_box: Rect) -> (f32, f32) {
        let own = self.own_box(id).map(|b| b.space).unwrap_or(Space::ROOT);
        let index = self.index();
        let (mut w, mut h) = (padding_box.width, padding_box.height);
        let inv = affine_invert(&own.transform).unwrap_or(IDENTITY);
        for d in doc.descendants(id) {
            if let Some(boxes) = index.get(&d) {
                for b in boxes {
                    let relative = affine_mul(&inv, &b.space.transform);
                    let r = affine_bounds(&relative, &b.rect);
                    w = w.max(r.right() - padding_box.x);
                    h = h.max(r.bottom() - padding_box.y);
                }
            }
        }
        (w, h)
    }
}

fn union(rects: &[Rect]) -> Rect {
    let Some(first) = rects.first() else { return Rect::default() };
    rects.iter().skip(1).fold(*first, |u, r| {
        let x = u.x.min(r.x);
        let y = u.y.min(r.y);
        Rect::new(x, y, u.right().max(r.right()) - x, u.bottom().max(r.bottom()) - y)
    })
}

/// The padding box of an element's border box under its style.
fn padding_box(rect: Rect, style: &browser_style::ComputedStyle) -> Rect {
    let b = &style.border_width;
    Rect::new(
        rect.x + b.left,
        rect.y + b.top,
        (rect.width - b.left - b.right).max(0.0),
        (rect.height - b.top - b.bottom).max(0.0),
    )
}

// ----- the lent view in the Dom -----

/// Run `f` with the document's layout current, the view and the document
/// borrowed apart. `None` when no view is lent (nothing runs then).
pub(crate) fn with_layout<T>(
    context: &mut Context,
    f: impl FnOnce(&mut View, &Document, bool) -> T,
) -> JsResult<Option<T>> {
    let quirks = host_state(context)?.borrow().info.quirks;
    let shared = dom(context)?;
    let mut guard = shared.borrow_mut();
    let Dom {
        doc,
        states,
        view,
        generation,
        changes,
        ..
    } = &mut *guard;
    let Some(view) = view.as_mut() else {
        return Ok(None);
    };
    view.ensure_layout(doc, states, *generation, changes);
    Ok(Some(f(view, doc, quirks)))
}

/// `ScrollBehavior` of CSSOM View's `ScrollOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollBehaviorArg {
    Auto,
    Instant,
    Smooth,
}

/// The element that scrolls the page: `html`, or `body` in quirks mode.
fn scrolling_element(doc: &Document, quirks: bool) -> Option<NodeId> {
    if quirks { doc.body() } else { doc.document_element() }
}

// ----- DOMRect -----

/// `DOMRectReadOnly` and `DOMRect`: x, y, width, height.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
struct RectData {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl RectData {
    fn from_rect(r: Rect) -> Self {
        Self {
            x: f64::from(r.x),
            y: f64::from(r.y),
            width: f64::from(r.width),
            height: f64::from(r.height),
        }
    }

    fn from_args(args: &[JsValue], context: &mut Context) -> JsResult<Self> {
        let num = |i: usize, context: &mut Context| -> JsResult<f64> {
            let v = args.get_or_undefined(i);
            if v.is_undefined() { Ok(0.0) } else { v.to_number(context) }
        };
        Ok(Self {
            x: num(0, context)?,
            y: num(1, context)?,
            width: num(2, context)?,
            height: num(3, context)?,
        })
    }

    /// `fromRect({x, y, width, height})`.
    fn from_init(init: &JsValue, context: &mut Context) -> JsResult<Self> {
        let mut out = Self {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
        if let Some(o) = init.as_object() {
            for (name, slot) in [
                ("x", &mut out.x),
                ("y", &mut out.y),
                ("width", &mut out.width),
                ("height", &mut out.height),
            ] {
                let v = o.get(JsString::from(name), context)?;
                if !v.is_undefined() {
                    *slot = v.to_number(context)?;
                }
            }
        }
        Ok(out)
    }
}

#[derive(Clone, Trace, Finalize)]
enum RectProp {
    X,
    Y,
    Width,
    Height,
    Top,
    Right,
    Bottom,
    Left,
}

fn this_rect(this: &JsValue) -> JsResult<JsObject> {
    this.as_object()
        .filter(|o| o.is::<RectData>())
        .ok_or_else(illegal_invocation)
}

fn rect_get(this: &JsValue, _: &[JsValue], prop: &RectProp, _: &mut Context) -> JsResult<JsValue> {
    let o = this_rect(this)?;
    let r = o.downcast_ref::<RectData>().ok_or_else(illegal_invocation)?.clone();
    Ok(match prop {
        RectProp::X => r.x,
        RectProp::Y => r.y,
        RectProp::Width => r.width,
        RectProp::Height => r.height,
        RectProp::Top => r.y.min(r.y + r.height),
        RectProp::Bottom => r.y.max(r.y + r.height),
        RectProp::Left => r.x.min(r.x + r.width),
        RectProp::Right => r.x.max(r.x + r.width),
    }
    .into())
}

fn rect_set(this: &JsValue, args: &[JsValue], prop: &RectProp, context: &mut Context) -> JsResult<JsValue> {
    let o = this_rect(this)?;
    let v = args.get_or_undefined(0).to_number(context)?;
    let mut r = o.downcast_mut::<RectData>().ok_or_else(illegal_invocation)?;
    match prop {
        RectProp::X => r.x = v,
        RectProp::Y => r.y = v,
        RectProp::Width => r.width = v,
        RectProp::Height => r.height = v,
        _ => {}
    }
    Ok(JsValue::undefined())
}

fn rect_to_json(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let o = this_rect(this)?;
    let r = o.downcast_ref::<RectData>().ok_or_else(illegal_invocation)?.clone();
    Ok(ObjectInitializer::new(context)
        .property(js_string!("x"), r.x, Attribute::all())
        .property(js_string!("y"), r.y, Attribute::all())
        .property(js_string!("width"), r.width, Attribute::all())
        .property(js_string!("height"), r.height, Attribute::all())
        .property(js_string!("top"), r.y.min(r.y + r.height), Attribute::all())
        .property(js_string!("right"), r.x.max(r.x + r.width), Attribute::all())
        .property(js_string!("bottom"), r.y.max(r.y + r.height), Attribute::all())
        .property(js_string!("left"), r.x.min(r.x + r.width), Attribute::all())
        .build()
        .into())
}

fn add_rect_getters(class: &mut ClassBuilder<'_>, writable: bool) {
    for (name, prop) in [
        ("x", RectProp::X),
        ("y", RectProp::Y),
        ("width", RectProp::Width),
        ("height", RectProp::Height),
        ("top", RectProp::Top),
        ("right", RectProp::Right),
        ("bottom", RectProp::Bottom),
        ("left", RectProp::Left),
    ] {
        let get = getter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(rect_get, prop.clone()),
        );
        let settable = writable && matches!(prop, RectProp::X | RectProp::Y | RectProp::Width | RectProp::Height);
        let set = settable.then(|| {
            setter(
                class.context(),
                name,
                NativeFunction::from_copy_closure_with_captures(rect_set, prop.clone()),
            )
        });
        class.accessor(JsString::from(name), Some(get), set, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
    }
    class.method(js_string!("toJSON"), 0, NativeFunction::from_fn_ptr(rect_to_json));
    class.static_method(js_string!("fromRect"), 1, NativeFunction::from_fn_ptr(rect_from_rect));
}

/// `DOMRect.fromRect(init)`, on both classes, making an instance of the
/// class it was called on (`this`).
fn rect_from_rect(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let data = RectData::from_init(args.get_or_undefined(0), context)?;
    let read_only = this
        .as_object()
        .and_then(|o| context.get_global_class::<DomRectReadOnlyClass>().map(|c| c.constructor() == o))
        .unwrap_or(false);
    let proto = if read_only {
        prototype_of::<DomRectReadOnlyClass>(context)?
    } else {
        prototype_of::<DomRectClass>(context)?
    };
    Ok(JsObject::from_proto_and_data(proto, data).into())
}

#[derive(Debug, Trace, Finalize, JsData)]
struct DomRectReadOnlyClass;

impl Class for DomRectReadOnlyClass {
    const NAME: &'static str = "DOMRectReadOnly";
    const LENGTH: usize = 0;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        add_rect_getters(class, false);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(JsNativeError::typ().with_message("DOMRectReadOnly constructor: called without new").into());
        }
        let data = RectData::from_args(args, context)?;
        Ok(JsObject::from_proto_and_data(prototype_of::<Self>(context)?, data))
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct DomRectClass;

impl Class for DomRectClass {
    const NAME: &'static str = "DOMRect";
    const LENGTH: usize = 0;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        add_rect_getters(class, true);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(JsNativeError::typ().with_message("DOMRect constructor: called without new").into());
        }
        let data = RectData::from_args(args, context)?;
        Ok(JsObject::from_proto_and_data(prototype_of::<Self>(context)?, data))
    }
}

fn prototype_of<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

fn new_dom_rect(r: Rect, context: &mut Context) -> JsResult<JsObject> {
    Ok(JsObject::from_proto_and_data(prototype_of::<DomRectClass>(context)?, RectData::from_rect(r)))
}

/// `DOMRectList`: `length`, `item(i)` and indexed access, not live.
fn new_rect_list(rects: &[Rect], context: &mut Context) -> JsResult<JsObject> {
    let mut init = ObjectInitializer::new(context);
    for (i, &r) in rects.iter().enumerate() {
        let rect = new_dom_rect(r, init.context())?;
        init.property(JsString::from(i.to_string()), rect, Attribute::ENUMERABLE);
    }
    init.property(js_string!("length"), rects.len() as i32, Attribute::empty());
    init.function(NativeFunction::from_fn_ptr(rect_list_item), js_string!("item"), 1);
    Ok(init.build())
}

fn rect_list_item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(o) = this.as_object() else { return Err(illegal_invocation()) };
    let i = args.get_or_undefined(0).to_number(context)?;
    if !i.is_finite() || i < 0.0 {
        return Ok(JsValue::null());
    }
    let v = o.get(JsString::from((i as u64).to_string()), context)?;
    Ok(if v.is_undefined() { JsValue::null() } else { v })
}

// ----- element geometry -----

#[derive(Clone, Trace, Finalize)]
enum GeoProp {
    OffsetWidth,
    OffsetHeight,
    OffsetLeft,
    OffsetTop,
    OffsetParent,
    ClientWidth,
    ClientHeight,
    ClientLeft,
    ClientTop,
    ScrollWidth,
    ScrollHeight,
    ScrollTop,
    ScrollLeft,
}

/// What a geometry getter found, resolved to a value outside the borrow.
enum Geo {
    Num(f64),
    Node(Option<NodeId>),
}

/// The `offsetParent` of `id`: none for the root, `body`, an element
/// without a box or a fixed one; else the nearest positioned ancestor,
/// table cell or table, else `body`.
fn offset_parent(view: &mut View, doc: &Document, id: NodeId) -> Option<NodeId> {
    let body = doc.body();
    if Some(id) == body || Some(id) == doc.document_element() || view.own_rect(id).is_none() && view.rects_of(doc, id).is_empty() {
        return None;
    }
    if view.styles.get(id).is_some_and(|s| s.position == Position::Fixed) {
        return None;
    }
    for a in doc.ancestors(id) {
        if Some(a) == body {
            return body;
        }
        let Some(e) = doc.element(a) else { continue };
        let positioned = view.styles.get(a).is_some_and(|s| s.position != Position::Static);
        let table_part = e.name.ns == ns!(html)
            && matches!(e.name.local, local_name!("td") | local_name!("th") | local_name!("table"));
        if positioned || table_part {
            return Some(a);
        }
    }
    body
}

fn geometry(view: &mut View, doc: &Document, quirks: bool, id: NodeId, prop: &GeoProp) -> Geo {
    let scroller = scrolling_element(doc, quirks) == Some(id);
    // The `offset*` and `client*` geometry is layout's, before any
    // transform (CSSOM View §8).
    let rect = view.layout_bounding(doc, id);
    let style = view.styles.get(id).cloned();
    let pad_box = style.as_ref().map_or(rect, |s| padding_box(rect, s));
    let num = |v: f32| Geo::Num(f64::from(v.round()));
    // An element scroll container: its client box is its padding box
    // less the scrollbar gutters (CSSOM View §6.1), its scroll extent
    // and offset the layout's.
    let info = view.scroller(id);
    let viewport = view.layout.as_ref().map(|t| t.viewport.clone());
    match prop {
        GeoProp::OffsetWidth => num(rect.width),
        GeoProp::OffsetHeight => num(rect.height),
        GeoProp::OffsetLeft | GeoProp::OffsetTop => {
            let parent = offset_parent(view, doc, id);
            let origin = match parent {
                Some(p) => {
                    let pr = view.layout_bounding(doc, p);
                    view.styles.get(p).map_or(pr, |s| padding_box(pr, s))
                }
                None => Rect::default(),
            };
            // Layout positions, not scrolled ones: the offsets of the
            // scrollers between the element and its offsetParent (that one
            // included) are added back, since the page coordinates have
            // them taken off. A fixed box is placed by the viewport: no
            // scroller moves it.
            let fixed = view.own_box(id).is_some_and(|b| b.space.fixed);
            let (sx, sy) = if fixed { (0.0, 0.0) } else { view.ancestor_scroll(doc, id, parent) };
            if matches!(prop, GeoProp::OffsetLeft) { num(rect.x + sx - origin.x) } else { num(rect.y + sy - origin.y) }
        }
        GeoProp::OffsetParent => Geo::Node(offset_parent(view, doc, id)),
        // The scrolling element's client box is the viewport less the
        // page scrollbars.
        GeoProp::ClientWidth => num(match (&viewport, &info) {
            (Some(v), _) if scroller => v.scrollport.width,
            (_, Some(i)) => i.padding_box.width - i.gutter_left - i.gutter_right,
            _ if scroller => view.viewport.0,
            _ => pad_box.width,
        }),
        GeoProp::ClientHeight => num(match (&viewport, &info) {
            (Some(v), _) if scroller => v.scrollport.height,
            (_, Some(i)) => i.padding_box.height - i.gutter_bottom,
            _ if scroller => view.viewport.1,
            _ => pad_box.height,
        }),
        // `clientLeft` counts a scrollbar on the left (a right-to-left
        // scroller's).
        GeoProp::ClientLeft => num(match (&viewport, &info) {
            (Some(v), _) if scroller => v.gutter_left,
            (_, Some(i)) => i.padding_box.x - rect.x + i.gutter_left,
            _ if scroller => 0.0,
            _ => pad_box.x - rect.x,
        }),
        GeoProp::ClientTop => num(match &info {
            _ if scroller => 0.0,
            Some(i) => i.padding_box.y - rect.y,
            None => pad_box.y - rect.y,
        }),
        GeoProp::ScrollWidth | GeoProp::ScrollHeight => {
            let (w, h) = if scroller {
                view.content_size()
            } else if let Some(i) = &info {
                (i.scroll_width, i.scroll_height)
            } else {
                view.content_extent(doc, id, pad_box)
            };
            num(if matches!(prop, GeoProp::ScrollWidth) { w } else { h })
        }
        GeoProp::ScrollTop => Geo::Num(f64::from(if scroller { view.scroll.1 } else { info.map_or(0.0, |i| i.offset.1) })),
        GeoProp::ScrollLeft => Geo::Num(f64::from(if scroller { view.scroll.0 } else { info.map_or(0.0, |i| i.offset.0) })),
    }
}

fn geo_get(this: &JsValue, _: &[JsValue], prop: &GeoProp, context: &mut Context) -> JsResult<JsValue> {
    let node = this
        .as_object()
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)?;
    let Some(id) = node.id else { return Err(illegal_invocation()) };
    let out = with_layout(context, |view, doc, quirks| {
        if !doc.contains(id) || !doc.is_connected(id) {
            return match prop {
                GeoProp::OffsetParent => Geo::Node(None),
                _ => Geo::Num(0.0),
            };
        }
        geometry(view, doc, quirks, id, prop)
    })?;
    Ok(match out {
        Some(Geo::Num(n)) => n.into(),
        Some(Geo::Node(Some(n))) => wrap(n, context)?,
        Some(Geo::Node(None)) => JsValue::null(),
        None => match prop {
            GeoProp::OffsetParent => JsValue::null(),
            _ => 0.into(),
        },
    })
}

/// `scrollTop = ` and `scrollLeft = `: the page scrolls for the
/// scrolling element, a scroll container (`overflow: hidden` included)
/// scrolls its content; anything else ignores it (CSSOM View §6.1).
/// A non-finite value is 0.
fn geo_set(this: &JsValue, args: &[JsValue], prop: &GeoProp, context: &mut Context) -> JsResult<JsValue> {
    let node = this
        .as_object()
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)?;
    let Some(id) = node.id else { return Err(illegal_invocation()) };
    let v = args.get_or_undefined(0).to_number(context)?;
    let v = if v.is_finite() { v as f32 } else { 0.0 };
    with_layout(context, |view, doc, quirks| {
        // The setter scrolls with behavior `auto` (CSSOM View §6.1): the
        // box's `scroll-behavior` decides.
        if scrolling_element(doc, quirks) == Some(id) {
            let (x, y) = view.scroll;
            match prop {
                GeoProp::ScrollTop => view.request_scroll(doc, None, x, v, ScrollBehaviorArg::Auto),
                GeoProp::ScrollLeft => view.request_scroll(doc, None, v, y, ScrollBehaviorArg::Auto),
                _ => false,
            };
        } else if let Some(info) = view.scroller(id) {
            match prop {
                GeoProp::ScrollTop => view.request_scroll(doc, Some(id), info.offset.0, v, ScrollBehaviorArg::Auto),
                GeoProp::ScrollLeft => view.request_scroll(doc, Some(id), v, info.offset.1, ScrollBehaviorArg::Auto),
                _ => false,
            };
        }
    })?;
    Ok(JsValue::undefined())
}

#[derive(Clone, Trace, Finalize)]
enum GeoMethod {
    BoundingClientRect,
    ClientRects,
    ScrollIntoView,
    /// `scrollTo`/`scroll`/`scrollBy` on an element: the page for the
    /// scrolling element, a scroll container's content, nothing
    /// elsewhere.
    ScrollTo,
    ScrollBy,
}

/// `ScrollOptions.behavior` and `ScrollIntoViewOptions.behavior`: a
/// WebIDL enum, so anything but its three values throws; `auto` when
/// left out.
fn scroll_behavior(options: &JsObject, context: &mut Context) -> JsResult<ScrollBehaviorArg> {
    let v = options.get(js_string!("behavior"), context)?;
    if v.is_undefined() {
        return Ok(ScrollBehaviorArg::Auto);
    }
    let s = v.to_string(context)?.to_std_string_escaped();
    Ok(match s.as_str() {
        "auto" => ScrollBehaviorArg::Auto,
        "instant" => ScrollBehaviorArg::Instant,
        "smooth" => ScrollBehaviorArg::Smooth,
        _ => {
            return Err(JsNativeError::typ()
                .with_message(format!("The provided value '{s}' is not a valid enum value of type ScrollBehavior."))
                .into());
        }
    })
}

/// The scroll-snap-free alignment of `scrollIntoView`'s `block`/`inline`
/// per CSSOM View §12.3 "scroll an element into view", for one axis: the
/// element's edges `lo..hi` grown by its `scroll-margin`, the scrollport
/// `port_lo..port_hi` shrunk by the scroller's `scroll-padding`; the
/// change of the scroller's offset that aligns them.
fn into_view_delta(how: &str, lo: f32, hi: f32, port_lo: f32, port_hi: f32) -> f32 {
    let size = port_hi - port_lo;
    let new_lo = match how {
        "start" => lo,
        "center" => (lo + hi) / 2.0 - size / 2.0,
        "end" => hi - size,
        // `nearest`: nothing when inside; else whichever edge is nearer,
        // the start edge when the element is taller than the port.
        _ => {
            if lo >= port_lo && hi <= port_hi {
                port_lo
            } else if hi - lo > size {
                if lo < port_lo { hi - size } else { lo }
            } else if lo < port_lo {
                lo
            } else {
                hi - size
            }
        }
    };
    new_lo - port_lo
}

/// `scrollIntoView` (CSSOM View §12.3, §12.4): for each scrolling box
/// from the nearest ancestor scroll container to the viewport (hidden
/// ones too), scroll it so the element's bounding box grown by its
/// `scroll-margin` is aligned per `block`/`inline` inside the scrollport
/// shrunk by the box's `scroll-padding` (the root element's for the
/// viewport); `container: "nearest"` stops after the first.
fn scroll_into_view(
    view: &mut View,
    doc: &Document,
    id: NodeId,
    block: &str,
    inline: &str,
    nearest_only: bool,
    behavior: ScrollBehaviorArg,
) {
    let margin = view.styles.get(id).map(|s| s.scroll_margin).unwrap_or(Sides::all(0.0));
    // What the scrollers below, animating toward their targets, will
    // still move the element by.
    let mut pending = (0.0f32, 0.0f32);
    let grow = |r: Rect| Rect::new(r.x - margin.left, r.y - margin.top, r.width + margin.left + margin.right, r.height + margin.top + margin.bottom);
    let padded = |port: Rect, padding: Sides<browser_style::ComputedLpAuto>| {
        let r = |v: browser_style::ComputedLpAuto, basis: f32| v.resolve(basis).unwrap_or(0.0);
        Rect::new(
            port.x + r(padding.left, port.width),
            port.y + r(padding.top, port.height),
            (port.width - r(padding.left, port.width) - r(padding.right, port.width)).max(0.0),
            (port.height - r(padding.top, port.height) - r(padding.bottom, port.height)).max(0.0),
        )
    };
    let ancestors: Vec<NodeId> = doc.ancestors(id).collect();
    for a in ancestors {
        let Some(info) = view.scroller(a) else { continue };
        let rect = grow(view.bounding(doc, id)).translate(-pending.0, -pending.1);
        let padding = view.styles.get(a).map(|s| s.scroll_padding).unwrap_or(Sides::all(browser_style::ComputedLpAuto::Auto));
        let port = padded(info.scrollport, padding);
        let dy = into_view_delta(block, rect.y, rect.bottom(), port.y, port.bottom());
        let dx = into_view_delta(inline, rect.x, rect.right(), port.x, port.right());
        let to = info.clamp((info.offset.0 + dx, info.offset.1 + dy));
        if view.smooth(doc, Some(a), behavior) {
            pending.0 += to.0 - info.offset.0;
            pending.1 += to.1 - info.offset.1;
        }
        view.request_scroll(doc, Some(a), to.0, to.1, behavior);
        if nearest_only {
            return;
        }
    }
    let rect = grow(view.bounding(doc, id)).translate(-pending.0, -pending.1);
    let Some(tree) = &view.layout else { return };
    let port = tree.viewport.scrollport.translate(view.scroll.0, view.scroll.1);
    let padding = doc
        .document_element()
        .and_then(|r| view.styles.get(r))
        .map(|s| s.scroll_padding)
        .unwrap_or(Sides::all(browser_style::ComputedLpAuto::Auto));
    let port = padded(port, padding);
    let dy = into_view_delta(block, rect.y, rect.bottom(), port.y, port.bottom());
    let dx = into_view_delta(inline, rect.x, rect.right(), port.x, port.right());
    let (sx, sy) = view.scroll;
    view.request_scroll(doc, None, sx + dx, sy + dy, behavior);
}

/// `scrollTo(x, y)`, `scrollTo({left, top, behavior})`, `scrollBy(...)`:
/// the target offset, with missing parts kept (`by`: added); a
/// non-finite part is 0 (WebIDL `unrestricted double` normalized per
/// CSSOM View).
fn scroll_args(args: &[JsValue], current: (f32, f32), by: bool, context: &mut Context) -> JsResult<((f32, f32), ScrollBehaviorArg)> {
    let first = args.get_or_undefined(0);
    let finite = |v: f64| if v.is_finite() { v as f32 } else { 0.0 };
    let mut behavior = ScrollBehaviorArg::Auto;
    let (dx, dy) = if let Some(o) = first.as_object() {
        let num = |name: &str, context: &mut Context| -> JsResult<Option<f32>> {
            let v = o.get(JsString::from(name), context)?;
            Ok(if v.is_undefined() { None } else { Some(finite(v.to_number(context)?)) })
        };
        let out = (num("left", context)?, num("top", context)?);
        behavior = scroll_behavior(&o, context)?;
        out
    } else {
        let x = if first.is_undefined() { None } else { Some(finite(first.to_number(context)?)) };
        let second = args.get_or_undefined(1);
        let y = if second.is_undefined() { None } else { Some(finite(second.to_number(context)?)) };
        (x, y)
    };
    let to = if by {
        (current.0 + dx.unwrap_or(0.0), current.1 + dy.unwrap_or(0.0))
    } else {
        (dx.unwrap_or(current.0), dy.unwrap_or(current.1))
    };
    Ok((to, behavior))
}

fn geo_call(this: &JsValue, args: &[JsValue], method: &GeoMethod, context: &mut Context) -> JsResult<JsValue> {
    let node = this
        .as_object()
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)?;
    let Some(id) = node.id else { return Err(illegal_invocation()) };
    // Arguments first: reading them runs script.
    let (block, inline, nearest_only, behavior) = match method {
        GeoMethod::ScrollIntoView => {
            let arg = args.get_or_undefined(0);
            if let Some(o) = arg.as_object() {
                let s = |name: &str, default: &str, allowed: &[&str], context: &mut Context| -> JsResult<String> {
                    let v = o.get(JsString::from(name), context)?;
                    if v.is_undefined() {
                        return Ok(default.to_owned());
                    }
                    let s = v.to_string(context)?.to_std_string_escaped();
                    if !allowed.contains(&s.as_str()) {
                        return Err(JsNativeError::typ()
                            .with_message(format!("The provided value '{s}' is not a valid enum value of type ScrollLogicalPosition."))
                            .into());
                    }
                    Ok(s)
                };
                const POSITIONS: &[&str] = &["start", "center", "end", "nearest"];
                let block = s("block", "start", POSITIONS, context)?;
                let inline = s("inline", "nearest", POSITIONS, context)?;
                let container = s("container", "all", &["all", "nearest"], context)?;
                let behavior = scroll_behavior(&o, context)?;
                (block, inline, container == "nearest", behavior)
            } else if arg.is_undefined() || arg.to_boolean() {
                ("start".to_owned(), "nearest".to_owned(), false, ScrollBehaviorArg::Auto)
            } else {
                ("end".to_owned(), "nearest".to_owned(), false, ScrollBehaviorArg::Auto)
            }
        }
        _ => (String::new(), String::new(), false, ScrollBehaviorArg::Auto),
    };
    // `scrollTo`/`scrollBy` are relative to the box's own offset: the
    // page's for the scrolling element, the scroller's for a scroll
    // container, nothing otherwise.
    let current = with_layout(context, |view, doc, quirks| {
        if scrolling_element(doc, quirks) == Some(id) {
            Some(view.scroll)
        } else {
            view.scroller(id).map(|i| i.offset)
        }
    })?
    .flatten();
    let target = match (method, current) {
        (GeoMethod::ScrollTo, Some(current)) => Some(scroll_args(args, current, false, context)?),
        (GeoMethod::ScrollBy, Some(current)) => Some(scroll_args(args, current, true, context)?),
        // Not a scroller: the arguments are still read (WebIDL), nothing
        // moves.
        (GeoMethod::ScrollTo | GeoMethod::ScrollBy, None) => {
            scroll_args(args, (0.0, 0.0), false, context)?;
            None
        }
        _ => None,
    };
    let rects = with_layout(context, |view, doc, quirks| {
        if !doc.contains(id) || !doc.is_connected(id) {
            return Vec::new();
        }
        match method {
            GeoMethod::BoundingClientRect | GeoMethod::ClientRects => {
                let (sx, sy) = view.scroll;
                view.rects_of(doc, id).into_iter().map(|r| r.translate(-sx, -sy)).collect()
            }
            GeoMethod::ScrollIntoView => {
                scroll_into_view(view, doc, id, &block, &inline, nearest_only, behavior);
                Vec::new()
            }
            GeoMethod::ScrollTo | GeoMethod::ScrollBy => {
                if let Some(((x, y), behavior)) = target {
                    let page = scrolling_element(doc, quirks) == Some(id);
                    view.request_scroll(doc, if page { None } else { Some(id) }, x, y, behavior);
                }
                Vec::new()
            }
        }
    })?
    .unwrap_or_default();
    Ok(match method {
        GeoMethod::BoundingClientRect => new_dom_rect(union(&rects), context)?.into(),
        GeoMethod::ClientRects => new_rect_list(&rects, context)?.into(),
        _ => JsValue::undefined(),
    })
}

/// The geometry members of `Element`.
pub(crate) fn add_element_geometry(class: &mut ClassBuilder<'_>) {
    for (name, prop, settable) in [
        ("clientWidth", GeoProp::ClientWidth, false),
        ("clientHeight", GeoProp::ClientHeight, false),
        ("clientLeft", GeoProp::ClientLeft, false),
        ("clientTop", GeoProp::ClientTop, false),
        ("scrollWidth", GeoProp::ScrollWidth, false),
        ("scrollHeight", GeoProp::ScrollHeight, false),
        ("scrollTop", GeoProp::ScrollTop, true),
        ("scrollLeft", GeoProp::ScrollLeft, true),
    ] {
        add_geo_accessor(class, name, prop, settable);
    }
    for (name, length, method) in [
        ("getBoundingClientRect", 0, GeoMethod::BoundingClientRect),
        ("getClientRects", 0, GeoMethod::ClientRects),
        ("scrollIntoView", 0, GeoMethod::ScrollIntoView),
        ("scrollTo", 0, GeoMethod::ScrollTo),
        ("scroll", 0, GeoMethod::ScrollTo),
        ("scrollBy", 0, GeoMethod::ScrollBy),
    ] {
        class.method(
            JsString::from(name),
            length,
            NativeFunction::from_copy_closure_with_captures(geo_call, method),
        );
    }
}

/// The `offset*` members of `HTMLElement`.
pub(crate) fn add_html_element_geometry(class: &mut ClassBuilder<'_>) {
    for (name, prop) in [
        ("offsetWidth", GeoProp::OffsetWidth),
        ("offsetHeight", GeoProp::OffsetHeight),
        ("offsetLeft", GeoProp::OffsetLeft),
        ("offsetTop", GeoProp::OffsetTop),
        ("offsetParent", GeoProp::OffsetParent),
    ] {
        add_geo_accessor(class, name, prop, false);
    }
}

fn add_geo_accessor(class: &mut ClassBuilder<'_>, name: &str, prop: GeoProp, settable: bool) {
    let get = getter(
        class.context(),
        name,
        NativeFunction::from_copy_closure_with_captures(geo_get, prop.clone()),
    );
    let set = settable.then(|| {
        setter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(geo_set, prop),
        )
    });
    class.accessor(JsString::from(name), Some(get), set, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
}

/// `document.scrollingElement`.
pub(crate) fn document_scrolling_element(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let quirks = host_state(context)?.borrow().info.quirks;
    let id = {
        let shared = dom(context)?;
        let dom = shared.borrow();
        scrolling_element(&dom.doc, quirks)
    };
    match id {
        Some(id) => wrap(id, context),
        None => Ok(JsValue::null()),
    }
}

// ----- window -----

#[derive(Clone, Trace, Finalize)]
enum WinProp {
    InnerWidth,
    InnerHeight,
    OuterWidth,
    OuterHeight,
    ScrollX,
    ScrollY,
    DevicePixelRatio,
    ScreenWidth,
    ScreenHeight,
    OrientationType,
    /// `visualViewport.pageLeft`/`pageTop` are the scroll offset; its
    /// width and height the viewport's.
    Zero,
}

fn win_get(_: &JsValue, _: &[JsValue], prop: &WinProp, context: &mut Context) -> JsResult<JsValue> {
    let shared = dom(context)?;
    let dom = shared.borrow();
    let Some(view) = dom.view.as_ref() else {
        return Ok(match prop {
            WinProp::DevicePixelRatio => 1.0.into(),
            WinProp::OrientationType => js_str("landscape-primary"),
            _ => 0.into(),
        });
    };
    Ok(match prop {
        WinProp::InnerWidth => f64::from(view.viewport.0).into(),
        WinProp::InnerHeight => f64::from(view.viewport.1).into(),
        WinProp::OuterWidth => f64::from(view.window.0).into(),
        WinProp::OuterHeight => f64::from(view.window.1).into(),
        WinProp::ScrollX => f64::from(view.scroll.0).into(),
        WinProp::ScrollY => f64::from(view.scroll.1).into(),
        WinProp::DevicePixelRatio => f64::from(view.scale).into(),
        WinProp::ScreenWidth => f64::from(view.screen.0).into(),
        WinProp::ScreenHeight => f64::from(view.screen.1).into(),
        WinProp::OrientationType => js_str(if view.screen.1 > view.screen.0 {
            "portrait-primary"
        } else {
            "landscape-primary"
        }),
        WinProp::Zero => 0.into(),
    })
}

#[derive(Clone, Trace, Finalize)]
enum WinMethod {
    ScrollTo,
    ScrollBy,
}

fn win_scroll(_: &JsValue, args: &[JsValue], method: &WinMethod, context: &mut Context) -> JsResult<JsValue> {
    let current = with_layout(context, |view, _, _| view.scroll)?.unwrap_or_default();
    let ((x, y), behavior) = scroll_args(args, current, matches!(method, WinMethod::ScrollBy), context)?;
    with_layout(context, |view, doc, _| view.request_scroll(doc, None, x, y, behavior))?;
    Ok(JsValue::undefined())
}

fn define_accessor(
    object: &JsObject,
    name: &str,
    get: NativeFunction,
    set: Option<NativeFunction>,
    context: &mut Context,
) -> JsResult<()> {
    let get = getter(context, name, get);
    let set = set.map(|f| setter(context, name, f));
    let mut builder = PropertyDescriptor::builder().get(get).enumerable(true).configurable(true);
    if let Some(set) = set {
        builder = builder.set(set);
    }
    object.define_property_or_throw(JsString::from(name), builder.build(), context)?;
    Ok(())
}

/// `window`'s viewport and scroll members, `screen`, `visualViewport`,
/// `matchMedia`, and the rectangle and media classes.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<DomRectReadOnlyClass>()?;
    context.register_global_class::<DomRectClass>()?;
    if let (Some(sub), Some(base)) = (
        context.get_global_class::<DomRectClass>(),
        context.get_global_class::<DomRectReadOnlyClass>(),
    ) {
        sub.prototype().set_prototype(Some(base.prototype()));
        sub.constructor().set_prototype(Some(base.constructor()));
    }
    context.register_global_class::<MediaQueryListClass>()?;
    if let (Some(mql), Some(target)) = (
        context.get_global_class::<MediaQueryListClass>(),
        context.get_global_class::<crate::events::EventTargetClass>(),
    ) {
        mql.prototype().set_prototype(Some(target.prototype()));
        mql.constructor().set_prototype(Some(target.constructor()));
    }

    let global = context.global_object();
    for (name, prop) in [
        ("innerWidth", WinProp::InnerWidth),
        ("innerHeight", WinProp::InnerHeight),
        ("outerWidth", WinProp::OuterWidth),
        ("outerHeight", WinProp::OuterHeight),
        ("scrollX", WinProp::ScrollX),
        ("pageXOffset", WinProp::ScrollX),
        ("scrollY", WinProp::ScrollY),
        ("pageYOffset", WinProp::ScrollY),
        ("devicePixelRatio", WinProp::DevicePixelRatio),
        ("screenX", WinProp::Zero),
        ("screenY", WinProp::Zero),
        ("screenLeft", WinProp::Zero),
        ("screenTop", WinProp::Zero),
    ] {
        define_accessor(
            &global,
            name,
            NativeFunction::from_copy_closure_with_captures(win_get, prop),
            None,
            context,
        )?;
    }
    for (name, method) in [
        ("scrollTo", WinMethod::ScrollTo),
        ("scroll", WinMethod::ScrollTo),
        ("scrollBy", WinMethod::ScrollBy),
    ] {
        context.register_global_builtin_callable(
            JsString::from(name),
            0,
            NativeFunction::from_copy_closure_with_captures(win_scroll, method),
        )?;
    }
    context.register_global_builtin_callable(js_string!("matchMedia"), 1, NativeFunction::from_fn_ptr(match_media))?;

    // screen: the size as the shell reports it (no system bars known,
    // so the available size is the whole), 24-bit colour, and the
    // orientation from the size.
    let orientation = ObjectInitializer::new(context).build();
    define_accessor(
        &orientation,
        "type",
        NativeFunction::from_copy_closure_with_captures(win_get, WinProp::OrientationType),
        None,
        context,
    )?;
    orientation.set(js_string!("angle"), 0, false, context)?;
    let screen = ObjectInitializer::new(context)
        .property(js_string!("colorDepth"), 24, Attribute::ENUMERABLE)
        .property(js_string!("pixelDepth"), 24, Attribute::ENUMERABLE)
        .property(js_string!("availLeft"), 0, Attribute::ENUMERABLE)
        .property(js_string!("availTop"), 0, Attribute::ENUMERABLE)
        .property(js_string!("orientation"), orientation, Attribute::ENUMERABLE)
        .build();
    for (name, prop) in [
        ("width", WinProp::ScreenWidth),
        ("height", WinProp::ScreenHeight),
        ("availWidth", WinProp::ScreenWidth),
        ("availHeight", WinProp::ScreenHeight),
    ] {
        define_accessor(
            &screen,
            name,
            NativeFunction::from_copy_closure_with_captures(win_get, prop),
            None,
            context,
        )?;
    }
    context.register_global_property(js_string!("screen"), screen, Attribute::ENUMERABLE)?;

    // visualViewport: with no pinch zoom it is the layout viewport; an
    // event target the tab fires `resize` and `scroll` on with window's.
    let target_proto = context
        .get_global_class::<crate::events::EventTargetClass>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message("EventTarget is not registered"))?;
    let id = {
        let shared = dom(context)?;
        let mut dom = shared.borrow_mut();
        let id = dom.events.next_plain_id();
        dom.visual_viewport = Some(id);
        id
    };
    let vv = JsObject::from_proto_and_data(target_proto, PlainTarget { id });
    for (name, prop) in [
        ("width", WinProp::InnerWidth),
        ("height", WinProp::InnerHeight),
        ("pageLeft", WinProp::ScrollX),
        ("pageTop", WinProp::ScrollY),
        ("offsetLeft", WinProp::Zero),
        ("offsetTop", WinProp::Zero),
    ] {
        define_accessor(
            &vv,
            name,
            NativeFunction::from_copy_closure_with_captures(win_get, prop),
            None,
            context,
        )?;
    }
    vv.set(js_string!("scale"), 1, false, context)?;
    crate::events::add_handler_properties(&vv, &["onresize", "onscroll", "onscrollend"], context)?;
    dom(context)?.borrow_mut().plain_objects.insert(id, vv.clone());
    context.register_global_property(js_string!("visualViewport"), vv, Attribute::ENUMERABLE)?;
    Ok(())
}

// ----- matchMedia -----

/// A `MediaQueryList` a script holds: its query, and whether it matched
/// when last evaluated, so a change can be reported.
pub(crate) struct MediaList {
    pub id: u64,
    pub query: String,
    pub list: MediaQueryList,
    pub matches: bool,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct MediaQueryListClass;

impl Class for MediaQueryListClass {
    const NAME: &'static str = "MediaQueryList";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (name, which) in [("matches", true), ("media", false)] {
            let get = getter(
                class.context(),
                name,
                NativeFunction::from_copy_closure_with_captures(mql_get, which),
            );
            class.accessor(JsString::from(name), Some(get), None, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
        }
        // The legacy pair: `change` listeners.
        class.method(
            js_string!("addListener"),
            1,
            NativeFunction::from_copy_closure_with_captures(mql_listener, true),
        );
        class.method(
            js_string!("removeListener"),
            1,
            NativeFunction::from_copy_closure_with_captures(mql_listener, false),
        );
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

fn this_plain_id(this: &JsValue) -> JsResult<u64> {
    this.as_object()
        .and_then(|o| o.downcast_ref::<PlainTarget>().map(|p| p.id))
        .ok_or_else(illegal_invocation)
}

fn mql_get(this: &JsValue, _: &[JsValue], matches: &bool, context: &mut Context) -> JsResult<JsValue> {
    let id = this_plain_id(this)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    let Some(entry) = dom.media_lists.iter().find(|m| m.id == id) else {
        return Err(illegal_invocation());
    };
    if *matches {
        let now = dom.view.as_ref().map_or(entry.matches, |v| entry.list.evaluate(&viewport_of(v)));
        Ok(now.into())
    } else {
        Ok(js_str(&entry.query))
    }
}

fn mql_listener(this: &JsValue, args: &[JsValue], add: &bool, context: &mut Context) -> JsResult<JsValue> {
    let target = this.clone();
    let name = if *add { "addEventListener" } else { "removeEventListener" };
    let f = target
        .as_object()
        .ok_or_else(illegal_invocation)?
        .get(JsString::from(name), context)?;
    if let Some(f) = f.as_object().filter(JsObject::is_callable) {
        f.call(&target, &[js_str("change"), args.get_or_undefined(0).clone()], context)?;
    }
    Ok(JsValue::undefined())
}

fn viewport_of(view: &View) -> Viewport {
    Viewport {
        width: view.viewport.0,
        height: view.viewport.1,
        scale_factor: view.scale,
        prefers_dark: false,
    }
}

/// `matchMedia(query)`: a `MediaQueryList` evaluated against the
/// viewport, firing `change` when the answer changes.
fn match_media(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let query = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let query = query.trim().to_owned();
    let list = MediaQueryList::parse_str(&query);
    let proto = prototype_of::<MediaQueryListClass>(context)?;
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    let id = dom.events.next_plain_id();
    let matches = dom.view.as_ref().is_some_and(|v| list.evaluate(&viewport_of(v)));
    let object = JsObject::from_proto_and_data(proto, PlainTarget { id });
    dom.media_lists.push(MediaList {
        id,
        query,
        list,
        matches,
    });
    dom.plain_objects.insert(id, object.clone());
    drop(dom);
    crate::events::add_handler_properties(&object, &["onchange"], context)?;
    Ok(object.into())
}

/// The viewport changed: every `MediaQueryList` whose answer flipped
/// gets a `change` (`MediaQueryListEvent`, with `media` and `matches`).
pub(crate) fn report_media_changes(context: &mut Context) -> JsResult<()> {
    let changed: Vec<(u64, String, bool)> = {
        let shared = dom(context)?;
        let mut dom = shared.borrow_mut();
        let Some(view) = dom.view.as_ref() else { return Ok(()) };
        let vp = viewport_of(view);
        let mut changed = Vec::new();
        for m in &mut dom.media_lists {
            let now = m.list.evaluate(&vp);
            if now != m.matches {
                m.matches = now;
                changed.push((m.id, m.query.clone(), now));
            }
        }
        changed
    };
    for (id, media, matches) in changed {
        let event = new_event(
            "change",
            UiEventInit {
                class: UiClass::MediaQueryList,
                media,
                media_matches: matches,
                ..UiEventInit::default()
            },
            context,
        )?;
        dispatch(&event, EventTargetRef::Plain(id), None, context)?;
    }
    Ok(())
}
