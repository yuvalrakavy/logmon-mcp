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

**Seeded 2026-08-03 from `retro-log.md` entries.** Rows before `logs.profile`
are reconstructed from those entries and are marked where a number cannot be
defended — the case-documents design gate ran before this ledger existed, and
"DG 0" there would claim the process worked when it means it was not measured.

### Notes on the rows

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
