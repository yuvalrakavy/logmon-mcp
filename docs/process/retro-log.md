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
