//! `URL` and `URLSearchParams` (URL Standard §6). Parsing, the parts'
//! getters and setters and the `application/x-www-form-urlencoded`
//! format are the `url` crate's, which implements the URL Standard
//! (`url::quirks` is its API section). A `URL`'s `searchParams` is a live
//! view: changing either changes the other (the standard's "update
//! steps").

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::JsArray;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsArgs, JsData, JsError, JsNativeError, JsObject, JsResult, JsString, JsSymbol, JsValue, NativeFunction, Source, js_string};
use boa_gc::{Finalize, Trace};
use url::Url;

use crate::dom::illegal_invocation;
use crate::{getter, js_str, setter};

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

fn type_error(message: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

/// A WebIDL `USVString`: lone surrogates become U+FFFD.
fn usv(value: &JsValue, context: &mut Context) -> JsResult<String> {
    Ok(value.to_string(context)?.to_std_string_lossy())
}

fn need_args(args: &[JsValue], count: usize, what: &str) -> JsResult<()> {
    if args.len() < count {
        return Err(type_error(format!(
            "Failed to execute {what}: {count} argument{} required, but only {} present.",
            if count == 1 { "" } else { "s" },
            args.len()
        )));
    }
    Ok(())
}

/// The API's URL parser: `input` against `base` when given (URL Standard
/// "API URL parser"); `Err` names which one failed.
fn api_parse(input: &str, base: Option<&str>) -> Result<Url, &'static str> {
    let base = match base {
        Some(b) => Some(Url::parse(b).map_err(|_| "Invalid base URL")?),
        None => None,
    };
    Url::options().base_url(base.as_ref()).parse(input).map_err(|_| "Invalid URL")
}

/// The form-encoded pairs of a query, or of `URLSearchParams` text.
fn parse_pairs(text: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(text.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn serialize_pairs(list: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(list.iter().map(|(k, v)| (k.as_str(), v.as_str()))).finish()
}

// ----- URL -----

#[derive(Trace, Finalize, JsData)]
struct UrlData {
    #[unsafe_ignore_trace]
    url: Url,
    /// Its `searchParams` ([SameObject]).
    params: JsObject,
}

impl std::fmt::Debug for UrlData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UrlData").field("url", &self.url.as_str()).finish()
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct UrlClass;

/// The parts of a URL, one getter (and setter) serving them all.
#[derive(Debug, Clone, PartialEq, Eq, Trace, Finalize)]
enum Part {
    Href,
    Origin,
    Protocol,
    Username,
    Password,
    Host,
    Hostname,
    Port,
    Pathname,
    Search,
    Hash,
}

impl Class for UrlClass {
    const NAME: &'static str = "URL";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (name, part, settable) in [
            ("href", Part::Href, true),
            ("origin", Part::Origin, false),
            ("protocol", Part::Protocol, true),
            ("username", Part::Username, true),
            ("password", Part::Password, true),
            ("host", Part::Host, true),
            ("hostname", Part::Hostname, true),
            ("port", Part::Port, true),
            ("pathname", Part::Pathname, true),
            ("search", Part::Search, true),
            ("hash", Part::Hash, true),
        ] {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(url_get, part.clone()));
            let set = settable.then(|| setter(class.context(), name, NativeFunction::from_copy_closure_with_captures(url_set, part)));
            class.accessor(JsString::from(name), Some(get), set, ATTR);
        }
        let params = getter(class.context(), "searchParams", NativeFunction::from_fn_ptr(url_search_params));
        class.accessor(js_string!("searchParams"), Some(params), None, ATTR);
        class
            .method(js_string!("toString"), 0, NativeFunction::from_copy_closure_with_captures(url_get, Part::Href))
            .method(js_string!("toJSON"), 0, NativeFunction::from_copy_closure_with_captures(url_get, Part::Href))
            .static_method(js_string!("canParse"), 1, NativeFunction::from_fn_ptr(url_can_parse))
            .static_method(js_string!("parse"), 1, NativeFunction::from_fn_ptr(url_parse_static));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(type_error("Failed to construct 'URL': Please use the 'new' operator, this DOM object constructor cannot be called as a function."));
        }
        need_args(args, 1, "'URL' constructor").map_err(|_| type_error("Failed to construct 'URL': 1 argument required, but only 0 present."))?;
        let (input, base) = url_args(args, context)?;
        let url = api_parse(&input, base.as_deref()).map_err(|why| type_error(format!("Failed to construct 'URL': {why}")))?;
        let proto = match new_target.as_object() {
            Some(o) => o.get(js_string!("prototype"), context)?.as_object(),
            None => None,
        };
        let proto = match proto {
            Some(p) => p,
            None => class_prototype::<UrlClass>(context)?,
        };
        new_url_object(url, proto, context)
    }
}

fn class_prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

/// The URL and the optional base, as `USVString`s.
fn url_args(args: &[JsValue], context: &mut Context) -> JsResult<(String, Option<String>)> {
    let input = usv(args.get_or_undefined(0), context)?;
    let base = match args.get(1) {
        Some(b) if !b.is_undefined() => Some(usv(b, context)?),
        _ => None,
    };
    Ok((input, base))
}

/// A `URL` object with its `searchParams`, the two linked.
fn new_url_object(url: Url, proto: JsObject, context: &mut Context) -> JsResult<JsObject> {
    let params_proto = class_prototype::<ParamsClass>(context)?;
    let params = JsObject::from_proto_and_data(params_proto, ParamsData { list: parse_pairs(url.query().unwrap_or("")), url: None });
    let object = JsObject::from_proto_and_data(proto, UrlData { url, params: params.clone() });
    if let Some(mut p) = params.downcast_mut::<ParamsData>() {
        p.url = Some(object.clone());
    }
    Ok(object)
}

fn this_url(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<UrlData>()).ok_or_else(illegal_invocation)
}

fn url_get(this: &JsValue, _: &[JsValue], part: &Part, _: &mut Context) -> JsResult<JsValue> {
    let object = this_url(this)?;
    let data = object.downcast_ref::<UrlData>().ok_or_else(illegal_invocation)?;
    let u = &data.url;
    use url::quirks as q;
    let text = match part {
        Part::Href => q::href(u).to_owned(),
        Part::Origin => q::origin(u),
        Part::Protocol => q::protocol(u).to_owned(),
        Part::Username => q::username(u).to_owned(),
        Part::Password => q::password(u).to_owned(),
        Part::Host => q::host(u).to_owned(),
        Part::Hostname => q::hostname(u).to_owned(),
        Part::Port => q::port(u).to_owned(),
        Part::Pathname => q::pathname(u).to_owned(),
        Part::Search => q::search(u).to_owned(),
        Part::Hash => q::hash(u).to_owned(),
    };
    Ok(js_str(&text))
}

fn url_set(this: &JsValue, args: &[JsValue], part: &Part, context: &mut Context) -> JsResult<JsValue> {
    let object = this_url(this)?;
    let value = usv(args.get_or_undefined(0), context)?;
    let params = {
        let mut data = object.downcast_mut::<UrlData>().ok_or_else(illegal_invocation)?;
        let u = &mut data.url;
        use url::quirks as q;
        match part {
            // An invalid `href` throws; the others ignore what they cannot
            // use, as the standard's setters do.
            Part::Href => {
                let parsed = Url::parse(&value).map_err(|_| type_error(format!("Failed to set the 'href' property on 'URL': Invalid URL '{value}'")))?;
                *u = parsed;
            }
            Part::Protocol => {
                let _ = q::set_protocol(u, &value);
            }
            Part::Username => {
                let _ = q::set_username(u, &value);
            }
            Part::Password => {
                let _ = q::set_password(u, &value);
            }
            Part::Host => {
                let _ = q::set_host(u, &value);
            }
            Part::Hostname => {
                let _ = q::set_hostname(u, &value);
            }
            Part::Port => {
                let _ = q::set_port(u, &value);
            }
            Part::Pathname => q::set_pathname(u, &value),
            Part::Search => q::set_search(u, &value),
            Part::Hash => q::set_hash(u, &value),
            Part::Origin => {}
        }
        // A new query: the `searchParams` list follows it.
        matches!(part, Part::Href | Part::Search).then(|| (data.params.clone(), parse_pairs(data.url.query().unwrap_or(""))))
    };
    if let Some((params, list)) = params
        && let Some(mut p) = params.downcast_mut::<ParamsData>()
    {
        p.list = list;
    }
    Ok(JsValue::undefined())
}

fn url_search_params(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let object = this_url(this)?;
    let data = object.downcast_ref::<UrlData>().ok_or_else(illegal_invocation)?;
    Ok(data.params.clone().into())
}

fn url_can_parse(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    need_args(args, 1, "'canParse' on 'URL'")?;
    let (input, base) = url_args(args, context)?;
    Ok(api_parse(&input, base.as_deref()).is_ok().into())
}

fn url_parse_static(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    need_args(args, 1, "'parse' on 'URL'")?;
    let (input, base) = url_args(args, context)?;
    match api_parse(&input, base.as_deref()) {
        Ok(url) => {
            let proto = class_prototype::<UrlClass>(context)?;
            Ok(new_url_object(url, proto, context)?.into())
        }
        Err(_) => Ok(JsValue::null()),
    }
}

// ----- URLSearchParams -----

#[derive(Debug, Trace, Finalize, JsData)]
struct ParamsData {
    #[unsafe_ignore_trace]
    list: Vec<(String, String)>,
    /// The `URL` whose `searchParams` this is.
    url: Option<JsObject>,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct ParamsClass;

impl Class for ParamsClass {
    const NAME: &'static str = "URLSearchParams";
    const LENGTH: usize = 0;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let size = getter(class.context(), "size", NativeFunction::from_fn_ptr(params_size));
        class.accessor(js_string!("size"), Some(size), None, ATTR);
        class
            .method(js_string!("append"), 2, NativeFunction::from_fn_ptr(params_append))
            .method(js_string!("delete"), 1, NativeFunction::from_fn_ptr(params_delete))
            .method(js_string!("get"), 1, NativeFunction::from_fn_ptr(params_get))
            .method(js_string!("getAll"), 1, NativeFunction::from_fn_ptr(params_get_all))
            .method(js_string!("has"), 1, NativeFunction::from_fn_ptr(params_has))
            .method(js_string!("set"), 2, NativeFunction::from_fn_ptr(params_set))
            .method(js_string!("sort"), 0, NativeFunction::from_fn_ptr(params_sort))
            .method(js_string!("toString"), 0, NativeFunction::from_fn_ptr(params_to_string))
            .method(js_string!("forEach"), 1, NativeFunction::from_fn_ptr(params_for_each))
            .method(js_string!("entries"), 0, NativeFunction::from_copy_closure_with_captures(params_iterate, 2u8))
            .method(js_string!("keys"), 0, NativeFunction::from_copy_closure_with_captures(params_iterate, 0u8))
            .method(js_string!("values"), 0, NativeFunction::from_copy_closure_with_captures(params_iterate, 1u8))
            .method(JsSymbol::iterator(), 0, NativeFunction::from_copy_closure_with_captures(params_iterate, 2u8));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(type_error(
                "Failed to construct 'URLSearchParams': Please use the 'new' operator, this DOM object constructor cannot be called as a function.",
            ));
        }
        let list = params_init(args.get_or_undefined(0), context)?;
        let proto = match new_target.as_object() {
            Some(o) => o.get(js_string!("prototype"), context)?.as_object(),
            None => None,
        };
        let proto = match proto {
            Some(p) => p,
            None => class_prototype::<ParamsClass>(context)?,
        };
        Ok(JsObject::from_proto_and_data(proto, ParamsData { list, url: None }))
    }
}

/// The constructor's argument: pairs (a sequence of two-item sequences),
/// a record (an object's own enumerable string keys), or text (a leading
/// `?` dropped) in the form encoding.
fn params_init(init: &JsValue, context: &mut Context) -> JsResult<Vec<(String, String)>> {
    // No argument: empty. (`null` is no object: it becomes the text "null".)
    if init.is_undefined() {
        return Ok(Vec::new());
    }
    if init.is_object() {
        let helper = helpers(context)?;
        let result = helper.call(&JsValue::undefined(), std::slice::from_ref(init), context)?;
        let Some(array) = result.as_object().and_then(|o| JsArray::from_object(o).ok()) else {
            return Ok(Vec::new());
        };
        let mut list = Vec::new();
        for i in 0..array.length(context)? {
            let Some(pair) = array.get(i, context)?.as_object() else { continue };
            let name = usv(&pair.get(0, context)?, context)?;
            let value = usv(&pair.get(1, context)?, context)?;
            list.push((name, value));
        }
        return Ok(list);
    }
    let text = usv(init, context)?;
    Ok(parse_pairs(text.strip_prefix('?').unwrap_or(&text)))
}

fn this_params(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<ParamsData>()).ok_or_else(illegal_invocation)
}

/// Change the list, then the standard's "update" steps: the linked URL's
/// query becomes the list's serialization (none when it is empty).
fn modify(object: &JsObject, f: impl FnOnce(&mut Vec<(String, String)>)) -> JsResult<()> {
    let (url, serialized) = {
        let mut data = object.downcast_mut::<ParamsData>().ok_or_else(illegal_invocation)?;
        f(&mut data.list);
        (data.url.clone(), serialize_pairs(&data.list))
    };
    if let Some(url) = url
        && let Some(mut u) = url.downcast_mut::<UrlData>()
    {
        if serialized.is_empty() {
            u.url.set_query(None);
        } else {
            u.url.set_query(Some(&serialized));
        }
    }
    Ok(())
}

fn list_of(object: &JsObject) -> JsResult<Vec<(String, String)>> {
    Ok(object.downcast_ref::<ParamsData>().ok_or_else(illegal_invocation)?.list.clone())
}

fn params_size(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok((list_of(&this_params(this)?)?.len() as u32).into())
}

fn params_append(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    need_args(args, 2, "'append' on 'URLSearchParams'")?;
    let (name, value) = (usv(&args[0], context)?, usv(&args[1], context)?);
    modify(&object, |list| list.push((name, value)))?;
    Ok(JsValue::undefined())
}

/// `delete(name)` removes every pair of the name; `delete(name, value)`
/// only those with that value too.
fn params_delete(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    need_args(args, 1, "'delete' on 'URLSearchParams'")?;
    let name = usv(&args[0], context)?;
    let value = match args.get(1) {
        Some(v) if !v.is_undefined() => Some(usv(v, context)?),
        _ => None,
    };
    modify(&object, |list| list.retain(|(k, v)| !(k == &name && value.as_ref().is_none_or(|want| v == want))))?;
    Ok(JsValue::undefined())
}

fn params_get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    need_args(args, 1, "'get' on 'URLSearchParams'")?;
    let name = usv(&args[0], context)?;
    Ok(list_of(&object)?.into_iter().find(|(k, _)| k == &name).map_or(JsValue::null(), |(_, v)| js_str(&v)))
}

fn params_get_all(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    need_args(args, 1, "'getAll' on 'URLSearchParams'")?;
    let name = usv(&args[0], context)?;
    let values: Vec<JsValue> = list_of(&object)?.into_iter().filter(|(k, _)| k == &name).map(|(_, v)| js_str(&v)).collect();
    Ok(JsArray::from_iter(values, context).into())
}

fn params_has(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    need_args(args, 1, "'has' on 'URLSearchParams'")?;
    let name = usv(&args[0], context)?;
    let value = match args.get(1) {
        Some(v) if !v.is_undefined() => Some(usv(v, context)?),
        _ => None,
    };
    Ok(list_of(&object)?.iter().any(|(k, v)| k == &name && value.as_ref().is_none_or(|want| v == want)).into())
}

/// The first pair of the name takes the value, the others go; with none,
/// the pair is appended.
fn params_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    need_args(args, 2, "'set' on 'URLSearchParams'")?;
    let (name, value) = (usv(&args[0], context)?, usv(&args[1], context)?);
    modify(&object, |list| match list.iter().position(|(k, _)| k == &name) {
        Some(first) => {
            list[first].1 = value;
            let mut index = 0;
            list.retain(|(k, _)| {
                let keep = index <= first || k != &name;
                index += 1;
                keep
            });
        }
        None => list.push((name, value)),
    })?;
    Ok(JsValue::undefined())
}

/// Sort by name in UTF-16 code unit order, stable (pairs of one name keep
/// their order).
fn params_sort(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    modify(&object, |list| list.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16())))?;
    Ok(JsValue::undefined())
}

fn params_to_string(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(js_str(&serialize_pairs(&list_of(&this_params(this)?)?)))
}

fn params_for_each(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_params(this)?;
    let Some(callback) = args.get_or_undefined(0).as_object().filter(JsObject::is_callable) else {
        return Err(type_error("Failed to execute 'forEach' on 'URLSearchParams': parameter 1 is not of type 'Function'."));
    };
    let this_arg = args.get_or_undefined(1).clone();
    // The list is read at every step: the callback may change it.
    let mut index = 0;
    loop {
        let Some((name, value)) = list_of(&object)?.into_iter().nth(index) else { break };
        callback.call(&this_arg, &[js_str(&value), js_str(&name), object.clone().into()], context)?;
        index += 1;
    }
    Ok(JsValue::undefined())
}

/// A `URLSearchParams` iterator: live over the list.
#[derive(Debug, Trace, Finalize, JsData)]
struct ParamsIterator {
    params: JsObject,
    #[unsafe_ignore_trace]
    index: usize,
    /// 0 keys, 1 values, 2 entries.
    #[unsafe_ignore_trace]
    kind: u8,
}

/// The realm's helpers: the pairs conversion and the iterator prototype.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
struct UrlHelpers {
    pairs: JsObject,
    iterator: JsObject,
}

fn helpers(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_data::<UrlHelpers>()
        .map(|h| h.pairs.clone())
        .ok_or_else(|| JsNativeError::error().with_message("URL helpers").into())
}

fn params_iterate(this: &JsValue, _: &[JsValue], kind: &u8, context: &mut Context) -> JsResult<JsValue> {
    let params = this_params(this)?;
    let proto = context
        .get_data::<UrlHelpers>()
        .map(|h| h.iterator.clone())
        .ok_or_else(|| JsError::from(JsNativeError::error().with_message("URL helpers")))?;
    Ok(JsObject::from_proto_and_data(proto, ParamsIterator { params, index: 0, kind: *kind }).into())
}

fn params_iterator_next(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object().filter(|o| o.is::<ParamsIterator>()) else {
        return Err(illegal_invocation());
    };
    let (pair, kind) = {
        let mut it = object.downcast_mut::<ParamsIterator>().ok_or_else(illegal_invocation)?;
        let pair = list_of(&it.params)?.into_iter().nth(it.index);
        if pair.is_some() {
            it.index += 1;
        }
        (pair, it.kind)
    };
    let (value, done) = match pair {
        None => (JsValue::undefined(), true),
        Some((name, value)) => (
            match kind {
                0 => js_str(&name),
                1 => js_str(&value),
                _ => JsArray::from_iter([js_str(&name), js_str(&value)], context).into(),
            },
            false,
        ),
    };
    Ok(ObjectInitializer::new(context)
        .property(js_string!("value"), value, Attribute::all())
        .property(js_string!("done"), done, Attribute::all())
        .build()
        .into())
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<UrlClass>()?;
    context.register_global_class::<ParamsClass>()?;
    // The pairs conversion of `URLSearchParams`' argument (WebIDL
    // `sequence<sequence<USVString>>` or `record<USVString, USVString>`),
    // made from source once, with what it uses captured.
    let made = context.eval(Source::from_bytes(
        b"(function () {\
            const ownKeys = Reflect.ownKeys, describe = Object.getOwnPropertyDescriptor, iterator = Symbol.iterator, TE = TypeError;\
            const iteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][iterator]()));\
            function pairs(init) {\
                const out = [];\
                const method = init[iterator];\
                if (method !== undefined && method !== null) {\
                    if (typeof method !== 'function') { throw new TE(\"Failed to construct 'URLSearchParams': The object's @@iterator is not callable.\"); }\
                    for (const pair of init) {\
                        if (pair === null || (typeof pair !== 'object' && typeof pair !== 'function')) {\
                            throw new TE(\"Failed to construct 'URLSearchParams': The provided value cannot be converted to a sequence.\");\
                        }\
                        const items = [];\
                        for (const item of pair) { items[items.length] = item; }\
                        if (items.length !== 2) { throw new TE(\"Failed to construct 'URLSearchParams': Sequence initializer must only contain pair elements\"); }\
                        out[out.length] = items;\
                    }\
                } else {\
                    const keys = ownKeys(init);\
                    for (let i = 0; i < keys.length; i++) {\
                        if (typeof keys[i] === 'symbol') { continue; }\
                        const d = describe(init, keys[i]);\
                        if (d !== undefined && d.enumerable) { out[out.length] = [keys[i], init[keys[i]]]; }\
                    }\
                }\
                return out;\
            }\
            return [pairs, iteratorPrototype];\
        })()",
    ))?;
    let Some(made) = made.as_object() else {
        return Err(JsNativeError::error().with_message("URL helpers").into());
    };
    let pairs = made.get(0, context)?.as_object().ok_or_else(|| JsError::from(JsNativeError::error().with_message("URL helpers")))?;
    let iterator_prototype = made.get(1, context)?.as_object().ok_or_else(|| JsError::from(JsNativeError::error().with_message("URL helpers")))?;
    let iterator = ObjectInitializer::new(context)
        .function(NativeFunction::from_fn_ptr(params_iterator_next), js_string!("next"), 0)
        .property(JsSymbol::to_string_tag(), js_string!("URLSearchParams Iterator"), Attribute::CONFIGURABLE)
        .build();
    iterator.set_prototype(Some(iterator_prototype));
    context.insert_data(UrlHelpers { pairs, iterator });
    Ok(())
}
