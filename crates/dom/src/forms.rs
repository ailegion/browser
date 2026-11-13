//! Form controls' state and the form algorithms ("Working sites first"
//! line 8; HTML §4.10): the value, checkedness and selectedness of each
//! control with their dirty flags, the value modes and sanitization of
//! every `<input type>`, radio button groups, a select's list of options
//! and its selectedness setting algorithm, the form owner (with the
//! parser's form element pointer), the reset algorithms, the entry list
//! a submission sends, and the three form encodings.
//!
//! State is kept beside the tree (`Document::controls`), never in
//! attributes: a control whose value the user or a script changed keeps
//! it while its `value` attribute stays the default, as in browsers. A
//! control nothing has touched has no state and reads its attributes
//! (its default), which is what the dirty flags being false means.
//!
//! Work that HTML does on every tree change (the selectedness setting
//! algorithm when a select's options change, radio groups when a checked
//! radio is inserted) is queued and done by `settle_forms`, which the tab
//! runs before styling and the script bindings after every DOM call, so
//! a burst of parser insertions costs one pass per select, not one per
//! option.

use std::collections::HashMap;

use html5ever::{LocalName, local_name, ns};
use url::Url;

use crate::{Directionality, Document, Element, NodeId, NodeKind};

/// An `<input>` element's type state (HTML §4.10.5). A missing or
/// unknown `type` is Text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputType {
    Hidden,
    Text,
    Search,
    Tel,
    Url,
    Email,
    Password,
    Date,
    Month,
    Week,
    Time,
    DatetimeLocal,
    Number,
    Range,
    Color,
    Checkbox,
    Radio,
    File,
    Submit,
    Image,
    Reset,
    Button,
}

/// How an input's `value` IDL attribute behaves (HTML §4.10.5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueMode {
    Value,
    Default,
    DefaultOn,
    Filename,
}

impl InputType {
    /// The state a `type` attribute value selects (ASCII
    /// case-insensitive; missing or invalid: Text).
    pub fn from_attr(value: Option<&str>) -> Self {
        let Some(v) = value else { return InputType::Text };
        match v.to_ascii_lowercase().as_str() {
            "hidden" => InputType::Hidden,
            "search" => InputType::Search,
            "tel" => InputType::Tel,
            "url" => InputType::Url,
            "email" => InputType::Email,
            "password" => InputType::Password,
            "date" => InputType::Date,
            "month" => InputType::Month,
            "week" => InputType::Week,
            "time" => InputType::Time,
            "datetime-local" => InputType::DatetimeLocal,
            "number" => InputType::Number,
            "range" => InputType::Range,
            "color" => InputType::Color,
            "checkbox" => InputType::Checkbox,
            "radio" => InputType::Radio,
            "file" => InputType::File,
            "submit" => InputType::Submit,
            "image" => InputType::Image,
            "reset" => InputType::Reset,
            "button" => InputType::Button,
            _ => InputType::Text,
        }
    }

    /// The keyword `input.type` returns.
    pub fn as_str(self) -> &'static str {
        match self {
            InputType::Hidden => "hidden",
            InputType::Text => "text",
            InputType::Search => "search",
            InputType::Tel => "tel",
            InputType::Url => "url",
            InputType::Email => "email",
            InputType::Password => "password",
            InputType::Date => "date",
            InputType::Month => "month",
            InputType::Week => "week",
            InputType::Time => "time",
            InputType::DatetimeLocal => "datetime-local",
            InputType::Number => "number",
            InputType::Range => "range",
            InputType::Color => "color",
            InputType::Checkbox => "checkbox",
            InputType::Radio => "radio",
            InputType::File => "file",
            InputType::Submit => "submit",
            InputType::Image => "image",
            InputType::Reset => "reset",
            InputType::Button => "button",
        }
    }

    pub fn value_mode(self) -> ValueMode {
        match self {
            InputType::Hidden | InputType::Submit | InputType::Image | InputType::Reset | InputType::Button => ValueMode::Default,
            InputType::Checkbox | InputType::Radio => ValueMode::DefaultOn,
            InputType::File => ValueMode::Filename,
            _ => ValueMode::Value,
        }
    }

    /// The fields that block implicit submission (HTML §4.10.22.2).
    pub fn blocks_implicit_submission(self) -> bool {
        matches!(
            self,
            InputType::Text
                | InputType::Search
                | InputType::Tel
                | InputType::Url
                | InputType::Email
                | InputType::Password
                | InputType::Date
                | InputType::Month
                | InputType::Week
                | InputType::Time
                | InputType::DatetimeLocal
                | InputType::Number
        )
    }

    /// The states whose `readonly` attribute makes the control immutable.
    pub fn readonly_applies(self) -> bool {
        matches!(
            self,
            InputType::Text
                | InputType::Search
                | InputType::Tel
                | InputType::Url
                | InputType::Email
                | InputType::Password
                | InputType::Date
                | InputType::Month
                | InputType::Week
                | InputType::Time
                | InputType::DatetimeLocal
                | InputType::Number
        )
    }
}

/// A control's state beyond its attributes. `None` fields mean the
/// control follows its content attributes there (its dirty flag is
/// false and nothing has overridden it).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ControlState {
    /// An input's value once its dirty value flag is set (as the user
    /// typed it, or sanitized as a script set it), a textarea's raw value
    /// once dirty.
    pub raw: Option<String>,
    /// An input's checkedness when it differs from following the
    /// `checked` attribute (set by the user, a script, or a radio group).
    pub checked: Option<bool>,
    pub dirty_checked: bool,
    pub indeterminate: bool,
    /// An option's selectedness when it differs from following the
    /// `selected` attribute.
    pub selected: Option<bool>,
    pub dirty_selected: bool,
}

/// An entry of an entry list (HTML §4.10.22.4): a name and a string or a
/// file.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub value: EntryValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryValue {
    Text(String),
    File { name: String, mime: String, bytes: Vec<u8> },
}

/// A form's method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
    Dialog,
}

/// A form's enctype.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enctype {
    UrlEncoded,
    Multipart,
    TextPlain,
}

/// The body of a form submitted as an entity body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostResource {
    pub content_type: String,
    pub body: Vec<u8>,
}

/// What a submission does once its events have run (HTML §4.10.22.3
/// steps 10 to 21): navigate (GET with the entries in the query, or POST
/// with them as the body) or close a dialog.
#[derive(Debug, Clone, PartialEq)]
pub enum SubmitAction {
    Navigate {
        url: Url,
        post: Option<PostResource>,
        /// The target keyword or name (`_self` when none applies).
        target: String,
        /// The form's link types ask for no referrer (`rel=noreferrer`).
        noreferrer: bool,
    },
    CloseDialog { dialog: NodeId, result: Option<String> },
}

/// A range control's minimum, maximum, allowed value step and step base,
/// each step with the digits after its decimal point.
type RangeParameters = (f64, f64, Option<(f64, i32)>, (f64, i32));

/// What legacy-pre-activation saved, to undo it when the `click` is
/// cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreActivation {
    Checkbox { checked: bool, indeterminate: bool, state_existed: bool, dirty: bool },
    Radio { previous: Option<NodeId>, was_checked: bool },
    None,
}

/// What activation behavior does for an element (HTML §4.10.6, §4.10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// A checkbox or radio: `input` and `change` fire.
    Changed,
    /// A submit button: its form owner is submitted with it.
    Submit { form: NodeId },
    /// A reset button: its form owner is reset.
    Reset { form: NodeId },
    Nothing,
}

fn is_html(e: &Element, name: LocalName) -> bool {
    e.name.ns == ns!(html) && e.name.local == name
}

/// HTML's "strip newlines".
fn strip_newlines(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\n' | '\r')).collect()
}

fn trim_ascii_ws(s: &str) -> &str {
    s.trim_matches(['\t', '\n', '\x0C', '\r', ' '])
}

// ----- microsyntaxes (HTML §2.3) -----

/// Whether `s` is a valid floating-point number (HTML §2.3.4.3).
pub fn is_valid_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - f;
        if frac_digits == 0 {
            return false;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

/// HTML's rules for parsing floating-point number values (§2.3.4.3):
/// leading white space skipped, trailing garbage ignored; `None` without
/// a number or out of range.
pub fn parse_float(s: &str) -> Option<f64> {
    let s = s.trim_start_matches(['\t', '\n', '\x0C', '\r', ' ']);
    let b = s.as_bytes();
    let mut i = 0;
    let mut text = String::new();
    match b.first() {
        Some(b'-') => {
            text.push('-');
            i += 1;
        }
        Some(b'+') => i += 1,
        _ => {}
    }
    if i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        text.push('0');
    } else if !b.get(i).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    while i < b.len() && b[i].is_ascii_digit() {
        text.push(b[i] as char);
        i += 1;
    }
    if i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        text.push('.');
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            text.push(b[i] as char);
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        let mut exp = String::from("e");
        if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
            exp.push(b[j] as char);
            j += 1;
        }
        let digits_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            exp.push(b[j] as char);
            j += 1;
        }
        if j > digits_start {
            text.push_str(&exp);
        }
    }
    let v: f64 = text.parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    // Negative zero is zero.
    Some(if v == 0.0 { 0.0 } else { v })
}

/// The best representation of a number as a floating-point number
/// (HTML §2.3.4.3): JavaScript's `Number::toString`.
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_owned();
    }
    if n == 0.0 {
        return "0".to_owned();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let sign = if n < 0.0 { "-" } else { "" };
    // Rust's `{:e}` gives the shortest digits that round-trip.
    let e = format!("{:e}", n.abs());
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    let k = digits.len() as i32;
    let point = exp + 1; // ECMA-262's n: the decimal point's position
    let body = if k <= point && point <= 21 {
        format!("{digits}{}", "0".repeat((point - k) as usize))
    } else if 0 < point && point <= 21 {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    } else if -6 < point && point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else {
        let e = point - 1;
        let es = if e >= 0 { format!("+{e}") } else { format!("{e}") };
        if k == 1 { format!("{digits}e{es}") } else { format!("{}.{}e{es}", &digits[..1], &digits[1..]) }
    };
    format!("{sign}{body}")
}

fn digits_exact(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

fn is_leap(year: u32) -> bool {
    year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        _ => 28,
    }
}

/// A year: four or more digits, greater than zero.
fn parse_year(s: &str) -> Option<u32> {
    if s.len() < 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: u32 = s.parse().ok()?;
    (y > 0).then_some(y)
}

/// A valid month string `YYYY-MM` (HTML §2.3.5.1).
pub fn parse_month_string(s: &str) -> Option<(u32, u32)> {
    let (y, m) = s.rsplit_once('-')?;
    let year = parse_year(y)?;
    let month = digits_exact(m, 2)?;
    (1..=12).contains(&month).then_some((year, month))
}

/// A valid date string `YYYY-MM-DD` (HTML §2.3.5.2).
pub fn parse_date_string(s: &str) -> Option<(u32, u32, u32)> {
    let (ym, d) = s.rsplit_once('-')?;
    let (year, month) = parse_month_string(ym)?;
    let day = digits_exact(d, 2)?;
    (1..=days_in_month(year, month)).contains(&day).then_some((year, month, day))
}

/// Day of the week of a date, 0 = Monday.
fn weekday(year: u32, month: u32, day: u32) -> u32 {
    // Days since 0001-01-01 (a Monday) in the proleptic Gregorian calendar.
    let y = u64::from(year - 1);
    let mut days = y * 365 + y / 4 - y / 100 + y / 400;
    for m in 1..month {
        days += u64::from(days_in_month(year, m));
    }
    days += u64::from(day - 1);
    (days % 7) as u32
}

/// A valid week string `YYYY-Www` (HTML §2.3.5.8): week 53 only in
/// years whose January 1st is a Thursday, or a Wednesday in a leap year.
pub fn parse_week_string(s: &str) -> Option<(u32, u32)> {
    let (y, w) = s.split_once("-W")?;
    let year = parse_year(y)?;
    let week = digits_exact(w, 2)?;
    let jan1 = weekday(year, 1, 1);
    let max = if jan1 == 3 || (jan1 == 2 && is_leap(year)) { 53 } else { 52 };
    (1..=max).contains(&week).then_some((year, week))
}

/// A valid time string `HH:MM[:SS[.s{1,3}]]` (HTML §2.3.5.4), as
/// (hour, minute, second, milliseconds).
pub fn parse_time_string(s: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = s.splitn(3, ':');
    let hour = digits_exact(parts.next()?, 2)?;
    let minute = digits_exact(parts.next()?, 2)?;
    if hour > 23 || minute > 59 {
        return None;
    }
    let (second, millis) = match parts.next() {
        None => (0, 0),
        Some(sec) => {
            let (whole, frac) = match sec.split_once('.') {
                Some((w, f)) => (w, Some(f)),
                None => (sec, None),
            };
            let second = digits_exact(whole, 2)?;
            if second > 59 {
                return None;
            }
            let millis = match frac {
                None => 0,
                Some(f) if (1..=3).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()) => {
                    format!("{f:0<3}").parse().ok()?
                }
                Some(_) => return None,
            };
            (second, millis)
        }
    };
    Some((hour, minute, second, millis))
}

/// The shortest valid time string for a time (HTML's normalized form:
/// seconds left out when zero, the fraction without trailing zeros).
fn shortest_time(hour: u32, minute: u32, second: u32, millis: u32) -> String {
    let mut out = format!("{hour:02}:{minute:02}");
    if second != 0 || millis != 0 {
        out.push_str(&format!(":{second:02}"));
        if millis != 0 {
            let frac = format!("{millis:03}");
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }
    }
    out
}

/// A valid local date and time string as its valid normalized form
/// (HTML §2.3.5.5), `None` when it is not one.
pub fn normalize_local_datetime(s: &str) -> Option<String> {
    let at = s.find(['T', ' '])?;
    let (date, time) = (&s[..at], &s[at + 1..]);
    parse_date_string(date)?;
    let (h, m, sec, ms) = parse_time_string(time)?;
    Some(format!("{date}T{}", shortest_time(h, m, sec, ms)))
}

/// A valid simple colour `#rrggbb`, lowercased.
fn simple_color(s: &str) -> Option<String> {
    let b = s.as_bytes();
    (b.len() == 7 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit)).then(|| s.to_ascii_lowercase())
}

/// Number of digits after the decimal point in a number's text (for
/// rounding step arithmetic back to what decimal arithmetic gives).
fn fraction_digits(s: &str) -> i32 {
    let s = s.trim();
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    let frac = mantissa.split_once('.').map_or(0, |(_, f)| f.len() as i32);
    (frac - exp).max(0)
}

fn round_to(v: f64, digits: i32) -> f64 {
    if digits <= 0 || digits > 15 {
        return v;
    }
    let p = 10f64.powi(digits);
    (v * p).round() / p
}

// ----- the document's control state -----

impl Document {
    fn control_state(&self, id: NodeId) -> Option<&ControlState> {
        self.controls.get(&id)
    }

    fn control_state_mut(&mut self, id: NodeId) -> &mut ControlState {
        self.controls.entry(id).or_default()
    }

    /// An element's HTML local name, if it is an HTML element.
    fn html_name(&self, id: NodeId) -> Option<&LocalName> {
        self.nodes.get(id).and_then(|n| n.as_element()).filter(|e| e.is_html()).map(|e| &e.name.local)
    }

    fn is_html_named(&self, id: NodeId, name: LocalName) -> bool {
        self.html_name(id) == Some(&name)
    }

    /// The type state of an HTML `<input>`, `None` for anything else.
    pub fn input_type(&self, id: NodeId) -> Option<InputType> {
        let e = self.nodes.get(id)?.as_element()?;
        is_html(e, local_name!("input")).then(|| InputType::from_attr(e.attr("type")))
    }

    fn attr_of(&self, id: NodeId, name: &str) -> Option<&str> {
        self.nodes.get(id)?.as_element()?.attr(name)
    }

    // ----- values -----

    /// The value sanitization algorithm of an input's type (HTML
    /// §4.10.5.1).
    fn sanitize(&self, id: NodeId, ty: InputType, value: &str) -> String {
        match ty {
            InputType::Text | InputType::Search | InputType::Tel | InputType::Password => strip_newlines(value),
            InputType::Url => trim_ascii_ws(&strip_newlines(value)).to_owned(),
            InputType::Email => {
                if self.attr_of(id, "multiple").is_some() {
                    value.split(',').map(trim_ascii_ws).collect::<Vec<_>>().join(",")
                } else {
                    trim_ascii_ws(&strip_newlines(value)).to_owned()
                }
            }
            InputType::Number => {
                if is_valid_float(value) {
                    value.to_owned()
                } else {
                    String::new()
                }
            }
            InputType::Range => self.sanitize_range(id, value),
            InputType::Color => simple_color(value).unwrap_or_else(|| "#000000".to_owned()),
            InputType::Date => if parse_date_string(value).is_some() { value.to_owned() } else { String::new() },
            InputType::Month => if parse_month_string(value).is_some() { value.to_owned() } else { String::new() },
            InputType::Week => if parse_week_string(value).is_some() { value.to_owned() } else { String::new() },
            InputType::Time => if parse_time_string(value).is_some() { value.to_owned() } else { String::new() },
            InputType::DatetimeLocal => normalize_local_datetime(value).unwrap_or_default(),
            _ => value.to_owned(),
        }
    }

    /// A range control's minimum, maximum, allowed value step and step
    /// base (HTML §4.10.5.1.13, §4.10.5.3.7).
    fn range_parameters(&self, id: NodeId) -> RangeParameters {
        let num = |name: &str| self.attr_of(id, name).and_then(parse_float);
        let min = num("min").unwrap_or(0.0);
        let max = num("max").unwrap_or(100.0);
        let step = match self.attr_of(id, "step") {
            Some(s) if s.eq_ignore_ascii_case("any") => None,
            Some(s) => match parse_float(s) {
                Some(v) if v > 0.0 => Some((v, fraction_digits(s))),
                _ => Some((1.0, 0)),
            },
            None => Some((1.0, 0)),
        };
        let base = self
            .attr_of(id, "min")
            .and_then(|s| parse_float(s).map(|v| (v, fraction_digits(s))))
            .or_else(|| self.attr_of(id, "value").and_then(|s| parse_float(s).map(|v| (v, fraction_digits(s)))))
            .unwrap_or((0.0, 0));
        (min, max, step, base)
    }

    /// A range's value sanitization: the default value when not a number,
    /// then clamped to the range and rounded to the step (ties toward
    /// positive infinity).
    fn sanitize_range(&self, id: NodeId, value: &str) -> String {
        let (min, max, step, (base, base_digits)) = self.range_parameters(id);
        let default = if max < min { min } else { min + (max - min) / 2.0 };
        let mut v = if is_valid_float(value) { parse_float(value).unwrap_or(default) } else { default };
        if v < min {
            v = min;
        } else if max >= min && v > max {
            v = max;
        }
        if let Some((step, step_digits)) = step {
            let k = (v - base) / step;
            if (k - k.round()).abs() > 1e-9 {
                let digits = step_digits.max(base_digits);
                let below = round_to(base + k.floor() * step, digits);
                let above = round_to(base + k.ceil() * step, digits);
                let ok = |x: f64| x >= min - 1e-12 && (max < min || x <= max + 1e-12);
                v = match (ok(below), ok(above)) {
                    (true, true) => {
                        // Decimal arithmetic would see a tie where binary
                        // floating point sees a hair's difference.
                        let (d_below, d_above) = (v - below, above - v);
                        if d_below < d_above && (d_above - d_below).abs() > 1e-9 * step.max(1.0) { below } else { above }
                    }
                    (true, false) => below,
                    (false, true) => above,
                    (false, false) => v,
                };
            }
        }
        number_to_string(v)
    }

    /// The value of a control as its `value` IDL attribute reads it: an
    /// input's by its value mode, a textarea's API value, a select's
    /// first selected option's value, an option's value, a button's
    /// `value` attribute, an output's text.
    pub fn control_value(&self, id: NodeId) -> String {
        let Some(name) = self.html_name(id).cloned() else { return String::new() };
        match name {
            local_name!("input") => {
                let ty = self.input_type(id).unwrap_or(InputType::Text);
                match ty.value_mode() {
                    ValueMode::Value => {
                        let raw = match self.control_state(id).and_then(|s| s.raw.as_deref()) {
                            Some(r) => r.to_owned(),
                            None => self.attr_of(id, "value").unwrap_or("").to_owned(),
                        };
                        self.sanitize(id, ty, &raw)
                    }
                    ValueMode::Default => self.attr_of(id, "value").unwrap_or("").to_owned(),
                    ValueMode::DefaultOn => self.attr_of(id, "value").unwrap_or("on").to_owned(),
                    // No file is ever selected (file inputs are a later part).
                    ValueMode::Filename => String::new(),
                }
            }
            local_name!("textarea") => self.textarea_api_value(id),
            local_name!("select") => self
                .list_of_options(id)
                .into_iter()
                .find(|&o| self.option_selectedness(o))
                .map(|o| self.option_value(o))
                .unwrap_or_default(),
            local_name!("option") => self.option_value(id),
            local_name!("button") => self.attr_of(id, "value").unwrap_or("").to_owned(),
            local_name!("output") => self.text_content(id),
            _ => String::new(),
        }
    }

    /// The text a text control shows: what the user typed (a value-mode
    /// input keeps it even while it does not sanitize to a value, as a
    /// browser's editor does), else the value; a textarea's API value.
    pub fn control_display_value(&self, id: NodeId) -> String {
        if self.is_html_named(id, local_name!("input"))
            && let Some(raw) = self.control_state(id).and_then(|s| s.raw.clone())
        {
            return raw;
        }
        self.control_value(id)
    }

    /// Whether the control's dirty value flag is set.
    pub fn value_is_dirty(&self, id: NodeId) -> bool {
        self.control_state(id).is_some_and(|s| s.raw.is_some())
    }

    /// The user typed: the text control's value is `text` (its dirty
    /// value flag set).
    pub fn set_value_by_user(&mut self, id: NodeId, text: &str) {
        self.control_state_mut(id).raw = Some(text.to_owned());
        self.controls_changed.push(id);
    }

    /// The `value` IDL setter (HTML §4.10.5.4, §4.10.11, §4.10.7). Errs
    /// with the `DOMException` name for a file input given a non-empty
    /// value. Returns whether the value changed (the caller moves a text
    /// cursor to the end then).
    pub fn set_value_by_script(&mut self, id: NodeId, value: &str) -> Result<bool, &'static str> {
        let Some(name) = self.html_name(id).cloned() else { return Ok(false) };
        match name {
            local_name!("input") => {
                let ty = self.input_type(id).unwrap_or(InputType::Text);
                match ty.value_mode() {
                    ValueMode::Value => {
                        let old = self.control_display_value(id);
                        let sanitized = self.sanitize(id, ty, value);
                        self.control_state_mut(id).raw = Some(sanitized.clone());
                        let changed = old != sanitized;
                        if changed {
                            self.controls_changed.push(id);
                        }
                        Ok(changed)
                    }
                    ValueMode::Default | ValueMode::DefaultOn => {
                        if let Some(e) = self.nodes[id].as_element_mut() {
                            e.set_attr("value", value);
                        }
                        self.controls_changed.push(id);
                        Ok(true)
                    }
                    ValueMode::Filename => {
                        if value.is_empty() {
                            Ok(false)
                        } else {
                            Err("InvalidStateError")
                        }
                    }
                }
            }
            local_name!("textarea") => {
                let old = self.textarea_api_value(id);
                self.control_state_mut(id).raw = Some(value.to_owned());
                let changed = old != self.textarea_api_value(id);
                if changed {
                    self.controls_changed.push(id);
                }
                Ok(changed)
            }
            local_name!("select") => {
                self.set_select_value(id, value);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// A textarea's raw value: its dirty value, else its child text
    /// content.
    fn textarea_raw_value(&self, id: NodeId) -> String {
        match self.control_state(id).and_then(|s| s.raw.clone()) {
            Some(r) => r,
            None => self.text_content(id),
        }
    }

    /// A textarea's API value: the raw value with CRLF and CR made LF.
    pub fn textarea_api_value(&self, id: NodeId) -> String {
        self.textarea_raw_value(id).replace("\r\n", "\n").replace('\r', "\n")
    }

    /// Whether the user may edit the control: not disabled, and not
    /// `readonly` where that applies (HTML's "mutable").
    pub fn control_is_mutable(&self, id: NodeId) -> bool {
        if self.disabled_state(id) == Some(true) {
            return false;
        }
        let readonly = self.attr_of(id, "readonly").is_some();
        match self.input_type(id) {
            Some(ty) => !(readonly && ty.readonly_applies()),
            None => !(readonly && self.is_html_named(id, local_name!("textarea"))),
        }
    }

    // ----- checkedness -----

    /// An input's checkedness.
    pub fn checkedness(&self, id: NodeId) -> bool {
        match self.control_state(id).and_then(|s| s.checked) {
            Some(c) => c,
            None => self.attr_of(id, "checked").is_some(),
        }
    }

    /// Whether the input's dirty checkedness flag is set.
    pub fn checkedness_is_dirty(&self, id: NodeId) -> bool {
        self.control_state(id).is_some_and(|s| s.dirty_checked)
    }

    /// Set an input's checkedness (`dirty`: by the user or the `checked`
    /// IDL setter). A radio that becomes checked unchecks the rest of its
    /// group.
    pub fn set_checkedness(&mut self, id: NodeId, checked: bool, dirty: bool) {
        let s = self.control_state_mut(id);
        s.checked = Some(checked);
        if dirty {
            s.dirty_checked = true;
        }
        self.controls_changed.push(id);
        if checked && self.input_type(id) == Some(InputType::Radio) {
            self.uncheck_group_except(id);
        }
    }

    pub fn indeterminate(&self, id: NodeId) -> bool {
        self.control_state(id).is_some_and(|s| s.indeterminate)
    }

    pub fn set_indeterminate(&mut self, id: NodeId, on: bool) {
        if self.indeterminate(id) != on {
            self.control_state_mut(id).indeterminate = on;
            self.controls_changed.push(id);
        }
    }

    /// The root of the tree `id` is in.
    fn tree_root(&self, id: NodeId) -> NodeId {
        self.ancestors(id).last().unwrap_or(id)
    }

    /// The other members of a radio's group (HTML §4.10.5.1.18): radios
    /// in the same tree with the same form owner and the same non-empty
    /// name.
    pub fn radio_group(&self, id: NodeId) -> Vec<NodeId> {
        let Some(name) = self.attr_of(id, "name").filter(|n| !n.is_empty()).map(str::to_owned) else {
            return Vec::new();
        };
        let owner = self.form_owner(id);
        let root = self.tree_root(id);
        self.descendants(root)
            .filter(|&n| {
                n != id
                    && self.input_type(n) == Some(InputType::Radio)
                    && self.attr_of(n, "name") == Some(name.as_str())
                    && self.form_owner(n) == owner
            })
            .collect()
    }

    fn uncheck_group_except(&mut self, id: NodeId) {
        for other in self.radio_group(id) {
            if self.checkedness(other) {
                self.control_state_mut(other).checked = Some(false);
                self.controls_changed.push(other);
            }
        }
    }

    // ----- legacy activation (HTML §4.10.5.1.17, §4.10.5.1.18) -----

    /// What activation does for an element that has activation behavior
    /// among the form controls, `None` for one that has none.
    pub fn has_form_activation(&self, id: NodeId) -> bool {
        match self.input_type(id) {
            Some(ty) => matches!(
                ty,
                InputType::Checkbox | InputType::Radio | InputType::Submit | InputType::Image | InputType::Reset | InputType::Button
            ),
            None => self.is_html_named(id, local_name!("button")),
        }
    }

    /// Legacy-pre-activation behavior: a checkbox flips (and stops being
    /// indeterminate), a radio becomes checked.
    pub fn pre_activate(&mut self, id: NodeId) -> PreActivation {
        match self.input_type(id) {
            Some(InputType::Checkbox) if self.control_is_mutable_check(id) => {
                let saved = PreActivation::Checkbox {
                    checked: self.checkedness(id),
                    indeterminate: self.indeterminate(id),
                    state_existed: self.control_state(id).is_some_and(|s| s.checked.is_some()),
                    dirty: self.checkedness_is_dirty(id),
                };
                let now = !self.checkedness(id);
                self.set_checkedness(id, now, true);
                self.set_indeterminate(id, false);
                saved
            }
            Some(InputType::Radio) if self.control_is_mutable_check(id) => {
                let previous = self.radio_group(id).into_iter().find(|&r| self.checkedness(r));
                let was_checked = self.checkedness(id);
                self.set_checkedness(id, true, true);
                PreActivation::Radio { previous, was_checked }
            }
            _ => PreActivation::None,
        }
    }

    /// Checkboxes and radios are mutable unless disabled (`readonly` does
    /// not apply to them).
    fn control_is_mutable_check(&self, id: NodeId) -> bool {
        self.disabled_state(id) != Some(true)
    }

    /// Legacy-canceled-activation behavior: the `click` was cancelled, so
    /// what pre-activation did is undone.
    pub fn cancel_activation(&mut self, id: NodeId, saved: PreActivation) {
        match saved {
            PreActivation::Checkbox { checked, indeterminate, state_existed, dirty } => {
                let s = self.control_state_mut(id);
                s.checked = state_existed.then_some(checked);
                s.dirty_checked = dirty;
                s.indeterminate = indeterminate;
                self.controls_changed.push(id);
            }
            PreActivation::Radio { previous, was_checked } => {
                let still = previous.filter(|&p| self.contains(p) && self.radio_group(id).contains(&p));
                match still {
                    Some(p) => self.set_checkedness(p, true, false),
                    None => {
                        if !was_checked {
                            self.control_state_mut(id).checked = Some(false);
                            self.controls_changed.push(id);
                        }
                    }
                }
            }
            PreActivation::None => {}
        }
    }

    /// The activation behavior of a form control (HTML §4.10.5 "input
    /// activation behavior", §4.10.6 for `button`), for the caller to
    /// carry out.
    pub fn activation(&self, id: NodeId) -> Activation {
        if let Some(ty) = self.input_type(id) {
            if self.disabled_state(id) == Some(true) && !matches!(ty, InputType::Checkbox | InputType::Radio) {
                return Activation::Nothing;
            }
            return match ty {
                InputType::Checkbox | InputType::Radio => {
                    if self.is_connected(id) && self.disabled_state(id) != Some(true) {
                        Activation::Changed
                    } else {
                        Activation::Nothing
                    }
                }
                InputType::Submit | InputType::Image => match self.form_owner(id) {
                    Some(form) => Activation::Submit { form },
                    None => Activation::Nothing,
                },
                InputType::Reset => match self.form_owner(id) {
                    Some(form) => Activation::Reset { form },
                    None => Activation::Nothing,
                },
                _ => Activation::Nothing,
            };
        }
        if self.is_html_named(id, local_name!("button")) {
            if self.disabled_state(id) == Some(true) {
                return Activation::Nothing;
            }
            let Some(form) = self.form_owner(id) else { return Activation::Nothing };
            return match self.button_type(id) {
                "submit" => Activation::Submit { form },
                "reset" => Activation::Reset { form },
                _ => Activation::Nothing,
            };
        }
        Activation::Nothing
    }

    /// A `<button>`'s type state: `submit` (missing or invalid), `reset`
    /// or `button`.
    pub fn button_type(&self, id: NodeId) -> &'static str {
        match self.attr_of(id, "type").map(str::to_ascii_lowercase).as_deref() {
            Some("reset") => "reset",
            Some("button") => "button",
            _ => "submit",
        }
    }

    /// Whether `id` is a submit button: a `<button>` in the Submit Button
    /// state, or an input of type `submit` or `image`.
    pub fn is_submit_button(&self, id: NodeId) -> bool {
        match self.input_type(id) {
            Some(ty) => matches!(ty, InputType::Submit | InputType::Image),
            None => self.is_html_named(id, local_name!("button")) && self.button_type(id) == "submit",
        }
    }

    fn is_button(&self, id: NodeId) -> bool {
        match self.input_type(id) {
            Some(ty) => matches!(ty, InputType::Submit | InputType::Image | InputType::Reset | InputType::Button),
            None => self.is_html_named(id, local_name!("button")),
        }
    }

    // ----- select and option -----

    /// A select's list of options (HTML §4.10.7): its option descendants
    /// in tree order, not looking inside a nested `select`, `hr`,
    /// `option`, `datalist`, or an `optgroup` within an `optgroup`.
    pub fn list_of_options(&self, select: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_options(select, false, &mut out);
        out
    }

    fn collect_options(&self, parent: NodeId, in_optgroup: bool, out: &mut Vec<NodeId>) {
        for c in self.children(parent) {
            let Some(name) = self.html_name(c).cloned() else {
                // Elements in other namespaces are walked through.
                if self.nodes[c].is_element() {
                    self.collect_options(c, in_optgroup, out);
                }
                continue;
            };
            match name {
                local_name!("option") => out.push(c),
                local_name!("select") | local_name!("hr") | local_name!("datalist") => {}
                local_name!("optgroup") if in_optgroup => {}
                local_name!("optgroup") => self.collect_options(c, true, out),
                _ => self.collect_options(c, in_optgroup, out),
            }
        }
    }

    /// The nearest `select` an option (or its parent) belongs to: its
    /// nearest ancestor select, unless an `hr`, `datalist` or `option`
    /// comes first, or an `optgroup` nested in an `optgroup`.
    fn owning_select(&self, node: NodeId) -> Option<NodeId> {
        let mut optgroups = 0;
        for a in std::iter::once(node).chain(self.ancestors(node)) {
            match self.html_name(a).cloned() {
                Some(local_name!("select")) => return (optgroups <= 1).then_some(a),
                Some(local_name!("hr") | local_name!("datalist")) if a != node => return None,
                Some(local_name!("option")) if a != node => return None,
                Some(local_name!("optgroup")) => optgroups += 1,
                _ => {}
            }
        }
        None
    }

    /// An option's selectedness.
    pub fn option_selectedness(&self, id: NodeId) -> bool {
        match self.control_state(id).and_then(|s| s.selected) {
            Some(s) => s,
            None => self.attr_of(id, "selected").is_some(),
        }
    }

    /// Set an option's selectedness (`dirty`: its dirtiness becomes true).
    /// The option's select runs its selectedness setting algorithm when
    /// `settle` (the `selected` IDL setter).
    pub fn set_option_selectedness(&mut self, id: NodeId, selected: bool, dirty: bool, settle: bool) {
        let s = self.control_state_mut(id);
        s.selected = Some(selected);
        if dirty {
            s.dirty_selected = true;
        }
        if let Some(select) = self.owning_select(id) {
            self.controls_changed.push(select);
            if settle {
                // "If the option was selected" in a single select, the
                // others are unselected by the algorithm (it keeps the
                // last), so an explicitly selected earlier option must
                // win: unselect the rest first.
                if selected && self.attr_of(select, "multiple").is_none() {
                    for o in self.list_of_options(select) {
                        if o != id && self.option_selectedness(o) {
                            self.control_state_mut(o).selected = Some(false);
                        }
                    }
                }
                self.selectedness_setting(select);
            }
        }
        self.controls_changed.push(id);
    }

    /// An option's value: its `value` attribute, else its text with
    /// white space collapsed.
    pub fn option_value(&self, id: NodeId) -> String {
        match self.attr_of(id, "value") {
            Some(v) => v.to_owned(),
            None => self.option_text(id),
        }
    }

    /// An option's `text`: its text content (not `script` or SVG
    /// `script` contents) with ASCII white space stripped and collapsed.
    pub fn option_text(&self, id: NodeId) -> String {
        let mut text = String::new();
        let mut stack: Vec<NodeId> = self.children(id).collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            match &self.nodes[n].kind {
                NodeKind::Text(t) => text.push_str(t),
                NodeKind::Element(e) if !matches!(&*e.name.local, "script") => {
                    let kids: Vec<NodeId> = self.children(n).collect();
                    stack.extend(kids.into_iter().rev());
                }
                _ => {}
            }
        }
        text.split(['\t', '\n', '\x0C', '\r', ' ']).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
    }

    /// An option's label: its `label` attribute when non-empty... as the
    /// select shows it: the label attribute if present and not empty,
    /// else its text.
    pub fn option_label(&self, id: NodeId) -> String {
        match self.attr_of(id, "label").filter(|l| !l.is_empty()) {
            Some(l) => l.to_owned(),
            None => self.option_text(id),
        }
    }

    /// Whether an option is disabled (HTML §4.10.10): its own attribute,
    /// or an `optgroup` ancestor's before a `select`, `hr`, `datalist` or
    /// `option`.
    pub fn option_disabled(&self, id: NodeId) -> bool {
        if self.attr_of(id, "disabled").is_some() {
            return true;
        }
        for a in self.ancestors(id) {
            match self.html_name(a).cloned() {
                Some(local_name!("select") | local_name!("hr") | local_name!("datalist") | local_name!("option")) => return false,
                Some(local_name!("optgroup")) if self.attr_of(a, "disabled").is_some() => return true,
                _ => {}
            }
        }
        false
    }

    /// A select's display size (HTML §4.10.7).
    pub fn display_size(&self, select: NodeId) -> u64 {
        match self.attr_of(select, "size").and_then(crate::parse_html_integer) {
            Some(n) if n >= 0 => n as u64,
            _ => {
                if self.attr_of(select, "multiple").is_some() {
                    4
                } else {
                    1
                }
            }
        }
    }

    /// The selectedness setting algorithm (HTML §4.10.7). True when it
    /// changed something.
    pub fn selectedness_setting(&mut self, select: NodeId) -> bool {
        if self.attr_of(select, "multiple").is_some() {
            return false;
        }
        let options = self.list_of_options(select);
        let selected: Vec<NodeId> = options.iter().copied().filter(|&o| self.option_selectedness(o)).collect();
        let mut changed = false;
        if selected.len() > 1 {
            for &o in &selected[..selected.len() - 1] {
                self.control_state_mut(o).selected = Some(false);
            }
            changed = true;
        }
        if selected.is_empty()
            && self.display_size(select) == 1
            && let Some(first) = options.iter().copied().find(|&o| !self.option_disabled(o))
        {
            self.control_state_mut(first).selected = Some(true);
            changed = true;
        }
        if changed {
            self.controls_changed.push(select);
        }
        changed
    }

    /// `select.selectedIndex`.
    pub fn selected_index(&self, select: NodeId) -> i64 {
        self.list_of_options(select)
            .into_iter()
            .position(|o| self.option_selectedness(o))
            .map_or(-1, |i| i as i64)
    }

    /// `selectedIndex = `: every option unselected, the one at `index`
    /// selected and dirty.
    pub fn set_selected_index(&mut self, select: NodeId, index: i64) {
        let options = self.list_of_options(select);
        for &o in &options {
            self.control_state_mut(o).selected = Some(false);
        }
        if index >= 0
            && let Some(&o) = options.get(index as usize)
        {
            let s = self.control_state_mut(o);
            s.selected = Some(true);
            s.dirty_selected = true;
        }
        self.controls_changed.push(select);
    }

    /// `select.value = `: the first option of that value selected.
    pub fn set_select_value(&mut self, select: NodeId, value: &str) {
        let options = self.list_of_options(select);
        let mut found = false;
        for &o in &options {
            let hit = !found && self.option_value(o) == value;
            found |= hit;
            let s = self.control_state_mut(o);
            s.selected = Some(hit);
            if hit {
                s.dirty_selected = true;
            }
        }
        self.controls_changed.push(select);
    }

    /// The user picked an option of a drop-down select: it is selected and
    /// dirty, the others not.
    pub fn pick_option(&mut self, select: NodeId, option: NodeId) {
        for o in self.list_of_options(select) {
            let s = self.control_state_mut(o);
            s.selected = Some(o == option);
            if o == option {
                s.dirty_selected = true;
            }
        }
        self.controls_changed.push(select);
    }

    /// Run what tree changes queued: each select whose options changed
    /// runs its selectedness setting algorithm, each radio that became
    /// connected checked unchecks the rest of its group.
    pub fn settle_forms(&mut self) {
        if self.pending_selects.is_empty() && self.pending_radios.is_empty() {
            return;
        }
        let selects = std::mem::take(&mut self.pending_selects);
        let mut done: Vec<NodeId> = Vec::new();
        for s in selects {
            if !self.contains(s) || done.contains(&s) {
                continue;
            }
            done.push(s);
            self.selectedness_setting(s);
            self.controls_changed.push(s);
        }
        // Latest first: each radio inserted checked unchecked the ones
        // before it, so the last one of a group is what stays checked.
        let radios = std::mem::take(&mut self.pending_radios);
        for r in radios.into_iter().rev() {
            if self.contains(r) && self.input_type(r) == Some(InputType::Radio) && self.checkedness(r) {
                self.uncheck_group_except(r);
            }
        }
    }

    /// The controls whose shown state changed since the last call (their
    /// boxes and the selectors on their state need recomputing).
    pub fn take_controls_changed(&mut self) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        for id in std::mem::take(&mut self.controls_changed) {
            if self.contains(id) && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    // ----- form owner (HTML §4.10.18.3) -----

    /// Whether `id` is a listed element: `button`, `fieldset`, `input`,
    /// `object`, `output`, `select`, `textarea`.
    pub fn is_listed(&self, id: NodeId) -> bool {
        matches!(
            self.html_name(id).map(|n| &**n),
            Some("button" | "fieldset" | "input" | "object" | "output" | "select" | "textarea")
        )
    }

    /// Whether `id` is form-associated (listed, or `img`).
    pub fn is_form_associated(&self, id: NodeId) -> bool {
        self.is_listed(id) || self.is_html_named(id, local_name!("img"))
    }

    /// Whether `id` is a submittable element.
    pub fn is_submittable(&self, id: NodeId) -> bool {
        matches!(self.html_name(id).map(|n| &**n), Some("button" | "input" | "select" | "textarea"))
    }

    /// Whether `id` is a resettable element.
    pub fn is_resettable(&self, id: NodeId) -> bool {
        matches!(self.html_name(id).map(|n| &**n), Some("input" | "output" | "select" | "textarea"))
    }

    /// The parser associated `element` with `form` (its form element
    /// pointer, HTML §13.2.6.1 "create an element for a token").
    pub fn associate_with_form(&mut self, element: NodeId, form: NodeId) {
        if self.is_form_associated(element) {
            self.parser_forms.insert(element, form);
        }
    }

    /// An element's form owner: the form its `form` attribute names (a
    /// listed element), else the parser's association while the element
    /// stays where the parser put it, else its nearest ancestor form.
    pub fn form_owner(&self, id: NodeId) -> Option<NodeId> {
        if !self.is_form_associated(id) {
            return None;
        }
        if self.is_listed(id)
            && let Some(target) = self.attr_of(id, "form")
        {
            if !self.is_connected(id) {
                return None;
            }
            let found = self.descendants(self.root).find(|&n| self.nodes[n].as_element().and_then(|e| e.id()) == Some(target));
            return found.filter(|&f| self.is_html_named(f, local_name!("form")));
        }
        if let Some(&form) = self.parser_forms.get(&id)
            && self.contains(form)
            && self.is_html_named(form, local_name!("form"))
            && self.tree_root(form) == self.tree_root(id)
        {
            return Some(form);
        }
        self.ancestors(id).find(|&a| self.is_html_named(a, local_name!("form")))
    }

    /// The listed elements whose form owner is `form`, in tree order (the
    /// form's `elements`, image buttons included; `form.elements` leaves
    /// them out itself).
    pub fn form_listed_elements(&self, form: NodeId) -> Vec<NodeId> {
        let root = self.tree_root(form);
        self.descendants(root).filter(|&n| self.is_listed(n) && self.form_owner(n) == Some(form)).collect()
    }

    /// The elements `form.elements` lists: the listed elements of the
    /// form except image buttons.
    pub fn form_elements(&self, form: NodeId) -> Vec<NodeId> {
        self.form_listed_elements(form)
            .into_iter()
            .filter(|&n| self.input_type(n) != Some(InputType::Image))
            .collect()
    }

    /// The form's default button: its first submit button in tree order.
    pub fn default_button(&self, form: NodeId) -> Option<NodeId> {
        self.form_listed_elements(form).into_iter().find(|&n| self.is_submit_button(n))
    }

    /// How many of the form's fields block implicit submission.
    pub fn implicit_submission_blockers(&self, form: NodeId) -> usize {
        self.form_listed_elements(form)
            .into_iter()
            .filter(|&n| self.input_type(n).is_some_and(InputType::blocks_implicit_submission))
            .count()
    }

    // ----- reset (HTML §4.10.23) -----

    /// The resettable elements whose form owner is `form`, in tree order.
    pub fn form_resettable_elements(&self, form: NodeId) -> Vec<NodeId> {
        let root = self.tree_root(form);
        self.descendants(root).filter(|&n| self.is_resettable(n) && self.form_owner(n) == Some(form)).collect()
    }

    /// A control's reset algorithm.
    pub fn reset_control(&mut self, id: NodeId) {
        let Some(name) = self.html_name(id).cloned() else { return };
        match name {
            local_name!("input") | local_name!("textarea") => {
                let indeterminate = self.indeterminate(id);
                let s = self.control_state_mut(id);
                s.raw = None;
                s.checked = None;
                s.dirty_checked = false;
                s.indeterminate = indeterminate;
                self.controls_changed.push(id);
                if self.input_type(id) == Some(InputType::Radio) && self.checkedness(id) {
                    self.uncheck_group_except(id);
                }
            }
            local_name!("select") => {
                for o in self.list_of_options(id) {
                    let s = self.control_state_mut(o);
                    s.selected = None;
                    s.dirty_selected = false;
                }
                self.selectedness_setting(id);
                self.controls_changed.push(id);
            }
            local_name!("output") => {
                // The default value override is not kept (no `output.value`
                // setter yet): its text is its default.
            }
            _ => {}
        }
    }

    // ----- tree and attribute hooks -----

    /// A node was placed under `parent` (by the parser or a script):
    /// options change their select's list, a checked radio that became
    /// connected settles its group, a parser association no longer
    /// holds for a node a script moved.
    pub(crate) fn forms_inserted(&mut self, node: NodeId, parent: NodeId) {
        if !self.forms_seen {
            return;
        }
        let mut has_option = false;
        let mut radios = Vec::new();
        for n in std::iter::once(node).chain(self.descendants(node)) {
            match self.html_name(n).map(|l| &**l) {
                Some("option" | "optgroup") => has_option = true,
                Some("input") if self.input_type(n) == Some(InputType::Radio) => radios.push(n),
                _ => {}
            }
        }
        if has_option && let Some(s) = self.owning_select(parent) {
            self.pending_selects.push(s);
        }
        if self.is_connected(parent) {
            self.pending_radios.extend(radios);
        }
    }

    /// `node` left `parent`.
    pub(crate) fn forms_removed(&mut self, node: NodeId, parent: NodeId) {
        if !self.forms_seen {
            return;
        }
        let has_option = std::iter::once(node)
            .chain(self.descendants(node))
            .any(|n| matches!(self.html_name(n).map(|l| &**l), Some("option" | "optgroup")));
        if has_option && let Some(s) = self.owning_select(parent) {
            self.pending_selects.push(s);
        }
        // A removed element loses what the parser associated it with
        // (its removal steps reset its form owner).
        if !self.parser_forms.is_empty() {
            let gone: Vec<NodeId> = std::iter::once(node).chain(self.descendants(node)).filter(|n| self.parser_forms.contains_key(n)).collect();
            for g in gone {
                self.parser_forms.remove(&g);
            }
        }
    }

    /// An attribute of `element` was set, changed or removed by a script
    /// (`old` its value before): the controls react as HTML says.
    pub fn note_form_attribute(&mut self, element: NodeId, name: &str, old: Option<&str>) {
        let Some(local) = self.html_name(element).cloned() else { return };
        match (&*local, name) {
            ("input", "type") => {
                let old_ty = InputType::from_attr(old);
                let new_ty = self.input_type(element).unwrap_or(InputType::Text);
                if old_ty != new_ty {
                    self.input_type_changed(element, old_ty, new_ty);
                }
            }
            ("input", "checked") => {
                if !self.checkedness_is_dirty(element) {
                    let s = self.control_state_mut(element);
                    s.checked = None;
                    self.controls_changed.push(element);
                    if self.input_type(element) == Some(InputType::Radio) && self.checkedness(element) {
                        self.uncheck_group_except(element);
                    }
                }
            }
            ("input", "name" | "form") => {
                if name == "form" {
                    self.parser_forms.remove(&element);
                }
                if self.input_type(element) == Some(InputType::Radio) && self.checkedness(element) {
                    self.uncheck_group_except(element);
                }
            }
            ("input", "value" | "min" | "max" | "step" | "multiple") => self.controls_changed.push(element),
            ("option", "selected") => {
                let dirty = self.control_state(element).is_some_and(|s| s.dirty_selected);
                if !dirty {
                    if let Some(s) = self.controls.get_mut(&element) {
                        s.selected = None;
                    }
                    if let Some(select) = self.owning_select(element) {
                        // An added `selected` in a single select: the
                        // algorithm keeps the last selected option, so
                        // this one must be the only one first.
                        if self.attr_of(element, "selected").is_some() && self.attr_of(select, "multiple").is_none() {
                            for o in self.list_of_options(select) {
                                if o != element && self.option_selectedness(o) {
                                    self.control_state_mut(o).selected = Some(false);
                                }
                            }
                        }
                        self.selectedness_setting(select);
                        self.controls_changed.push(select);
                    }
                }
            }
            ("option", "disabled" | "value" | "label") | ("optgroup", "disabled") => {
                if let Some(select) = self.owning_select(element) {
                    if name == "disabled" {
                        self.selectedness_setting(select);
                    }
                    self.controls_changed.push(select);
                }
            }
            ("select", "multiple") => {
                if old.is_some() && self.attr_of(element, "multiple").is_none() {
                    // Removed: all but the first selected option go.
                    let mut first = true;
                    for o in self.list_of_options(element) {
                        if self.option_selectedness(o) {
                            if !first {
                                self.control_state_mut(o).selected = Some(false);
                            }
                            first = false;
                        }
                    }
                }
                self.selectedness_setting(element);
                self.controls_changed.push(element);
            }
            ("select", "size") => {
                self.selectedness_setting(element);
                self.controls_changed.push(element);
            }
            (_, "form") => {
                self.parser_forms.remove(&element);
            }
            _ => {}
        }
    }

    /// The `type` attribute changed (HTML §4.10.5 "type attribute change
    /// steps").
    fn input_type_changed(&mut self, id: NodeId, old: InputType, new: InputType) {
        let (om, nm) = (old.value_mode(), new.value_mode());
        if om == ValueMode::Value && matches!(nm, ValueMode::Default | ValueMode::DefaultOn) {
            // The value so far becomes the `value` attribute (when there
            // is one other than the empty string).
            let value = match self.control_state(id).and_then(|s| s.raw.clone()) {
                Some(raw) => self.sanitize(id, old, &raw),
                None => self.sanitize(id, old, self.attr_of(id, "value").unwrap_or("")),
            };
            if !value.is_empty()
                && let Some(e) = self.nodes[id].as_element_mut()
            {
                e.set_attr("value", &value);
            }
            self.control_state_mut(id).raw = None;
        } else if (om != ValueMode::Value && nm == ValueMode::Value) || (om != ValueMode::Filename && nm == ValueMode::Filename) {
            // Into the value mode the value is the attribute again (the dirty
            // flag false); into the filename mode it is empty.
            self.control_state_mut(id).raw = None;
        } else if nm == ValueMode::Value
            && let Some(raw) = self.control_state(id).and_then(|s| s.raw.clone())
        {
            // Sanitized for the new type, for good.
            let v = self.sanitize(id, new, &raw);
            self.control_state_mut(id).raw = Some(v);
        }
        if new == InputType::Radio && self.checkedness(id) {
            self.uncheck_group_except(id);
        }
        self.controls_changed.push(id);
    }

    /// An element was created: the document notes that forms are in use
    /// (the hooks above cost nothing until then).
    pub(crate) fn note_created_element(&mut self, local: &LocalName) {
        if !self.forms_seen
            && matches!(&**local, "input" | "select" | "option" | "optgroup" | "textarea" | "form" | "button")
        {
            self.forms_seen = true;
        }
    }

    /// The cloning steps of `input` and `textarea`: the copy takes the
    /// original's value, dirty flag, checkedness and indeterminateness.
    pub(crate) fn clone_control_state(&mut self, from: NodeId, to: NodeId) {
        let Some(name) = self.html_name(from).cloned() else { return };
        if !matches!(&*name, "input" | "textarea") {
            return;
        }
        if let Some(s) = self.controls.get(&from).cloned() {
            self.controls.insert(
                to,
                ControlState {
                    selected: None,
                    dirty_selected: false,
                    ..s
                },
            );
        }
    }

    // ----- submission (HTML §4.10.22) -----

    /// The encoding a form submits with: the first of its
    /// `accept-charset` labels that names an encoding, else the
    /// document's; as its output encoding (UTF-16 gives UTF-8).
    pub fn form_encoding(&self, form: NodeId) -> &'static encoding_rs::Encoding {
        let doc = self.encoding.unwrap_or(encoding_rs::UTF_8);
        let picked = match self.attr_of(form, "accept-charset") {
            Some(list) => {
                let found = list
                    .split(['\t', '\n', '\x0C', '\r', ' '])
                    .filter(|t| !t.is_empty())
                    .find_map(|t| encoding_rs::Encoding::for_label(t.as_bytes()));
                found.unwrap_or(encoding_rs::UTF_8)
            }
            None => doc,
        };
        picked.output_encoding()
    }

    /// An attribute of the submitter (when a submit button that has it)
    /// or else of the form.
    fn submitter_attr(&self, form: NodeId, submitter: Option<NodeId>, own: &str, form_attr: &str) -> Option<String> {
        if let Some(s) = submitter.filter(|&s| s != form && self.is_submit_button(s))
            && let Some(v) = self.attr_of(s, own)
        {
            return Some(v.to_owned());
        }
        self.attr_of(form, form_attr).map(str::to_owned)
    }

    /// The submitter's method.
    pub fn form_method(&self, form: NodeId, submitter: Option<NodeId>) -> FormMethod {
        match self.submitter_attr(form, submitter, "formmethod", "method").map(|m| m.to_ascii_lowercase()).as_deref() {
            Some("post") => FormMethod::Post,
            Some("dialog") => FormMethod::Dialog,
            _ => FormMethod::Get,
        }
    }

    /// The submitter's enctype.
    pub fn form_enctype(&self, form: NodeId, submitter: Option<NodeId>) -> Enctype {
        match self.submitter_attr(form, submitter, "formenctype", "enctype").map(|m| m.to_ascii_lowercase()).as_deref() {
            Some("multipart/form-data") => Enctype::Multipart,
            Some("text/plain") => Enctype::TextPlain,
            _ => Enctype::UrlEncoded,
        }
    }

    /// The submitter's no-validate state.
    pub fn form_no_validate(&self, form: NodeId, submitter: Option<NodeId>) -> bool {
        submitter.is_some_and(|s| s != form && self.is_submit_button(s) && self.attr_of(s, "formnovalidate").is_some())
            || self.attr_of(form, "novalidate").is_some()
    }

    /// The target a submission goes to: the submitter's `formtarget`, the
    /// form's `target`, else the document's `<base target>`.
    pub fn form_target(&self, form: NodeId, submitter: Option<NodeId>) -> String {
        if let Some(s) = submitter.filter(|&s| s != form && self.is_submit_button(s))
            && let Some(t) = self.attr_of(s, "formtarget")
        {
            return t.to_owned();
        }
        if let Some(t) = self.attr_of(form, "target") {
            return t.to_owned();
        }
        self.descendants(self.root)
            .find(|&n| self.is_html_named(n, local_name!("base")) && self.attr_of(n, "target").is_some())
            .and_then(|b| self.attr_of(b, "target"))
            .unwrap_or("")
            .to_owned()
    }

    /// Construct the entry list (HTML §4.10.22.4, without the `formdata`
    /// event, which the script bindings fire over the result).
    /// `coordinate` is an image button submitter's selected coordinate.
    pub fn construct_entry_list(
        &self,
        form: NodeId,
        submitter: Option<NodeId>,
        encoding: &'static encoding_rs::Encoding,
        coordinate: (i32, i32),
    ) -> Vec<Entry> {
        let mut entries = Vec::new();
        let root = self.tree_root(form);
        let fields: Vec<NodeId> =
            self.descendants(root).filter(|&n| self.is_submittable(n) && self.form_owner(n) == Some(form)).collect();
        let text = |name: &str, value: String| Entry { name: name.to_owned(), value: EntryValue::Text(value) };
        for field in fields {
            if self.ancestors(field).any(|a| self.is_html_named(a, local_name!("datalist"))) {
                continue;
            }
            if self.disabled_state(field) == Some(true) {
                continue;
            }
            if self.is_button(field) && Some(field) != submitter {
                continue;
            }
            let ty = self.input_type(field);
            if matches!(ty, Some(InputType::Checkbox | InputType::Radio)) && !self.checkedness(field) {
                continue;
            }
            if ty == Some(InputType::Image) {
                let name = match self.attr_of(field, "name").filter(|n| !n.is_empty()) {
                    Some(n) => format!("{n}."),
                    None => String::new(),
                };
                entries.push(text(&format!("{name}x"), coordinate.0.to_string()));
                entries.push(text(&format!("{name}y"), coordinate.1.to_string()));
                continue;
            }
            let Some(name) = self.attr_of(field, "name").filter(|n| !n.is_empty()).map(str::to_owned) else {
                continue;
            };
            if self.is_html_named(field, local_name!("select")) {
                for o in self.list_of_options(field) {
                    if self.option_selectedness(o) && !self.option_disabled(o) {
                        entries.push(text(&name, self.option_value(o)));
                    }
                }
            } else if matches!(ty, Some(InputType::Checkbox | InputType::Radio)) {
                entries.push(text(&name, self.attr_of(field, "value").unwrap_or("on").to_owned()));
            } else if ty == Some(InputType::File) {
                entries.push(Entry {
                    name: name.clone(),
                    value: EntryValue::File {
                        name: String::new(),
                        mime: "application/octet-stream".to_owned(),
                        bytes: Vec::new(),
                    },
                });
            } else if ty == Some(InputType::Hidden) && name.eq_ignore_ascii_case("_charset_") {
                entries.push(text(&name, encoding.name().to_owned()));
            } else if self.is_html_named(field, local_name!("textarea")) {
                // The element's value (the API value; `wrap=hard` inserts
                // no breaks here).
                entries.push(text(&name, self.textarea_api_value(field)));
            } else {
                entries.push(text(&name, self.control_value(field)));
            }
            // `dirname`: the field's directionality as another entry.
            if let Some(dirname) = self.attr_of(field, "dirname").filter(|d| !d.is_empty())
                && self.auto_directionality_control(field)
            {
                let dir = match self.directionality(field) {
                    Directionality::Ltr => "ltr",
                    Directionality::Rtl => "rtl",
                };
                entries.push(text(dirname, dir.to_owned()));
            }
        }
        entries
    }

    /// Auto-directionality form-associated elements: textareas and text,
    /// search, tel, url, email, password, submit, reset, button and
    /// hidden inputs.
    fn auto_directionality_control(&self, id: NodeId) -> bool {
        match self.input_type(id) {
            Some(ty) => matches!(
                ty,
                InputType::Hidden
                    | InputType::Text
                    | InputType::Search
                    | InputType::Tel
                    | InputType::Url
                    | InputType::Email
                    | InputType::Password
                    | InputType::Submit
                    | InputType::Reset
                    | InputType::Button
            ),
            None => self.is_html_named(id, local_name!("textarea")),
        }
    }

    /// What a submission does after its entry list is made (HTML
    /// §4.10.22.3 steps 10 to 21); `None` when it does nothing (no
    /// dialog for `method=dialog`, an action that does not parse).
    pub fn submission_action(
        &self,
        form: NodeId,
        submitter: Option<NodeId>,
        entries: &[Entry],
        encoding: &'static encoding_rs::Encoding,
        coordinate: Option<(i32, i32)>,
        boundary: &str,
    ) -> Option<SubmitAction> {
        let method = self.form_method(form, submitter);
        if method == FormMethod::Dialog {
            let dialog = self.ancestors(form).find(|&a| self.is_html_named(a, local_name!("dialog")))?;
            let result = match submitter {
                Some(s) if self.input_type(s) == Some(InputType::Image) => {
                    let (x, y) = coordinate.unwrap_or((0, 0));
                    Some(format!("{x},{y}"))
                }
                Some(s) if s != form && self.is_submit_button(s) => self.attr_of(s, "value").map(str::to_owned),
                _ => None,
            };
            return Some(SubmitAction::CloseDialog { dialog, result });
        }
        let action = self
            .submitter_attr(form, submitter, "formaction", "action")
            .filter(|a| !trim_ascii_ws(a).is_empty());
        let mut url = match action {
            Some(a) => self.resolve_url(&a)?,
            None => self.base_url.clone()?,
        };
        let enctype = self.form_enctype(form, submitter);
        let target = self.form_target(form, submitter);
        let noreferrer = self
            .attr_of(form, "rel")
            .is_some_and(|r| r.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("noreferrer")));
        let pairs = || name_value_pairs(entries);
        let post = match (url.scheme(), method) {
            ("http" | "https" | "data", FormMethod::Get) => {
                url.set_query(Some(&urlencoded(&pairs(), encoding)));
                None
            }
            ("http" | "https", _) => Some(match enctype {
                Enctype::UrlEncoded => PostResource {
                    content_type: "application/x-www-form-urlencoded".to_owned(),
                    body: urlencoded(&pairs(), encoding).into_bytes(),
                },
                Enctype::Multipart => PostResource {
                    content_type: format!("multipart/form-data; boundary={boundary}"),
                    body: multipart(entries, encoding, boundary),
                },
                Enctype::TextPlain => PostResource {
                    content_type: "text/plain".to_owned(),
                    body: encode(&text_plain(&pairs()), encoding),
                },
            }),
            ("mailto", FormMethod::Get) => {
                url.set_query(Some(&urlencoded(&pairs(), encoding).replace('+', "%20")));
                None
            }
            ("mailto", _) => {
                let body = match enctype {
                    Enctype::TextPlain => percent_encode_default(&text_plain(&pairs())),
                    _ => urlencoded(&pairs(), encoding),
                };
                let mut query = url.query().unwrap_or("").to_owned();
                if !query.is_empty() {
                    query.push('&');
                }
                query.push_str("body=");
                query.push_str(&body);
                url.set_query(Some(&query));
                None
            }
            // ftp, javascript, data POST and other schemes: get the action
            // URL.
            _ => None,
        };
        Some(SubmitAction::Navigate { url, post, target, noreferrer })
    }
}

// ----- encodings -----

/// Name-value pairs of an entry list (HTML §4.10.22.6): newlines made
/// CRLF in names and values, a file by its name.
pub fn name_value_pairs(entries: &[Entry]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|e| {
            let value = match &e.value {
                EntryValue::Text(t) => t.clone(),
                EntryValue::File { name, .. } => name.clone(),
            };
            (to_crlf(&e.name), to_crlf(&value))
        })
        .collect()
}

/// Every lone CR, lone LF and CRLF as CRLF.
pub fn to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            c => out.push(c),
        }
    }
    out
}

/// `text` in `encoding`, characters it cannot hold as HTML numeric
/// character references (the encoding standard's "html" error mode).
pub fn encode(text: &str, encoding: &'static encoding_rs::Encoding) -> Vec<u8> {
    if encoding == encoding_rs::UTF_8 {
        return text.as_bytes().to_vec();
    }
    let (bytes, _, _) = encoding.encode(text);
    bytes.into_owned()
}

/// The `application/x-www-form-urlencoded` serializer (URL Standard
/// §5.2): each name and value encoded, then percent-encoded with the
/// form-urlencoded set, a space as `+`, pairs joined by `&`.
pub fn urlencoded(pairs: &[(String, String)], encoding: &'static encoding_rs::Encoding) -> String {
    let mut out = String::new();
    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        form_urlencode_bytes(&encode(name, encoding), &mut out);
        out.push('=');
        form_urlencode_bytes(&encode(value, encoding), &mut out);
    }
    out
}

fn form_urlencode_bytes(bytes: &[u8], out: &mut String) {
    for &b in bytes {
        match b {
            b' ' => out.push('+'),
            b'*' | b'-' | b'.' | b'_' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

/// UTF-8 percent-encoding with the default encode set (for a `mailto:`
/// body): C0 controls, space, `"`, `#`, `<`, `>`, `?`, `` ` ``, `{`, `}`
/// and bytes above 0x7E.
fn percent_encode_default(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if !(0x21..=0x7E).contains(&b) || matches!(b, b'"' | b'#' | b'<' | b'>' | b'?' | b'`' | b'{' | b'}') {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    out
}

/// The `text/plain` encoding algorithm: `name=value` CRLF per pair.
pub fn text_plain(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in pairs {
        out.push_str(name);
        out.push('=');
        out.push_str(value);
        out.push_str("\r\n");
    }
    out
}

/// A multipart boundary as Chrome makes them: `----WebKitFormBoundary`
/// and sixteen random letters and digits.
pub fn new_boundary() -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::from("----WebKitFormBoundary");
    let state = std::collections::hash_map::RandomState::new();
    let mut h = state.build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    let mut bits = h.finish();
    let mut h2 = state.build_hasher();
    h2.write_u64(bits);
    let mut more = h2.finish();
    for i in 0..16 {
        if i == 8 {
            bits = more;
            more = 0;
        }
        out.push(ALPHABET[(bits % 62) as usize] as char);
        bits /= 62;
    }
    let _ = more;
    out
}

/// A field name or file name in a multipart header: LF, CR and `"`
/// percent-escaped, nothing else.
fn escape_multipart_name(s: &str) -> String {
    s.replace('\n', "%0A").replace('\r', "%0D").replace('"', "%22")
}

/// The `multipart/form-data` encoding algorithm (HTML §4.10.22.7, RFC
/// 7578): one part per entry with its `Content-Disposition` (and a file's
/// `Content-Type`), names and text in the form's encoding.
pub fn multipart(entries: &[Entry], encoding: &'static encoding_rs::Encoding, boundary: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let name = escape_multipart_name(&to_crlf(&e.name));
        out.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        out.extend_from_slice(&encode(&name, encoding));
        out.push(b'"');
        match &e.value {
            EntryValue::Text(t) => {
                out.extend_from_slice(b"\r\n\r\n");
                out.extend_from_slice(&encode(&to_crlf(t), encoding));
            }
            EntryValue::File { name, mime, bytes } => {
                out.extend_from_slice(b"; filename=\"");
                out.extend_from_slice(&encode(&escape_multipart_name(name), encoding));
                out.extend_from_slice(b"\"\r\nContent-Type: ");
                out.extend_from_slice(if mime.is_empty() { b"application/octet-stream" } else { mime.as_bytes() });
                out.extend_from_slice(b"\r\n\r\n");
                out.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// Where control state lives (`Document::controls`).
pub(crate) type Controls = HashMap<NodeId, ControlState>;

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Document {
        let mut d = crate::parse_html(html.as_bytes());
        d.settle_forms();
        d
    }

    fn by_id(d: &Document, id: &str) -> NodeId {
        d.descendants(d.root()).find(|&n| d.element(n).and_then(|e| e.id()) == Some(id)).expect("element")
    }

    #[test]
    fn microsyntaxes() {
        for ok in ["1", "-1", "0.5", ".5", "1e3", "1E-3", "-.5e+2", "007"] {
            assert!(is_valid_float(ok), "{ok}");
        }
        for bad in ["", "+1", "1.", ".", "-", "1e", "e3", " 1", "1 ", "Infinity", "NaN", "0x10"] {
            assert!(!is_valid_float(bad), "{bad}");
        }
        assert_eq!(parse_float("  12abc"), Some(12.0));
        assert_eq!(parse_float("+.5"), Some(0.5));
        assert_eq!(parse_float("1e"), Some(1.0));
        assert_eq!(parse_float("abc"), None);
        assert_eq!(parse_float("1e400"), None);
        for (n, s) in [
            (50.0, "50"),
            (0.5, "0.5"),
            (-0.0, "0"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.000001, "0.000001"),
            (1.5e-10, "1.5e-10"),
            (0.1 + 0.2, "0.30000000000000004"),
        ] {
            assert_eq!(number_to_string(n), s);
        }
        assert_eq!(parse_date_string("2024-02-29"), Some((2024, 2, 29)));
        assert!(parse_date_string("2023-02-29").is_none());
        assert!(parse_date_string("0000-01-01").is_none());
        assert!(parse_date_string("12345-01-01").is_some());
        assert!(parse_date_string("2023-1-01").is_none());
        assert!(parse_month_string("2023-13").is_none());
        // 2020 began on a Wednesday and is a leap year: 53 weeks; 2021
        // began on a Friday: 52; 2015 began on a Thursday: 53.
        assert!(parse_week_string("2020-W53").is_some());
        assert!(parse_week_string("2021-W53").is_none());
        assert!(parse_week_string("2015-W53").is_some());
        assert!(parse_week_string("2015-W00").is_none());
        assert_eq!(parse_time_string("23:59:59.999"), Some((23, 59, 59, 999)));
        assert_eq!(parse_time_string("09:05"), Some((9, 5, 0, 0)));
        assert!(parse_time_string("24:00").is_none());
        assert!(parse_time_string("10:00:60").is_none());
        assert!(parse_time_string("10:00:00.1234").is_none());
        assert_eq!(normalize_local_datetime("2020-01-01 10:00:00"), Some("2020-01-01T10:00".to_owned()));
        assert_eq!(normalize_local_datetime("2020-01-01T10:00:30.500"), Some("2020-01-01T10:00:30.5".to_owned()));
        assert_eq!(normalize_local_datetime("2020-01-01"), None);
    }

    #[test]
    fn values_follow_their_modes_and_sanitization() {
        let mut d = doc("<input id=t value='a\nb'><input id=n type=number value=abc><input id=n2 type=number value='1e3'>\
             <input id=e type=email value='  x@y.z '><input id=em type=email multiple value=' a@b , c@d'>\
             <input id=r type=range><input id=r2 type=range min=0 max=10 step=3 value=8><input id=r3 type=range value=150>\
             <input id=r4 type=range min=10 max=5><input id=c type=color value='#ABCDEF'><input id=c2 type=color value=red>\
             <input id=cb type=checkbox><input id=cb2 type=checkbox value=yes><input id=h type=hidden value=hv>\
             <input id=dt type=datetime-local value='2020-01-01 10:00:00'><input id=f type=file>\
             <input id=u type=url value=' http://x/ '><input id=r5 type=range min=0 max=1 step=0.1 value=0.35>");
        let v = |d: &Document, id: &str| d.control_value(by_id(d, id));
        assert_eq!(v(&d, "t"), "ab");
        assert_eq!(v(&d, "n"), "");
        assert_eq!(v(&d, "n2"), "1e3");
        assert_eq!(v(&d, "e"), "x@y.z");
        assert_eq!(v(&d, "em"), "a@b,c@d");
        assert_eq!(v(&d, "r"), "50");
        assert_eq!(v(&d, "r2"), "9", "8 rounds to the nearer step, 9 (ties up)");
        assert_eq!(v(&d, "r3"), "100");
        assert_eq!(v(&d, "r4"), "10", "max below min: the minimum");
        assert_eq!(v(&d, "r5"), "0.4", "a tie between 0.3 and 0.4 goes up");
        assert_eq!(v(&d, "c"), "#abcdef");
        assert_eq!(v(&d, "c2"), "#000000");
        assert_eq!(v(&d, "cb"), "on");
        assert_eq!(v(&d, "cb2"), "yes");
        assert_eq!(v(&d, "h"), "hv");
        assert_eq!(v(&d, "dt"), "2020-01-01T10:00");
        assert_eq!(v(&d, "f"), "");
        assert_eq!(v(&d, "u"), "http://x/");
        // A script's value is sanitized and sticks; the attribute is the
        // default from then on.
        let t = by_id(&d, "t");
        assert_eq!(d.set_value_by_script(t, "x\ny"), Ok(true));
        assert_eq!(d.control_value(t), "xy");
        d.get_mut(t).as_element_mut().expect("element").set_attr("value", "zzz");
        d.note_form_attribute(t, "value", Some("a\nb"));
        assert_eq!(d.control_value(t), "xy");
        assert_eq!(d.element(t).expect("element").attr("value"), Some("zzz"));
        let f = by_id(&d, "f");
        assert_eq!(d.set_value_by_script(f, "x"), Err("InvalidStateError"));
        assert_eq!(d.set_value_by_script(f, ""), Ok(false));
        // The user's text in a number field shows as typed; its value is
        // what sanitizes.
        let n = by_id(&d, "n");
        d.set_value_by_user(n, "1.");
        assert_eq!(d.control_display_value(n), "1.");
        assert_eq!(d.control_value(n), "");
        // Default mode: the value IDL attribute is the content attribute.
        let h = by_id(&d, "h");
        d.set_value_by_script(h, "new").expect("ok");
        assert_eq!(d.element(h).expect("element").attr("value"), Some("new"));
    }

    #[test]
    fn type_changes_move_the_value() {
        let mut d = doc("<input id=t value=def>");
        let t = by_id(&d, "t");
        d.set_value_by_script(t, "typed").expect("ok");
        d.get_mut(t).as_element_mut().expect("element").set_attr("type", "checkbox");
        d.note_form_attribute(t, "type", None);
        assert_eq!(d.element(t).expect("element").attr("value"), Some("typed"), "value mode to default/on writes the attribute");
        assert_eq!(d.control_value(t), "typed");
        d.get_mut(t).as_element_mut().expect("element").set_attr("type", "number");
        d.note_form_attribute(t, "type", Some("checkbox"));
        assert_eq!(d.control_value(t), "", "back to value mode: the attribute, sanitized");
        d.set_value_by_script(t, "12").expect("ok");
        d.get_mut(t).as_element_mut().expect("element").set_attr("type", "date");
        d.note_form_attribute(t, "type", Some("number"));
        d.get_mut(t).as_element_mut().expect("element").set_attr("type", "text");
        d.note_form_attribute(t, "type", Some("date"));
        assert_eq!(d.control_value(t), "", "sanitization is for good");
    }

    #[test]
    fn checkedness_and_radio_groups() {
        let mut d = doc("<form><input type=radio name=g id=a checked><input type=radio name=g id=b checked>\
             <input type=radio name=G id=c checked></form><input type=radio name=g id=d checked>\
             <input type=checkbox id=x checked>");
        let (a, b, c, dd, x) = (by_id(&d, "a"), by_id(&d, "b"), by_id(&d, "c"), by_id(&d, "d"), by_id(&d, "x"));
        assert!(!d.checkedness(a) && d.checkedness(b), "the later checked radio of a group wins");
        assert!(d.checkedness(c), "names compare case-sensitively");
        assert!(d.checkedness(dd), "outside the form: another group");
        d.set_checkedness(a, true, true);
        assert!(d.checkedness(a) && !d.checkedness(b) && d.checkedness(dd));
        // A dirty checkbox ignores its attribute; a reset restores it.
        d.set_checkedness(x, false, true);
        d.get_mut(x).as_element_mut().expect("element").remove_attr("checked");
        d.note_form_attribute(x, "checked", Some(""));
        d.get_mut(x).as_element_mut().expect("element").set_attr("checked", "");
        d.note_form_attribute(x, "checked", None);
        assert!(!d.checkedness(x));
        d.reset_control(x);
        assert!(d.checkedness(x));
        // Pre-activation and its undo.
        let saved = d.pre_activate(x);
        assert!(!d.checkedness(x));
        d.cancel_activation(x, saved);
        assert!(d.checkedness(x));
        let saved = d.pre_activate(b);
        assert!(d.checkedness(b) && !d.checkedness(a));
        d.cancel_activation(b, saved);
        assert!(d.checkedness(a) && !d.checkedness(b));
    }

    #[test]
    fn select_selectedness() {
        let mut d = doc("<select id=s><option id=o1>A<option id=o2 selected>B<option id=o3 selected>C</select>\
             <select id=t><option disabled>X<option id=t2>Y</select><select id=m multiple><option>1<option>2</select>\
             <select id=g><optgroup label=l><option id=g1 value=v1> one  two </option></optgroup></select>");
        let s = by_id(&d, "s");
        assert_eq!(d.selected_index(s), 2, "the last selected option stays");
        assert_eq!(d.control_value(s), "C");
        assert_eq!(d.selected_index(by_id(&d, "t")), 1, "the first option not disabled");
        assert_eq!(d.selected_index(by_id(&d, "m")), -1, "a multiple select selects nothing by itself");
        assert_eq!(d.control_value(by_id(&d, "g")), "v1");
        assert_eq!(d.option_text(by_id(&d, "g1")), "one two");
        d.set_selected_index(s, 0);
        assert_eq!(d.control_value(s), "A");
        d.set_select_value(s, "nope");
        assert_eq!(d.selected_index(s), -1, "no option of that value: none selected");
        d.reset_control(s);
        assert_eq!(d.selected_index(s), 2);
        // A script adding an option to a select with none selected.
        let e = d.create_html_element("select");
        let body = d.body().expect("ok");
        d.append_child(body, e);
        let o = d.create_html_element("option");
        d.append_child(e, o);
        d.settle_forms();
        assert_eq!(d.selected_index(e), 0);
    }

    fn entries_of(d: &Document, form: &str, submitter: Option<&str>) -> Vec<(String, String)> {
        let f = by_id(d, form);
        let s = submitter.map(|s| by_id(d, s));
        name_value_pairs(&d.construct_entry_list(f, s, encoding_rs::UTF_8, (3, 4)))
    }

    #[test]
    fn entry_lists() {
        let d = doc("<form id=f><input name=a value=1><input name=b disabled value=2><input name=c type=checkbox>\
             <input name=d type=checkbox checked><input name=e type=radio value=r checked><button name=btn value=bv id=btn>\
             <input type=submit name=s value=sv id=s><input type=image name=img id=img><select name=sel multiple>\
             <option selected>x<option selected disabled>y<option selected value=z>zz</select><textarea name=ta>l1\nl2</textarea>\
             <input type=file name=up><input type=hidden name=_charset_><input name=''><input value=noname>\
             <fieldset disabled><input name=in_disabled></fieldset><datalist><input name=in_datalist></datalist>\
             <input name=dn value=hi dirname=dn.dir></form><input form=f name=outside value=o>");
        let names = |v: &[(String, String)]| v.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>();
        assert_eq!(
            names(&entries_of(&d, "f", Some("s"))),
            vec!["a=1", "d=on", "e=r", "s=sv", "sel=x", "sel=z", "ta=l1\r\nl2", "up=", "_charset_=UTF-8", "dn=hi", "dn.dir=ltr", "outside=o"]
        );
        let with_image = names(&entries_of(&d, "f", Some("img")));
        assert!(with_image.contains(&"img.x=3".to_owned()) && with_image.contains(&"img.y=4".to_owned()));
        assert!(names(&entries_of(&d, "f", Some("btn"))).contains(&"btn=bv".to_owned()));
        assert!(!names(&entries_of(&d, "f", None)).iter().any(|e| e.starts_with("s=")));
    }

    #[test]
    fn encodings() {
        let pairs = vec![("a b".to_owned(), "é&=*-._~+".to_owned())];
        assert_eq!(urlencoded(&pairs, encoding_rs::UTF_8), "a+b=%C3%A9%26%3D*-._%7E%2B");
        assert_eq!(urlencoded(&[("x".to_owned(), "é€".to_owned())], encoding_rs::WINDOWS_1252), "x=%E9%80");
        assert_eq!(urlencoded(&[("x".to_owned(), "日".to_owned())], encoding_rs::WINDOWS_1252), "x=%26%2326085%3B");
        assert_eq!(text_plain(&[("a".to_owned(), "1".to_owned()), ("b".to_owned(), "2".to_owned())]), "a=1\r\nb=2\r\n");
        let entries = vec![
            Entry { name: "f\"n".to_owned(), value: EntryValue::Text("v\nw".to_owned()) },
            Entry {
                name: "up".to_owned(),
                value: EntryValue::File { name: String::new(), mime: "application/octet-stream".to_owned(), bytes: Vec::new() },
            },
        ];
        let body = String::from_utf8(multipart(&entries, encoding_rs::UTF_8, "XYZ")).expect("ok");
        assert_eq!(
            body,
            "--XYZ\r\nContent-Disposition: form-data; name=\"f%22n\"\r\n\r\nv\r\nw\r\n\
             --XYZ\r\nContent-Disposition: form-data; name=\"up\"; filename=\"\"\r\nContent-Type: application/octet-stream\r\n\r\n\r\n--XYZ--\r\n"
        );
        let b = new_boundary();
        assert!(b.starts_with("----WebKitFormBoundary") && b.len() == 38 && b[22..].bytes().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn submission_actions() {
        let mut d = doc("<base target=named><form id=f action='/go?old=1#frag'><input name=q value='a b'></form>\
             <form id=p method=post enctype=text/plain action=/post><input name=q value=x><button id=pb formmethod=get formaction=/other formtarget=_blank>\
             </button></form><form id=m action='mailto:x@y.z?subject=s' method=post><input name=q value='a b'></form>\
             <dialog><form id=dl method=dialog><button id=db value=ok></button></form></dialog>");
        d.base_url = Some(Url::parse("https://example.test/dir/page").expect("url"));
        let act = |d: &Document, form: &str, sub: Option<&str>| {
            let f = by_id(d, form);
            let s = sub.map(|s| by_id(d, s));
            let enc = d.form_encoding(f);
            let entries = d.construct_entry_list(f, s, enc, (0, 0));
            d.submission_action(f, s, &entries, enc, None, "B").expect("action")
        };
        assert_eq!(
            act(&d, "f", None),
            SubmitAction::Navigate {
                url: Url::parse("https://example.test/go?q=a+b#frag").expect("url"),
                post: None,
                target: "named".to_owned(),
                noreferrer: false
            }
        );
        let SubmitAction::Navigate { url, post, .. } = act(&d, "p", None) else { panic!() };
        assert_eq!(url.as_str(), "https://example.test/post");
        assert_eq!(post, Some(PostResource { content_type: "text/plain".to_owned(), body: b"q=x\r\n".to_vec() }));
        let SubmitAction::Navigate { url, post, target, .. } = act(&d, "p", Some("pb")) else { panic!() };
        assert_eq!((url.as_str(), post, target.as_str()), ("https://example.test/other?q=x", None, "_blank"));
        let SubmitAction::Navigate { url, .. } = act(&d, "m", None) else { panic!() };
        assert_eq!(url.as_str(), "mailto:x@y.z?subject=s&body=q=a+b");
        assert!(matches!(act(&d, "dl", Some("db")), SubmitAction::CloseDialog { result: Some(r), .. } if r == "ok"));
    }

    #[test]
    fn parser_form_pointer_and_form_attribute() {
        // The input lands outside the form (foster-parented out of the
        // table) yet belongs to it.
        let d = doc("<table><form id=f><tr><td><input id=i name=n></td></tr></form></table><input id=j form=f><input id=k form=nope>");
        let (f, i) = (by_id(&d, "f"), by_id(&d, "i"));
        assert!(!d.ancestors(i).any(|a| a == f), "not a descendant");
        assert_eq!(d.form_owner(i), Some(f));
        assert_eq!(d.form_owner(by_id(&d, "j")), Some(f));
        assert_eq!(d.form_owner(by_id(&d, "k")), None);
        assert_eq!(d.form_elements(f).len(), 2);
    }
}
