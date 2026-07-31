//! Entries, timestamps and `ttl` — spec §3.2, §3.3.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// One recorded belief.
///
/// Two timestamps rather than one "last modified", because that conflates two
/// questions: set six days ago and never revisited is a guess, the same value
/// confirmed five minutes ago is evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub value: String,
    /// When THIS VALUE came into force. Moves only when the value changes.
    pub created_at: DateTime<Utc>,
    /// When someone last confirmed it is still true. Invariant:
    /// `validated_at >= created_at`.
    pub validated_at: DateTime<Utc>,
    /// How long after confirmation this stays believable. Absent means the
    /// document reports an age and **no verdict** — never "current".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

impl Entry {
    pub fn new(value: String, now: DateTime<Utc>, ttl_secs: Option<u64>) -> Self {
        Self {
            value,
            created_at: now,
            validated_at: now,
            ttl_secs,
        }
    }

    /// Age since last confirmation, which is what a document renders.
    pub fn age(&self, now: DateTime<Utc>) -> Duration {
        now - self.validated_at
    }

    /// Whether the stated lifetime has elapsed **since `validated_at`**.
    ///
    /// Measured from validation, not creation, and the choice is load-bearing.
    /// From `created_at` a key restated daily for a month would read expired
    /// forever, while §3.2's opening argument is that a confirmed value is
    /// evidence. The cost is that restating a value you did not actually
    /// re-derive buys it a lifetime it did not earn — which is why §8 tells an
    /// agent to send values only for what it re-read and key-only for the rest.
    ///
    /// `None` means unknown, never fresh: no `ttl`, no verdict.
    pub fn is_expired(&self, now: DateTime<Utc>) -> Option<bool> {
        let ttl = self.ttl_secs?;
        Some((now - self.validated_at).num_seconds() >= ttl as i64)
    }
}

/// What a caller asked to happen to one key's lifetime.
///
/// Three states, because "leave it alone", "set it" and "take it away" are
/// three different requests. The repo already met this problem on
/// `CollectorsEdit.threshold`, which solved it with `Option<Option<T>>` and
/// `null` meaning *remove*. Here `null` is already taken — §3.3 defines absent
/// ≡ `null` ≡ *unchanged*, for both `value` and `ttl`, so that a validate-only
/// entry cannot silently drop a lifetime — so the clear is the boolean `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlSpec {
    Set(u64),
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtlParseError {
    /// Not `<integer><unit>`, or a unit outside `s m h d w`.
    Malformed,
    /// `0s` and friends. A zero lifetime reads as "stale immediately" at least
    /// as naturally as "no lifetime", so it is refused rather than guessed at.
    Zero,
    Overflow,
}

/// Parse a duration literal — one integer and one unit, no compounds.
///
/// `30s`, `5m`, `2h`, `7d`, `4w`. Deliberately narrow: the draft shipped `ttl`
/// without ever saying what one looked like, which means an agent following the
/// spec sets none, which means every key reports "age, no verdict" and the
/// feature goes unused in exactly the way §3.6.2 exists to prevent.
pub fn parse_duration(s: &str) -> Result<u64, TtlParseError> {
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let mult: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(TtlParseError::Malformed),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TtlParseError::Malformed);
    }
    let n: u64 = digits.parse().map_err(|_| TtlParseError::Overflow)?;
    if n == 0 {
        return Err(TtlParseError::Zero);
    }
    n.checked_mul(mult).ok_or(TtlParseError::Overflow)
}

/// Render seconds back as the shortest exact literal, so a value round-trips.
pub fn render_duration(secs: u64) -> String {
    for (unit, mult) in [("w", 604_800u64), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs.is_multiple_of(mult) {
            return format!("{}{unit}", secs / mult);
        }
    }
    format!("{secs}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse_duration("30s"), Ok(30));
        assert_eq!(parse_duration("5m"), Ok(300));
        assert_eq!(parse_duration("2h"), Ok(7200));
        assert_eq!(parse_duration("7d"), Ok(604_800));
        assert_eq!(parse_duration("4w"), Ok(2_419_200));
    }

    #[test]
    fn rejects_the_shapes_that_would_have_to_be_guessed_at() {
        for s in ["", "m", "30", "30x", "1h30m", "-5m", "5 m", "1.5h", "s5"] {
            assert_eq!(
                parse_duration(s),
                Err(TtlParseError::Malformed),
                "{s} should be malformed"
            );
        }
        assert_eq!(parse_duration("0s"), Err(TtlParseError::Zero));
        assert_eq!(parse_duration("0d"), Err(TtlParseError::Zero));
    }

    #[test]
    fn an_overflowing_literal_does_not_wrap() {
        assert_eq!(
            parse_duration(&format!("{}w", u64::MAX)),
            Err(TtlParseError::Overflow)
        );
        assert!(parse_duration("99999999999999999999s").is_err());
    }

    #[test]
    fn durations_round_trip_through_rendering() {
        for s in ["30s", "5m", "2h", "7d", "4w", "90s"] {
            let secs = parse_duration(s).unwrap();
            assert_eq!(parse_duration(&render_duration(secs)), Ok(secs));
        }
    }

    #[test]
    fn expiry_is_measured_from_validation_not_creation() {
        // The whole point: a value confirmed five minutes ago is evidence,
        // however long ago it was first set.
        let e = Entry {
            value: "1.0.2".into(),
            created_at: t("2026-07-01T00:00:00Z"),
            validated_at: t("2026-07-31T13:58:00Z"),
            ttl_secs: Some(1800),
        };
        assert_eq!(e.is_expired(t("2026-07-31T14:15:00Z")), Some(false));
        // Anchored on created_at instead, this month-old key would read expired
        // no matter how recently anyone confirmed it.
        assert_eq!(e.is_expired(t("2026-07-31T14:29:00Z")), Some(true));
    }

    #[test]
    fn expiry_is_inclusive_at_the_boundary() {
        let e = Entry::new("x".into(), t("2026-07-31T12:00:00Z"), Some(60));
        assert_eq!(e.is_expired(t("2026-07-31T12:00:59Z")), Some(false));
        assert_eq!(e.is_expired(t("2026-07-31T12:01:00Z")), Some(true));
    }

    #[test]
    fn no_ttl_yields_no_verdict_never_fresh() {
        let e = Entry::new("x".into(), t("2020-01-01T00:00:00Z"), None);
        assert_eq!(e.is_expired(t("2026-07-31T12:00:00Z")), None);
    }

    #[test]
    fn age_runs_from_validation() {
        let e = Entry {
            value: "x".into(),
            created_at: t("2026-07-01T00:00:00Z"),
            validated_at: t("2026-07-31T12:00:00Z"),
            ttl_secs: None,
        };
        assert_eq!(e.age(t("2026-07-31T12:30:00Z")).num_minutes(), 30);
    }
}
