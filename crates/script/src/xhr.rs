//! `XMLHttpRequest` ("Working sites first" line 7), after the XMLHttpRequest
//! standard: `XMLHttpRequestEventTarget`, `XMLHttpRequestUpload`,
//! `XMLHttpRequest` and `ProgressEvent`.
//!
//! A request goes to the tab as a `NetOp` like a `fetch()` (mode `cors`,
//! credentials by `withCredentials`), and its answers come back through
//! `fetch::deliver`. A synchronous request runs to its end inside `send()`
//! through the tab's `SyncNet`.
//!
//! An `XMLHttpRequest` and its upload object keep their listeners in their
//! own data, where the collector traces them: a request nobody holds any
//! more goes with its listeners and its response, however its handlers
//! refer back to it. While a request is under way the network state holds
//! the object, as the standard's garbage collection rule requires.

use std::time::{Duration, Instant};

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::builtins::JsArray;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};
use url::Url;

use crate::dom::{dom, dom_exception, illegal_invocation};
use crate::events::{EventTargetRef, Listener, UiEventInit};
use crate::fetch::{
    CredentialsMode, NetEvent, NetFailure, NetOp, NetRequest, NetResponseHead, RedirectMode, RequestCache, RequestMode,
    is_forbidden_method, is_forbidden_request_header, is_token, mime_essence, net, normalize_method, normalize_value,
    parse_api_url,
};
use crate::{getter, js_str, setter};

const UNSENT: u8 = 0;
const OPENED: u8 = 1;
const HEADERS_RECEIVED: u8 = 2;
const LOADING: u8 = 3;
const DONE: u8 = 4;

/// `responseType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseType {
    Default,
    ArrayBuffer,
    Blob,
    Document,
    Json,
    Text,
}

impl ResponseType {
    fn keyword(self) -> &'static str {
        match self {
            ResponseType::Default => "",
            ResponseType::ArrayBuffer => "arraybuffer",
            ResponseType::Blob => "blob",
            ResponseType::Document => "document",
            ResponseType::Json => "json",
            ResponseType::Text => "text",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "" => ResponseType::Default,
            "arraybuffer" => ResponseType::ArrayBuffer,
            "blob" => ResponseType::Blob,
            "document" => ResponseType::Document,
            "json" => ResponseType::Json,
            "text" => ResponseType::Text,
            _ => return None,
        })
    }
}

/// The request's state as the standard names it.
struct State {
    ready_state: u8,
    method: String,
    url: Option<Url>,
    synchronous: bool,
    author_headers: Vec<(String, String)>,
    with_credentials: bool,
    timeout_ms: u32,
    send_flag: bool,
    upload_complete: bool,
    upload_listener: bool,
    timed_out: bool,
    /// The network request under way.
    request: Option<u64>,
    sent_at: Option<Instant>,
    /// The request body's length, for the upload's progress events.
    body_length: u64,
    /// The response's head; `None` is the standard's network error.
    response: Option<NetResponseHead>,
    received: Vec<u8>,
    response_type: ResponseType,
    override_mime: Option<String>,
    /// The response object could not be made: `response` is null.
    response_failed: bool,
    /// The text response, for the number of bytes it was decoded from.
    text: Option<(usize, String)>,
    /// When a `progress` event last fired for an arriving body.
    last_progress: Option<Instant>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            ready_state: UNSENT,
            method: String::new(),
            url: None,
            synchronous: false,
            author_headers: Vec::new(),
            with_credentials: false,
            timeout_ms: 0,
            send_flag: false,
            upload_complete: false,
            upload_listener: false,
            timed_out: false,
            request: None,
            sent_at: None,
            body_length: 0,
            response: None,
            received: Vec::new(),
            response_type: ResponseType::Default,
            override_mime: None,
            response_failed: false,
            text: None,
            last_progress: None,
        }
    }
}

/// What an `XMLHttpRequest` object carries.
#[derive(Trace, Finalize, JsData)]
pub(crate) struct XhrData {
    #[unsafe_ignore_trace]
    pub(crate) id: u64,
    pub(crate) listeners: Vec<Listener>,
    upload: Option<JsObject>,
    /// The `response` object once made (an `ArrayBuffer`, a parsed JSON
    /// value).
    response_object: Option<JsValue>,
    #[unsafe_ignore_trace]
    state: State,
}

impl std::fmt::Debug for XhrData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XMLHttpRequest").field("readyState", &self.state.ready_state).finish()
    }
}

/// What an `XMLHttpRequestUpload` object carries.
#[derive(Trace, Finalize, JsData)]
pub(crate) struct UploadData {
    #[unsafe_ignore_trace]
    pub(crate) id: u64,
    pub(crate) listeners: Vec<Listener>,
}

impl std::fmt::Debug for UploadData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("XMLHttpRequestUpload")
    }
}

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

fn prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

// ----- classes -----

/// `XMLHttpRequestEventTarget`: the handlers both objects share.
#[derive(Debug, Trace, Finalize, JsData)]
struct XhrEventTargetClass;

const TARGET_HANDLERS: &[&str] = &["onloadstart", "onprogress", "onabort", "onerror", "onload", "ontimeout", "onloadend"];

impl Class for XhrEventTargetClass {
    const NAME: &'static str = "XMLHttpRequestEventTarget";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        crate::events::add_named_handler_attributes(class, TARGET_HANDLERS);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct XhrUploadClass;

impl Class for XhrUploadClass {
    const NAME: &'static str = "XMLHttpRequestUpload";

    fn init(_: &mut ClassBuilder<'_>) -> JsResult<()> {
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct XhrClass;

impl Class for XhrClass {
    const NAME: &'static str = "XMLHttpRequest";
    const LENGTH: usize = 0;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        crate::events::add_named_handler_attributes(class, &["onreadystatechange"]);
        for (name, value) in [
            ("UNSENT", UNSENT),
            ("OPENED", OPENED),
            ("HEADERS_RECEIVED", HEADERS_RECEIVED),
            ("LOADING", LOADING),
            ("DONE", DONE),
        ] {
            class.property(JsString::from(name), value as i32, Attribute::ENUMERABLE);
            class.static_property(JsString::from(name), value as i32, Attribute::ENUMERABLE);
        }
        for (name, prop, settable) in [
            ("readyState", Prop::ReadyState, false),
            ("timeout", Prop::Timeout, true),
            ("withCredentials", Prop::WithCredentials, true),
            ("upload", Prop::Upload, false),
            ("responseURL", Prop::ResponseUrl, false),
            ("status", Prop::Status, false),
            ("statusText", Prop::StatusText, false),
            ("responseType", Prop::ResponseType, true),
            ("response", Prop::Response, false),
            ("responseText", Prop::ResponseText, false),
            ("responseXML", Prop::ResponseXml, false),
        ] {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(xhr_get, prop.clone()));
            let set = settable.then(|| setter(class.context(), name, NativeFunction::from_copy_closure_with_captures(xhr_set, prop)));
            class.accessor(JsString::from(name), Some(get), set, ATTR);
        }
        class
            .method(js_string!("open"), 2, NativeFunction::from_fn_ptr(xhr_open))
            .method(js_string!("setRequestHeader"), 2, NativeFunction::from_fn_ptr(xhr_set_request_header))
            .method(js_string!("send"), 0, NativeFunction::from_fn_ptr(xhr_send))
            .method(js_string!("abort"), 0, NativeFunction::from_fn_ptr(xhr_abort))
            .method(js_string!("getResponseHeader"), 1, NativeFunction::from_fn_ptr(xhr_get_response_header))
            .method(js_string!("getAllResponseHeaders"), 0, NativeFunction::from_fn_ptr(xhr_get_all_response_headers))
            .method(js_string!("overrideMimeType"), 1, NativeFunction::from_fn_ptr(xhr_override_mime_type));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(JsNativeError::typ()
                .with_message("Failed to construct 'XMLHttpRequest': Please use the 'new' operator")
                .into());
        }
        let shared = dom(context)?;
        let (id, upload_id) = {
            let mut dom = shared.borrow_mut();
            (dom.events.next_plain_id(), dom.events.next_plain_id())
        };
        let upload_proto = prototype::<XhrUploadClass>(context)?;
        let upload = JsObject::from_proto_and_data(
            upload_proto,
            UploadData {
                id: upload_id,
                listeners: Vec::new(),
            },
        );
        let proto = match new_target.as_object() {
            Some(o) => o.get(js_string!("prototype"), context)?.as_object(),
            None => None,
        };
        let proto = match proto {
            Some(p) => p,
            None => prototype::<XhrClass>(context)?,
        };
        Ok(JsObject::from_proto_and_data(
            proto,
            XhrData {
                id,
                listeners: Vec::new(),
                upload: Some(upload),
                response_object: None,
                state: State::default(),
            },
        ))
    }
}

/// `ProgressEvent`: `lengthComputable`, `loaded`, `total`.
#[derive(Debug, Trace, Finalize, JsData)]
struct ProgressEventClass;

impl Class for ProgressEventClass {
    const NAME: &'static str = "ProgressEvent";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (index, name) in ["lengthComputable", "loaded", "total"].into_iter().enumerate() {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(progress_get, index as u32));
            class.accessor(JsString::from(name), Some(get), None, ATTR);
        }
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.is_empty() {
            return Err(JsNativeError::typ()
                .with_message("Failed to construct 'ProgressEvent': 1 argument required, called with new")
                .into());
        }
        let (mut computable, mut loaded, mut total) = (false, 0.0, 0.0);
        if let Some(init) = args.get_or_undefined(1).as_object() {
            computable = init.get(js_string!("lengthComputable"), context)?.to_boolean();
            let number = |name: &str, context: &mut Context| -> JsResult<f64> {
                let v = init.get(JsString::from(name), context)?;
                if v.is_undefined() {
                    return Ok(0.0);
                }
                let n = v.to_number(context)?;
                // An `unsigned long long`: whole, not negative.
                Ok(if n.is_finite() { n.trunc().max(0.0) } else { 0.0 })
            };
            loaded = number("loaded", context)?;
            total = number("total", context)?;
        }
        let proto = prototype::<ProgressEventClass>(context)?;
        let value = JsArray::from_iter([computable.into(), loaded.into(), total.into()], context);
        crate::events::constructed_event_with_value(args, proto, value.into(), context)
    }
}

fn progress_get(this: &JsValue, _: &[JsValue], index: &u32, context: &mut Context) -> JsResult<JsValue> {
    let value = crate::events::event_value(this).ok_or_else(illegal_invocation)?;
    let Some(array) = value.as_object() else { return Err(illegal_invocation()) };
    array.get(*index, context)
}

// ----- events -----

fn target_of(object: &JsObject) -> Option<u64> {
    if let Some(x) = object.downcast_ref::<XhrData>() {
        return Some(x.id);
    }
    object.downcast_ref::<UploadData>().map(|u| u.id)
}

fn fire_plain(xhr: &JsObject, kind: &str, context: &mut Context) -> JsResult<()> {
    let Some(id) = target_of(xhr) else { return Ok(()) };
    let event = crate::events::new_event(kind, UiEventInit::default(), context)?;
    crate::events::dispatch(&event, EventTargetRef::Plain(id), Some(xhr), context)?;
    Ok(())
}

/// The standard's "fire a progress event": `lengthComputable` when the
/// total is known (not zero).
fn fire_progress(target: &JsObject, kind: &str, loaded: u64, total: u64, context: &mut Context) -> JsResult<()> {
    let Some(id) = target_of(target) else { return Ok(()) };
    let proto = prototype::<ProgressEventClass>(context)?;
    let value = JsArray::from_iter([(total != 0).into(), (loaded as f64).into(), (total as f64).into()], context);
    let event = crate::events::trusted_event_with_value(kind, proto, value.into(), context)?;
    crate::events::dispatch(&event, EventTargetRef::Plain(id), Some(target), context)?;
    Ok(())
}

// ----- helpers -----

fn this_xhr(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<XhrData>()).ok_or_else(illegal_invocation)
}

fn with<R>(xhr: &JsObject, f: impl FnOnce(&mut XhrData) -> R) -> JsResult<R> {
    let mut data = xhr.downcast_mut::<XhrData>().ok_or_else(illegal_invocation)?;
    Ok(f(&mut data))
}

fn invalid_state(what: &str, context: &mut Context) -> boa_engine::JsError {
    dom_exception("InvalidStateError", what, context)
}

/// The length a response declares (`Content-Length`), zero when it does
/// not or the values disagree.
fn content_length(head: &NetResponseHead) -> u64 {
    let mut found: Option<u64> = None;
    for (_, value) in head.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
        for part in value.split(',') {
            let Ok(n) = part.trim().parse::<u64>() else { return 0 };
            if found.is_some_and(|f| f != n) {
                return 0;
            }
            found = Some(n);
        }
    }
    found.unwrap_or(0)
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let values: Vec<&str> = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// A parameter of a MIME type string (`charset`), unquoted.
fn mime_parameter(value: &str, name: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let Some((key, v)) = part.split_once('=') else { continue };
        if key.trim().eq_ignore_ascii_case(name) {
            let v = v.trim();
            let v = v.strip_prefix('"').map_or(v, |inner| inner.split('"').next().unwrap_or(inner));
            return Some(v.to_owned());
        }
    }
    None
}

/// The standard's final MIME type: the override, else the response's
/// `Content-Type`, else `text/xml`.
fn final_mime(state: &State) -> String {
    if let Some(mime) = &state.override_mime {
        return mime.clone();
    }
    state
        .response
        .as_ref()
        .and_then(|r| header_value(&r.headers, "content-type"))
        .filter(|v| mime_essence(v).is_some())
        .unwrap_or_else(|| "text/xml".to_owned())
}

fn is_xml_mime(essence: &str) -> bool {
    essence == "text/xml" || essence == "application/xml" || essence.ends_with("+xml")
}

/// The encoding an XML document declares in its declaration, if any.
fn xml_declared_encoding(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let rest = text.strip_prefix("<?xml")?;
    let declaration = &rest[..rest.find("?>")?];
    let after = &declaration[declaration.find("encoding")? + "encoding".len()..];
    let after = after.trim_start().strip_prefix('=')?.trim_start();
    let quote = after.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let label = after[1..].split(quote).next()?;
    encoding_rs::Encoding::for_label(label.as_bytes())
}

/// The standard's text response.
fn text_response(state: &mut State) -> String {
    if state.response.is_none() {
        return String::new();
    }
    if let Some((length, text)) = &state.text
        && *length == state.received.len()
    {
        return text.clone();
    }
    let mime = final_mime(state);
    let mut encoding = mime_parameter(&mime, "charset").and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()));
    if encoding.is_none()
        && state.response_type == ResponseType::Default
        && mime_essence(&mime).is_some_and(|e| is_xml_mime(&e))
    {
        encoding = xml_declared_encoding(&state.received);
    }
    let encoding = encoding.unwrap_or(encoding_rs::UTF_8);
    // A byte order mark outranks the charset, as the Encoding standard's
    // "decode" has it.
    let (text, _, _) = encoding.decode(&state.received);
    let text = text.into_owned();
    state.text = Some((state.received.len(), text.clone()));
    text
}

// ----- properties -----

#[derive(Clone, Trace, Finalize)]
enum Prop {
    ReadyState,
    Timeout,
    WithCredentials,
    Upload,
    ResponseUrl,
    Status,
    StatusText,
    ResponseType,
    Response,
    ResponseText,
    ResponseXml,
}

fn xhr_get(this: &JsValue, _: &[JsValue], prop: &Prop, context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    match prop {
        Prop::ReadyState => with(&xhr, |x| (x.state.ready_state as i32).into()),
        Prop::Timeout => with(&xhr, |x| x.state.timeout_ms.into()),
        Prop::WithCredentials => with(&xhr, |x| x.state.with_credentials.into()),
        Prop::Upload => with(&xhr, |x| x.upload.clone().map_or(JsValue::null(), JsValue::from)),
        Prop::ResponseUrl => with(&xhr, |x| match &x.state.response {
            Some(head) => {
                let mut url = head.url.clone();
                url.set_fragment(None);
                js_str(url.as_str())
            }
            None => js_str(""),
        }),
        Prop::Status => with(&xhr, |x| (x.state.response.as_ref().map_or(0, |r| r.status) as i32).into()),
        Prop::StatusText => with(&xhr, |x| js_str(x.state.response.as_ref().map_or("", |r| r.status_text.as_str()))),
        Prop::ResponseType => with(&xhr, |x| js_str(x.state.response_type.keyword())),
        Prop::ResponseText => {
            let (kind, ready) = with(&xhr, |x| (x.state.response_type, x.state.ready_state))?;
            if !matches!(kind, ResponseType::Default | ResponseType::Text) {
                return Err(invalid_state(
                    "Failed to read the 'responseText' property from 'XMLHttpRequest': The value is only accessible if the object's 'responseType' is '' or 'text'.",
                    context,
                ));
            }
            if ready != LOADING && ready != DONE {
                return Ok(js_str(""));
            }
            with(&xhr, |x| js_str(&text_response(&mut x.state)))
        }
        Prop::ResponseXml => {
            let kind = with(&xhr, |x| x.state.response_type)?;
            if !matches!(kind, ResponseType::Default | ResponseType::Document) {
                return Err(invalid_state(
                    "Failed to read the 'responseXML' property from 'XMLHttpRequest': The value is only accessible if the object's 'responseType' is '' or 'document'.",
                    context,
                ));
            }
            // A document response needs a second document a script can
            // hold; there is none yet.
            Ok(JsValue::null())
        }
        Prop::Response => {
            let (kind, ready, failed, cached) =
                with(&xhr, |x| (x.state.response_type, x.state.ready_state, x.state.response_failed, x.response_object.clone()))?;
            if matches!(kind, ResponseType::Default | ResponseType::Text) {
                if ready != LOADING && ready != DONE {
                    return Ok(js_str(""));
                }
                return with(&xhr, |x| js_str(&text_response(&mut x.state)));
            }
            if ready != DONE || failed {
                return Ok(JsValue::null());
            }
            if let Some(object) = cached {
                return Ok(object);
            }
            let (bytes, has_response) = with(&xhr, |x| (x.state.received.clone(), x.state.response.is_some()))?;
            if !has_response {
                return Ok(JsValue::null());
            }
            let made: Option<JsValue> = match kind {
                ResponseType::ArrayBuffer => crate::fetch::array_buffer(&bytes, context).ok(),
                ResponseType::Json => crate::fetch::json_parse(&crate::fetch::utf8_decode(&bytes), context).ok(),
                // Blob and document responses: no `Blob`, no second
                // document.
                _ => None,
            };
            with(&xhr, |x| match &made {
                Some(value) => x.response_object = Some(value.clone()),
                None => x.state.response_failed = true,
            })?;
            Ok(made.unwrap_or(JsValue::null()))
        }
    }
}

fn xhr_set(this: &JsValue, args: &[JsValue], prop: &Prop, context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    let value = args.get_or_undefined(0);
    match prop {
        Prop::Timeout => {
            let ms = value.to_u32(context)?;
            let (synchronous, request, sent_at) = with(&xhr, |x| (x.state.synchronous && x.state.ready_state != UNSENT, x.state.request, x.state.sent_at))?;
            if synchronous {
                return Err(dom_exception(
                    "InvalidAccessError",
                    "Failed to set the 'timeout' property on 'XMLHttpRequest': Timeouts cannot be set for synchronous requests made from a document.",
                    context,
                ));
            }
            with(&xhr, |x| x.state.timeout_ms = ms)?;
            if let (Some(id), Some(_)) = (request, sent_at) {
                let timeout = (ms != 0).then(|| Duration::from_millis(ms.into()));
                net(context)?.borrow_mut().ops.push(NetOp::SetTimeout(id, timeout));
            }
        }
        Prop::WithCredentials => {
            let on = value.to_boolean();
            let (ready, sent) = with(&xhr, |x| (x.state.ready_state, x.state.send_flag))?;
            if (ready != UNSENT && ready != OPENED) || sent {
                return Err(invalid_state(
                    "Failed to set the 'withCredentials' property on 'XMLHttpRequest': The value may only be set if the object's state is UNSENT or OPENED.",
                    context,
                ));
            }
            with(&xhr, |x| x.state.with_credentials = on)?;
        }
        Prop::ResponseType => {
            let text = value.to_string(context)?.to_std_string_escaped();
            // Not one of the enumeration's values: the set is ignored.
            let Some(kind) = ResponseType::parse(&text) else { return Ok(JsValue::undefined()) };
            let (ready, synchronous) = with(&xhr, |x| (x.state.ready_state, x.state.synchronous && x.state.ready_state != UNSENT))?;
            if ready == LOADING || ready == DONE {
                return Err(invalid_state(
                    "Failed to set the 'responseType' property on 'XMLHttpRequest': The response type cannot be set if the object's state is LOADING or DONE.",
                    context,
                ));
            }
            if synchronous {
                return Err(dom_exception(
                    "InvalidAccessError",
                    "Failed to set the 'responseType' property on 'XMLHttpRequest': The response type cannot be changed for synchronous requests made from a document.",
                    context,
                ));
            }
            with(&xhr, |x| x.state.response_type = kind)?;
        }
        _ => {}
    }
    Ok(JsValue::undefined())
}

// ----- methods -----

/// Stop the request under way without any event (`open()` again, or
/// `abort()` before its own steps).
fn terminate(xhr: &JsObject, context: &mut Context) -> JsResult<()> {
    let request = with(xhr, |x| x.state.request.take())?;
    if let Some(id) = request {
        let shared = net(context)?;
        let mut state = shared.borrow_mut();
        state.forget(id);
        state.ops.push(NetOp::Abort(id));
    }
    Ok(())
}

fn xhr_open(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    if args.len() < 2 {
        return Err(JsNativeError::typ()
            .with_message(format!(
                "Failed to execute 'open' on 'XMLHttpRequest': 2 arguments required, but only {} present.",
                args.len()
            ))
            .into());
    }
    let method = args[0].to_string(context)?.to_std_string_escaped();
    let url_text = args[1].to_string(context)?.to_std_string_escaped();
    let synchronous = args.len() >= 3 && !args[2].to_boolean();
    let credential = |index: usize, context: &mut Context| -> JsResult<Option<String>> {
        match args.get(index) {
            Some(v) if !v.is_null_or_undefined() => Ok(Some(v.to_string(context)?.to_std_string_escaped())),
            _ => Ok(None),
        }
    };
    let (user, password) = if args.len() >= 3 { (credential(3, context)?, credential(4, context)?) } else { (None, None) };
    if !is_token(&method) {
        return Err(dom_exception(
            "SyntaxError",
            &format!("Failed to execute 'open' on 'XMLHttpRequest': '{method}' is not a valid HTTP method."),
            context,
        ));
    }
    if is_forbidden_method(&method) {
        return Err(dom_exception(
            "SecurityError",
            &format!("Failed to execute 'open' on 'XMLHttpRequest': '{method}' HTTP method is unsupported."),
            context,
        ));
    }
    let method = normalize_method(&method);
    let Some(mut url) = parse_api_url(&url_text, context)? else {
        return Err(dom_exception(
            "SyntaxError",
            &format!("Failed to execute 'open' on 'XMLHttpRequest': Invalid URL '{url_text}'"),
            context,
        ));
    };
    if url.host().is_some() {
        if let Some(user) = &user {
            let _ = url.set_username(user);
        }
        if let Some(password) = &password {
            let _ = url.set_password(Some(password));
        }
    }
    let (timeout, kind) = with(&xhr, |x| (x.state.timeout_ms, x.state.response_type))?;
    if synchronous && (timeout != 0 || kind != ResponseType::Default) {
        return Err(dom_exception(
            "InvalidAccessError",
            "Failed to execute 'open' on 'XMLHttpRequest': Synchronous requests from a document must not set a response type or a timeout.",
            context,
        ));
    }
    terminate(&xhr, context)?;
    let fire = with(&xhr, |x| {
        let s = &mut x.state;
        s.send_flag = false;
        s.upload_listener = false;
        s.method = method;
        s.url = Some(url);
        s.synchronous = synchronous;
        s.author_headers.clear();
        s.response = None;
        s.received.clear();
        s.text = None;
        s.response_failed = false;
        s.timed_out = false;
        s.sent_at = None;
        s.last_progress = None;
        x.response_object = None;
        let fire = x.state.ready_state != OPENED;
        x.state.ready_state = OPENED;
        fire
    })?;
    if fire {
        fire_plain(&xhr, "readystatechange", context)?;
    }
    Ok(JsValue::undefined())
}

fn xhr_set_request_header(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    if args.len() < 2 {
        return Err(JsNativeError::typ()
            .with_message(format!(
                "Failed to execute 'setRequestHeader' on 'XMLHttpRequest': 2 arguments required, but only {} present.",
                args.len()
            ))
            .into());
    }
    let name = args[0].to_string(context)?.to_std_string_escaped();
    let value = args[1].to_string(context)?.to_std_string_escaped();
    let (ready, sent) = with(&xhr, |x| (x.state.ready_state, x.state.send_flag))?;
    if ready != OPENED || sent {
        return Err(invalid_state(
            "Failed to execute 'setRequestHeader' on 'XMLHttpRequest': The object's state must be OPENED.",
            context,
        ));
    }
    let value = normalize_value(&value).to_owned();
    let bytes = |s: &str| s.chars().all(|c| (c as u32) <= 0xFF);
    if !bytes(&name) || !is_token(&name) {
        return Err(dom_exception(
            "SyntaxError",
            &format!("Failed to execute 'setRequestHeader' on 'XMLHttpRequest': '{name}' is not a valid HTTP header field name."),
            context,
        ));
    }
    if !bytes(&value) || value.chars().any(|c| matches!(c, '\0' | '\r' | '\n')) {
        return Err(dom_exception(
            "SyntaxError",
            &format!("Failed to execute 'setRequestHeader' on 'XMLHttpRequest': '{value}' is not a valid HTTP header field value."),
            context,
        ));
    }
    if is_forbidden_request_header(&name, &value) {
        return Ok(JsValue::undefined());
    }
    // Fetch's "combine": a header of the name takes the value after a
    // comma and a space.
    with(&xhr, |x| {
        match x.state.author_headers.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&name)) {
            Some((_, existing)) => {
                existing.push_str(", ");
                existing.push_str(&value);
            }
            None => x.state.author_headers.push((name, value)),
        }
    })?;
    Ok(JsValue::undefined())
}

fn xhr_send(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    let (ready, sent, method) = with(&xhr, |x| (x.state.ready_state, x.state.send_flag, x.state.method.clone()))?;
    if ready != OPENED || sent {
        return Err(invalid_state(
            "Failed to execute 'send' on 'XMLHttpRequest': The object's state must be OPENED.",
            context,
        ));
    }
    // The body and the content type it implies.
    let body_arg = args.get_or_undefined(0);
    let mut body: Option<Vec<u8>> = None;
    let mut extracted_type: Option<String> = None;
    let mut is_text = false;
    if !body_arg.is_null_or_undefined() && !matches!(method.as_str(), "GET" | "HEAD") {
        let document = body_arg.as_object().and_then(|o| crate::dom::serialize_document_argument(&o, context));
        match document {
            Some((markup, html)) => {
                body = Some(markup.into_bytes());
                extracted_type = Some(if html { "text/html;charset=UTF-8" } else { "application/xml;charset=UTF-8" }.to_owned());
                is_text = true;
            }
            None => {
                let (bytes, content_type) = crate::fetch::extract_body(body_arg, context)?;
                is_text = content_type.is_some();
                extracted_type = content_type.map(str::to_owned);
                body = Some(bytes);
            }
        }
    }
    let upload = with(&xhr, |x| x.upload.clone())?;
    let upload_listener = upload
        .as_ref()
        .is_some_and(|u| u.downcast_ref::<UploadData>().is_some_and(|d| !d.listeners.is_empty()));
    let shared = net(context)?;
    let id = shared.borrow_mut().next_id();
    let request = with(&xhr, |x| {
        let s = &mut x.state;
        // An author `Content-Type` for a text body gets its charset set
        // to UTF-8; without one the body's own type is used.
        if body.is_some() {
            match s.author_headers.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                Some((_, value)) => {
                    if is_text
                        && mime_essence(value).is_some()
                        && let Some(charset) = mime_parameter(value, "charset")
                        && !charset.eq_ignore_ascii_case("utf-8")
                    {
                        let rebuilt: Vec<String> = value
                            .split(';')
                            .enumerate()
                            .map(|(i, part)| {
                                let is_charset =
                                    i > 0 && part.split_once('=').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("charset"));
                                if is_charset { "charset=UTF-8".to_owned() } else { part.trim().to_owned() }
                            })
                            .collect();
                        *value = rebuilt.join(";");
                    }
                }
                None => {
                    if let Some(content_type) = &extracted_type {
                        s.author_headers.push(("Content-Type".to_owned(), content_type.clone()));
                    }
                }
            }
        }
        s.upload_listener = upload_listener;
        s.upload_complete = body.is_none();
        s.timed_out = false;
        s.send_flag = true;
        s.body_length = body.as_ref().map_or(0, |b| b.len() as u64);
        s.request = Some(id);
        s.sent_at = Some(Instant::now());
        NetRequest {
            id,
            url: s.url.clone().unwrap_or_else(|| Url::parse("about:blank").expect("static url")),
            method: s.method.clone(),
            headers: s.author_headers.clone(),
            body: body.clone(),
            mode: RequestMode::Cors,
            credentials: if s.with_credentials { CredentialsMode::Include } else { CredentialsMode::SameOrigin },
            cache: RequestCache::Default,
            redirect: RedirectMode::Follow,
            integrity: String::new(),
            force_preflight: upload_listener,
            timeout: (s.timeout_ms != 0).then(|| Duration::from_millis(s.timeout_ms.into())),
            keepalive: false,
            referrer: crate::fetch::ScriptReferrer::Client,
            referrer_policy: browser_ipc_types::ReferrerPolicy::Empty,
        }
    })?;
    let synchronous = with(&xhr, |x| x.state.synchronous)?;
    if synchronous {
        let sync = shared.borrow().sync.clone();
        let result = match sync {
            Some(sync) => sync.fetch(request),
            None => Err(NetFailure::Network("no network".to_owned())),
        };
        with(&xhr, |x| x.state.request = None)?;
        return match result {
            Ok((head, bytes)) => {
                with(&xhr, |x| {
                    x.state.response = Some(head);
                    x.state.received = bytes;
                })?;
                end_of_body(&xhr, context)?;
                Ok(JsValue::undefined())
            }
            Err(failure) => {
                with(&xhr, |x| {
                    x.state.ready_state = DONE;
                    x.state.send_flag = false;
                    x.state.response = None;
                })?;
                Err(match failure {
                    NetFailure::Timeout => dom_exception("TimeoutError", "Failed to execute 'send' on 'XMLHttpRequest': The request timed out.", context),
                    NetFailure::Network(_) => {
                        let url = with(&xhr, |x| x.state.url.as_ref().map(|u| u.to_string()).unwrap_or_default())?;
                        dom_exception("NetworkError", &format!("Failed to execute 'send' on 'XMLHttpRequest': Failed to load '{url}'."), context)
                    }
                })
            }
        };
    }
    {
        let mut state = shared.borrow_mut();
        state.register_xhr(id, xhr.clone());
        state.ops.push(NetOp::Start(request));
    }
    let (upload_complete, body_length) = with(&xhr, |x| (x.state.upload_complete, x.state.body_length))?;
    fire_progress(&xhr, "loadstart", 0, 0, context)?;
    if !upload_complete
        && upload_listener
        && let Some(upload) = &upload
    {
        fire_progress(upload, "loadstart", 0, body_length, context)?;
    }
    Ok(JsValue::undefined())
}

/// The standard's "request error steps" for an asynchronous request.
fn request_error(xhr: &JsObject, kind: &str, context: &mut Context) -> JsResult<()> {
    let (upload_pending, upload_listener, upload) = with(xhr, |x| {
        let s = &mut x.state;
        s.ready_state = DONE;
        s.send_flag = false;
        s.response = None;
        s.received.clear();
        s.text = None;
        s.request = None;
        x.response_object = None;
        let pending = !x.state.upload_complete;
        x.state.upload_complete = true;
        (pending, x.state.upload_listener, x.upload.clone())
    })?;
    fire_plain(xhr, "readystatechange", context)?;
    if upload_pending
        && upload_listener
        && let Some(upload) = &upload
    {
        fire_progress(upload, kind, 0, 0, context)?;
        fire_progress(upload, "loadend", 0, 0, context)?;
    }
    fire_progress(xhr, kind, 0, 0, context)?;
    fire_progress(xhr, "loadend", 0, 0, context)?;
    Ok(())
}

/// The standard's "handle response end-of-body".
fn end_of_body(xhr: &JsObject, context: &mut Context) -> JsResult<()> {
    let (transmitted, length, synchronous) = with(xhr, |x| {
        let length = x.state.response.as_ref().map_or(0, content_length);
        (x.state.received.len() as u64, length, x.state.synchronous)
    })?;
    if !synchronous {
        fire_progress(xhr, "progress", transmitted, length, context)?;
    }
    with(xhr, |x| {
        x.state.ready_state = DONE;
        x.state.send_flag = false;
        x.state.request = None;
    })?;
    fire_plain(xhr, "readystatechange", context)?;
    fire_progress(xhr, "load", transmitted, length, context)?;
    fire_progress(xhr, "loadend", transmitted, length, context)?;
    Ok(())
}

/// The request body has gone out: the upload object's last events.
fn request_end_of_body(xhr: &JsObject, context: &mut Context) -> JsResult<()> {
    let (pending, listener, upload, length) = with(xhr, |x| {
        let pending = !x.state.upload_complete;
        x.state.upload_complete = true;
        (pending, x.state.upload_listener, x.upload.clone(), x.state.body_length)
    })?;
    if pending
        && listener
        && let Some(upload) = &upload
    {
        fire_progress(upload, "progress", length, length, context)?;
        fire_progress(upload, "load", length, length, context)?;
        fire_progress(upload, "loadend", length, length, context)?;
    }
    Ok(())
}

/// An answer from the tab for the request this object has under way.
pub(crate) fn deliver(xhr: &JsObject, event: NetEvent, context: &mut Context) -> JsResult<()> {
    if !with(xhr, |x| x.state.send_flag && x.state.request.is_some())? {
        return Ok(());
    }
    match event {
        NetEvent::Response(head) => {
            request_end_of_body(xhr, context)?;
            with(xhr, |x| {
                x.state.response = Some(head);
                x.state.ready_state = HEADERS_RECEIVED;
            })?;
            fire_plain(xhr, "readystatechange", context)?;
        }
        NetEvent::Data(bytes) => {
            let (ready, length, received, due) = with(xhr, |x| {
                let s = &mut x.state;
                s.received.extend_from_slice(&bytes);
                let now = Instant::now();
                // A `progress` event at most every 50 ms.
                let due = s.last_progress.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(50));
                if due {
                    s.last_progress = Some(now);
                }
                (s.ready_state, s.response.as_ref().map_or(0, content_length), s.received.len() as u64, due)
            })?;
            if !due || !matches!(ready, HEADERS_RECEIVED | LOADING) {
                return Ok(());
            }
            with(xhr, |x| x.state.ready_state = LOADING)?;
            fire_plain(xhr, "readystatechange", context)?;
            fire_progress(xhr, "progress", received, length, context)?;
        }
        NetEvent::Done => end_of_body(xhr, context)?,
        NetEvent::Failed(failure) => {
            let kind = match failure {
                NetFailure::Timeout => {
                    with(xhr, |x| x.state.timed_out = true)?;
                    "timeout"
                }
                NetFailure::Network(_) => "error",
            };
            request_error(xhr, kind, context)?;
        }
    }
    Ok(())
}

fn xhr_abort(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    terminate(&xhr, context)?;
    let (ready, sent) = with(&xhr, |x| (x.state.ready_state, x.state.send_flag))?;
    if (ready == OPENED && sent) || ready == HEADERS_RECEIVED || ready == LOADING {
        request_error(&xhr, "abort", context)?;
    }
    with(&xhr, |x| {
        if x.state.ready_state == DONE {
            x.state.ready_state = UNSENT;
            x.state.response = None;
            x.state.received.clear();
            x.state.text = None;
            x.response_object = None;
        }
    })?;
    Ok(JsValue::undefined())
}

fn xhr_get_response_header(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    let name = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    with(&xhr, |x| {
        x.state
            .response
            .as_ref()
            .and_then(|r| header_value(&r.headers, &name))
            .map_or(JsValue::null(), |v| js_str(&v))
    })
}

fn xhr_get_all_response_headers(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    with(&xhr, |x| {
        let Some(head) = &x.state.response else { return js_str("") };
        let mut names: Vec<String> = head.headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        // The standard's legacy order: by the names in upper case.
        names.sort_by_key(|n| n.to_ascii_uppercase());
        names.dedup();
        let mut out = String::new();
        for name in names {
            if let Some(value) = header_value(&head.headers, &name) {
                out.push_str(&name);
                out.push_str(": ");
                out.push_str(&value);
                out.push_str("\r\n");
            }
        }
        js_str(&out)
    })
}

fn xhr_override_mime_type(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let xhr = this_xhr(this)?;
    let mime = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let ready = with(&xhr, |x| x.state.ready_state)?;
    if ready == LOADING || ready == DONE {
        return Err(invalid_state(
            "Failed to execute 'overrideMimeType' on 'XMLHttpRequest': MimeType cannot be overridden when the state is LOADING or DONE.",
            context,
        ));
    }
    let mime = if mime_essence(&mime).is_some() { mime } else { "application/octet-stream".to_owned() };
    with(&xhr, |x| {
        x.state.override_mime = Some(mime);
        x.state.text = None;
    })?;
    Ok(JsValue::undefined())
}

/// Register the classes; the event classes must already be there.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<XhrEventTargetClass>()?;
    context.register_global_class::<XhrUploadClass>()?;
    context.register_global_class::<XhrClass>()?;
    context.register_global_class::<ProgressEventClass>()?;
    crate::events::inherit::<XhrEventTargetClass, crate::events::EventTargetClass>(context);
    crate::events::inherit::<XhrUploadClass, XhrEventTargetClass>(context);
    crate::events::inherit::<XhrClass, XhrEventTargetClass>(context);
    crate::events::inherit::<ProgressEventClass, crate::events::EventData>(context);
    Ok(())
}
