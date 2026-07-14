use crate::daemon::domain::DomainId;
use crate::daemon::session::SessionRegistry;
use crate::engine::pipeline::{LogPipeline, PipelineEvent};
use crate::gelf::message::{LogEntry, LogSource};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Spawns the main log processing loop for a single domain. Every entry off
/// `receiver` is processed against `pipeline` (this domain's store) and only
/// the sessions bound to `domain`.
pub fn spawn_log_processor(
    mut receiver: mpsc::Receiver<LogEntry>,
    pipeline: Arc<LogPipeline>,
    sessions: Arc<SessionRegistry>,
    domain: DomainId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut entry) = receiver.recv().await {
            process_entry_for_domain(&mut entry, &pipeline, &sessions, &domain);
        }
    })
}

/// Resize `pipeline`'s pre-buffer to the largest pre_window across the sessions
/// bound to `domain`. Call after adding/editing/removing triggers.
pub fn sync_pre_buffer_size_for_domain(
    pipeline: &LogPipeline,
    sessions: &SessionRegistry,
    domain: &DomainId,
) {
    let max_pre = sessions.max_pre_window_for_domain(domain) as usize;
    pipeline.resize_pre_buffer(max_pre);
}

/// Convenience: sync the pre-buffer for the `default` domain. Used by the boot
/// restore path and by single-domain unit tests.
pub fn sync_pre_buffer_size(pipeline: &LogPipeline, sessions: &SessionRegistry) {
    sync_pre_buffer_size_for_domain(pipeline, sessions, &DomainId::default_domain());
}

/// Process one entry against `domain`: assign seq, buffer it, evaluate the
/// triggers/filters of the sessions bound to `domain`, and store per the
/// trigger/post-window/filter rules. Considers ONLY `domain`'s sessions —
/// a filter or trigger in another domain can neither suppress storage here nor
/// fire on this record (spec §2 isolation, §9.1).
pub fn process_entry_for_domain(
    entry: &mut LogEntry,
    pipeline: &LogPipeline,
    sessions: &SessionRegistry,
    domain: &DomainId,
) {
    // 1. Assign seq
    entry.seq = pipeline.assign_seq();

    // 2. Append to pre-trigger buffer
    pipeline.pre_buffer_append(entry.clone());

    // 3. Evaluate triggers per session (scoped to this domain)
    let mut any_post_window_active = false;

    let session_ids = sessions.active_session_ids_sorted_by_pre_window_for_domain(domain);

    for sid in &session_ids {
        // 4a. Check post-window
        if sessions.decrement_post_window(sid) {
            any_post_window_active = true;
            continue;
        }

        // 4b. Evaluate triggers
        let matches = sessions.evaluate_triggers(sid, entry);
        if !matches.is_empty() {
            let trigger_max_pre = matches.iter().map(|m| m.pre_window).max().unwrap_or(0);
            let trigger_max_post = matches.iter().map(|m| m.post_window).max().unwrap_or(0);

            // Always store the triggering entry
            if !pipeline.contains_seq(entry.seq) {
                let mut trigger_entry = entry.clone();
                trigger_entry.source = LogSource::PreTrigger;
                pipeline.append_to_store(trigger_entry);
            }

            // Copy pre_window entries from pre-buffer into store
            let pre_entries = pipeline.pre_buffer_copy(trigger_max_pre as usize);
            for mut pre_entry in pre_entries {
                if !pipeline.contains_seq(pre_entry.seq) {
                    pre_entry.source = LogSource::PreTrigger;
                    pipeline.append_to_store(pre_entry);
                }
            }

            // Additionally, copy logs from the same trace_id
            if let Some(tid) = entry.trace_id {
                let trace_entries = pipeline.pre_buffer_entries_by_trace_id(tid);
                for mut trace_entry in trace_entries {
                    if !pipeline.contains_seq(trace_entry.seq) {
                        trace_entry.source = LogSource::PreTrigger;
                        pipeline.append_to_store(trace_entry);
                    }
                }
            }

            // Activate post-window for this session
            sessions.set_post_window(sid, trigger_max_post);
            any_post_window_active = true;

            // Build notification event and send/queue
            let mut any_oneshot_removed = false;
            for m in &matches {
                let context_before =
                    pipeline.context_by_seq(entry.seq, m.pre_window as usize, 0);
                let event = PipelineEvent {
                    session_id: sid.to_string(),
                    trigger_id: m.id,
                    trigger_description: m.description.clone(),
                    filter_string: m.filter_string.clone(),
                    matched_entry: entry.clone(),
                    context_before,
                    pre_trigger_flushed: trigger_max_pre as usize,
                    pre_window: m.pre_window,
                    post_window_size: m.post_window,
                    notify_context: m.notify_context,
                    oneshot: m.oneshot,
                    trace_id: entry.trace_id,
                    trace_summary: None,
                };
                // Queue for disconnected named sessions (no-op for connected)
                // and broadcast to all live subscribers; the per-connection
                // task in `daemon::server` filters the broadcast by
                // `session_id` before writing to its socket.
                sessions.send_or_queue_notification(sid, event.clone());
                pipeline.send_event(event);

                // Auto-remove oneshot triggers after dispatch. The trigger may
                // already have been removed by an earlier match in this same
                // evaluation (shouldn't happen in practice — one trigger fires
                // once per evaluation — but the remove returns NotFound rather
                // than panicking either way).
                if m.oneshot {
                    let _ = sessions.remove_trigger(sid, m.id);
                    any_oneshot_removed = true;
                }
            }

            // If we removed a oneshot trigger whose pre_window was the
            // domain-wide max, the pre-buffer can shrink.
            // sync_pre_buffer_size_for_domain computes the new max across this
            // domain's sessions so this is correct regardless of which
            // session/trigger was the previous max.
            if any_oneshot_removed {
                sync_pre_buffer_size_for_domain(pipeline, sessions, domain);
            }
        }
    }

    // 5. Buffer storage (if entry not already stored by trigger logic)
    if !pipeline.contains_seq(entry.seq) {
        if any_post_window_active {
            // Post-window active -- store unconditionally
            let mut store_entry = entry.clone();
            store_entry.source = LogSource::PostTrigger;
            pipeline.append_to_store(store_entry);
        } else {
            // Evaluate union of this domain's sessions' filters
            let (should_store, matched_descriptions) =
                sessions.evaluate_filters_for_domain(domain, entry);
            if should_store {
                let mut store_entry = entry.clone();
                store_entry.matched_filters = matched_descriptions;
                store_entry.source = LogSource::Filter;
                pipeline.append_to_store(store_entry);
            }
        }
    }
}

/// Convenience: process one entry in the `default` domain. Used by
/// single-domain unit tests.
pub fn process_entry(entry: &mut LogEntry, pipeline: &LogPipeline, sessions: &SessionRegistry) {
    process_entry_for_domain(entry, pipeline, sessions, &DomainId::default_domain());
}
