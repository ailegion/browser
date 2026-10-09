//! The user's events (Phase 3 item 3.3, block 2): what the tab fires for
//! the pointer, the mouse, the keyboard, focus, editing, the wheel,
//! scrolling and resizing, with the default action held back when a
//! listener cancels the event.
//!
//! Every event is a task through `with_script` on the document the host
//! serves. Nothing fires while a new document is still parsing: the page
//! on screen is then the old one, whose context is gone. A dispatch that
//! cannot reach a listener is skipped (`wants_event`).
//!
//! Pointer moves are coalesced per event batch, as browsers coalesce
//! them per frame: `flush` finds the hover change once at the batch's
//! final position (O15) and fires one `pointermove` and one `mousemove`
//! there. Pointer events come before their compatibility mouse events;
//! a cancelled `pointerdown` holds back `mousedown`, `mousemove` and
//! `mouseup` until the pointer goes up, as Pointer Events say.

use browser_dom::{Document, NodeId};
use browser_ipc_types::{Key, MouseButton};
use browser_script::{EventTargetRef, MOUSE_POINTER_ID, UiClass, UiEventInit, forwarded_to_window};

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
pub(super) fn button_number(button: MouseButton) -> i16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Other => 3,
    }
}

/// `PointerEvent.button` for an event that changes no button.
const NO_BUTTON: i16 = -1;

/// `KeyboardEvent.key` and `code` of a key. `code` is the shell's
/// (winit's physical key, named as the DOM names it); when the shell
/// has none, it is made from the character for letters and digits and
/// is the key's name otherwise.
fn key_names(key: &Key, code: &str) -> (String, String) {
    let name = match key {
        Key::Character(s) => s.clone(),
        Key::Space => " ".to_owned(),
        // The named keys' names are the DOM's.
        named => format!("{named:?}"),
    };
    if !code.is_empty() {
        return (name, code.to_owned());
    }
    let code = match key {
        Key::Character(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => format!("Key{}", c.to_ascii_uppercase()),
                (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}"),
                _ => String::new(),
            }
        }
        Key::Space => "Space".to_owned(),
        _ => name.clone(),
    };
    (name, code)
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
        let name = format!("on{kind}");
        let has_attr = |n: NodeId| doc.element(n).is_some_and(|e| e.attr(&name).is_some());
        // `<body onload>`, `<body onscroll>` and the rest are `window`'s
        // handlers, which any dispatch reaching `window` meets.
        if forwarded_to_window(kind) && doc.body().is_some_and(has_attr) {
            return true;
        }
        let EventTargetRef::Node(id) = target else { return false };
        if !doc.contains(id) {
            return false;
        }
        std::iter::once(id).chain(doc.ancestors(id)).any(has_attr)
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

    /// The viewport origin of the first box of `target`, for `offsetX/Y`.
    fn box_origin(&self, target: EventTargetRef) -> (f32, f32) {
        match (target, &self.layout, &self.doc) {
            (EventTargetRef::Node(t), Some(tree), Some(doc)) => tree
                .first_rect(|n| n == t || doc.parent(n) == Some(t))
                .map(|r| (r.x, r.y)),
            _ => None,
        }
        .unwrap_or((0.0, 0.0))
    }

    /// A mouse or pointer event's fields at viewport point `(x, y)`:
    /// page coordinates add the scroll offset, offset coordinates are
    /// from the target's first box. `related` is dropped if it left the
    /// document.
    fn mouse_init(
        &self,
        class: UiClass,
        target: EventTargetRef,
        x: f32,
        y: f32,
        button: i16,
        detail: u32,
        related: Option<NodeId>,
    ) -> UiEventInit {
        let (page_x, page_y) = (x + self.scroll_x, y + self.scroll_y);
        let origin = self.box_origin(target);
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
            button,
            buttons: self.buttons,
            related_target: related.filter(|&r| self.live(r)).map(EventTargetRef::Node),
            alt: m.alt,
            ctrl: m.ctrl,
            shift: m.shift,
            meta: m.meta,
            pointer_id: MOUSE_POINTER_ID,
            pointer_type: "mouse".to_owned(),
            pressure: if self.buttons != 0 { 0.5 } else { 0.0 },
            is_primary: true,
            ..UiEventInit::default()
        }
    }

    /// A `MouseEvent` (`class` `Mouse`) or `PointerEvent` (`Pointer`) of
    /// `kind` at the element under the pointer, at viewport `(x, y)`.
    #[allow(clippy::too_many_arguments)]
    fn fire_at(
        &mut self,
        class: UiClass,
        kind: &str,
        node: Option<NodeId>,
        x: f32,
        y: f32,
        button: i16,
        detail: u32,
        related: Option<NodeId>,
        bubbles: bool,
        cancelable: bool,
        movement: (f32, f32),
    ) -> bool {
        let target = self.mouse_target(node);
        if !self.wants_event(target, kind) {
            return true;
        }
        let mut init = self.mouse_init(class, target, x, y, button, detail, related);
        init.bubbles = bubbles;
        init.cancelable = cancelable;
        init.movement_x = f64::from(movement.0);
        init.movement_y = f64::from(movement.1);
        self.fire_user_event(target, kind, init)
    }

    /// `mousedown`, `mouseup`, `dblclick`: a `MouseEvent`, bubbling and
    /// cancelable; `detail` is the click count.
    pub(super) fn fire_mouse(
        &mut self,
        kind: &str,
        node: Option<NodeId>,
        x: f32,
        y: f32,
        button: MouseButton,
        detail: u32,
    ) -> bool {
        self.fire_at(UiClass::Mouse, kind, node, x, y, button_number(button), detail, None, true, true, (0.0, 0.0))
    }

    /// `pointerdown` and `pointerup`: bubbling, cancelable, `detail` 0.
    pub(super) fn fire_pointer(&mut self, kind: &str, node: Option<NodeId>, x: f32, y: f32, button: MouseButton) -> bool {
        self.fire_at(UiClass::Pointer, kind, node, x, y, button_number(button), 0, None, true, true, (0.0, 0.0))
    }

    /// `click`, `auxclick` and `contextmenu` from the mouse are
    /// `PointerEvent`s (UI Events); `detail` is the click count.
    pub(super) fn fire_click(
        &mut self,
        kind: &str,
        node: Option<NodeId>,
        x: f32,
        y: f32,
        button: MouseButton,
        detail: u32,
    ) -> bool {
        self.fire_at(UiClass::Pointer, kind, node, x, y, button_number(button), detail, None, true, true, (0.0, 0.0))
    }

    /// The `click` the keyboard makes on a link, button or checkbox
    /// (Enter, or Space on release): a `PointerEvent` with `pointerId`
    /// -1, an empty `pointerType` and no coordinates, per HTML's
    /// "fire a synthetic pointer event".
    pub(super) fn fire_synthetic_click(&mut self, node: NodeId) -> bool {
        let target = EventTargetRef::Node(node);
        if !self.wants_event(target, "click") {
            return true;
        }
        let m = self.modifiers;
        let init = UiEventInit {
            class: UiClass::Pointer,
            bubbles: true,
            cancelable: true,
            pointer_id: -1,
            is_primary: true,
            alt: m.alt,
            ctrl: m.ctrl,
            shift: m.shift,
            meta: m.meta,
            ..UiEventInit::default()
        };
        self.fire_user_event(target, "click", init)
    }

    /// `contextmenu` from the context-menu key (or Shift+F10): at the
    /// focused element, else `body`, with the element's box as the
    /// position; `pointerId` -1 as for any keyboard-made pointer event.
    pub(super) fn fire_keyboard_contextmenu(&mut self) -> bool {
        let target = self.key_target();
        if !self.wants_event(target, "contextmenu") {
            return true;
        }
        let (ox, oy) = self.box_origin(target);
        let (x, y) = (ox - self.scroll_x, oy - self.scroll_y);
        let mut init = self.mouse_init(UiClass::Pointer, target, x, y, NO_BUTTON, 0, None);
        init.pointer_id = -1;
        init.pointer_type = String::new();
        init.pressure = 0.0;
        self.fire_user_event(target, "contextmenu", init)
    }

    /// Make a capture a script asked for (or released) active before the
    /// next pointer event: `lostpointercapture` on the old element,
    /// `gotpointercapture` on the new, and the boundary events move to
    /// the capturing element (and back to what is under the pointer on
    /// release), since hover follows the capture (`update_hover`).
    pub(super) fn process_pointer_capture(&mut self) {
        let Some(host) = &mut self.script else { return };
        let mut pending = host.pointer_capture();
        if pending.is_some_and(|p| !self.live(p)) {
            pending = None;
            if let Some(host) = &mut self.script {
                host.clear_pointer_capture();
            }
        }
        if pending == self.pointer_capture {
            return;
        }
        let (x, y) = self.mouse.unwrap_or((0.0, 0.0));
        if let Some(old) = self.pointer_capture.take() {
            self.fire_at(UiClass::Pointer, "lostpointercapture", Some(old), x, y, NO_BUTTON, 0, None, true, false, (0.0, 0.0));
        }
        if let Some(new) = pending {
            self.pointer_capture = Some(new);
            self.fire_at(UiClass::Pointer, "gotpointercapture", Some(new), x, y, NO_BUTTON, 0, None, true, false, (0.0, 0.0));
        }
        self.hover_dirty = true;
        self.update_hover();
    }

    /// The pointer went up: a capture is released (`lostpointercapture`).
    pub(super) fn release_pointer_capture(&mut self) {
        if let Some(host) = &mut self.script {
            host.clear_pointer_capture();
        }
        self.process_pointer_capture();
    }

    /// Once per batch in which the pointer moved, at its final position:
    /// a pending capture is made active, then `pointermove` and
    /// `mousemove` at the hovered element (which `flush` has just found).
    /// `movementX/Y` are from the last move fired. True when something
    /// fired.
    pub(super) fn fire_pointer_move(&mut self) -> bool {
        if !std::mem::take(&mut self.pointer_moved) {
            return false;
        }
        let Some((x, y)) = self.mouse else { return false };
        if self.event_doc().is_none() || self.over_scrollbar() {
            return false;
        }
        self.process_pointer_capture();
        let node = self.hover;
        let movement = self.last_move.map_or((0.0, 0.0), |(lx, ly)| (x - lx, y - ly));
        self.last_move = Some((x, y));
        let target = self.mouse_target(node);
        let mut fired = false;
        if self.wants_event(target, "pointermove") {
            self.fire_at(UiClass::Pointer, "pointermove", node, x, y, NO_BUTTON, 0, None, true, true, movement);
            fired = true;
        }
        if !self.compat_suppressed && self.wants_event(target, "mousemove") {
            self.fire_at(UiClass::Mouse, "mousemove", node, x, y, 0, 0, None, true, true, movement);
            fired = true;
        }
        fired
    }

    /// The pointer went from `old` to `new`: `pointerout`, `pointerleave`
    /// (per element left, innermost first), `mouseout`, `mouseleave`,
    /// then `pointerover`, `pointerenter` (per element entered, outermost
    /// first), `mouseover`, `mouseenter`. The enter and leave events do
    /// not bubble.
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
        let old = old.filter(|_| !old_chain.is_empty());
        let new = new.filter(|_| !new_chain.is_empty());
        let fire = |this: &mut Self, class: UiClass, kind: &str, node: NodeId, bubbles: bool, related: Option<NodeId>| {
            let button = if class == UiClass::Pointer { NO_BUTTON } else { 0 };
            this.fire_at(class, kind, Some(node), x, y, button, 0, related, bubbles, bubbles, (0.0, 0.0));
        };
        if let Some(o) = old {
            fire(self, UiClass::Pointer, "pointerout", o, true, new);
            for &n in &left {
                fire(self, UiClass::Pointer, "pointerleave", n, false, new);
            }
            fire(self, UiClass::Mouse, "mouseout", o, true, new);
            for &n in &left {
                fire(self, UiClass::Mouse, "mouseleave", n, false, new);
            }
        }
        if let Some(n) = new {
            fire(self, UiClass::Pointer, "pointerover", n, true, old);
            for &e in &entered {
                fire(self, UiClass::Pointer, "pointerenter", e, false, old);
            }
            fire(self, UiClass::Mouse, "mouseover", n, true, old);
            for &e in &entered {
                fire(self, UiClass::Mouse, "mouseenter", e, false, old);
            }
        }
    }

    /// `wheel` at the hovered element, before the scroll it asks for;
    /// cancelled, the page does not scroll.
    pub(super) fn fire_wheel(&mut self, dx: f32, dy: f32) -> bool {
        let Some((x, y)) = self.mouse else { return true };
        if self.event_doc().is_none() {
            return true;
        }
        let target = self.mouse_target(self.hover);
        if !self.wants_event(target, "wheel") {
            return true;
        }
        let mut init = self.mouse_init(UiClass::Wheel, target, x, y, 0, 0, None);
        init.delta_x = f64::from(dx);
        init.delta_y = f64::from(dy);
        self.fire_user_event(target, "wheel", init)
    }

    /// `change` at a text control whose value differs from when it got
    /// focus (or from the last `change`); the baseline is then the
    /// current value.
    pub(super) fn fire_change_if_edited(&mut self, control: NodeId) {
        let current = self.control_value(control);
        let edited = self.focus_value.as_ref().is_some_and(|v| *v != current);
        self.focus_value = Some(current);
        if edited && self.live(control) {
            self.fire_user_event(
                EventTargetRef::Node(control),
                "change",
                UiEventInit {
                    bubbles: true,
                    ..UiEventInit::default()
                },
            );
        }
    }

    /// Focus is leaving `old` for `new`: `change` for a text control the
    /// user edited, then `blur` and `focusout`. Nothing fires for an
    /// element no longer in the document (browsers fire nothing on
    /// removal either).
    pub(super) fn fire_blur(&mut self, old: NodeId, new: Option<NodeId>) {
        if !self.live(old) {
            self.focus_value = None;
            return;
        }
        if self.focus_value.is_some() {
            self.fire_change_if_edited(old);
        }
        self.focus_value = None;
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
    /// of a text control is remembered for `change`.
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
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fire_key(
        &mut self,
        kind: &str,
        key: &Key,
        code: &str,
        repeat: bool,
        shift: bool,
        ctrl: bool,
        alt: bool,
    ) -> bool {
        let target = self.key_target();
        if !self.wants_event(target, kind) {
            return true;
        }
        let (key, code) = key_names(key, code);
        let init = UiEventInit {
            class: UiClass::Keyboard,
            bubbles: true,
            cancelable: true,
            key,
            code,
            repeat,
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
