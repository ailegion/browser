//! OS calls that no pure-Rust binding crate covers.
//!
//! This is the only crate in the workspace allowed to contain `unsafe`
//! (plan D01). Every unsafe block must carry a `// SAFETY:` comment and be
//! reviewed. As of Phase 0 nothing lives here; winit, wgpu, rfd and arboard
//! cover every OS interaction needed so far.

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
