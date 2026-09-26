//! Style engine. See plan/02-architecture.md, section "Style engine".
//!
//! Pipeline: `Stylesheet::parse` turns CSS text into rules with selector
//! lists (via the `selectors` crate) and typed declarations (our property
//! table). `compute_styles` matches rules against the DOM, orders the
//! matching declarations by origin, importance, specificity and source
//! order, and produces a `ComputedStyle` per element with inheritance and
//! lengths resolved to pixels.

#![forbid(unsafe_code)]

pub mod cascade;
pub mod computed;
pub mod custom;
pub mod media;
pub mod properties;
pub mod selector_impl;
pub mod stylesheet;
pub mod ua;
pub mod values;

pub use cascade::{StyleMap, Stylist, compute_styles};
pub use computed::{ComputedStyle, LineHeight, Viewport};
pub use properties::*;
pub use media::MediaQueryList;
pub use stylesheet::{Origin, Rule, StyleRule, Stylesheet, parse_declaration_block};
pub use values::{ComputedLp, ComputedLpAuto, ComputedSize, Rgba};

/// Parse error type used by all value parsers; the custom payload is unit
/// because a failed declaration is simply dropped.
pub type ParseErr<'i> = cssparser::ParseError<'i, ()>;
