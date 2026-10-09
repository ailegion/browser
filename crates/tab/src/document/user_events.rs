//! The user's events (Phase 3 item 3.3, block 2): what the tab fires for
//! the mouse, the keyboard, focus, editing, the wheel, scrolling and
//! resizing, with the default action held back when a listener cancels
//! the event.
//!
//! Every event is a task through `with_script` on the document the host
//! serves. Nothing fires while a new document is still parsing: the page
//! on screen is then the old one, whose context is gone. A dispatch that
//! cannot reach a listener is skipped (`wants_event`), so a mouse move on
//! a page without mouse listeners costs a hit test and nothing more.

use browser_dom::{Document, NodeId};
use browser_ipc_types::{Key, MouseButton};
use browser_script::{EventTargetRef, UiClass, UiEventInit};

use super::{TabState, is_text_control};

/// The modifier keys held, as the shell last reported them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// The bit of a button in `MouseEvent.buttons`.
pub(super) fn button_bit(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
        MouseButton::Other => 8,
    }
}

/// `MouseEvent.button`.
fn button_number(button: MouseButton) -> i16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Other => 3,
    }
}

/// `KeyboardEvent.key` and `code` of a key. The shell reports no
/// physical key, so `code` is made from the character for letters and
/// digits and is the key's name otherwise.
fn key_names(key: &Key) -> (String, String) {
    match key {
        Key::Character(s) => {
            let mut chars = s.chars();
            let code = match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => format!("Key{}", c.to_ascii_uppercase()),
                (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}"),
                _ => String::new(),
            };
            (s.clone(), code)
        }
        Key::Space => (" ".to_owned(), "Space".to_owned()),
        // The named keys' names are the DOM's.
        named => {
            let name = format!("{named:?}");
            (name.clone(), name)
        }
    }
}

impl TabState {
    /// The document on screen, when the host serves it (see the module
    /// doc).
    fn event_doc(&self) -> Option<&Document> {
        if self.parser.is_some() || self.script.is_none() {
            return None;
        }
        self.doc.as_ref()
    }

    /// Whether `id` is a node of the document on screen, connected.
    pub(super) fn live(&self, id: NodeId) -> bool {
        self.event_doc().is_some_and(|d| d.contains(id) && d.is_connected(id))
    }

    /// Whether a dispatch of `kind` at `target` could reach a listener:
    /// one registered anywhere, or an `on<kind>` attribute on the target
    /// or an ancestor (those compile on their first dispatch).
    pub(super) fn wants_event(&self, target: EventTargetRef, kind: &str) -> bool {
        let (Some(host), Some(doc)) = (&self.script, self.event_doc()) else {
            return false;
        };
        if host.has_listeners(kind) {
            return true;
        }
        let EventTargetRef::Node(id) = target else { return false };
        if !doc.contains(id) {
            return false;
        }
        let name = format!("on{kind}");
        std::iter::once(id)
            .chain(doc.ancestors(id))
            .any(|n| doc.element(n).is_some_and(|e| e.attr(&name).is_some()))
    }

    /// Fire a trusted event of the user's as a task. True when the
    /// default action may proceed: nobody listened, or no listener
    /// cancelled it.
    pub(super) fn fire_user_event(&mut self, target: EventTargetRef, kind: &str, init: UiEventInit) -> bool {
        if !self.wants_event(target, kind) {
            return true;
        }
        self.sync_document_info();
        let mut proceed = true;
        self.with_script(|host| {
            proceed = host.fire_ui_event(target, kind, init);
        });
        self.after_script();
        proceed
    }

    /// The target of a mouse event: the element under the pointer, else
    /// the root element, else the document.
    fn mouse_target(&self, node: Option<NodeId>) -> EventTargetRef {
        node.or_else(|| self.doc.as_ref()?.document_element())
            .map_or(EventTargetRef::Document, EventTargetRef::Node)
    }

    /// The target of a key event: the focused element, else `body`, else
    /// the document.
    fn key_target(&self) -> EventTargetRef {
        self.focus
            .or_else(|| self.doc.as_ref()?.body())
            .map_or(EventTargetRef::Document, EventTargetRef::Node)
    }

    /// A mouse event's fields at viewport point `(x, y)`: page
    /// coordinates add the scroll offset, offset coordinates are from
    /// the target's first box. `related` is dropped if it left the
    /// document.
    fn mouse_init(
        &self,
        class: UiClass,
        target: EventTargetRef,
        x: f32,
        y: f32,
        button: Option<MouseButton>,
        detail: u32,
        related: Option<NodeId>,
    ) -> UiEventInit {
        let (page_x, page_y) = (x + self.scroll_x, y + self.scroll_y);
        let origin = match (target, &self.layout, &self.doc) {
            (EventTargetRef::Node(t), Some(tree), Some(doc)) => tree
                .first_rect(|n| n == t || doc.parent(n) == Some(t))
                .map(|r| (r.x, r.y)),
            _ => None,
        }
        .unwrap_or((0.0, 0.0));
        let m = self.modifiers;
        UiEventInit {
            class,
            bubbles: true,
            cancelable: true,
            detail: detail as i32,
            screen_x: f64::from(x),
            screen_y: f64::from(y),
            client_x: f64::from(x),
            client_y: f64::from(y),
            page_x: f64::from(page_x),
            page_y: f64::from(page_y),
            offset_x: f64::from(page_x - origin.0),
            offset_y: f64::from(page_y - origin.1),
            button: button.map_or(0, button_number),
            buttons: self.buttons,
            related_target: related.filter(|&r| self.live(r)).map(EventTargetRef::Node),
            alt: m.alt,
            ctrl: m.ctrl,
            shift: m.shift,
            meta: m.meta,
            ..UiEventInit::default()
        }
    }

    /// A `MouseEvent` of `kind` at the element under the pointer
    /// (bubbling, cancelable); `detail` is the click count.
    pub(super) fn fire_mouse(
        &mut self,
        kind: &str,
        node: Option<NodeId>,
        x: f32,
        y: f32,
        button: Option<MouseButton>,
        detail: u32,
    ) -> bool {
        let target = self.mouse_target(node);
        if !self.wants_event(target, kind) {
            return true;
        }
        let init = self.mouse_init(UiClass::Mouse, target, x, y, button, detail, None);
        self.fire_user_event(target, kind, init)
    }

    /// `mousemove` at the element under the pointer. The hover change
    /// (`mouseover` and the rest) is normally found once per batch; with
    /// a listener for any of those it is found now, so they come before
    /// `mousemove` as in browsers.
    pub(super) fn mouse_moved(&mut self, x: f32, y: f32) {
        if self.event_doc().is_none() || self.over_scrollbar() {
            return;
        }
        let node = self.hit_node(x, y).and_then(|n| self.element_of(n));
        let target = self.mouse_target(node);
        if node != self.hover {
            let old = self.mouse_target(self.hover);
            let crossing = ["mouseover", "mouseout", "mouseenter", "mouseleave"]
                .iter()
                .any(|k| self.wants_event(target, k) || self.wants_event(old, k));
            if crossing {
                self.update_hover();
            }
        }
        if !self.wants_event(target, "mousemove") {
            return;
        }
        let init = self.mouse_init(UiClass::Mouse, target, x, y, None, 0, None);
        self.fire_user_event(target, "mousemove", init);
    }

    /// The pointer went from `old` to `new`: `mouseout` and `mouseleave`
    /// on the way out, `mouseover` and `mouseenter` on the way in. The
    /// enter and leave events do not bubble and fire once per element
    /// left or entered; enter goes outermost first.
    pub(super) fn fire_hover_change(&mut self, old: Option<NodeId>, new: Option<NodeId>) {
        let Some(doc) = self.event_doc() else { return };
        let (x, y) = self.mouse.unwrap_or((0.0, 0.0));
        let chain = |n: Option<NodeId>| -> Vec<NodeId> {
            n.filter(|&n| doc.contains(n))
                .map(|n| {
                    std::iter::once(n)
                        .chain(doc.ancestors(n))
                        .filter(|&a| doc.get(a).is_element())
                        .collect()
                })
                .unwrap_or_default()
        };
        let old_chain = chain(old);
        let new_chain = chain(new);
        let left: Vec<NodeId> = old_chain.iter().copied().filter(|n| !new_chain.contains(n)).collect();
        let entered: Vec<NodeId> = new_chain.iter().copied().filter(|n| !old_chain.contains(n)).rev().collect();
        let fire = |this: &mut Self, kind: &str, node: NodeId, bubbles: bool, related: Option<NodeId>| {
            let target = EventTargetRef::Node(node);
            if !this.wants_event(target, kind) {
                return;
            }
            let mut init = this.mouse_init(UiClass::Mouse, target, x, y, None, 0, related);
            init.bubbles = bubbles;
            this.fire_user_event(target, kind, init);
        };
        if let Some(o) = old.filter(|&o| !old_chain.is_empty() && o == old_chain[0]) {
            fire(self, "mouseout", o, true, new);
            for n in left {
                fire(self, "mouseleave", n, false, new);
            }
        }
        if let Some(n) = new.filter(|&n| !new_chain.is_empty() && n == new_chain[0]) {
            fire(self, "mouseover", n, true, old);
            for e in entered {
                fire(self, "mouseenter", e, false, old);
            }
        }
    }

    /// `wheel` at the element under the pointer, before the scroll it
    /// asks for; cancelled, the page does not scroll.
    pub(super) fn fire_wheel(&mut self, dx: f32, dy: f32) -> bool {
        let Some((x, y)) = self.mouse else { return true };
        if self.event_doc().is_none() {
            return true;
        }
        let node = self.hit_node(x, y).and_then(|n| self.element_of(n));
        let target = self.mouse_target(node);
        if !self.wants_event(target, "wheel") {
            return true;
        }
        let mut init = self.mouse_init(UiClass::Wheel, target, x, y, None, 0, None);
        init.delta_x = f64::from(dx);
        init.delta_y = f64::from(dy);
        self.fire_user_event(target, "wheel", init)
    }

    /// Focus is leaving `old` for `new`: `change` for a text control the
    /// user edited, then `blur` and `focusout`. Nothing fires for an
    /// element no longer in the document (browsers fire nothing on
    /// removal either).
    pub(super) fn fire_blur(&mut self, old: NodeId, new: Option<NodeId>) {
        let edited = self.focus_value.take();
        if !self.live(old) {
            return;
        }
        if edited.is_some_and(|v| v != self.control_value(old)) {
            self.fire_user_event(
                EventTargetRef::Node(old),
                "change",
                UiEventInit {
                    bubbles: true,
                    ..UiEventInit::default()
                },
            );
        }
        let init = |bubbles: bool, related: Option<NodeId>| UiEventInit {
            class: UiClass::Focus,
            bubbles,
            related_target: related.map(EventTargetRef::Node),
            ..UiEventInit::default()
        };
        let related = new.filter(|&n| self.live(n));
        self.fire_user_event(EventTargetRef::Node(old), "blur", init(false, related));
        let related = new.filter(|&n| self.live(n));
        self.fire_user_event(EventTargetRef::Node(old), "focusout", init(true, related));
    }

    /// `new` got the focus from `old`: `focus`, then `focusin`. The value
    /// of a text control is remembered for `change` on blur.
    pub(super) fn fire_focus(&mut self, new: NodeId, old: Option<NodeId>) {
        self.focus_value = self
            .doc
            .as_ref()
            .and_then(|d| d.element(new))
            .is_some_and(is_text_control)
            .then(|| self.control_value(new));
        if !self.live(new) {
            return;
        }
        let init = |bubbles: bool, related: Option<NodeId>| UiEventInit {
            class: UiClass::Focus,
            bubbles,
            related_target: related.map(EventTargetRef::Node),
            ..UiEventInit::default()
        };
        let related = old.filter(|&o| self.live(o));
        self.fire_user_event(EventTargetRef::Node(new), "focus", init(false, related));
        let related = old.filter(|&o| self.live(o));
        self.fire_user_event(EventTargetRef::Node(new), "focusin", init(true, related));
    }

    /// A `KeyboardEvent` of `kind` at the focused element (else `body`).
    pub(super) fn fire_key(&mut self, kind: &str, key: &Key, shift: bool, ctrl: bool, alt: bool) -> bool {
        let target = self.key_target();
        if !self.wants_event(target, kind) {
            return true;
        }
        let (key, code) = key_names(key);
        let init = UiEventInit {
            class: UiClass::Keyboard,
            bubbles: true,
            cancelable: true,
            key,
            code,
            alt,
            ctrl,
            shift,
            meta: self.modifiers.meta,
            ..UiEventInit::default()
        };
        self.fire_user_event(target, kind, init)
    }

    /// `beforeinput` (cancelable) before an edit of a text control and
    /// `input` after it, with the edit's `inputType` and `data`.
    pub(super) fn fire_input(&mut self, control: NodeId, kind: &str, input_type: &str, data: Option<&str>) -> bool {
        let init = UiEventInit {
            class: UiClass::Input,
            bubbles: true,
            cancelable: kind == "beforeinput",
            data: data.map(str::to_owned),
            input_type: input_type.to_owned(),
            ..UiEventInit::default()
        };
        self.fire_user_event(EventTargetRef::Node(control), kind, init)
    }

    /// A checkbox, radio or select the user changed: `input`, then
    /// `change`; both bubble, neither can be cancelled.
    pub(super) fn fire_control_changed(&mut self, control: NodeId) {
        self.fire_user_event(
            EventTargetRef::Node(control),
            "input",
            UiEventInit {
                class: UiClass::Input,
                bubbles: true,
                ..UiEventInit::default()
            },
        );
        self.fire_user_event(
            EventTargetRef::Node(control),
            "change",
            UiEventInit {
                bubbles: true,
                ..UiEventInit::default()
            },
        );
    }

    /// `resize` on `window` and `scroll` on the document once the
    /// viewport or the scroll offset moved since last reported, after
    /// layout. True when something fired.
    pub(super) fn fire_view_events(&mut self) -> bool {
        let mut fired = false;
        let vp = (self.viewport.width, self.viewport.height);
        if vp != self.viewport_reported {
            self.viewport_reported = vp;
            if self.wants_event(EventTargetRef::Window, "resize") {
                self.fire_user_event(
                    EventTargetRef::Window,
                    "resize",
                    UiEventInit {
                        class: UiClass::Ui,
                        ..UiEventInit::default()
                    },
                );
                fired = true;
            }
        }
        let scroll = (self.scroll_x, self.scroll_y);
        if scroll != self.scroll_reported {
            self.scroll_reported = scroll;
            if self.wants_event(EventTargetRef::Document, "scroll") {
                self.fire_user_event(
                    EventTargetRef::Document,
                    "scroll",
                    UiEventInit {
                        bubbles: true,
                        ..UiEventInit::default()
                    },
                );
                fired = true;
            }
        }
        fired
    }

    /// The nearest element both `a` and `b` are in, for `click` after a
    /// press and a release on different elements.
    pub(super) fn common_ancestor(&self, a: Option<NodeId>, b: Option<NodeId>) -> Option<NodeId> {
        let doc = self.doc.as_ref()?;
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            (one, None) | (None, one) => return one.filter(|&n| doc.contains(n)),
        };
        if !doc.contains(a) || !doc.contains(b) {
            return None;
        }
        let up: Vec<NodeId> = std::iter::once(b).chain(doc.ancestors(b)).collect();
        std::iter::once(a)
            .chain(doc.ancestors(a))
            .find(|n| up.contains(n))
            .filter(|&n| doc.get(n).is_element())
    }
}
