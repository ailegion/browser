//! The state an `<img>` element carries for its image (HTML §4.8.4.3,
//! "Working sites first" 6m): its current and pending image requests,
//! the last selected source, the dimension attribute source and the last
//! auto-sizes width. The algorithms that drive it (source selection,
//! "update the image data", reacting to environment changes) are the
//! style crate's (`browser_style::responsive`), which can evaluate media
//! conditions and lengths; the tab and the script bindings run them.
//! Layout reads the current request, the cascade the dimension
//! attribute source, `currentSrc` the current URL.

use url::Url;

use crate::NodeId;

/// An image request's state (HTML §4.8.4.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestState {
    #[default]
    Unavailable,
    PartiallyAvailable,
    CompletelyAvailable,
    Broken,
}

/// An image request: its current URL (what `currentSrc` returns; the
/// selected source itself when it did not parse as a URL), its current
/// pixel density, its state, and the URL it fetches.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRequest {
    pub current_url: String,
    pub density: f64,
    pub state: RequestState,
    /// The parsed URL the request fetches or fetched; `None` for a
    /// request that never fetches (the initial one, a broken one).
    pub url: Option<Url>,
    /// Its fetch waits for the element to be rendered (an `<img>` that
    /// allows auto-sizes: lazy loading's deferral) and does not delay the
    /// document's load event.
    pub deferred: bool,
}

impl Default for ImageRequest {
    fn default() -> Self {
        Self {
            current_url: String::new(),
            density: 1.0,
            state: RequestState::Unavailable,
            url: None,
            deferred: false,
        }
    }
}

/// Which algorithm made the pending request: it decides what its fetch's
/// outcome does (HTML §4.8.4.3.5 against §4.8.4.3.13).
#[derive(Debug, Clone, PartialEq)]
pub enum PendingKind {
    /// "Update the image data": a failure replaces the current request.
    Update,
    /// "React to environment changes", with the selected source and
    /// density it records once done; a failure only drops it.
    Environment { selected: String, density: f64 },
}

/// An `<img>` element's image state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageData {
    pub current: ImageRequest,
    pub pending: Option<(ImageRequest, PendingKind)>,
    pub last_selected: Option<String>,
    /// The element whose `width`/`height` attributes size the `<img>`:
    /// itself, or the selected `<source>` of its `<picture>` that has
    /// them.
    pub dimension_source: Option<NodeId>,
    pub last_auto_width: Option<f32>,
    /// The concrete width the tab last saw this `<img>` rendered at, for
    /// an `<img>` that allows auto-sizes: a new one is a relevant
    /// mutation (it started being rendered, or its width changed).
    pub rendered_width_seen: Option<f32>,
    /// Bumped by every run of "update the image data"; a run whose
    /// microtask finds a later one started returns (§4.8.4.3.5 step 10).
    pub generation: u64,
    /// Bumped by every relevant mutation; an environment update started
    /// before one is dropped when it finishes (§4.8.4.3.13 step 16.1).
    pub mutations: u64,
    /// The mutation count an environment update started at.
    pub env_started_at: u64,
    /// The `maybe omit events` flag of the run in progress.
    pub omit_events: bool,
    /// The current URL before the run in progress (`previousURL`).
    pub previous_url: String,
}

/// A task "update the image data" queues on the DOM manipulation task
/// source: set the current request's current URL (when given), then fire
/// the event unless the flags say otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageTask {
    pub element: NodeId,
    pub set_current_url: Option<String>,
    /// `load` or `error`; `None` when only the URL changes.
    pub event: Option<&'static str>,
}
