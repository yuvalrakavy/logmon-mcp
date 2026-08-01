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
pub mod map;
pub mod path;
pub mod persist;
pub mod store;

pub use entry::{parse_duration, render_duration, Entry, TtlParseError, TtlSpec};
pub use map::DomainDataStore;
pub use path::{Key, KeyError, MAX_KEYS, MAX_PATH_BYTES, MAX_SIGIL_KEYS, MAX_VALUE_BYTES};
pub use store::{
    is_scoped, partition_scoped, scope_one, DataEntry, DataOutcome, Outcome, Registry,
    RegistryError, RejectReason, RemoveOutcome, ScopedData, ScopedFact, UnknownCause,
};

/// The recommended core keys (§3.6.1) — the set a document missing which
/// cannot be acted on.
///
/// Reported against, never enforced: `update` accepts any path and warns about
/// none. Coverage is a property of the *document*, not of the registry, and it
/// exists because a convention that lives only in prose is a capability nobody
/// reaches for.
pub const CORE_KEYS: [&str; 3] = ["/Build/commit", "/Build/profile", "/Action"];

/// The registry key holding this project's case-document filename prefix
/// (§5.3). The one key logmon *reads* rather than only renders.
pub const CASE_NAME_KEY: &str = "/case-name";
