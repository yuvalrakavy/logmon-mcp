//! Filter admission for span collectors — spec §7.
//!
//! Replaces an earlier plan to reuse `is_span_filter` as a rejection gate.
//! That predicate returns **false** for bare patterns, attribute-only filters
//! and pure bookmark windows — all legal span filters, and two of them are the
//! design's own examples. It was written as a "should I even try this trigger
//! against spans" hint, where a false negative is a silent no-op; as a gate it
//! turns that gap into a loud refusal of valid input.
//!
//! The rule here is narrower and two-sided: **block only what provably cannot
//! match, and mark everything else that is likely to surprise.** A blocking
//! check that rejects valid input gets disabled or routed around, and then it
//! protects nothing.

use crate::filter::parser::{DurationOp, ParsedFilter, Qualifier, Selector};

/// Why a filter cannot be accepted. Every variant is *provable* from the
/// parsed filter alone — nothing here is a heuristic.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionError {
    /// `NONE` matches nothing, by definition.
    MatchesNothing,
    /// A log-only qualifier (`l>=`, `m=`, `h=`, …) cannot match a span.
    LogOnlyQualifier(String),
    /// `d>=NaN`, `d<=NaN`, `d>=+inf`, `d<=-inf`. Every comparison against NaN
    /// is false, and no finite duration reaches +inf.
    ///
    /// Deliberately **not** `d<=+inf` or `d>=-inf`: those match *everything*,
    /// which is the opposite of the stated bar. They are no-ops, and are
    /// marked rather than blocked.
    ImpossibleDurationThreshold(f64),
    /// `d>=100, d<=50`. Qualifiers are AND-ed, so the interval is empty — and
    /// it is a plausible transposition of "between 50 and 100 ms".
    EmptyDurationInterval { lower: f64, upper: f64 },
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionError::MatchesNothing => {
                write!(f, "filter matches nothing (NONE) and would never collect")
            }
            AdmissionError::LogOnlyQualifier(s) => write!(
                f,
                "filter contains the log-only qualifier `{s}`, which cannot match a span"
            ),
            AdmissionError::ImpossibleDurationThreshold(v) => write!(
                f,
                "duration threshold `{v}` can never be satisfied by a finite span duration"
            ),
            AdmissionError::EmptyDurationInterval { lower, upper } => write!(
                f,
                "duration interval is empty: d>={lower} and d<={upper} cannot both hold"
            ),
        }
    }
}

impl std::error::Error for AdmissionError {}

/// Something admitted, but likely not what the author meant. Reported so the
/// surprise is immediate rather than discovered after a 40-minute run.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionWarning {
    /// A selector the parser did not recognise, so it became an attribute
    /// lookup. `parse_selector`'s catch-all turns every typo into one of
    /// these: `SV=store_server` — the flagship filter with the shift key
    /// stuck — parses cleanly and matches nothing.
    ///
    /// Reported **per qualifier**, not per filter: `sn=x, SV=y` is a perfectly
    /// natural filter whose AND still yields nothing, and a whole-filter check
    /// would stay silent on it.
    UninterpretedSelector(String),
    /// No `sn`/`sv`/`st`/`sk`/`d` qualifier anywhere, so nothing in the filter
    /// is span-specific.
    NoSpanSpecificQualifier,
    /// `d<=+inf` / `d>=-inf`: admitted, but a no-op.
    NoOpDurationThreshold(f64),
    /// A bare pattern matches the span **name only** — narrower than the log
    /// path, where it scans every field.
    BarePatternMatchesNameOnly,
    /// `st=` substring-matches the error *message*, so `st=ok` also admits an
    /// errored span whose message happens to contain "ok" ("broken pipe"
    /// does). A collector armed to measure only successful spans will quietly
    /// include failures.
    StatusMatchesErrorMessages,
    /// Attributes are read through `.as_str()` on the span path, so a
    /// non-string value never matches, whatever the pattern.
    AttributeMatchIsStringOnly(String),
}

impl std::fmt::Display for AdmissionWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionWarning::UninterpretedSelector(s) => write!(
                f,
                "`{s}` is not a known selector, so it was read as a span attribute name; \
                 if that attribute does not exist this filter matches nothing"
            ),
            AdmissionWarning::NoSpanSpecificQualifier => write!(
                f,
                "no span-specific qualifier (sn/sv/st/sk/d) — this filter is not obviously about spans"
            ),
            AdmissionWarning::NoOpDurationThreshold(v) => {
                write!(f, "duration threshold `{v}` matches every span; it has no effect")
            }
            AdmissionWarning::BarePatternMatchesNameOnly => write!(
                f,
                "a bare pattern matches the span NAME only, unlike on the log path where it scans every field"
            ),
            AdmissionWarning::StatusMatchesErrorMessages => write!(
                f,
                "`st=` also substring-matches error messages, so `st=ok` can admit errored spans"
            ),
            AdmissionWarning::AttributeMatchIsStringOnly(k) => write!(
                f,
                "attribute `{k}` is matched via as_str(), so a boolean or numeric value never matches"
            ),
        }
    }
}

/// The outcome of admitting a filter for a span collector.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Admission {
    pub warnings: Vec<AdmissionWarning>,
}

fn selector_name(sel: &Selector) -> Option<&str> {
    match sel {
        Selector::Message => Some("m"),
        Selector::FullMessage => Some("fm"),
        Selector::MessageOrFull => Some("mfm"),
        Selector::Host => Some("h"),
        Selector::Facility => Some("fa"),
        Selector::File => Some("fi"),
        Selector::Line => Some("ln"),
        _ => None,
    }
}

/// Admit a filter for use as a span collector's predicate.
pub fn admit_span_filter(filter: &ParsedFilter) -> Result<Admission, AdmissionError> {
    let qs = match filter {
        ParsedFilter::None => return Err(AdmissionError::MatchesNothing),
        // `ALL` is span-specific enough: it matches every span. Marked below
        // as having no span-specific qualifier, but admitted.
        ParsedFilter::All => return Ok(Admission::default()),
        ParsedFilter::Qualifiers(qs) => qs,
    };

    let mut warnings = Vec::new();
    let mut has_span_specific = false;
    let mut lower: Option<f64> = None;
    let mut upper: Option<f64> = None;

    for q in qs {
        match q {
            Qualifier::LevelFilter { .. } => {
                return Err(AdmissionError::LogOnlyQualifier("l".to_string()));
            }
            Qualifier::SelectorPattern(sel, _) => {
                if let Some(name) = selector_name(sel) {
                    return Err(AdmissionError::LogOnlyQualifier(name.to_string()));
                }
                match sel {
                    Selector::SpanName | Selector::ServiceName | Selector::SpanKind => {
                        has_span_specific = true;
                    }
                    Selector::SpanStatus => {
                        has_span_specific = true;
                        warnings.push(AdmissionWarning::StatusMatchesErrorMessages);
                    }
                    Selector::AdditionalField(key) => {
                        // Per-qualifier, always: a filter can pair a valid span
                        // selector with a typo and still collect nothing.
                        warnings.push(AdmissionWarning::UninterpretedSelector(key.clone()));
                        warnings.push(AdmissionWarning::AttributeMatchIsStringOnly(key.clone()));
                    }
                    _ => {}
                }
            }
            Qualifier::DurationFilter(op, v) => {
                has_span_specific = true;
                if v.is_nan() {
                    return Err(AdmissionError::ImpossibleDurationThreshold(*v));
                }
                match (op, v.is_infinite(), v.is_sign_positive()) {
                    // d>=+inf and d<=-inf can never hold.
                    (DurationOp::Gte, true, true) | (DurationOp::Lte, true, false) => {
                        return Err(AdmissionError::ImpossibleDurationThreshold(*v));
                    }
                    // d<=+inf and d>=-inf always hold: no-ops, not errors.
                    (DurationOp::Lte, true, true) | (DurationOp::Gte, true, false) => {
                        warnings.push(AdmissionWarning::NoOpDurationThreshold(*v));
                    }
                    _ => match op {
                        DurationOp::Gte => lower = Some(lower.map_or(*v, |l: f64| l.max(*v))),
                        DurationOp::Lte => upper = Some(upper.map_or(*v, |u: f64| u.min(*v))),
                    },
                }
            }
            Qualifier::BarePattern(_) => {
                warnings.push(AdmissionWarning::BarePatternMatchesNameOnly);
            }
            // Bookmarks are rejected separately by the RPC layer for collectors
            // (a pre-parsed filter would freeze a stale seq bound); they are
            // legal for `traces.profile`, so admission does not object.
            Qualifier::BookmarkFilter { .. }
            | Qualifier::CursorFilter { .. }
            | Qualifier::SeqFilter { .. } => {}
        }
    }

    // Strict: `d>=100, d<=100` legitimately matches exactly 100.0.
    if let (Some(l), Some(u)) = (lower, upper) {
        if l > u {
            return Err(AdmissionError::EmptyDurationInterval { lower: l, upper: u });
        }
    }

    if !has_span_specific {
        warnings.push(AdmissionWarning::NoSpanSpecificQualifier);
    }

    Ok(Admission { warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parser::parse_filter;

    fn admit(s: &str) -> Result<Admission, AdmissionError> {
        admit_span_filter(&parse_filter(s).expect("parses"))
    }

    fn warns(s: &str) -> Vec<AdmissionWarning> {
        admit(s).expect("admitted").warnings
    }

    // ---- blocked: provably never matches -------------------------------

    #[test]
    fn none_is_blocked() {
        assert_eq!(admit("NONE"), Err(AdmissionError::MatchesNothing));
    }

    #[test]
    fn log_only_qualifiers_are_blocked() {
        assert_eq!(
            admit("l>=ERROR"),
            Err(AdmissionError::LogOnlyQualifier("l".into()))
        );
        assert_eq!(
            admit("h=myhost"),
            Err(AdmissionError::LogOnlyQualifier("h".into()))
        );
        // A filter MIXING log and span selectors never reaches admission: the
        // parser rejects it outright. Asserted here so the division of labour
        // is pinned — if the parser ever relaxed, admission would have to
        // cover this and this test would say so.
        assert!(
            matches!(
                parse_filter("sv=svc, m=boom"),
                Err(crate::filter::parser::FilterParseError::MixedLogSpanSelectors)
            ),
            "mixed selectors are a parse error, not an admission error"
        );
    }

    #[test]
    fn impossible_duration_thresholds_are_blocked() {
        for f in ["d>=nan", "d<=nan", "d>=inf", "d<=-inf"] {
            assert!(
                matches!(
                    admit(f),
                    Err(AdmissionError::ImpossibleDurationThreshold(_))
                ),
                "{f} can never match and must be blocked"
            );
        }
    }

    #[test]
    fn an_empty_duration_interval_is_blocked() {
        assert_eq!(
            admit("d>=100, d<=50"),
            Err(AdmissionError::EmptyDurationInterval {
                lower: 100.0,
                upper: 50.0
            })
        );
    }

    #[test]
    fn a_degenerate_but_satisfiable_interval_is_admitted() {
        // The bound is STRICT: exactly 100.0 is a real, matchable duration.
        assert!(admit("d>=100, d<=100").is_ok());
    }

    // ---- marked, never blocked -----------------------------------------

    #[test]
    fn no_op_duration_thresholds_are_marked_not_blocked() {
        // They match EVERYTHING, which is the opposite of the blocking bar.
        for f in ["d<=inf", "d>=-inf"] {
            let w = warns(f);
            assert!(
                w.iter()
                    .any(|w| matches!(w, AdmissionWarning::NoOpDurationThreshold(_))),
                "{f} should be marked as a no-op"
            );
        }
    }

    #[test]
    fn a_mistyped_selector_is_marked_per_qualifier() {
        // The flagship filter with the shift key stuck.
        let w = warns("SV=store_server");
        assert!(w
            .iter()
            .any(|w| matches!(w, AdmissionWarning::UninterpretedSelector(k) if k == "SV")));

        // And — the case a whole-filter check misses — a valid span selector
        // paired with a typo. The AND still collects nothing.
        let w = warns("sn=cache.lookup, SV=store_server");
        assert!(
            w.iter()
                .any(|w| matches!(w, AdmissionWarning::UninterpretedSelector(k) if k == "SV")),
            "a per-filter check would stay silent here"
        );
    }

    #[test]
    fn attribute_filters_are_admitted_and_marked_string_only() {
        // The driving example's own selector: a boolean attribute never
        // matches, for any pattern.
        let w = warns("cache.enabled=true");
        assert!(w.iter().any(
            |w| matches!(w, AdmissionWarning::AttributeMatchIsStringOnly(k) if k == "cache.enabled")
        ));
    }

    #[test]
    fn legal_span_filters_the_old_predicate_rejected_are_admitted() {
        // Every one of these is rejected by `is_span_filter`, and every one is
        // a legal span filter.
        assert!(admit("schema.resolve").is_ok(), "bare pattern");
        assert!(admit("cache.enabled=true").is_ok(), "attribute only");
        assert!(admit("ALL").is_ok(), "ALL matches every span");
    }

    #[test]
    fn a_bare_pattern_is_marked_as_name_only() {
        let w = warns("schema.resolve");
        assert!(w
            .iter()
            .any(|w| matches!(w, AdmissionWarning::BarePatternMatchesNameOnly)));
    }

    #[test]
    fn status_filters_are_marked_because_they_match_error_messages() {
        let w = warns("st=ok");
        assert!(w
            .iter()
            .any(|w| matches!(w, AdmissionWarning::StatusMatchesErrorMessages)));
    }

    #[test]
    fn a_plain_span_filter_admits_without_surprises() {
        let w = warns("sv=store_server, d>=10");
        assert!(
            w.is_empty(),
            "the design's own recommended filter should be quiet: {w:?}"
        );
    }

    #[test]
    fn no_span_specific_qualifier_is_marked() {
        let w = warns("cache.enabled=true");
        assert!(w
            .iter()
            .any(|w| matches!(w, AdmissionWarning::NoSpanSpecificQualifier)));
        // …and is absent once anything span-specific appears.
        let w = warns("sv=svc");
        assert!(!w
            .iter()
            .any(|w| matches!(w, AdmissionWarning::NoSpanSpecificQualifier)));
    }
}
