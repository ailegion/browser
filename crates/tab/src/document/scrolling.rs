//! Scroll containers in the tab (roadmap 4a (4s), block 3): the page and
//! every element with `overflow` `auto`, `scroll` or `hidden` keep a
//! scroll offset here; wheel, keys, scrollbars, selection drags and
//! middle-click autoscroll move them the way Chrome does, and focus, find,
//! fragments and the caret are brought into view through every scroll
//! container above them.
//!
//! The offsets are the tab's: the layout tree is given them
//! (`LayoutEngine::layout_with_scrolls`) and moves a scroller's content
//! without a relayout (`LayoutTree::set_scroll_offset`); the page's offset
//! is applied by the painter and told to the tree for sticky boxes.

use std::time::{Duration, Instant};

use browser_dom::{Document, NodeId};
use browser_ipc_types::{Cursor, TabToShell};
use browser_layout::{LayoutTree, Rect, ScrollInfo, ScrollbarGeometry, ScrollbarPart};
use browser_style::{ComputedLpAuto, OverscrollBehavior, Sides};

use super::{LINE_SCROLL, TabState, is_focusable};

/// What a scroll acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ScrollTarget {
    Page,
    Element(NodeId),
}

/// A wheel gesture stays with the scroller it started on while its events
/// come closer together than this (Chrome's scroll latching, as recalled).
pub(super) const WHEEL_LATCH: Duration = Duration::from_millis(500);
/// A held scrollbar button or track repeats after this, then at the
/// shorter interval (Blink's autoscroll timer delays).
const REPEAT_INITIAL: Duration = Duration::from_millis(250);
const REPEAT: Duration = Duration::from_millis(50);
/// A thumb drag snaps back to where it started when the pointer strays
/// further than this many bar thicknesses from the bar (Blink's
/// off-side multiplier).
const SNAP_BACK_MULTIPLIER: f32 = 8.0;
/// Middle-click autoscroll (Blink's `AutoscrollController`, as recalled):
/// nothing within this radius of the press, then a speed in px/ms of
/// `multiplier × distance^exponent` toward the pointer.
const PAN_RADIUS: f32 = 15.0;
const PAN_EXPONENT: f32 = 2.2;
const PAN_MULTIPLIER: f32 = 0.000008;
const PAN_TICK: Duration = Duration::from_millis(16);
/// A selection drag held outside a scroller scrolls it a step at each of
/// these intervals (the page's existing autoscroll).
const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);

/// A wheel gesture latched to a target.
#[derive(Debug, Clone, Copy)]
pub(super) struct Latch {
    pub target: ScrollTarget,
    pub at: Instant,
}

/// A scrollbar thumb being dragged.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScrollDrag {
    pub target: ScrollTarget,
    pub vertical: bool,
    /// Where in the thumb it was grabbed, along the bar.
    pub grab: f32,
    /// The bar position when the drag began, to snap back to.
    pub origin: f32,
}

/// A scrollbar button or track being held: it steps again when due.
#[derive(Debug, Clone, Copy)]
pub(super) struct BarRepeat {
    pub target: ScrollTarget,
    pub vertical: bool,
    pub part: ScrollbarPart,
    pub next: Instant,
}

/// Middle-click autoscroll in progress.
#[derive(Debug, Clone, Copy)]
pub(super) struct Pan {
    /// Where the middle button went down, in viewport coordinates.
    pub origin: (f32, f32),
    /// The node under the press: the scroll chain starts there.
    pub start: Option<NodeId>,
    /// The button is still held; released without moving, the mode stays
    /// on until the next press, key or wheel.
    pub pressed: bool,
    /// The pointer left the dead zone while the button was held.
    pub moved: bool,
    pub last: Instant,
    pub pointer: (f32, f32),
}

/// How a rectangle is brought into a scrollport.
#[derive(Debug, Clone, Copy)]
pub(super) enum Reveal {
    /// Scroll the least that puts it inside, with this much room around
    /// it; nothing when it already is.
    Nearest(f32),
    /// Its top at the scrollport's top (fragment navigation).
    Start,
    /// A third of the way down, and across, when it is not fully inside
    /// (find in page).
    Third,
}

/// A scrolling key.
#[derive(Debug, Clone, Copy)]
pub(super) enum KeyScroll {
    Line(f32, f32),
    /// A page down (`1.0`) or up (`-1.0`).
    Page(f32),
    Home,
    End,
}

/// Blink's page step: seven eighths of the scrollport, or all of it but
/// 40px, whichever is more.
pub(super) fn page_step(length: f32) -> f32 {
    (length * 0.875).max(length - 40.0).max(1.0)
}

/// The viewport scroll (`dx`, `dy`) that brings `rect` into `port` per
/// `how`; both in the same coordinates.
fn reveal_delta(how: Reveal, rect: Rect, port: Rect) -> (f32, f32) {
    let axis = |lo: f32, hi: f32, port_lo: f32, port_hi: f32| -> f32 {
        match how {
            Reveal::Nearest(m) => {
                let mut d = 0.0;
                if hi + m > port_hi {
                    d = hi + m - port_hi;
                }
                if lo - m < port_lo {
                    d = lo - m - port_lo;
                }
                d
            }
            Reveal::Start => lo - port_lo,
            Reveal::Third => {
                if lo < port_lo || hi > port_hi {
                    lo - port_lo - (port_hi - port_lo) / 3.0
                } else {
                    0.0
                }
            }
        }
    };
    let dy = axis(rect.y, rect.bottom(), port.y, port.bottom());
    let dx = match how {
        Reveal::Start => 0.0,
        _ => axis(rect.x, rect.right(), port.x, port.right()),
    };
    (dx, dy)
}

/// The smallest rectangle around every fragment of an element (its own
/// boxes, or its content's for an inline element), in page coordinates.
pub(super) fn element_rect(tree: &LayoutTree, doc: &Document, node: NodeId) -> Option<Rect> {
    let mut out: Option<Rect> = None;
    tree.root.walk(&mut |f| {
        let Some(n) = f.node else { return };
        let mut cur = Some(n);
        let inside = loop {
            match cur {
                Some(c) if c == node => break true,
                Some(c) => cur = doc.parent(c),
                None => break false,
            }
        };
        if inside && f.rect.width > 0.0 && f.rect.height > 0.0 {
            out = Some(out.map_or(f.rect, |o| o.union(&f.rect)));
        }
    });
    out
}

impl TabState {
    /// The scroll state of a target, the page's with its current offset.
    pub(super) fn scroll_info(&self, target: ScrollTarget) -> Option<ScrollInfo> {
        let tree = self.layout.as_ref()?;
        match target {
            ScrollTarget::Page => {
                let mut info = tree.viewport.clone();
                info.offset = (self.scroll_x, self.scroll_y);
                Some(info)
            }
            ScrollTarget::Element(n) => tree.scroller(n).cloned(),
        }
    }

    fn target_exists(&self, target: ScrollTarget) -> bool {
        match target {
            ScrollTarget::Page => self.layout.is_some(),
            ScrollTarget::Element(n) => self.layout.as_ref().is_some_and(|t| t.scroller(n).is_some()),
        }
    }

    /// Scroll a target to `want`, clamped to its range. A `user` scroll
    /// leaves an axis whose `overflow` is `hidden` alone (scripts, focus
    /// and the like may move it). True when the offset changed.
    pub(super) fn set_scroll(&mut self, target: ScrollTarget, want: (f32, f32), user: bool) -> bool {
        let Some(tree) = self.layout.as_mut() else { return false };
        let changed = match target {
            ScrollTarget::Page => {
                let info = &tree.viewport;
                let (ux, uy) = info.user_scrollable();
                let want = (
                    if user && !ux { self.scroll_x } else { want.0 },
                    if user && !uy { self.scroll_y } else { want.1 },
                );
                let new = info.clamp(want);
                let changed = new != (self.scroll_x, self.scroll_y);
                self.scroll_x = new.0;
                self.scroll_y = new.1;
                tree.set_page_scroll(new);
                changed
            }
            ScrollTarget::Element(n) => {
                let Some(info) = tree.scroller(n) else { return false };
                let (ux, uy) = info.user_scrollable();
                let old = info.offset;
                let want = (if user && !ux { old.0 } else { want.0 }, if user && !uy { old.1 } else { want.1 });
                let Some(new) = tree.set_scroll_offset(n, want) else { return false };
                self.scrolls.insert(n, new);
                new != old
            }
        };
        if changed {
            self.needs_paint = true;
            self.hover_dirty = true;
        }
        changed
    }

    pub(super) fn scroll_by(&mut self, target: ScrollTarget, dx: f32, dy: f32, user: bool) -> bool {
        let Some(info) = self.scroll_info(target) else { return false };
        self.set_scroll(target, (info.offset.0 + dx, info.offset.1 + dy), user)
    }

    /// Bring the page's offset back into range after a layout.
    pub(super) fn clamp_scroll(&mut self) {
        if self.layout.is_none() {
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
            return;
        }
        self.set_scroll(ScrollTarget::Page, (self.scroll_x, self.scroll_y), false);
    }

    /// The scroll containers from `from` outward (`from` itself first when
    /// it is one), then the page.
    pub(super) fn scroll_chain(&self, from: Option<NodeId>) -> Vec<ScrollTarget> {
        let mut out = Vec::new();
        if let (Some(doc), Some(tree)) = (&self.doc, &self.layout) {
            let mut cur = from.filter(|&n| doc.contains(n));
            while let Some(n) = cur {
                if tree.scroller(n).is_some() {
                    out.push(ScrollTarget::Element(n));
                }
                cur = doc.parent(n);
            }
        }
        out.push(ScrollTarget::Page);
        out
    }

    /// The target a user scroll by `(dx, dy)` from `from` goes to: the
    /// first scroll container up the chain that can move that way (scroll
    /// chaining), stopped by an `overscroll-behavior` other than `auto`
    /// on the axis of the larger delta.
    pub(super) fn chain_target(&self, from: Option<NodeId>, dx: f32, dy: f32) -> Option<ScrollTarget> {
        let vertical = dy.abs() >= dx.abs();
        for t in self.scroll_chain(from) {
            let Some(info) = self.scroll_info(t) else { continue };
            if info.can_scroll_by(dx, dy) {
                return Some(t);
            }
            if let ScrollTarget::Element(n) = t
                && let Some(s) = self.styles.get(n)
            {
                let behavior = if vertical { s.overscroll_behavior_y } else { s.overscroll_behavior_x };
                if behavior != OverscrollBehavior::Auto {
                    return None;
                }
            }
        }
        None
    }

    /// A wheel scroll at the pointer: latched to the gesture's target
    /// while the events keep coming, else to the first scroller up from
    /// the pointer that can take it.
    pub(super) fn wheel_scroll(&mut self, dx: f32, dy: f32) {
        self.stop_pan();
        let now = Instant::now();
        let latched = self
            .wheel_latch
            .filter(|l| now.duration_since(l.at) < WHEEL_LATCH && self.target_exists(l.target))
            .map(|l| l.target);
        let target = latched.or_else(|| {
            let from = self.mouse.and_then(|(x, y)| self.hit_node(x, y));
            self.chain_target(from, dx, dy)
        });
        match target {
            Some(t) => {
                self.wheel_latch = Some(Latch { target: t, at: now });
                self.scroll_by(t, dx, dy, true);
            }
            None => self.wheel_latch = None,
        }
    }

    /// A scrolling key: it goes to the first scroller up from the focused
    /// element (or from the last primary press when nothing is focused)
    /// that can move that way, then the page.
    pub(super) fn keyboard_scroll(&mut self, key: KeyScroll) {
        self.stop_pan();
        let from = self.focus.or(self.press_scroll_start);
        for t in self.scroll_chain(from) {
            let Some(info) = self.scroll_info(t) else { continue };
            let (dx, dy) = match key {
                KeyScroll::Line(x, y) => (x, y),
                KeyScroll::Page(dir) => (0.0, dir * page_step(info.scrollport.height)),
                KeyScroll::Home => (0.0, -1.0e9),
                KeyScroll::End => (0.0, 1.0e9),
            };
            if info.can_scroll_by(dx, dy) {
                self.scroll_by(t, dx, dy, true);
                return;
            }
        }
    }

    /// Whether a scroller with no focusable content is in the Tab order
    /// (Chrome's keyboard-focusable scrollers): it scrolls in some axis
    /// and nothing inside it takes focus.
    pub(super) fn keyboard_focusable_scroller(&self, n: NodeId) -> bool {
        let (Some(doc), Some(tree)) = (&self.doc, &self.layout) else { return false };
        let Some(info) = tree.scroller(n) else { return false };
        let (ux, uy) = info.user_scrollable();
        let (ox, oy) = info.overflows();
        if !((ux && ox) || (uy && oy)) {
            return false;
        }
        !doc.descendants(n).any(|d| doc.element(d).is_some_and(is_focusable))
    }

    // ----- scrollbars -----

    /// The scrollbar under a viewport point and the part hit: the page's
    /// bars, which lie over everything, else those of the scroll
    /// container there. The geometry is in page coordinates for an
    /// element's bar, viewport coordinates for the page's (the same at
    /// the viewport box).
    pub(super) fn scrollbar_at(&self, x: f32, y: f32) -> Option<(ScrollTarget, ScrollbarGeometry, ScrollbarPart)> {
        let tree = self.layout.as_ref()?;
        let page = self.scroll_info(ScrollTarget::Page)?;
        for g in [page.vertical_scrollbar(), page.horizontal_scrollbar()].into_iter().flatten() {
            if let Some(part) = g.hit(x, y) {
                return Some((ScrollTarget::Page, g, part));
            }
        }
        if page.corner().is_some_and(|c| c.contains(x, y)) {
            return None;
        }
        let (px, py) = (x + self.scroll_x, y + self.scroll_y);
        let node = tree.hit_test(px, py)?;
        let info = tree.scroller(node)?;
        if !info.padding_box.contains(px, py) || info.scrollport.contains(px, py) {
            return None;
        }
        for g in [info.vertical_scrollbar(), info.horizontal_scrollbar()].into_iter().flatten() {
            if let Some(part) = g.hit(px, py) {
                return Some((ScrollTarget::Element(node), g, part));
            }
        }
        None
    }

    /// Whether a viewport point is on a scrollbar or a corner, the page's
    /// or an element's: no page mouse events reach there.
    pub(super) fn over_scrollbar_area(&self, x: f32, y: f32) -> bool {
        if self.scrollbar_at(x, y).is_some() {
            return true;
        }
        let Some(tree) = self.layout.as_ref() else { return false };
        if tree.viewport.corner().is_some_and(|c| c.contains(x, y)) {
            return true;
        }
        let (px, py) = (x + self.scroll_x, y + self.scroll_y);
        tree.hit_test(px, py)
            .and_then(|n| tree.scroller(n))
            .is_some_and(|i| i.padding_box.contains(px, py) && !i.scrollport.contains(px, py))
    }

    /// Whether a viewport point is on the page's own scrollbars, which
    /// cover the page: nothing under them is hovered.
    pub(super) fn over_page_scrollbar(&self, x: f32, y: f32) -> bool {
        let Some(tree) = self.layout.as_ref() else { return false };
        let v = &tree.viewport;
        [v.vertical_track(), v.horizontal_track(), v.corner()]
            .into_iter()
            .flatten()
            .any(|r| r.contains(x, y))
    }

    /// The pointer's position along a bar's axis in the bar's coordinates.
    fn along_bar(&self, target: ScrollTarget, vertical: bool, x: f32, y: f32) -> (f32, f32, f32) {
        let (px, py) = match target {
            ScrollTarget::Page => (x, y),
            ScrollTarget::Element(_) => (x + self.scroll_x, y + self.scroll_y),
        };
        (if vertical { py } else { px }, px, py)
    }

    /// A primary press on a scrollbar: the thumb starts a drag, a button a
    /// line step, the track a page step (Shift: a jump to the pointer),
    /// buttons and track repeating while held. True when a scrollbar or
    /// corner took the press.
    pub(super) fn scrollbar_mouse_down(&mut self, x: f32, y: f32, shift: bool) -> bool {
        let Some((target, g, part)) = self.scrollbar_at(x, y) else {
            return self.over_scrollbar_area(x, y);
        };
        self.stop_pan();
        let (along, _, _) = self.along_bar(target, g.vertical, x, y);
        match part {
            ScrollbarPart::Thumb => {
                let thumb = g.thumb_rect();
                let start = if g.vertical { thumb.y } else { thumb.x };
                self.scroll_drag = Some(ScrollDrag {
                    target,
                    vertical: g.vertical,
                    grab: along - start,
                    origin: g.position,
                });
            }
            ScrollbarPart::TrackBefore | ScrollbarPart::TrackAfter if shift => {
                let position = g.position_for_thumb_centre(along);
                self.set_bar_position(target, g.vertical, position);
            }
            _ => {
                self.bar_step(target, g.vertical, part);
                self.bar_repeat = Some(BarRepeat {
                    target,
                    vertical: g.vertical,
                    part,
                    next: Instant::now() + REPEAT_INITIAL,
                });
            }
        }
        self.bar_pressed = Some((target, g.vertical, part));
        self.needs_paint = true;
        true
    }

    /// A line step for a button, a page step for the track.
    fn bar_step(&mut self, target: ScrollTarget, vertical: bool, part: ScrollbarPart) {
        let Some(info) = self.scroll_info(target) else { return };
        let length = if vertical { info.scrollport.height } else { info.scrollport.width };
        let d = match part {
            ScrollbarPart::ButtonStart => -LINE_SCROLL,
            ScrollbarPart::ButtonEnd => LINE_SCROLL,
            ScrollbarPart::TrackBefore => -page_step(length),
            ScrollbarPart::TrackAfter => page_step(length),
            ScrollbarPart::Thumb => 0.0,
        };
        if vertical {
            self.scroll_by(target, 0.0, d, true);
        } else {
            self.scroll_by(target, d, 0.0, true);
        }
    }

    /// Scroll so a bar's thumb is at `position` along it.
    fn set_bar_position(&mut self, target: ScrollTarget, vertical: bool, position: f32) {
        let Some(info) = self.scroll_info(target) else { return };
        let want = if vertical {
            (info.offset.0, position)
        } else {
            (info.offset_for_horizontal_position(position), info.offset.1)
        };
        self.set_scroll(target, want, true);
    }

    /// The pointer moved during a thumb drag: the thumb follows along the
    /// bar, or snaps back to where the drag began when the pointer is far
    /// off the bar. True when a drag is on.
    pub(super) fn scrollbar_mouse_move(&mut self, x: f32, y: f32) -> bool {
        let Some(drag) = self.scroll_drag else { return false };
        let Some(info) = self.scroll_info(drag.target) else { return true };
        let geometry = if drag.vertical { info.vertical_scrollbar() } else { info.horizontal_scrollbar() };
        let Some(g) = geometry else { return true };
        let (along, px, py) = self.along_bar(drag.target, drag.vertical, x, y);
        let position = if g.distance_across(px, py) > SNAP_BACK_MULTIPLIER * g.thickness {
            drag.origin
        } else {
            g.position_for_thumb_start(along - drag.grab)
        };
        self.set_bar_position(drag.target, drag.vertical, position);
        true
    }

    /// The button went up: a drag or a held button or track ends. True
    /// when one was on.
    pub(super) fn scrollbar_mouse_up(&mut self) -> bool {
        let had = self.scroll_drag.is_some() || self.bar_repeat.is_some() || self.bar_pressed.is_some();
        self.scroll_drag = None;
        self.bar_repeat = None;
        self.bar_pressed = None;
        if had {
            self.needs_paint = true;
            self.hover_dirty = true;
        }
        had
    }

    /// A held button or track steps again when due; a held track waits
    /// while the thumb has reached the pointer.
    pub(super) fn bar_repeat_tick(&mut self, now: Instant) {
        let Some(r) = self.bar_repeat else { return };
        if now < r.next {
            return;
        }
        let next = BarRepeat { next: now + REPEAT, ..r };
        if matches!(r.part, ScrollbarPart::TrackBefore | ScrollbarPart::TrackAfter) {
            let still_there = self
                .mouse
                .and_then(|(x, y)| self.scrollbar_at(x, y))
                .is_some_and(|(t, g, p)| t == r.target && g.vertical == r.vertical && p == r.part);
            if !still_there {
                self.bar_repeat = Some(next);
                return;
            }
        }
        self.bar_step(r.target, r.vertical, r.part);
        self.bar_repeat = Some(next);
    }

    /// Keep the scrollbar hover highlight current for the pointer.
    pub(super) fn update_bar_hover(&mut self) {
        let hover = self
            .mouse
            .and_then(|(x, y)| self.scrollbar_at(x, y))
            .map(|(t, g, p)| (t, g.vertical, p));
        if hover != self.bar_hover {
            self.bar_hover = hover;
            self.needs_paint = true;
        }
    }

    // ----- selection autoscroll -----

    /// Whether a selection drag with the pointer at a viewport point would
    /// scroll something: the pointer is outside the scrollport of a
    /// scroller above the selection's anchor that can move toward it, or
    /// outside the viewport with a page that can.
    pub(super) fn autoscroll_wanted(&self, x: f32, y: f32) -> bool {
        self.autoscroll_target(x, y).is_some()
    }

    fn autoscroll_target(&self, x: f32, y: f32) -> Option<(ScrollTarget, f32, f32)> {
        let anchor = self.select_anchor?;
        let (px, py) = (x + self.scroll_x, y + self.scroll_y);
        for t in self.scroll_chain(Some(anchor.node)) {
            let Some(info) = self.scroll_info(t) else { continue };
            let port = match t {
                ScrollTarget::Page => info.scrollport.translate(self.scroll_x, self.scroll_y),
                ScrollTarget::Element(_) => info.scrollport,
            };
            let over = |p: f32, lo: f32, hi: f32| if p < lo { p - lo } else if p > hi { p - hi } else { 0.0 };
            let (ox, oy) = (over(px, port.x, port.right()), over(py, port.y, port.bottom()));
            if ox == 0.0 && oy == 0.0 {
                // Inside this scroller's port, so inside every one above it.
                return None;
            }
            let (dx, dy) = ((ox * 0.25).clamp(-60.0, 60.0), (oy * 0.25).clamp(-60.0, 60.0));
            if info.can_scroll_by(dx, dy) {
                return Some((t, dx, dy));
            }
        }
        None
    }

    /// One step of scrolling toward a pointer held outside a scroller
    /// during a selection drag. Returns when the next step is due.
    pub(super) fn autoscroll_step(&mut self) -> Option<Instant> {
        let (x, y) = self.mouse?;
        self.select_anchor?;
        let (target, dx, dy) = self.autoscroll_target(x, y)?;
        if self.scroll_by(target, dx, dy, true) {
            self.extend_selection_to(x, y);
        }
        Some(Instant::now() + AUTOSCROLL_TICK)
    }

    // ----- middle-click autoscroll -----

    /// A middle press not on a link starts autoscroll when something up
    /// from the press can scroll at all. True when it did.
    pub(super) fn start_pan(&mut self, x: f32, y: f32, start: Option<NodeId>) -> bool {
        let any = self.scroll_chain(start).into_iter().any(|t| {
            self.scroll_info(t).is_some_and(|i| {
                let (ux, uy) = i.user_scrollable();
                let (ox, oy) = i.overflows();
                (ux && ox) || (uy && oy)
            })
        });
        if !any {
            return false;
        }
        self.pan = Some(Pan {
            origin: (x, y),
            start,
            pressed: true,
            moved: false,
            last: Instant::now(),
            pointer: (x, y),
        });
        self.set_cursor(Cursor::AllScroll);
        true
    }

    pub(super) fn pan_move(&mut self, x: f32, y: f32) {
        if let Some(p) = &mut self.pan {
            p.pointer = (x, y);
            if (x - p.origin.0).abs() > PAN_RADIUS || (y - p.origin.1).abs() > PAN_RADIUS {
                p.moved = true;
            }
        }
    }

    /// The middle button went up: a press-and-drag ends the autoscroll, a
    /// click leaves the mode on.
    pub(super) fn pan_release(&mut self) {
        let moved = match &mut self.pan {
            Some(p) => {
                p.pressed = false;
                p.moved
            }
            None => return,
        };
        if moved {
            self.stop_pan();
        }
    }

    pub(super) fn stop_pan(&mut self) {
        if self.pan.take().is_some() {
            self.hover_dirty = true;
            self.set_cursor(Cursor::Default);
        }
    }

    /// Scroll for the time since the last tick at the speed the pointer's
    /// distance from the press gives, toward the pointer.
    pub(super) fn pan_tick(&mut self, now: Instant) {
        let Some(p) = self.pan.as_mut() else { return };
        if now < p.last + PAN_TICK {
            return;
        }
        let ms = now.duration_since(p.last).as_secs_f32() * 1000.0;
        p.last = now;
        let speed = |d: f32| {
            if d.abs() <= PAN_RADIUS {
                0.0
            } else {
                d.signum() * PAN_MULTIPLIER * d.abs().powf(PAN_EXPONENT)
            }
        };
        let (dx, dy) = (speed(p.pointer.0 - p.origin.0) * ms, speed(p.pointer.1 - p.origin.1) * ms);
        let start = p.start;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        if let Some(t) = self.chain_target(start, dx, dy) {
            self.scroll_by(t, dx, dy, true);
        }
    }

    pub(super) fn pan_next_wake(&self) -> Option<Instant> {
        self.pan.as_ref().map(|p| p.last + PAN_TICK)
    }

    pub(super) fn set_cursor(&mut self, cursor: Cursor) {
        if cursor != self.cursor {
            self.cursor = cursor;
            self.send(TabToShell::Cursor(cursor));
        }
    }

    // ----- into view -----

    /// Bring a rectangle of `node` into view through every scroll
    /// container above it, innermost first, then the page: each is
    /// scrolled per `how` so the rectangle (as `rect_of` finds it in the
    /// tree after the scrollers below were moved, grown by the element's
    /// `scroll-margin` when `with_margin`) is inside its scrollport inset
    /// by its `scroll-padding` (the root element's for the page). Hidden
    /// scrollers move too, as for any programmatic scroll.
    pub(super) fn reveal(
        &mut self,
        node: NodeId,
        rect_of: &dyn Fn(&LayoutTree, &Document) -> Option<Rect>,
        how: Reveal,
        with_margin: bool,
    ) {
        let margin = if with_margin {
            self.styles.get(node).map(|s| s.scroll_margin).unwrap_or(Sides::all(0.0))
        } else {
            Sides::all(0.0)
        };
        let parent = self.doc.as_ref().and_then(|d| d.parent(node));
        for t in self.scroll_chain(parent) {
            let rect = {
                let (Some(tree), Some(doc)) = (&self.layout, &self.doc) else { return };
                let Some(r) = rect_of(tree, doc) else { return };
                Rect::new(
                    r.x - margin.left,
                    r.y - margin.top,
                    r.width + margin.left + margin.right,
                    r.height + margin.top + margin.bottom,
                )
            };
            let Some(info) = self.scroll_info(t) else { continue };
            let port = match t {
                ScrollTarget::Page => info.scrollport.translate(self.scroll_x, self.scroll_y),
                ScrollTarget::Element(_) => info.scrollport,
            };
            let padding_source = match t {
                ScrollTarget::Page => self.doc.as_ref().and_then(|d| d.document_element()),
                ScrollTarget::Element(n) => Some(n),
            };
            let sp = padding_source
                .and_then(|n| self.styles.get(n))
                .map(|s| s.scroll_padding)
                .unwrap_or(Sides::all(ComputedLpAuto::Auto));
            let resolve = |v: ComputedLpAuto, basis: f32| v.resolve(basis).unwrap_or(0.0);
            let port = port.inset(
                resolve(sp.top, port.height),
                resolve(sp.right, port.width),
                resolve(sp.bottom, port.height),
                resolve(sp.left, port.width),
            );
            let (dx, dy) = reveal_delta(how, rect, port);
            if dx != 0.0 || dy != 0.0 {
                self.scroll_by(t, dx, dy, false);
            }
        }
    }

    /// After a layout: the element offsets are what the tree applied
    /// (clamped; gone for elements that are no longer scroll containers)
    /// and the page's is clamped.
    pub(super) fn sync_scrolls_after_layout(&mut self) {
        if let Some(tree) = &self.layout {
            self.scrolls = tree
                .scrollers
                .iter()
                .filter(|(_, i)| i.offset != (0.0, 0.0))
                .map(|(n, i)| (*n, i.offset))
                .collect();
        }
        self.clamp_scroll();
        if let Some(l) = self.wheel_latch
            && !self.target_exists(l.target)
        {
            self.wheel_latch = None;
        }
    }
}
