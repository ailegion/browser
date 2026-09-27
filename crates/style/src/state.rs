//! Interaction state per element (`:hover`, `:active`, `:focus`,
//! `:focus-within`), and how far a change in it can reach through the
//! stylesheets, so the tab restyles only what the change can affect.

use std::collections::HashSet;
use std::sync::LazyLock;

use browser_dom::{Document, NodeId};
use html5ever::LocalName;
use slotmap::SecondaryMap;

/// Which interaction pseudo-classes currently apply to which elements.
/// Hover, active and focus-within are chains from a target up to the root;
/// focus is a single element.
#[derive(Debug, Default, Clone)]
pub struct ElementStates {
    flags: SecondaryMap<NodeId, u8>,
}

/// No element in any state; what a plain cascade uses.
pub static NO_STATES: LazyLock<ElementStates> = LazyLock::new(ElementStates::default);

impl ElementStates {
    pub const HOVER: u8 = 1;
    pub const ACTIVE: u8 = 2;
    pub const FOCUS: u8 = 4;
    pub const FOCUS_WITHIN: u8 = 8;
    /// The element the URL fragment names (`:target`).
    pub const TARGET: u8 = 16;

    pub fn has(&self, id: NodeId, flag: u8) -> bool {
        self.flags.get(id).is_some_and(|f| f & flag != 0)
    }

    /// Set `flag` on exactly the elements from `target` up to the root and
    /// clear it everywhere else. Returns the elements whose flag changed.
    pub fn set_chain(&mut self, doc: &Document, target: Option<NodeId>, flag: u8) -> Vec<NodeId> {
        let mut keep = Vec::new();
        let mut cur = target;
        while let Some(n) = cur {
            if doc.get(n).is_element() {
                keep.push(n);
            }
            cur = doc.parent(n);
        }
        self.set_exactly(&keep, flag)
    }

    /// Set `flag` on exactly `target` (or nothing). Returns the elements
    /// whose flag changed.
    pub fn set_single(&mut self, target: Option<NodeId>, flag: u8) -> Vec<NodeId> {
        self.set_exactly(target.as_slice(), flag)
    }

    fn set_exactly(&mut self, keep: &[NodeId], flag: u8) -> Vec<NodeId> {
        let mut changed = Vec::new();
        let currently: Vec<NodeId> = self
            .flags
            .iter()
            .filter(|(_, f)| **f & flag != 0)
            .map(|(k, _)| k)
            .collect();
        for n in currently {
            if !keep.contains(&n) {
                let f = self.flags[n] & !flag;
                if f == 0 {
                    self.flags.remove(n);
                } else {
                    self.flags[n] = f;
                }
                changed.push(n);
            }
        }
        for &n in keep {
            let cur = self.flags.get(n).copied().unwrap_or(0);
            if cur & flag == 0 {
                self.flags.insert(n, cur | flag);
                changed.push(n);
            }
        }
        changed
    }

    pub fn is_empty(&self) -> bool {
        self.flags.is_empty()
    }
}

/// How far a change in one interaction state can reach through the
/// selectors in effect, from cheapest to dearest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Reach {
    /// No selector mentions the state.
    #[default]
    None,
    /// Only elements whose own state changed (`a:hover`).
    Element,
    /// Their descendants too (`li:hover > ul`).
    Subtree,
    /// Their later siblings too (`a:hover + .tip`): restyle from the parent.
    Parent,
    /// Ancestors too (`:has(:hover)`): restyle everything.
    Document,
}

/// The reach of each state under the current stylesheets, and which
/// elements beyond the changed ones a state change can restyle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InteractionDeps {
    pub hover: Reach,
    pub active: Reach,
    pub focus: Reach,
    pub target: Reach,
    /// Subjects of the selectors that put an interaction pseudo-class to
    /// the left of a combinator or inside `:has()`: the only elements,
    /// other than the ones whose state changed, whose matching can change.
    pub subjects: SubjectKeys,
}

/// Keys of selector subjects (the rightmost compound), one per selector:
/// its id if it has one, else its first class, else its tag, else it is
/// universal. An element matching a subject matches at least its key, so
/// testing the key is a cheap superset test.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubjectKeys {
    /// Some subject has no id, class or tag (`.menu:hover *`), so every
    /// element is a candidate.
    pub universal: bool,
    pub ids: HashSet<String>,
    pub classes: HashSet<String>,
    pub tags: HashSet<LocalName>,
}

impl SubjectKeys {
    /// Whether `element` could be the subject of one of the selectors.
    pub fn matches(&self, element: &browser_dom::Element) -> bool {
        self.universal
            || self.tags.contains(&element.name.local)
            || element.id().is_some_and(|id| self.ids.contains(id))
            || element.classes().any(|c| self.classes.contains(c))
    }

    pub fn is_empty(&self) -> bool {
        !self.universal && self.ids.is_empty() && self.classes.is_empty() && self.tags.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_dom::parse_html;

    fn nth(doc: &Document, tag: &str, n: usize) -> NodeId {
        doc.descendants(doc.root())
            .filter(|&id| doc.element(id).is_some_and(|e| &*e.name.local == tag))
            .nth(n)
            .expect("element")
    }

    #[test]
    fn chains_diff_and_single_flags_coexist() {
        let doc = parse_html(b"<div><p><a>x</a></p><p><b>y</b></p></div>");
        let (div, a, b) = (nth(&doc, "div", 0), nth(&doc, "a", 0), nth(&doc, "b", 0));
        let (p1, p2) = (nth(&doc, "p", 0), nth(&doc, "p", 1));
        let mut s = ElementStates::default();

        let changed = s.set_chain(&doc, Some(a), ElementStates::HOVER);
        assert!(changed.contains(&a) && changed.contains(&p1) && changed.contains(&div));
        assert!(s.has(div, ElementStates::HOVER) && !s.has(p2, ElementStates::HOVER));

        // Moving to a sibling subtree changes only the differing tails.
        let changed = s.set_chain(&doc, Some(b), ElementStates::HOVER);
        assert_eq!(changed.len(), 4, "{changed:?}");
        assert!(!changed.contains(&div));
        assert!(!s.has(a, ElementStates::HOVER) && s.has(b, ElementStates::HOVER));

        s.set_single(Some(b), ElementStates::FOCUS);
        assert!(s.has(b, ElementStates::FOCUS) && s.has(b, ElementStates::HOVER));
        // Clearing drops the whole chain: b, p, div, body, html.
        let changed = s.set_chain(&doc, None, ElementStates::HOVER);
        assert_eq!(changed.len(), 5);
        assert!(s.has(b, ElementStates::FOCUS) && !s.has(b, ElementStates::HOVER));
        assert_eq!(s.set_single(Some(b), ElementStates::FOCUS).len(), 0);
        s.set_single(None, ElementStates::FOCUS);
        assert!(s.is_empty());
    }
}
