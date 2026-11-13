//! Session history from script ("Working sites first" line 7; HTML
//! §7.4): `history` (`length`, `state`, `scrollRestoration`, `go`, `back`,
//! `forward`, `pushState`, `replaceState`), `PopStateEvent` and
//! `HashChangeEvent`.
//!
//! The tab owns the session history. `pushState`/`replaceState` take
//! effect for the script at once (`location`, `document.URL`,
//! `history.state` and `history.length` read the new values) and reach the
//! tab as a `HostRequest`; traversals (`go`, `back`, `forward`) are
//! requests the tab carries out after the script, as the standard queues
//! them. The tab reports the entry a document is at (`set_history`) and
//! fires `popstate` and `hashchange` (`fire_popstate`, `fire_hashchange`).
//!
//! A state is kept serialized (`SerializedState`), since an entry outlives
//! the document that made it: a reload, or a traversal back to the entry
//! from another document, restores `history.state` from it. The
//! serialization is the structured clone algorithm's for storage, done by
//! functions made once from script source (they walk objects, maps and
//! sets through the language's own operations) with everything they use
//! captured before any page script runs.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::object::builtins::JsArray;
use boa_engine::property::{Attribute, PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, Source, js_string};
use boa_gc::{Finalize, Trace};
use url::Url;

use crate::dom::{dom, dom_exception, illegal_invocation};
use crate::events::EventTargetRef;
use crate::{HostRequest, getter, host_state, js_str, setter};

/// A value serialized for a session history entry (HTML's
/// StructuredSerializeForStorage). Opaque outside this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerializedState(Vec<u16>);

/// The most entries a tab's session history holds, as in Chrome and
/// Firefox; `history.length` never reads more.
pub const MAX_HISTORY_ENTRIES: usize = 50;

#[derive(Default)]
pub(crate) struct HistoryState {
    object: Option<JsObject>,
    /// The current entry's index in the session history and the number
    /// of entries.
    index: usize,
    length: usize,
    /// The current entry's state as serialized, and the value scripts
    /// read for it (made once per entry, so `history.state` is one
    /// object).
    serialized: Option<SerializedState>,
    value: Option<JsValue>,
    manual_scroll: bool,
    serialize: Option<JsObject>,
    deserialize: Option<JsObject>,
}

pub(crate) type SharedHistory = Rc<RefCell<HistoryState>>;

fn history(context: &mut Context) -> JsResult<SharedHistory> {
    context
        .get_data::<SharedHistory>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no history").into())
}

#[derive(Debug, Trace, Finalize, JsData)]
struct HistoryData;

#[derive(Debug, Trace, Finalize, JsData)]
struct HistoryClass;

impl Class for HistoryClass {
    const NAME: &'static str = "History";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let attr = Attribute::ENUMERABLE | Attribute::CONFIGURABLE;
        let get = getter(class.context(), "length", NativeFunction::from_fn_ptr(history_length));
        class.accessor(js_string!("length"), Some(get), None, attr);
        let get = getter(class.context(), "scrollRestoration", NativeFunction::from_fn_ptr(scroll_restoration_get));
        let set = setter(class.context(), "scrollRestoration", NativeFunction::from_fn_ptr(scroll_restoration_set));
        class.accessor(js_string!("scrollRestoration"), Some(get), Some(set), attr);
        let get = getter(class.context(), "state", NativeFunction::from_fn_ptr(history_state));
        class.accessor(js_string!("state"), Some(get), None, attr);
        class
            .method(js_string!("go"), 0, NativeFunction::from_copy_closure_with_captures(history_go, 0i32))
            .method(js_string!("back"), 0, NativeFunction::from_copy_closure_with_captures(history_go, -1i32))
            .method(js_string!("forward"), 0, NativeFunction::from_copy_closure_with_captures(history_go, 1i32))
            .method(js_string!("pushState"), 2, NativeFunction::from_copy_closure_with_captures(history_push, false))
            .method(js_string!("replaceState"), 2, NativeFunction::from_copy_closure_with_captures(history_push, true));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

fn this_history(this: &JsValue) -> JsResult<()> {
    match this.as_object() {
        Some(o) if o.is::<HistoryData>() => Ok(()),
        _ => Err(illegal_invocation()),
    }
}

fn history_length(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_history(this)?;
    Ok((history(context)?.borrow().length.max(1) as u32).into())
}

fn scroll_restoration_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_history(this)?;
    Ok(js_str(if history(context)?.borrow().manual_scroll { "manual" } else { "auto" }))
}

fn scroll_restoration_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_history(this)?;
    let manual = match args.get_or_undefined(0).to_string(context)?.to_std_string_escaped().as_str() {
        "manual" => true,
        "auto" => false,
        // Not one of the enumeration's values: ignored.
        _ => return Ok(JsValue::undefined()),
    };
    history(context)?.borrow_mut().manual_scroll = manual;
    host_state(context)?.borrow_mut().requests.push(HostRequest::ScrollRestoration { manual });
    Ok(JsValue::undefined())
}

/// The value of a serialized state; null when there is none or it does
/// not deserialize.
fn deserialize(state: Option<&SerializedState>, context: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state else { return Ok(JsValue::null()) };
    let function = history(context)?.borrow().deserialize.clone();
    let Some(function) = function else { return Ok(JsValue::null()) };
    let text = JsString::from(state.0.as_slice());
    Ok(function.call(&JsValue::undefined(), &[text.into()], context).unwrap_or(JsValue::null()))
}

fn history_state(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_history(this)?;
    let shared = history(context)?;
    if let Some(value) = shared.borrow().value.clone() {
        return Ok(value);
    }
    let serialized = shared.borrow().serialized.clone();
    let value = deserialize(serialized.as_ref(), context)?;
    shared.borrow_mut().value = Some(value.clone());
    Ok(value)
}

/// `go(delta)`, `back()` and `forward()`: a traversal the tab makes after
/// the script; `go()` and `go(0)` reload.
fn history_go(this: &JsValue, args: &[JsValue], fixed: &i32, context: &mut Context) -> JsResult<JsValue> {
    this_history(this)?;
    let delta = if *fixed != 0 { *fixed } else { args.get_or_undefined(0).to_i32(context)? };
    let request = if delta == 0 { HostRequest::Reload } else { HostRequest::Traverse { delta } };
    host_state(context)?.borrow_mut().requests.push(request);
    Ok(JsValue::undefined())
}

/// HTML's "can have its URL rewritten": the same origin parts; for a
/// scheme that is not HTTP(S), the same path and query too.
fn can_rewrite(document: &Url, target: &Url) -> bool {
    if target.scheme() != document.scheme()
        || target.username() != document.username()
        || target.password() != document.password()
        || target.host() != document.host()
        || target.port() != document.port()
    {
        return false;
    }
    if matches!(target.scheme(), "http" | "https") {
        return true;
    }
    target.path() == document.path() && target.query() == document.query()
}

fn history_push(this: &JsValue, args: &[JsValue], replace: &bool, context: &mut Context) -> JsResult<JsValue> {
    this_history(this)?;
    let method = if *replace { "replaceState" } else { "pushState" };
    if args.len() < 2 {
        return Err(JsNativeError::typ()
            .with_message(format!(
                "Failed to execute '{method}' on 'History': 2 arguments required, but only {} present.",
                args.len()
            ))
            .into());
    }
    let shared = history(context)?;
    // The state is serialized first: what cannot be cloned throws before
    // anything changes.
    let serialize = shared.borrow().serialize.clone().ok_or_else(illegal_invocation)?;
    let ordinary = FunctionObjectBuilder::new(context.realm(), NativeFunction::from_fn_ptr(is_ordinary)).build();
    let fail = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(clone_error, *replace),
    )
    .build();
    let text = serialize.call(&JsValue::undefined(), &[args[0].clone(), ordinary.into(), fail.into()], context)?;
    let Some(text) = text.as_string() else {
        return Err(clone_failure(*replace, "the value", context));
    };
    let serialized = SerializedState(text.to_vec());

    let host = host_state(context)?;
    let document_url = crate::current_url(&host.borrow().info);
    let mut new_url = document_url.clone();
    let url_arg = args.get_or_undefined(2);
    if !url_arg.is_null_or_undefined() {
        let text = url_arg.to_string(context)?.to_std_string_escaped();
        if !text.is_empty() {
            let refused = |context: &mut Context| {
                dom_exception(
                    "SecurityError",
                    &format!(
                        "Failed to execute '{method}' on 'History': A history state object with URL '{text}' cannot be created in a document with origin '{}' and URL '{document_url}'.",
                        document_url.origin().ascii_serialization()
                    ),
                    context,
                )
            };
            let Some(parsed) = crate::fetch::parse_api_url(&text, context)? else {
                return Err(refused(context));
            };
            if !can_rewrite(&document_url, &parsed) {
                return Err(refused(context));
            }
            new_url = parsed;
        }
    }
    // The document's URL, and what relative URLs resolve against when no
    // `<base>` says otherwise.
    {
        let shared_dom = dom(context)?;
        let mut dom = shared_dom.borrow_mut();
        // The fallback base URL; a `<base>`'s frozen base URL stays.
        dom.doc.set_document_url(Some(new_url.clone()));
    }
    host.borrow_mut().info.url = Some(new_url.clone());
    let value = deserialize(Some(&serialized), context)?;
    {
        let mut state = shared.borrow_mut();
        if !*replace {
            // The entries after the current one go; the new one follows it.
            state.index = (state.index + 1).min(MAX_HISTORY_ENTRIES - 1);
            state.length = (state.index + 1).min(MAX_HISTORY_ENTRIES);
        }
        state.serialized = Some(serialized.clone());
        state.value = Some(value);
    }
    host.borrow_mut().requests.push(HostRequest::PushState {
        url: new_url,
        state: serialized,
        replace: *replace,
    });
    Ok(JsValue::undefined())
}

/// Whether an object is an ordinary one for the serialization: no data of
/// its own beyond its properties (not a node, an event, a promise, a
/// proxy), and not callable.
fn is_ordinary(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(args.get_or_undefined(0).as_object().is_some_and(|o| o.is_ordinary() && !o.is_callable()).into())
}

fn clone_failure(replace: bool, what: &str, context: &mut Context) -> boa_engine::JsError {
    let method = if replace { "replaceState" } else { "pushState" };
    dom_exception(
        "DataCloneError",
        &format!("Failed to execute '{method}' on 'History': {what} could not be cloned."),
        context,
    )
}

fn clone_error(_: &JsValue, args: &[JsValue], replace: &bool, context: &mut Context) -> JsResult<JsValue> {
    let what = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    Err(clone_failure(*replace, &what, context))
}

// ----- PopStateEvent and HashChangeEvent -----

#[derive(Debug, Trace, Finalize, JsData)]
struct PopStateEventClass;

impl Class for PopStateEventClass {
    const NAME: &'static str = "PopStateEvent";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let attr = Attribute::ENUMERABLE | Attribute::CONFIGURABLE;
        let get = getter(class.context(), "state", NativeFunction::from_fn_ptr(popstate_state));
        class.accessor(js_string!("state"), Some(get), None, attr);
        let get = getter(class.context(), "hasUAVisualTransition", NativeFunction::from_fn_ptr(|_, _, _| Ok(false.into())));
        class.accessor(js_string!("hasUAVisualTransition"), Some(get), None, attr);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.is_empty() {
            return Err(JsNativeError::typ()
                .with_message("Failed to construct 'PopStateEvent': 1 argument required, called with new")
                .into());
        }
        let state = match args.get_or_undefined(1).as_object() {
            Some(init) => {
                let v = init.get(js_string!("state"), context)?;
                if v.is_undefined() { JsValue::null() } else { v }
            }
            None => JsValue::null(),
        };
        let proto = event_prototype::<Self>(context)?;
        crate::events::constructed_event_with_value(args, proto, state, context)
    }
}

fn popstate_state(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    crate::events::event_value(this).ok_or_else(illegal_invocation)
}

#[derive(Debug, Trace, Finalize, JsData)]
struct HashChangeEventClass;

impl Class for HashChangeEventClass {
    const NAME: &'static str = "HashChangeEvent";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (index, name) in ["oldURL", "newURL"].into_iter().enumerate() {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(hashchange_url, index as u32));
            class.accessor(JsString::from(name), Some(get), None, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
        }
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.is_empty() {
            return Err(JsNativeError::typ()
                .with_message("Failed to construct 'HashChangeEvent': 1 argument required, called with new")
                .into());
        }
        let mut urls = [js_str(""), js_str("")];
        if let Some(init) = args.get_or_undefined(1).as_object() {
            for (index, name) in ["oldURL", "newURL"].into_iter().enumerate() {
                let v = init.get(JsString::from(name), context)?;
                if !v.is_undefined() {
                    urls[index] = v.to_string(context)?.into();
                }
            }
        }
        let proto = event_prototype::<Self>(context)?;
        let value = JsArray::from_iter(urls, context);
        crate::events::constructed_event_with_value(args, proto, value.into(), context)
    }
}

fn hashchange_url(this: &JsValue, _: &[JsValue], index: &u32, context: &mut Context) -> JsResult<JsValue> {
    let value = crate::events::event_value(this).ok_or_else(illegal_invocation)?;
    let Some(array) = value.as_object() else { return Err(illegal_invocation()) };
    array.get(*index, context)
}

fn event_prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

// ----- the tab's side -----

/// The entry the document is at: its index, the number of entries, its
/// state and scroll restoration mode.
pub(crate) fn set_entry(index: usize, length: usize, state: Option<SerializedState>, manual_scroll: bool, context: &mut Context) {
    let Ok(shared) = history(context) else { return };
    let mut h = shared.borrow_mut();
    h.index = index;
    h.length = length;
    h.manual_scroll = manual_scroll;
    if h.serialized != state {
        h.serialized = state;
        h.value = None;
    }
}

/// A traversal reached an entry of this document: `history.state` becomes
/// the entry's state (a fresh value) and `popstate` fires at `window`
/// with it.
pub(crate) fn fire_popstate(state: Option<SerializedState>, context: &mut Context) -> JsResult<()> {
    let shared = history(context)?;
    let value = deserialize(state.as_ref(), context)?;
    {
        let mut h = shared.borrow_mut();
        h.serialized = state;
        h.value = Some(value.clone());
    }
    let proto = event_prototype::<PopStateEventClass>(context)?;
    let event = crate::events::trusted_event_with_value("popstate", proto, value, context)?;
    crate::events::dispatch(&event, EventTargetRef::Window, None, context)?;
    Ok(())
}

/// The document's URL changed in its fragment only: `hashchange` at
/// `window`.
pub(crate) fn fire_hashchange(old: &Url, new: &Url, context: &mut Context) -> JsResult<()> {
    let proto = event_prototype::<HashChangeEventClass>(context)?;
    let value = JsArray::from_iter([js_str(old.as_str()), js_str(new.as_str())], context);
    let event = crate::events::trusted_event_with_value("hashchange", proto, value.into(), context)?;
    crate::events::dispatch(&event, EventTargetRef::Window, None, context)?;
    Ok(())
}

/// The structured clone functions, made once: `serialize(value, ordinary,
/// fail)` gives the value as text, `deserialize(text)` the value again.
const CLONE_SOURCE: &str = r#"(function () {
    const apply = Reflect.apply, keysOf = Object.keys, isArray = Array.isArray, is = Object.is;
    const describe = Object.getOwnPropertyDescriptor, define = Object.defineProperty, protoOf = Object.getPrototypeOf;
    const getterOf = function (proto, name) { return describe(proto, name).get; };
    const M = Map, S = Set, D = Date, RE = RegExp, AB = ArrayBuffer, DV = DataView, U8 = Uint8Array;
    const Num = Number, Str = String, Bool = Boolean, Big = BigInt, Obj = Object;
    const mapGet = M.prototype.get, mapSet = M.prototype.set, mapHas = M.prototype.has, mapForEach = M.prototype.forEach;
    const setAdd = S.prototype.add, setHas = S.prototype.has, setForEach = S.prototype.forEach;
    const dateTime = D.prototype.getTime;
    const numValue = Num.prototype.valueOf, strValue = Str.prototype.valueOf, boolValue = Bool.prototype.valueOf;
    const bigValue = Big.prototype.valueOf, bigString = Big.prototype.toString;
    const reSource = getterOf(RE.prototype, 'source'), reFlags = getterOf(RE.prototype, 'flags');
    const abLength = getterOf(AB.prototype, 'byteLength');
    const TA = protoOf(U8);
    const taTag = getterOf(TA.prototype, Symbol.toStringTag), taBuffer = getterOf(TA.prototype, 'buffer');
    const taOffset = getterOf(TA.prototype, 'byteOffset'), taLength = getterOf(TA.prototype, 'length');
    const dvBuffer = getterOf(DV.prototype, 'buffer'), dvOffset = getterOf(DV.prototype, 'byteOffset');
    const dvLength = getterOf(DV.prototype, 'byteLength');
    const fromCharCode = Str.fromCharCode, charCodeAt = Str.prototype.charCodeAt;
    const stringify = JSON.stringify, parse = JSON.parse, objectToString = Obj.prototype.toString, tagSymbol = Symbol.toStringTag;
    const views = { Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array,
        Float32Array, Float64Array, BigInt64Array, BigUint64Array };
    if (typeof Float16Array === 'function') views.Float16Array = Float16Array;
    const errors = { Error, EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError };
    const hasShared = typeof SharedArrayBuffer === 'function';
    const sabLength = hasShared ? getterOf(SharedArrayBuffer.prototype, 'byteLength') : null;

    function brand(f, v) { try { return [apply(f, v, [])]; } catch (e) { return null; } }
    function num(n) { return is(n, -0) ? '-0' : '' + n; }
    function describeValue(v) { try { return apply(objectToString, v, []); } catch (e) { return 'The object'; } }

    function serialize(value, ordinary, fail) {
        const memory = new M();
        const nodes = [];
        function add(node) { nodes[nodes.length] = node; return nodes.length - 1; }
        function properties(v) {
            const keys = keysOf(v);
            const out = [];
            for (let i = 0; i < keys.length; i++) {
                const key = keys[i];
                if (describe(v, key) !== undefined) {
                    out[out.length] = key;
                    out[out.length] = walk(v[key]);
                }
            }
            return out;
        }
        function walk(v) {
            const t = typeof v;
            if (t === 'undefined') return add(['u']);
            if (v === null) return add(['0']);
            if (t === 'boolean') return add(['b', v]);
            if (t === 'number') return add(['n', num(v)]);
            if (t === 'bigint') return add(['i', apply(bigString, v, [])]);
            if (t === 'string') return add(['s', v]);
            if (t === 'symbol') fail('Symbol()');
            if (apply(mapHas, memory, [v])) return apply(mapGet, memory, [v]);
            if (t === 'function') fail(describeValue(v));
            const index = add(null);
            apply(mapSet, memory, [v, index]);
            let b;
            if ((b = brand(boolValue, v))) { nodes[index] = ['B', b[0]]; return index; }
            if ((b = brand(numValue, v))) { nodes[index] = ['N', num(b[0])]; return index; }
            if ((b = brand(bigValue, v))) { nodes[index] = ['I', apply(bigString, b[0], [])]; return index; }
            if ((b = brand(strValue, v))) { nodes[index] = ['S', b[0]]; return index; }
            if ((b = brand(dateTime, v))) { nodes[index] = ['d', num(b[0])]; return index; }
            if ((b = brand(reSource, v)) && v !== RE.prototype) {
                nodes[index] = ['r', b[0], apply(reFlags, v, [])];
                return index;
            }
            if (hasShared && brand(sabLength, v)) fail(describeValue(v));
            if ((b = brand(abLength, v))) {
                const bytes = new U8(v);
                let text = '';
                for (let i = 0; i < bytes.length; i++) text += fromCharCode(bytes[i]);
                nodes[index] = ['f', text];
                return index;
            }
            const tag = apply(taTag, v, []);
            if (tag !== undefined) {
                nodes[index] = ['v', tag, walk(apply(taBuffer, v, [])), apply(taOffset, v, []), apply(taLength, v, [])];
                return index;
            }
            if ((b = brand(dvLength, v))) {
                nodes[index] = ['v', 'DataView', walk(apply(dvBuffer, v, [])), apply(dvOffset, v, []), b[0]];
                return index;
            }
            if (brand(mapHas, v)) {
                const pairs = [];
                apply(mapForEach, v, [function (value, key) { pairs[pairs.length] = key; pairs[pairs.length] = value; }]);
                const out = [];
                for (let i = 0; i < pairs.length; i++) out[i] = walk(pairs[i]);
                nodes[index] = ['m', out];
                return index;
            }
            if (brand(setHas, v)) {
                const items = [];
                apply(setForEach, v, [function (value) { items[items.length] = value; }]);
                const out = [];
                for (let i = 0; i < items.length; i++) out[i] = walk(items[i]);
                nodes[index] = ['t', out];
                return index;
            }
            if (v[tagSymbol] === undefined && apply(objectToString, v, []) === '[object Error]') {
                let name = v.name;
                name = name === undefined ? 'Error' : '' + name;
                if (errors[name] === undefined) name = 'Error';
                const message = describe(v, 'message');
                const node = ['e', name];
                if (message !== undefined && 'value' in message) node[2] = '' + message.value;
                nodes[index] = node;
                return index;
            }
            if (isArray(v)) {
                nodes[index] = ['a', v.length, properties(v)];
                return index;
            }
            if (!ordinary(v)) fail(describeValue(v));
            nodes[index] = ['o', properties(v)];
            return index;
        }
        walk(value);
        return stringify(nodes);
    }

    function data(o, key, value) {
        define(o, key, { value: value, writable: true, enumerable: true, configurable: true });
    }

    function deserialize(text) {
        const nodes = parse(text);
        const out = [];
        // The buffers first (views are made over them), then the rest.
        for (let pass = 0; pass < 2; pass++) {
            for (let i = 0; i < nodes.length; i++) {
                const n = nodes[i];
                const isBuffer = n[0] === 'f';
                if ((pass === 0) !== isBuffer) continue;
                switch (n[0]) {
                    case 'u': out[i] = undefined; break;
                    case '0': out[i] = null; break;
                    case 'b': out[i] = n[1]; break;
                    case 'n': out[i] = n[1] === '-0' ? -0 : Num(n[1]); break;
                    case 'i': out[i] = Big(n[1]); break;
                    case 's': out[i] = n[1]; break;
                    case 'B': out[i] = new Bool(n[1]); break;
                    case 'N': out[i] = new Num(n[1] === '-0' ? -0 : Num(n[1])); break;
                    case 'I': out[i] = Obj(Big(n[1])); break;
                    case 'S': out[i] = new Str(n[1]); break;
                    case 'd': out[i] = new D(Num(n[1])); break;
                    case 'r': out[i] = new RE(n[1], n[2]); break;
                    case 'f': {
                        const bytes = new U8(n[1].length);
                        for (let j = 0; j < n[1].length; j++) bytes[j] = apply(charCodeAt, n[1], [j]);
                        out[i] = bytes.buffer;
                        break;
                    }
                    case 'v': break;
                    case 'm': out[i] = new M(); break;
                    case 't': out[i] = new S(); break;
                    case 'e': {
                        const E = errors[n[1]];
                        out[i] = n.length > 2 ? new E(n[2]) : new E();
                        break;
                    }
                    case 'a': { const a = []; a.length = n[1]; out[i] = a; break; }
                    case 'o': out[i] = {}; break;
                }
            }
        }
        for (let i = 0; i < nodes.length; i++) {
            const n = nodes[i];
            if (n[0] === 'v') {
                out[i] = n[1] === 'DataView' ? new DV(out[n[2]], n[3], n[4]) : new views[n[1]](out[n[2]], n[3], n[4]);
            }
        }
        for (let i = 0; i < nodes.length; i++) {
            const n = nodes[i];
            if (n[0] === 'm') {
                for (let j = 0; j < n[1].length; j += 2) apply(mapSet, out[i], [out[n[1][j]], out[n[1][j + 1]]]);
            } else if (n[0] === 't') {
                for (let j = 0; j < n[1].length; j++) apply(setAdd, out[i], [out[n[1][j]]]);
            } else if (n[0] === 'a') {
                for (let j = 0; j < n[2].length; j += 2) data(out[i], n[2][j], out[n[2][j + 1]]);
            } else if (n[0] === 'o') {
                for (let j = 0; j < n[1].length; j += 2) data(out[i], n[1][j], out[n[1][j + 1]]);
            }
        }
        return out[0];
    }
    return [serialize, deserialize];
})()"#;

/// Register `history`, `PopStateEvent` and `HashChangeEvent`.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let shared: SharedHistory = Rc::new(RefCell::new(HistoryState {
        length: 1,
        ..HistoryState::default()
    }));
    context.insert_data(shared.clone());
    context.register_global_class::<HistoryClass>()?;
    context.register_global_class::<PopStateEventClass>()?;
    context.register_global_class::<HashChangeEventClass>()?;
    crate::events::inherit::<PopStateEventClass, crate::events::EventData>(context);
    crate::events::inherit::<HashChangeEventClass, crate::events::EventData>(context);
    let made = context.eval(Source::from_bytes(CLONE_SOURCE.as_bytes()))?;
    let made = made.as_object().ok_or_else(|| JsNativeError::error().with_message("clone helpers"))?;
    let function = |index: u32, context: &mut Context| -> JsResult<JsObject> {
        made.get(index, context)?
            .as_object()
            .ok_or_else(|| JsNativeError::error().with_message("clone helpers").into())
    };
    let serialize = function(0, context)?;
    let deserialize = function(1, context)?;
    let proto = event_prototype::<HistoryClass>(context)?;
    let object = JsObject::from_proto_and_data(proto, HistoryData);
    {
        let mut h = shared.borrow_mut();
        h.serialize = Some(serialize);
        h.deserialize = Some(deserialize);
        h.object = Some(object.clone());
    }
    let get = getter(context, "history", NativeFunction::from_fn_ptr(window_history));
    context.global_object().define_property_or_throw(
        PropertyKey::from(js_string!("history")),
        PropertyDescriptor::builder().get(get).enumerable(true).configurable(true).build(),
        context,
    )?;
    Ok(())
}

fn window_history(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(history(context)?.borrow().object.clone().map_or(JsValue::undefined(), JsValue::from))
}
