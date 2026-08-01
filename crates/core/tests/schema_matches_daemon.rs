//! The committed schema's enums must equal what the daemon actually accepts.
//!
//! Phase A declares each closed-set parameter's accepted values in the protocol
//! schema, so a client can learn them without reading Rust. Those declarations
//! were transcribed by hand from the parsers, and a hand-transcribed claim is
//! exactly the thing that drifts — six of the ten doc comments this replaced
//! were already wrong about their own parameter, which is what motivated the
//! phase in the first place.
//!
//! So the match is checked in both directions:
//!
//! * **Forward** — every value the schema declares must parse. A schema wider
//!   than the daemon promises calls that fail.
//! * **Reverse** — every canonical spelling the daemon has must be declared. A
//!   schema narrower than the daemon makes a validating client reject calls the
//!   daemon would have served, which is the failure mode that killed the plan
//!   to turn these into Rust enums.
//!
//! Aliases (`>` for `gt`, `markdown` for `md`) are covered forward only: a
//! function from `&str` cannot be asked what it accepts. Adding a *variant*
//! without declaring it is caught by the exhaustive matches below; adding an
//! *alias* without declaring it is not, and that is the residue.

use logmon_broker_core::collector::diff::DiffGroupBy;
use logmon_broker_core::collector::document::Format;
use logmon_broker_core::collector::project::GroupBy;
use logmon_broker_core::collector::sample::Level;
use logmon_broker_core::collector::threshold::{Metric, Op};
use logmon_broker_core::span::types::SlowGroupBy;

use logmon_broker_protocol::mcp_tools::SCHEMA_JSON as SCHEMA;

/// The `enum` a definition declares for one field. `null` entries are dropped:
/// they mean "the optional field may be null", which every `opt_*` reader maps
/// to absent, and no value parser ever sees.
fn declared(type_name: &str, field: &str) -> Vec<String> {
    let schema: serde_json::Value = serde_json::from_str(SCHEMA).expect("schema parses");
    let values = schema["definitions"][type_name]["properties"][field]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{type_name}.{field} declares no enum"));
    values
        .iter()
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("{type_name}.{field} declares a non-string: {v}"))
                .to_string()
        })
        .collect()
}

/// Assert both directions at once for one field.
///
/// `canonical` is every spelling the daemon's own type can produce, so a new
/// variant that nobody declared shows up here rather than in a client.
fn agrees(
    type_name: &str,
    field: &str,
    canonical: &[&str],
    parses: impl Fn(&str) -> bool,
    a_value_it_must_reject: &str,
) {
    let declared = declared(type_name, field);

    for value in &declared {
        assert!(
            parses(value),
            "the schema declares `{value}` for {type_name}.{field}, but the daemon \
             rejects it — a client trusting the schema would send a call that fails"
        );
    }
    for value in canonical {
        assert!(
            declared.iter().any(|d| d == value),
            "the daemon accepts `{value}` for {type_name}.{field} but the schema does \
             not declare it — a validating client would refuse a call the daemon serves"
        );
    }
    assert!(
        !parses(a_value_it_must_reject),
        "`{a_value_it_must_reject}` was expected to be rejected, so this test's \
         forward direction proves nothing about {type_name}.{field}"
    );
}

/// Every `GroupBy` spelling, read from the type's own list rather than
/// transcribed here.
///
/// This file used to keep its own copy behind an exhaustive match, which caught
/// a *new variant* but not a stale *message* — and a stale message is what
/// actually shipped. `GroupBy::ALL` is now the single enumeration, so this test
/// and the daemon's rejection text cannot disagree. The tripwire is unchanged
/// in strength and better placed: adding a variant is still a compile error, at
/// the exhaustive `as_str` one line below `ALL`.
fn group_by_canonical() -> Vec<&'static str> {
    GroupBy::ALL.map(GroupBy::as_str).to_vec()
}

fn diff_group_by_canonical() -> Vec<&'static str> {
    DiffGroupBy::ALL.map(DiffGroupBy::as_str).to_vec()
}

#[test]
fn threshold_op_agrees_with_the_daemon() {
    agrees(
        "ThresholdSpec",
        "op",
        &["gt", "gte", "lt", "lte"],
        |s| Op::parse(s).is_ok(),
        "=<",
    );
}

#[test]
fn threshold_metric_agrees_with_the_daemon() {
    agrees(
        "ThresholdSpec",
        "metric",
        &[
            "count",
            "total_ms",
            "avg_ms",
            "error_count",
            "error_rate_pct",
        ],
        |s| Metric::parse(s).is_ok(),
        // Deliberately a percentile: the parser answers these with a bespoke
        // explanation rather than "unknown", and both are rejections.
        "p95_ms",
    );
}

#[test]
fn collector_level_agrees_with_the_daemon() {
    let parses = |s: &str| matches!(s, "scalar" | "timing" | "tree");
    let canonical = [Level::Scalar, Level::Timing, Level::Tree].map(|l| l.as_str());
    for type_name in ["CollectorsAdd", "CollectorsEdit"] {
        agrees(type_name, "level", &canonical, parses, "trees");
    }
}

#[test]
fn profile_group_by_agrees_with_the_daemon() {
    // Both read through `profile_options`, so one parser governs two methods.
    for type_name in ["CollectorsGet", "TracesProfile"] {
        agrees(
            type_name,
            "group_by",
            &group_by_canonical(),
            |s| GroupBy::parse(s).is_some(),
            "NAME",
        );
    }
}

#[test]
fn diff_group_by_agrees_with_the_daemon() {
    for type_name in ["CollectorsDiff", "CollectorsDocument"] {
        agrees(
            type_name,
            "group_by",
            &diff_group_by_canonical(),
            |s| DiffGroupBy::parse(s).is_some(),
            // Rejected for a diff specifically: a recorded run keeps no
            // per-span records, so there is nothing to project a trace from.
            "trace",
        );
    }
}

#[test]
fn document_format_agrees_with_the_daemon() {
    agrees(
        "CollectorsDocument",
        "format",
        &[
            Format::Md.as_str(),
            Format::Json.as_str(),
            Format::Folded.as_str(),
        ],
        |s| Format::parse(s).is_some(),
        "html",
    );
}

/// `traces.slow` was the one `group_by` that was NOT a closed set, and this
/// test used to pin that open. It is closed now (#7): the argument for leaving
/// it open was that an enum "would reject every valid call that means do not
/// group", which held only while `traces.slow` had no spelling for that. It has
/// one, so the exemption is gone and this field is checked like the rest.
#[test]
fn traces_slow_group_by_agrees_with_the_daemon() {
    agrees(
        "TracesSlow",
        "group_by",
        &SlowGroupBy::ALL.map(SlowGroupBy::as_str),
        |s| SlowGroupBy::parse(s).is_some(),
        // The exact shape the old leniency swallowed: a transposed `name`,
        // which used to come back as a full ungrouped result.
        "nmae",
    );
}
