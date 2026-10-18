//! Specified and computed value types, and their parsers.

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;

/// A CSS length before computation, in the unit it was written in (the
/// CSSOM serializes a specified value as written). Percentages are kept
/// separate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Em(f32),
    Rem(f32),
    Vw(f32),
    Vh(f32),
    Vmin(f32),
    Vmax(f32),
    /// `ch`, `ex`: approximated from font size at compute time.
    Ch(f32),
    Ex(f32),
    /// The absolute units: fixed ratios to `px`.
    Pt(f32),
    Pc(f32),
    In(f32),
    Cm(f32),
    Mm(f32),
    Q(f32),
}

impl Length {
    pub const ZERO: Length = Length::Px(0.0);

    /// Resolve to pixels.
    pub fn to_px(self, ctx: &LengthContext) -> f32 {
        match self {
            Length::Px(v) => v,
            Length::Em(v) => v * ctx.font_size,
            Length::Rem(v) => v * ctx.root_font_size,
            Length::Vw(v) => v * ctx.viewport_width / 100.0,
            Length::Vh(v) => v * ctx.viewport_height / 100.0,
            Length::Vmin(v) => v * ctx.viewport_width.min(ctx.viewport_height) / 100.0,
            Length::Vmax(v) => v * ctx.viewport_width.max(ctx.viewport_height) / 100.0,
            Length::Ch(v) => v * ctx.font_size * 0.5,
            Length::Ex(v) => v * ctx.font_size * 0.5,
            Length::Pt(v) => v * 96.0 / 72.0,
            Length::Pc(v) => v * 16.0,
            Length::In(v) => v * 96.0,
            Length::Cm(v) => v * 96.0 / 2.54,
            Length::Mm(v) => v * 96.0 / 25.4,
            Length::Q(v) => v * 96.0 / 25.4 / 4.0,
        }
    }

    /// The number and the unit, as written.
    pub fn parts(self) -> (f32, &'static str) {
        match self {
            Length::Px(v) => (v, "px"),
            Length::Em(v) => (v, "em"),
            Length::Rem(v) => (v, "rem"),
            Length::Vw(v) => (v, "vw"),
            Length::Vh(v) => (v, "vh"),
            Length::Vmin(v) => (v, "vmin"),
            Length::Vmax(v) => (v, "vmax"),
            Length::Ch(v) => (v, "ch"),
            Length::Ex(v) => (v, "ex"),
            Length::Pt(v) => (v, "pt"),
            Length::Pc(v) => (v, "pc"),
            Length::In(v) => (v, "in"),
            Length::Cm(v) => (v, "cm"),
            Length::Mm(v) => (v, "mm"),
            Length::Q(v) => (v, "q"),
        }
    }

    /// Serialize as written: `<number><unit>`.
    pub fn to_css(self) -> String {
        let (v, unit) = self.parts();
        format!("{}{unit}", css_number(v))
    }

    /// `self` with its number replaced.
    fn with_value(self, v: f32) -> Length {
        match self {
            Length::Px(_) => Length::Px(v),
            Length::Em(_) => Length::Em(v),
            Length::Rem(_) => Length::Rem(v),
            Length::Vw(_) => Length::Vw(v),
            Length::Vh(_) => Length::Vh(v),
            Length::Vmin(_) => Length::Vmin(v),
            Length::Vmax(_) => Length::Vmax(v),
            Length::Ch(_) => Length::Ch(v),
            Length::Ex(_) => Length::Ex(v),
            Length::Pt(_) => Length::Pt(v),
            Length::Pc(_) => Length::Pc(v),
            Length::In(_) => Length::In(v),
            Length::Cm(_) => Length::Cm(v),
            Length::Mm(_) => Length::Mm(v),
            Length::Q(_) => Length::Q(v),
        }
    }
}

/// Serialize a CSS number: the shortest form with at most six decimals,
/// no trailing zeros, no negative zero (what browsers print).
pub fn css_number(v: f32) -> String {
    if !v.is_finite() {
        return "0".to_owned();
    }
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    let s = if s.is_empty() || s == "-" || s == "-0" { "0" } else { s };
    s.to_owned()
}

/// A math function (CSS Values 4 §10; "Working sites first" line 6f):
/// `calc()`, `min()`, `max()` or `clamp()` of lengths, percentages and
/// numbers with `+`, `-`, `*`, `/`, kept as written for the CSSOM.
#[derive(Debug, Clone, PartialEq)]
pub struct Calc {
    pub node: CalcNode,
}

/// A node of a math expression.
#[derive(Debug, Clone, PartialEq)]
pub enum CalcNode {
    Number(f32),
    Length(Length),
    Percent(f32),
    /// Terms, each subtracted (`true`) or added.
    Sum(Vec<(bool, CalcNode)>),
    /// Factors, each a divisor (`true`) or a multiplier.
    Product(Vec<(bool, CalcNode)>),
    Min(Vec<CalcNode>),
    Max(Vec<CalcNode>),
    Clamp(Box<[CalcNode; 3]>),
    /// A nested `calc()` or parentheses.
    Calc(Box<CalcNode>),
}

/// What a math expression resolves to: a number or a length (where a
/// percentage is a length too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalcKind {
    Number,
    Length,
}

impl CalcNode {
    /// The expression's type, or `None` when the operands do not combine
    /// (a length plus a number, a length times a length, a division by a
    /// length): such an expression does not parse (§10.8).
    fn kind(&self) -> Option<CalcKind> {
        match self {
            CalcNode::Number(_) => Some(CalcKind::Number),
            CalcNode::Length(_) | CalcNode::Percent(_) => Some(CalcKind::Length),
            CalcNode::Sum(terms) | CalcNode::Product(terms) if terms.is_empty() => None,
            CalcNode::Sum(terms) => {
                let first = terms[0].1.kind()?;
                terms.iter().all(|(_, t)| t.kind() == Some(first)).then_some(first)
            }
            CalcNode::Product(factors) => {
                let mut lengths = 0;
                for (divide, f) in factors {
                    match f.kind()? {
                        CalcKind::Length if *divide => return None,
                        CalcKind::Length => lengths += 1,
                        CalcKind::Number => {}
                    }
                }
                match lengths {
                    0 => Some(CalcKind::Number),
                    1 => Some(CalcKind::Length),
                    _ => None,
                }
            }
            CalcNode::Min(args) | CalcNode::Max(args) => {
                let first = args.first()?.kind()?;
                args.iter().all(|a| a.kind() == Some(first)).then_some(first)
            }
            CalcNode::Clamp(args) => {
                let k = args[0].kind()?;
                (args[1].kind() == Some(k) && args[2].kind() == Some(k)).then_some(k)
            }
            CalcNode::Calc(inner) => inner.kind(),
        }
    }

    /// Whether a percentage appears anywhere.
    pub fn has_percent(&self) -> bool {
        match self {
            CalcNode::Percent(_) => true,
            CalcNode::Number(_) | CalcNode::Length(_) => false,
            CalcNode::Sum(v) | CalcNode::Product(v) => v.iter().any(|(_, n)| n.has_percent()),
            CalcNode::Min(v) | CalcNode::Max(v) => v.iter().any(CalcNode::has_percent),
            CalcNode::Clamp(a) => a.iter().any(CalcNode::has_percent),
            CalcNode::Calc(n) => n.has_percent(),
        }
    }

    /// The value in pixels (or the number), lengths through `ctx` when
    /// given (else they must already be pixels), percentages of `basis`
    /// (`None`: a percentage makes the whole `None`). `min`/`max`/`clamp`
    /// as defined (clamp's minimum wins over its maximum).
    pub fn eval(&self, ctx: Option<&LengthContext>, basis: Option<f32>) -> Option<f32> {
        Some(match self {
            CalcNode::Number(n) => *n,
            CalcNode::Length(l) => match ctx {
                Some(ctx) => l.to_px(ctx),
                None => l.parts().0,
            },
            CalcNode::Percent(p) => p / 100.0 * basis?,
            CalcNode::Sum(terms) => {
                let mut acc = 0.0;
                for (neg, t) in terms {
                    let v = t.eval(ctx, basis)?;
                    acc += if *neg { -v } else { v };
                }
                acc
            }
            CalcNode::Product(factors) => {
                let mut acc = 1.0;
                for (divide, f) in factors {
                    let v = f.eval(ctx, basis)?;
                    acc = if *divide { acc / v } else { acc * v };
                }
                acc
            }
            CalcNode::Min(args) => {
                let mut acc = f32::INFINITY;
                for a in args {
                    acc = acc.min(a.eval(ctx, basis)?);
                }
                acc
            }
            CalcNode::Max(args) => {
                let mut acc = f32::NEG_INFINITY;
                for a in args {
                    acc = acc.max(a.eval(ctx, basis)?);
                }
                acc
            }
            CalcNode::Clamp(a) => {
                let (lo, v, hi) = (a[0].eval(ctx, basis)?, a[1].eval(ctx, basis)?, a[2].eval(ctx, basis)?);
                v.min(hi).max(lo)
            }
            CalcNode::Calc(n) => n.eval(ctx, basis)?,
        })
    }

    /// A single value: the number and its unit (`""` for a number, `"%"`).
    fn leaf(&self) -> Option<(f32, &'static str)> {
        match self {
            CalcNode::Number(n) => Some((*n, "")),
            CalcNode::Percent(p) => Some((*p, "%")),
            CalcNode::Length(l) => Some(l.parts()),
            _ => None,
        }
    }

    fn from_leaf(v: f32, unit: &'static str, like: &CalcNode) -> CalcNode {
        match like {
            CalcNode::Number(_) => CalcNode::Number(v),
            CalcNode::Percent(_) => CalcNode::Percent(v),
            CalcNode::Length(l) => CalcNode::Length(l.with_value(v)),
            _ => {
                debug_assert!(unit.is_empty());
                CalcNode::Number(v)
            }
        }
    }

    /// The expression simplified (CSS Values 4 §10.12): nested `calc()`
    /// unwrapped, sums flattened with like units added and sorted (number,
    /// percentage, lengths by unit, then the rest), numbers multiplied in,
    /// `min`/`max`/`clamp` of like units computed. Absolute units become
    /// `px`; with `ctx` (computed values) every length does.
    fn simplify(&self, ctx: Option<&LengthContext>) -> CalcNode {
        match self {
            CalcNode::Number(_) | CalcNode::Percent(_) => self.clone(),
            CalcNode::Length(l) => CalcNode::Length(match (ctx, l) {
                (Some(ctx), l) => Length::Px(l.to_px(ctx)),
                (None, Length::Pt(_) | Length::Pc(_) | Length::In(_) | Length::Cm(_) | Length::Mm(_) | Length::Q(_)) => {
                    Length::Px(l.to_px(&LengthContext::ABSOLUTE))
                }
                (None, l) => *l,
            }),
            CalcNode::Calc(n) => n.simplify(ctx),
            CalcNode::Sum(terms) => {
                fn flatten(out: &mut Vec<(bool, CalcNode)>, neg: bool, node: CalcNode) {
                    match node {
                        CalcNode::Sum(inner) => {
                            for (n, c) in inner {
                                flatten(out, neg ^ n, c);
                            }
                        }
                        other => out.push((neg, other)),
                    }
                }
                let mut flat = Vec::new();
                for (neg, t) in terms {
                    flatten(&mut flat, *neg, t.simplify(ctx));
                }
                // Like units added, in their first node's form.
                let mut leaves: Vec<(f32, &'static str, CalcNode)> = Vec::new();
                let mut rest = Vec::new();
                for (neg, n) in flat {
                    match n.leaf() {
                        Some((v, unit)) => {
                            let v = if neg { -v } else { v };
                            match leaves.iter_mut().find(|e| e.1 == unit) {
                                Some(e) => e.0 += v,
                                None => leaves.push((v, unit, n)),
                            }
                        }
                        None => rest.push((neg, n)),
                    }
                }
                let rank = |unit: &str| match unit {
                    "" => 0,
                    "%" => 1,
                    _ => 2,
                };
                leaves.sort_by(|a, b| rank(a.1).cmp(&rank(b.1)).then_with(|| a.1.cmp(b.1)));
                let mut out: Vec<(bool, CalcNode)> =
                    leaves.into_iter().map(|(v, unit, like)| (false, CalcNode::from_leaf(v, unit, &like))).collect();
                out.extend(rest);
                if out.len() == 1 && !out[0].0 {
                    return out.pop().map(|(_, n)| n).unwrap_or(CalcNode::Number(0.0));
                }
                CalcNode::Sum(out)
            }
            CalcNode::Product(factors) => {
                fn flatten(num: &mut f32, rest: &mut Vec<(bool, CalcNode)>, divide: bool, node: CalcNode) {
                    match node {
                        CalcNode::Number(n) => *num = if divide { *num / n } else { *num * n },
                        CalcNode::Product(inner) if !divide => {
                            for (d, c) in inner {
                                flatten(num, rest, d, c);
                            }
                        }
                        other => rest.push((divide, other)),
                    }
                }
                let mut num = 1.0;
                let mut rest = Vec::new();
                for (divide, f) in factors {
                    flatten(&mut num, &mut rest, *divide, f.simplify(ctx));
                }
                let scale = |n: &CalcNode| n.leaf().map(|(v, unit)| CalcNode::from_leaf(v * num, unit, n));
                match rest.as_slice() {
                    [] => return CalcNode::Number(num),
                    [(false, only)] => {
                        if let Some(leaf) = scale(only) {
                            return leaf;
                        }
                        if num == 1.0 {
                            return only.clone();
                        }
                        // A number times a sum of values: the sum, scaled.
                        if let CalcNode::Sum(terms) = only {
                            if terms.iter().all(|(_, t)| t.leaf().is_some()) {
                                return CalcNode::Sum(
                                    terms
                                        .iter()
                                        .map(|(neg, t)| {
                                            let (v, unit) = t.leaf().unwrap_or((0.0, ""));
                                            let v = if *neg { -v } else { v };
                                            (false, CalcNode::from_leaf(v * num, unit, t))
                                        })
                                        .collect(),
                                );
                            }
                        }
                    }
                    _ => {}
                }
                let mut out = Vec::new();
                if num != 1.0 {
                    out.push((false, CalcNode::Number(num)));
                }
                out.extend(rest);
                CalcNode::Product(out)
            }
            CalcNode::Min(args) | CalcNode::Max(args) => {
                let min = matches!(self, CalcNode::Min(_));
                let args: Vec<CalcNode> = args.iter().map(|a| a.simplify(ctx)).collect();
                if args.len() == 1 {
                    return args.into_iter().next().unwrap_or(CalcNode::Number(0.0));
                }
                let first = args[0].leaf();
                if let Some((_, unit)) = first {
                    if args.iter().all(|a| a.leaf().is_some_and(|l| l.1 == unit)) {
                        let mut best = &args[0];
                        for a in &args[1..] {
                            let (v, b) = (a.leaf().map_or(0.0, |l| l.0), best.leaf().map_or(0.0, |l| l.0));
                            if (min && v < b) || (!min && v > b) {
                                best = a;
                            }
                        }
                        return best.clone();
                    }
                }
                if min { CalcNode::Min(args) } else { CalcNode::Max(args) }
            }
            CalcNode::Clamp(a) => {
                let [lo, v, hi] = [a[0].simplify(ctx), a[1].simplify(ctx), a[2].simplify(ctx)];
                if let (Some(l), Some(m), Some(h)) = (lo.leaf(), v.leaf(), hi.leaf()) {
                    if l.1 == m.1 && m.1 == h.1 {
                        return CalcNode::from_leaf(m.0.min(h.0).max(l.0), m.1, &v);
                    }
                }
                CalcNode::Clamp(Box::new([lo, v, hi]))
            }
        }
    }

    /// Serialize a simplified expression at the top (§10.13): a value or
    /// a sum or product inside `calc()`, a function as itself.
    fn css_root(&self) -> String {
        match self {
            CalcNode::Sum(_) | CalcNode::Product(_) => format!("calc({})", self.css_inner()),
            CalcNode::Calc(n) => n.css_root(),
            _ if self.leaf().is_some() => format!("calc({})", self.css_term()),
            _ => self.css_term(),
        }
    }

    /// As a function's argument: a sum or product without parentheses.
    fn css_arg(&self) -> String {
        match self {
            CalcNode::Sum(_) | CalcNode::Product(_) => self.css_inner(),
            _ => self.css_term(),
        }
    }

    /// As an operand of a sum or product: a sum or product in parentheses.
    fn css_term(&self) -> String {
        let join = |v: &[CalcNode]| v.iter().map(CalcNode::css_arg).collect::<Vec<_>>().join(", ");
        match self {
            CalcNode::Number(n) => css_number(*n),
            CalcNode::Length(l) => l.to_css(),
            CalcNode::Percent(p) => format!("{}%", css_number(*p)),
            CalcNode::Sum(_) | CalcNode::Product(_) => format!("({})", self.css_inner()),
            CalcNode::Min(v) => format!("min({})", join(v)),
            CalcNode::Max(v) => format!("max({})", join(v)),
            CalcNode::Clamp(a) => format!("clamp({})", join(&a[..])),
            CalcNode::Calc(n) => n.css_term(),
        }
    }

    fn css_inner(&self) -> String {
        let mut s = String::new();
        match self {
            CalcNode::Sum(terms) => {
                for (i, (neg, t)) in terms.iter().enumerate() {
                    // A negative value after the first is subtracted.
                    let (neg, t) = match t.leaf() {
                        Some((v, unit)) if i > 0 && v < 0.0 => (!neg, CalcNode::from_leaf(-v, unit, t)),
                        _ => (*neg, t.clone()),
                    };
                    if i > 0 {
                        s.push_str(if neg { " - " } else { " + " });
                        s.push_str(&t.css_term());
                    } else if neg {
                        s.push_str(&format!("(-1 * {})", t.css_term()));
                    } else {
                        s.push_str(&t.css_term());
                    }
                }
            }
            CalcNode::Product(factors) => {
                for (i, (divide, f)) in factors.iter().enumerate() {
                    if i > 0 {
                        s.push_str(if *divide { " / " } else { " * " });
                    } else if *divide {
                        s.push_str("1 / ");
                    }
                    s.push_str(&f.css_term());
                }
            }
            other => s.push_str(&other.css_term()),
        }
        s
    }
}

impl Calc {
    /// Serialize as the CSSOM does (§10.13): simplified, absolute units in
    /// `px`, relative units kept.
    pub fn to_css(&self) -> String {
        self.node.simplify(None).css_root()
    }

    /// The computed value: pixels when no percentage is involved, a
    /// percentage when that is all that remains, else the simplified
    /// expression with its lengths in pixels, kept to layout.
    fn to_computed(&self, ctx: &LengthContext) -> ComputedLp {
        let node = self.node.simplify(Some(ctx));
        match node {
            CalcNode::Percent(p) => ComputedLp::Percent(p),
            _ if !node.has_percent() => ComputedLp::Px(node.eval(None, None).unwrap_or(0.0)),
            node => ComputedLp::Calc(CalcHandle::intern(node, false)),
        }
    }
}

/// A computed math expression involving percentages, interned: the
/// computed values stay `Copy`, and layout resolves the expression against
/// its basis (`CalcHandle::resolve`). A handle for a property that takes
/// no negative values (`width`, `padding`) clamps its result at zero
/// (§10.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CalcHandle(u32);

type CalcTable = (Vec<(CalcNode, bool)>, std::collections::HashMap<String, u32>);

fn calc_table() -> &'static std::sync::RwLock<CalcTable> {
    static TABLE: std::sync::OnceLock<std::sync::RwLock<CalcTable>> = std::sync::OnceLock::new();
    TABLE.get_or_init(Default::default)
}

impl CalcHandle {
    fn intern(node: CalcNode, non_negative: bool) -> CalcHandle {
        let key = format!("{non_negative}{node:?}");
        if let Some(&i) = calc_table().read().ok().and_then(|t| t.1.get(&key).copied()).as_ref() {
            return CalcHandle(i);
        }
        let mut t = calc_table().write().unwrap_or_else(|e| e.into_inner());
        if let Some(&i) = t.1.get(&key) {
            return CalcHandle(i);
        }
        let i = t.0.len() as u32;
        t.0.push((node, non_negative));
        t.1.insert(key, i);
        CalcHandle(i)
    }

    /// The same expression, its result clamped at zero.
    pub fn non_negative(self) -> CalcHandle {
        let node = {
            let t = calc_table().read().unwrap_or_else(|e| e.into_inner());
            match t.0.get(self.0 as usize) {
                Some((_, true)) | None => return self,
                Some((node, false)) => node.clone(),
            }
        };
        CalcHandle::intern(node, true)
    }

    /// A sum of a percentage and pixels, as its two parts.
    pub(crate) fn linear(self) -> Option<(f32, f32)> {
        let t = calc_table().read().unwrap_or_else(|e| e.into_inner());
        let Some((CalcNode::Sum(terms), _)) = t.0.get(self.0 as usize) else { return None };
        let (mut percent, mut px) = (0.0, 0.0);
        for (neg, term) in terms {
            let sign = if *neg { -1.0 } else { 1.0 };
            match term {
                CalcNode::Percent(p) => percent += sign * p,
                CalcNode::Length(Length::Px(v)) => px += sign * v,
                _ => return None,
            }
        }
        Some((percent, px))
    }

    /// `percent% + self` or `percent% - self`, simplified (a position from
    /// the center or the end edge).
    pub(crate) fn offset_from(self, percent: f32, subtract: bool) -> CalcHandle {
        let node = {
            let t = calc_table().read().unwrap_or_else(|e| e.into_inner());
            match t.0.get(self.0 as usize) {
                Some((node, _)) => node.clone(),
                None => return self,
            }
        };
        let sum = CalcNode::Sum(vec![(false, CalcNode::Percent(percent)), (subtract, node)]);
        CalcHandle::intern(sum.simplify(None), false)
    }

    /// The expression's value for a percentage basis.
    pub fn resolve(self, basis: f32) -> f32 {
        let t = calc_table().read().unwrap_or_else(|e| e.into_inner());
        match t.0.get(self.0 as usize) {
            Some((n, non_negative)) => {
                let v = n.eval(None, Some(basis)).unwrap_or(0.0);
                if *non_negative { v.max(0.0) } else { v }
            }
            None => 0.0,
        }
    }

    /// The handle as an opaque non-zero, 8-aligned token (taffy's calc
    /// values), and back.
    pub fn token(self) -> usize {
        (self.0 as usize + 1) << 3
    }

    pub fn from_token(token: usize) -> CalcHandle {
        CalcHandle(((token >> 3).max(1) - 1) as u32)
    }

    /// The computed expression as CSS (`getComputedStyle`).
    pub fn to_css(self) -> String {
        let t = calc_table().read().unwrap_or_else(|e| e.into_inner());
        t.0.get(self.0 as usize)
            .map(|(n, _)| n.css_root())
            .unwrap_or_else(|| "0px".to_owned())
    }
}

/// What a length needs to resolve.
#[derive(Debug, Clone, Copy)]
pub struct LengthContext {
    pub font_size: f32,
    pub root_font_size: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
}

impl LengthContext {
    /// For the absolute units, which need nothing.
    const ABSOLUTE: LengthContext =
        LengthContext { font_size: 16.0, root_font_size: 16.0, viewport_width: 0.0, viewport_height: 0.0 };
}

#[derive(Debug, Clone, PartialEq)]
pub enum LengthPercentage {
    Length(Length),
    Percent(f32),
    /// A `calc()`, kept as written; `fold` gives what the engine uses.
    Calc(Calc),
}

impl LengthPercentage {
    pub const ZERO: LengthPercentage = LengthPercentage::Length(Length::ZERO);

    pub fn to_computed(&self, ctx: &LengthContext) -> ComputedLp {
        match self {
            LengthPercentage::Length(l) => ComputedLp::Px(l.to_px(ctx)),
            LengthPercentage::Percent(p) => ComputedLp::Percent(*p),
            LengthPercentage::Calc(c) => c.to_computed(ctx),
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            LengthPercentage::Length(l) => l.to_css(),
            LengthPercentage::Percent(p) => format!("{}%", css_number(*p)),
            LengthPercentage::Calc(c) => c.to_css(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LengthPercentageAuto {
    Length(Length),
    Percent(f32),
    Calc(Calc),
    Auto,
}

impl LengthPercentageAuto {
    pub fn to_computed(&self, ctx: &LengthContext) -> ComputedLpAuto {
        match self {
            LengthPercentageAuto::Auto => ComputedLpAuto::Auto,
            LengthPercentageAuto::Length(l) => ComputedLpAuto::Px(l.to_px(ctx)),
            LengthPercentageAuto::Percent(p) => ComputedLpAuto::Percent(*p),
            LengthPercentageAuto::Calc(c) => match LengthPercentage::Calc(c.clone()).to_computed(ctx) {
                ComputedLp::Px(v) => ComputedLpAuto::Px(v),
                ComputedLp::Percent(p) => ComputedLpAuto::Percent(p),
                ComputedLp::Calc(h) => ComputedLpAuto::Calc(h),
            },
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            LengthPercentageAuto::Auto => "auto".to_owned(),
            LengthPercentageAuto::Length(l) => l.to_css(),
            LengthPercentageAuto::Percent(p) => format!("{}%", css_number(*p)),
            LengthPercentageAuto::Calc(c) => c.to_css(),
        }
    }
}

/// Computed length-or-percentage: lengths are in px; a math expression
/// with percentages is resolved against the basis (`Calc`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ComputedLp {
    Px(f32),
    Percent(f32),
    Calc(CalcHandle),
}

impl ComputedLp {
    pub const ZERO: ComputedLp = ComputedLp::Px(0.0);
    pub fn resolve(self, basis: f32) -> f32 {
        match self {
            ComputedLp::Px(v) => v,
            ComputedLp::Percent(p) => p / 100.0 * basis,
            ComputedLp::Calc(h) => h.resolve(basis),
        }
    }

    /// Whether the value depends on a percentage basis.
    pub fn has_percent(self) -> bool {
        !matches!(self, ComputedLp::Px(_))
    }

    /// For a property that takes no negative values: a math function's
    /// result clamped at zero (§10.12).
    pub fn non_negative(self) -> ComputedLp {
        match self {
            ComputedLp::Px(v) => ComputedLp::Px(v.max(0.0)),
            ComputedLp::Percent(p) => ComputedLp::Percent(p.max(0.0)),
            ComputedLp::Calc(h) => ComputedLp::Calc(h.non_negative()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ComputedLpAuto {
    Px(f32),
    Percent(f32),
    Calc(CalcHandle),
    #[default]
    Auto,
}

impl ComputedLpAuto {
    pub fn resolve(self, basis: f32) -> Option<f32> {
        match self {
            ComputedLpAuto::Px(v) => Some(v),
            ComputedLpAuto::Percent(p) => Some(p / 100.0 * basis),
            ComputedLpAuto::Calc(h) => Some(h.resolve(basis)),
            ComputedLpAuto::Auto => None,
        }
    }
}

/// Size value: auto, length, percentage, or the keywords taffy understands.
#[derive(Debug, Clone, PartialEq)]
pub enum SizeValue {
    Auto,
    Length(Length),
    Percent(f32),
    Calc(Calc),
    MinContent,
    MaxContent,
    FitContent,
    /// `none` for max-width/max-height.
    None,
}

impl SizeValue {
    fn from_lp(lp: LengthPercentage) -> SizeValue {
        match lp {
            LengthPercentage::Length(l) => SizeValue::Length(l),
            LengthPercentage::Percent(p) => SizeValue::Percent(p),
            LengthPercentage::Calc(c) => SizeValue::Calc(c),
        }
    }

    pub fn to_computed(&self, ctx: &LengthContext) -> ComputedSize {
        match self {
            SizeValue::Auto => ComputedSize::Auto,
            SizeValue::Length(l) => ComputedSize::Px(l.to_px(ctx)),
            SizeValue::Percent(p) => ComputedSize::Percent(*p),
            // A size can not be negative: a math function's result is
            // clamped to the range (CSS Values 4 §10.12).
            SizeValue::Calc(c) => match LengthPercentage::Calc(c.clone()).to_computed(ctx) {
                ComputedLp::Px(v) => ComputedSize::Px(v.max(0.0)),
                ComputedLp::Percent(p) => ComputedSize::Percent(p.max(0.0)),
                ComputedLp::Calc(h) => ComputedSize::Calc(h.non_negative()),
            },
            SizeValue::MinContent => ComputedSize::MinContent,
            SizeValue::MaxContent => ComputedSize::MaxContent,
            SizeValue::FitContent => ComputedSize::FitContent,
            SizeValue::None => ComputedSize::None,
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            SizeValue::Auto => "auto".to_owned(),
            SizeValue::Length(l) => l.to_css(),
            SizeValue::Percent(p) => format!("{}%", css_number(*p)),
            SizeValue::Calc(c) => c.to_css(),
            SizeValue::MinContent => "min-content".to_owned(),
            SizeValue::MaxContent => "max-content".to_owned(),
            SizeValue::FitContent => "fit-content".to_owned(),
            SizeValue::None => "none".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ComputedSize {
    #[default]
    Auto,
    Px(f32),
    Percent(f32),
    /// A math expression with percentages, resolved against the basis
    /// (never below zero).
    Calc(CalcHandle),
    MinContent,
    MaxContent,
    FitContent,
    None,
}

/// sRGB color with alpha, all 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    pub const fn rgb8(r: u8, g: u8, b: u8) -> Rgba {
        Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    pub fn is_transparent(&self) -> bool {
        self.a <= 0.0
    }

    pub fn to_rgba8(&self) -> [u8; 4] {
        let f = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [f(self.r), f(self.g), f(self.b), f(self.a)]
    }

    /// Serialize per CSS Color 4: `rgb(r, g, b)`, or `rgba(r, g, b, a)`
    /// when not opaque (alpha with up to three decimals).
    pub fn to_css(&self) -> String {
        let [r, g, b, _] = self.to_rgba8();
        let a = self.a.clamp(0.0, 1.0);
        if a >= 1.0 {
            format!("rgb({r}, {g}, {b})")
        } else {
            let s = format!("{:.3}", a);
            let s = s.trim_end_matches('0').trim_end_matches('.');
            let s = if s.is_empty() { "0" } else { s };
            format!("rgba({r}, {g}, {b}, {s})")
        }
    }
}

/// Specified color: a concrete color, a named color (kept by name, as the
/// CSSOM serializes it), `transparent` or `currentcolor`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    Rgba(Rgba),
    /// Index into `NAMED_COLORS`.
    Named(u8),
    Transparent,
    CurrentColor,
}

impl Color {
    pub fn resolve(self, current: Rgba) -> Rgba {
        match self {
            Color::Rgba(c) => c,
            Color::Named(i) => named_rgba(i),
            Color::Transparent => Rgba::TRANSPARENT,
            Color::CurrentColor => current,
        }
    }

    /// Serialize the specified value as written (keywords stay keywords).
    pub fn to_css(self) -> String {
        match self {
            Color::Rgba(c) => c.to_css(),
            Color::Named(i) => NAMED_COLORS[i as usize].0.to_owned(),
            Color::Transparent => "transparent".to_owned(),
            Color::CurrentColor => "currentcolor".to_owned(),
        }
    }
}

fn named_rgba(i: u8) -> Rgba {
    let v = NAMED_COLORS[i as usize].1;
    Rgba::rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

// ----- parsing -----

pub fn parse_length<'i>(input: &mut Parser<'i, '_>) -> Result<Length, ParseErr<'i>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        Token::Dimension { value, unit, .. } => length_from_unit(value, &unit)
            .ok_or_else(|| location.new_unexpected_token_error(Token::Ident(unit))),
        Token::Number { value, .. } if value == 0.0 => Ok(Length::Px(0.0)),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn length_from_unit(value: f32, unit: &str) -> Option<Length> {
    Some(match_ignore_ascii_case! { unit,
        "px" => Length::Px(value),
        "em" => Length::Em(value),
        "rem" => Length::Rem(value),
        "vw" => Length::Vw(value),
        "vh" => Length::Vh(value),
        "vmin" => Length::Vmin(value),
        "vmax" => Length::Vmax(value),
        "ch" => Length::Ch(value),
        "ex" => Length::Ex(value),
        "pt" => Length::Pt(value),
        "pc" => Length::Pc(value),
        "in" => Length::In(value),
        "cm" => Length::Cm(value),
        "mm" => Length::Mm(value),
        "q" => Length::Q(value),
        _ => return None,
    })
}

pub fn parse_length_percentage<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<LengthPercentage, ParseErr<'i>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        Token::Dimension { value, unit, .. } => length_from_unit(value, &unit)
            .map(LengthPercentage::Length)
            .ok_or_else(|| location.new_unexpected_token_error(Token::Ident(unit))),
        Token::Percentage { unit_value, .. } => Ok(LengthPercentage::Percent(unit_value * 100.0)),
        Token::Number { value, .. } if value == 0.0 => Ok(LengthPercentage::ZERO),
        Token::Function(ref name) if is_math_function(name) => {
            let name = name.to_ascii_lowercase();
            let node = input.parse_nested_block(|i| parse_math_function(&name, i))?;
            if node.kind() != Some(CalcKind::Length) {
                return Err(location.new_custom_error(()));
            }
            Ok(LengthPercentage::Calc(Calc { node }))
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn is_math_function(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "calc" | "min" | "max" | "clamp" | "-webkit-calc")
}

/// The arguments of a math function, `name` its (lowercased) name.
fn parse_math_function<'i>(name: &str, input: &mut Parser<'i, '_>) -> Result<CalcNode, ParseErr<'i>> {
    match name {
        "calc" | "-webkit-calc" => Ok(CalcNode::Calc(Box::new(parse_calc_sum(input)?))),
        "min" | "max" => {
            let args = input.parse_comma_separated(parse_calc_sum)?;
            Ok(if name == "min" { CalcNode::Min(args) } else { CalcNode::Max(args) })
        }
        _ => {
            let a = parse_calc_sum(input)?;
            input.expect_comma()?;
            let b = parse_calc_sum(input)?;
            input.expect_comma()?;
            let c = parse_calc_sum(input)?;
            Ok(CalcNode::Clamp(Box::new([a, b, c])))
        }
    }
}

/// `<calc-sum> = <calc-product> [ [ '+' | '-' ] <calc-product> ]*`.
fn parse_calc_sum<'i>(input: &mut Parser<'i, '_>) -> Result<CalcNode, ParseErr<'i>> {
    let mut terms = vec![(false, parse_calc_product(input)?)];
    loop {
        let state = input.state();
        let neg = match input.next() {
            Ok(Token::Delim('+')) => false,
            Ok(Token::Delim('-')) => true,
            _ => {
                input.reset(&state);
                break;
            }
        };
        terms.push((neg, parse_calc_product(input)?));
    }
    Ok(if terms.len() == 1 { terms.pop().map(|t| t.1).unwrap_or(CalcNode::Number(0.0)) } else { CalcNode::Sum(terms) })
}

/// `<calc-product> = <calc-value> [ [ '*' | '/' ] <calc-value> ]*`.
fn parse_calc_product<'i>(input: &mut Parser<'i, '_>) -> Result<CalcNode, ParseErr<'i>> {
    let mut factors = vec![(false, parse_calc_value(input)?)];
    loop {
        let state = input.state();
        let divide = match input.next() {
            Ok(Token::Delim('*')) => false,
            Ok(Token::Delim('/')) => true,
            _ => {
                input.reset(&state);
                break;
            }
        };
        factors.push((divide, parse_calc_value(input)?));
    }
    Ok(if factors.len() == 1 { factors.pop().map(|f| f.1).unwrap_or(CalcNode::Number(1.0)) } else { CalcNode::Product(factors) })
}

/// `<calc-value>`: a number, length or percentage, a parenthesized sum,
/// a nested math function, or the constants `e` and `pi`.
fn parse_calc_value<'i>(input: &mut Parser<'i, '_>) -> Result<CalcNode, ParseErr<'i>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        Token::Number { value, .. } => Ok(CalcNode::Number(value)),
        Token::Percentage { unit_value, .. } => Ok(CalcNode::Percent(unit_value * 100.0)),
        Token::Dimension { value, unit, .. } => length_from_unit(value, &unit)
            .map(CalcNode::Length)
            .ok_or_else(|| location.new_unexpected_token_error(Token::Ident(unit))),
        Token::ParenthesisBlock => Ok(CalcNode::Calc(Box::new(input.parse_nested_block(parse_calc_sum)?))),
        Token::Function(ref name) if is_math_function(name) => {
            let name = name.to_ascii_lowercase();
            input.parse_nested_block(|i| parse_math_function(&name, i))
        }
        Token::Ident(ref id) if id.eq_ignore_ascii_case("e") => Ok(CalcNode::Number(std::f32::consts::E)),
        Token::Ident(ref id) if id.eq_ignore_ascii_case("pi") => Ok(CalcNode::Number(std::f32::consts::PI)),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

pub fn parse_length_percentage_auto<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<LengthPercentageAuto, ParseErr<'i>> {
    if input
        .try_parse(|i| i.expect_ident_matching("auto"))
        .is_ok()
    {
        return Ok(LengthPercentageAuto::Auto);
    }
    Ok(match parse_length_percentage(input)? {
        LengthPercentage::Length(l) => LengthPercentageAuto::Length(l),
        LengthPercentage::Percent(p) => LengthPercentageAuto::Percent(p),
        LengthPercentage::Calc(c) => LengthPercentageAuto::Calc(c),
    })
}

pub fn parse_size<'i>(input: &mut Parser<'i, '_>, allow_none: bool) -> Result<SizeValue, ParseErr<'i>> {
    if let Ok(ident) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        return Ok(match_ignore_ascii_case! { &ident,
            "auto" => SizeValue::Auto,
            "min-content" => SizeValue::MinContent,
            "max-content" => SizeValue::MaxContent,
            "fit-content" => SizeValue::FitContent,
            "none" if allow_none => SizeValue::None,
            _ => return Err(input.new_custom_error(())),
        });
    }
    Ok(SizeValue::from_lp(parse_length_percentage(input)?))
}

pub fn parse_number<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseErr<'i>> {
    Ok(input.expect_number()?)
}

/// `<number>` or `<percentage>` returning 0..=1 style factor for opacity.
pub fn parse_opacity<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseErr<'i>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Number { value, .. } => Ok(value.clamp(0.0, 1.0)),
        Token::Percentage { unit_value, .. } => Ok(unit_value.clamp(0.0, 1.0)),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

pub fn parse_color<'i>(input: &mut Parser<'i, '_>) -> Result<Color, ParseErr<'i>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        Token::Ident(name) => {
            if name.eq_ignore_ascii_case("currentcolor") {
                return Ok(Color::CurrentColor);
            }
            if name.eq_ignore_ascii_case("transparent") {
                return Ok(Color::Transparent);
            }
            // Chrome's focus ring colour (its `-webkit-focus-ring-color`
            // system colour: rgb(16, 16, 16) in light mode).
            if name.eq_ignore_ascii_case("-webkit-focus-ring-color") {
                return Ok(Color::Rgba(Rgba::rgb8(16, 16, 16)));
            }
            named_color(&name)
                .map(Color::Named)
                .ok_or_else(|| location.new_unexpected_token_error(Token::Ident(name)))
        }
        Token::IDHash(hex) | Token::Hash(hex) => parse_hex_color(&hex)
            .map(Color::Rgba)
            .ok_or_else(|| location.new_unexpected_token_error(Token::Hash(hex))),
        Token::Function(name) => {
            let name = name.to_string();
            input.parse_nested_block(|i| parse_color_function(&name, i))
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn parse_hex_color(hex: &str) -> Option<Rgba> {
    let h = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
    let hh = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    if !hex.is_ascii() {
        return None;
    }
    let (r, g, b, a) = match hex.len() {
        3 => (h(0)? * 17, h(1)? * 17, h(2)? * 17, 255),
        4 => (h(0)? * 17, h(1)? * 17, h(2)? * 17, h(3)? * 17),
        6 => (hh(0)?, hh(2)?, hh(4)?, 255),
        8 => (hh(0)?, hh(2)?, hh(4)?, hh(6)?),
        _ => return None,
    };
    Some(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    })
}

/// rgb()/rgba()/hsl()/hsla() in both legacy comma and modern space syntax.
fn parse_color_function<'i>(name: &str, input: &mut Parser<'i, '_>) -> Result<Color, ParseErr<'i>> {
    let is_rgb = match_ignore_ascii_case! { name,
        "rgb" | "rgba" => true,
        "hsl" | "hsla" => false,
        _ => return Err(input.new_custom_error(())),
    };

    // Component 1
    let c1 = parse_color_component(input, is_rgb, true)?;
    let legacy = input.try_parse(|i| i.expect_comma()).is_ok();
    let c2 = parse_color_component(input, is_rgb, false)?;
    if legacy {
        input.expect_comma()?;
    }
    let c3 = parse_color_component(input, is_rgb, false)?;

    let mut alpha = 1.0;
    if !input.is_exhausted() {
        if legacy {
            input.expect_comma()?;
        } else {
            input.expect_delim('/')?;
        }
        alpha = parse_opacity(input)?;
    }
    input.expect_exhausted()?;

    let (r, g, b) = if is_rgb {
        (c1, c2, c3)
    } else {
        hsl_to_rgb(c1, c2, c3)
    };
    Ok(Color::Rgba(Rgba {
        r: r.clamp(0.0, 1.0),
        g: g.clamp(0.0, 1.0),
        b: b.clamp(0.0, 1.0),
        a: alpha,
    }))
}

/// For rgb: returns 0..=1. For hsl: first is hue in degrees, others 0..=1.
fn parse_color_component<'i>(
    input: &mut Parser<'i, '_>,
    is_rgb: bool,
    first: bool,
) -> Result<f32, ParseErr<'i>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Number { value, .. } => {
            if is_rgb {
                Ok(value / 255.0)
            } else if first {
                Ok(value)
            } else {
                Ok(value / 100.0)
            }
        }
        Token::Percentage { unit_value, .. } => Ok(unit_value),
        Token::Dimension { value, unit, .. } if !is_rgb && first => {
            Ok(match_ignore_ascii_case! { &unit,
                "deg" => value,
                "grad" => value * 0.9,
                "rad" => value.to_degrees(),
                "turn" => value * 360.0,
                _ => return Err(location.new_unexpected_token_error(Token::Ident(unit))),
            })
        }
        Token::Ident(ref i) if i.eq_ignore_ascii_case("none") => Ok(0.0),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(360.0) / 360.0;
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

/// The index of a named color (case-insensitive), if there is one.
fn named_color(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    NAMED_COLORS
        .binary_search_by(|(n, _)| n.cmp(&lower.as_str()))
        .ok()
        .map(|i| i as u8)
}

/// CSS Color Level 4 named colors, sorted by name for binary search.
pub static NAMED_COLORS: [(&str, u32); 148] = [
    ("aliceblue", 0xF0F8FF),
    ("antiquewhite", 0xFAEBD7),
    ("aqua", 0x00FFFF),
    ("aquamarine", 0x7FFFD4),
    ("azure", 0xF0FFFF),
    ("beige", 0xF5F5DC),
    ("bisque", 0xFFE4C4),
    ("black", 0x000000),
    ("blanchedalmond", 0xFFEBCD),
    ("blue", 0x0000FF),
    ("blueviolet", 0x8A2BE2),
    ("brown", 0xA52A2A),
    ("burlywood", 0xDEB887),
    ("cadetblue", 0x5F9EA0),
    ("chartreuse", 0x7FFF00),
    ("chocolate", 0xD2691E),
    ("coral", 0xFF7F50),
    ("cornflowerblue", 0x6495ED),
    ("cornsilk", 0xFFF8DC),
    ("crimson", 0xDC143C),
    ("cyan", 0x00FFFF),
    ("darkblue", 0x00008B),
    ("darkcyan", 0x008B8B),
    ("darkgoldenrod", 0xB8860B),
    ("darkgray", 0xA9A9A9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xA9A9A9),
    ("darkkhaki", 0xBDB76B),
    ("darkmagenta", 0x8B008B),
    ("darkolivegreen", 0x556B2F),
    ("darkorange", 0xFF8C00),
    ("darkorchid", 0x9932CC),
    ("darkred", 0x8B0000),
    ("darksalmon", 0xE9967A),
    ("darkseagreen", 0x8FBC8F),
    ("darkslateblue", 0x483D8B),
    ("darkslategray", 0x2F4F4F),
    ("darkslategrey", 0x2F4F4F),
    ("darkturquoise", 0x00CED1),
    ("darkviolet", 0x9400D3),
    ("deeppink", 0xFF1493),
    ("deepskyblue", 0x00BFFF),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1E90FF),
    ("firebrick", 0xB22222),
    ("floralwhite", 0xFFFAF0),
    ("forestgreen", 0x228B22),
    ("fuchsia", 0xFF00FF),
    ("gainsboro", 0xDCDCDC),
    ("ghostwhite", 0xF8F8FF),
    ("gold", 0xFFD700),
    ("goldenrod", 0xDAA520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xADFF2F),
    ("grey", 0x808080),
    ("honeydew", 0xF0FFF0),
    ("hotpink", 0xFF69B4),
    ("indianred", 0xCD5C5C),
    ("indigo", 0x4B0082),
    ("ivory", 0xFFFFF0),
    ("khaki", 0xF0E68C),
    ("lavender", 0xE6E6FA),
    ("lavenderblush", 0xFFF0F5),
    ("lawngreen", 0x7CFC00),
    ("lemonchiffon", 0xFFFACD),
    ("lightblue", 0xADD8E6),
    ("lightcoral", 0xF08080),
    ("lightcyan", 0xE0FFFF),
    ("lightgoldenrodyellow", 0xFAFAD2),
    ("lightgray", 0xD3D3D3),
    ("lightgreen", 0x90EE90),
    ("lightgrey", 0xD3D3D3),
    ("lightpink", 0xFFB6C1),
    ("lightsalmon", 0xFFA07A),
    ("lightseagreen", 0x20B2AA),
    ("lightskyblue", 0x87CEFA),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xB0C4DE),
    ("lightyellow", 0xFFFFE0),
    ("lime", 0x00FF00),
    ("limegreen", 0x32CD32),
    ("linen", 0xFAF0E6),
    ("magenta", 0xFF00FF),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66CDAA),
    ("mediumblue", 0x0000CD),
    ("mediumorchid", 0xBA55D3),
    ("mediumpurple", 0x9370DB),
    ("mediumseagreen", 0x3CB371),
    ("mediumslateblue", 0x7B68EE),
    ("mediumspringgreen", 0x00FA9A),
    ("mediumturquoise", 0x48D1CC),
    ("mediumvioletred", 0xC71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xF5FFFA),
    ("mistyrose", 0xFFE4E1),
    ("moccasin", 0xFFE4B5),
    ("navajowhite", 0xFFDEAD),
    ("navy", 0x000080),
    ("oldlace", 0xFDF5E6),
    ("olive", 0x808000),
    ("olivedrab", 0x6B8E23),
    ("orange", 0xFFA500),
    ("orangered", 0xFF4500),
    ("orchid", 0xDA70D6),
    ("palegoldenrod", 0xEEE8AA),
    ("palegreen", 0x98FB98),
    ("paleturquoise", 0xAFEEEE),
    ("palevioletred", 0xDB7093),
    ("papayawhip", 0xFFEFD5),
    ("peachpuff", 0xFFDAB9),
    ("peru", 0xCD853F),
    ("pink", 0xFFC0CB),
    ("plum", 0xDDA0DD),
    ("powderblue", 0xB0E0E6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xFF0000),
    ("rosybrown", 0xBC8F8F),
    ("royalblue", 0x4169E1),
    ("saddlebrown", 0x8B4513),
    ("salmon", 0xFA8072),
    ("sandybrown", 0xF4A460),
    ("seagreen", 0x2E8B57),
    ("seashell", 0xFFF5EE),
    ("sienna", 0xA0522D),
    ("silver", 0xC0C0C0),
    ("skyblue", 0x87CEEB),
    ("slateblue", 0x6A5ACD),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xFFFAFA),
    ("springgreen", 0x00FF7F),
    ("steelblue", 0x4682B4),
    ("tan", 0xD2B48C),
    ("teal", 0x008080),
    ("thistle", 0xD8BFD8),
    ("tomato", 0xFF6347),
    ("turquoise", 0x40E0D0),
    ("violet", 0xEE82EE),
    ("wheat", 0xF5DEB3),
    ("white", 0xFFFFFF),
    ("whitesmoke", 0xF5F5F5),
    ("yellow", 0xFFFF00),
    ("yellowgreen", 0x9ACD32),
];

#[cfg(test)]
mod tests {
    use super::*;
    use cssparser::ParserInput;

    fn color(s: &str) -> Option<Rgba> {
        let mut input = ParserInput::new(s);
        let mut parser = Parser::new(&mut input);
        match parse_color(&mut parser) {
            Ok(Color::CurrentColor) | Err(_) => None,
            Ok(c) => Some(c.resolve(Rgba::BLACK)),
        }
    }

    fn lp(s: &str) -> Option<LengthPercentage> {
        let mut input = ParserInput::new(s);
        let mut parser = Parser::new(&mut input);
        parse_length_percentage(&mut parser).ok()
    }

    #[test]
    fn serializes_numbers_lengths_calc_and_colors() {
        assert_eq!(css_number(10.0), "10");
        assert_eq!(css_number(0.5), "0.5");
        assert_eq!(css_number(-0.0), "0");
        assert_eq!(css_number(0.123_456_8), "0.123457");
        assert_eq!(css_number(1.10), "1.1");
        assert_eq!(lp("10PT").map(|v| v.to_css()).as_deref(), Some("10pt"));
        assert_eq!(lp("0").map(|v| v.to_css()).as_deref(), Some("0px"));
        assert_eq!(lp("1.50%").map(|v| v.to_css()).as_deref(), Some("1.5%"));
        assert_eq!(lp("calc(1px + 2px)").map(|v| v.to_css()).as_deref(), Some("calc(3px)"));
        assert_eq!(lp("calc(10px - 5%)").map(|v| v.to_css()).as_deref(), Some("calc(-5% + 10px)"));
        assert_eq!(lp("calc(1em + 2px - 1px)").map(|v| v.to_css()).as_deref(), Some("calc(1em + 1px)"));
        let ctx = LengthContext { font_size: 10.0, root_font_size: 10.0, viewport_width: 100.0, viewport_height: 100.0 };
        assert_eq!(lp("calc(1px + 2px)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(3.0)));
        assert_eq!(lp("calc(1em + 1em)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(20.0)));
        assert_eq!(lp("calc(3em - 1em)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(20.0)));
        assert_eq!(lp("calc(1em + calc(1em + 1em))").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(30.0)));
        assert_eq!(lp("calc(1em + 1px)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Px(11.0)));
        assert_eq!(lp("calc(50%)").map(|v| v.to_computed(&ctx)), Some(ComputedLp::Percent(50.0)));
        let calc = |s: &str| match lp(s).map(|v| v.to_computed(&ctx)) {
            Some(ComputedLp::Calc(h)) => (h.to_css(), h.resolve(200.0)),
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(calc("calc(10px + 5%)"), ("calc(5% + 10px)".to_owned(), 20.0));
        assert_eq!(calc("calc(100% - 2em)"), ("calc(100% - 20px)".to_owned(), 180.0));
        assert_eq!(calc("min(1em, 10%)"), ("min(10px, 10%)".to_owned(), 10.0));
        assert_eq!(calc("max(1em, 10%)"), ("max(10px, 10%)".to_owned(), 20.0));
        assert_eq!(calc("clamp(5px, 50%, 2em)"), ("clamp(5px, 50%, 20px)".to_owned(), 20.0));
        assert_eq!(calc("calc(2 * (10% + 1px))"), ("calc(20% + 2px)".to_owned(), 42.0));
        assert_eq!(calc("calc((10% + 4px) / 2)"), ("calc(5% + 2px)".to_owned(), 12.0));
    }

    /// CSS Values 4 §10: math functions, typed, simplified and serialized
    /// as WPT's `minmax-length-percent-serialize.html` expects.
    #[test]
    fn math_functions_parse_simplify_and_serialize() {
        let ctx = LengthContext { font_size: 16.0, root_font_size: 16.0, viewport_width: 800.0, viewport_height: 600.0 };
        let specified = |s: &str| lp(s).map(|v| v.to_css()).unwrap_or_else(|| "invalid".to_owned());
        let computed = |s: &str| match lp(s).map(|v| v.to_computed(&ctx)) {
            Some(ComputedLp::Px(v)) => format!("{}px", css_number(v)),
            Some(ComputedLp::Percent(p)) => format!("{}%", css_number(p)),
            Some(ComputedLp::Calc(h)) => h.to_css(),
            None => "invalid".to_owned(),
        };
        let cases = [
            ("min(1px)", "calc(1px)", "1px"),
            ("max(1px)", "calc(1px)", "1px"),
            ("min(1% + 1px)", "calc(1% + 1px)", "calc(1% + 1px)"),
            ("min(1px + 1%)", "calc(1% + 1px)", "calc(1% + 1px)"),
            ("max(1px + 1%)", "calc(1% + 1px)", "calc(1% + 1px)"),
            ("min(1px, 2px)", "calc(1px)", "1px"),
            ("min(20px, 10%)", "min(20px, 10%)", "min(20px, 10%)"),
            ("min(1em, 10%)", "min(1em, 10%)", "min(16px, 10%)"),
            ("min(10%, 20px)", "min(10%, 20px)", "min(10%, 20px)"),
            ("min(10%, 1em)", "min(10%, 1em)", "min(10%, 16px)"),
            ("max(20px, 10%)", "max(20px, 10%)", "max(20px, 10%)"),
            ("max(1em, 10%)", "max(1em, 10%)", "max(16px, 10%)"),
            ("max(10%, 20px)", "max(10%, 20px)", "max(10%, 20px)"),
            ("max(10%, 1em)", "max(10%, 1em)", "max(10%, 16px)"),
            ("min(10% + 30px, 5em + 5%)", "min(10% + 30px, 5% + 5em)", "min(10% + 30px, 5% + 80px)"),
            ("max(10% + 30px, 5em + 5%)", "max(10% + 30px, 5% + 5em)", "max(10% + 30px, 5% + 80px)"),
            (
                "calc(min(10% + 1px) + max(1em + 10%) + min(10% + 20px))",
                "calc(30% + 1em + 21px)",
                "calc(30% + 37px)",
            ),
            ("calc(1em + max(10% + 20px) + 5% + min(1em + 10%) + 10px)", "calc(25% + 2em + 30px)", "calc(25% + 62px)"),
            (
                "max((min(10%, 30px) + 10px) * 2 + 10px, 5em + 5%)",
                "max(10px + (2 * (10px + min(10%, 30px))), 5% + 5em)",
                "max(10px + (2 * (10px + min(10%, 30px))), 5% + 80px)",
            ),
            // Wikipedia's icon size: font-relative and absolute units sum to
            // pixels when styles are computed.
            ("calc(max(calc(1rem + 4px), 10px))", "max(4px + 1rem, 10px)", "20px"),
            ("calc(1rem + 4px)", "calc(4px + 1rem)", "20px"),
            ("calc(1in + 1px)", "calc(97px)", "97px"),
            ("calc(10vw - 2 * 1em)", "calc(-2em + 10vw)", "48px"),
            ("clamp(10px, 1em, 12px)", "clamp(10px, 1em, 12px)", "12px"),
            ("clamp(20px, 1em, 12px)", "clamp(20px, 1em, 12px)", "20px"),
            ("clamp(10px, 11px, 12px)", "calc(11px)", "11px"),
            ("calc(100px / 4)", "calc(25px)", "25px"),
            ("calc(pi * 0)", "invalid", "invalid"),
            ("-webkit-calc(1px + 1px)", "calc(2px)", "2px"),
            // Typing (§10.8): a length plus a number, a length times a
            // length, a division by a length do not parse.
            ("calc(1px + 1)", "invalid", "invalid"),
            ("calc(1px * 1px)", "invalid", "invalid"),
            ("calc(1px / 1px)", "invalid", "invalid"),
            ("calc(2)", "invalid", "invalid"),
            ("min(1px, 1)", "invalid", "invalid"),
        ];
        for (input, spec, comp) in cases {
            assert_eq!(specified(input), spec, "specified {input}");
            assert_eq!(computed(input), comp, "computed {input}");
        }
        let c = |s: &str| {
            let mut input = ParserInput::new(s);
            let mut parser = Parser::new(&mut input);
            parse_color(&mut parser).map(|c| c.to_css()).ok()
        };
        assert_eq!(c("Red").as_deref(), Some("red"));
        assert_eq!(c("#ff0000").as_deref(), Some("rgb(255, 0, 0)"));
        assert_eq!(c("#00ff0080").as_deref(), Some("rgba(0, 255, 0, 0.502)"));
        assert_eq!(c("rgba(1, 2, 3, 0.5)").as_deref(), Some("rgba(1, 2, 3, 0.5)"));
        assert_eq!(c("transparent").as_deref(), Some("transparent"));
        assert_eq!(c("CurrentColor").as_deref(), Some("currentcolor"));
        assert_eq!(c("hsl(120, 100%, 50%)").as_deref(), Some("rgb(0, 255, 0)"));
    }

    #[test]
    fn named_colors_sorted() {
        for w in NAMED_COLORS.windows(2) {
            assert!(w[0].0 < w[1].0, "{} before {}", w[0].0, w[1].0);
        }
    }

    #[test]
    fn parses_colors() {
        assert_eq!(color("red").map(|c| c.to_rgba8()), Some([255, 0, 0, 255]));
        assert_eq!(color("#0f0").map(|c| c.to_rgba8()), Some([0, 255, 0, 255]));
        assert_eq!(color("#00ff0080").map(|c| c.to_rgba8()), Some([0, 255, 0, 128]));
        assert_eq!(color("rgb(1, 2, 3)").map(|c| c.to_rgba8()), Some([1, 2, 3, 255]));
        assert_eq!(color("rgba(1, 2, 3, 0.5)").map(|c| c.to_rgba8()), Some([1, 2, 3, 128]));
        assert_eq!(color("rgb(1 2 3 / 50%)").map(|c| c.to_rgba8()), Some([1, 2, 3, 128]));
        assert_eq!(color("hsl(120, 100%, 50%)").map(|c| c.to_rgba8()), Some([0, 255, 0, 255]));
        assert_eq!(color("hsl(0 0% 100%)").map(|c| c.to_rgba8()), Some([255, 255, 255, 255]));
        assert_eq!(color("transparent").map(|c| c.a), Some(0.0));
        assert!(color("notacolor").is_none());
    }
}
