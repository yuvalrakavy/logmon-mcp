//! Span time collectors.
//!
//! See `docs/superpowers/specs/2026-07-29-span-time-collector-design.md`.
//!
//! Collect broadly; decide at read time. A collector retains a compact record
//! per matched span, and every metric is a read-time projection over it — so
//! the shape of the question does not have to be known when collection starts.

pub mod diff;
pub mod document;
pub mod exact;
pub mod history;
pub mod intern;
pub mod persist;
pub mod project;
pub mod registry;
pub mod sample;
pub mod sketch;
pub mod state;
