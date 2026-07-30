//! `status` subcommand — print broker status (uptime, receivers, store stats, session).

use logmon_broker_protocol::StatusGet;
use logmon_broker_sdk::Broker;

use super::format;

pub async fn dispatch(broker: &Broker, json: bool) -> i32 {
    let result = match broker.status_get(StatusGet {}).await {
        Ok(r) => r,
        Err(e) => {
            format::error(&format!("status.get failed: {e}"), json);
            return 1;
        }
    };

    if json {
        format::print_json(&result);
        return 0;
    }

    // Human format: simple key=value block.
    //
    // The version is reported, but the *decision* is driven by the tool sets —
    // via the same shared composer the MCP tool result uses, so the two surfaces
    // cannot give a reader opposite answers. Comparing version strings would:
    // this repo has two commits stamped `0.5.1` with different tool sets, so an
    // equal-version check prints an all-clear exactly when tools are missing,
    // and a differing-version check demands a reinstall after a PATCH release
    // that changed nothing a caller can see.
    let shim_version = env!("CARGO_PKG_VERSION");
    if result.broker_version.is_empty() {
        println!("broker: unknown (predates version reporting), cli {shim_version}");
    } else {
        println!("broker: {}, cli {shim_version}", result.broker_version);
    }
    if let Some(note) = logmon_broker_protocol::mcp_tools::skew_note(
        &result.broker_version,
        &result.broker_tools,
        &logmon_broker_protocol::mcp_tools::TOOLS
            .iter()
            .map(|(t, _)| *t)
            .collect::<Vec<_>>(),
    ) {
        println!("{note}");
    }
    println!("uptime: {}s", result.daemon_uptime_secs);
    print!("receivers:");
    if result.receivers.is_empty() {
        println!(" (none)");
    } else {
        println!();
        for r in &result.receivers {
            println!("  - {r}");
        }
    }
    println!(
        "store: total_received={} total_stored={} malformed={} current_size={}",
        result.store.total_received,
        result.store.total_stored,
        result.store.malformed_count,
        result.store.current_size,
    );
    // Sibling of receiver_drops (never itself printed here; JSON-only today —
    // see TraceIngestCounts's doc for why the two don't merge). Silent like
    // the rest of this human view when there's nothing to report.
    let ti = &result.trace_ingest;
    if ti.dropped != 0 || ti.shed_batches != 0 || ti.malformed_dropped != 0 {
        println!(
            "trace ingest: {} dropped, {} batches shed, {} malformed",
            ti.dropped, ti.shed_batches, ti.malformed_dropped,
        );
    }
    if let Some(s) = &result.session {
        println!(
            "session: id={} name={} connected={} triggers={} filters={} queue={} last_seen={}s",
            s.id,
            s.name.as_deref().unwrap_or("(anonymous)"),
            s.connected,
            s.trigger_count,
            s.filter_count,
            s.queue_size,
            s.last_seen_secs_ago,
        );
    } else {
        println!("session: (none)");
    }
    println!("domain: {}", result.current_domain);
    print!("filters:");
    if result.active_filters.is_empty() {
        println!(" (none)");
    } else {
        println!(" {}", result.active_filters.join(", "));
    }
    0
}
