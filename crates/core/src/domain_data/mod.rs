//! The per-domain key/value registry — spec §3.
//!
//! One store, and exactly one. `domain_data.update`, `collectors.add(data)` and
//! `cases.create(data)` are the same call with different ergonomics (§3.8); the
//! only thing a caller varies per key is scope, with the `@` sigil (§3.9).
//!
//! **Provenance is what makes a captured window evidence rather than a log
//! dump**, and the failure this is built against is that *wrong information is
//! worse than none*. Hence two timestamps rather than one, `unknown` carrying a
//! cause rather than a shrug, and expiry that never mutates what it reports on.

pub mod entry;
pub mod path;
pub mod persist;
pub mod store;

pub use entry::{parse_duration, render_duration, Entry, TtlParseError, TtlSpec};
pub use path::{Key, KeyError, MAX_KEYS, MAX_PATH_BYTES, MAX_SIGIL_KEYS, MAX_VALUE_BYTES};
pub use store::{
    DataEntry, DataOutcome, Outcome, RegistryError, RejectReason, RemoveOutcome, UnknownCause,
};
