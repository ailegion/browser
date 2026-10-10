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
use browser_layout::{Fragment, ImageSizes, LayoutEngine, LayoutTree, Rect};
use browser_style::media::MediaQueryList;
use browser_style::ua::ua_stylesheet;
use browser_style::{
    ElementStates, InteractionDeps, Origin, Position, Rule, Sides, StyleMap, Stylesheet, Stylist, Viewport,
    compute_styles_with,
};
use html5ever::{local_name, ns};
use url::Url;

use crate::dom::{Dom, DomNode, dom, illegal_invocation, wrap};
use crate::events::{EventTargetRef, PlainTarget, UiClass, UiEventInit, dispatch, new_event};
use crate::{getter, host_state, js_str, setter};

/// Nested `@import` depth followed, as the tab's loader.
const MAX_IMPORT_DEPTH: u8 = 6;

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
/// and the used padding and margin layout gave it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FragmentBox {
    order: u32,
    pub(crate) rect: Rect,
    pub(crate) padding: Sides<f32>,
    pub(crate) margin: Sides<f32>,
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
    pub fn ensure_layout(&mut self, doc: &Document, states: &ElementStates, generation: u64) {
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
        self.styles = compute_styles_with(doc, &stylist, &vp, states);
        self.deps = stylist.interaction_deps();
        let Some(engine) = &mut self.engine else { return };
        let tree = engine.layout(doc, &self.styles, self.viewport.0, self.viewport.1, &self.images);
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
        let (cw, ch) = self.content_size();
        self.scroll.0 = self.scroll.0.clamp(0.0, (cw - self.viewport.0).max(0.0));
        self.scroll.1 = self.scroll.1.clamp(0.0, (ch - self.viewport.1).max(0.0));
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

    fn index(&mut self) -> &HashMap<NodeId, Vec<FragmentBox>> {
        if self.rect_index.is_none() {
            let mut index: HashMap<NodeId, Vec<FragmentBox>> = HashMap::new();
            let mut order = 0u32;
            fn visit(f: &Fragment, order: &mut u32, index: &mut HashMap<NodeId, Vec<FragmentBox>>) {
                if let Some(n) = f.node {
                    index.entry(n).or_default().push(FragmentBox {
                        order: *order,
                        rect: f.rect,
                        padding: f.padding,
                        margin: f.margin,
                    });
                    *order += 1;
                }
                for c in &f.children {
                    visit(c, order, index);
                }
            }
            if let Some(tree) = &self.layout {
                visit(&tree.root, &mut order, &mut index);
            }
            self.rect_index = Some(index);
        }
        self.rect_index.as_ref().expect("built")
    }

    /// The boxes of `id` in page coordinates, in tree order: its own
    /// fragments, or, for an inline element (which has no box of its
    /// own), the fragments of what it contains, one per line.
    pub fn rects_of(&mut self, doc: &Document, id: NodeId) -> Vec<Rect> {
        let index = self.index();
        if let Some(own) = index.get(&id) {
            return own.iter().map(|b| b.rect).collect();
        }
        if !doc.contains(id) {
            return Vec::new();
        }
        let mut found: Vec<(u32, Rect)> = Vec::new();
        for d in doc.descendants(id) {
            if let Some(boxes) = index.get(&d) {
                found.extend(boxes.iter().map(|b| (b.order, b.rect)));
            }
        }
        found.sort_by_key(|&(o, _)| o);
        // A box fragment covers the fragments inside it: keep the outer.
        let mut out: Vec<Rect> = Vec::new();
        for (_, r) in found {
            if out.iter().any(|o| o.x <= r.x && o.y <= r.y && o.right() >= r.right() && o.bottom() >= r.bottom()) {
                continue;
            }
            out.push(r);
        }
        out
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

    /// The smallest rectangle around all of an element's boxes; zero
    /// when it has none (not rendered).
    pub fn bounding(&mut self, doc: &Document, id: NodeId) -> Rect {
        union(&self.rects_of(doc, id))
    }

    /// How far the boxes inside `id` reach past its padding box origin:
    /// `scrollWidth`/`scrollHeight` without a scroll container.
    fn content_extent(&mut self, doc: &Document, id: NodeId, padding_box: Rect) -> (f32, f32) {
        let index = self.index();
        let (mut w, mut h) = (padding_box.width, padding_box.height);
        for d in doc.descendants(id) {
            if let Some(boxes) = index.get(&d) {
                for b in boxes {
                    w = w.max(b.rect.right() - padding_box.x);
                    h = h.max(b.rect.bottom() - padding_box.y);
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
        ..
    } = &mut *guard;
    let Some(view) = view.as_mut() else {
        return Ok(None);
    };
    view.ensure_layout(doc, states, *generation);
    Ok(Some(f(view, doc, quirks)))
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
    let rect = view.bounding(doc, id);
    let style = view.styles.get(id).cloned();
    let pad_box = style.as_ref().map_or(rect, |s| padding_box(rect, s));
    let num = |v: f32| Geo::Num(f64::from(v.round()));
    match prop {
        GeoProp::OffsetWidth => num(rect.width),
        GeoProp::OffsetHeight => num(rect.height),
        GeoProp::OffsetLeft | GeoProp::OffsetTop => {
            let parent = offset_parent(view, doc, id);
            let origin = match parent {
                Some(p) => {
                    let pr = view.bounding(doc, p);
                    view.styles.get(p).map_or(pr, |s| padding_box(pr, s))
                }
                None => Rect::default(),
            };
            if matches!(prop, GeoProp::OffsetLeft) { num(rect.x - origin.x) } else { num(rect.y - origin.y) }
        }
        GeoProp::OffsetParent => Geo::Node(offset_parent(view, doc, id)),
        // The scrolling element's client box is the viewport.
        GeoProp::ClientWidth => num(if scroller { view.viewport.0 } else { pad_box.width }),
        GeoProp::ClientHeight => num(if scroller { view.viewport.1 } else { pad_box.height }),
        GeoProp::ClientLeft => num(if scroller { 0.0 } else { pad_box.x - rect.x }),
        GeoProp::ClientTop => num(if scroller { 0.0 } else { pad_box.y - rect.y }),
        GeoProp::ScrollWidth | GeoProp::ScrollHeight => {
            let (w, h) = if scroller { view.content_size() } else { view.content_extent(doc, id, pad_box) };
            num(if matches!(prop, GeoProp::ScrollWidth) { w } else { h })
        }
        GeoProp::ScrollTop => Geo::Num(f64::from(if scroller { view.scroll.1 } else { 0.0 })),
        GeoProp::ScrollLeft => Geo::Num(f64::from(if scroller { view.scroll.0 } else { 0.0 })),
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
/// scrolling element; other elements do not scroll (no scroll
/// containers yet, Phase 5).
fn geo_set(this: &JsValue, args: &[JsValue], prop: &GeoProp, context: &mut Context) -> JsResult<JsValue> {
    let node = this
        .as_object()
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)?;
    let Some(id) = node.id else { return Err(illegal_invocation()) };
    let v = args.get_or_undefined(0).to_number(context)? as f32;
    with_layout(context, |view, doc, quirks| {
        if scrolling_element(doc, quirks) == Some(id) {
            match prop {
                GeoProp::ScrollTop => view.set_scroll(view.scroll.0, v),
                GeoProp::ScrollLeft => view.set_scroll(v, view.scroll.1),
                _ => {}
            }
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
    /// scrolling element, nothing elsewhere.
    ScrollTo,
    ScrollBy,
}

/// Where `scrollIntoView` puts an edge: `start`, `center`, `end` or
/// `nearest` (nothing when already in view).
fn align(block: &str, lo: f32, hi: f32, view_lo: f32, view_size: f32) -> f32 {
    match block {
        "start" => lo,
        "center" => (lo + hi) / 2.0 - view_size / 2.0,
        "end" => hi - view_size,
        _ => {
            if lo >= view_lo && hi <= view_lo + view_size {
                view_lo
            } else if (lo - view_lo).abs() < (hi - (view_lo + view_size)).abs() {
                lo
            } else {
                hi - view_size
            }
        }
    }
}

/// `scrollTo(x, y)`, `scrollTo({left, top})`, `scrollBy(...)`: the
/// target offset, with missing parts kept (`by`: added).
fn scroll_args(args: &[JsValue], current: (f32, f32), by: bool, context: &mut Context) -> JsResult<(f32, f32)> {
    let first = args.get_or_undefined(0);
    let (dx, dy) = if let Some(o) = first.as_object() {
        let num = |name: &str, context: &mut Context| -> JsResult<Option<f32>> {
            let v = o.get(JsString::from(name), context)?;
            Ok(if v.is_undefined() { None } else { Some(v.to_number(context)? as f32) })
        };
        (num("left", context)?, num("top", context)?)
    } else {
        let x = if first.is_undefined() { None } else { Some(first.to_number(context)? as f32) };
        let second = args.get_or_undefined(1);
        let y = if second.is_undefined() { None } else { Some(second.to_number(context)? as f32) };
        (x, y)
    };
    Ok(if by {
        (current.0 + dx.unwrap_or(0.0), current.1 + dy.unwrap_or(0.0))
    } else {
        (dx.unwrap_or(current.0), dy.unwrap_or(current.1))
    })
}

fn geo_call(this: &JsValue, args: &[JsValue], method: &GeoMethod, context: &mut Context) -> JsResult<JsValue> {
    let node = this
        .as_object()
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)?;
    let Some(id) = node.id else { return Err(illegal_invocation()) };
    // Arguments first: reading them runs script.
    let (block, inline) = match method {
        GeoMethod::ScrollIntoView => {
            let arg = args.get_or_undefined(0);
            if let Some(o) = arg.as_object() {
                let s = |name: &str, default: &str, context: &mut Context| -> JsResult<String> {
                    let v = o.get(JsString::from(name), context)?;
                    Ok(if v.is_undefined() {
                        default.to_owned()
                    } else {
                        v.to_string(context)?.to_std_string_escaped()
                    })
                };
                (s("block", "start", context)?, s("inline", "nearest", context)?)
            } else if arg.is_undefined() || arg.to_boolean() {
                ("start".to_owned(), "nearest".to_owned())
            } else {
                ("end".to_owned(), "nearest".to_owned())
            }
        }
        _ => (String::new(), String::new()),
    };
    let current = with_layout(context, |view, _, _| view.scroll)?.unwrap_or_default();
    let target = match method {
        GeoMethod::ScrollTo => Some(scroll_args(args, current, false, context)?),
        GeoMethod::ScrollBy => Some(scroll_args(args, current, true, context)?),
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
                let r = view.bounding(doc, id);
                let (vw, vh) = view.viewport;
                let (sx, sy) = view.scroll;
                let y = align(&block, r.y, r.bottom(), sy, vh);
                let x = align(&inline, r.x, r.right(), sx, vw);
                view.set_scroll(x, y);
                Vec::new()
            }
            GeoMethod::ScrollTo | GeoMethod::ScrollBy => {
                if scrolling_element(doc, quirks) == Some(id)
                    && let Some((x, y)) = target
                {
                    view.set_scroll(x, y);
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
    let (x, y) = scroll_args(args, current, matches!(method, WinMethod::ScrollBy), context)?;
    with_layout(context, |view, _, _| view.set_scroll(x, y))?;
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
    crate::events::add_handler_properties(&vv, &["onresize", "onscroll"], context)?;
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
