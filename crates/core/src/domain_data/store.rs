//! The registry itself — spec §3.3, §3.4, §3.6.2.

use super::entry::{parse_duration, Entry, TtlParseError, TtlSpec};
use super::path::{
    is_reserved, matches_prefix, Key, KeyError, MAX_KEYS, MAX_SIGIL_KEYS, MAX_VALUE_BYTES,
};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// One requested change.
///
/// **Absent ≡ `null` ≡ unchanged, for both `value` and `ttl`**, and the two
/// share one rule deliberately. Two optional fields on one object with opposite
/// absent semantics is precisely the footgun this is built to avoid — and a
/// client serialising a missing field as `null` would otherwise mean the
/// opposite of what it sent, which is the absent-vs-zero trap that produced
/// three defects in 0.8.0.
///
/// Value absent → validate only. Erasure is `remove`, never `set(null)`.
#[derive(Debug, Clone, Default)]
pub struct DataEntry {
    pub path: String,
    pub value: Option<String>,
    pub ttl: Option<TtlSpec>,
}

/// What happened to one entry. An enum rather than prose, because the
/// difference between a caller that can branch and one that string-matches is
/// this type existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Updated,
    Validated,
    Unknown { cause: UnknownCause },
    Rejected { reason: RejectReason },
}

/// Why a key-only entry found nothing.
///
/// Three, not two. An earlier draft offered `NeverSet` and `NoRegistry` as
/// though they were distinguishable — but `/logmon/first_seen` is written
/// identically for a brand-new domain and for one whose file was lost or
/// quarantined, so from the registry alone they are not. `Undetermined` is the
/// honest default; claiming `NeverSet` for both would be exactly the
/// confident-shaped guess this module exists to refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownCause {
    /// The registry exists and this key is not in it.
    NeverSet,
    /// A quarantine artifact proves the file was lost.
    NoRegistry,
    /// No registry, and nothing establishes why.
    Undetermined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    MalformedPath,
    PathTooLong,
    /// A `value` that was present but not a string. Rejected rather than read
    /// as absent, because absent means *validate* — so a number would silently
    /// confirm the old value and report success for a set that never happened.
    MalformedValue,
    ValueTooLong,
    ReservedPrefix,
    SigilNotAllowedHere,
    MalformedTtl,
    RegistryFull,
}

impl RejectReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MalformedPath => "malformed_path",
            Self::PathTooLong => "path_too_long",
            Self::MalformedValue => "malformed_value",
            Self::ValueTooLong => "value_too_long",
            Self::ReservedPrefix => "reserved_prefix",
            Self::SigilNotAllowedHere => "sigil_not_allowed_here",
            Self::MalformedTtl => "malformed_ttl",
            Self::RegistryFull => "registry_full",
        }
    }
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Validated => "validated",
            Self::Unknown { .. } => "unknown",
            Self::Rejected { .. } => "rejected",
        }
    }
}

impl UnknownCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NeverSet => "never_set",
            Self::NoRegistry => "no_registry",
            Self::Undetermined => "undetermined",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataOutcome {
    /// The key as the caller spelled it, sigil included.
    pub path: String,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub pattern: String,
    pub removed: usize,
    pub rejected: Option<RejectReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    Reserved,
}

/// A domain's registry: agent keys and logmon's own.
///
/// **Document-scoped (`@`) keys never reach it.** They belong to one capture,
/// and the registry is long-lived and shared — holding them here would let the
/// next case on the domain inherit the last one's assertions, which is the
/// laundering the sigil exists to prevent. [`partition_scoped`] splits them off
/// before this type sees them, and [`ScopedData`] carries them to the document.
///
/// `BTreeMap` rather than a hash map so iteration order is the archive's order
/// — a document rendered twice from one registry must be byte-identical, and a
/// diff between two documents must not be noise.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
    /// True once anything establishes that a prior file was lost — currently a
    /// quarantine artifact found at load. Drives [`UnknownCause`].
    saw_quarantine: bool,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries(entries: BTreeMap<String, Entry>) -> Self {
        Self {
            entries,
            saw_quarantine: false,
        }
    }

    /// Record that a quarantine artifact was found, which is the only evidence
    /// that distinguishes a lost registry from a new one (§3.3).
    pub fn note_quarantined(&mut self) {
        self.saw_quarantine = true;
    }

    pub fn entries(&self) -> &BTreeMap<String, Entry> {
        &self.entries
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    /// Keys counted against [`MAX_KEYS`] — agent keys only.
    ///
    /// `/logmon/*` is exempt because otherwise logmon could lock an agent out
    /// of its own registry. Scoped keys never appear here at all.
    fn counted_len(&self) -> usize {
        self.entries.keys().filter(|k| !is_reserved(k)).count()
    }

    /// Apply a batch.
    ///
    /// **Entries are applied in the order given.** When a batch would cross
    /// [`MAX_KEYS`], the overflow is rejected with `RegistryFull` rather than
    /// the batch being sorted or refused wholesale — otherwise two callers
    /// sending the same set in different orders end up with different
    /// registries, which is a difference nothing in the archive could explain.
    ///
    /// One malformed entry never rejects the batch. Valid rows apply; rejected
    /// rows return their reason.
    ///
    /// **A sigilled key is always rejected here.** The registry is the wrong
    /// home for a fact scoped to one document — see [`partition_scoped`].
    pub fn update(&mut self, entries: &[DataEntry], now: DateTime<Utc>) -> Vec<DataOutcome> {
        entries
            .iter()
            .map(|e| DataOutcome {
                path: e.path.clone(),
                outcome: self.apply_one(e, now),
            })
            .collect()
    }

    fn apply_one(&mut self, e: &DataEntry, now: DateTime<Utc>) -> Outcome {
        let reject = |reason| Outcome::Rejected { reason };

        let key = match Key::parse(&e.path, false) {
            Ok(k) => k,
            Err(KeyError::Malformed) => return reject(RejectReason::MalformedPath),
            Err(KeyError::TooLong) => return reject(RejectReason::PathTooLong),
            Err(KeyError::SigilNotAllowed) => return reject(RejectReason::SigilNotAllowedHere),
        };
        // Policy, not syntax, so it lives here rather than in `parse`: the same
        // parser serves `set_reserved`, which is the one caller allowed in.
        if key.is_reserved() {
            return reject(RejectReason::ReservedPrefix);
        }
        if let Some(v) = &e.value {
            if v.len() > MAX_VALUE_BYTES {
                return reject(RejectReason::ValueTooLong);
            }
        }
        let ttl_secs = match e.ttl {
            None => None,
            Some(TtlSpec::Clear) => Some(None),
            Some(TtlSpec::Set(s)) => Some(Some(s)),
        };

        let stored = key.rendered();
        match (&e.value, self.entries.get_mut(&stored)) {
            // Validate only.
            (None, Some(existing)) => {
                existing.validated_at = now;
                if let Some(t) = ttl_secs {
                    existing.ttl_secs = t;
                }
                Outcome::Validated
            }
            // Key-only on a missing key must NOT create: inventing a valueless
            // key is the helpful guess that becomes wrong data.
            (None, None) => Outcome::Unknown {
                cause: self.unknown_cause(),
            },
            (Some(v), Some(existing)) => {
                let changed = existing.value != *v;
                if changed {
                    existing.value = v.clone();
                    existing.created_at = now;
                }
                existing.validated_at = now;
                if let Some(t) = ttl_secs {
                    existing.ttl_secs = t;
                }
                if changed {
                    Outcome::Updated
                } else {
                    Outcome::Validated
                }
            }
            (Some(v), None) => {
                if self.counted_len() >= MAX_KEYS {
                    return reject(RejectReason::RegistryFull);
                }
                self.entries
                    .insert(stored, Entry::new(v.clone(), now, ttl_secs.flatten()));
                Outcome::Created
            }
        }
    }

    fn unknown_cause(&self) -> UnknownCause {
        if self.entries.is_empty() {
            if self.saw_quarantine {
                UnknownCause::NoRegistry
            } else {
                UnknownCause::Undetermined
            }
        } else {
            UnknownCause::NeverSet
        }
    }

    /// Remove by prefix pattern, reporting a count per pattern.
    ///
    /// A pattern whose first segment is reserved is refused — which covers the
    /// bare `/logmon` as well as `/logmon/version`, and that is the whole
    /// reason [`is_reserved`] is stated over segments. `remove` has no undo,
    /// and `/logmon/first_seen` and `/logmon/incarnation` are H3's and H6's
    /// entire defence.
    pub fn remove(&mut self, patterns: &[String]) -> Vec<RemoveOutcome> {
        patterns
            .iter()
            .map(|pattern| {
                let parsed = Key::parse(pattern, false);
                let rejected = match &parsed {
                    Err(KeyError::Malformed) => Some(RejectReason::MalformedPath),
                    Err(KeyError::TooLong) => Some(RejectReason::PathTooLong),
                    Err(KeyError::SigilNotAllowed) => Some(RejectReason::SigilNotAllowedHere),
                    Ok(k) if k.is_reserved() => Some(RejectReason::ReservedPrefix),
                    Ok(_) => None,
                };
                if let Some(reason) = rejected {
                    return RemoveOutcome {
                        pattern: pattern.clone(),
                        removed: 0,
                        rejected: Some(reason),
                    };
                }
                let k = parsed.expect("checked above");
                let target = k.rendered();
                let doomed: Vec<String> = self
                    .entries
                    .keys()
                    .filter(|stored| matches_prefix(&target, stored))
                    .cloned()
                    .collect();
                for d in &doomed {
                    self.entries.remove(d);
                }
                RemoveOutcome {
                    pattern: pattern.clone(),
                    removed: doomed.len(),
                    rejected: None,
                }
            })
            .collect()
    }

    /// Logmon's own write path. The only route to `/logmon/*`, and it refuses
    /// everything else so a bug here cannot quietly become an agent write.
    pub fn set_reserved(
        &mut self,
        path: &str,
        value: String,
        now: DateTime<Utc>,
    ) -> Result<Outcome, RegistryError> {
        if !is_reserved(path) {
            return Err(RegistryError::Reserved);
        }
        match self.entries.get_mut(path) {
            Some(existing) => {
                let changed = existing.value != value;
                if changed {
                    existing.value = value;
                    existing.created_at = now;
                }
                existing.validated_at = now;
                Ok(if changed {
                    Outcome::Updated
                } else {
                    Outcome::Validated
                })
            }
            None => {
                self.entries
                    .insert(path.to_string(), Entry::new(value, now, None));
                Ok(Outcome::Created)
            }
        }
    }

    /// Read, optionally narrowed by prefix and by staleness.
    ///
    /// `validated_before` is a filter over `get` rather than a tool of its own:
    /// "what is stale" is a question about the registry, and answering it with
    /// a second surface would mean two things to keep in step.
    pub fn query(
        &self,
        prefix: Option<&str>,
        validated_before: Option<DateTime<Utc>>,
    ) -> Vec<(&String, &Entry)> {
        self.entries
            .iter()
            .filter(|(k, _)| prefix.is_none_or(|p| matches_prefix(p, k)))
            .filter(|(_, e)| validated_before.is_none_or(|t| e.validated_at < t))
            .collect()
    }
}

/// One document-scoped assertion (§3.9) — a `@` key and its value.
///
/// Deliberately **not** an [`Entry`]: it has no `created_at`/`validated_at` pair
/// because it has no history to have. It is asserted once, at capture, and the
/// document records the capture instant and the anchor's instant beside it so a
/// reader can see the gap. A registry key accumulates confirmations; this does
/// not, and giving it the same shape would invite rendering it as though it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedFact {
    /// Rendered with its sigil, so `grep '/Data/seed'` and `grep '@/Data/seed'`
    /// stay two different questions over an archive.
    pub key: String,
    pub value: String,
}

/// Everything a single capture asserted about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopedData {
    pub facts: Vec<ScopedFact>,
}

/// Split a `data` list into the part that writes the registry and the part that
/// belongs to one document.
///
/// This is the whole mechanism behind §3.9, and it runs **before** the registry
/// sees anything. The registry is long-lived and shared; a capture's assertions
/// are not. Letting them into it would mean the next case on the same domain
/// silently inherits the last one's seed — laundering one incident's fact into
/// another's document, which is the failure the sigil was added to prevent.
///
/// Returns `(registry_entries, scoped, rejections)`. Rejections are outcomes for
/// scoped keys that could not be accepted, in the caller's original order, so a
/// caller can report per-entry results for the whole list without reassembling
/// two halves.
pub fn partition_scoped(entries: &[DataEntry]) -> (Vec<DataEntry>, ScopedData, Vec<DataOutcome>) {
    let mut registry = Vec::new();
    let mut scoped = ScopedData::default();
    let mut rejections = Vec::new();

    for e in entries {
        if !e.path.starts_with(super::path::SIGIL) {
            registry.push(e.clone());
            continue;
        }
        let reject = |reason| DataOutcome {
            path: e.path.clone(),
            outcome: Outcome::Rejected { reason },
        };
        let key = match Key::parse(&e.path, true) {
            Ok(k) => k,
            Err(KeyError::Malformed) => {
                rejections.push(reject(RejectReason::MalformedPath));
                continue;
            }
            Err(KeyError::TooLong) => {
                rejections.push(reject(RejectReason::PathTooLong));
                continue;
            }
            Err(KeyError::SigilNotAllowed) => {
                rejections.push(reject(RejectReason::SigilNotAllowedHere));
                continue;
            }
        };
        // A sigil scopes a key; it does not launder the reservation.
        if key.is_reserved() {
            rejections.push(reject(RejectReason::ReservedPrefix));
            continue;
        }
        let Some(value) = &e.value else {
            // "Validate" is meaningless against a document that does not exist
            // yet. A scoped key with no value has nothing to assert.
            rejections.push(reject(RejectReason::MalformedPath));
            continue;
        };
        if value.len() > MAX_VALUE_BYTES {
            rejections.push(reject(RejectReason::ValueTooLong));
            continue;
        }
        if scoped.facts.len() >= MAX_SIGIL_KEYS {
            rejections.push(reject(RejectReason::RegistryFull));
            continue;
        }
        let rendered = key.rendered();
        // Last writer wins within one call, matching the registry's own
        // semantics for a repeated key in a single batch.
        match scoped.facts.iter_mut().find(|f| f.key == rendered) {
            Some(existing) => existing.value = value.clone(),
            None => scoped.facts.push(ScopedFact {
                key: rendered,
                value: value.clone(),
            }),
        }
    }
    (registry, scoped, rejections)
}

/// Parse a `ttl` field off the wire: a duration string, or `false` to clear.
pub fn parse_ttl(raw: &serde_json::Value) -> Result<Option<TtlSpec>, TtlParseError> {
    match raw {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(false) => Ok(Some(TtlSpec::Clear)),
        // `true` has no meaning here and guessing one is how a provenance field
        // starts lying.
        serde_json::Value::Bool(true) => Err(TtlParseError::Malformed),
        serde_json::Value::String(s) => parse_duration(s).map(|secs| Some(TtlSpec::Set(secs))),
        _ => Err(TtlParseError::Malformed),
    }
}

#[cfg(test)]
mod tests;
