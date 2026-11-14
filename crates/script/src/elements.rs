//! HTML's element interfaces (HTML §4 and the index of elements, the
//! obsolete ones of §16 included): every HTML element's wrapper has its
//! interface's prototype, so `instanceof HTMLDivElement` and the like
//! work, each interface object is on `window` and throws "Illegal
//! constructor" when called, and `Object.prototype.toString` names it.
//!
//! The interfaces with members of their own here (`HTMLElement`,
//! `HTMLImageElement`, the form controls) are classes of their own
//! (`dom.rs`, `forms.rs`); the rest are made here, empty, under
//! `HTMLElement` (`HTMLAudioElement` and `HTMLVideoElement` under
//! `HTMLMediaElement`). Their own members come with the items that need
//! them.

use std::collections::HashMap;

use boa_engine::object::FunctionObjectBuilder;
use boa_engine::property::PropertyDescriptor;
use boa_engine::{Context, JsData, JsNativeError, JsObject, JsResult, JsString, JsSymbol, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};

/// The interface of an HTML element by its local name. Names not in HTML
/// are `HTMLUnknownElement`, except valid custom element names, which
/// are `HTMLElement`.
pub(crate) fn interface_for(local: &str) -> &'static str {
    match local {
        "a" => "HTMLAnchorElement",
        "area" => "HTMLAreaElement",
        "audio" => "HTMLAudioElement",
        "base" => "HTMLBaseElement",
        "blockquote" | "q" => "HTMLQuoteElement",
        "body" => "HTMLBodyElement",
        "br" => "HTMLBRElement",
        "button" => "HTMLButtonElement",
        "canvas" => "HTMLCanvasElement",
        "caption" => "HTMLTableCaptionElement",
        "col" | "colgroup" => "HTMLTableColElement",
        "data" => "HTMLDataElement",
        "datalist" => "HTMLDataListElement",
        "del" | "ins" => "HTMLModElement",
        "details" => "HTMLDetailsElement",
        "dialog" => "HTMLDialogElement",
        "dir" => "HTMLDirectoryElement",
        "div" => "HTMLDivElement",
        "dl" => "HTMLDListElement",
        "embed" => "HTMLEmbedElement",
        "fieldset" => "HTMLFieldSetElement",
        "font" => "HTMLFontElement",
        "form" => "HTMLFormElement",
        "frame" => "HTMLFrameElement",
        "frameset" => "HTMLFrameSetElement",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "HTMLHeadingElement",
        "head" => "HTMLHeadElement",
        "hr" => "HTMLHRElement",
        "html" => "HTMLHtmlElement",
        "iframe" => "HTMLIFrameElement",
        "img" => "HTMLImageElement",
        "input" => "HTMLInputElement",
        "label" => "HTMLLabelElement",
        "legend" => "HTMLLegendElement",
        "li" => "HTMLLIElement",
        "link" => "HTMLLinkElement",
        "map" => "HTMLMapElement",
        "marquee" => "HTMLMarqueeElement",
        "menu" => "HTMLMenuElement",
        "meta" => "HTMLMetaElement",
        "meter" => "HTMLMeterElement",
        "object" => "HTMLObjectElement",
        "ol" => "HTMLOListElement",
        "optgroup" => "HTMLOptGroupElement",
        "option" => "HTMLOptionElement",
        "output" => "HTMLOutputElement",
        "p" => "HTMLParagraphElement",
        "param" => "HTMLParamElement",
        "picture" => "HTMLPictureElement",
        "pre" | "listing" | "xmp" => "HTMLPreElement",
        "progress" => "HTMLProgressElement",
        "script" => "HTMLScriptElement",
        "select" => "HTMLSelectElement",
        "slot" => "HTMLSlotElement",
        "source" => "HTMLSourceElement",
        "span" => "HTMLSpanElement",
        "style" => "HTMLStyleElement",
        "table" => "HTMLTableElement",
        "tbody" | "thead" | "tfoot" => "HTMLTableSectionElement",
        "td" | "th" => "HTMLTableCellElement",
        "template" => "HTMLTemplateElement",
        "textarea" => "HTMLTextAreaElement",
        "time" => "HTMLTimeElement",
        "title" => "HTMLTitleElement",
        "tr" => "HTMLTableRowElement",
        "track" => "HTMLTrackElement",
        "ul" => "HTMLUListElement",
        "video" => "HTMLVideoElement",
        // HTML's elements with no interface of their own, the obsolete
        // ones of §16 among them.
        "abbr" | "address" | "article" | "aside" | "b" | "bdi" | "bdo" | "cite" | "code" | "dd" | "dfn" | "dt" | "em" | "figcaption"
        | "figure" | "footer" | "header" | "hgroup" | "i" | "kbd" | "main" | "mark" | "nav" | "noscript" | "rp" | "rt" | "ruby" | "s"
        | "samp" | "search" | "section" | "small" | "strong" | "sub" | "summary" | "sup" | "u" | "var" | "wbr" | "acronym"
        | "basefont" | "big" | "center" | "nobr" | "noembed" | "noframes" | "plaintext" | "rb" | "rtc" | "strike" | "tt" => {
            "HTMLElement"
        }
        name if is_custom_element_name(name) => "HTMLElement",
        _ => "HTMLUnknownElement",
    }
}

/// HTML's "valid custom element name", for names as the parser and
/// `createElement` give them (lower case): a lower-case ASCII letter
/// first, a hyphen in it, none of the names SVG and MathML took.
fn is_custom_element_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.contains('-')
        && !name.bytes().any(|b| b.is_ascii_uppercase())
        && !matches!(
            name,
            "annotation-xml" | "color-profile" | "font-face" | "font-face-src" | "font-face-uri" | "font-face-format" | "font-face-name" | "missing-glyph"
        )
}

/// The interfaces made here, each with the interface it inherits from
/// (`HTMLMediaElement` before its two).
const MADE: &[(&str, &str)] = &[
    ("HTMLMediaElement", "HTMLElement"),
    ("HTMLAudioElement", "HTMLMediaElement"),
    ("HTMLVideoElement", "HTMLMediaElement"),
    ("HTMLAnchorElement", "HTMLElement"),
    ("HTMLAreaElement", "HTMLElement"),
    ("HTMLBaseElement", "HTMLElement"),
    ("HTMLBodyElement", "HTMLElement"),
    ("HTMLBRElement", "HTMLElement"),
    ("HTMLCanvasElement", "HTMLElement"),
    ("HTMLDataElement", "HTMLElement"),
    ("HTMLDataListElement", "HTMLElement"),
    ("HTMLDetailsElement", "HTMLElement"),
    ("HTMLDialogElement", "HTMLElement"),
    ("HTMLDirectoryElement", "HTMLElement"),
    ("HTMLDivElement", "HTMLElement"),
    ("HTMLDListElement", "HTMLElement"),
    ("HTMLEmbedElement", "HTMLElement"),
    ("HTMLFieldSetElement", "HTMLElement"),
    ("HTMLFontElement", "HTMLElement"),
    ("HTMLFrameElement", "HTMLElement"),
    ("HTMLFrameSetElement", "HTMLElement"),
    ("HTMLHeadElement", "HTMLElement"),
    ("HTMLHeadingElement", "HTMLElement"),
    ("HTMLHRElement", "HTMLElement"),
    ("HTMLHtmlElement", "HTMLElement"),
    ("HTMLIFrameElement", "HTMLElement"),
    ("HTMLLabelElement", "HTMLElement"),
    ("HTMLLegendElement", "HTMLElement"),
    ("HTMLLIElement", "HTMLElement"),
    ("HTMLLinkElement", "HTMLElement"),
    ("HTMLMapElement", "HTMLElement"),
    ("HTMLMarqueeElement", "HTMLElement"),
    ("HTMLMenuElement", "HTMLElement"),
    ("HTMLMetaElement", "HTMLElement"),
    ("HTMLMeterElement", "HTMLElement"),
    ("HTMLModElement", "HTMLElement"),
    ("HTMLObjectElement", "HTMLElement"),
    ("HTMLOListElement", "HTMLElement"),
    ("HTMLOptGroupElement", "HTMLElement"),
    ("HTMLOutputElement", "HTMLElement"),
    ("HTMLParagraphElement", "HTMLElement"),
    ("HTMLParamElement", "HTMLElement"),
    ("HTMLPictureElement", "HTMLElement"),
    ("HTMLPreElement", "HTMLElement"),
    ("HTMLProgressElement", "HTMLElement"),
    ("HTMLQuoteElement", "HTMLElement"),
    ("HTMLScriptElement", "HTMLElement"),
    ("HTMLSlotElement", "HTMLElement"),
    ("HTMLSourceElement", "HTMLElement"),
    ("HTMLSpanElement", "HTMLElement"),
    ("HTMLStyleElement", "HTMLElement"),
    ("HTMLTableCaptionElement", "HTMLElement"),
    ("HTMLTableCellElement", "HTMLElement"),
    ("HTMLTableColElement", "HTMLElement"),
    ("HTMLTableElement", "HTMLElement"),
    ("HTMLTableRowElement", "HTMLElement"),
    ("HTMLTableSectionElement", "HTMLElement"),
    ("HTMLTemplateElement", "HTMLElement"),
    ("HTMLTimeElement", "HTMLElement"),
    ("HTMLTitleElement", "HTMLElement"),
    ("HTMLTrackElement", "HTMLElement"),
    ("HTMLUListElement", "HTMLElement"),
    ("HTMLUnknownElement", "HTMLElement"),
];

/// The prototypes of the interfaces made here, by name.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
pub(crate) struct ElementInterfaces {
    #[unsafe_ignore_trace]
    names: HashMap<&'static str, usize>,
    prototypes: Vec<JsObject>,
}

/// The prototype for an element whose interface is `name`, when it is one
/// made here.
pub(crate) fn prototype(name: &str, context: &Context) -> Option<JsObject> {
    let interfaces = context.get_data::<ElementInterfaces>()?;
    interfaces.names.get(name).map(|&i| interfaces.prototypes[i].clone())
}

fn illegal_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Err(JsNativeError::typ().with_message("Illegal constructor").into())
}

/// Name an interface prototype for `Object.prototype.toString`
/// (WebIDL's @@toStringTag).
pub(crate) fn tag(prototype: &JsObject, name: &str, context: &mut Context) -> JsResult<()> {
    prototype.define_property_or_throw(
        JsSymbol::to_string_tag(),
        PropertyDescriptor::builder().value(JsString::from(name)).writable(false).enumerable(false).configurable(true),
        context,
    )?;
    Ok(())
}

/// Make the interfaces under `HTMLElement` (its prototype and its
/// constructor) and put them on the global object.
pub(crate) fn register(html_element: (JsObject, JsObject), context: &mut Context) -> JsResult<()> {
    let mut made: HashMap<&'static str, (JsObject, JsObject)> = HashMap::new();
    made.insert("HTMLElement", html_element);
    let mut names = HashMap::new();
    let mut prototypes = Vec::new();
    for &(name, parent) in MADE {
        let (parent_proto, parent_ctor) = made.get(parent).cloned().ok_or_else(|| JsNativeError::typ().with_message("element interface order"))?;
        let constructor = FunctionObjectBuilder::new(context.realm(), NativeFunction::from_fn_ptr(illegal_constructor))
            .name(JsString::from(name))
            .length(0)
            .constructor(true)
            .build();
        let constructor: JsObject = constructor.into();
        let proto = JsObject::with_object_proto(context.intrinsics());
        proto.set_prototype(Some(parent_proto));
        constructor.set_prototype(Some(parent_ctor));
        constructor.define_property_or_throw(
            js_string!("prototype"),
            PropertyDescriptor::builder().value(proto.clone()).writable(false).enumerable(false).configurable(false),
            context,
        )?;
        proto.define_property_or_throw(
            js_string!("constructor"),
            PropertyDescriptor::builder().value(constructor.clone()).writable(true).enumerable(false).configurable(true),
            context,
        )?;
        tag(&proto, name, context)?;
        context.global_object().define_property_or_throw(
            JsString::from(name),
            PropertyDescriptor::builder().value(constructor.clone()).writable(true).enumerable(false).configurable(true),
            context,
        )?;
        names.insert(name, prototypes.len());
        prototypes.push(proto.clone());
        made.insert(name, (proto, constructor));
    }
    context.insert_data(ElementInterfaces { names, prototypes });
    Ok(())
}
