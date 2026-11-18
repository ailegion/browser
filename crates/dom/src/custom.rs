//! Custom elements, the part the tree keeps (DOM §4.9, HTML §4.13): each
//! element's custom element state, its `is` value and which definition it
//! was upgraded with. The definitions themselves hold script objects and
//! live in the script crate; here a definition is a number.
//!
//! The data sits beside the tree, only for the elements that have any: an
//! element with no entry is "undefined" when it is an HTML element with a
//! valid custom element name, "uncustomized" otherwise (what DOM's
//! "create an element" gives an element created with no definition).

use html5ever::ns;

use crate::{Document, NodeId, NodeKind};

/// An element's custom element state (DOM §4.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomElementState {
    Undefined,
    Failed,
    Uncustomized,
    Precustomized,
    Custom,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CustomData {
    /// A state that was set; `None` is the state the element's name and
    /// `is` value give it.
    state: Option<CustomElementState>,
    is: Option<Box<str>>,
    definition: Option<u32>,
}

impl CustomData {
    fn is_empty(&self) -> bool {
        self.state.is_none() && self.is.is_none() && self.definition.is_none()
    }
}

/// DOM's "valid element local name".
pub fn is_valid_element_local_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    if first.is_ascii_alphabetic() {
        return !name.chars().any(|c| matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ' | '\0' | '/' | '>'));
    }
    if !(first == ':' || first == '_' || first >= '\u{80}') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_') || c >= '\u{80}')
}

/// HTML's "valid custom element name": a valid element local name that
/// starts with a lower-case ASCII letter, has no upper-case ASCII letter,
/// has a hyphen, and is none of the names SVG and MathML took.
pub fn is_valid_custom_element_name(name: &str) -> bool {
    is_valid_element_local_name(name)
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.bytes().any(|b| b.is_ascii_uppercase())
        && name.contains('-')
        && !matches!(
            name,
            "annotation-xml" | "color-profile" | "font-face" | "font-face-src" | "font-face-uri" | "font-face-format" | "font-face-name" | "missing-glyph"
        )
}

impl Document {
    /// The element's custom element state.
    pub fn custom_element_state(&self, id: NodeId) -> CustomElementState {
        let data = self.custom.get(&id);
        if let Some(state) = data.and_then(|d| d.state) {
            return state;
        }
        match &self.get(id).kind {
            NodeKind::Element(e) if e.name.ns == ns!(html) && (data.is_some_and(|d| d.is.is_some()) || is_valid_custom_element_name(&e.name.local)) => {
                CustomElementState::Undefined
            }
            _ => CustomElementState::Uncustomized,
        }
    }

    pub fn set_custom_element_state(&mut self, id: NodeId, state: CustomElementState) {
        self.custom.entry(id).or_default().state = Some(state);
    }

    /// The element's `is` value: the `is` attribute the parser saw, or
    /// what `createElement` was given; a clone has its original's.
    pub fn is_value(&self, id: NodeId) -> Option<&str> {
        self.custom.get(&id).and_then(|d| d.is.as_deref())
    }

    pub fn set_is_value(&mut self, id: NodeId, is: Option<&str>) {
        match is {
            Some(is) => self.custom.entry(id).or_default().is = Some(is.into()),
            None => {
                if let Some(data) = self.custom.get_mut(&id) {
                    data.is = None;
                    if data.is_empty() {
                        self.custom.remove(&id);
                    }
                }
            }
        }
    }

    /// The definition the element was upgraded or constructed with (the
    /// script crate's number for it).
    pub fn custom_element_definition(&self, id: NodeId) -> Option<u32> {
        self.custom.get(&id).and_then(|d| d.definition)
    }

    pub fn set_custom_element_definition(&mut self, id: NodeId, definition: Option<u32>) {
        match definition {
            Some(d) => self.custom.entry(id).or_default().definition = Some(d),
            None => {
                if let Some(data) = self.custom.get_mut(&id) {
                    data.definition = None;
                    if data.is_empty() {
                        self.custom.remove(&id);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_states_and_is_values() {
        for (name, valid) in [
            ("my-element", true),
            ("x-", true),
            ("a-b.c_d", true),
            ("my-élément", true),
            ("My-element", false),
            ("my-Element", false),
            ("element", false),
            ("-element", false),
            ("1-element", false),
            ("font-face", false),
            ("annotation-xml", false),
            ("my element-x", false),
            ("my-el>", false),
            ("", false),
        ] {
            assert_eq!(is_valid_custom_element_name(name), valid, "{name:?}");
        }
        assert!(is_valid_element_local_name("div") && is_valid_element_local_name("_x") && is_valid_element_local_name(":a.b"));
        assert!(!is_valid_element_local_name("1a") && !is_valid_element_local_name("_x y") && !is_valid_element_local_name("a/b"));

        let mut doc = crate::parse_html(b"<!DOCTYPE html><div></div><my-el></my-el><p is=\"fancy-p\"></p><svg><a-b></a-b></svg>");
        let find = |doc: &Document, local: &str| {
            doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == local)).expect("element")
        };
        let (div, mine, p, svg_child) = (find(&doc, "div"), find(&doc, "my-el"), find(&doc, "p"), find(&doc, "a-b"));
        assert_eq!(doc.custom_element_state(div), CustomElementState::Uncustomized);
        assert_eq!(doc.custom_element_state(mine), CustomElementState::Undefined);
        assert_eq!(doc.custom_element_state(p), CustomElementState::Undefined, "an is value");
        assert_eq!(doc.custom_element_state(svg_child), CustomElementState::Uncustomized, "not an HTML element");
        assert_eq!((doc.is_value(p), doc.is_value(div)), (Some("fancy-p"), None));
        // A clone has its original's `is` value, not what its attribute says.
        if let Some(e) = doc.get_mut(div).as_element_mut() {
            e.set_attr("is", "late-div");
        }
        let (div2, p2) = (doc.clone_subtree(div, false), doc.clone_subtree(p, false));
        assert_eq!((doc.is_value(div2), doc.is_value(p2)), (None, Some("fancy-p")));
        // A state and a definition that were set are kept, and go with the node.
        doc.set_custom_element_state(mine, CustomElementState::Custom);
        doc.set_custom_element_definition(mine, Some(3));
        assert_eq!((doc.custom_element_state(mine), doc.custom_element_definition(mine)), (CustomElementState::Custom, Some(3)));
        let mine2 = doc.clone_subtree(mine, false);
        assert_eq!((doc.custom_element_state(mine2), doc.custom_element_definition(mine2)), (CustomElementState::Undefined, None));
        doc.remove_subtree(mine);
        assert!(doc.custom.is_empty() || !doc.custom.contains_key(&mine));
    }
}
