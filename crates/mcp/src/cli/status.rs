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
    0
}
