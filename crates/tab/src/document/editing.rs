//! Editing inside text controls (Phase 3 item 3.3, block 3): the
//! selection within a control, the keys and the mouse that move and
//! change it, the clipboard, and IME composition with its events.
//!
//! A control's value lives in its `value` attribute (a textarea's in its
//! text); the caret and the selection anchor are byte offsets into it.
//! Every change goes through `replace_range`, which fires `beforeinput`
//! and `input` around it. The composition text of an IME is part of the
//! value while it is being composed, as in browsers, underlined by the
//! painter, and replaced by the committed text at the end.

use browser_dom::NodeId;
use browser_ipc_types::{Key, TabToShell};
use browser_layout::SelectionRanges;
use browser_script::{EventTargetRef, UiClass, UiEventInit};

use super::{TabState, input_type, is_text_control, is_textarea};

/// A composition in progress: the IME's text sits in the value at
/// `start` for `len` bytes until it is committed or cancelled.
#[derive(Debug, Clone, Copy)]
pub(super) struct Composition {
    pub start: usize,
    pub len: usize,
    /// `compositionstart` was cancelled: nothing is shown or kept.
    pub refused: bool,
    /// The IME cleared the preedit. A commit may follow as the next
    /// message; anything else makes it a cancel.
    pub clearing: bool,
}

/// A bullet is what a password shows per character.
const BULLET_LEN: usize = '\u{2022}'.len_utf8();

/// The start of the word before `at` (a run of letters and digits, or
/// of other characters, spaces skipped first), for Ctrl+Left and
/// Ctrl+Backspace.
fn word_left(value: &str, at: usize) -> usize {
    let mut chars: Vec<(usize, char)> = value[..at].char_indices().collect();
    while let Some(&(_, c)) = chars.last()
        && c.is_whitespace()
    {
        chars.pop();
    }
    let Some(&(_, last)) = chars.last() else { return 0 };
    let class = last.is_alphanumeric();
    while let Some(&(i, c)) = chars.last()
        && c.is_alphanumeric() == class
        && !c.is_whitespace()
    {
        chars.pop();
        if chars.is_empty() {
            return i;
        }
    }
    chars.last().map_or(0, |&(i, c)| i + c.len_utf8())
}

/// The end of the word after `at`, for Ctrl+Right and Ctrl+Delete.
fn word_right(value: &str, at: usize) -> usize {
    let mut it = value[at..].char_indices().peekable();
    while let Some(&(_, c)) = it.peek()
        && c.is_whitespace()
    {
        it.next();
    }
    let Some(&(_, first)) = it.peek() else { return value.len() };
    let class = first.is_alphanumeric();
    let mut end = value.len();
    for (i, c) in it {
        if c.is_alphanumeric() != class || c.is_whitespace() {
            end = at + i;
            break;
        }
    }
    end
}

/// The word around `at`: a run of letters and digits, or a run of
/// spaces, as a double click selects it.
fn word_at(value: &str, at: usize) -> (usize, usize) {
    let at = at.min(value.len());
    let class_of = |c: char| if c.is_whitespace() { 1 } else if c.is_alphanumeric() { 2 } else { 0 };
    let here = value[at..]
        .chars()
        .next()
        .or_else(|| value[..at].chars().next_back())
        .map(class_of);
    let Some(class) = here else { return (0, 0) };
    let start = value[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class_of(c) == class)
        .last()
        .map_or(at, |(i, _)| i);
    let end = value[at..]
        .char_indices()
        .find(|&(_, c)| class_of(c) != class)
        .map_or(value.len(), |(i, _)| at + i);
    (start, end)
}

/// The byte offset of the character `n` positions into `value`, for
/// the line-to-line moves of a textarea.
fn offset_of_column(line: &str, column: usize) -> usize {
    line.char_indices().nth(column).map_or(line.len(), |(i, _)| i)
}

impl TabState {
    /// The focused text control, if that is what has focus.
    pub(super) fn text_focus(&self) -> Option<NodeId> {
        self.focus
            .filter(|&f| self.doc.as_ref().and_then(|d| d.element(f)).is_some_and(is_text_control))
    }

    fn is_password(&self, id: NodeId) -> bool {
        self.doc
            .as_ref()
            .and_then(|d| d.element(id))
            .is_some_and(|e| input_type(e).as_deref() == Some("password"))
    }

    /// The selection in the focused control as `(lo, hi)` byte offsets
    /// of its value, when not collapsed.
    pub(super) fn control_selection(&self) -> Option<(usize, usize)> {
        let anchor = self.sel_anchor?;
        let caret = self.caret;
        (anchor != caret).then(|| (anchor.min(caret), anchor.max(caret)))
    }

    /// A value byte offset as an offset into the text the control shows
    /// (a password shows one bullet per character).
    fn display_offset(&self, id: NodeId, value: &str, offset: usize) -> usize {
        if self.is_password(id) {
            value[..offset.min(value.len())].chars().count() * BULLET_LEN
        } else {
            offset
        }
    }

    /// The reverse: an offset into the shown text as a value offset.
    fn value_offset(&self, id: NodeId, value: &str, display: usize) -> usize {
        if self.is_password(id) {
            value
                .char_indices()
                .nth(display / BULLET_LEN)
                .map_or(value.len(), |(i, _)| i)
        } else {
            display.min(value.len())
        }
    }

    /// The highlight of the selection inside the focused control.
    pub(super) fn control_selection_ranges(&self) -> SelectionRanges {
        let mut ranges = SelectionRanges::default();
        if let (Some(f), Some((lo, hi))) = (self.text_focus(), self.control_selection()) {
            let value = self.control_value(f);
            ranges.add(f, self.display_offset(f, &value, lo), self.display_offset(f, &value, hi));
        }
        ranges
    }

    /// The composition text to underline.
    pub(super) fn composition_ranges(&self) -> SelectionRanges {
        let mut ranges = SelectionRanges::default();
        if let (Some(f), Some(c)) = (self.text_focus(), self.composition)
            && !c.refused
            && c.len > 0
        {
            let value = self.control_value(f);
            ranges.add(
                f,
                self.display_offset(f, &value, c.start),
                self.display_offset(f, &value, c.start + c.len),
            );
        }
        ranges
    }

    /// Put the caret at `caret` with the selection anchored at `anchor`
    /// (none: collapsed). A changed, non-empty selection fires `select`.
    fn set_caret(&mut self, control: NodeId, caret: usize, anchor: Option<usize>) {
        let value = self.control_value(control);
        let clamp = |mut off: usize| {
            off = off.min(value.len());
            while !value.is_char_boundary(off) {
                off -= 1;
            }
            off
        };
        self.caret = clamp(caret);
        self.sel_anchor = anchor.map(clamp).filter(|&a| a != self.caret);
        self.needs_paint = true;
        self.update_caret();
        let selection = self.control_selection();
        if selection != self.last_select {
            self.last_select = selection;
            if selection.is_some() {
                self.fire_user_event(
                    EventTargetRef::Node(control),
                    "select",
                    UiEventInit {
                        bubbles: true,
                        ..UiEventInit::default()
                    },
                );
            }
        }
    }

    fn move_caret(&mut self, control: NodeId, target: usize, extend: bool) {
        let anchor = if extend { Some(self.sel_anchor.unwrap_or(self.caret)) } else { None };
        self.set_caret(control, target, anchor);
    }

    /// Replace `lo..hi` of the control's value with `text` after a
    /// `beforeinput` of `input_type` (cancelable unless `forced`, as a
    /// composition's is not), then `input`. The caret ends after the
    /// text. False when a listener cancelled it.
    fn replace_range(
        &mut self,
        control: NodeId,
        (lo, hi): (usize, usize),
        text: &str,
        input_type: &str,
        data: Option<&str>,
        forced: bool,
    ) -> bool {
        let init = |cancelable: bool| UiEventInit {
            class: UiClass::Input,
            bubbles: true,
            cancelable,
            data: data.map(str::to_owned),
            input_type: input_type.to_owned(),
            ..UiEventInit::default()
        };
        if !self.fire_user_event(EventTargetRef::Node(control), "beforeinput", init(!forced)) && !forced {
            return false;
        }
        let mut value = self.control_value(control);
        let (lo, hi) = (lo.min(value.len()), hi.min(value.len()));
        value.replace_range(lo..hi, text);
        self.set_control_value(control, &value);
        // With a layout pending the caret is placed after it.
        self.caret = lo + text.len();
        self.sel_anchor = None;
        self.last_select = None;
        self.fire_user_event(EventTargetRef::Node(control), "input", init(false));
        true
    }

    /// Editing keys in a focused text control.
    pub(super) fn edit_key(&mut self, f: NodeId, key: Key, shift: bool, ctrl: bool, alt: bool) {
        let textarea = self.doc.as_ref().and_then(|d| d.element(f)).is_some_and(is_textarea);
        let value = self.control_value(f);
        let mut caret = self.caret.min(value.len());
        while !value.is_char_boundary(caret) {
            caret -= 1;
        }
        let selection = self.control_selection();
        let prev_len = |c: usize| value[..c].chars().next_back().map_or(0, char::len_utf8);
        let next_len = |c: usize| value[c..].chars().next().map_or(0, char::len_utf8);
        let insert = |this: &mut Self, text: &str, input_type: &str, data: Option<&str>| {
            let range = selection.unwrap_or((caret, caret));
            this.replace_range(f, range, text, input_type, data, false);
        };
        match key {
            Key::Character(s) if !ctrl && !alt => {
                let s: String = s.chars().filter(|c| !c.is_control()).collect();
                if !s.is_empty() {
                    insert(self, &s, "insertText", Some(&s));
                }
            }
            Key::Space if !ctrl && !alt => insert(self, " ", "insertText", Some(" ")),
            Key::Enter if textarea && !ctrl && !alt => insert(self, "\n", "insertLineBreak", None),
            Key::Backspace => {
                let (range, kind) = match selection {
                    Some(sel) => (sel, "deleteContentBackward"),
                    None if ctrl => ((word_left(&value, caret), caret), "deleteWordBackward"),
                    None => ((caret - prev_len(caret), caret), "deleteContentBackward"),
                };
                if range.0 < range.1 {
                    self.replace_range(f, range, "", kind, None, false);
                }
            }
            Key::Delete => {
                let (range, kind) = match selection {
                    Some(sel) => (sel, "deleteContentForward"),
                    None if ctrl => ((caret, word_right(&value, caret)), "deleteWordForward"),
                    None => ((caret, caret + next_len(caret)), "deleteContentForward"),
                };
                if range.0 < range.1 {
                    self.replace_range(f, range, "", kind, None, false);
                }
            }
            Key::ArrowLeft => {
                let target = match selection {
                    _ if ctrl => word_left(&value, caret),
                    Some((lo, _)) if !shift => lo,
                    _ => caret - prev_len(caret),
                };
                self.move_caret(f, target, shift);
            }
            Key::ArrowRight => {
                let target = match selection {
                    _ if ctrl => word_right(&value, caret),
                    Some((_, hi)) if !shift => hi,
                    _ => caret + next_len(caret),
                };
                self.move_caret(f, target, shift);
            }
            // A line up or down in a textarea keeps the column; in an
            // input they go to the start and the end.
            Key::ArrowUp | Key::ArrowDown => {
                let down = key == Key::ArrowDown;
                let target = if !textarea {
                    if down { value.len() } else { 0 }
                } else {
                    let line_start = value[..caret].rfind('\n').map_or(0, |i| i + 1);
                    let column = value[line_start..caret].chars().count();
                    if down {
                        match value[caret..].find('\n') {
                            Some(i) => {
                                let next = caret + i + 1;
                                let next_end = value[next..].find('\n').map_or(value.len(), |j| next + j);
                                next + offset_of_column(&value[next..next_end], column)
                            }
                            None => value.len(),
                        }
                    } else if line_start == 0 {
                        0
                    } else {
                        let prev_start = value[..line_start - 1].rfind('\n').map_or(0, |i| i + 1);
                        prev_start + offset_of_column(&value[prev_start..line_start - 1], column)
                    }
                };
                self.move_caret(f, target, shift);
            }
            Key::Home => {
                let target = if ctrl { 0 } else { value[..caret].rfind('\n').map_or(0, |i| i + 1) };
                self.move_caret(f, target, shift);
            }
            Key::End => {
                let target = if ctrl {
                    value.len()
                } else {
                    caret + value[caret..].find('\n').unwrap_or(value.len() - caret)
                };
                self.move_caret(f, target, shift);
            }
            _ => {}
        }
    }

    /// The value offset under a viewport point in the control, if the
    /// point is on its text; else the end.
    fn control_offset_at(&self, control: NodeId, x: f32, y: f32) -> usize {
        let value = self.control_value(control);
        let Some(doc) = &self.doc else { return value.len() };
        match self.text_position_at(x, y) {
            Some(p) if p.node == control || doc.parent(p.node) == Some(control) => {
                self.value_offset(control, &value, p.offset)
            }
            _ => value.len(),
        }
    }

    /// A primary press inside the focused text control: one click puts
    /// the caret there (with Shift, extends the selection to there) and
    /// starts a drag; a double click selects the word, a triple the whole
    /// value.
    pub(super) fn control_press(&mut self, control: NodeId, x: f32, y: f32, clicks: u32) {
        let at = self.control_offset_at(control, x, y);
        let value = self.control_value(control);
        match clicks {
            2 => {
                let (start, end) = word_at(&value, at);
                self.set_caret(control, end, Some(start));
            }
            3 => self.set_caret(control, value.len(), Some(0)),
            _ if self.modifiers.shift => {
                let anchor = self.sel_anchor.unwrap_or(self.caret);
                self.set_caret(control, at, Some(anchor));
            }
            _ => {
                self.set_caret(control, at, None);
                self.control_drag = true;
            }
        }
    }

    /// The pointer moved during a drag inside the focused control.
    pub(super) fn control_drag_to(&mut self, x: f32, y: f32) {
        let Some(control) = self.text_focus() else { return };
        let at = self.control_offset_at(control, x, y);
        let anchor = self.sel_anchor.unwrap_or(self.caret);
        self.set_caret(control, at, Some(anchor));
    }

    /// Ctrl+A in a focused text control selects its value.
    pub(super) fn select_all_in_control(&mut self, control: NodeId) {
        let len = self.control_value(control).len();
        self.set_caret(control, len, Some(0));
    }

    /// The selected text of the focused control, for the clipboard;
    /// nothing from a password field.
    fn control_selected_text(&self) -> Option<(NodeId, (usize, usize), String)> {
        let control = self.text_focus()?;
        let sel = self.control_selection()?;
        if self.is_password(control) {
            return None;
        }
        let value = self.control_value(control);
        Some((control, sel, value[sel.0..sel.1].to_owned()))
    }

    /// Ctrl+C with a selection in a text control: true when handled.
    pub(super) fn copy_from_control(&mut self) -> bool {
        let Some((_, _, text)) = self.control_selected_text() else {
            return false;
        };
        self.send(TabToShell::CopyText { text });
        true
    }

    /// Ctrl+X: the selection goes to the clipboard and out of the value
    /// (`deleteByCut`, cancelable).
    pub(super) fn cut_from_control(&mut self) {
        let Some((control, sel, text)) = self.control_selected_text() else {
            return;
        };
        self.send(TabToShell::CopyText { text });
        self.replace_range(control, sel, "", "deleteByCut", None, false);
    }

    /// Ctrl+V: the clipboard text replaces the selection
    /// (`insertFromPaste`, cancelable). An input takes one line: line
    /// breaks are dropped, as browsers do.
    pub(super) fn paste_into_control(&mut self, text: &str) {
        let Some(control) = self.text_focus() else { return };
        let textarea = self.doc.as_ref().and_then(|d| d.element(control)).is_some_and(is_textarea);
        let text: String = if textarea {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.chars().filter(|c| !matches!(c, '\n' | '\r')).collect()
        };
        let range = self.control_selection().unwrap_or((self.caret, self.caret));
        self.replace_range(control, range, &text, "insertFromPaste", Some(&text), false);
    }

    fn fire_composition(&mut self, control: NodeId, kind: &str, data: &str, cancelable: bool) -> bool {
        self.fire_user_event(
            EventTargetRef::Node(control),
            kind,
            UiEventInit {
                class: UiClass::Composition,
                bubbles: true,
                cancelable,
                data: Some(data.to_owned()),
                ..UiEventInit::default()
            },
        )
    }

    /// The IME's composition text changed. The first text starts the
    /// composition (`compositionstart`, cancelable: refused, nothing is
    /// inserted); each text replaces the previous one in the value with
    /// `beforeinput` and `input` of `insertCompositionText` around
    /// `compositionupdate`. An empty text is a clear: a commit may
    /// follow, else `settle_composition` makes it a cancel.
    pub(super) fn ime_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        let Some(control) = self.text_focus() else { return };
        if text.is_empty() {
            if let Some(c) = &mut self.composition {
                c.clearing = true;
            }
            return;
        }
        if self.composition.is_none() {
            let ok = self.fire_composition(control, "compositionstart", "", true);
            // The selection is what the composition replaces.
            let (lo, hi) = self.control_selection().unwrap_or((self.caret, self.caret));
            self.composition = Some(Composition {
                start: lo,
                len: hi - lo,
                refused: !ok,
                clearing: false,
            });
        }
        let Some(mut c) = self.composition else { return };
        c.clearing = false;
        self.composition = Some(c);
        if c.refused {
            return;
        }
        self.fire_composition(control, "compositionupdate", text, false);
        self.replace_range(control, (c.start, c.start + c.len), text, "insertCompositionText", Some(text), true);
        c.len = text.len();
        self.composition = Some(c);
        let at = c.start + cursor.map_or(text.len(), |(_, end)| end.min(text.len()));
        self.set_caret(control, at, None);
    }

    /// The IME committed `text`: it replaces the composition text (or the
    /// selection, when nothing was being composed, as dead keys commit)
    /// and `compositionend` closes the composition.
    pub(super) fn ime_commit(&mut self, text: &str) {
        let Some(control) = self.text_focus() else { return };
        match self.composition.take() {
            Some(c) if !c.refused => {
                self.replace_range(
                    control,
                    (c.start, c.start + c.len),
                    text,
                    "insertCompositionText",
                    Some(text),
                    true,
                );
                self.fire_composition(control, "compositionend", text, false);
            }
            Some(_) => {
                self.fire_composition(control, "compositionend", text, false);
            }
            None => {
                let range = self.control_selection().unwrap_or((self.caret, self.caret));
                self.replace_range(control, range, text, "insertText", Some(text), false);
            }
        }
    }

    /// A composition the IME cleared with no commit following: a cancel.
    /// The text comes out of the value with `compositionupdate` of an
    /// empty string, `input`, and `compositionend`.
    pub(super) fn settle_composition(&mut self) {
        let Some(c) = self.composition else { return };
        if !c.clearing {
            return;
        }
        self.composition = None;
        let Some(control) = self.text_focus() else { return };
        if !c.refused {
            self.fire_composition(control, "compositionupdate", "", false);
            self.replace_range(control, (c.start, c.start + c.len), "", "insertCompositionText", Some(""), true);
        }
        self.fire_composition(control, "compositionend", "", false);
    }

    /// Focus is leaving `control` mid-composition: the text so far is
    /// committed as it stands, as browsers do on blur.
    pub(super) fn end_composition(&mut self, control: NodeId) {
        let Some(c) = self.composition.take() else { return };
        let value = self.control_value(control);
        let text = if c.refused {
            String::new()
        } else {
            value.get(c.start..c.start + c.len).unwrap_or("").to_owned()
        };
        self.fire_composition(control, "compositionend", &text, false);
    }

    /// Tell the shell where the caret of the focused text control is (in
    /// viewport coordinates), or that there is none, when that changed:
    /// it enables the IME and places its candidate window there.
    pub(super) fn report_caret(&mut self) {
        let rect = match (self.text_focus(), self.caret_rect) {
            (Some(_), Some(r)) => Some((r.x - self.scroll_x, r.y - self.scroll_y, r.width, r.height)),
            _ => None,
        };
        if rect != self.caret_reported {
            self.caret_reported = rect;
            self.send(TabToShell::Caret { rect });
        }
    }
}
