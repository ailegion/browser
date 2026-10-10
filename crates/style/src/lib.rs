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
pub mod serialize;
pub mod state;
pub mod stylesheet;
pub mod ua;
pub mod values;

pub use cascade::{Restyled, StateChange, StyleMap, Stylist, compute_styles, compute_styles_with, restyle};
pub use state::{ElementStates, InteractionDeps, Reach, Scope, StateKind, StateRule, SubjectKey, SubjectKeys};
pub use computed::{ComputedStyle, LineHeight, Viewport};
pub use properties::*;
pub use media::MediaQueryList;
pub use serialize::{UsedValues, resolved_value, serialize_block, serialize_declared, serialize_shorthand};
pub use stylesheet::{
    Origin, Rule, StyleRule, Stylesheet, parse_cssom_block, parse_declaration_block, parse_declaration_value,
};
pub use values::{ComputedLp, ComputedLpAuto, ComputedSize, Rgba, css_number};

/// Parse error type used by all value parsers; the custom payload is unit
/// because a failed declaration is simply dropped.
pub type ParseErr<'i> = cssparser::ParseError<'i, ()>;
