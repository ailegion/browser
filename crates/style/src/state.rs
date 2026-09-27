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
    /// The focused element when focus came from the keyboard
    /// (`:focus-visible`); a mouse click focuses without it.
    pub const FOCUS_VISIBLE: u8 = 32;

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

    /// Whether any element has `flag`.
    pub fn has_any(&self, flag: u8) -> bool {
        self.flags.values().any(|f| f & flag != 0)
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
    /// `Parent`, plus the ancestors of the changed elements and the earlier
    /// siblings of those (`li:has(a:hover)`, `h2:has(+ p:hover)`): the
    /// only places the subject of a `:has()` in the subject compound can
    /// be.
    Ancestors,
    /// `Ancestors`, plus the subtrees of those ancestors and earlier
    /// siblings that carry a `:has()` left of a combinator
    /// (`.menu:has(:hover) .item`): the subject is somewhere below the
    /// `:has()` element.
    Document,
}

/// The interaction states a selector can depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKind {
    Hover,
    Active,
    Focus,
    Target,
}

/// Where the elements a state change can affect are, relative to the
/// element whose state changed (the one the rule's trigger matches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Descendants matching the key (`li:hover > ul`).
    Subtree(SubjectKey),
    /// Later siblings and their descendants matching the key
    /// (`a:hover + .tip`, `a:hover ~ p b`): the parent's subtree.
    Siblings(SubjectKey),
    /// The element carrying `:has()` with the state inside, which is an
    /// ancestor of the changed element or an earlier sibling of one
    /// (`li:has(a:hover)`, `h2:has(+ p:hover)`).
    Above(SubjectKey),
    /// A `:has()` element as above, but left of a combinator
    /// (`.menu:has(:hover) .item`): the subjects are in its subtree.
    AboveSubtree { has: SubjectKey, subject: SubjectKey },
}

/// One way a state change can alter the style of an element other than
/// the changed one: when `trigger` matches the changed element, the
/// elements in `scope` may match the selector differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRule {
    pub state: StateKind,
    /// Key of the compound that carries the state pseudo-class.
    pub trigger: SubjectKey,
    pub scope: Scope,
}

/// The reach of each state under the current stylesheets (a summary for
/// logging and tests), and the rules that say exactly what a change can
/// affect. A changed element is always recomputed itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InteractionDeps {
    pub hover: Reach,
    pub active: Reach,
    pub focus: Reach,
    pub target: Reach,
    pub rules: Vec<StateRule>,
}

impl InteractionDeps {
    pub fn reach(&self, state: StateKind) -> Reach {
        match state {
            StateKind::Hover => self.hover,
            StateKind::Active => self.active,
            StateKind::Focus => self.focus,
            StateKind::Target => self.target,
        }
    }

    pub fn reach_mut(&mut self, state: StateKind) -> &mut Reach {
        match state {
            StateKind::Hover => &mut self.hover,
            StateKind::Active => &mut self.active,
            StateKind::Focus => &mut self.focus,
            StateKind::Target => &mut self.target,
        }
    }

    pub fn rules_for(&self, state: StateKind) -> impl Iterator<Item = &StateRule> {
        self.rules.iter().filter(move |r| r.state == state)
    }
}

/// The key of one compound selector: its id if it has one, else its
/// first class, else its tag, else universal. An element matching the
/// compound matches its key, so the key is a cheap superset test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectKey {
    Id(String),
    Class(String),
    Tag(LocalName),
    Universal,
}

impl SubjectKey {
    pub fn matches(&self, element: &browser_dom::Element) -> bool {
        match self {
            SubjectKey::Id(id) => element.id() == Some(id.as_str()),
            SubjectKey::Class(class) => element.classes().any(|c| c == class),
            SubjectKey::Tag(tag) => element.name.local == *tag,
            SubjectKey::Universal => true,
        }
    }
}

impl SubjectKeys {
    pub fn insert(&mut self, key: SubjectKey) {
        match key {
            SubjectKey::Id(id) => {
                self.ids.insert(id);
            }
            SubjectKey::Class(class) => {
                self.classes.insert(class);
            }
            SubjectKey::Tag(tag) => {
                self.tags.insert(tag);
            }
            SubjectKey::Universal => self.universal = true,
        }
    }
}

/// A set of subject keys: the elements to recompute in one subtree walk.
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
