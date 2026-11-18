//! `<script>` elements, the part the tree keeps (HTML §4.12.1): which
//! have "already started", which the parser made, which a script told to
//! run in order (`async = false`), and which may need preparing because
//! they became connected or their children changed.

use html5ever::{local_name, ns};

use crate::{Document, Element, NodeId, NodeKind};

/// What a `<script>` element's `type` (and legacy `language`) makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptType {
    Classic,
    Module,
}

/// The essences HTML calls JavaScript MIME types (MIME Sniffing §4.6).
const JAVASCRIPT_TYPES: [&str; 16] = [
    "application/ecmascript",
    "application/javascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
];

/// "Prepare the script element", steps 8 to 11: the script's type from
/// its `type` attribute, else its `language`; `None` for a data block.
pub fn script_type(e: &Element) -> Option<ScriptType> {
    let is_javascript = |essence: &str| JAVASCRIPT_TYPES.iter().any(|t| t.eq_ignore_ascii_case(essence));
    let block_type = match (e.attr_ns("", "type"), e.attr_ns("", "language")) {
        (Some(""), _) | (None, Some("")) | (None, None) => "text/javascript".to_owned(),
        (Some(ty), _) => ty.trim_matches(['\t', '\n', '\x0C', '\r', ' ']).to_owned(),
        (None, Some(language)) => format!("text/{language}"),
    };
    if is_javascript(&block_type) {
        Some(ScriptType::Classic)
    } else if block_type.eq_ignore_ascii_case("module") {
        Some(ScriptType::Module)
    } else {
        None
    }
}

fn is_script(e: &Element) -> bool {
    e.name.ns == ns!(html) && e.name.local == local_name!("script")
}

/// The script state beside the tree.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scripts {
    /// A `<script>` element was ever created: until then the hooks of the
    /// tree primitives do nothing.
    seen: bool,
    started: std::collections::HashSet<NodeId>,
    /// Made by the HTML parser (the document's or a fragment's): the
    /// parser's own preparation is theirs.
    parser: std::collections::HashSet<NodeId>,
    /// `async = false` was set by a script: an external script runs in
    /// the order it was inserted.
    ordered: std::collections::HashSet<NodeId>,
    /// Elements that became connected or whose children changed.
    candidates: Vec<NodeId>,
}

impl Document {
    pub(crate) fn note_created_script(&mut self, kind: &NodeKind) {
        if matches!(kind, NodeKind::Element(e) if is_script(e)) {
            self.scripts.seen = true;
        }
    }

    pub fn is_script_element(&self, id: NodeId) -> bool {
        self.element(id).is_some_and(is_script)
    }

    /// The element's "already started" flag.
    pub fn script_started(&self, id: NodeId) -> bool {
        self.scripts.started.contains(&id)
    }

    pub fn mark_script_started(&mut self, id: NodeId) {
        self.scripts.started.insert(id);
    }

    /// Whether the HTML parser made the element.
    pub fn is_parser_script(&self, id: NodeId) -> bool {
        self.scripts.parser.contains(&id)
    }

    pub fn mark_parser_script(&mut self, id: NodeId) {
        self.scripts.parser.insert(id);
    }

    /// The element's "force async" flag: set for an element a script
    /// made, until `async` is assigned.
    pub fn script_force_async(&self, id: NodeId) -> bool {
        !self.scripts.parser.contains(&id) && !self.scripts.ordered.contains(&id)
    }

    pub fn clear_script_force_async(&mut self, id: NodeId) {
        self.scripts.ordered.insert(id);
    }

    /// A `<script>` that may need preparing (its `src` was set).
    pub fn note_script_candidate(&mut self, id: NodeId) {
        if self.is_script_element(id) && !self.scripts.candidates.contains(&id) {
            self.scripts.candidates.push(id);
        }
    }

    /// The `<script>` elements that became connected or whose children
    /// changed since the last call, in order.
    pub fn take_script_candidates(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.scripts.candidates)
    }

    pub fn has_script_candidates(&self) -> bool {
        !self.scripts.candidates.is_empty()
    }

    /// `child` was placed under `parent`: a `<script>` that became
    /// connected, or whose children changed, is a candidate.
    pub(crate) fn scripts_inserted(&mut self, child: NodeId, parent: NodeId) {
        if !self.scripts.seen {
            return;
        }
        if self.is_script_element(parent) {
            self.note_script_candidate(parent);
        }
        if !matches!(self.get(child).kind, NodeKind::Element(_) | NodeKind::DocumentFragment) || !self.is_connected(parent) {
            return;
        }
        let found: Vec<NodeId> = std::iter::once(child).chain(self.descendants(child)).filter(|&n| self.is_script_element(n)).collect();
        for script in found {
            self.note_script_candidate(script);
        }
    }

    /// A clone has its original's "already started" flag (HTML's cloning
    /// steps for `script`).
    pub(crate) fn clone_script_state(&mut self, from: NodeId, to: NodeId) {
        if self.scripts.started.contains(&from) {
            self.scripts.started.insert(to);
        }
        if self.scripts.ordered.contains(&from) {
            self.scripts.ordered.insert(to);
        }
    }

    pub(crate) fn forget_script(&mut self, id: NodeId) {
        if self.scripts.seen {
            self.scripts.started.remove(&id);
            self.scripts.parser.remove(&id);
            self.scripts.ordered.remove(&id);
        }
    }
}
