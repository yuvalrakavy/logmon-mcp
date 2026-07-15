# Process retro log

Micro-retro entries appended during the finishing beat of T1+ tasks (see the
`ways-of-working` skill). Consolidated by `/process-retro` every ~5 entries.

Entry template:

```markdown
## YYYY-MM-DD <feature> (T<n>)
- time: <rough split: design / implement / test / review / friction>
- catches: probe=<n> self-review=<n> gate=<n> user=<n> post-merge=<n> — one line on the big ones
- friction: <items, with rough cost; mark >15-min items GUARDED/UNGUARDED>
- tier-call: <right / should-have-been-T<n>, why>
- delegation: <what went to which model + outcome, or "none">
- improve: <one candidate improvement, or "none">
```

---

## 2026-07-15 Wave 2 domains — deep gate (core) + stage 2.4 surfacing + surfacing gate (T3)
- time: mostly review + implement; design was pre-session (spec already gated). Two full gates + remediation dominated.
- catches: gate=6 self-review=1 user=1 post-merge=0.
  - Deep gate (core, steps 4–6): `domains.create` TOCTOU (**3-finder convergent**), unclamped-buffer→process-abort, OTLP boot-fail regression, §5 anonymous cross-domain bookmark cleanup (**4-finder convergent**), dispatch-arm nit.
  - Surfacing gate (2.4): a **CRITICAL** the author's own design narrative asserted was impossible — `--domain` stuck on the persistent named "cli" session. Fresh-context finder traced the session-persistence path and broke it in two invocations.
  - self-review caught the delete-while-bound event-channel edge during phase self-review.
  - user: idempotent same-explicit-ports re-create for stateless dev-tracks (already implemented; test gap closed).
- friction: concurrent-connect test flake under full-suite load (~15 min: root-caused to 8 simultaneous `TestClient::connect` stressing the accept loop; fixed by connect-then-race — GUARDED). Residual rare full-suite flake is the pre-existing ingest-timing pattern (`wait_log_count` 2s budget under parallel load; 20/20 clean in isolation — UNGUARDED, pre-existing, noted in cli_common).
- tier-call: right — core engine + data-integrity + a session-state migration surface.
- delegation: deep gate = 3× Sonnet breadth (found the TOCTOU + the CRITICAL buffer-abort) + 1× Opus depth (verified teardown Arc-clean + blast-radius, no strong-ref cycle). Surfacing gate = 2× Sonnet (one found the `--domain` CRITICAL). Every finding re-verified against the code by the orchestrator before fixing; all fixes failing-test-first. Outcome: strong — convergence flagged the load-bearing ones, and the gate caught what author self-review could not.
- improve: **do not skip the lighter gate on "mechanical" surfacing** — the `--domain` CRITICAL was invisible to the author (whose docs literally claimed the opposite) yet obvious to a fresh context tracing persistence. Candidate red-flag: when a new CLI/UI flag maps to session-scoped server state, write a one-line "who persists this, and when is it reset?" table BEFORE wiring it (the named-session persistence was the whole trap).

## 2026-07-15 Durability: design gate → rescope → config-declared domains (T3 design / T2 build)
- time: mostly design + the design gate; the build was small + pattern-following.
- catches: gate=~10 (**design** gate, pre-code). Two fresh reviewers found full Option A NOT sound/buildable — two HIGH data-loss defects (rebuild-from-live shutdown erases boot-skipped durable domains; restore runs before domains exist → 100% bookmark drop), convergent. More valuable than the defect list: the gate exposed that the complexity concentrates in the **consumer-less** seq/bookmark part → rescoped to declarations-only before writing a line of data-integrity code.
- friction: none material — gate ran clean; the rescope was one clean decision.
- tier-call: right to treat the persisted-schema change as T3 and gate the DESIGN; the rescoped config-domains build is T2 (mints a config.json contract, otherwise pattern-following the existing domain build).
- delegation: design gate = Sonnet (buildability) + Opus (soundness); both converged on the load-bearing holes (restore-ordering, rebuild-vs-merge). The author's own §17 narrative asserted the opposite of what soundness found — same author-blind-spot pattern as the `--domain` surfacing catch.
- improve: the design gate's highest value wasn't the 10 defects — it was exposing that the FRAMING ("simple additive, zero-migration") was wrong, which flipped the build-vs-defer decision. Lesson/red-flag: **gate the DESIGN of a persisted-schema change before committing to scope**, and feed the gate's complexity findings back into the YAGNI call — a design needing ~10 fixes on the data-integrity path for a feature with no consumer is itself the signal to descope, not to grind through the fixes.

## 2026-07-15 Consumer feedback #1–#5 (connect-time domain binding, liveness, beacon/OTLP-guard/lifecycle) (T2)
- time: implement-heavy; the 5 items were mostly additive. Design collapsed to a merits-assessment of the consumer's own 5 asks (pushed back on #3's premise, accepted the rest). One deep gate + doc/schema remediation.
- catches: gate=5 self-review/test=1 user=0 post-merge=0.
  - Deep gate: **schema drift** — the new `domains.*` types were never registered in `xtask gen-schema`, so the shipped `protocol-v1.schema.json` omitted the whole surface (the `verify-schema` guard would fail CI but was green locally because the types compiled); "durable"/"survives restart" **overstatement** in README/handoff corrected to the *named-session fail-loud* contract (anonymous → `SessionLost`, never silent `default`); spec's port stride wrong (`+N` → `+2N`, the #4 collision); `stale_after_secs` missing from README; #4 guard generalized to reject **any two** of a domain's own ports coinciding (not just the two I first thought of).
  - test/self-review: `reconnect_preserves_bound_domain` initially failed on an anonymous session — which surfaced that reconnect-preservation *is* the named-session contract; fixed the test to `.session_name(...)` and documented the requirement (the failure was the design telling me the contract).
- friction: the deep-gate **reconnect** finder (dispatched Opus, the depth lens) returned **0 tool uses** and just echoed recalled memory content — a non-review. Fell back to verifying the reconnect state machine inline by reading `reconnect_loop` directly (~10 min). Recurring failure mode (seen before this session).
- tier-call: right at T2 — #1 changes a **binding contract** (the domain re-sent through the SDK reconnect handshake), so it took the contract lens; #2–#5 additive fields + docs, pattern-following.
- delegation: gate breadth = Sonnet (liveness finder — clean, useful); depth = Opus reconnect finder **failed structurally** (0 tool uses). escalate-on-signal has no "up" from the top model, so inline verification by the orchestrator was the correct fallback (briefing cost ≈ doing it).
- improve: **treat a finding-set produced with 0 tool calls as void, not as a review.** Candidate guard/red-flag: brief every fresh-context finder with an explicit "you MUST `Read`/`Grep` the diff and cite `file:line` before reporting — a report with no tool use is discarded," and have the orchestrator check the finder's tool-use count before trusting (or redoing) its verdict. This is the second time an Opus finder echoed memories instead of reading the diff.
