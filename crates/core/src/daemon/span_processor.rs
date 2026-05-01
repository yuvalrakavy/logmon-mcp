use crate::daemon::log_processor::sync_pre_buffer_size;
use crate::daemon::session::SessionRegistry;
use crate::engine::pipeline::LogPipeline;
use crate::filter::matcher::matches_span;
use crate::filter::parser::{is_span_filter, parse_filter};
use crate::span::store::SpanStore;
use crate::span::types::{SpanEntry, SpanStatus, TraceSummary};
use std::sync::Arc;
use tokio::sync::mpsc;

pub fn spawn_span_processor(
    mut receiver: mpsc::Receiver<SpanEntry>,
    span_store: Arc<SpanStore>,
    sessions: Arc<SessionRegistry>,
    pipeline: Arc<LogPipeline>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(span) = receiver.recv().await {
            process_span(&span, &span_store, &sessions, &pipeline);
        }
    })
}

pub fn process_span(
    span: &SpanEntry,
    store: &SpanStore,
    sessions: &SessionRegistry,
    pipeline: &LogPipeline,
) {
    // 1. Store unconditionally (SpanStore assigns seq)
    store.insert(span.clone());

    // 2. Evaluate span triggers for each session
    let session_ids = sessions.active_session_ids();
    let mut any_oneshot_removed = false;
    for sid in &session_ids {
        let triggers = sessions.list_triggers(sid);
        for trigger in &triggers {
            if is_span_filter_str(&trigger.filter_string) {
                if let Ok(filter) = parse_filter(&trigger.filter_string) {
                    if matches_span(&filter, span) {
                        let trace_summary = build_trace_summary(span.trace_id, store);
                        let event = SessionRegistry::build_span_event(
                            sid,
                            span,
                            trigger,
                            trace_summary,
                        );
                        sessions.send_or_queue_notification(sid, event.clone());
                        pipeline.send_event(event);
                        if trigger.oneshot {
                            let _ = sessions.remove_trigger(sid, trigger.id);
                            any_oneshot_removed = true;
                        }
                    }
                }
            }
        }
    }

    // Pre-buffer size is driven by triggers' max pre_window across all sessions,
    // so we resync if any oneshot trigger was just auto-removed.
    if any_oneshot_removed {
        sync_pre_buffer_size(pipeline, sessions);
    }
}

fn is_span_filter_str(filter_str: &str) -> bool {
    parse_filter(filter_str)
        .map(|f| is_span_filter(&f))
        .unwrap_or(false)
}

fn build_trace_summary(trace_id: u128, store: &SpanStore) -> Option<TraceSummary> {
    let spans = store.get_trace(trace_id);
    if spans.is_empty() {
        return None;
    }
    let root = spans.iter().find(|s| s.parent_span_id.is_none());
    Some(TraceSummary {
        trace_id,
        root_span_name: root.map_or("[incomplete]".to_string(), |r| r.name.clone()),
        service_name: root.map_or("unknown".to_string(), |r| r.service_name.clone()),
        start_time: root.map_or(spans[0].start_time, |r| r.start_time),
        total_duration_ms: root.map_or(0.0, |r| r.duration_ms),
        span_count: spans.len() as u32,
        has_errors: spans
            .iter()
            .any(|s| matches!(s.status, SpanStatus::Error(_))),
        linked_log_count: 0, // cross-store query done at RPC level
    })
}
