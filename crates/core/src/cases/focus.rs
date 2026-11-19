//! Chronological, trace-attributed views over the standard raw evidence files.
use crate::{gelf::message::LogEntry, render::escape::cell, span::types::SpanEntry};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[derive(Default)]
pub(crate) struct Intervals {
    traces: BTreeMap<u128, (DateTime<Utc>, DateTime<Utc>)>,
}
impl Intervals {
    pub fn observe(&mut self, trace: u128, start: DateTime<Utc>, end: DateTime<Utc>) {
        self.traces
            .entry(trace)
            .and_modify(|(a, b)| {
                *a = (*a).min(start);
                *b = (*b).max(end);
            })
            .or_insert((start, end));
    }
    pub fn overlapping(&self, focus: u128) -> Option<(DateTime<Utc>, Vec<u128>)> {
        let &(start, end) = self.traces.get(&focus)?;
        let mut earliest = start;
        let mut peers = Vec::new();
        for (&trace, &(a, b)) in &self.traces {
            if trace != focus && a <= end && b >= start {
                earliest = earliest.min(a);
                peers.push(trace);
            }
        }
        Some((earliest, peers))
    }
}

pub(crate) fn render<'a>(
    out: &mut String,
    case: &super::document::CaseInput,
    peers: &[u128],
    zone: Tz,
    logs: impl Iterator<Item = &'a LogEntry>,
    spans: impl Iterator<Item = &'a SpanEntry>,
) {
    let focus = &case.focus_traces;
    let stem = &case.stem;
    let selected: BTreeSet<_> = focus.iter().chain(peers).copied().collect();
    let mut rows = BTreeMap::<_, Vec<_>>::new();
    for log in logs {
        if let Some(id) = log.trace_id.filter(|id| selected.contains(id)) {
            rows.entry(id)
                .or_default()
                .push((log.timestamp, log.seq, "log", log.message.as_str()));
        }
    }
    for span in spans.filter(|s| selected.contains(&s.trace_id)) {
        rows.entry(span.trace_id).or_default().push((
            span.start_time,
            span.seq,
            "span",
            span.name.as_str(),
        ));
    }
    let mut timeline = |out: &mut String, id: u128| {
        let records = rows.entry(id).or_default();
        records.sort_unstable_by_key(|r| (r.0, r.1));
        writeln!(out,"\n### Trace `{id:032x}` ({} records)\n\n| Time ({zone}) | Sequence | Kind | Message or span |\n|---|---:|---|---|", records.len()).unwrap();
        // The Markdown is a report; raw sidecars preserve every byte and record.
        for &(at, seq, kind, text) in records.iter().take(200) {
            writeln!(
                out,
                "| {} | {seq} | {kind} | {} |",
                at.with_timezone(&zone).to_rfc3339(),
                cell(&text.chars().take(240).collect::<String>())
            )
            .unwrap();
        }
        if records.len() > 200 {
            writeln!(out,"\nShowing the first 200 records. The sidecars contain the complete retained timeline.").unwrap();
        }
    };
    out.push_str("\n## Focus trace evidence\n");
    for id in focus {
        timeline(out, *id);
    }
    out.push_str("\n## Directly overlapping traces\n");
    for id in peers {
        writeln!(out, "- `{id:032x}`").unwrap();
    }
    out.push_str("\n## Parallel trace evidence\n");
    for id in peers {
        timeline(out, *id);
    }
    out.push_str("\n## Complete retained domain window\n\n");
    if case.logdata.is_some() {
        writeln!(out, "[All retained logs]({stem}.logdata.jsonl)").unwrap();
    }
    if case.spandata.is_some() {
        writeln!(out, "[All retained spans]({stem}.spandata.jsonl)").unwrap();
    }
    out.push_str("Load with scope `all` to include retained untagged activity.\n");
}
