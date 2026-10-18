//! CSS Font Loading ("Working sites first" line 6): `FontFace`,
//! `document.fonts` (a `FontFaceSet`) and `FontFaceSetLoadEvent`.
//!
//! The tab loads fonts; this module keeps the objects scripts see. The
//! set holds the document's `@font-face` rules as CSS-connected faces (in
//! document order) and the faces a script added, after them. The tab
//! reports each face's load state (`ScriptHost::sync_fonts`), which
//! resolves or rejects the faces' `loaded` promises, switches the set
//! between `loading` and `loaded` (firing `loading`, `loadingdone`,
//! `loadingerror`) and resolves `ready` once nothing is loading and the
//! document's layout has settled. A script's `load()` and the bytes of a
//! face made from a buffer go to the tab as `FontRequest`s.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::builtins::{JsArray, JsArrayBuffer, JsDataView, JsPromise, JsTypedArray};
use boa_engine::property::Attribute;
use boa_engine::{Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsSymbol, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};
use browser_style::fontface::{self, FontFaceRule};

use crate::dom::{dom, dom_exception};
use crate::events::{EventTargetRef, PlainTarget};
use crate::{getter, js_str, setter};

/// A face's load state, as `FontFace.status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontLoadStatus {
    Unloaded,
    Loading,
    Loaded,
    Error,
}

impl FontLoadStatus {
    fn keyword(self) -> &'static str {
        match self {
            FontLoadStatus::Unloaded => "unloaded",
            FontLoadStatus::Loading => "loading",
            FontLoadStatus::Loaded => "loaded",
            FontLoadStatus::Error => "error",
        }
    }
}

/// Which face the tab and the script mean: an `@font-face` rule (by the
/// tab's key for it) or a face a script made (by its id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaceRef {
    Css(usize),
    Script(u64),
}

/// What a script asked of the tab's font loading.
#[derive(Debug, Clone)]
pub enum FontRequest {
    /// Load the face now (`FontFace.load()`, `FontFaceSet.load()`),
    /// whether text uses it or not.
    Load(FaceRef),
    /// A face made from bytes: check them (the face loads or errors) and
    /// keep them for when the face is in the set.
    Data { id: u64, bytes: Arc<[u8]> },
}

/// A face a script made, as the tab needs it.
#[derive(Debug, Clone)]
pub struct ScriptFace {
    pub id: u64,
    pub rule: Arc<FontFaceRule>,
    /// In `document.fonts`: the face takes part in the document's font
    /// matching.
    pub in_set: bool,
}

struct Face {
    id: u64,
    object: JsObject,
    /// The tab's key of the `@font-face` rule a CSS-connected face is.
    css: Option<usize>,
    rule: Arc<FontFaceRule>,
    /// `featureSettings` and `variationSettings` as given (the descriptors
    /// themselves are not applied; Phase 5, Style).
    feature_settings: String,
    variation_settings: String,
    in_set: bool,
    status: FontLoadStatus,
    loaded: JsPromise,
    resolvers: Option<ResolvingFunctions>,
}

/// The faces and the set (`document.fonts`).
#[derive(Default)]
pub(crate) struct Fonts {
    set: Option<JsObject>,
    set_id: u64,
    faces: Vec<Face>,
    next_id: u64,
    /// `document.fonts.status` is `loading`.
    loading: bool,
    ready: Option<(JsPromise, Option<ResolvingFunctions>)>,
    /// The faces of the set that finished while it was loading.
    period_loaded: Vec<u64>,
    period_failed: Vec<u64>,
    requests: Vec<FontRequest>,
}

pub(crate) type SharedFonts = Rc<RefCell<Fonts>>;

fn fonts(context: &mut Context) -> JsResult<SharedFonts> {
    context
        .get_data::<SharedFonts>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no font set").into())
}

/// What a `FontFace` object carries: its face's id.
#[derive(Debug, Trace, Finalize, JsData)]
struct FaceData {
    #[unsafe_ignore_trace]
    id: u64,
}

fn this_face(this: &JsValue) -> JsResult<u64> {
    this.as_object()
        .and_then(|o| o.downcast_ref::<FaceData>().map(|d| d.id))
        .ok_or_else(|| JsNativeError::typ().with_message("Illegal invocation").into())
}

/// The `FontFace` attributes and the descriptors they are.
const DESCRIPTORS: &[(&str, &str)] = &[
    ("style", "font-style"),
    ("weight", "font-weight"),
    ("stretch", "font-stretch"),
    ("unicodeRange", "unicode-range"),
    ("display", "font-display"),
    ("ascentOverride", "ascent-override"),
    ("descentOverride", "descent-override"),
    ("lineGapOverride", "line-gap-override"),
    ("sizeAdjust", "size-adjust"),
    ("featureSettings", "font-feature-settings"),
    ("variationSettings", "font-variation-settings"),
];

#[derive(Debug, Trace, Finalize, JsData)]
struct FontFaceClass;

impl Class for FontFaceClass {
    const NAME: &'static str = "FontFace";
    const LENGTH: usize = 2;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let rw = Attribute::ENUMERABLE | Attribute::CONFIGURABLE;
        let get = getter(class.context(), "family", NativeFunction::from_fn_ptr(face_family_get));
        let set = setter(class.context(), "family", NativeFunction::from_fn_ptr(face_family_set));
        class.accessor(js_string!("family"), Some(get), Some(set), rw);
        for (index, (name, _)) in DESCRIPTORS.iter().enumerate() {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(face_descriptor_get, index));
            let set = setter(class.context(), name, NativeFunction::from_copy_closure_with_captures(face_descriptor_set, index));
            class.accessor(JsString::from(*name), Some(get), Some(set), rw);
        }
        let status = getter(class.context(), "status", NativeFunction::from_fn_ptr(face_status_get));
        class.accessor(js_string!("status"), Some(status), None, rw);
        let loaded = getter(class.context(), "loaded", NativeFunction::from_fn_ptr(face_loaded_get));
        class.accessor(js_string!("loaded"), Some(loaded), None, rw);
        class.method(js_string!("load"), 0, NativeFunction::from_fn_ptr(face_load));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    /// `new FontFace(family, source, descriptors)`. A family or
    /// descriptor that does not parse does not throw: the face is in the
    /// `error` state and `loaded` rejects with a `SyntaxError`.
    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(JsNativeError::typ().with_message("Failed to construct 'FontFace': Please use the 'new' operator").into());
        }
        if args.len() < 2 {
            return Err(JsNativeError::typ()
                .with_message(format!("Failed to construct 'FontFace': 2 arguments required, but only {} present.", args.len()))
                .into());
        }
        let family_text = args[0].to_string(context)?.to_std_string_escaped();
        let base = crate::host_state(context)?.borrow().info.url.clone();
        let mut syntax_error: Option<String> = None;
        let mut rule = FontFaceRule::default();
        match fontface::parse_family(&family_text) {
            Some(f) => rule.family = f,
            None => {
                rule.family = Arc::from(family_text.as_str());
                syntax_error = Some(format!("Failed to set '{family_text}' as a property value."));
            }
        }
        // The source: bytes, or the `src` descriptor's text.
        let mut bytes: Option<Arc<[u8]>> = None;
        let source = &args[1];
        if let Some(b) = source.as_object().map(|o| buffer_bytes(&o, context)).transpose()?.flatten() {
            bytes = Some(Arc::from(b));
        } else {
            let text = source.to_string(context)?.to_std_string_escaped();
            if !fontface::set_descriptor(&mut rule, "src", &text, base.as_ref()) && syntax_error.is_none() {
                syntax_error = Some(format!("Failed to set '{text}' as a property value."));
            }
        }
        let mut feature_settings = "normal".to_owned();
        let mut variation_settings = "normal".to_owned();
        if let Some(desc) = args.get_or_undefined(2).as_object() {
            for (name, css) in DESCRIPTORS {
                let v = desc.get(JsString::from(*name), context)?;
                if v.is_undefined() {
                    continue;
                }
                let text = v.to_string(context)?.to_std_string_escaped();
                let ok = match *css {
                    "font-feature-settings" => {
                        feature_settings = text.clone();
                        true
                    }
                    "font-variation-settings" => {
                        variation_settings = text.clone();
                        true
                    }
                    _ => fontface::set_descriptor(&mut rule, css, &text, None),
                };
                if !ok && syntax_error.is_none() {
                    syntax_error = Some(format!("Failed to set '{text}' as a property value."));
                }
            }
        }
        let proto = new_target
            .as_object()
            .map(|o| o.get(js_string!("prototype"), context))
            .transpose()?
            .and_then(|p| p.as_object())
            .map_or_else(|| prototype::<FontFaceClass>(context), Ok)?;
        let shared = fonts(context)?;
        let id = {
            let mut f = shared.borrow_mut();
            f.next_id += 1;
            f.next_id
        };
        let object = JsObject::from_proto_and_data(proto, FaceData { id });
        let (loaded, resolvers) = JsPromise::new_pending(context);
        let mut face = Face {
            id,
            object: object.clone(),
            css: None,
            rule: Arc::new(rule),
            feature_settings,
            variation_settings,
            in_set: false,
            status: FontLoadStatus::Unloaded,
            loaded,
            resolvers: Some(resolvers),
        };
        if let Some(message) = syntax_error {
            face.status = FontLoadStatus::Error;
            if let Some(r) = face.resolvers.take() {
                let err = dom_exception("SyntaxError", &message, context).into_opaque(context)?;
                r.reject.call(&JsValue::undefined(), &[err], context)?;
            }
            mark_handled(&face.loaded, context)?;
        } else if let Some(bytes) = bytes {
            // Bytes load at once (the tab checks them; the answer comes
            // back as for any face).
            face.status = FontLoadStatus::Loading;
            shared.borrow_mut().requests.push(FontRequest::Data { id, bytes });
        }
        shared.borrow_mut().faces.push(face);
        Ok(object)
    }
}

/// A rejected `loaded` promise nobody listens to is not an unhandled
/// rejection a page sees reported (a `FontFace` that failed is normal).
fn mark_handled(promise: &JsPromise, context: &mut Context) -> JsResult<()> {
    let noop = NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::undefined()));
    let f = boa_engine::object::FunctionObjectBuilder::new(context.realm(), noop).build();
    promise.then(None, Some(f), context)?;
    Ok(())
}

/// The bytes of an `ArrayBuffer`, a typed array or a `DataView`; `None`
/// for any other object.
fn buffer_bytes(o: &JsObject, context: &mut Context) -> JsResult<Option<Vec<u8>>> {
    if let Ok(buffer) = JsArrayBuffer::from_object(o.clone()) {
        return Ok(Some(buffer.to_vec().unwrap_or_default()));
    }
    let (buffer, offset, length) = if let Ok(view) = JsTypedArray::from_object(o.clone()) {
        (view.buffer(context)?, view.byte_offset(context)?, view.byte_length(context)?)
    } else if let Ok(view) = JsDataView::from_object(o.clone()) {
        (view.buffer(context)?, view.byte_offset(context)? as usize, view.byte_length(context)? as usize)
    } else {
        return Ok(None);
    };
    let Some(buffer) = buffer.as_object().and_then(|b| JsArrayBuffer::from_object(b).ok()) else {
        return Ok(Some(Vec::new()));
    };
    let all = buffer.to_vec().unwrap_or_default();
    Ok(Some(all.get(offset..offset + length).map(<[u8]>::to_vec).unwrap_or_default()))
}

fn prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

fn with_face<T>(id: u64, context: &mut Context, f: impl FnOnce(&mut Face) -> T) -> JsResult<T> {
    let shared = fonts(context)?;
    let mut fonts = shared.borrow_mut();
    let face = fonts
        .faces
        .iter_mut()
        .find(|x| x.id == id)
        .ok_or_else(|| JsNativeError::typ().with_message("Illegal invocation"))?;
    Ok(f(face))
}

fn face_family_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_face(this)?;
    with_face(id, context, |f| js_str(&f.rule.family))
}

fn face_family_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_face(this)?;
    let text = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let Some(family) = fontface::parse_family(&text) else {
        return Err(dom_exception("SyntaxError", &format!("Failed to set '{text}' as a property value."), context));
    };
    with_face(id, context, |f| {
        let mut rule = (*f.rule).clone();
        rule.family = family;
        f.rule = Arc::new(rule);
    })?;
    Ok(JsValue::undefined())
}

fn face_descriptor_get(this: &JsValue, _: &[JsValue], index: &usize, context: &mut Context) -> JsResult<JsValue> {
    let id = this_face(this)?;
    let (_, css) = DESCRIPTORS[*index];
    with_face(id, context, |f| match css {
        "font-feature-settings" => js_str(&f.feature_settings),
        "font-variation-settings" => js_str(&f.variation_settings),
        _ => js_str(&fontface::descriptor_css(&f.rule, css)),
    })
}

fn face_descriptor_set(this: &JsValue, args: &[JsValue], index: &usize, context: &mut Context) -> JsResult<JsValue> {
    let id = this_face(this)?;
    let (_, css) = DESCRIPTORS[*index];
    let text = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let ok = with_face(id, context, |f| match css {
        "font-feature-settings" => {
            f.feature_settings = text.clone();
            true
        }
        "font-variation-settings" => {
            f.variation_settings = text.clone();
            true
        }
        _ => {
            let mut rule = (*f.rule).clone();
            let ok = fontface::set_descriptor(&mut rule, css, &text, None);
            if ok {
                f.rule = Arc::new(rule);
            }
            ok
        }
    })?;
    if !ok {
        return Err(dom_exception("SyntaxError", &format!("Failed to set '{text}' as a property value."), context));
    }
    Ok(JsValue::undefined())
}

fn face_status_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_face(this)?;
    with_face(id, context, |f| js_str(f.status.keyword()))
}

fn face_loaded_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_face(this)?;
    with_face(id, context, |f| JsValue::from(f.loaded.clone()))
}

/// `FontFace.load()`: an unloaded face starts loading; the `loaded`
/// promise either way.
fn face_load(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_face(this)?;
    let promise = start_load(id, context)?;
    Ok(promise.into())
}

fn start_load(id: u64, context: &mut Context) -> JsResult<JsPromise> {
    let shared = fonts(context)?;
    let mut fonts = shared.borrow_mut();
    let Some(face) = fonts.faces.iter_mut().find(|x| x.id == id) else {
        return Err(JsNativeError::typ().with_message("Illegal invocation").into());
    };
    let promise = face.loaded.clone();
    if face.status == FontLoadStatus::Unloaded {
        face.status = FontLoadStatus::Loading;
        let target = face.css.map_or(FaceRef::Script(id), FaceRef::Css);
        fonts.requests.push(FontRequest::Load(target));
    }
    Ok(promise)
}

// ----- the set -----

#[derive(Debug, Trace, Finalize, JsData)]
struct FontFaceSetClass;

impl Class for FontFaceSetClass {
    const NAME: &'static str = "FontFaceSet";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let rw = Attribute::ENUMERABLE | Attribute::CONFIGURABLE;
        for (name, f) in [
            ("size", set_size as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>),
            ("status", set_status),
            ("ready", set_ready),
        ] {
            let get = getter(class.context(), name, NativeFunction::from_fn_ptr(f));
            class.accessor(JsString::from(name), Some(get), None, rw);
        }
        class
            .method(js_string!("add"), 1, NativeFunction::from_fn_ptr(set_add))
            .method(js_string!("delete"), 1, NativeFunction::from_fn_ptr(set_delete))
            .method(js_string!("clear"), 0, NativeFunction::from_fn_ptr(set_clear))
            .method(js_string!("has"), 1, NativeFunction::from_fn_ptr(set_has))
            .method(js_string!("forEach"), 1, NativeFunction::from_fn_ptr(set_for_each))
            .method(js_string!("check"), 1, NativeFunction::from_fn_ptr(set_check))
            .method(js_string!("load"), 1, NativeFunction::from_fn_ptr(set_load))
            .method(js_string!("values"), 0, NativeFunction::from_copy_closure_with_captures(set_iterate, 0u8))
            .method(js_string!("keys"), 0, NativeFunction::from_copy_closure_with_captures(set_iterate, 0u8))
            .method(js_string!("entries"), 0, NativeFunction::from_copy_closure_with_captures(set_iterate, 1u8))
            .method(JsSymbol::iterator(), 0, NativeFunction::from_copy_closure_with_captures(set_iterate, 0u8));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

fn this_set(this: &JsValue, context: &mut Context) -> JsResult<SharedFonts> {
    let shared = fonts(context)?;
    let is_set = this
        .as_object()
        .is_some_and(|o| shared.borrow().set.as_ref().is_some_and(|s| JsObject::equals(s, &o)));
    if !is_set {
        return Err(JsNativeError::typ().with_message("Illegal invocation").into());
    }
    Ok(shared)
}

/// The faces of the set, in its order: the CSS-connected ones (document
/// order), then those a script added.
fn members(fonts: &Fonts) -> Vec<&Face> {
    fonts.faces.iter().filter(|f| f.in_set).collect()
}

fn set_size(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    Ok((members(&shared.borrow()).len() as u32).into())
}

fn set_status(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    Ok(js_str(if shared.borrow().loading { "loading" } else { "loaded" }))
}

fn set_ready(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let existing = shared.borrow().ready.as_ref().map(|(p, _)| p.clone());
    let promise = match existing {
        Some(p) => p,
        None => {
            let (p, r) = JsPromise::new_pending(context);
            shared.borrow_mut().ready = Some((p.clone(), Some(r)));
            p
        }
    };
    Ok(promise.into())
}

fn face_arg(v: &JsValue) -> JsResult<u64> {
    v.as_object()
        .and_then(|o| o.downcast_ref::<FaceData>().map(|d| d.id))
        .ok_or_else(|| {
            JsNativeError::typ()
                .with_message("Failed to execute on 'FontFaceSet': parameter 1 is not of type 'FontFace'.")
                .into()
        })
}

/// `add(face)`: a CSS-connected face cannot be added (it is in the set
/// while its rule applies).
fn set_add(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let id = face_arg(args.get_or_undefined(0))?;
    let css = shared.borrow().faces.iter().find(|f| f.id == id).is_some_and(|f| f.css.is_some());
    if css {
        return Err(dom_exception(
            "InvalidModificationError",
            "Failed to execute 'add' on 'FontFaceSet': Cannot add a CSS-connected FontFace.",
            context,
        ));
    }
    {
        let mut fonts = shared.borrow_mut();
        // Added faces go to the end of the set, after the CSS-connected
        // ones and in the order they were added.
        if let Some(i) = fonts.faces.iter().position(|f| f.id == id)
            && !fonts.faces[i].in_set
        {
            let mut face = fonts.faces.remove(i);
            face.in_set = true;
            fonts.faces.push(face);
        }
    }
    update_status(&shared, context)?;
    Ok(this.clone())
}

fn set_delete(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let id = face_arg(args.get_or_undefined(0))?;
    let removed = {
        let mut fonts = shared.borrow_mut();
        match fonts.faces.iter_mut().find(|f| f.id == id) {
            Some(f) if f.css.is_none() && f.in_set => {
                f.in_set = false;
                true
            }
            _ => false,
        }
    };
    update_status(&shared, context)?;
    Ok(removed.into())
}

fn set_clear(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    for f in shared.borrow_mut().faces.iter_mut().filter(|f| f.css.is_none()) {
        f.in_set = false;
    }
    update_status(&shared, context)?;
    Ok(JsValue::undefined())
}

fn set_has(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let Some(id) = args.get_or_undefined(0).as_object().and_then(|o| o.downcast_ref::<FaceData>().map(|d| d.id)) else {
        return Ok(false.into());
    };
    Ok(shared.borrow().faces.iter().any(|f| f.id == id && f.in_set).into())
}

fn member_objects(shared: &SharedFonts) -> Vec<JsObject> {
    members(&shared.borrow()).iter().map(|f| f.object.clone()).collect()
}

fn set_for_each(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let Some(callback) = args.get_or_undefined(0).as_callable() else {
        return Err(JsNativeError::typ()
            .with_message("Failed to execute 'forEach' on 'FontFaceSet': The callback provided as parameter 1 is not a function.")
            .into());
    };
    let this_arg = args.get_or_undefined(1).clone();
    for face in member_objects(&shared) {
        callback.call(&this_arg, &[face.clone().into(), face.into(), this.clone()], context)?;
    }
    Ok(JsValue::undefined())
}

/// `values()`/`keys()`/`[Symbol.iterator]()` (faces) and `entries()`
/// (`[face, face]` pairs, as a set's).
fn set_iterate(this: &JsValue, _: &[JsValue], entries: &u8, context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let faces = member_objects(&shared);
    let items: Vec<JsValue> = if *entries == 1 {
        faces
            .into_iter()
            .map(|f| JsArray::from_iter([JsValue::from(f.clone()), JsValue::from(f)], context).into())
            .collect()
    } else {
        faces.into_iter().map(JsValue::from).collect()
    };
    let array = JsArray::from_iter(items, context);
    let values = array.get(js_string!("values"), context)?;
    let Some(values) = values.as_callable() else {
        return Err(JsNativeError::typ().with_message("no array iterator").into());
    };
    values.call(&array.into(), &[], context)
}

/// The faces of the set a `font` shorthand and a text would use (CSS
/// Font Loading 3, "find the matching font faces"): for each family of the
/// list (generic families have none), the faces font matching picks for
/// the style, weight and stretch (equal faces together), those whose
/// `unicode-range` holds a character of the text. `None` when `font` does
/// not parse as the shorthand.
fn matching_faces(fonts: &Fonts, font: &str, text: &str) -> Option<Vec<u64>> {
    use browser_style::properties::{DeclaredValue, PropertyDeclaration as P};
    let decls = browser_style::stylesheet::parse_declaration_block(&format!("font: {font}"));
    if decls.is_empty() {
        return None;
    }
    let mut families = None;
    let mut style = browser_style::FontStyle::Normal;
    let mut weight = 400.0;
    let mut stretch = 100.0;
    for d in &decls {
        match &d.value {
            DeclaredValue::Value(P::FontFamily(list)) => families = Some(list.clone()),
            DeclaredValue::Value(P::FontStyle(s)) => style = s.computed(),
            // Relative to the initial 400 (the shorthand has no parent).
            DeclaredValue::Value(P::FontWeight(w)) => {
                weight = match w {
                    browser_style::properties::FontWeight::Absolute(n) => f32::from(*n),
                    browser_style::properties::FontWeight::Bolder => 700.0,
                    browser_style::properties::FontWeight::Lighter => 100.0,
                }
            }
            DeclaredValue::Value(P::FontStretch(s)) => stretch = s.percent(),
            DeclaredValue::Value(_) => {}
            // `font: inherit` and the other CSS-wide keywords: no font.
            _ => return None,
        }
    }
    let families = families?;
    let text: Vec<char> = text.chars().collect();
    let in_set: Vec<&Face> = members(fonts);
    let mut out = Vec::new();
    for family in families {
        // A generic family (unquoted keyword) has no `@font-face` faces.
        let name = match family {
            browser_style::properties::FamilyName::Quoted(s) => s,
            browser_style::properties::FamilyName::Ident(s) => {
                if browser_style::properties::GENERIC_FAMILIES.contains(&s.to_ascii_lowercase().as_str()) {
                    continue;
                }
                s
            }
        };
        let candidates: Vec<&Face> = in_set.iter().copied().filter(|f| f.rule.family.eq_ignore_ascii_case(&name)).collect();
        let rules: Vec<&FontFaceRule> = candidates.iter().map(|f| &*f.rule).collect();
        let Some(best) = fontface::best_face(&rules, style, weight, stretch) else { continue };
        let matched = candidates[best].rule.matched();
        for f in candidates.iter().filter(|f| f.rule.matched() == matched) {
            if text.iter().any(|c| f.rule.covers(u32::from(*c))) && !out.contains(&f.id) {
                out.push(f.id);
            }
        }
    }
    Some(out)
}

fn font_and_text(args: &[JsValue], method: &str, context: &mut Context) -> JsResult<(String, String)> {
    if args.is_empty() {
        return Err(JsNativeError::typ()
            .with_message(format!("Failed to execute '{method}' on 'FontFaceSet': 1 argument required, but only 0 present."))
            .into());
    }
    let font = args[0].to_string(context)?.to_std_string_escaped();
    let text = match args.get_or_undefined(1) {
        v if v.is_undefined() => " ".to_owned(),
        v => v.to_string(context)?.to_std_string_escaped(),
    };
    Ok((font, text))
}

/// `check(font, text)`: whether every face the font and text would use is
/// loaded (true when none would: the fallback needs no loading).
fn set_check(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let (font, text) = font_and_text(args, "check", context)?;
    let found = matching_faces(&shared.borrow(), &font, &text);
    let Some(found) = found else {
        return Err(dom_exception(
            "SyntaxError",
            &format!("Failed to execute 'check' on 'FontFaceSet': Could not resolve '{font}' as a font."),
            context,
        ));
    };
    let fonts = shared.borrow();
    Ok(found
        .iter()
        .all(|id| fonts.faces.iter().any(|f| f.id == *id && f.status == FontLoadStatus::Loaded))
        .into())
}

/// `load(font, text)`: load the faces the font and text would use; a
/// promise of them, rejected if one fails.
fn set_load(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = this_set(this, context)?;
    let (font, text) = font_and_text(args, "load", context)?;
    let found = matching_faces(&shared.borrow(), &font, &text);
    let Some(found) = found else {
        let err = dom_exception(
            "SyntaxError",
            &format!("Failed to execute 'load' on 'FontFaceSet': Could not resolve '{font}' as a font."),
            context,
        );
        return Ok(JsPromise::reject(err, context)?.into());
    };
    let mut promises = Vec::new();
    for id in found {
        promises.push(start_load(id, context)?);
    }
    update_status(&shared, context)?;
    Ok(JsPromise::all(promises, context)?.into())
}

// ----- the tab's side -----

/// Make `document.fonts` (called once, after the event classes).
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<FontFaceClass>()?;
    context.register_global_class::<FontFaceSetClass>()?;
    context.register_global_class::<FontFaceSetLoadEventClass>()?;
    crate::events::inherit::<FontFaceSetClass, crate::events::EventTargetClass>(context);
    crate::events::inherit::<FontFaceSetLoadEventClass, crate::events::EventData>(context);
    let shared: SharedFonts = Rc::new(RefCell::new(Fonts::default()));
    context.insert_data(shared.clone());
    let proto = prototype::<FontFaceSetClass>(context)?;
    let id = dom(context)?.borrow_mut().events.next_plain_id();
    let set = JsObject::from_proto_and_data(proto, PlainTarget { id });
    crate::events::add_handler_properties(&set, &["onloading", "onloadingdone", "onloadingerror"], context)?;
    dom(context)?.borrow_mut().plain_objects.insert(id, set.clone());
    let mut fonts = shared.borrow_mut();
    fonts.set = Some(set);
    fonts.set_id = id;
    Ok(())
}

/// `document.fonts`.
pub(crate) fn document_fonts(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(fonts(context)?.borrow().set.clone().map_or(JsValue::undefined(), JsValue::from))
}

#[derive(Debug, Trace, Finalize, JsData)]
struct FontFaceSetLoadEventClass;

impl Class for FontFaceSetLoadEventClass {
    const NAME: &'static str = "FontFaceSetLoadEvent";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let get = getter(class.context(), "fontfaces", NativeFunction::from_fn_ptr(event_fontfaces));
        class.accessor(js_string!("fontfaces"), Some(get), None, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    /// `new FontFaceSetLoadEvent(type, { fontfaces })`.
    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.is_empty() {
            return Err(JsNativeError::typ()
                .with_message("FontFaceSetLoadEvent constructor: 1 argument required, called with new")
                .into());
        }
        let faces: Vec<JsValue> = match args.get_or_undefined(1).as_object() {
            Some(init) => {
                let list = init.get(js_string!("fontfaces"), context)?;
                match list.as_object().and_then(|o| JsArray::from_object(o).ok()) {
                    Some(array) => {
                        let len = array.length(context)?;
                        let mut out = Vec::new();
                        for i in 0..len {
                            let v = array.get(i, context)?;
                            face_arg(&v)?;
                            out.push(v);
                        }
                        out
                    }
                    None if list.is_undefined() => Vec::new(),
                    None => return Err(JsNativeError::typ().with_message("fontfaces is not a sequence").into()),
                }
            }
            None => Vec::new(),
        };
        let proto = prototype::<FontFaceSetLoadEventClass>(context)?;
        let array = frozen_array(faces, context)?;
        crate::events::constructed_event_with_value(args, proto, array, context)
    }
}

fn frozen_array(items: Vec<JsValue>, context: &mut Context) -> JsResult<JsValue> {
    let array = JsArray::from_iter(items, context);
    let object: JsObject = array.into();
    object.set_integrity_level(boa_engine::object::IntegrityLevel::Frozen, context)?;
    Ok(object.into())
}

fn event_fontfaces(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    crate::events::event_value(this).ok_or_else(|| JsNativeError::typ().with_message("Illegal invocation").into())
}

impl Fonts {
    /// Reconcile the CSS-connected faces with the document's `@font-face`
    /// rules, in order (a rule gone leaves the set; its `FontFace` stays
    /// as the script holds it), then the load states.
    fn apply(&mut self, css: &[(usize, Arc<FontFaceRule>)], statuses: &[(FaceRef, FontLoadStatus)], context: &mut Context) -> JsResult<Vec<(u64, FontLoadStatus)>> {
        let mut css_faces: Vec<Face> = Vec::new();
        for (key, rule) in css {
            match self.faces.iter().position(|f| f.css == Some(*key)) {
                Some(i) => {
                    let mut f = self.faces.remove(i);
                    f.rule = rule.clone();
                    f.in_set = true;
                    css_faces.push(f);
                }
                None => {
                    self.next_id += 1;
                    let id = self.next_id;
                    let proto = prototype::<FontFaceClass>(context)?;
                    let object = JsObject::from_proto_and_data(proto, FaceData { id });
                    let (loaded, resolvers) = JsPromise::new_pending(context);
                    css_faces.push(Face {
                        id,
                        object,
                        css: Some(*key),
                        rule: rule.clone(),
                        feature_settings: "normal".to_owned(),
                        variation_settings: "normal".to_owned(),
                        in_set: true,
                        status: FontLoadStatus::Unloaded,
                        loaded,
                        resolvers: Some(resolvers),
                    });
                }
            }
        }
        // CSS-connected faces whose rule is gone: out of the set.
        for f in self.faces.iter_mut().filter(|f| f.css.is_some()) {
            f.in_set = false;
        }
        let rest = std::mem::take(&mut self.faces);
        self.faces = css_faces;
        self.faces.extend(rest);
        let mut changed = Vec::new();
        for (target, status) in statuses {
            let Some(face) = self.faces.iter_mut().find(|f| match target {
                FaceRef::Css(k) => f.css == Some(*k),
                FaceRef::Script(id) => f.css.is_none() && f.id == *id,
            }) else {
                continue;
            };
            // A settled face stays settled.
            if face.status == *status || matches!(face.status, FontLoadStatus::Loaded | FontLoadStatus::Error) {
                continue;
            }
            face.status = *status;
            changed.push((face.id, *status));
        }
        Ok(changed)
    }
}

/// The tab's report: settle the `loaded` promises of faces that finished,
/// then the set's status, events and `ready`.
pub(crate) fn sync(
    css: &[(usize, Arc<FontFaceRule>)],
    statuses: &[(FaceRef, FontLoadStatus)],
    settled: bool,
    context: &mut Context,
) -> JsResult<()> {
    let shared = fonts(context)?;
    let changed = {
        let mut fonts = shared.borrow_mut();
        fonts.apply(css, statuses, context)?
    };
    for (id, status) in changed {
        let (object, resolvers, promise, in_set) = {
            let mut fonts = shared.borrow_mut();
            let Some(face) = fonts.faces.iter_mut().find(|f| f.id == id) else { continue };
            let resolvers = match status {
                FontLoadStatus::Loaded | FontLoadStatus::Error => face.resolvers.take(),
                _ => None,
            };
            (face.object.clone(), resolvers, face.loaded.clone(), face.in_set)
        };
        if let Some(r) = resolvers {
            if status == FontLoadStatus::Loaded {
                r.resolve.call(&JsValue::undefined(), &[object.into()], context)?;
            } else {
                let err = dom_exception("NetworkError", "A network error occurred.", context).into_opaque(context)?;
                r.reject.call(&JsValue::undefined(), &[err], context)?;
                mark_handled(&promise, context)?;
            }
        }
        if in_set {
            let mut fonts = shared.borrow_mut();
            match status {
                FontLoadStatus::Loaded => fonts.period_loaded.push(id),
                FontLoadStatus::Error => fonts.period_failed.push(id),
                _ => {}
            }
        }
    }
    update_status(&shared, context)?;
    if settled {
        resolve_ready(&shared, context)?;
    }
    Ok(())
}

/// Switch the set between `loading` and `loaded` as its faces load,
/// firing the events of each switch (CSS Font Loading 3 §2.3).
fn update_status(shared: &SharedFonts, context: &mut Context) -> JsResult<()> {
    let (was, now, set_id) = {
        let fonts = shared.borrow();
        let now = members(&fonts).iter().any(|f| f.status == FontLoadStatus::Loading);
        (fonts.loading, now, fonts.set_id)
    };
    if was == now {
        return Ok(());
    }
    if now {
        let faces: Vec<JsValue> = {
            let mut fonts = shared.borrow_mut();
            fonts.loading = true;
            // A resolved `ready` is replaced by a pending one.
            if fonts.ready.as_ref().is_some_and(|(_, r)| r.is_none()) {
                fonts.ready = None;
            }
            members(&fonts)
                .iter()
                .filter(|f| f.status == FontLoadStatus::Loading)
                .map(|f| f.object.clone().into())
                .collect()
        };
        if shared.borrow().ready.is_none() {
            let (p, r) = JsPromise::new_pending(context);
            shared.borrow_mut().ready = Some((p, Some(r)));
        }
        fire(set_id, "loading", faces, context)?;
    } else {
        let (loaded, failed) = {
            let mut fonts = shared.borrow_mut();
            fonts.loading = false;
            let ids_loaded = std::mem::take(&mut fonts.period_loaded);
            let ids_failed = std::mem::take(&mut fonts.period_failed);
            let object = |id: &u64| fonts.faces.iter().find(|f| f.id == *id).map(|f| JsValue::from(f.object.clone()));
            (
                ids_loaded.iter().filter_map(object).collect::<Vec<_>>(),
                ids_failed.iter().filter_map(object).collect::<Vec<_>>(),
            )
        };
        fire(set_id, "loadingdone", loaded, context)?;
        if !failed.is_empty() {
            fire(set_id, "loadingerror", failed, context)?;
        }
    }
    Ok(())
}

fn resolve_ready(shared: &SharedFonts, context: &mut Context) -> JsResult<()> {
    if shared.borrow().loading {
        return Ok(());
    }
    let set = shared.borrow().set.clone();
    let resolvers = {
        let mut fonts = shared.borrow_mut();
        if fonts.ready.is_none() {
            let (p, r) = JsPromise::new_pending(context);
            fonts.ready = Some((p, Some(r)));
        }
        fonts.ready.as_mut().and_then(|(_, r)| r.take())
    };
    if let (Some(r), Some(set)) = (resolvers, set) {
        r.resolve.call(&JsValue::undefined(), &[set.into()], context)?;
    }
    Ok(())
}

fn fire(set_id: u64, kind: &str, faces: Vec<JsValue>, context: &mut Context) -> JsResult<()> {
    let proto = prototype::<FontFaceSetLoadEventClass>(context)?;
    let array = frozen_array(faces, context)?;
    let event = crate::events::trusted_event_with_value(kind, proto, array, context)?;
    crate::events::dispatch(&event, EventTargetRef::Plain(set_id), None, context)?;
    Ok(())
}

pub(crate) fn take_requests(context: &mut Context) -> Vec<FontRequest> {
    fonts(context).map(|f| std::mem::take(&mut f.borrow_mut().requests)).unwrap_or_default()
}

pub(crate) fn script_faces(context: &mut Context) -> Vec<ScriptFace> {
    fonts(context)
        .map(|f| {
            f.borrow()
                .faces
                .iter()
                .filter(|x| x.css.is_none() && x.status != FontLoadStatus::Error)
                .map(|x| ScriptFace { id: x.id, rule: x.rule.clone(), in_set: x.in_set })
                .collect()
        })
        .unwrap_or_default()
}
