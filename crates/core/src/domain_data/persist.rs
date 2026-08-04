//! Registry persistence — spec §3.5.
//!
//! `domain_data` is the durable-domain consumer the 2026-07-14 spec deferred
//! `persist=true` pending. It does **not** inherit a persistence seam; it
//! brings one.
//!
//! The collector precedent's fsync-outside-the-lock rule does not transfer
//! here, and saying it did was an earlier draft's error: nothing on any ingest
//! path reads `domain_data`, so a slow fsync cannot stall span ingest. The real
//! hazard is different — collector files are keyed `(owner, name)` and have one
//! logical writer, while these are keyed by **domain**, and many sessions bind
//! one domain by design.

use super::entry::Entry;
use super::store::Registry;
use crate::daemon::persistence::atomic_write;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

/// Bumped only when a change cannot be expressed additively. A file whose
/// version this build does not know is quarantined rather than guessed at.
pub const FORMAT_VERSION: u32 = 1;

/// Subdirectory under the config dir.
///
/// Not optional, and not cosmetic. `config` and `state` are both legal domain
/// names, so a flat layout would put `domain_data`'s `state.json` on top of the
/// daemon's — and `DaemonState` derives `Default` with `#[serde(default)]` on
/// its fields, so an arbitrary JSON object **parses successfully** as valid
/// state. The broker would come back on default ports having silently lost
/// every named session, trigger, filter and bookmark, without even reaching the
/// quarantine path.
pub const DOMAIN_DATA_DIR: &str = "domain_data";

#[derive(Debug, Serialize, Deserialize)]
pub struct PersistedRegistry {
    pub format_version: u32,
    /// The domain name as spelled, since the filename is case-folded.
    pub domain: String,
    pub entries: BTreeMap<String, Entry>,
}

/// FNV-1a, written out rather than taken from `DefaultHasher`.
///
/// `DefaultHasher`'s output is explicitly not stable across Rust releases, and
/// this hash lands in a **filename** — a toolchain upgrade would orphan every
/// registry on disk. FNV-1a is fixed by its own definition.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The file a domain's registry lives in.
///
/// Case-folded, with a short hash of the **original** name appended. Domain
/// names are case-sensitive (`is_valid_name` preserves case), so `Build` and
/// `build` are distinct domains — and on APFS and on Windows `Build.json` and
/// `build.json` are the same file. Two domains would silently overwrite each
/// other's provenance. Folding plus a hash of the unfolded name makes distinct
/// domains land in distinct files on a case-insensitive filesystem while
/// keeping the mapping deterministic.
pub fn file_name_for(domain: &str) -> String {
    // Low 32 bits, so the suffix is a fixed 8 characters. The collision space
    // this has to separate is tiny — case variants of one name — so a short
    // hash is ample, and a name a human can still recognise matters more.
    let short = (fnv1a(domain) & 0xffff_ffff) as u32;
    format!("{}-{short:08x}.json", domain.to_lowercase())
}

pub fn path_for(config_dir: &Path, domain: &str) -> PathBuf {
    config_dir.join(DOMAIN_DATA_DIR).join(file_name_for(domain))
}

/// One domain's registry, with the two locks §3.5 requires.
///
/// The lock order is the load-bearing rule, and an earlier draft stated it
/// ambiguously enough to describe the broken version:
///
/// ```text
/// acquire write mutex → acquire data lock → serialize → release data lock
///                     → atomic_write      → release write mutex
/// ```
///
/// Read as "the mutex may not enclose the data lock", the snapshot must be
/// taken *before* the mutex — and then T1 serializes v1, T2 serializes v2, T2
/// wins the mutex and writes v2, and T1 writes v1 after it. The older lands
/// last, silently dropping the newer key from disk while memory keeps serving
/// it, invisible until a restart. The mutex may enclose an acquisition of the
/// data lock; never the reverse.
#[derive(Debug)]
pub struct DomainRegistry {
    domain: String,
    path: PathBuf,
    data: RwLock<Registry>,
    write: Mutex<()>,
    /// When true, mutations stay in memory and never reach `path`.
    ///
    /// **This exists for postmortem domains, and it is not a convenience.** The
    /// registry file is keyed by domain NAME and outlives the domain — the
    /// module docs say so: "the registry for `X` survives while its file does,
    /// so a deleted domain that is re-created adopts its own history". Writing a
    /// three-month-old case's provenance into that file would leave it there
    /// after the postmortem domain is deleted, for the next live domain of the
    /// same name to adopt as its own. Merely *resolving* it also stamps the
    /// reader's broker version and today's `first_seen`.
    ///
    /// A loaded case's provenance describes a machine that is not this one. It
    /// belongs to the domain, not to the name.
    ephemeral: bool,
}

impl DomainRegistry {
    pub fn new(domain: String, path: PathBuf, registry: Registry) -> Self {
        Self {
            domain,
            path,
            data: RwLock::new(registry),
            write: Mutex::new(()),
            ephemeral: false,
        }
    }

    /// A registry that lives and dies with its domain — see [`Self::ephemeral`].
    pub fn in_memory(domain: String, registry: Registry) -> Self {
        Self {
            domain,
            path: PathBuf::new(),
            data: RwLock::new(registry),
            write: Mutex::new(()),
            ephemeral: true,
        }
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read under the data lock. Never taken while the write mutex is held by
    /// this thread for anything but the serialize step above.
    pub fn read<T>(&self, f: impl FnOnce(&Registry) -> T) -> T {
        let g = self.data.read().expect("domain_data lock poisoned");
        f(&g)
    }

    /// Mutate, then persist — the whole sequence under the write mutex, so two
    /// concurrent updates cannot reach `atomic_write` out of order.
    ///
    /// Returns the mutation's result alongside any persistence failure. The
    /// failure is **returned, not swallowed**: a caller that changed memory and
    /// could not durably record it must be able to say so, because the change
    /// will not survive a restart.
    pub fn mutate<T>(&self, f: impl FnOnce(&mut Registry) -> T) -> (T, Result<(), String>) {
        let _w = self.write.lock().expect("domain_data write lock poisoned");
        if self.ephemeral {
            // Not a swallowed failure: there is nothing to persist to, by
            // design, so `Ok` is the truthful answer rather than a shrug.
            let mut g = self.data.write().expect("domain_data lock poisoned");
            return (f(&mut g), Ok(()));
        }
        let (out, bytes) = {
            let mut g = self.data.write().expect("domain_data lock poisoned");
            let out = f(&mut g);
            let snapshot = PersistedRegistry {
                format_version: FORMAT_VERSION,
                domain: self.domain.clone(),
                entries: g.entries().clone(),
            };
            (out, serde_json::to_vec_pretty(&snapshot))
        };
        let saved = bytes
            .map_err(|e| format!("serialize: {e}"))
            .and_then(|b| atomic_write(&self.path, &b).map_err(|e| format!("write: {e}")));
        (out, saved)
    }
}

/// Load one domain's registry, quarantining a file that cannot be read.
///
/// A missing file is a new registry, not an error. An unreadable one is
/// quarantined and the registry starts empty **with the quarantine recorded**,
/// because that artifact is the only thing that distinguishes a lost registry
/// from a never-written one — which is what `unknown`'s cause leans on.
pub fn load(path: &Path) -> Registry {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Registry::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "domain_data unreadable");
            let mut r = Registry::new();
            r.note_quarantined();
            return r;
        }
    };
    match serde_json::from_slice::<PersistedRegistry>(&raw) {
        Ok(p) if p.format_version <= FORMAT_VERSION => Registry::from_entries(p.entries),
        Ok(p) => {
            tracing::warn!(
                path = %path.display(),
                found = p.format_version,
                known = FORMAT_VERSION,
                "domain_data written by a newer build; quarantining rather than guessing"
            );
            quarantine_and_note(path)
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "domain_data corrupt");
            quarantine_and_note(path)
        }
    }
}

fn quarantine_and_note(path: &Path) -> Registry {
    let mut r = Registry::new();
    if crate::daemon::persistence::quarantine(path).is_some() {
        r.note_quarantined();
    }
    r
}

/// Logmon's own keys (§3.7), refreshed at boot.
///
/// `/logmon/version` moves only `validated_at` on a same-version boot and both
/// timestamps on an upgrade, which is ordinary §3.2 machinery rather than the
/// "refreshed on boot" special case an earlier draft needed. The key then reads
/// *"0.9.0 — created 3 days ago, validated 2 minutes ago"*: on this version for
/// three days, and running now.
pub fn stamp_boot(reg: &DomainRegistry, version: &str, now: DateTime<Utc>) -> Result<(), String> {
    let domain = reg.domain().to_string();
    let is_new = reg.read(|r| r.entries().is_empty());
    let (_, saved) = reg.mutate(|r| {
        let _ = r.set_reserved("/logmon/domain", domain, now);
        let _ = r.set_reserved("/logmon/version", version.to_string(), now);
        if is_new {
            let _ = r.set_reserved("/logmon/first_seen", now.to_rfc3339(), now);
            let _ = r.set_reserved("/logmon/incarnation", "1".into(), now);
            let _ = r.set_reserved("/logmon/incarnation_started", now.to_rfc3339(), now);
        }
    });
    saved
}

/// Count a new era — spec §3.5.
///
/// **Anchored to the seq origin, not to domain creation**, and the difference
/// is not cosmetic. `default` is created on every boot and its seq never
/// restarts (it is seeded from the persisted `seq_block`), so counting
/// creations would report "incarnation 47" on the one domain that was never
/// reused. Config-declared domains restart seq at 0 on **every** boot with no
/// re-creation at all, so the hazard fires without the event an earlier draft
/// keyed on.
///
/// The event that makes two seq ranges incomparable is a counter seeded at 0
/// for a name whose registry already exists. That is the only thing this
/// counter is for.
pub fn note_seq_origin(reg: &DomainRegistry, now: DateTime<Utc>) -> Result<(), String> {
    let previous = reg.read(|r| {
        r.get("/logmon/incarnation")
            .and_then(|e| e.value.parse::<u64>().ok())
    });
    let next = match previous {
        // No prior registry: `stamp_boot` seeds era 1. Nothing to advance.
        None => return Ok(()),
        Some(n) => n.saturating_add(1),
    };
    let (_, saved) = reg.mutate(|r| {
        let _ = r.set_reserved("/logmon/incarnation", next.to_string(), now);
        let _ = r.set_reserved("/logmon/incarnation_started", now.to_rfc3339(), now);
    });
    saved
}

#[cfg(test)]
mod tests;
