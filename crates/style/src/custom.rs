//! Custom properties (`--name`) and `var()` substitution, CSS Variables 1.
//!
//! A custom property's value is kept as raw text. A standard property whose
//! value mentions `var()` is kept as raw text too (`DeclaredValue::Pending`)
//! and parsed per element, once the element's custom properties are known:
//! `expand_pending` substitutes the references and runs the normal parser.
//! A reference with no value and no fallback, a cycle, or a value that does
//! not parse after substitution makes the declaration invalid at
//! computed-value time, which the spec defines as `unset`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cssparser::{ParseError, Parser, ParserInput, SourcePosition, Token};

use crate::properties::{CustomValue, DeclaredValue, longhands_of, parse_property};

/// An element's custom properties, name (with the leading dashes) to value.
/// Shared between an element and its children when the child declares none.
pub type CustomMap = HashMap<Arc<str>, Arc<str>>;

/// Reference nesting allowed in fallbacks and chains.
const MAX_DEPTH: usize = 16;
/// Cap on a substituted value, so a page cannot expand a few kilobytes of
/// nested references into gigabytes (the spec allows UA limits here).
const MAX_LEN: usize = 64 * 1024;

/// Cheap parse-time check for `var(` anywhere in a value.
pub fn contains_var(raw: &str) -> bool {
    raw.as_bytes().windows(4).any(|w| w.eq_ignore_ascii_case(b"var("))
}

/// Replace every `var()` in `raw` with the value `lookup` gives for its
/// name, or its fallback. `None` when a reference has neither, or a limit
/// is exceeded. Text between references is copied verbatim.
pub fn substitute(raw: &str, lookup: &mut dyn FnMut(&str) -> Option<Arc<str>>) -> Option<String> {
    let mut out = String::new();
    substitute_into(raw, lookup, &mut out, 0).then_some(out)
}

fn substitute_into(
    raw: &str,
    lookup: &mut dyn FnMut(&str) -> Option<Arc<str>>,
    out: &mut String,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let mut input = ParserInput::new(raw);
    let mut parser = Parser::new(&mut input);
    let mut last = parser.position();
    if walk(&mut parser, lookup, out, &mut last, depth).is_err() {
        return false;
    }
    out.push_str(parser.slice_from(last));
    out.len() <= MAX_LEN
}

/// Walk tokens, descending into blocks, copying source up to each `var()`
/// and appending its substitution. `last` is the source position copied so
/// far; positions are absolute in the input, so nested parsers share it.
fn walk<'i>(
    input: &mut Parser<'i, '_>,
    lookup: &mut dyn FnMut(&str) -> Option<Arc<str>>,
    out: &mut String,
    last: &mut SourcePosition,
    depth: usize,
) -> Result<(), ()> {
    loop {
        // Whitespace and comments come through as tokens so `start` is
        // exactly where `var(` begins and the text before it is kept.
        let start = input.position();
        let token = match input.next_including_whitespace_and_comments() {
            Ok(t) => t.clone(),
            Err(_) => return Ok(()),
        };
        match token {
            Token::Function(name) if name.eq_ignore_ascii_case("var") => {
                out.push_str(input.slice(*last..start));
                input
                    .parse_nested_block(|i| {
                        let name = i.expect_ident()?.to_string();
                        if !name.starts_with("--") {
                            return Err(i.new_custom_error(()));
                        }
                        let fallback = if i.is_exhausted() {
                            None
                        } else {
                            i.expect_comma()?;
                            let s = i.position();
                            while i.next().is_ok() {}
                            Some(i.slice_from(s))
                        };
                        match lookup(&name) {
                            Some(v) => {
                                out.push_str(&v);
                                Ok(())
                            }
                            None => match fallback {
                                Some(fb) if substitute_into(fb, lookup, out, depth + 1) => Ok(()),
                                _ => Err(i.new_custom_error(())),
                            },
                        }
                    })
                    .map_err(|_: ParseError<'i, ()>| ())?;
                *last = input.position();
                if out.len() > MAX_LEN {
                    return Err(());
                }
            }
            Token::Function(_) | Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                input
                    .parse_nested_block(|i| walk(i, lookup, out, last, depth).map_err(|()| i.new_custom_error(())))
                    .map_err(|_: ParseError<'i, ()>| ())?;
            }
            _ => {}
        }
    }
}

/// The custom properties of an element: the parent's, overridden by the
/// element's own declarations (already in cascade order, later wins), with
/// references between them resolved. A property in a reference cycle, or
/// referencing a missing property without fallback, is dropped.
pub fn resolve_customs(parent: &Arc<CustomMap>, declared: &[(&Arc<str>, &CustomValue)]) -> Arc<CustomMap> {
    if declared.is_empty() {
        return parent.clone();
    }
    let mut own: HashMap<&str, &CustomValue> = HashMap::with_capacity(declared.len());
    for (name, value) in declared {
        own.insert(name, value);
    }
    let mut resolver = Resolver {
        map: (**parent).clone(),
        pending: HashMap::new(),
        in_progress: HashSet::new(),
        invalid: HashSet::new(),
    };
    for (name, value) in own {
        match value {
            CustomValue::Initial => {
                resolver.map.remove(name);
            }
            CustomValue::Inherit => {}
            CustomValue::Raw(raw) => {
                if contains_var(raw) {
                    resolver.pending.insert(name, raw);
                } else {
                    resolver.map.insert(Arc::from(name), raw.clone());
                }
            }
        }
    }
    let names: Vec<&str> = resolver.pending.keys().copied().collect();
    for name in names {
        resolver.get(name);
    }
    Arc::new(resolver.map)
}

struct Resolver<'a> {
    map: CustomMap,
    /// Own declarations still to resolve, by name.
    pending: HashMap<&'a str, &'a Arc<str>>,
    in_progress: HashSet<&'a str>,
    invalid: HashSet<&'a str>,
}

impl<'a> Resolver<'a> {
    fn get(&mut self, name: &str) -> Option<Arc<str>> {
        let Some((&key, &raw)) = self.pending.get_key_value(name) else {
            if self.invalid.contains(name) {
                return None;
            }
            return self.map.get(name).cloned();
        };
        if self.in_progress.contains(key) {
            // A cycle: every member becomes invalid as they unwind.
            return None;
        }
        self.in_progress.insert(key);
        let result = substitute(raw, &mut |n| self.get(n));
        self.in_progress.remove(key);
        self.pending.remove(key);
        match result {
            Some(s) => {
                let v: Arc<str> = Arc::from(s);
                self.map.insert(Arc::from(key), v.clone());
                Some(v)
            }
            None => {
                self.invalid.insert(key);
                self.map.remove(key);
                None
            }
        }
    }
}

/// Substitute and parse a pending declaration. Invalid at computed-value
/// time (missing reference, cycle, or bad value) yields `unset` for each of
/// the property's longhands, as the spec requires.
pub fn expand_pending(name: &str, raw: &str, custom: &CustomMap) -> Vec<DeclaredValue> {
    let parsed = substitute(raw, &mut |n| custom.get(n).cloned()).and_then(|value| {
        let mut input = ParserInput::new(&value);
        let mut parser = Parser::new(&mut input);
        parse_property(name, &mut parser).ok()
    });
    match parsed {
        Some(values) => values,
        None => longhands_of(name)
            .unwrap_or_default()
            .into_iter()
            .map(DeclaredValue::Unset)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> CustomMap {
        pairs.iter().map(|(k, v)| (Arc::from(*k), Arc::from(*v))).collect()
    }

    fn subst(raw: &str, m: &CustomMap) -> Option<String> {
        substitute(raw, &mut |n| m.get(n).cloned())
    }

    #[test]
    fn substitutes_plain_nested_and_fallback() {
        let m = map(&[("--a", "red"), ("--n", "3")]);
        assert_eq!(subst("var(--a)", &m).as_deref(), Some("red"));
        assert_eq!(subst("1px solid var(--a)", &m).as_deref(), Some("1px solid red"));
        assert_eq!(subst("rgb(var(--n), 0, calc(var(--n) * 2))", &m).as_deref(), Some("rgb(3, 0, calc(3 * 2))"));
        // A fallback keeps its leading space, like the token stream it is.
        assert_eq!(subst("var(--x, blue)", &m).as_deref(), Some(" blue"));
        assert_eq!(subst("var(--x,var(--a))", &m).as_deref(), Some("red"));
        assert_eq!(subst("var(--x, a, b)", &m).as_deref(), Some(" a, b"));
        assert_eq!(subst("VAR( --a )", &m).as_deref(), Some("red"));
        assert_eq!(subst("var(--x)", &m), None);
        assert_eq!(subst("var(a)", &m), None);
        assert_eq!(subst("var(--x, var(--y))", &m), None);
    }

    #[test]
    fn resolves_chains_and_cycles() {
        let a: Arc<str> = Arc::from("--a");
        let b: Arc<str> = Arc::from("--b");
        let c: Arc<str> = Arc::from("--c");
        let parent = Arc::new(map(&[("--p", "10px"), ("--a", "old")]));
        let va = CustomValue::Raw(Arc::from("var(--b)"));
        let vb = CustomValue::Raw(Arc::from("calc(var(--p) + 1px)"));
        let vc = CustomValue::Initial;
        let m = resolve_customs(&parent, &[(&a, &va), (&b, &vb), (&c, &vc)]);
        assert_eq!(m.get("--a").map(|v| &**v), Some("calc(10px + 1px)"));
        assert_eq!(m.get("--p").map(|v| &**v), Some("10px"));

        let cyc_a = CustomValue::Raw(Arc::from("var(--b)"));
        let cyc_b = CustomValue::Raw(Arc::from("var(--a)"));
        let m = resolve_customs(&parent, &[(&a, &cyc_a), (&b, &cyc_b)]);
        assert!(m.get("--a").is_none(), "cycle members are invalid, even over an inherited value");
        assert!(m.get("--b").is_none());
    }

    #[test]
    fn expansion_limits_hold() {
        // Each level doubles: 2^40 would be terabytes.
        let mut pairs: Vec<(String, String)> = vec![("--v0".into(), "x".repeat(1000))];
        for i in 1..40 {
            pairs.push((format!("--v{i}"), format!("var(--v{}) var(--v{})", i - 1, i - 1)));
        }
        let parent = Arc::new(CustomMap::new());
        let names: Vec<Arc<str>> = pairs.iter().map(|(k, _)| Arc::from(k.as_str())).collect();
        let values: Vec<CustomValue> = pairs.iter().map(|(_, v)| CustomValue::Raw(Arc::from(v.as_str()))).collect();
        let declared: Vec<(&Arc<str>, &CustomValue)> = names.iter().zip(&values).collect();
        let m = resolve_customs(&parent, &declared);
        assert!(m.values().all(|v| v.len() <= MAX_LEN));
    }
}
