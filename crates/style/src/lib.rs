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
pub mod content;
pub mod custom;
pub mod fontface;
pub mod grid;
pub mod image;
pub mod mask;
pub mod media;
pub mod properties;
pub mod selector_impl;
pub mod serialize;
pub mod state;
pub mod stylesheet;
pub mod transform;
pub mod ua;
pub mod values;

pub use cascade::{
    FeatureChange, GeneratedPseudo, Restyled, StateChange, StyleMap, Stylist, TreeChange, compute_styles, compute_styles_with,
    generates_pseudo_elements, pseudo_element_style, restyle, restyle_mutations, sync_pseudo_elements,
};
pub use content::{
    ComputedContent, CounterEntry, CounterList, CounterStyle, GeneratedItem, Quotes, auto_quotes,
};
pub use state::{ElementStates, Feature, InteractionDeps, MutationRule, Reach, Scope, StateKind, StateRule, SubjectKey, SubjectKeys};
pub use computed::{
    AppliedDecoration, ComputedBackgroundSize, ComputedPosition, ComputedStyle, ComputedVerticalAlign, DecorationFont,
    DecorationThickness, LetterSpacing, LineHeight, PositionOffset, PseudoStyles, UnderlineOffset, Viewport, ViewportOverflow,
    viewport_overflow,
};
pub use image::{
    ColorInterpolation, ComputedCandidate, ComputedGradient, ComputedGradientItem, ComputedGradientPos,
    ComputedGradientPosition, ComputedGradientShape, ComputedImage, ComputedPositionComponent, ComputedRadialSize,
    Gradient, GradientItem, GradientPos, GradientShape, HueMethod, ImageSetCandidate, InterpolationSpace,
    LinearDirection, RadialShape, RadialSize,
};
pub use properties::*;
pub use media::MediaQueryList;
pub use serialize::{UsedGrid, UsedTracks, UsedValues, resolved_value, serialize_block, serialize_declared, serialize_shorthand};
pub use stylesheet::{
    Origin, Rule, StyleRule, Stylesheet, parse_cssom_block, parse_declaration_block, parse_declaration_value,
};
pub use values::{CalcHandle, ComputedLp, ComputedLpAuto, ComputedSize, Rgba, css_number};

/// Parse error type used by all value parsers; the custom payload is unit
/// because a failed declaration is simply dropped.
pub type ParseErr<'i> = cssparser::ParseError<'i, ()>;
