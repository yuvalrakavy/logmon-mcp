use crate::daemon::session::SessionRegistry;
use crate::engine::pipeline::{LogPipeline, PipelineEvent};
use crate::gelf::message::{LogEntry, LogSource};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Spawns the main log processing loop.
pub fn spawn_log_processor(
    mut receiver: mpsc::Receiver<LogEntry>,
    pipeline: Arc<LogPipeline>,
    sessions: Arc<SessionRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut entry) = receiver.recv().await {
            process_entry(&mut entry, &pipeline, &sessions);
        }
    })
}

/// Call after adding/editing/removing triggers to resize the pre-buffer.
pub fn sync_pre_buffer_size(pipeline: &LogPipeline, sessions: &SessionRegistry) {
    let max_pre = sessions.max_pre_window() as usize;
    pipeline.resize_pre_buffer(max_pre);
}

pub fn process_entry(entry: &mut LogEntry, pipeline: &LogPipeline, sessions: &SessionRegistry) {
    // 1. Assign seq
    entry.seq = pipeline.assign_seq();

    // 2. Append to pre-trigger buffer
    pipeline.pre_buffer_append(entry.clone());

    // 3. Evaluate triggers per session
    let mut any_post_window_active = false;

    let session_ids = sessions.active_session_ids_sorted_by_pre_window();

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
            for m in &matches {
                let context_before =
                    pipeline.context_by_seq(entry.seq, m.pre_window as usize, 0);
                let event = PipelineEvent {
                    trigger_id: m.id,
                    trigger_description: m.description.clone(),
                    filter_string: m.filter_string.clone(),
                    matched_entry: entry.clone(),
                    context_before,
                    pre_trigger_flushed: trigger_max_pre as usize,
                    post_window_size: m.post_window,
                    trace_id: entry.trace_id,
                    trace_summary: None,
                };
                sessions.send_or_queue_notification(sid, event);
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
            // Evaluate union of all sessions' filters
            let (should_store, matched_descriptions) = sessions.evaluate_filters(entry);
            if should_store {
                let mut store_entry = entry.clone();
                store_entry.matched_filters = matched_descriptions;
                store_entry.source = LogSource::Filter;
                pipeline.append_to_store(store_entry);
            }
        }
    }
}
