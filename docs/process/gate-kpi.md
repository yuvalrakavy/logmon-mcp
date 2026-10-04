# Gate-escape KPIs

Two numbers per feature, both driven DOWN, both severity-weighted so a silent
correctness defect never scores like a comment nit. **DG** — the design was
wrong, caught at the pre-implementation gate. **IG** — the code was wrong,
caught at the pre-merge deep gate. **SG** — a tagged subset of IG: the design
was RIGHT and the implementation silently did not deliver it.

Weights: **S3 = 10** (would have shipped a silent defect — wrong result, data
divergence, corruption, or a test plan that cannot detect the defect it exists
for), **S2 = 3** (a real defect that would have been caught loudly, a required
test not delivered, or a false claim in a doc), **S1 = 1** (precision: a comment
overselling the code, citation drift, a latent footgun), **S0 = 0** (reported
and traced FALSE — recorded because it measures gate noise, not the work).

**These are diagnostics, never a score to defend.** They are read against
**post-merge**, which is not under the gates' control: DG and IG falling while
post-merge holds at zero is real improvement; either falling while post-merge
rises means the gates got quieter, not the work better. Never narrow a brief,
shorten a lens set, or drop a finder to move these numbers.

`loop` records how many architect/reviewer rounds the design took and whether
the verification pass still found anything — **DG=0 with a recorded loop is the
process working, and stays distinguishable from DG not run**, which is the
process skipped.

---

## Ledger

| date | feature | tier | DG | IG | SG | post-merge | loop | cost |
|---|---|---|---|---|---|---|---|---|
| 2026-08-01/02 | case documents (`cases.create`, epoch log) | T2 | *not recorded* | 98 | — | 0 | — | — |
| 2026-08-02 | `_display` — daemon-supplied presentation | T2 | 46 | 100 | — | **1** | 2 | — |
| 2026-08-02/03 | daemon-served skill | T2 | — | *low, not weighted* | — | 0 | 1 | — |
| 2026-08-02/03 | `logs.fields` | T2 | *skipped — see note* | ≈70 | — | 0 | 1 | — |
| 2026-08-03 | `logs.profile` | T2 | ≈90 | ≈84 | ≈6 | 0 | 2 | ~1.6M |
| 2026-10-03/04 | seq-ordered log ring (+ `traces.logs` perf) | T2 | *not recorded* | ≈105 | ≈13 | *pending* | 2 | ~3.1M |
| 2026-10-04 | open-issue batch: cursor late records (#23) + #21 #24 #25 #27 #28 #29 | T2 (#23) / T1 | ≈38 | ≈62 + 9 re-gates (see note) | ≈1 | *pending* | 2 | ~2.5M + ~6.5M re-gates |

**Seeded 2026-08-03 from `retro-log.md` entries.** Rows before `logs.profile`
are reconstructed from those entries and are marked where a number cannot be
defended — the case-documents design gate ran before this ledger existed, and
"DG 0" there would claim the process worked when it means it was not measured.

### Notes on the rows

- **open-issue batch (#23 + six T1 fixes)**: DG ≈38 is the one fresh-eyes pass on the cursor
  spec — two S3 (its main verification test passed against the bug it was for, because the
  cursor had not read past the record before it was stored late; and the store decided the
  commit before the handler truncated, losing a record), plus four S2. IG ≈62 is three lenses
  on the frozen diff. Its one S3 was a DESIGN miss the design gate also missed — a "from now"
  cursor took records that arrived BEFORE it, because the spec's sufficiency argument covered
  records the cursor had read past and never records it had never had; the fix is a creation
  floor. Two lenses converged independently on six findings; all six were real. The mutation
  lens added ~28 of test-plan gaps (14 proved by probes). SG ≈1: the dropped-commit guard.
  One control run hung on a test-client call that the daemon never answered; 50 reruns did
  not reproduce it. Explained later by a debugger attach on the stuck process: it was running
  the G12 MUTATION, which removed a cursor refusal and so tripped a `debug_assert!` in the
  trace-query path; the panic killed the connection task, and the client waited on the closed
  connection forever. The reruns ran unmutated code, which is why they never reproduced it.
  The client-side flaw was real, and was also in the SDK the MCP shim uses — fixed there in
  #33. Lesson kept: a control run executes deliberately broken code, so a hang inside one is
  evidence about the harness around it, not about production — find out which mutation was
  applied before diagnosing.
- **open-issue batch, the re-gates**: IG ≈62 is the first gate only. Nine re-gates of the fix
  sets followed (two lenses each, the last a single lens on the final delta), and rounds 1-7
  each found real defects, the worst of a round usually one the PREVIOUS round's fix had
  introduced — an anonymous session's bookmarks wiped by a drop keyed on its id; a panic in
  name-keyed cleanup poisoning the session lock; file I/O under the session lock; a
  check-then-unlink race on collector files; a write stamped when it finished, not when it
  found its collector; a 128-connection GELF TCP cap that 128 idle sockets locked out; oversize
  drops merged into a counter whose protocol doc forbids it; a snapshot reported filed after
  its collector was removed. Rounds 8 and 9 found no high defect and no medium one in
  production code — test strength, claims in my own commit messages, and adjacent pre-existing
  defects (fixed). The per-round severities were not tallied as the rounds ran, so the
  re-gates are not weighted here: recorded as nine non-empty rounds rather than a number that
  would claim a measurement not taken. Cost: rounds 7-9 measured at ≈0.70M / 0.73M / 0.72M
  subagent tokens; rounds 1-6 estimated at the same rate. The re-gates cost more than twice
  the first gate — see retro-log.md for what that says about building mechanisms under review.
- **seq-ordered log ring**: IG counts the deep gate plus the re-gates of its fixes,
  because two of the re-gates' worst findings were defects the FIXES introduced — an
  S3 (a clear raised the loss floor to the counter, so spans' seqs read as lost logs)
  and an S2 regression (a shortfall recount that turned a span eviction into a false
  "nothing more at that end"). A gate that scored only the original diff would hide
  exactly the escape the re-gate exists for. DG is not recorded — no design-gate tally
  survives for this spec, and "0" would claim a measurement that was not taken.
- **`_display`** is the only **post-merge = 1** in the window: `cargo install`
  ignores `Cargo.lock` without `--locked`, so the tagged release failed to build
  for a user on a commit where the whole suite and clippy were green. No test
  can catch it by construction — the suite builds *from* the lockfile. Running
  the documented install command is now a release step.
- **`logs.fields`** records a **skipped design gate** rather than DG=0. I
  recommended skipping it because the spec looked small; the deep gate then
  found design-level defects, and the re-gate found the headline fix had
  *displaced* its defect rather than removed it. That recommendation is the
  calibration this row exists to preserve.
- **`logs.profile`** is the first row with a `cost` figure: ~1.6M subagent
  tokens across seven agents (4 design lenses, mutation, 2 readers, 1 re-gate).
  Quality per unit cost is a ratio and only the numerator was being measured, so
  every "make the gate cheaper" proposal was unfalsifiable. Record it going
  forward.

### What the window says

DG did not fall. Self-review catches rose across the window (1 → 5 → 3 → 8) and
probe catches with them (2 → 3 → 2 → 6), so defects are being caught earlier —
but not yet earlier *enough*, and **DG is the leading indicator**: a design
defect caught at the deep gate has already been built. IG holding flat while DG
falls would mean defects moving downstream rather than disappearing; neither has
fallen yet, so there is nothing to celebrate and nothing to relax.

The one unambiguous positive: **post-merge = 1 across five features**, and that
one was structurally invisible to the suite.
