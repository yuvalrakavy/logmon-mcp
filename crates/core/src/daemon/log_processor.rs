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

/// Process one entry against `domain`: assign seq, evaluate the triggers/filters of the
/// sessions bound to `domain`, store per the trigger/post-window/filter rules, and only then
/// add it to the pre-trigger buffer (so a trigger's pre-window is the records BEFORE it). Considers ONLY `domain`'s sessions —
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

    // 2. (The pre-trigger buffer takes this entry AFTER its triggers are evaluated — see
    //    below — so a trigger's pre-window is the N records before the match.)

    // 3. Evaluate triggers per session (scoped to this domain)
    let mut any_post_window_active = false;

    let session_ids = sessions.active_session_ids_sorted_by_pre_window_for_domain(domain);

    for sid in &session_ids {
        // 4a. Advance this session's post-window. It governs STORAGE only —
        // during the window every entry is stored unconditionally, bypassing
        // the session's filters, so the context after a trigger is captured.
        //
        // It deliberately does NOT skip trigger evaluation. It used to
        // `continue` here, which blinded every trigger in the session for
        // `post_window` entries after any one of them fired — so a
        // frequently-matching trigger (`l>=ERROR` during a test run) starved
        // every quiet one, and a trigger armed to catch something rare would
        // never be evaluated at all. Firing suppression is now per-trigger,
        // inside `TriggerManager::evaluate`, where a trigger debounces only
        // itself.
        if sessions.decrement_post_window(sid) {
            any_post_window_active = true;
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

            // The pre-window, then the trace's other entries still in the pre-buffer — in that
            // order, since the copy drains what the trace read would otherwise return twice —
            // stored as ONE batch. Both are OLDER than the record just stored, and the batch is
            // not sorted (the trace read returns entries older than the drained window), so the
            // store merges it into the ring in seq order (`InMemoryStore::insert_sorted`), and
            // skips anything already held — a record a filter stored earlier keeps its own
            // `source`. One merge per firing session, before that session's `context_before`
            // below reads the ring.
            let mut flushed = pipeline.pre_buffer_copy(trigger_max_pre as usize);
            if let Some(tid) = entry.trace_id {
                flushed.extend(pipeline.pre_buffer_entries_by_trace_id(tid));
            }
            for e in &mut flushed {
                e.source = LogSource::PreTrigger;
            }
            pipeline.insert_sorted(flushed);

            // Activate post-window for this session. EXTEND rather than set:
            // a match can now land inside an already-open window, and a small
            // post_window must not truncate a larger one still in flight.
            sessions.extend_post_window(sid, trigger_max_post);
            any_post_window_active = true;

            // Build notification event and send/queue
            let mut any_oneshot_removed = false;
            for m in &matches {
                // The `notify_context` records just before the match, in seq order — the
                // documented bound on `context_before`. Not the whole pre-window: that is
                // STORED (the flush above) and readable with `logs.context` around this
                // seq. And not the match itself, which `context_by_seq` includes and the
                // notification already carries as `matched_entry`.
                let mut context_before =
                    pipeline.context_by_seq(entry.seq, m.notify_context as usize, 0);
                if context_before.last().is_some_and(|e| e.seq == entry.seq) {
                    context_before.pop();
                }
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

    // The entry joins the pre-trigger buffer only now, after its own triggers ran. Appended
    // before them (as it once was), it sat in the buffer when it fired, and a pre-window of N
    // flushed the matching record itself as one of the N — N-1 records before the match.
    pipeline.pre_buffer_append(entry.clone());

    // 5. Buffer storage (if entry not already stored by trigger logic)
    if !pipeline.contains_seq(entry.seq) {
        if any_post_window_active {
            // Post-window active -- store unconditionally
            let mut store_entry = entry.clone();
            store_entry.source = LogSource::PostTrigger;
            pipeline.append_to_store(store_entry);
        } else {
            // Evaluate union of this domain's sessions' filters
            let decision = sessions.evaluate_filters_for_domain(domain, entry);

            // The epoch boundary is recorded HERE, from the policy that just
            // made the decision — not stamped on the RPC thread beside the
            // mutation that caused the flip. `entry.seq` was assigned at the
            // top of this function, so a stamp taken over there would be
            // approximate by whatever is in flight, and `complete` would end up
            // claimed over entries the other policy actually judged.
            //
            // Only this branch observes, because only this branch lets the
            // policy decide anything. An entry stored by a trigger or inside a
            // post-window is in the store whatever the filters say, so a
            // boundary that lands after a stretch of them mis-attributes
            // entries that are all present — see `engine::epoch`.
            pipeline.epochs().observe(entry.seq, &decision.policy);

            if decision.should_store {
                let mut store_entry = entry.clone();
                store_entry.matched_filters = decision.matched_descriptions;
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
