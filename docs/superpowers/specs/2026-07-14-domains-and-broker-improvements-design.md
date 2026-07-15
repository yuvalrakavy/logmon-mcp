# Logmon Domains + Broker Improvements — Design Spec

**Date:** 2026-07-14 (rev 2 — post design-gate)
**Status:** DESIGN — passed the T3 design gate (4 fresh-context reviewers); blockers
resolved in this revision; ready to build.
**Tier:** **T3** — first data partition, an externally-binding contract (Store dev-tracks
builds against it), and a back-compat state migration. Failing-test-first + blast-radius
review apply to the core.

> **Design-gate resolution log (rev 1 → rev 2).** Four blind reviewers (soundness/
> blast-radius, migration, B1-false-positive, buildability). Model A's *core*
> (replicate-don't-split) was confirmed sound by all three technical reviewers and is
> NOT reopened. Convergent findings drove the changes:
> - **★ Session-scoping breach (soundness + buildability, independently):** rev-1 §9.1
>   named 1 of **3** global cross-session scan sites. Fixed — §3 + §9.1 now specify the
>   `SessionState.domain` mechanism and scope all three (`evaluate_filters`,
>   `active_session_ids_sorted_by_pre_window`, span `active_session_ids`). This was a
>   cross-domain data-loss + payload-leak trap.
> - **★ Migration seq-block (soundness + migration):** default must replicate the
>   reserve-block-and-persist; and a **pre-existing** `seq_block` high-water bug (>1000
>   records/run → seq reuse on restart) is now **fixed in-scope** (§8, decision #2).
> - **★ Production bookmark durability (migration + buildability):** config-declared
>   domains now persist their bookmarks (§5, decision #1).
> - **B1 wording (false-positive lens):** rev-1's "earliest of `>=`,`<=`,`=`" wording
>   would reject the valid escape hatch `"a>=b"` / `/a>=b/`. Fixed — §B1 pins the rule
>   to the `selector=`-split path.
> - Buildability blockers (session→store plumbing, RPC naming) + concretizations
>   (OTLP dep, event re-subscribe, trigger-default conflation) folded into §3/§4/§9.

---

## 0. Scope

**In:** Part A (Domains, Model A) + B1 (strict filter parsing), B2 (matched/scanned), B5
(truncation), B7 (richer status + stall). **Plus (added at the gate):** fix the
pre-existing `seq_block` high-water-mark bug (decision #2), and persist config-declared
domains' bookmarks (decision #1).

**Deferred (named):** B3/B4 (ack + signature-dedup); B6 (trigger TTL); per-query
`domain=` override; per-domain trigger-window defaults; idle-domain reaping; proactive
(push) stall alerts. (Ephemeral domains persist nothing — that's their definition, not a
deferred gap; durable `config`/`persistent` domains persist bookmarks, §5.)

**Grounding:** every `file:line` verified against the tree on 2026-07-14 (initial pass +
4-reviewer gate). Anchors drift; re-confirm before editing.

---

## 1. Current architecture (the seams we build on)

- **Ingestion** — GELF (UDP+TCP) + OTLP (gRPC+HTTP) receivers built in
  `daemon/server.rs:262-314` from scalar ports on `DaemonConfig`
  (`daemon/persistence.rs:87-107`), bound to `0.0.0.0:{port}`, all fanning into one
  `log_tx`/`span_tx` (`server.rs:271-272`). No per-listener identity on the channel item.
- **Log store** — one global `InMemoryStore` (`store/memory.rs:24-30`): single
  `RwLock<StoreInner>` over `{ entries: VecDeque<LogEntry>, seq_set, trace_index }`. FIFO
  eviction (`:61-73`); `clear()` (`:175`); **pre-allocates** via `with_capacity` (`:36`).
- **Span store** — one global `SpanStore` (`span/store.rs:14-23`), same shape; keyed by
  seq + `trace_index`; pre-allocates (`:29`); **no `clear()`**.
- **Seq** — one shared `SeqCounter` (`server.rs:204`) feeds both stores (`:205-211`). Boot
  reserves a block ahead: `reserved_seq_block = initial_seq + SEQ_BLOCK_SIZE(1000)`
  (`server.rs:214-219`, `persistence.rs:5`), persisted before serving and re-persisted at
  shutdown (`:441`). Bookmark eviction `should_evict(bookmark_seq, oldest_log_seq,
  oldest_span_seq)` (`store/bookmarks.rs:127`) compares against **both** stores' oldest —
  valid only because logs+spans share one line.
- **Sessions** — `SessionState` (`daemon/session.rs:82-95`): config scope (triggers,
  filters, notification queue), no data, **no domain field yet**. `SessionId` enum
  `Anonymous|Named` (`:18-22`). Anonymous dropped on disconnect (`:252`); Named persisted
  (`:779-848`) / restored (`:703-774`). Triggers per-session (`TriggerManager`, `:84`),
  seeded with 2 defaults (windows `500/200/5`, `trigger.rs:81-83`).
- **Query surface** — one dispatch `match`, 25 methods (`daemon/rpc_handler.rs:42-68`).
  `RpcHandler::new` (`:20-38`) takes `pipeline`/`span_store`/`bookmarks` as fields;
  handlers read `self.*` directly. Queries hit the **global** store; `session_id` scopes
  only bookmark-name resolution, per-session config, and `status.get` (`:248-280`).
  Capabilities `["bookmarks","oneshot_triggers","client_info"]` (`:88-92`).

**Load-bearing fact:** storage is one global bucket; sessions scope config, not data.

---

## PART A — DOMAINS

## 2. Concept

A **domain** is an isolated bundle of *ingest ports + log buffer + span buffer + seq
space + bookmark namespace + receivers* that **shares nothing** with other domains.
Two confirmed consumers: **dev tracks** (worktree → `tN`) and **production vs
development** (durable `production` alongside `dev`/`tN`).

**Isolation invariant:** a record ingested on A's port is visible only to consumers on
`use_domain(A)`. Cross-domain queries are impossible by construction.

**Non-goal — isolation is not security.** No access control on
`create/delete/use_domain`; domains organize noise on a local single-user broker, they
are not a tenancy boundary. `session.*` methods (e.g. `session.list`,
`rpc_handler.rs:743`) intentionally span domains — config is global, only *data* is
partitioned; stated so it's a decision, not an accident.

## 3. Model A — a domain is a full instance of today's machinery

Each domain owns its own copy of the per-broker machinery; queries resolve the domain
**once** at the RPC boundary and then run today's inner code unchanged.

```
struct Domain {
    pipeline:   LogPipeline,      // owns InMemoryStore + SeqCounter + PreTriggerBuffer + event channel
    span_store: SpanStore,        // shares this domain's SeqCounter (as server.rs:204-211 does today)
    bookmarks:  BookmarkStore,    // per-domain (bookmarks reference this domain's seq)
    metrics:    ReceiverMetrics,  // per-domain (was one global Arc); B7 liveness fields live here
    receivers:  DomainReceivers,  // GelfReceiver + OtlpReceiver + spawned processors for this domain
    config:     DomainConfig,     // name, gelf_port, otlp_grpc_port, otlp_http_port,
                                  //   log_buffer_size, span_buffer_size, source: Config|Persistent|Ephemeral
    // B7 liveness: last_log_received_at / last_span_received_at (AtomicU64 epoch-ms)
}
struct DomainRegistry { domains: RwLock<HashMap<DomainId, Arc<Domain>>> }  // DomainId = validated name
```

**Ownership split:** the `Domain` owns all **seq-referencing** state (pipeline, span
store, bookmarks, receivers, metrics); the **session** owns config (triggers, filters,
notification queue) + the domain-binding.

**The session→domain→store plumbing (resolves buildability Blocker 1 — this is concrete,
not "one line"):**
- `SessionState` gains `domain: RwLock<DomainId>`, defaulting to `DomainId("default")` in
  `new_anonymous`/`new_named`/`restore_named`. Mutable (unlike `name`) because
  `use_domain` switches it.
- `SessionRegistry` gains `domain_of(&SessionId) -> DomainId` and `set_domain(&SessionId,
  DomainId)` (mirroring `set_client_info`, `session.rs:271`).
- `RpcHandler::new` takes `domains: Arc<DomainRegistry>` **instead of**
  `pipeline`/`span_store`/`bookmarks`. `handle()` resolves `let d =
  self.domains.get(&self.sessions.domain_of(session_id))?` once, and **passes `&d: &Domain`
  as a new first parameter into every private `handle_*` method** (each currently reads
  `self.pipeline`/`self.span_store`/`self.bookmarks`; they switch to `d.pipeline` etc.).
  The store-internal code is byte-for-byte unchanged; the ~25 handler *signatures* change.

**What Model A dissolves:** per-domain seq is free (each `Domain` has its own
`SeqCounter`); `default` = the domain named `"default"`, `N=1` today (uniform path);
delete-while-bound = `Arc` refcount; trace↔log join stays in-domain automatically;
per-domain broadcast channel. (Verified by the gate: `should_evict`, cursors,
`bookmarks.add` default start_seq, and per-domain seq non-collision all hold under
replication.)

## 4. The contract (MCP + CLI + SDK)

Method names follow the enforced `<group>.<verb>` convention (wire →
`protocol/src/methods.rs` `<Group><Verb>` → SDK `<group>_<verb>` → CLI `logmon domains
<verb>`). All ports/tunables **optional with sensible defaults** —
`domains.create{name:"t3"}` alone works.

```
domains.create(name, gelf_port?, otlp_grpc_port?, otlp_http_port?,
               log_buffer_size?, span_buffer_size?, persist?=false)
  -> { name, gelf_port, otlp_grpc_port, otlp_http_port,
       log_buffer_size, span_buffer_size, source:"ephemeral"|"persistent" }
  # Idempotent ensure. gelf_port serves UDP+TCP.
  # persist=false (default) → EPHEMERAL runtime domain (gone on restart; dev-tracks).
  # persist=true → DURABLE: also recorded in a machine-owned domains store so it is
  #   re-created on the next boot. Promotes an existing ephemeral domain in place.
  #   (HOW it persists is an implementation detail — a machine-owned store, NOT
  #    config.json, which stays user-authored and read-only.)
  # provided ports → bind (error if a DIFFERENT domain holds any); omitted → allocate.
  # A port explicitly set to 0 DISABLES that receiver for the domain (mirrors the
  #   daemon-level `otlp_grpc_port>0 || otlp_http_port>0` gate, server.rs:287).
  # exists w/ same ports → no-op; different ports → error. Refuse if max_domains reached.
  # Ports acquired SYNCHRONOUSLY (incl. OTLP pre-bind, §9.3) → clean conflict errors.

domains.delete(name) -> { name, deleted:true }
  # Deletes ephemeral OR persistent (API-created) domains — for a persistent one it
  #   also removes it from the machine-owned store. Refuses `config`-declared domains
  #   (incl. default): those are user-authored — edit config.json. Arc-graceful teardown.

domains.use(name)          # session-scoped sticky bind; switchable; error if absent; none → default.
                           # Also a `domain` param on session.start (atomic connect-time bind).
                           # session.domain is connect-time state, never persisted.

domains.clear()            # NEW method. Dispose the BOUND domain's DATA: logs + spans
                           #   (InMemoryStore::clear + new SpanStore::clear). Keeps domain
                           #   + receivers alive; seq stays monotonic. `logs.clear` is left
                           #   UNCHANGED (logs-only, back-compat). No separate spans.clear.

domains.list()
  -> { domains: [ { name, gelf_port, otlp_grpc_port, otlp_http_port,
                    source:"config"|"persistent"|"ephemeral", log_buffer_size, span_buffer_size,
                    log_count, span_count, oldest_seq, newest_seq,
                    last_log_received_at, last_span_received_at, idle_secs, stale } ] }
  # Pure registry view — no session context. (Liveness fields populated in Wave 3 / B7;
  #   Wave 2 ships them null.)
```

**Flag-backs to the Store dev-tracks spec (§13):** 3 ports not 4 (`gelf_port` = UDP+TCP);
method names `domains.create/use/...`; the `persist` flag + `source:config|persistent|ephemeral`
taxonomy; `session.start` `domain` param; `domains.clear`; liveness fields; the OTLP beacon
caveat (§9.8). Derivation stays `gelf 12201+N / otlp_grpc 4317+N / otlp_http 4318+N`.

**Capabilities:** add `"domains"` (`rpc_handler.rs:88-92`).

## 5. Lifecycle & persistence

Three sources → the `source` field. A **machine-owned domains store** holds API-created
durable domains, so `config.json` stays user-authored and read-only:
- **`config`** — `DaemonConfig` grows an optional `domains` array (JSON — the config is
  `serde_json`, `persistence.rs:161`, **not** TOML). `default` is the implicit entry.
  Durable; `domains.delete` refuses these (edit the file).
- **`persistent`** — `domains.create(..., persist=true)`. **The MCP/CLI path to a durable
  domain.** Broker-managed: recorded in a machine-owned domains store (a small
  `{name, ports, sizes}` declaration file, re-created fresh at boot like a config domain —
  so it needs no `seq_block`/state machinery and can't reintroduce the restart-seq edge
  cases). `domains.delete` removes it.
- **`ephemeral`** — `domains.create` (default `persist=false`). Gone on restart, re-ensured
  by the producer (`dev-server.sh` does this) — the dev-tracks default; binds ports only
  for tracks actually running.

`max_domains` (config, default 32) caps the **API-created** domains (ephemeral +
persistent); `config` + `default` are always honored and don't count against it.

**Bookmark durability follows domain durability (decision #1).** DURABLE domains
(`config` + `persistent`) **persist their `BookmarkStore`** to `state.json`; `ephemeral`
domains do not. Concretely: `snapshot_named_for_persistence` (`session.rs:779-848`) /
`restore_named` (`:703-774`) iterate the durable domains' bookmark stores (today they
touch one global store; the call site is `server.rs:227`). A durable domain's user
annotations survive restart, consistent with the domain itself surviving.

**Delete-while-bound, reader side:** a query against a deleted domain returns
`domain "t3" no longer exists — use_domain to rebind` — **no** silent fallback to
`default`. Since `session.domain` isn't persisted, restart lands everyone at `default`
and clients re-bind — consistent.

**Anonymous bookmark cleanup across domains:** an anonymous session that switched domains
may hold bookmarks in more than one store; on disconnect, clear it across every domain it
touched (track touched-domains on the session), rather than only its current binding
(`server.rs:695`, `bookmarks.clear_session`).

## 6. Tunables, lazy allocation, trigger-window defaults

- **Per-domain sizes:** `log_buffer_size` (default `buffer_size`=10000) + `span_buffer_size`
  (default 10000), on `domains.create` + config. Prod: `log_buffer_size=100_000`.
- **Lazy buffer allocation (invariant):** replace `VecDeque::with_capacity(capacity)`
  (`memory.rs:36`, `store.rs:29`) with `VecDeque::new()` + one-time `reserve_exact(max_capacity)`
  on the **first** `append`/`insert` (guard `entries.capacity()==0`). Idle domain → ~0
  buffer memory; steady-state footprint unchanged. Pre-trigger buffer already lazy.
- **Trigger-window defaults (decision #4 — note the two distinct hardcodes):**
  `DaemonConfig` gains `default_trigger_pre_window/post_window/notify_context` (optional,
  default `500/200/5`). These are applied at **both** places that currently hardcode: (a)
  `TriggerManager::new()`'s seeded built-in triggers (`trigger.rs:81-83`), and (b) the
  ad-hoc `triggers.add`/`triggers.edit` omission default, which is **`0` today**
  (`rpc_handler.rs` `handle_triggers_add`, independent of `trigger.rs`). **Behavior
  change:** omitting a window on `triggers.add` now yields `500/200` instead of `0`, so an
  ad-hoc trigger captures context by default. Called out intentionally.

## 7. `status.get` — session situational awareness

Extend `handle_status` (`rpc_handler.rs:248-280`) with `current_domain` and an echo of
`active_filters` (strings; `filters.list` `:286` already returns them). One call answers
"where am I / what's narrowing me / is data flowing." B7 adds per-domain/per-source counts.

## 8. Migration (T3 data-integrity) + the seq-block fix

**Existing `state.json` → default-only world (gate verdict: SAFE):** `seq_block` seeds the
`default` domain's `SeqCounter`; named sessions restore as config (unbound → default);
restored bookmarks land in **default's** `BookmarkStore` (confirmed persisted +
seq-carrying, `session.rs:779-848` / `restore_named` / `insert_persisted`
`bookmarks.rs:194`). No field is dropped. The in-memory buffers are never persisted
(`persist_buffer_on_exit` is defined-but-unwired). **`default` must replicate the
reserve-block-and-persist on every boot** (`server.rs:214-219`) — not just inherit the seq
value — or restored cursors mis-resolve.

**Fix the pre-existing seq-block high-water bug (decision #2 — in scope):** today
`reserved_seq_block = initial_seq + 1000` is a **fixed** step that never tracks
`seq_counter.current()`, so a run emitting **>1000 records reuses seqs after any restart**,
mis-slicing persisted cursors. Fix: on shutdown persist
`seq_block = max(reserved_seq_block, counter.current())` (per domain that persists) so seqs
never rewind. Orthogonal to domains but a real data-integrity bug in the path we're
touching. Own test: a >1000-record run, restart, assert a pre-restart cursor still resolves
correctly.

## 9. Implementation seams

1. **★ Scope ALL THREE global session scans to the domain (the gate's load-bearing fix).**
   The per-domain processors must consider only sessions bound to their domain. Add a
   domain arg (backed by §3's `SessionState.domain`) to each:
   - `evaluate_filters` (`session.rs:521` → `log_processor.rs:139`) → `..._for_domain(domain, entry)`.
     **This is the data-loss site** — global today, a filter in domain B would suppress
     storage of domain A's non-matching logs (breaks §2). MANDATORY.
   - `active_session_ids_sorted_by_pre_window` (`session.rs:329` → `log_processor.rs:36`) →
     `..._for_domain(domain)`. Prevents foreign trigger firing + `post_window` `AtomicU32`
     corruption from two processors racing one counter.
   - `active_session_ids` for spans (`session.rs:696` → `span_processor.rs:34`) →
     `..._for_domain(domain)`. **Prevents real cross-domain payload delivery** — a B-bound
     session's span trigger firing on A's spans and queuing A's data into B's notification
     queue (`send_or_queue_notification` `session.rs:603`, drained with no domain check).
   - Plus `evaluate_trigger` (`session.rs:344`) scoping, as before.
2. **Pre-buffer re-sync on bind.** `use_domain` must call `sync_pre_buffer_size`
   (`log_processor.rs:21`) for **both** the old and new domain (old may shrink, new may
   grow), and `max_pre_window` must be **domain-scoped** (`session.rs:408`) — else a
   rebind leaves the new domain's pre-buffer at cap 0 and triggers fire with **zero
   pre-context** (silent). (This is why §9.1 alone is insufficient.)
3. **Runtime receiver lifecycle + OTLP sync pre-bind.** `Gelf/OtlpReceiver::start`
   (`server.rs:279-293`) are re-callable; all four types have stop paths (GELF
   oneshot-on-drop `udp.rs:68`/`tcp.rs:92`; OTLP `serve_with_shutdown` `grpc.rs:369`/
   `http.rs:61` + `Drop` abort `otlp/mod.rs:87-107`). GELF binds synchronously (clean
   `Err`); **OTLP binds inside its spawned task**, so `domains.create` must pre-bind: HTTP
   → hand `axum::serve` a pre-bound `TcpListener` (it already accepts one); gRPC → tonic
   `serve_with_incoming_shutdown` fed by `tokio_stream::wrappers::TcpListenerStream` —
   **adds a `tokio-stream` dependency to `crates/core`** (not currently present).
4. **Event subscription follows `use_domain`.** `handle_connection`'s `select!` loop owns
   `event_rx` (`server.rs:642,646`); a `use_domain` handled inside `handler.handle()`
   (`:650`) can't reach it directly. Mechanism: after each `handle()`, the loop re-reads
   `sessions.domain_of(&session_id)`, diffs against a cached value, and re-subscribes to
   the new domain's channel on change.
5. **`SpanStore::clear()`** — add it (mirrors `InMemoryStore::clear`; `SpanStore` doesn't
   implement `LogStore`, so it's a self-contained inherent method). Needed by
   `domains.clear` + `domains.delete`.
6. **Trace↔log join stays in-domain** — real join (`rpc_handler.rs:499-503`) resolves
   within the bound domain automatically; the notification placeholder
   (`span_processor.rs:90`, hardcoded 0) must not cross domains.
7. **Windows transport parity** — the `#[cfg(windows)]` control socket (`server.rs:452-523`,
   `auto_start.rs:122`) is unaffected by ingest ports; keep in sync on server bring-up.
8. **OTLP beacon is a protocol change, not a call-site tweak.** `send_otel_beacon("OTEL:ONLINE")`
   (`server.rs:55-60,297`) is a fixed multicast with no port/domain — with N domains a
   producer keying off it can be released by the wrong domain's OTLP. Making it
   per-domain-meaningful requires adding port/domain to the payload — a wire change; flag
   to Store (§13). Interim: suppress the beacon for API-created domains, or leave it default-only.
9. **Port allocation** (omitted mode): allocate from an ephemeral range, bind
   synchronously, retry on race, return chosen ports.
10. **Name validation** reuse `is_valid_session_name` (`session.rs:164`, `[A-Za-z0-9_-]`);
    **lifecycle logging** of create/delete/bind + bind failures (ERROR=fix-code, WARN=zero-at-idle).

---

## PART B — Usage-driven improvements

## B1 · Strict filter parsing

**Why "reject unknown selectors" is too blunt:** `parse_selector` maps any unknown token
to `Selector::AdditionalField` (`parser.rs:167,285`) — a real feature (`_track=t3`).
So flag **only the provable** (the "blocking check biases to false-negatives" rule):

**Provable typo → hard error.** Evaluate the rule **on the `selector=value` path only —
after** the quoted-pattern (`parser.rs:381`) and bare-regex (`:390`) branches, keyed off
the **first `=` split** (`:395`): if `lhs.ends_with('>') || lhs.ends_with('<')`, strip that
trailing char and reject when the remainder ∉ `{l,d,b,c}`. Catches `level>=WARN`,
`duration>=100`, `retries>=3`, `L>=warn`.
> **Do NOT** implement as a raw "earliest of `>=`,`<=`,`=` over the token" scan — that
> false-positives on the valid escape hatch `"a>=b"` (bare quoted, `:381`) and `/a>=b/`
> (bare regex, `:390`), which the quote/regex branches must claim first. (`_url=a>=b`,
> `m=x>=y` are safe under the split rule: their first `=` is the selector `=`.) The
> `{l,d,b,c}` exemption is dead code — those are consumed by strip_prefix above — but keep
> it (belt-and-suspenders vs a future reorder) with a comment saying why it's unreachable.
> Compare the base **without** stripping internal whitespace so `l >= warn` errors rather
> than silently becoming `AdditionalField("l >")`.

**Error must be actionable:** name the token, suggest the intended selector, **and point
to the escape hatch** — `unknown selector "retries>" — did you mean l>= / d>= ? For a
literal search, quote it ("retries>=3") or use regex (/retries>=3/). valid: m, fm, mfm, h,
fa, fi, ln, l, sn, sv, st, sk`.

**Ambiguous `unknown=value` → soft hint, not rejection.** Can't prove `message_contains=X`
is a typo; don't block it. Backstop via B2: when an additional-field selector matches **0
of scanned>0** records, attach a hint. Gate the hint on **field-KEY absence** across
scanned records, not just value-mismatch — the matcher's `.get(name)` (`matcher.rs:99-109`)
already distinguishes key-absent from present-but-non-matching, so a valid-but-absent
`_track=t3` (field present, value differs) does **not** fire a misleading "did you mean m=?".

Applies at every `parse_filter` site. Hard errors fail fast at `add_filter`/`add_trigger`;
hints ride query responses.

## B2 · `matched` / `scanned` counts

Every query-shaped response carries `{ matched, scanned, buffer_total, buffer_oldest_seq,
buffer_newest_seq }` — `matched=0, scanned=5000` (filter's fault) vs `scanned=0` (empty/
dead). Applies to `logs.recent` + `traces.recent` (primary), and — for consistency —
`logs.export`, `logs.context`, `traces.get/slow/logs`, `spans.context` where a count is
meaningful (pin per-method in the test list). Thread `scanned` out of `recent`
(`memory.rs:82`). Additive — **verify `crates/protocol`/`crates/sdk` response structs
don't `deny_unknown_fields`**. Powers B1's soft hint.

## B5 · Truncation awareness

When a query's lower bound (`b>=`/`c>=`-resolved seq, or window) is older than the bound
domain's `buffer_oldest_seq`, set `truncated:true, evicted_before_window:<n|"unknown">`.
Resolve at the `bookmark_resolver` boundary (`filter/bookmark_resolver.rs`) against
`d.pipeline.oldest_log_seq()` — either grow `ResolvedFilter` with a `truncated` field or
compare post-hoc at the RPC site. Rides B2's `oldest_seq`.

## B7 · Richer status + stall detection (two tiers)

- **Tier A (cheap):** per-domain counts + `last_log/span_received_at` (broker-ingest
  epoch-ms, stamped on append/insert — not the producer's timestamp) + `idle_secs` +
  `stale` (trips when `idle_secs > stale_after_secs`; config default 12h, optional
  per-domain override). Per-listener liveness (gelf_udp/tcp, otlp_grpc/http) rides the
  now-per-domain `ReceiverMetrics` (§3; `receiver/metrics.rs:18-25`) — add `last_received_at`
  + received-count per `ReceiverSource` to see *which transport* stalled.
- **Tier B:** per-source (host / service_name) counts + recent rate (records/sec) — new
  per-source map + windowed rate. Answers "is store_server talking right now?".

Surfaced in `status.get` (current domain, per-source) + `domains.list` (per-domain).
**Pull-based** (a stalled domain has no consumer to push to).

---

## 10. Build order

**Wave 1 — response-shape + parser (no domain coupling; self-verifiable):** B2 → B1
(now gate-verified) → B5.

**Wave 2 — Part A core (T3: failing-test-first + blast-radius):**
4. `SessionState.domain` + registry `domain_of`/`set_domain`; `Domain`/`DomainRegistry`;
   `RpcHandler` restructure (holds `Arc<DomainRegistry>`, threads `&d`); `default`=`N=1`;
   **the three-site scoping (§9.1)** + pre-buffer re-sync (§9.2); migration (§8) + the
   seq-block fix, each with its own test.
5. Runtime receiver lifecycle (`domains.create/delete`, OTLP sync pre-bind + `tokio-stream`),
   config-declared domains + persistence of their bookmarks (§5), `max_domains`.
6. Binding (`domains.use` + `session.start`), `domains.clear` + `SpanStore::clear`, event
   re-subscribe (§9.4), lazy allocation, trigger-window defaults (§6).
7. Contract surface: `domains.*` methods, `"domains"` capability, `status.get` additions.

**Wave 3 — B7:** Tier A (domain + receiver liveness + stall; populates the `domains.list`/
`status.get` liveness fields), then Tier B (per-source + rates).

Wave 1→2 stays low-rework: B2/B5 run against `d.pipeline` after the resolve-at-boundary
lands; only the field access changes.

## 11. Test list (verification + adversarial)

**Part A**
- **Isolation (the gate's three sites):** a filter set by a B-bound session does NOT drop
  A's logs (`evaluate_filters` scoping); a B-bound trigger does NOT fire on A's records
  (log + span enumeration); a B-bound span trigger does NOT deliver A's span payload into
  B's queue; two domains' processors don't corrupt a shared session's `post_window`.
- Pre-buffer: after `use_domain(B)`, a trigger with `pre_window=500` captures 500 pre-records
  from B (not zero) — the rebind re-sync.
- Isolation baseline: record on A's port invisible under `use_domain(B)`; seq=5 in A and B
  don't alias.
- Default back-compat: no `use_domain` behaves exactly as today.
- **Migration:** old `state.json` → default domain, seqs preserved, bookmarks resolvable
  against `default`; `default` re-reserves+persists the seq block on boot.
- **Seq-block fix:** >1000-record run, restart, a pre-restart cursor still resolves correctly.
- **Durable domains + bookmarks:** `domains.create(persist=true)` returns
  `source:"persistent"` and the domain is re-created from the machine store after restart;
  `domains.delete` removes it. A bookmark in a durable domain (`config` or `persistent`)
  survives restart; an `ephemeral` domain's does not.
- Lifecycle: idempotent create (same/different/held ports); delete refused on config/default;
  Arc-graceful delete-while-bound; query-after-delete → rebind error (no silent fallback).
- `max_domains` refusal (API-created only); omitted-ports allocation; **OTLP port clash → clean
  synchronous error** (prove the pre-bind catches it, not a dead task); `port=0` disables that receiver.
- `domains.clear` empties logs+spans, keeps receivers, seq monotonic.
- Lazy alloc: created-but-idle domain allocates ~0 buffer; first record allocates once.

**Part B**
- B1 hard-error: `level>=WARN`, `duration>=100`, `retries>=3`, `L>=warn`, `l >= warn` → error
  with escape-hatch guidance. **B1 false-positive (load-bearing):** `"a>=b"`, `/a>=b/`,
  `/x<=y/`, `_url=a>=b`, `m=x>=y`, `_track=t3`, bareword, `/regex/i` → all still valid.
- B1 soft-hint: fires on a key-absent additional field with scanned>0; **silent** on a
  present-but-value-mismatch field (`_track=t3` when records have `_track=t1`) and on scanned=0.
- B2: counts correct on empty/full/matches-none/matches-all; old client ignoring new fields works.
- B5: bookmark older than `oldest_seq` → `truncated`; in-range → not.
- B7: `stale` crosses threshold; per-listener liveness distinguishes stalled GELF vs live OTLP.

## 12. Design gate — DONE (rev 2)

Four fresh-context reviewers ran (soundness/blast-radius, migration, B1-false-positive,
buildability). Model A's core confirmed sound; convergent findings (session-scoping,
seq-block, production bookmarks) resolved above. A **light re-verification of the §9.1
three-site rewrite** is warranted before Wave 2 (it's the load-bearing change), but Wave 1
(B2/B1/B5) is domain-independent and B1 is already gate-verified, so Wave 1 may start now.
Per-phase self-review + mid-checkpoint after Wave 2 step 4 + deep adversarial gate over the
full diff before merge still apply.

## 13. Flag-backs to the Store dev-tracks spec

Contract deltas Store must absorb: **3 ports not 4**; method names `domains.create/use/
delete/list/clear`; the **`persist` flag** on `domains.create` + the
`source:config|persistent|ephemeral` taxonomy (so MCP/CLI can create durable domains);
`session.start` `domain` param; `domains.list` shape; stall/liveness fields; and the
**OTLP beacon caveat** (§9.8 — the current `OTEL:ONLINE` beacon can't distinguish domains
without a payload/wire change; if dev-tracks' tracing-init keys off it, that needs
addressing). Derivation stays `gelf 12201+N / otlp_grpc 4317+N / otlp_http 4318+N`.

## 14. Cross-references

- Seed proposal: `2026-06-30-domains-and-broker-improvements-proposal.md`.
- Store dev-tracks design: Store repo
  `docs/superpowers/specs/2026-06-29-multi-worktree-dev-tracks-design.md` §6.9.
- Generic logmon guide (Store repo) `docs/guides/logmon.md` — update once landed.

## 15. §9 re-verify addenda (fold in during Wave 2 build)

A fresh pre-build re-gate confirmed §9.1's three-site scoping is COMPLETE for the live
path (`evaluate_filters`, `active_session_ids_sorted_by_pre_window`, `active_session_ids`
— plus `max_pre_window` in §9.2 = the fourth) and the resolve-at-boundary mechanism is
sound. Localized fixes to fold in:

- **F1 (isolation) — notification queue is domain-agnostic.** §9.1 scopes producers, not
  the per-session `notification_queue` (`session.rs:87`; unconditional drain at
  `server.rs:634`). A cross-domain reconnect (`session.start{name:S, domain:B}` for an S
  that queued events while bound to A) ships A's payload to a B client. Fix: `set_domain`
  clears `notification_queue` when the binding actually changes. → §11 isolation suite.
- **F2 (footgun) — dead cross-domain scans.** `any_post_window_active` (`session.rs:591`)
  and `queue_notification` (`session.rs:635`) are prod-DEAD (test-only callers). When
  adding the `_for_domain` variants, delete them or give them domain-scoped signatures so
  the un-scoped forms can't be reached for.
- **F3 (wart) — post-window carryover on rebind.** `post_window_remaining` (`session.rs:89`)
  follows the session across domains; a trigger that fired on A then shapes B's PostTrigger
  storage. Fix: reset to 0 in `set_domain` (or accept + document). → §11.
- **F4 (buildability) — `sync_pre_buffer_size` fan-out.** Called from **8 sites** (5 RPC
  handlers → `&d`; 2 processors → captured domain-id; boot-restore `server.rs:324` →
  default); gains a `domain: &DomainId` param. In `use_domain`, `set_domain` MUST run
  BEFORE syncing both old+new domains (else A won't shrink / B won't grow).
- **F5 (wording).** Drop §9.1's "plus `evaluate_trigger`" bullet — `evaluate_triggers`
  (`session.rs:344`) is already per-`SessionId`, auto-scoped once sites 1/3 are.
- **F6 (keystone/stage 2.1).** Fold `run_with_overrides`'s pipeline (`server.rs:204`),
  span_store (`:211`), seq (`:204`), bookmarks (`:225`), receiver_metrics (`:233`), the 3
  processor spawns (log `:321`, span `:306`) and BOTH receiver arms (injected `:247` vs
  real `:262`) into `default`'s Domain. Initialize `event_rx` (`server.rs:642`) and §9.4's
  re-subscribe cache from the CONNECT-TIME domain (a reconnecting named session may already
  be bound non-default). Make the contract explicit: `DomainRegistry::get` clones the
  `Arc<Domain>` OUT of the `RwLock` guard so queries never hold the registry lock during
  execution — this is what makes delete-while-bound Arc-graceful.

---

## 16. Implementation status (Wave 2 — updated 2026-07-15)

Wave 2 was built in a testability-driven refinement of §10's step order.

**Done — the domain feature is core-complete:**
- **Step 4 / stage 2.1** (keystone): `Domain`/`DomainRegistry`, resolve-at-boundary,
  §9.1 three-site scoping, the seq-block fix (§8), F1–F6. Commits `01a6221`,
  `654a5f7`, `723baec`.
- **Step 5 / stage 2.2 — EPHEMERAL lifecycle** (`f512ea2`, `5c7906e`):
  2.2a OTLP synchronous pre-bind (§9.3) + `tokio-stream` dep (port clash → clean
  `Err`, aligning OTLP with GELF's fail-loud boot); 2.2b `domains.create/delete/list`
  (ephemeral) + per-domain `DomainReceivers` (Arc-graceful teardown) + `max_domains`
  + port allocation, wired as core RPC methods and tested end-to-end via real
  GELF/OTLP ports (ingest isolation).
- **Step 6 / stage 2.3 — binding + disposal** (`bb0ab67`, `d868e76`, `5feaccb`,
  `f0c7b4e`): 2.3a `domains.use` + `session.start` `domain` param + event
  re-subscribe (§9.4) + pre-buffer re-sync (§9.2/F4) + delete-while-bound keep-alive;
  2.3b `domains.clear` + `SpanStore::clear`; 2.3c lazy buffer allocation (§6);
  2.3d ad-hoc trigger windows default `0 → 500/200/5` (§6, decision #4).

**Reorg vs §10:** `domains.create/delete/list/use/clear` were wired as core RPC
methods DURING steps 5–6 (not batched into step 7), so each stage is testable
end-to-end through the harness. Step 7 is thus reduced to the CLIENT surface.

**Deep gate — core (steps 4–6), done 2026-07-15** (commit `3434795`): 3 breadth
finders (Sonnet) + 1 depth finder (Opus, blast-radius), fresh contexts, distinct
lenses. The core T3 invariants were verified **clean** — per-domain isolation (all
~24 data handlers resolve at the boundary; no global-store reach), seq independence
(fresh `SeqCounter` per domain), Arc-graceful teardown (no spawned task holds a
strong `Arc<Domain>` back to its own domain, so delete drops the last Arc), full
blast-radius on every changed signature, and default-path back-compat. Findings
were adjudicated against the code (not taken on the finders' word) and remediated:
- **A (HIGH, 3-finder convergence):** `domains.create` check→bind→insert straddled
  the port-bind `.await` on the shared handler; concurrent same-name creates
  orphaned receivers + overshot `max_domains`. Fixed with an async create-lock.
- **B (HIGH):** unclamped client buffer sizes → `reserve_exact` → process abort on
  first ingest. Bounded at `MAX_DOMAIN_BUFFER_SIZE` (10M).
- **C (back-compat):** OTLP port clash at boot aborted the whole daemon (incl.
  GELF). **Decision (revises §9.3 for the boot path):** the default domain's
  OPTIONAL OTLP now DEGRADES at boot (warn + disable, keep serving); explicit
  `domains.create` keeps §9.3 fail-loud. GELF stays fail-loud (core function).
- **D (§5, 4-finder convergence):** anonymous cross-domain bookmark cleanup swept
  only the current binding. Added per-session touched-domains tracking.
- **G (LOW):** explicit `handle()` arm for the async-only `domains.create`.

Deferred to 2.4 (noted, not lost): notification micro-race on `domains.use` (E;
tiny window, data retained, connected-only, no client surface yet), CLI
`triggers add` still `0/0/0` (F; pre-existing, out-of-diff — the daemon default
only fires on omission, clap always sends `Some(0)`), OTLP single-arm-`0` allocates
a random port instead of disabling (H; mirrors the daemon gate, §4 wording).

**Step 7 / stage 2.4 — surfacing, done 2026-07-15** (commit `6cc5098`): SDK
`domains_create/delete/list/use/clear`; MCP `create_domain/delete_domain/
list_domains/use_domain/clear_domain` tools; `logmon domains create/delete/list/
clear` + a global `--domain NAME` connect-time bind — the one-shot CLI's stand-in
for sticky `use_domain` (`logmon --domain t3 domains clear` clears a specific
domain); the `"domains"` capability; and `status.get` `current_domain` +
`active_filters`. New logic (status fields, capability) is failing-test-first; the
SDK/CLI wrappers have round-trip tests; the MCP tools are mechanical `broker.call`
mirrors of the gated wire methods (covered transitively — no MCP harness exists).
Docs (skill + 3 READMEs) updated.

**Surfacing gate (done 2026-07-15, commit `b2582be`):** 2 fresh-context finders.
Wrapper fidelity (all 10 SDK/MCP wire-method strings + param forwarding) verified
clean. Caught a **CRITICAL** the author's own design narrative had missed: the CLI
connects with a *persistent named* session (`"cli"`), so the `--domain` flag's
`domains.use` bind was **sticky** — `logmon --domain t3 status` then an unflagged
`logmon status` silently kept serving t3. Fixed: every non-registry command now
binds to `--domain`-or-`default` (reset per invocation); registry verbs
(create/delete/list) are domain-agnostic and skip the bind (so
`--domain t3 domains create t3` works). Also locked in idempotent
same-explicit-ports re-create for stateless dev-tracks (user request — the
existing `ensure_idempotent` already did it; added the explicit-ports test).

**Remaining:**
- **DEFERRED — durability** (config-declared + persistent domains + durable bookmark
  persistence — the rest of step 5's §5). Blocked on a persisted-schema decision.
  **Gap found during 2.2:** §5/decision-#1 (durable domains persist bookmarks)
  conflicts with §5's "persistent domains start fresh, no seq_block machinery" — a
  bookmark anchors a **seq**, so a durable domain that resets its seq on reboot
  dangles its restored bookmarks. `default` avoids this only because it persists its
  seq high-water. Fixing it for durable non-default domains needs **per-domain seq
  persistence in `DaemonState`**. The primary consumer (dev-tracks) uses ephemeral
  domains and does not need durability, so this was deferred pending that decision.

## 17. Durability build design — Option A (chosen 2026-07-15)

Durable domains reach **full parity with `default`**: re-created at boot, per-domain
seq high-water persisted, bookmarks persisted and domain-routed. The `default`
domain's existing machinery (seq_block reserve-at-boot / persist-at-shutdown;
`named_sessions[].bookmarks`) is **generalized**, not replaced — every addition is
`#[serde(default)]`, so an existing `state.json`/`config.json` loads byte-identically
(**zero migration**; this is the load-bearing property for a data-integrity change).

### 17.1 Schema

**`config.json` (`DaemonConfig`)** — user-authored config domains:
```rust
#[serde(default)] pub domains: Vec<ConfigDomain>,
// ConfigDomain { name: String, gelf_port/otlp_grpc_port/otlp_http_port: Option<u16>,
//                log_buffer_size/span_buffer_size: Option<usize> }
```

**`state.json` (`DaemonState`)** — machine-owned:
```rust
#[serde(default)] pub domain_seq_blocks: HashMap<String, u64>,   // durable NON-default seq high-water
#[serde(default)] pub persistent_domains: Vec<PersistentDomainDecl>, // API-created durable decls
// PersistentDomainDecl { name, gelf_port, otlp_grpc_port, otlp_http_port: u16,
//                        log_buffer_size, span_buffer_size: usize }  // ports are concrete (allocated)
```
`default`'s seq stays in the existing top-level `seq_block` (special-cased — the
zero-migration wart we accept over restructuring the one field crashes would corrupt).

**`PersistedBookmark`** gains `#[serde(default = "default_domain_str")] pub domain: String`
(`"default"`). Old bookmarks (no field) → `default`. Snapshot tags each; restore routes
by it.

### 17.2 Boot (`server.rs`, after the default domain is assembled)

1. Assemble `default` exactly as today (seed from `seq_block`, reserve `+SEQ_BLOCK_SIZE`, save).
2. **Durable set** = `config.domains` (source `Config`) ∪ `state.persistent_domains` (source
   `Persistent`). On a name collision **config wins**; the persistent entry is dropped +
   pruned from `state.json` with a WARN (config.json is authoritative user intent).
3. For each durable non-default domain, build it like `spawn_ephemeral_domain` **but durable**:
   seed its `SeqCounter` from `domain_seq_blocks[name]` (absent → 0), reserve `+SEQ_BLOCK_SIZE`,
   bind receivers at its declared ports, insert with the right `source`. **A bind failure
   (port clash) SKIPS that domain with a loud WARN — the daemon keeps running** (default +
   siblings stay up), consistent with the §9.3/finding-C resilience principle (an optional
   domain must not take the daemon down). Empty buffers (buffers never persist — same as
   `default`).
4. Persist the reserved seq blocks for all durable domains immediately (mirrors default's
   reserve-and-save, so a crash can't reuse seqs).

### 17.3 Shutdown + snapshot/restore (domain-routed bookmarks)

- **Seq:** for each durable domain persist `domain_seq_blocks[name] = reserved.max(counter.current())`
  (default → `seq_block`, unchanged).
- **`snapshot_named_for_persistence`** (today takes ONE store): change to take the durable-domain
  set; snapshot each domain's `BookmarkStore`, tag each bookmark with its `domain`, bucket by
  session into `named_sessions[].bookmarks`.
- **`restore_named`** (today takes ONE store): route each persisted bookmark to
  `domains.get(&pb.domain)`'s store via `insert_persisted`; a bookmark whose domain no longer
  exists at boot is skipped with a WARN (its domain is gone → its bookmarks go with it,
  matching the delete-drops-bookmarks model). Ephemeral domains don't exist at boot, so their
  bookmarks (if any leaked into state) are dropped — ephemeral was never durable.

### 17.4 Lifecycle (`persist=true` stops being rejected)

- `domains.create(persist=true)`: create the domain durable, append its concrete decl to
  `state.persistent_domains`, `save_state`. **Promote-in-place:** if an *ephemeral* domain of
  that name exists, flip its `source` to `Persistent` + record its decl (no teardown — receivers
  and live buffers/seq stay). Idempotent: `persist=true` on an existing persistent domain with
  matching ports → no-op. `persist=true` on a `config` domain → error (edit config.json).
- `domains.delete` on a `Persistent` domain: remove from registry (Arc-graceful, as today) AND
  from `state.persistent_domains` + `save_state`. `Config` domains still refuse delete (unchanged).

### 17.5 Migration proof (zero)

Old `state.json` = `{seq_block, named_sessions:{...bookmarks without domain}}` →
`domain_seq_blocks={}`, `persistent_domains=[]`, each bookmark `domain="default"`. Old
`config.json` (no `domains`) → `domains=[]`. Result: **default-only world, byte-identical
behavior.** Covered by a dedicated old-state load test.

### 17.6 Build phases (each failing-test-first; T3)

1. **Schema types** — the four `serde(default)` additions + `default_domain_str`. Test: an
   old-shape `state.json`/`config.json` loads to the default-only world (migration proof).
2. **Config-declared domains at boot** — load `config.domains`, build durable domains with
   per-domain seq seed/reserve/persist + skip-on-clash. Tests: a config domain boots with its
   ports + isolated ingest; its seq high-water survives restart (durable-restart seq test);
   a clashing config domain is skipped, daemon stays up.
3. **Domain-routed bookmarks** — snapshot/restore refactor. Tests: a bookmark made in a config
   domain survives restart and resolves (b>= matches post-restart ingest); a bookmark whose
   domain vanished is dropped; default bookmarks still round-trip unchanged.
4. **Persistent domains** — `domains.create(persist=true)` + promote-in-place + delete-removes.
   Tests: create persist=true survives restart; promote an ephemeral in place; delete a
   persistent removes it from `state.json` (gone after restart); persist=true on config errors.

### 17.7 Open decisions surfaced for the design gate

- **Config-wins on name collision** (vs. error) — chosen; gate to confirm it can't silently
  mask a user typo.
- **Skip-on-clash keeps the daemon up** (vs. fail-loud) — chosen for resilience parity with
  finding C; gate to confirm a skipped domain degrades cleanly (no half-created state, clear
  WARN, `domains.list` reflects absence).
- **`default` seq stays top-level** (vs. unify into the map) — chosen for zero-migration; gate
  to confirm the special-case doesn't desync the two seq paths.

### 17.8 Design-gate outcome (2026-07-15) — Option A needs rework or rescoping

Two fresh-context reviewers (buildability = Sonnet, soundness = Opus) reviewed §17 BEFORE
any code. **The design is NOT sound/buildable as sketched** — the "simple additive" framing
understated it. Convergent findings are load-bearing:

- **[HIGH · convergent] Rebuild-from-live persistence is wrong for durable domains.** Shutdown
  rebuilds `state.json` fresh from LIVE objects (`server.rs:497`, `session.rs:876`). A durable
  domain SKIPPED at boot (port clash, §17.2 step 3) is absent from the live set, so an ordinary
  graceful shutdown **erases** its persisted seq high-water + bookmarks (+ its `persistent_domains`
  decl) — one transient port clash → permanent data loss + seq rewind. Fix: shutdown must **merge**
  (carry forward declared-but-not-live domains' loaded state), not rebuild.
- **[HIGH · convergent] Restore ordering.** `restore_named` runs at `server.rs:227`, BEFORE any
  `Domain`/registry exists (built at `:357+`). Domain-routing bookmarks there drops 100% of them
  (every lookup misses). Fix: split restore — triggers/filters early (unchanged), bookmarks LATE
  (after all durable domains are inserted).
- **[HIGH] Runtime `persist=true` writes stale state.** `RpcHandler` has no state handle / no
  `save_state` today; §17.4's runtime write persists a stale boot-snapshot seq + bookmarks → data
  loss on a later crash. Fix: a **separate decl file** (§5's original intent — resolves the
  §5↔§17.1 contradiction) or re-snapshot live state under a lock.
- **[MED] Boot binds receivers before persisting the seq reservation** (§17.2 reuses
  `spawn_ephemeral_domain`'s bind-first shape) → seq-reuse window (masked today only because
  buffers never persist). Fix: reserve+persist before bind.
- **[MED] A declared domain named `default` isn't rejected** → could overwrite the real default
  (seq rewind + bookmark loss via `insert`-replaces-by-key). Fix: reject at config/state load.
- **[MED] Config-domain ports must be explicit** — omitted → auto-allocate re-randomizes every
  restart, defeating a durable domain's fixed-port purpose.
- **[BUILDABLE] Promote-in-place needs `RwLock<DomainSource>`** (source is immutable behind the
  registry `Arc<Domain>`; ~3 read + 7 construct sites). `spawn_ephemeral_domain` also needs
  seq + source params. `save_state` needs a lock (runtime vs shutdown race).
- **[LOW] Config-wins prune** must remove only the decl (config domain inherits seq/bookmarks);
  **downgrade round-trip** (old binary rewrites old-shape state.json) drops the new fields — note
  as a known limit.

**Key signal:** nearly every HIGH/MED hazard lives in the **seq + bookmark durability** — the part
with **no current consumer** (dev-tracks are ephemeral). Config-declared domains *without*
seq/bookmark durability (declarations-only, explicit ports, reject `default`) sidesteps the
rebuild-vs-merge data loss, the restore re-ordering, the runtime-write staleness, and the seq-window
entirely. **Scope re-decision surfaced to the user before rework/build.**
