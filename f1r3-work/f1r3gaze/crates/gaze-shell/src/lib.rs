//! `gaze-shell` — the F1R3Gaze application (spec §3, WP C1, K1).
//!
//! * [`engine`]: what a profile's tabs share (HTTP, blobs, the shard bridge,
//!   the broker) and each tab's services.
//! * [`tab`]: the load pipeline — fetch, parse, verify scripts, plan grants,
//!   prompt, run.
//! * `chrome` (feature `window`): the window's own document — tab strip,
//!   address bar, prompt bar, grants and console panels — hosting each tab's
//!   `RhoDocument` as a sub-document.
//! * [`headless`]: the same pipeline without a window.

pub mod engine;
pub mod headless;
pub mod pages;
pub mod profile;
pub mod tab;

#[cfg(feature = "window")]
pub mod chrome;

pub use engine::Engine;
