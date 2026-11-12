//! Fetch ("Working sites first" line 7): `fetch()`, `Headers`, `Request`
//! and `Response`, and what they share with `XMLHttpRequest` (`xhr.rs`).
//!
//! The tab carries requests out; this module keeps the objects scripts
//! see. A `fetch()` or an `XMLHttpRequest.send()` queues a `NetOp` the tab
//! takes after the script (`ScriptHost::take_net_ops`) and runs through
//! its fetch algorithm (same-origin and CORS checks, preflights,
//! redirects, credentials, the HTTP cache). The tab answers with the
//! response's head, its body as it arrives, and its end or failure
//! (`ScriptHost::net_response`, `net_data`, `net_done`, `net_failed`),
//! each a task of its own.
//!
//! A `fetch()` promise settles when the head arrives, as in browsers; the
//! body goes on arriving into the response's body source, and a `text()`
//! or `json()` asked for before its end waits for it. `clone()` shares the
//! source (the bytes are the same; each object has its own "used" state).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::{JsArray, JsPromise, JsUint8Array};
use boa_engine::property::Attribute;
use boa_engine::{
    Context, JsArgs, JsData, JsError, JsNativeError, JsObject, JsResult, JsString, JsSymbol, JsValue, NativeFunction, Source,
    js_string,
};
use boa_gc::{Finalize, Trace};
use url::Url;

use crate::dom::{dom, illegal_invocation};
use crate::{getter, host_state, js_str};

// ----- what the tab and the bindings share -----

/// Fetch's request mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMode {
    SameOrigin,
    Cors,
    NoCors,
}

/// Fetch's credentials mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialsMode {
    Omit,
    SameOrigin,
    Include,
}

/// Fetch's cache mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestCache {
    Default,
    NoStore,
    Reload,
    NoCache,
    ForceCache,
    OnlyIfCached,
}

/// Fetch's redirect mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectMode {
    Follow,
    Error,
    Manual,
}

/// What a script may see of a response (Fetch's filtered responses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    Basic,
    Cors,
    Opaque,
    OpaqueRedirect,
}

/// A request a script made, for the tab to carry out.
#[derive(Debug, Clone)]
pub struct NetRequest {
    pub id: u64,
    pub url: Url,
    pub method: String,
    /// The author's headers, names as given.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub mode: RequestMode,
    pub credentials: CredentialsMode,
    pub cache: RequestCache,
    pub redirect: RedirectMode,
    /// Subresource integrity metadata (`integrity`), as given.
    pub integrity: String,
    /// An `XMLHttpRequest` with listeners on its upload object: a
    /// cross-origin request is preflighted even when it is simple.
    pub force_preflight: bool,
    /// `XMLHttpRequest.timeout`, from the moment it was sent.
    pub timeout: Option<Duration>,
    /// The request outlives the document that made it (`keepalive`, a
    /// beacon): it is carried to its end with nobody to tell.
    pub keepalive: bool,
}

/// What a script asked of the network since the tab last took them.
#[derive(Debug, Clone)]
pub enum NetOp {
    Start(NetRequest),
    /// The request is no longer wanted (`XMLHttpRequest.abort()`); nothing
    /// more is reported for it.
    Abort(u64),
    /// `XMLHttpRequest.timeout` was changed while the request is under
    /// way: the time allowed from the moment it was sent.
    SetTimeout(u64, Option<Duration>),
}

/// The head of a response as the script may see it: the tab has already
/// applied the response's filtering (`kind`), so `headers` holds only what
/// is exposed.
#[derive(Debug, Clone)]
pub struct NetResponseHead {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    /// The URL the response came from, after redirects.
    pub url: Url,
    pub redirected: bool,
    pub kind: ResponseKind,
}

/// Why a request produced no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetFailure {
    /// A network error: the request was refused by a check (CORS, the
    /// mode, a redirect) or failed on the wire. The text is for the
    /// console only; scripts never see it.
    Network(String),
    /// `XMLHttpRequest.timeout` ran out.
    Timeout,
}

/// A synchronous `XMLHttpRequest`: the tab runs the request to its end on
/// the spot while the script waits.
pub trait SyncNet {
    fn fetch(&self, request: NetRequest) -> Result<(NetResponseHead, Vec<u8>), NetFailure>;
}

/// Who waits for a request's answer.
enum Consumer {
    Fetch(FetchConsumer),
    Xhr(JsObject),
}

struct FetchConsumer {
    resolvers: Option<ResolvingFunctions>,
    /// The request's method: a `HEAD` response has no body.
    method: String,
    /// The request's URL, for an opaque redirect response.
    url: Url,
    /// The body source of the response made from the head, fed until the
    /// end.
    body: Option<SharedBody>,
}

/// The requests under way and the objects the bindings share.
#[derive(Default)]
pub(crate) struct NetState {
    next_id: u64,
    pub(crate) ops: Vec<NetOp>,
    consumers: HashMap<u64, Consumer>,
    pub(crate) sync: Option<Rc<dyn SyncNet>>,
    helpers: Option<Helpers>,
}

/// Functions made once from script source (they need the iteration
/// protocol or `JSON`), captured so a page that overwrites the globals
/// does not reach them.
#[derive(Clone)]
struct Helpers {
    /// `(init) => [[name, value], ...]` for a `HeadersInit`.
    pairs: JsObject,
    json_parse: JsObject,
    json_stringify: JsObject,
    /// The prototype of `Headers` iterators.
    headers_iterator: JsObject,
}

pub(crate) type SharedNet = Rc<RefCell<NetState>>;

pub(crate) fn net(context: &mut Context) -> JsResult<SharedNet> {
    context
        .get_data::<SharedNet>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no network").into())
}

impl NetState {
    pub(crate) fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub(crate) fn register_xhr(&mut self, id: u64, object: JsObject) {
        self.consumers.insert(id, Consumer::Xhr(object));
    }

    pub(crate) fn forget(&mut self, id: u64) {
        self.consumers.remove(&id);
    }
}

fn helpers(context: &mut Context) -> JsResult<Helpers> {
    net(context)?
        .borrow()
        .helpers
        .clone()
        .ok_or_else(|| JsNativeError::error().with_message("no network").into())
}

fn type_error(message: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

fn prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

/// The prototype a constructor call's object gets: `new.target.prototype`
/// when it is an object (a subclass), else the class's own.
fn new_target_prototype<C: Class>(new_target: &JsValue, context: &mut Context) -> JsResult<JsObject> {
    let from_target = match new_target.as_object() {
        Some(o) => o.get(js_string!("prototype"), context)?.as_object(),
        None => None,
    };
    match from_target {
        Some(p) => Ok(p),
        None => prototype::<C>(context),
    }
}

/// The URL relative URLs in the API resolve against: the document's base
/// URL.
pub(crate) fn api_base_url(context: &mut Context) -> JsResult<Option<Url>> {
    if let Some(base) = dom(context)?.borrow().doc.base_url.clone() {
        return Ok(Some(base));
    }
    Ok(host_state(context)?.borrow().info.url.clone())
}

pub(crate) fn parse_api_url(text: &str, context: &mut Context) -> JsResult<Option<Url>> {
    let base = api_base_url(context)?;
    Ok(match base {
        Some(base) => base.join(text).ok(),
        None => Url::parse(text).ok(),
    })
}

// ----- header syntax (Fetch §2.2, RFC 9110) -----

/// A WebIDL `ByteString`: every code point at most U+00FF.
fn is_byte_string(s: &str) -> bool {
    s.chars().all(|c| (c as u32) <= 0xFF)
}

pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        })
}

/// Fetch's "normalize" of a header value: leading and trailing HTTP
/// whitespace removed.
pub(crate) fn normalize_value(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

fn is_header_value(s: &str) -> bool {
    !s.chars().any(|c| matches!(c, '\0' | '\r' | '\n'))
}

/// Fetch's forbidden methods.
pub(crate) fn is_forbidden_method(method: &str) -> bool {
    ["CONNECT", "TRACE", "TRACK"].iter().any(|m| method.eq_ignore_ascii_case(m))
}

/// Fetch's "normalize" of a method: the six common ones in upper case.
pub(crate) fn normalize_method(method: &str) -> String {
    for m in ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"] {
        if method.eq_ignore_ascii_case(m) {
            return m.to_owned();
        }
    }
    method.to_owned()
}

/// Fetch's forbidden request-headers: the ones the browser controls.
pub(crate) fn is_forbidden_request_header(name: &str, value: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const NAMES: &[&str] = &[
        "accept-charset",
        "accept-encoding",
        "access-control-request-headers",
        "access-control-request-method",
        "connection",
        "content-length",
        "cookie",
        "cookie2",
        "date",
        "dnt",
        "expect",
        "host",
        "keep-alive",
        "origin",
        "referer",
        "set-cookie",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "via",
    ];
    if NAMES.contains(&name.as_str()) || name.starts_with("proxy-") || name.starts_with("sec-") {
        return true;
    }
    if matches!(name.as_str(), "x-http-method" | "x-http-method-override" | "x-method-override") {
        return value.split(',').any(|m| is_forbidden_method(m.trim_matches(|c| matches!(c, ' ' | '\t'))));
    }
    false
}

fn is_forbidden_response_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("set-cookie") || name.eq_ignore_ascii_case("set-cookie2")
}

/// Fetch's CORS-unsafe request-header bytes.
fn has_cors_unsafe_byte(value: &str) -> bool {
    value.chars().any(|c| {
        let c = c as u32;
        (c < 0x20 && c != 0x09) || matches!(c, 0x22 | 0x28 | 0x29 | 0x3A | 0x3C | 0x3E | 0x3F | 0x40 | 0x5B | 0x5C | 0x5D | 0x7B | 0x7D | 0x7F)
    })
}

/// The essence (`type/subtype`, lower case) of a MIME type string, when
/// it parses.
pub(crate) fn mime_essence(value: &str) -> Option<String> {
    let essence = value.split(';').next()?.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'));
    let (kind, sub) = essence.split_once('/')?;
    (is_token(kind) && is_token(sub)).then(|| essence.to_ascii_lowercase())
}

/// Fetch's CORS-safelisted request-header.
pub fn is_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    if value.len() > 128 {
        return false;
    }
    match name.to_ascii_lowercase().as_str() {
        "accept" => !has_cors_unsafe_byte(value),
        "accept-language" | "content-language" => value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')),
        "content-type" => {
            !has_cors_unsafe_byte(value)
                && mime_essence(value).is_some_and(|e| {
                    matches!(e.as_str(), "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain")
                })
        }
        "range" => {
            // A simple range: `bytes=<start>-[<end>]`.
            let Some(rest) = value.strip_prefix("bytes=") else { return false };
            let Some((start, end)) = rest.split_once('-') else { return false };
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            digits(start)
                && (end.is_empty() || digits(end))
                && (end.is_empty() || start.parse::<u64>().ok() <= end.parse::<u64>().ok())
        }
        _ => false,
    }
}

/// Fetch's no-CORS-safelisted request-header.
fn is_no_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "accept" | "accept-language" | "content-language" | "content-type")
        && is_cors_safelisted_request_header(name, value)
}

// ----- Headers -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guard {
    None,
    Request,
    RequestNoCors,
    Response,
    Immutable,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct HeadersData {
    #[unsafe_ignore_trace]
    list: Vec<(String, String)>,
    #[unsafe_ignore_trace]
    guard: Guard,
}

fn header_get(list: &[(String, String)], name: &str) -> Option<String> {
    let values: Vec<&str> = list.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// Fetch's "sort and combine": names in lower case, sorted, each with
/// its values joined; `set-cookie` values each on their own.
fn sort_and_combine(list: &[(String, String)]) -> Vec<(String, String)> {
    let mut names: Vec<String> = list.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
    names.sort();
    names.dedup();
    let mut out = Vec::new();
    for name in names {
        if name == "set-cookie" {
            out.extend(list.iter().filter(|(k, _)| k.eq_ignore_ascii_case(&name)).map(|(_, v)| (name.clone(), v.clone())));
        } else if let Some(value) = header_get(list, &name) {
            out.push((name, value));
        }
    }
    out
}

/// Validate a name and value and tell whether the guard lets the change
/// through (`Ok(false)`: silently ignored).
fn header_allowed(name: &str, value: &str, guard: Guard, what: &str) -> JsResult<bool> {
    if !is_byte_string(name) || !is_token(name) {
        return Err(type_error(format!("Failed to execute '{what}' on 'Headers': Invalid name")));
    }
    if !is_byte_string(value) || !is_header_value(value) {
        return Err(type_error(format!("Failed to execute '{what}' on 'Headers': Invalid value")));
    }
    Ok(match guard {
        Guard::Immutable => return Err(type_error(format!("Failed to execute '{what}' on 'Headers': Headers are immutable"))),
        Guard::Request | Guard::RequestNoCors => !is_forbidden_request_header(name, value),
        Guard::Response => !is_forbidden_response_header(name),
        Guard::None => true,
    })
}

/// Fetch's "append" on a `Headers` object.
fn headers_append_to(data: &mut HeadersData, name: &str, value: &str) -> JsResult<()> {
    let value = normalize_value(value);
    if !header_allowed(name, value, data.guard, "append")? {
        return Ok(());
    }
    if data.guard == Guard::RequestNoCors {
        let combined = match header_get(&data.list, name) {
            Some(existing) => format!("{existing}, {value}"),
            None => value.to_owned(),
        };
        if !is_no_cors_safelisted_request_header(name, &combined) {
            return Ok(());
        }
    }
    // A name already in the list keeps the casing of its first use.
    let name = data.list.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map_or(name, |(k, _)| k.as_str()).to_owned();
    data.list.push((name, value.to_owned()));
    Ok(())
}

/// Fill a header list from a `HeadersInit` value.
fn headers_fill(data: &mut HeadersData, init: &JsValue, context: &mut Context) -> JsResult<()> {
    if init.is_undefined() {
        return Ok(());
    }
    if !init.is_object() {
        return Err(type_error(
            "Failed to construct 'Headers': The provided value is not of type '(record<ByteString, ByteString> or sequence<sequence<ByteString>>)'.",
        ));
    }
    let pairs = helpers(context)?.pairs;
    let result = pairs.call(&JsValue::undefined(), std::slice::from_ref(init), context)?;
    let Some(array) = result.as_object().and_then(|o| JsArray::from_object(o).ok()) else {
        return Ok(());
    };
    let len = array.length(context)?;
    for i in 0..len {
        let Some(pair) = array.get(i, context)?.as_object() else { continue };
        let name = pair.get(0, context)?.to_string(context)?.to_std_string_escaped();
        let value = pair.get(1, context)?.to_string(context)?.to_std_string_escaped();
        headers_append_to(data, &name, &value)?;
    }
    Ok(())
}

fn new_headers(list: Vec<(String, String)>, guard: Guard, context: &mut Context) -> JsResult<JsObject> {
    let proto = prototype::<HeadersClass>(context)?;
    Ok(JsObject::from_proto_and_data(proto, HeadersData { list, guard }))
}

fn headers_list(object: &JsObject) -> Vec<(String, String)> {
    object.downcast_ref::<HeadersData>().map(|d| d.list.clone()).unwrap_or_default()
}

#[derive(Debug, Trace, Finalize, JsData)]
struct HeadersClass;

impl Class for HeadersClass {
    const NAME: &'static str = "Headers";
    const LENGTH: usize = 0;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        class
            .method(js_string!("append"), 2, NativeFunction::from_fn_ptr(headers_append))
            .method(js_string!("delete"), 1, NativeFunction::from_fn_ptr(headers_delete))
            .method(js_string!("get"), 1, NativeFunction::from_fn_ptr(headers_get))
            .method(js_string!("getSetCookie"), 0, NativeFunction::from_fn_ptr(headers_get_set_cookie))
            .method(js_string!("has"), 1, NativeFunction::from_fn_ptr(headers_has))
            .method(js_string!("set"), 2, NativeFunction::from_fn_ptr(headers_set))
            .method(js_string!("forEach"), 1, NativeFunction::from_fn_ptr(headers_for_each))
            .method(js_string!("entries"), 0, NativeFunction::from_copy_closure_with_captures(headers_iterate, 2u8))
            .method(js_string!("keys"), 0, NativeFunction::from_copy_closure_with_captures(headers_iterate, 0u8))
            .method(js_string!("values"), 0, NativeFunction::from_copy_closure_with_captures(headers_iterate, 1u8))
            .method(JsSymbol::iterator(), 0, NativeFunction::from_copy_closure_with_captures(headers_iterate, 2u8));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(type_error("Failed to construct 'Headers': Please use the 'new' operator"));
        }
        let mut data = HeadersData {
            list: Vec::new(),
            guard: Guard::None,
        };
        headers_fill(&mut data, args.get_or_undefined(0), context)?;
        let proto = new_target_prototype::<Self>(new_target, context)?;
        Ok(JsObject::from_proto_and_data(proto, data))
    }
}

fn this_headers(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<HeadersData>()).ok_or_else(illegal_invocation)
}

fn string_arg(args: &[JsValue], index: usize, context: &mut Context) -> JsResult<String> {
    Ok(args.get_or_undefined(index).to_string(context)?.to_std_string_escaped())
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

fn headers_append(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_headers(this)?;
    need_args(args, 2, "'append' on 'Headers'")?;
    let (name, value) = (string_arg(args, 0, context)?, string_arg(args, 1, context)?);
    let mut data = object.downcast_mut::<HeadersData>().ok_or_else(illegal_invocation)?;
    headers_append_to(&mut data, &name, &value)?;
    Ok(JsValue::undefined())
}

/// Fetch's "remove privileged no-CORS request-headers".
fn remove_privileged_no_cors(data: &mut HeadersData) {
    if data.guard == Guard::RequestNoCors {
        data.list.retain(|(k, _)| !k.eq_ignore_ascii_case("range"));
    }
}

fn headers_delete(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_headers(this)?;
    need_args(args, 1, "'delete' on 'Headers'")?;
    let name = string_arg(args, 0, context)?;
    let mut data = object.downcast_mut::<HeadersData>().ok_or_else(illegal_invocation)?;
    if !header_allowed(&name, "", data.guard, "delete")? {
        return Ok(JsValue::undefined());
    }
    if data.guard == Guard::RequestNoCors
        && !matches!(name.to_ascii_lowercase().as_str(), "accept" | "accept-language" | "content-language" | "content-type" | "range")
    {
        return Ok(JsValue::undefined());
    }
    if !data.list.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
        return Ok(JsValue::undefined());
    }
    data.list.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
    remove_privileged_no_cors(&mut data);
    Ok(JsValue::undefined())
}

fn headers_get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_headers(this)?;
    need_args(args, 1, "'get' on 'Headers'")?;
    let name = string_arg(args, 0, context)?;
    if !is_byte_string(&name) || !is_token(&name) {
        return Err(type_error("Failed to execute 'get' on 'Headers': Invalid name"));
    }
    let data = object.downcast_ref::<HeadersData>().ok_or_else(illegal_invocation)?;
    Ok(header_get(&data.list, &name).map_or(JsValue::null(), |v| js_str(&v)))
}

fn headers_get_set_cookie(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_headers(this)?;
    let values: Vec<JsValue> = {
        let data = object.downcast_ref::<HeadersData>().ok_or_else(illegal_invocation)?;
        data.list.iter().filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie")).map(|(_, v)| js_str(v)).collect()
    };
    Ok(JsArray::from_iter(values, context).into())
}

fn headers_has(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_headers(this)?;
    need_args(args, 1, "'has' on 'Headers'")?;
    let name = string_arg(args, 0, context)?;
    if !is_byte_string(&name) || !is_token(&name) {
        return Err(type_error("Failed to execute 'has' on 'Headers': Invalid name"));
    }
    let data = object.downcast_ref::<HeadersData>().ok_or_else(illegal_invocation)?;
    Ok(data.list.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)).into())
}

fn headers_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_headers(this)?;
    need_args(args, 2, "'set' on 'Headers'")?;
    let (name, value) = (string_arg(args, 0, context)?, string_arg(args, 1, context)?);
    let value = normalize_value(&value).to_owned();
    let mut data = object.downcast_mut::<HeadersData>().ok_or_else(illegal_invocation)?;
    if !header_allowed(&name, &value, data.guard, "set")? {
        return Ok(JsValue::undefined());
    }
    if data.guard == Guard::RequestNoCors && !is_no_cors_safelisted_request_header(&name, &value) {
        return Ok(JsValue::undefined());
    }
    // The first header of the name takes the value (keeping its casing);
    // the others go.
    match data.list.iter().position(|(k, _)| k.eq_ignore_ascii_case(&name)) {
        Some(first) => {
            data.list[first].1 = value;
            let mut index = 0;
            data.list.retain(|(k, _)| {
                let keep = index <= first || !k.eq_ignore_ascii_case(&name);
                index += 1;
                keep
            });
        }
        None => data.list.push((name, value)),
    }
    remove_privileged_no_cors(&mut data);
    Ok(JsValue::undefined())
}

fn headers_for_each(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_headers(this)?;
    let Some(callback) = args.get_or_undefined(0).as_object().filter(JsObject::is_callable) else {
        return Err(type_error("Failed to execute 'forEach' on 'Headers': parameter 1 is not of type 'Function'."));
    };
    let this_arg = args.get_or_undefined(1).clone();
    // The pairs are computed again for every step: a callback may change
    // the headers.
    let mut index = 0;
    loop {
        let pair = {
            let data = object.downcast_ref::<HeadersData>().ok_or_else(illegal_invocation)?;
            sort_and_combine(&data.list).into_iter().nth(index)
        };
        let Some((name, value)) = pair else { break };
        callback.call(&this_arg, &[js_str(&value), js_str(&name), object.clone().into()], context)?;
        index += 1;
    }
    Ok(JsValue::undefined())
}

/// A `Headers` iterator: live over the headers, as the pairs are computed
/// at every `next()`.
#[derive(Debug, Trace, Finalize, JsData)]
struct HeadersIterator {
    headers: JsObject,
    #[unsafe_ignore_trace]
    index: usize,
    /// 0 keys, 1 values, 2 entries.
    #[unsafe_ignore_trace]
    kind: u8,
}

fn headers_iterate(this: &JsValue, _: &[JsValue], kind: &u8, context: &mut Context) -> JsResult<JsValue> {
    let headers = this_headers(this)?;
    let proto = helpers(context)?.headers_iterator;
    Ok(JsObject::from_proto_and_data(
        proto,
        HeadersIterator {
            headers,
            index: 0,
            kind: *kind,
        },
    )
    .into())
}

fn headers_iterator_next(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object().filter(|o| o.is::<HeadersIterator>()) else {
        return Err(illegal_invocation());
    };
    let (pair, kind) = {
        let mut it = object.downcast_mut::<HeadersIterator>().ok_or_else(illegal_invocation)?;
        let pair = sort_and_combine(&headers_list(&it.headers)).into_iter().nth(it.index);
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

// ----- bodies -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyState {
    Loading,
    Done,
    Errored,
}

/// How a body is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consume {
    ArrayBuffer,
    Bytes,
    Text,
    Json,
}

/// The bytes of a body: complete for a body a script made, arriving for
/// a response from the network.
pub(crate) struct BodySource {
    bytes: Vec<u8>,
    state: BodyState,
    waiters: Vec<(Consume, ResolvingFunctions)>,
}

pub(crate) type SharedBody = Rc<RefCell<BodySource>>;

fn done_body(bytes: Vec<u8>) -> SharedBody {
    Rc::new(RefCell::new(BodySource {
        bytes,
        state: BodyState::Done,
        waiters: Vec::new(),
    }))
}

/// A `BodyInit` value's bytes and the content type it implies.
pub(crate) fn extract_body(value: &JsValue, context: &mut Context) -> JsResult<(Vec<u8>, Option<&'static str>)> {
    if let Some(object) = value.as_object()
        && let Some(bytes) = crate::fonts::buffer_bytes(&object, context)?
    {
        return Ok((bytes, None));
    }
    let text = value.to_string(context)?;
    Ok((utf8_encode(&text), Some("text/plain;charset=UTF-8")))
}

/// UTF-8 of a JavaScript string, lone surrogates as U+FFFD.
pub(crate) fn utf8_encode(text: &JsString) -> Vec<u8> {
    text.to_std_string_lossy().into_bytes()
}

/// The standard's "UTF-8 decode": a byte order mark is dropped, errors
/// become U+FFFD.
pub(crate) fn utf8_decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// An `ArrayBuffer` holding a copy of the bytes.
pub(crate) fn array_buffer(bytes: &[u8], context: &mut Context) -> JsResult<JsValue> {
    JsUint8Array::from_iter(bytes.iter().copied(), context)?.buffer(context)
}

fn convert_body(bytes: &[u8], how: Consume, context: &mut Context) -> JsResult<JsValue> {
    match how {
        Consume::ArrayBuffer => array_buffer(bytes, context),
        Consume::Bytes => {
            Ok(JsUint8Array::from_iter(bytes.iter().copied(), context)?.into())
        }
        Consume::Text => Ok(js_str(&utf8_decode(bytes))),
        Consume::Json => {
            let parse = helpers(context)?.json_parse;
            parse.call(&JsValue::undefined(), &[js_str(&utf8_decode(bytes))], context)
        }
    }
}

fn settle(resolvers: &ResolvingFunctions, result: JsResult<JsValue>, context: &mut Context) -> JsResult<()> {
    match result {
        Ok(value) => resolvers.resolve.call(&JsValue::undefined(), &[value], context)?,
        Err(err) => {
            let reason = err.into_opaque(context)?;
            resolvers.reject.call(&JsValue::undefined(), &[reason], context)?
        }
    };
    Ok(())
}

/// The body methods' work (Fetch's "consume body"): a promise for the
/// whole body read as `how`.
fn consume(body: Option<SharedBody>, already_used: bool, how: Consume, context: &mut Context) -> JsResult<JsValue> {
    let (promise, resolvers) = JsPromise::new_pending(context);
    if already_used {
        settle(&resolvers, Err(type_error("body stream already read")), context)?;
        return Ok(promise.into());
    }
    let Some(body) = body else {
        let result = convert_body(&[], how, context);
        settle(&resolvers, result, context)?;
        return Ok(promise.into());
    };
    let state = body.borrow().state;
    match state {
        BodyState::Loading => body.borrow_mut().waiters.push((how, resolvers)),
        BodyState::Done => {
            let bytes = body.borrow().bytes.clone();
            let result = convert_body(&bytes, how, context);
            settle(&resolvers, result, context)?;
        }
        BodyState::Errored => settle(&resolvers, Err(type_error("Failed to fetch")), context)?,
    }
    Ok(promise.into())
}

/// The end of a body that was arriving: settle whoever waited for it.
fn finish_body(body: &SharedBody, ok: bool, context: &mut Context) -> JsResult<()> {
    let waiters = {
        let mut source = body.borrow_mut();
        source.state = if ok { BodyState::Done } else { BodyState::Errored };
        std::mem::take(&mut source.waiters)
    };
    if waiters.is_empty() {
        return Ok(());
    }
    let bytes = body.borrow().bytes.clone();
    for (how, resolvers) in waiters {
        let result = if ok { convert_body(&bytes, how, context) } else { Err(type_error("Failed to fetch")) };
        settle(&resolvers, result, context)?;
    }
    Ok(())
}

/// A member of an init dictionary; `None` when absent or undefined.
fn member(init: Option<&JsObject>, name: &str, context: &mut Context) -> JsResult<Option<JsValue>> {
    let Some(init) = init else { return Ok(None) };
    let value = init.get(JsString::from(name), context)?;
    Ok((!value.is_undefined()).then_some(value))
}

/// An enumeration member of an init dictionary: its index among `values`.
fn enum_member(init: Option<&JsObject>, name: &str, values: &[&str], what: &str, context: &mut Context) -> JsResult<Option<usize>> {
    let Some(value) = member(init, name, context)? else { return Ok(None) };
    let text = value.to_string(context)?.to_std_string_escaped();
    match values.iter().position(|v| *v == text) {
        Some(i) => Ok(Some(i)),
        None => Err(type_error(format!(
            "Failed to construct '{what}': The provided value '{text}' is not a valid enum value of the member '{name}'."
        ))),
    }
}

// ----- Request -----

#[derive(Trace, Finalize, JsData)]
struct RequestData {
    #[unsafe_ignore_trace]
    url: Url,
    #[unsafe_ignore_trace]
    method: String,
    headers: JsObject,
    #[unsafe_ignore_trace]
    body: Option<SharedBody>,
    #[unsafe_ignore_trace]
    used: bool,
    #[unsafe_ignore_trace]
    mode: RequestMode,
    #[unsafe_ignore_trace]
    credentials: CredentialsMode,
    #[unsafe_ignore_trace]
    cache: RequestCache,
    #[unsafe_ignore_trace]
    redirect: RedirectMode,
    #[unsafe_ignore_trace]
    integrity: String,
    #[unsafe_ignore_trace]
    keepalive: bool,
    /// `about:client`, empty for no referrer, or a URL.
    #[unsafe_ignore_trace]
    referrer: String,
    #[unsafe_ignore_trace]
    referrer_policy: String,
}

impl std::fmt::Debug for RequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request").field("url", &self.url.as_str()).field("method", &self.method).finish()
    }
}

const MODES: &[&str] = &["same-origin", "cors", "no-cors", "navigate"];
const CREDENTIALS: &[&str] = &["omit", "same-origin", "include"];
const CACHES: &[&str] = &["default", "no-store", "reload", "no-cache", "force-cache", "only-if-cached"];
const REDIRECTS: &[&str] = &["follow", "error", "manual"];
const REFERRER_POLICIES: &[&str] = &[
    "",
    "no-referrer",
    "no-referrer-when-downgrade",
    "same-origin",
    "origin",
    "strict-origin",
    "origin-when-cross-origin",
    "strict-origin-when-cross-origin",
    "unsafe-url",
];

fn mode_keyword(mode: RequestMode) -> &'static str {
    match mode {
        RequestMode::SameOrigin => "same-origin",
        RequestMode::Cors => "cors",
        RequestMode::NoCors => "no-cors",
    }
}

fn credentials_keyword(mode: CredentialsMode) -> &'static str {
    match mode {
        CredentialsMode::Omit => "omit",
        CredentialsMode::SameOrigin => "same-origin",
        CredentialsMode::Include => "include",
    }
}

fn cache_keyword(mode: RequestCache) -> &'static str {
    match mode {
        RequestCache::Default => "default",
        RequestCache::NoStore => "no-store",
        RequestCache::Reload => "reload",
        RequestCache::NoCache => "no-cache",
        RequestCache::ForceCache => "force-cache",
        RequestCache::OnlyIfCached => "only-if-cached",
    }
}

fn redirect_keyword(mode: RedirectMode) -> &'static str {
    match mode {
        RedirectMode::Follow => "follow",
        RedirectMode::Error => "error",
        RedirectMode::Manual => "manual",
    }
}

/// The `Request` constructor's steps (Fetch §5.4): the request `input`
/// and `init` describe.
fn make_request(input: &JsValue, init: &JsValue, context: &mut Context) -> JsResult<RequestData> {
    const WHAT: &str = "Request";
    let init = match init {
        v if v.is_null_or_undefined() => None,
        v => Some(v.as_object().ok_or_else(|| type_error("Failed to construct 'Request': The provided value is not of type 'RequestInit'."))?),
    };
    let init = init.as_ref();
    let input_request = input.as_object().filter(|o| o.is::<RequestData>());
    // The request to start from: the input's, or a new one for its URL.
    let (mut request, input_headers, input_body) = match &input_request {
        Some(object) => {
            let data = object.downcast_ref::<RequestData>().ok_or_else(illegal_invocation)?;
            let request = RequestData {
                url: data.url.clone(),
                method: data.method.clone(),
                headers: data.headers.clone(),
                body: None,
                used: false,
                mode: data.mode,
                credentials: data.credentials,
                cache: data.cache,
                redirect: data.redirect,
                integrity: data.integrity.clone(),
                keepalive: data.keepalive,
                referrer: data.referrer.clone(),
                referrer_policy: data.referrer_policy.clone(),
            };
            (request, headers_list(&data.headers), data.body.clone().map(|b| (b, data.used)))
        }
        None => {
            let text = input.to_string(context)?.to_std_string_escaped();
            let Some(url) = parse_api_url(&text, context)? else {
                return Err(type_error(format!("Failed to construct 'Request': Failed to parse URL from {text}")));
            };
            if !url.username().is_empty() || url.password().is_some() {
                return Err(type_error(format!(
                    "Failed to construct 'Request': Request cannot be constructed from a URL that includes credentials: {text}"
                )));
            }
            // The headers object is made below, once the guard is known.
            let placeholder = new_headers(Vec::new(), Guard::Request, context)?;
            let request = RequestData {
                url,
                method: "GET".to_owned(),
                headers: placeholder,
                body: None,
                used: false,
                mode: RequestMode::Cors,
                credentials: CredentialsMode::SameOrigin,
                cache: RequestCache::Default,
                redirect: RedirectMode::Follow,
                integrity: String::new(),
                keepalive: false,
                referrer: "about:client".to_owned(),
                referrer_policy: String::new(),
            };
            (request, Vec::new(), None)
        }
    };
    if let Some(window) = member(init, "window", context)?
        && !window.is_null()
    {
        return Err(type_error("Failed to construct 'Request': 'window' can only be null."));
    }
    if let Some(referrer) = member(init, "referrer", context)? {
        let text = referrer.to_string(context)?.to_std_string_escaped();
        if text.is_empty() {
            request.referrer = String::new();
        } else {
            let Some(url) = parse_api_url(&text, context)? else {
                return Err(type_error(format!("Failed to construct 'Request': Referrer '{text}' is not a valid URL.")));
            };
            let document = host_state(context)?.borrow().info.url.clone();
            let same_origin = document.as_ref().is_some_and(|d| d.origin() == url.origin());
            // A cross-origin referrer (and `about:client`) means the
            // client's.
            request.referrer = if (url.scheme() == "about" && url.path() == "client") || !same_origin {
                "about:client".to_owned()
            } else {
                url.to_string()
            };
        }
    }
    if let Some(i) = enum_member(init, "referrerPolicy", REFERRER_POLICIES, WHAT, context)? {
        request.referrer_policy = REFERRER_POLICIES[i].to_owned();
    }
    match enum_member(init, "mode", MODES, WHAT, context)? {
        Some(0) => request.mode = RequestMode::SameOrigin,
        Some(1) => request.mode = RequestMode::Cors,
        Some(2) => request.mode = RequestMode::NoCors,
        Some(_) => return Err(type_error("Failed to construct 'Request': Cannot construct a Request with a RequestInit whose mode member is set as 'navigate'.")),
        None => {}
    }
    match enum_member(init, "credentials", CREDENTIALS, WHAT, context)? {
        Some(0) => request.credentials = CredentialsMode::Omit,
        Some(1) => request.credentials = CredentialsMode::SameOrigin,
        Some(_) => request.credentials = CredentialsMode::Include,
        None => {}
    }
    if let Some(i) = enum_member(init, "cache", CACHES, WHAT, context)? {
        request.cache = [
            RequestCache::Default,
            RequestCache::NoStore,
            RequestCache::Reload,
            RequestCache::NoCache,
            RequestCache::ForceCache,
            RequestCache::OnlyIfCached,
        ][i];
    }
    if request.cache == RequestCache::OnlyIfCached && request.mode != RequestMode::SameOrigin {
        return Err(type_error(
            "Failed to construct 'Request': 'only-if-cached' can be set only with 'same-origin' mode",
        ));
    }
    if let Some(i) = enum_member(init, "redirect", REDIRECTS, WHAT, context)? {
        request.redirect = [RedirectMode::Follow, RedirectMode::Error, RedirectMode::Manual][i];
    }
    if let Some(integrity) = member(init, "integrity", context)? {
        request.integrity = integrity.to_string(context)?.to_std_string_escaped();
    }
    if let Some(keepalive) = member(init, "keepalive", context)? {
        request.keepalive = keepalive.to_boolean();
    }
    if let Some(method) = member(init, "method", context)? {
        let method = method.to_string(context)?.to_std_string_escaped();
        if !is_token(&method) {
            return Err(type_error(format!("Failed to construct 'Request': '{method}' is not a valid HTTP method.")));
        }
        if is_forbidden_method(&method) {
            return Err(type_error(format!("Failed to construct 'Request': '{method}' HTTP method is unsupported.")));
        }
        request.method = normalize_method(&method);
    }
    // The headers: the init's when given, else the input's, appended
    // under the request's guard.
    let guard = if request.mode == RequestMode::NoCors {
        if !matches!(request.method.as_str(), "GET" | "HEAD" | "POST") {
            return Err(type_error(format!(
                "Failed to construct 'Request': '{}' is unsupported in no-cors mode.",
                request.method
            )));
        }
        Guard::RequestNoCors
    } else {
        Guard::Request
    };
    let mut headers = HeadersData {
        list: Vec::new(),
        guard,
    };
    match member(init, "headers", context)? {
        Some(value) => headers_fill(&mut headers, &value, context)?,
        None => {
            for (name, value) in &input_headers {
                headers_append_to(&mut headers, name, value)?;
            }
        }
    }
    // The body: the init's, else the input's (which the new request
    // takes over).
    let init_body = member(init, "body", context)?.filter(|v| !v.is_null());
    let has_input_body = input_body.is_some();
    if (init_body.is_some() || has_input_body) && matches!(request.method.as_str(), "GET" | "HEAD") {
        return Err(type_error("Failed to construct 'Request': Request with GET/HEAD method cannot have body."));
    }
    match init_body {
        Some(value) => {
            let (bytes, content_type) = extract_body(&value, context)?;
            if let Some(content_type) = content_type
                && !headers.list.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            {
                headers_append_to(&mut headers, "Content-Type", content_type)?;
            }
            request.body = Some(done_body(bytes));
        }
        None => {
            if let Some((body, used)) = input_body {
                if used {
                    return Err(type_error("Failed to construct 'Request': Cannot construct a Request with a Request object that has already been used."));
                }
                request.body = Some(body);
                if let Some(object) = &input_request
                    && let Some(mut data) = object.downcast_mut::<RequestData>()
                {
                    data.used = true;
                }
            }
        }
    }
    let proto = prototype::<HeadersClass>(context)?;
    request.headers = JsObject::from_proto_and_data(proto, headers);
    Ok(request)
}

impl Class for RequestData {
    const NAME: &'static str = "Request";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (name, prop) in [
            ("method", RequestProp::Method),
            ("url", RequestProp::Url),
            ("headers", RequestProp::Headers),
            ("destination", RequestProp::Destination),
            ("referrer", RequestProp::Referrer),
            ("referrerPolicy", RequestProp::ReferrerPolicy),
            ("mode", RequestProp::Mode),
            ("credentials", RequestProp::Credentials),
            ("cache", RequestProp::Cache),
            ("redirect", RequestProp::Redirect),
            ("integrity", RequestProp::Integrity),
            ("keepalive", RequestProp::Keepalive),
            ("isReloadNavigation", RequestProp::False),
            ("isHistoryNavigation", RequestProp::False),
            ("duplex", RequestProp::Duplex),
            ("bodyUsed", RequestProp::BodyUsed),
        ] {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(request_get, prop));
            class.accessor(JsString::from(name), Some(get), None, ATTR);
        }
        class.method(js_string!("clone"), 0, NativeFunction::from_fn_ptr(request_clone));
        add_body_methods(class);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(type_error("Failed to construct 'Request': Please use the 'new' operator"));
        }
        if args.is_empty() {
            return Err(type_error("Failed to construct 'Request': 1 argument required, but only 0 present."));
        }
        let data = make_request(&args[0], args.get_or_undefined(1), context)?;
        let proto = new_target_prototype::<Self>(new_target, context)?;
        Ok(JsObject::from_proto_and_data(proto, data))
    }
}

#[derive(Clone, Trace, Finalize)]
enum RequestProp {
    Method,
    Url,
    Headers,
    Destination,
    Referrer,
    ReferrerPolicy,
    Mode,
    Credentials,
    Cache,
    Redirect,
    Integrity,
    Keepalive,
    False,
    Duplex,
    BodyUsed,
}

fn request_get(this: &JsValue, _: &[JsValue], prop: &RequestProp, _: &mut Context) -> JsResult<JsValue> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let data = object.downcast_ref::<RequestData>().ok_or_else(illegal_invocation)?;
    Ok(match prop {
        RequestProp::Method => js_str(&data.method),
        RequestProp::Url => js_str(data.url.as_str()),
        RequestProp::Headers => data.headers.clone().into(),
        RequestProp::Destination => js_str(""),
        RequestProp::Referrer => js_str(&data.referrer),
        RequestProp::ReferrerPolicy => js_str(&data.referrer_policy),
        RequestProp::Mode => js_str(mode_keyword(data.mode)),
        RequestProp::Credentials => js_str(credentials_keyword(data.credentials)),
        RequestProp::Cache => js_str(cache_keyword(data.cache)),
        RequestProp::Redirect => js_str(redirect_keyword(data.redirect)),
        RequestProp::Integrity => js_str(&data.integrity),
        RequestProp::Keepalive => data.keepalive.into(),
        RequestProp::False => false.into(),
        RequestProp::Duplex => js_str("half"),
        RequestProp::BodyUsed => (data.body.is_some() && data.used).into(),
    })
}

fn request_clone(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let (mut copy, list, guard) = {
        let data = object.downcast_ref::<RequestData>().ok_or_else(illegal_invocation)?;
        if data.body.is_some() && data.used {
            return Err(type_error("Failed to execute 'clone' on 'Request': Request body is already used"));
        }
        let guard = data.headers.downcast_ref::<HeadersData>().map_or(Guard::Request, |h| h.guard);
        let copy = RequestData {
            url: data.url.clone(),
            method: data.method.clone(),
            headers: data.headers.clone(),
            body: data.body.clone(),
            used: false,
            mode: data.mode,
            credentials: data.credentials,
            cache: data.cache,
            redirect: data.redirect,
            integrity: data.integrity.clone(),
            keepalive: data.keepalive,
            referrer: data.referrer.clone(),
            referrer_policy: data.referrer_policy.clone(),
        };
        (copy, headers_list(&data.headers), guard)
    };
    copy.headers = new_headers(list, guard, context)?;
    let proto = prototype::<RequestData>(context)?;
    Ok(JsObject::from_proto_and_data(proto, copy).into())
}

// ----- Response -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseType {
    Basic,
    Cors,
    Default,
    Error,
    Opaque,
    OpaqueRedirect,
}

#[derive(Trace, Finalize, JsData)]
struct ResponseData {
    #[unsafe_ignore_trace]
    kind: ResponseType,
    #[unsafe_ignore_trace]
    url: Option<Url>,
    #[unsafe_ignore_trace]
    redirected: bool,
    #[unsafe_ignore_trace]
    status: u16,
    #[unsafe_ignore_trace]
    status_text: String,
    headers: JsObject,
    #[unsafe_ignore_trace]
    body: Option<SharedBody>,
    #[unsafe_ignore_trace]
    used: bool,
}

impl std::fmt::Debug for ResponseData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response").field("status", &self.status).finish()
    }
}

/// Fetch's null body statuses.
fn is_null_body_status(status: u16) -> bool {
    matches!(status, 101 | 103 | 204 | 205 | 304)
}

/// RFC 9110's `reason-phrase`: tabs, spaces, visible characters and
/// obs-text.
fn is_reason_phrase(s: &str) -> bool {
    s.chars().all(|c| {
        let c = c as u32;
        c == 0x09 || c == 0x20 || (0x21..=0x7E).contains(&c) || (0x80..=0xFF).contains(&c)
    })
}

/// Fetch's "initialize a response": status, status text and headers from
/// a `ResponseInit`, then the body.
fn init_response(
    data: &mut ResponseData,
    headers: &mut HeadersData,
    init: &JsValue,
    body: Option<(Vec<u8>, Option<&'static str>)>,
    what: &str,
    context: &mut Context,
) -> JsResult<()> {
    let init = match init {
        v if v.is_null_or_undefined() => None,
        v => Some(v.as_object().ok_or_else(|| type_error(format!("{what}: The provided value is not of type 'ResponseInit'.")))?),
    };
    let init = init.as_ref();
    if let Some(status) = member(init, "status", context)? {
        // An `unsigned short`: the number modulo 2^16.
        let number = status.to_number(context)?;
        let status = if number.is_finite() { (number.trunc().rem_euclid(65536.0)) as u16 } else { 0 };
        if !(200..=599).contains(&status) {
            return Err(JsNativeError::range()
                .with_message(format!("{what}: The status provided ({status}) is outside the range [200, 599]."))
                .into());
        }
        data.status = status;
    }
    if let Some(text) = member(init, "statusText", context)? {
        let text = text.to_string(context)?.to_std_string_escaped();
        if !is_byte_string(&text) || !is_reason_phrase(&text) {
            return Err(type_error(format!("{what}: Invalid statusText")));
        }
        data.status_text = text;
    }
    if let Some(value) = member(init, "headers", context)? {
        headers_fill(headers, &value, context)?;
    }
    if let Some((bytes, content_type)) = body {
        if is_null_body_status(data.status) {
            return Err(type_error(format!("{what}: Response with null body status cannot have body")));
        }
        if let Some(content_type) = content_type
            && !headers.list.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            headers.list.push(("content-type".to_owned(), content_type.to_owned()));
        }
        data.body = Some(done_body(bytes));
    }
    Ok(())
}

fn blank_response(kind: ResponseType, status: u16, context: &mut Context) -> JsResult<ResponseData> {
    Ok(ResponseData {
        kind,
        url: None,
        redirected: false,
        status,
        status_text: String::new(),
        headers: new_headers(Vec::new(), Guard::Response, context)?,
        body: None,
        used: false,
    })
}

impl Class for ResponseData {
    const NAME: &'static str = "Response";
    const LENGTH: usize = 0;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (name, prop) in [
            ("type", ResponseProp::Type),
            ("url", ResponseProp::Url),
            ("redirected", ResponseProp::Redirected),
            ("status", ResponseProp::Status),
            ("ok", ResponseProp::Ok),
            ("statusText", ResponseProp::StatusText),
            ("headers", ResponseProp::Headers),
            ("bodyUsed", ResponseProp::BodyUsed),
        ] {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(response_get, prop));
            class.accessor(JsString::from(name), Some(get), None, ATTR);
        }
        class
            .method(js_string!("clone"), 0, NativeFunction::from_fn_ptr(response_clone))
            .static_method(js_string!("error"), 0, NativeFunction::from_fn_ptr(response_error))
            .static_method(js_string!("redirect"), 1, NativeFunction::from_fn_ptr(response_redirect))
            .static_method(js_string!("json"), 1, NativeFunction::from_fn_ptr(response_json));
        add_body_methods(class);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        const WHAT: &str = "Failed to construct 'Response'";
        if new_target.is_undefined() {
            return Err(type_error("Failed to construct 'Response': Please use the 'new' operator"));
        }
        let body = match args.get_or_undefined(0) {
            v if v.is_null_or_undefined() => None,
            v => Some(extract_body(v, context)?),
        };
        let mut data = blank_response(ResponseType::Default, 200, context)?;
        let mut headers = HeadersData {
            list: Vec::new(),
            guard: Guard::Response,
        };
        init_response(&mut data, &mut headers, args.get_or_undefined(1), body, WHAT, context)?;
        data.headers = new_headers(std::mem::take(&mut headers.list), Guard::Response, context)?;
        let proto = new_target_prototype::<Self>(new_target, context)?;
        Ok(JsObject::from_proto_and_data(proto, data))
    }
}

#[derive(Clone, Trace, Finalize)]
enum ResponseProp {
    Type,
    Url,
    Redirected,
    Status,
    Ok,
    StatusText,
    Headers,
    BodyUsed,
}

fn response_get(this: &JsValue, _: &[JsValue], prop: &ResponseProp, _: &mut Context) -> JsResult<JsValue> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let data = object.downcast_ref::<ResponseData>().ok_or_else(illegal_invocation)?;
    Ok(match prop {
        ResponseProp::Type => js_str(match data.kind {
            ResponseType::Basic => "basic",
            ResponseType::Cors => "cors",
            ResponseType::Default => "default",
            ResponseType::Error => "error",
            ResponseType::Opaque => "opaque",
            ResponseType::OpaqueRedirect => "opaqueredirect",
        }),
        ResponseProp::Url => match &data.url {
            Some(url) => {
                let mut url = url.clone();
                url.set_fragment(None);
                js_str(url.as_str())
            }
            None => js_str(""),
        },
        ResponseProp::Redirected => data.redirected.into(),
        ResponseProp::Status => (data.status as i32).into(),
        ResponseProp::Ok => (200..=299).contains(&data.status).into(),
        ResponseProp::StatusText => js_str(&data.status_text),
        ResponseProp::Headers => data.headers.clone().into(),
        ResponseProp::BodyUsed => (data.body.is_some() && data.used).into(),
    })
}

fn response_clone(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let (mut copy, list, guard) = {
        let data = object.downcast_ref::<ResponseData>().ok_or_else(illegal_invocation)?;
        if data.body.is_some() && data.used {
            return Err(type_error("Failed to execute 'clone' on 'Response': Response body is already used"));
        }
        let guard = data.headers.downcast_ref::<HeadersData>().map_or(Guard::Response, |h| h.guard);
        let copy = ResponseData {
            kind: data.kind,
            url: data.url.clone(),
            redirected: data.redirected,
            status: data.status,
            status_text: data.status_text.clone(),
            headers: data.headers.clone(),
            body: data.body.clone(),
            used: false,
        };
        (copy, headers_list(&data.headers), guard)
    };
    copy.headers = new_headers(list, guard, context)?;
    let proto = prototype::<ResponseData>(context)?;
    Ok(JsObject::from_proto_and_data(proto, copy).into())
}

fn response_error(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let mut data = blank_response(ResponseType::Error, 0, context)?;
    data.headers = new_headers(Vec::new(), Guard::Immutable, context)?;
    let proto = prototype::<ResponseData>(context)?;
    Ok(JsObject::from_proto_and_data(proto, data).into())
}

fn response_redirect(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    need_args(args, 1, "'redirect' on 'Response'")?;
    let text = string_arg(args, 0, context)?;
    let Some(url) = parse_api_url(&text, context)? else {
        return Err(type_error(format!("Failed to execute 'redirect' on 'Response': Invalid URL '{text}'")));
    };
    let status = match args.get(1).filter(|v| !v.is_undefined()) {
        Some(v) => {
            let number = v.to_number(context)?;
            if number.is_finite() { number.trunc().rem_euclid(65536.0) as u16 } else { 0 }
        }
        None => 302,
    };
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Err(JsNativeError::range()
            .with_message("Failed to execute 'redirect' on 'Response': Invalid status code")
            .into());
    }
    let mut data = blank_response(ResponseType::Default, status, context)?;
    data.headers = new_headers(vec![("location".to_owned(), url.to_string())], Guard::Immutable, context)?;
    let proto = prototype::<ResponseData>(context)?;
    Ok(JsObject::from_proto_and_data(proto, data).into())
}

fn response_json(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    const WHAT: &str = "Failed to execute 'json' on 'Response'";
    need_args(args, 1, "'json' on 'Response'")?;
    let stringify = helpers(context)?.json_stringify;
    let text = stringify.call(&JsValue::undefined(), &[args[0].clone()], context)?;
    let Some(text) = text.as_string() else {
        return Err(type_error(format!("{WHAT}: The data is not JSON serializable")));
    };
    let bytes = utf8_encode(&text);
    let mut data = blank_response(ResponseType::Default, 200, context)?;
    let mut headers = HeadersData {
        list: Vec::new(),
        guard: Guard::Response,
    };
    init_response(&mut data, &mut headers, args.get_or_undefined(1), Some((bytes, Some("application/json"))), WHAT, context)?;
    data.headers = new_headers(std::mem::take(&mut headers.list), Guard::Response, context)?;
    let proto = prototype::<ResponseData>(context)?;
    Ok(JsObject::from_proto_and_data(proto, data).into())
}

// ----- the body methods of Request and Response -----

fn add_body_methods(class: &mut ClassBuilder<'_>) {
    for (name, how) in [
        ("arrayBuffer", Consume::ArrayBuffer),
        ("bytes", Consume::Bytes),
        ("text", Consume::Text),
        ("json", Consume::Json),
    ] {
        class.method(JsString::from(name), 0, NativeFunction::from_copy_closure_with_captures(body_method, how as u8));
    }
}

fn body_method(this: &JsValue, _: &[JsValue], how: &u8, context: &mut Context) -> JsResult<JsValue> {
    let how = match *how {
        0 => Consume::ArrayBuffer,
        1 => Consume::Bytes,
        2 => Consume::Text,
        _ => Consume::Json,
    };
    // A wrong receiver rejects, as every promise-returning operation.
    let taken = this.as_object().and_then(|object| {
        if let Some(mut data) = object.downcast_mut::<ResponseData>() {
            let used = data.used;
            data.used |= data.body.is_some();
            return Some((data.body.clone(), used));
        }
        if let Some(mut data) = object.downcast_mut::<RequestData>() {
            let used = data.used;
            data.used |= data.body.is_some();
            return Some((data.body.clone(), used));
        }
        None
    });
    match taken {
        Some((body, used)) => consume(body, used, how, context),
        None => {
            let (promise, resolvers) = JsPromise::new_pending(context);
            settle(&resolvers, Err(illegal_invocation()), context)?;
            Ok(promise.into())
        }
    }
}

// ----- fetch() -----

fn fetch(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (promise, resolvers) = JsPromise::new_pending(context);
    let made = if args.is_empty() {
        Err(type_error("Failed to execute 'fetch' on 'Window': 1 argument required, but only 0 present."))
    } else {
        make_request(&args[0], args.get_or_undefined(1), context)
    };
    let request = match made {
        Ok(request) => request,
        Err(err) => {
            settle(&resolvers, Err(err), context)?;
            return Ok(promise.into());
        }
    };
    let shared = net(context)?;
    let mut state = shared.borrow_mut();
    let id = state.next_id();
    let body = request.body.as_ref().map(|b| b.borrow().bytes.clone());
    state.ops.push(NetOp::Start(NetRequest {
        id,
        url: request.url.clone(),
        method: request.method.clone(),
        headers: headers_list(&request.headers),
        body,
        mode: request.mode,
        credentials: request.credentials,
        cache: request.cache,
        redirect: request.redirect,
        integrity: request.integrity.clone(),
        force_preflight: false,
        timeout: None,
        keepalive: request.keepalive,
    }));
    state.consumers.insert(
        id,
        Consumer::Fetch(FetchConsumer {
            resolvers: Some(resolvers),
            method: request.method.clone(),
            url: request.url.clone(),
            body: None,
        }),
    );
    Ok(promise.into())
}

/// The most a beacon may carry (Fetch's keepalive quota).
const BEACON_LIMIT: usize = 64 * 1024;

/// `navigator.sendBeacon(url, data)` (Beacon §3.1): a `POST` with
/// credentials whose answer nobody reads; `no-cors` unless the data's
/// type is not a CORS-safelisted `Content-Type`. False when the data is
/// over the quota.
fn send_beacon(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    const WHAT: &str = "Failed to execute 'sendBeacon' on 'Navigator'";
    if args.is_empty() {
        return Err(type_error(format!("{WHAT}: 1 argument required, but only 0 present.")));
    }
    let text = string_arg(args, 0, context)?;
    let Some(url) = parse_api_url(&text, context)? else {
        return Err(type_error(format!("{WHAT}: The URL argument is ill-formed or unsupported.")));
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(type_error(format!("{WHAT}: Beacons are only supported over HTTP(S).")));
    }
    let mut headers = Vec::new();
    let mut body = None;
    let mut mode = RequestMode::NoCors;
    let data = args.get_or_undefined(1);
    if !data.is_null_or_undefined() {
        let (bytes, content_type) = extract_body(data, context)?;
        if bytes.len() > BEACON_LIMIT {
            return Ok(false.into());
        }
        if let Some(content_type) = content_type {
            if !is_cors_safelisted_request_header("content-type", content_type) {
                mode = RequestMode::Cors;
            }
            headers.push(("Content-Type".to_owned(), content_type.to_owned()));
        }
        body = Some(bytes);
    }
    let shared = net(context)?;
    let mut state = shared.borrow_mut();
    let id = state.next_id();
    state.ops.push(NetOp::Start(NetRequest {
        id,
        url,
        method: "POST".to_owned(),
        headers,
        body,
        mode,
        credentials: CredentialsMode::Include,
        cache: RequestCache::Default,
        redirect: RedirectMode::Follow,
        integrity: String::new(),
        force_preflight: false,
        timeout: None,
        keepalive: true,
    }));
    Ok(true.into())
}

/// The tab's answers. Each returns the `XMLHttpRequest` the request is
/// one's, for `xhr.rs` to go on with.
pub(crate) enum NetEvent {
    Response(NetResponseHead),
    Data(Vec<u8>),
    Done,
    Failed(NetFailure),
}

pub(crate) fn deliver(id: u64, event: NetEvent, context: &mut Context) -> JsResult<()> {
    let shared = net(context)?;
    let xhr = match shared.borrow().consumers.get(&id) {
        None => return Ok(()),
        Some(Consumer::Xhr(object)) => Some(object.clone()),
        Some(Consumer::Fetch(_)) => None,
    };
    if let Some(object) = xhr {
        if matches!(event, NetEvent::Done | NetEvent::Failed(_)) {
            shared.borrow_mut().consumers.remove(&id);
        }
        return crate::xhr::deliver(&object, event, context);
    }
    match event {
        NetEvent::Response(head) => {
            let (resolvers, method, request_url) = {
                let mut state = shared.borrow_mut();
                let Some(Consumer::Fetch(consumer)) = state.consumers.get_mut(&id) else { return Ok(()) };
                (consumer.resolvers.take(), consumer.method.clone(), consumer.url.clone())
            };
            let (kind, guard) = match head.kind {
                ResponseKind::Basic => (ResponseType::Basic, Guard::Immutable),
                ResponseKind::Cors => (ResponseType::Cors, Guard::Immutable),
                ResponseKind::Opaque => (ResponseType::Opaque, Guard::Immutable),
                ResponseKind::OpaqueRedirect => (ResponseType::OpaqueRedirect, Guard::Immutable),
            };
            let opaque = matches!(head.kind, ResponseKind::Opaque | ResponseKind::OpaqueRedirect);
            let body = if opaque || is_null_body_status(head.status) || method == "HEAD" {
                None
            } else {
                Some(Rc::new(RefCell::new(BodySource {
                    bytes: Vec::new(),
                    state: BodyState::Loading,
                    waiters: Vec::new(),
                })))
            };
            let data = ResponseData {
                kind,
                url: match head.kind {
                    ResponseKind::Opaque => None,
                    ResponseKind::OpaqueRedirect => Some(request_url),
                    _ => Some(head.url),
                },
                redirected: !opaque && head.redirected,
                status: if opaque { 0 } else { head.status },
                status_text: if opaque { String::new() } else { head.status_text },
                headers: new_headers(if opaque { Vec::new() } else { head.headers }, guard, context)?,
                body: body.clone(),
                used: false,
            };
            if let Some(Consumer::Fetch(consumer)) = shared.borrow_mut().consumers.get_mut(&id) {
                consumer.body = body;
            }
            let proto = prototype::<ResponseData>(context)?;
            let response = JsObject::from_proto_and_data(proto, data);
            if let Some(resolvers) = resolvers {
                resolvers.resolve.call(&JsValue::undefined(), &[response.into()], context)?;
            }
        }
        NetEvent::Data(bytes) => {
            if let Some(Consumer::Fetch(consumer)) = shared.borrow().consumers.get(&id)
                && let Some(body) = &consumer.body
            {
                body.borrow_mut().bytes.extend_from_slice(&bytes);
            }
        }
        NetEvent::Done | NetEvent::Failed(_) => {
            let Some(Consumer::Fetch(consumer)) = shared.borrow_mut().consumers.remove(&id) else { return Ok(()) };
            let ok = matches!(event, NetEvent::Done);
            if let Some(resolvers) = consumer.resolvers {
                // No head came: the fetch itself failed.
                settle(&resolvers, Err(type_error("Failed to fetch")), context)?;
            }
            if let Some(body) = consumer.body {
                finish_body(&body, ok, context)?;
            }
        }
    }
    Ok(())
}

/// Register `fetch`, `Headers`, `Request` and `Response`.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let shared: SharedNet = Rc::new(RefCell::new(NetState::default()));
    context.insert_data(shared.clone());
    context.register_global_class::<HeadersClass>()?;
    context.register_global_class::<RequestData>()?;
    context.register_global_class::<ResponseData>()?;
    context.register_global_builtin_callable(js_string!("fetch"), 1, NativeFunction::from_fn_ptr(fetch))?;
    // `navigator.sendBeacon` (left to this item by Phase 3 item 3.1).
    if let Some(navigator) = context.global_object().get(js_string!("navigator"), context)?.as_object() {
        let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), NativeFunction::from_fn_ptr(send_beacon))
            .name(js_string!("sendBeacon"))
            .length(1)
            .build();
        navigator.set(js_string!("sendBeacon"), function, false, context)?;
    }

    // The helpers: made from source once, with what they use captured.
    let made = context.eval(Source::from_bytes(
        b"(function () {\
            const ownKeys = Reflect.ownKeys, describe = Object.getOwnPropertyDescriptor, iterator = Symbol.iterator, TE = TypeError;\
            const parse = JSON.parse, stringify = JSON.stringify;\
            const iteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][iterator]()));\
            function pairs(init) {\
                const out = [];\
                const method = init[iterator];\
                if (method !== undefined && method !== null) {\
                    for (const pair of init) {\
                        if (pair === null || (typeof pair !== 'object' && typeof pair !== 'function')) {\
                            throw new TE(\"Failed to construct 'Headers': The provided value cannot be converted to a sequence.\");\
                        }\
                        const items = [];\
                        for (const item of pair) { items[items.length] = `${item}`; }\
                        if (items.length !== 2) { throw new TE(\"Failed to construct 'Headers': Invalid value\"); }\
                        out[out.length] = items;\
                    }\
                } else {\
                    const keys = ownKeys(init);\
                    for (let i = 0; i < keys.length; i++) {\
                        const d = describe(init, keys[i]);\
                        if (d !== undefined && d.enumerable) { out[out.length] = [`${keys[i]}`, `${init[keys[i]]}`]; }\
                    }\
                }\
                return out;\
            }\
            return [pairs, function (text) { return parse(text); }, function (value) { return stringify(value); }, iteratorPrototype];\
        })()",
    ))?;
    let Some(made) = made.as_object() else {
        return Err(JsNativeError::error().with_message("fetch helpers").into());
    };
    let object_at = |index: u32, context: &mut Context| -> JsResult<JsObject> {
        made.get(index, context)?
            .as_object()
            .ok_or_else(|| JsNativeError::error().with_message("fetch helpers").into())
    };
    let iterator_prototype = object_at(3, context)?;
    let headers_iterator = ObjectInitializer::new(context)
        .function(NativeFunction::from_fn_ptr(headers_iterator_next), js_string!("next"), 0)
        .property(JsSymbol::to_string_tag(), js_string!("Headers Iterator"), Attribute::CONFIGURABLE)
        .build();
    headers_iterator.set_prototype(Some(iterator_prototype));
    let helpers = Helpers {
        pairs: object_at(0, context)?,
        json_parse: object_at(1, context)?,
        json_stringify: object_at(2, context)?,
        headers_iterator,
    };
    shared.borrow_mut().helpers = Some(helpers);
    Ok(())
}

/// `JSON.parse` as the realm made it, for `XMLHttpRequest`'s `json`
/// response type.
pub(crate) fn json_parse(text: &str, context: &mut Context) -> JsResult<JsValue> {
    let parse = helpers(context)?.json_parse;
    parse.call(&JsValue::undefined(), &[js_str(text)], context)
}
