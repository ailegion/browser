//! `<script>` elements a script inserts (HTML §4.12.1, "prepare the
//! script element" for an element that is not parser-inserted), the
//! `HTMLScriptElement` members, and `document.currentScript`.
//!
//! An inline classic script runs where it is inserted; an external one
//! and a module are the tab's to fetch and run (`HostRequest::LoadScript`,
//! `InlineModule`), as soon as possible or, after `async = false`, in the
//! order they were inserted.

use std::path::Path;

use boa_engine::object::FunctionObjectBuilder;
use boa_engine::property::PropertyDescriptor;
use boa_engine::{Context, JsArgs, JsObject, JsResult, JsString, JsValue, NativeFunction, Script, Source, js_string};
use browser_dom::NodeId;
use browser_dom::scripts::{ScriptType, script_type};
use browser_ipc_types::ReferrerPolicy;

use crate::dom::{dom, flush_images, illegal_invocation, node_id_of, string_replace_all, wrap, write_attribute};
use crate::{HostRequest, current_url, host_state, js_str};

const WHITESPACE: [char; 5] = ['\t', '\n', '\x0C', '\r', ' '];

/// What preparing an element decided.
enum Prepared {
    Nothing,
    Inline(String),
    Request(HostRequest),
}

/// "Prepare the script element" for one a script made.
fn prepare(element: NodeId, context: &mut Context) -> JsResult<Prepared> {
    let base = crate::fetch::api_base_url(context)?;
    let resolve = |raw: &str| match &base {
        Some(base) => base.join(raw).ok(),
        None => url::Url::parse(raw).ok(),
    };
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    let doc = &mut dom.doc;
    if !doc.contains(element) || !doc.is_script_element(element) || doc.is_parser_script(element) || doc.script_started(element) {
        return Ok(Prepared::Nothing);
    }
    let Some(e) = doc.element(element) else { return Ok(Prepared::Nothing) };
    let src = e.attr_ns("", "src").map(str::to_owned);
    // The child text content: its text node children's data.
    let text: String = doc.children(element).filter_map(|c| doc.get(c).as_text().map(str::to_owned)).collect();
    if (src.is_none() && text.is_empty()) || !doc.is_connected(element) {
        return Ok(Prepared::Nothing);
    }
    let Some(kind) = script_type(e) else { return Ok(Prepared::Nothing) };
    if kind == ScriptType::Classic {
        if e.attr_ns("", "nomodule").is_some() {
            return Ok(Prepared::Nothing);
        }
        // `for` and `event`: only `window` and `onload` run.
        if let (Some(target), Some(event)) = (e.attr_ns("", "for"), e.attr_ns("", "event")) {
            let event = event.trim_matches(WHITESPACE);
            if !target.trim_matches(WHITESPACE).eq_ignore_ascii_case("window") || !(event.eq_ignore_ascii_case("onload") || event.eq_ignore_ascii_case("onload()")) {
                return Ok(Prepared::Nothing);
            }
        }
    }
    let policy = ReferrerPolicy::from_attribute(e.attr_ns("", "referrerpolicy"));
    let in_order = e.attr_ns("", "async").is_none() && !doc.script_force_async(element);
    doc.mark_script_started(element);
    Ok(match src {
        Some(raw) => match resolve(&raw).filter(|_| !raw.is_empty()) {
            Some(url) => Prepared::Request(HostRequest::LoadScript { node: element, url, module: kind == ScriptType::Module, in_order, policy }),
            None => Prepared::Request(HostRequest::ScriptError { node: element }),
        },
        None if kind == ScriptType::Module => Prepared::Request(HostRequest::InlineModule { node: element, source: text, in_order, policy }),
        None => Prepared::Inline(text),
    })
}

/// Run an inline classic script where it stands, `document.currentScript`
/// naming its element meanwhile; what it throws is reported.
fn run_inline(element: NodeId, source: &str, context: &mut Context) -> JsResult<()> {
    let shared = dom(context)?;
    let name = current_url(&host_state(context)?.borrow().info);
    let previous = shared.borrow_mut().current_script.replace(element);
    let source = Source::from_bytes(source.as_bytes()).with_path(Path::new(name.as_str()));
    let result = Script::parse(source, None, context).and_then(|script| script.evaluate(context));
    let mut dom = shared.borrow_mut();
    dom.current_script = previous;
    if let Err(err) = result {
        dom.events.errors.push(format!("Uncaught {err}"));
    }
    Ok(())
}

/// Prepare every `<script>` that became connected, or whose children or
/// `src` changed, since the last call; a script run here may bring more.
pub(crate) fn prepare_pending(context: &mut Context) {
    let Ok(shared) = dom(context) else { return };
    loop {
        let candidates = {
            let mut dom = shared.borrow_mut();
            if !dom.doc.has_script_candidates() {
                return;
            }
            dom.doc.take_script_candidates()
        };
        for element in candidates {
            match prepare(element, context) {
                Ok(Prepared::Inline(source)) => {
                    let _ = run_inline(element, &source, context);
                }
                Ok(Prepared::Request(request)) => {
                    if let Ok(host) = host_state(context) {
                        host.borrow_mut().requests.push(request);
                    }
                }
                Ok(Prepared::Nothing) | Err(_) => {}
            }
        }
    }
}

// ----- HTMLScriptElement -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Member {
    Src,
    /// A string reflecting the attribute named.
    Text(&'static str),
    Flag(&'static str),
    Async,
    CrossOrigin,
    ReferrerPolicy,
    FetchPriority,
    ChildText,
}

const MEMBERS: &[(&str, Member)] = &[
    ("src", Member::Src),
    ("type", Member::Text("type")),
    ("charset", Member::Text("charset")),
    ("integrity", Member::Text("integrity")),
    ("event", Member::Text("event")),
    ("htmlFor", Member::Text("for")),
    ("noModule", Member::Flag("nomodule")),
    ("defer", Member::Flag("defer")),
    ("async", Member::Async),
    ("crossOrigin", Member::CrossOrigin),
    ("referrerPolicy", Member::ReferrerPolicy),
    ("fetchPriority", Member::FetchPriority),
    ("text", Member::ChildText),
];

const REFERRER_POLICIES: [&str; 8] = [
    "no-referrer",
    "no-referrer-when-downgrade",
    "same-origin",
    "origin",
    "strict-origin",
    "origin-when-cross-origin",
    "strict-origin-when-cross-origin",
    "unsafe-url",
];

fn this_script(this: &JsValue, context: &mut Context) -> JsResult<NodeId> {
    let id = node_id_of(this, context)?.ok_or_else(illegal_invocation)?;
    if dom(context)?.borrow().doc.is_script_element(id) { Ok(id) } else { Err(illegal_invocation()) }
}

fn get(this: &JsValue, _: &[JsValue], index: &usize, context: &mut Context) -> JsResult<JsValue> {
    let id = this_script(this, context)?;
    let base = crate::fetch::api_base_url(context)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    let doc = &dom.doc;
    let attr = |name: &str| doc.element(id).and_then(|e| e.attr_ns("", name));
    Ok(match MEMBERS[*index].1 {
        // The URL the attribute names, against the document base URL; as
        // written when it does not parse.
        Member::Src => js_str(
            &attr("src")
                .map(|raw| {
                    let resolved = match &base {
                        Some(base) => base.join(raw).ok(),
                        None => url::Url::parse(raw).ok(),
                    };
                    resolved.map_or_else(|| raw.to_owned(), |u| u.to_string())
                })
                .unwrap_or_default(),
        ),
        Member::Text(name) => js_str(attr(name).unwrap_or_default()),
        Member::Flag(name) => attr(name).is_some().into(),
        Member::Async => (doc.script_force_async(id) || attr("async").is_some()).into(),
        Member::CrossOrigin => match attr("crossorigin") {
            None => JsValue::null(),
            Some(v) if v.eq_ignore_ascii_case("use-credentials") => js_str("use-credentials"),
            Some(_) => js_str("anonymous"),
        },
        Member::ReferrerPolicy => js_str(attr("referrerpolicy").and_then(|v| REFERRER_POLICIES.iter().find(|p| p.eq_ignore_ascii_case(v))).copied().unwrap_or_default()),
        Member::FetchPriority => js_str(attr("fetchpriority").and_then(|v| ["high", "low", "auto"].into_iter().find(|p| p.eq_ignore_ascii_case(v))).unwrap_or("auto")),
        Member::ChildText => js_str(&doc.children(id).filter_map(|c| doc.get(c).as_text().map(str::to_owned)).collect::<String>()),
    })
}

fn set(this: &JsValue, args: &[JsValue], index: &usize, context: &mut Context) -> JsResult<JsValue> {
    let id = this_script(this, context)?;
    let given = args.get_or_undefined(0);
    let member = MEMBERS[*index].1;
    // The attribute and what it becomes (`None` removes it).
    let (name, value): (&str, Option<String>) = match member {
        Member::Src => ("src", Some(given.to_string(context)?.to_std_string_escaped())),
        Member::Text(name) => (name, Some(given.to_string(context)?.to_std_string_escaped())),
        Member::Flag(name) => (name, given.to_boolean().then(String::new)),
        Member::Async => ("async", given.to_boolean().then(String::new)),
        Member::CrossOrigin => ("crossorigin", if given.is_null() { None } else { Some(given.to_string(context)?.to_std_string_escaped()) }),
        Member::ReferrerPolicy => ("referrerpolicy", Some(given.to_string(context)?.to_std_string_escaped())),
        Member::FetchPriority => ("fetchpriority", Some(given.to_string(context)?.to_std_string_escaped())),
        Member::ChildText => {
            let text = given.to_string(context)?.to_std_string_escaped();
            return crate::custom::with_reactions(context, |context| {
                let shared = dom(context)?;
                string_replace_all(&mut shared.borrow_mut(), id, &text);
                flush_images(context);
                crate::observers::drain(context);
                Ok(JsValue::undefined())
            });
        }
    };
    crate::custom::with_reactions(context, |context| {
        let shared = dom(context)?;
        {
            let mut dom = shared.borrow_mut();
            // Assigning `async` ends "force async", whatever is assigned.
            if member == Member::Async {
                dom.doc.clear_script_force_async(id);
            }
            write_attribute(&mut dom, id, "", None, name, value.as_deref());
        }
        flush_images(context);
        crate::observers::drain(context);
        Ok(JsValue::undefined())
    })
}

/// `HTMLScriptElement.supports(type)`.
fn supports(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let kind = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    Ok(matches!(kind.as_str(), "classic" | "module").into())
}

/// `document.currentScript`: the classic script element being run.
fn current_script(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let current = dom(context)?.borrow().current_script;
    match current {
        Some(id) => wrap(id, context),
        None => Ok(JsValue::null()),
    }
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let (_, _, document) = crate::dom::node_element_document_prototypes(context)?;
    let get_current = crate::getter(context, "currentScript", NativeFunction::from_fn_ptr(current_script));
    document.define_property_or_throw(
        js_string!("currentScript"),
        PropertyDescriptor::builder().get(get_current).enumerable(true).configurable(true),
        context,
    )?;
    let Some(prototype) = crate::elements::prototype("HTMLScriptElement", context) else { return Ok(()) };
    for (index, (name, _)) in MEMBERS.iter().enumerate() {
        let getter = crate::getter(context, name, NativeFunction::from_copy_closure_with_captures(get, index));
        let setter = crate::setter(context, name, NativeFunction::from_copy_closure_with_captures(set, index));
        prototype.define_property_or_throw(
            JsString::from(*name),
            PropertyDescriptor::builder().get(getter).set(setter).enumerable(true).configurable(true),
            context,
        )?;
    }
    let constructor: Option<JsObject> = prototype.get(js_string!("constructor"), context)?.as_object();
    if let Some(constructor) = constructor {
        let function = FunctionObjectBuilder::new(context.realm(), NativeFunction::from_fn_ptr(supports)).name(js_string!("supports")).length(1).build();
        constructor.define_property_or_throw(
            js_string!("supports"),
            PropertyDescriptor::builder().value(function).writable(true).enumerable(true).configurable(true),
            context,
        )?;
    }
    Ok(())
}
