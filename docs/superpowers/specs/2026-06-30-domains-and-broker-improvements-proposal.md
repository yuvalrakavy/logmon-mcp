# Logmon Domains + Broker Improvements — Proposal

**Date:** 2026-06-30
**Status:** PROPOSAL — seed document for a dedicated design+build session. Not yet a
finalized spec; the contract sketches here are the starting point, to be refined in
that session through the normal design/review process.
**Author context:** Written from the Store project, the heaviest logmon consumer, as
part of designing multi-worktree parallel development (see the companion Store spec
`docs/superpowers/specs/2026-06-29-multi-worktree-dev-tracks-design.md` in the Store
repo, §6.9). The domain feature is a *carved-out dependency* of that work — its
contract was pinned by a real consumer, and its home is here.

---

## 0. Purpose & how to use this document

Two things prompted this: (1) parallel development needs **log isolation between
concurrent stacks**, and (2) years of daily logmon use surfaced a handful of recurring
frictions worth fixing while the broker is open for surgery. This document proposes:

- **Part A — Domains** (the primary feature): isolated per-domain ingest ports +
  buffers on the one always-on broker.
- **Part B — Usage-driven improvements**: smaller, independent fixes each backed by a
  concrete real-world scar.

Treat Part A as the main rock and Part B as an à-la-carte menu — several Part-B items
are cheap, high-value, and independent of domains; do them even if domains slip.

---

## 1. Current architecture (grounding)

Verified against the code 2026-06-30. Workspace: six crates; `broker` + `mcp` are the
shipped binaries; `core` is the engine.

- **Ingestion** — GELF (UDP+TCP) and OTLP (gRPC+HTTP) receivers, built in
  `crates/core/src/daemon/server.rs:262-314` from **scalar ports** on `DaemonConfig`,
  bound to hardcoded `0.0.0.0:{port}`. All receivers fan into **one shared**
  `log_tx` / `span_tx` channel; the processor has no idea which listener a record came
  from.
- **Storage** — **one global ring buffer for logs** (`InMemoryStore`,
  `crates/core/src/store/memory.rs:24`, single `VecDeque` under one `RwLock`) and **one
  for spans** (`SpanStore`, `crates/core/src/span/store.rs:14`). Both keyed only by a
  **single shared monotonic `seq`** (`SeqCounter`, `server.rs:204`) with a secondary
  `trace_id` index. **No partitioning exists** — not by app, service, connection, or
  session.
- **Sessions** — a "session" (`crates/core/src/daemon/session.rs`) is a **client-side
  config scope** (its own triggers, filters, bookmarks, notification queue), NOT a log
  source and NOT a data partition. Anonymous (dropped on disconnect) or named
  (persisted to `state.json`).
- **Query surface** — JSON-RPC over a Unix socket; dispatch is one `match` in
  `crates/core/src/daemon/rpc_handler.rs:41-69`. Queries run against the **global**
  store; the only scoping is `filter` (DSL) + `count` + optional `trace_id`, plus
  bookmark/cursor qualifiers resolved against a **global** `BookmarkStore`. `session_id`
  is threaded into query handlers but used **only** for bookmark-name resolution and
  `status.get` — never to scope which records are returned.

**The one load-bearing fact for domains:** *storage is a single global bucket; sessions
scope config, not data.* Domains introduce the first data partition.

---

## PART A — Domains

## 2. Concept

A **domain** is an isolated bundle of **ingest ports + log buffer + span buffer**
(+ its own seq space and bookmark namespace) that **shares nothing** with other
domains, all hosted by the single always-on broker. Producers emit to a domain's
ingest ports; consumers scope their reads to a domain. This gives real
**multi-producer / multi-consumer isolation** on one daemon.

**Two confirmed consumers** (reusability is concrete, not hypothetical):

1. **Parallel dev tracks** — each Store dev worktree ("track") runs a full stack
   (store_server + ht_server + renderer) that emits to its own domain; the agent's MCP
   session for that worktree scopes reads to that domain. No more interleaved logs from
   two concurrent feature stacks, and no reliance on remembering a `track=` filter.
2. **Production vs development** — a production Pi deployment emits to a `production`
   domain while dev runs use `dev` / `tN` domains. You can watch prod without a dev run
   drowning it out, and vice-versa. Independent of dev-tracks; needs no extra contract.

## 3. Proposed contract (MCP + CLI + SDK)

Idempotent, name-keyed. Ports may be **provided** (deterministic, caller-derived) or
**omitted** (broker allocates a free pair and returns them) — both modes are required
(dev-tracks pass derived ports; ad-hoc consumers take what they're given).

```
create_domain(name, gelf_udp_port?, gelf_tcp_port?, otlp_grpc_port?, otlp_http_port?)
    -> { name, gelf_udp_port, gelf_tcp_port, otlp_grpc_port, otlp_http_port }
    # idempotent "ensure": if the domain exists with the same ports, no-op + return.
    # if ports provided: bind them; error if a DIFFERENT domain holds them.
    # if omitted: allocate free ports and return them.
    # creates the domain's isolated receivers + log buffer + span buffer + seq space.

delete_domain(name)
    # tear down its receivers, free its ports, drop its buffers. Refuse (or require
    # force) if consumers are actively bound? (open question §5).

use_domain(name)
    # SESSION-SCOPED: from now on, every command in THIS mcp/cli session
    # (logs.recent, logs.context, traces.*, filters.*, triggers.*, bookmarks.*, clear)
    # operates on ONLY that domain's buffers. Sticky for the session's lifetime.

list_domains() -> [{ name, ports, log_count, span_count, oldest_seq, newest_seq }]
```

**Default domain = today's behavior (back-compat, non-negotiable).** A producer that
emits to the existing default ports, and a consumer that never calls `use_domain`, both
operate on a `default` domain that behaves exactly as the current global broker does.
Existing SDK consumers (store-test), existing MCP configs, and existing dashboards keep
working unchanged. `create_domain`/`use_domain` are purely additive.

**Isolation invariant:** a record ingested on domain A's port is visible **only** to
consumers on `use_domain(A)`; buffers, seq counters, cursors, filters, triggers, and
bookmarks are all per-domain. A cross-domain query is not possible by construction
(that's the point) — if cross-domain aggregation is ever wanted, it's a separate,
explicit feature.

## 4. Where it slots in (seams identified in the code)

- **Storage → domain-keyed registry.** Replace the single `InMemoryStore` (owned by
  `LogPipeline`, `pipeline.rs:58`) and single `SpanStore` (`server.rs:211`) with a
  `DomainRegistry` (e.g. `HashMap<DomainId, DomainBuffers>` behind an `RwLock`, or a
  concurrent map). Thread a `DomainId` through `append`, `recent`, `logs_by_trace_id`,
  `context_by_seq`, and every query in `rpc_handler.rs`.
- **Tag records at ingest.** Receivers currently fan into one channel with no source
  identity. Give each receiver a `DomainId` and either (a) carry it on the channel item
  (wrap `LogEntry`/`SpanEntry`), or (b) run a per-domain processor. Then `process_entry`
  (`log_processor.rs:26`) / `process_span` (`span_processor.rs:24`) route to the right
  buffer.
- **Receiver construction** (`server.rs:262-314`) — the scalar-port block becomes a loop
  over configured/created domains, each building its own `GelfReceiverConfig` /
  `OtlpReceiverConfig` bound to that domain's ports.
- **Query handlers** (`rpc_handler.rs:142-191`, `486-741`) — add domain resolution.
  `session_id` is already threaded as a per-caller identity and is the natural place to
  look up the session's bound domain (`use_domain` sets a `SessionState.domain` field),
  though semantically domain ≠ session.
- **Config** — `DaemonConfig` (`persistence.rs:87`) grows a domains section; domains
  created at runtime via `create_domain` are held in memory (and optionally persisted to
  `state.json` alongside named sessions, so they survive a broker restart — open
  question §5).
- **Capabilities** — add a `domains` capability to the hardcoded list
  (`rpc_handler.rs:88-92`) so SDK clients can feature-detect.

## 5. Key design decisions to resolve in the session

1. **Seq space: per-domain or global?** Today one `SeqCounter` feeds both stores and
   bookmarks/cursors/eviction assume a single monotonic line. Per-domain buffers most
   naturally want **per-domain seq** (isolation, independent eviction), but that touches
   the bookmark/cursor model (`store/bookmarks.rs`) and the `oldest_seq` eviction sweep.
   Recommendation to evaluate: **per-domain seq space**, with bookmarks/cursors scoped
   to a domain (they already qualify by session; add domain).
2. **How a consumer binds a domain: sticky `use_domain` vs per-query selector.** Sticky
   (session-scoped) matches how I actually work (one worktree ↔ one domain) and keeps
   every existing query signature unchanged. A per-query `domain=` selector is more
   flexible but noisier. Recommendation: **sticky `use_domain`**, optionally with a
   per-query override later.
3. **Default-domain semantics** — is `default` a real domain in the registry (uniform
   code path) or a special-cased fast path? Uniform is cleaner; verify no perf/lock
   regression on the hot single-domain case.
4. **Lifecycle & persistence** — do created domains persist across broker restarts
   (like named sessions) or are they re-created by producers on each boot? For
   dev-tracks, `dev-server.sh` re-ensures the domain on each launch, so **non-persistent
   is acceptable**; persistence is a nicety. `delete_domain` while consumers are bound:
   refuse, force, or orphan?
5. **Span/trace joins must stay within a domain.** `TraceSummary.linked_log_count`
   joins spans↔logs by trace_id at the RPC level (`span_processor.rs:90` placeholder +
   RPC join). A per-domain split must keep both sides in the same domain or the join
   silently breaks. Also: `SpanStore` has **no `clear()`** today (`memory.rs:175` has one
   for logs) — per-domain teardown needs span clearing too.
6. **Broadcast fan-out** — `PipelineEvent.session_id` is stringly-typed and the
   per-connection task filters events by string compare (`server.rs:670`). If domain
   events piggyback this fan-out, mind the O(subscribers) compare; consider a domain tag
   on the event.
7. **OTLP HTTP per-domain** — distinct ports per domain is straightforward; if instead a
   shared OTLP endpoint with a path/header discriminator is ever wanted, that's a
   different demux. (Flagged in the Store spec as a possible contract wrinkle to
   flag-back on.) Recommendation: **distinct ports per domain**, uniform with GELF.
8. **Windows transport** — the Windows TCP path (`server.rs:457`, `auto_start.rs:122`)
   is a parallel code path to keep in sync.

## 6. Dev-track consumption (the driving consumer, for reference)

From the Store spec: each track derives its ingest ports (`GELF 12201+N`,
`OTLP 4318+N`), `dev-server.sh` calls `create_domain("tN", <those ports>)` and points
the stack's tracing-init at them; the worktree's MCP session calls `use_domain("tN")`.
`t0`/home uses the `default` domain (unchanged). This is the reason the "provide ports"
mode exists.

---

## PART B — Usage-driven improvements

Each item is independent of domains unless noted. Ordered by value/effort. Every one is
backed by a concrete incident from daily use.

## B1. Strict filter parsing — kill the 0-result false-negative *(highest ROI)*

**Scar:** the single most expensive recurring logmon trap. A filter with an **unknown
selector** (`message_contains "X"`, `level>=WARN`) is silently treated as matching
nothing and returns `count=0`. That reads identically to "the log is absent" — and has
repeatedly led to hour-long misdiagnoses (e.g. concluding the GELF pipeline was dead and
restarting servers, when `m=DIAGTEST` would have found everything instantly).

**Proposal:** make the DSL parser **reject unknown selectors** with an actionable error
instead of silently matching nothing:
`unknown selector "message_contains" — did you mean m= (message)? valid: m, fm, mfm, h, fa, fi, ln, l, sn, sv, b>=, b<=, c>=`.
A bareword substring is still valid (that's a feature), but a `token=value` /
`token>=value` with an **unrecognized token** is almost always a typo, not a substring
search — so treat `<ident><op>` where `<ident>` isn't a known selector as an error.
Applies in `add_filter`, `add_trigger`, and all query methods (parser in
`crates/core/src/filter/`). This is the structural fix for a class of error a human
keeps making.

## B2. `matched` vs `scanned` counts in query responses

**Scar:** the other half of the 0-result panic — "is the filter wrong, or is the buffer
empty, or is the pipeline dead?"

**Proposal:** every `logs.recent` / `traces.recent` response carries
`{ matched: N, scanned: M, buffer_total: K, buffer_oldest_seq, buffer_newest_seq }`.
Then `matched=0, scanned=5000` (filter matched nothing, data is flowing) is instantly
distinguishable from `scanned=0` (empty buffer / dead pipeline). Cheap, purely
additive, and it makes B1's class of error self-evident even when B1 misses one.

## B3. Broker-side ack / mute of reviewed & fixed errors

**Scar:** I currently hand-maintain **memory files** (`logmon_last_seq`,
`logmon_fixed_seqs`) tracking "last error seq I reviewed" and "these seqs are known
/fixed" — because the broker has no such concept. That bookkeeping is fragile, external,
and per-agent-memory rather than shared.

**Proposal:** a broker-side **acknowledgement** facility:
- `ack(seq | signature)` / `unack(...)` — mark a record (or an error *signature*, see
  B4) as reviewed/known.
- A query qualifier `unacked` (or `-acked`) to exclude acknowledged records.
- Optionally a note attached to an ack (why it's known/fixed).
This replaces the external memory-file dance with shared broker state and pairs
naturally with the cursor (`c>=`) mechanism (cursor = "new since I looked"; ack = "and
I've dispositioned these"). Consider whether acks are global or per-domain/per-session.

## B4. Signature-based dedup / grouping

**Scar:** my `logmon_fixed_seqs` shows the *same* error recurring across dozens of seqs
(`178-179, 242-243, 277-278, 312-313: same DimmerPath error`). Scanning them
individually is noise.

**Proposal:** an optional **group-by-signature** mode on `logs.recent` — collapse
records with the same signature (normalized message + facility + file:line, trace_id
stripped) into one row with `{ count, first_seq, last_seq, sample }`. Turns "40 lines of
the same error" into one line with a count. Composes with B3 (ack a whole signature).

## B5. Rollover / retention awareness

**Scar:** buffers evict oldest-first at capacity; a query whose window (bookmark/time)
predates the oldest retained record silently returns a truncated view — evidence can
vanish without any signal.

**Proposal:** when a query's lower bound (bookmark seq, or the implied window) is older
than `buffer_oldest_seq`, set a response flag
`truncated: true, evicted_before_window: <n or unknown>` so the consumer knows records
were rolled off rather than assuming it saw everything. (Builds on B2's `buffer_oldest_seq`.)

## B6. Trigger ergonomics — TTL / auto-expire *(verify current state)*

**Note:** the capabilities list already advertises `oneshot_triggers`
(`rpc_handler.rs:88-92`), so one-shot (fire-once) triggers may already exist — **verify
before building.** The residual friction from the "set a trigger while waiting, remember
to remove it" pattern is *cleanup*: a trigger armed for a wait that succeeds silently is
left dangling and later fires on unrelated errors.

**Proposal (if not already covered):** a **TTL / auto-expire** on triggers (expire after
N seconds or on the arming session's disconnect), and/or a trigger scoped to a bookmark
window that self-removes when the window closes. Small ergonomic win for the
wait-and-watch workflow.

## B7. Richer `get_status` — per-source counts & rates

**Scar:** "are logs even flowing from store_server right now?" is currently
unanswerable at a glance (feeds the 0-result panic).

**Proposal:** `status.get` reports **per-source** (by `host` and/or GELF `service_name`
/ facility, and — post-A — **per-domain**) record counts and a recent rate
(records/sec), plus receiver liveness (packets received per listener). Makes "the
pipeline is alive and here's who's talking" immediately visible. Pairs with domains
(per-domain status is the natural home).

---

## 7. Suggested sequencing

- **B1 + B2 first** — cheap, independent, and they neutralize the highest-frequency real
  failure (0-result misdiagnosis). Do these regardless of domains.
- **Part A (domains)** — the big rock; its own design pass on the §5 decisions
  (especially per-domain seq + bookmark/cursor scoping) before coding. The receiver +
  storage seams are well-isolated (§4).
- **B7** folds naturally into the domain work (per-domain status).
- **B3 + B4 + B5** — opportunistic; B3/B4 together would retire my external memory-file
  bookkeeping entirely.
- **B6** — verify `oneshot_triggers` first; may be a no-op or a small TTL add.

## 8. Cross-references

- Store dev-tracks design (the driving consumer): Store repo
  `docs/superpowers/specs/2026-06-29-multi-worktree-dev-tracks-design.md` §6.9 (the
  domain contract as pinned by that consumer) and the derivation table (per-track ingest
  ports).
- Generic logmon guide (Store repo): `docs/guides/logmon.md` — update with domains
  (create/use/delete, per-domain isolation) once landed.
- This proposal is the seed; the dedicated session should run it through the normal
  design/review process, refine the §5 decisions, and **flag back to the Store
  dev-tracks spec** if the domain contract needs to change (the Store side is being
  built against the contract in §3).
