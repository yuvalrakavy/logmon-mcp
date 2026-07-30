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
    // The broker's version leads, and the CLI's own follows when they differ:
    // this is the surface a human is looking at while deciding whether to
    // reinstall, and the two numbers are the whole decision. An older broker
    // omits the field, which deserializes to "" — say "unknown" rather than
    // printing an empty value that reads like a bug.
    let shim_version = env!("CARGO_PKG_VERSION");
    if result.broker_version.is_empty() {
        println!("broker: unknown (predates version reporting), cli {shim_version}");
    } else if result.broker_version == shim_version {
        println!("broker: {}", result.broker_version);
    } else {
        println!(
            "broker: {} — this cli is {shim_version}; reinstall with \
             `cargo install --path crates/mcp` to match",
            result.broker_version
        );
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
