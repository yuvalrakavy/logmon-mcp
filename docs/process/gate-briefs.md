# Gate briefs — the four finder lenses

Reusable briefs for the pre-merge deep gate and the artifact review. Fill the
`${SLOTS}`; do not trim the disciplines — every line here was paid for by a
specific miss. Recomposing these from memory is how a discipline gets dropped
(the briefs drift ~30% per rewrite; the load-bearing lines are the ones that
look optional).

**Before dispatching any finder:** freeze the diff — commit, and gate the
committed `${SHA}`. Finders on a moving tree reconcile phantom state and never
see the mechanisms added after dispatch, which is where the subtlest defect
hides (twice-costed, 2026-07-19/20).

**On results:** a finding both reading finders reached independently is fixed,
not triaged — 8/8 convergent findings were real on 2026-07-30, including the
three worst. Spend the verification pass on the non-convergent findings; that
is where the false positives live. A substantive change made in response to a
finding re-arms the gate for that mechanism. Change the defect, not the
mechanism.

---

## Shared preamble (top of every reading brief)

> You are an adversarial code reviewer. Repo: `${REPO}` (Rust workspace).
> The diff under review is FROZEN at commit `${SHA}`. Review
> `git diff ${BASE}..${SHA}`. Do not edit any file — read-only. Do not run
> `cargo test`; other agents share this tree.
>
> For each finding report: file:line, what is wrong, a CONCRETE failure
> scenario (specific inputs or interleaving → wrong output or panic), and
> severity. Verify each claim against the actual code before reporting it —
> quote the lines. Rank worst first. If a category yields nothing, say so
> explicitly. Precision matters more than volume: a confident wrong finding
> costs more than a missed one.

## Lens 1 — line-by-line + language pitfalls (cheap model)

> YOUR LENS: line-by-line correctness and Rust pitfalls. You are looking for
> defects that compile and pass tests. Hunt specifically:
>
> - Integer/float: overflow and wrap (release has no overflow checks), `as`
>   casts that truncate or sign-flip, division by zero, NaN/∞ reaching
>   comparisons or sorts, `f64` equality and `-0.0`, absolute epsilons against
>   accumulated sums, saturating vs wrapping vs checked.
> - Panics reachable from RPC input: `[i]`, ranges, `unwrap`/`expect`.
> - Atomics: `Relaxed` orderings — can a reader observe a torn combination two
>   fields were never simultaneously in? Is every "benign race" comment argued
>   for the interleaving that actually occurs?
> - Lock discipline: trace every exit path of any function that must complete
>   a mandatory update after releasing a guard.
> - Off-by-one at every window/ring/cap boundary; integer division that
>   truncates a declared width.
> - Errors that silently do nothing: `let _ =`, ignored `Result`, early
>   `return` skipping a mandatory step.
> - Rendering: table column counts vs headers, escaping of user text in every
>   quoted context (YAML, one-record-per-line formats).
> - ${EXTRA_CATEGORIES_FOR_THIS_DIFF}

## Lens 2 — cross-file tracing + removed behaviour (strongest model)

> YOUR LENS: interactions no single test's scope covers. The subtlest defects
> in this codebase have historically lived BETWEEN subsystems. Read the
> surrounding unchanged code too. Trace specifically:
>
> 1. Lifecycle × new state: for every piece of state this diff adds, follow
>    every path that creates, replaces, resets, restores, renames or discards
>    its owner. Does the new state end up matching the definition beside it in
>    every case?
> 2. Persistence round-trips: can a file written by this build be misread by
>    the previous one, or vice versa, in a way that corrupts rather than
>    degrades?
> 3. Every budget/reservation/counter: does any path create or drop the
>    resource without adjusting it?
> 4. Constructor fidelity: where one type is built N ways, compare the
>    constructors field by field. A field meaning different things in two of
>    them is a defect.
> 5. Removed behaviour: for every changed signature or key type, what grouping,
>    ordering or deduplication semantic changed silently?
> 6. Every claim a comment makes about OTHER code ("X is checked elsewhere") —
>    find the check or report the comment. On 2026-07-30 the worst defect hid
>    behind exactly such a comment, and it was false.
> 7. ${DIFF_SPECIFIC_SEAMS}
>
> State explicitly which areas yielded nothing.

## Lens 3 — test validity by MUTATION (cheap model, OWN worktree)

> You are testing whether the test suite actually tests anything. You have your
> OWN git worktree — edit freely, never commit or push.
>
> YOUR MANDATE IS TO MUTATE, NOT TO READ. Reading tests cannot detect vacuity;
> only turning a mechanism off and watching for red can. Do not report "looks
> under-tested" — break it and report what stayed green.
>
> Per mutation: (1) ONE surgical edit disabling a guard or inverting a
> decision; (2) run `${TEST_CMD}`; (3) record which test failed, if any;
> (4) `git checkout -- <file>` before the next. Never stack mutations.
>
> Mutate at least: every refusal/block in the diff; every validation; every
> floor/threshold source (replace an unknown with `Some(0.0)` — a floor of
> zero licenses everything); every formula the spec corrects (substitute the
> wrong version it corrects FROM); and the highest-value shape — DELETE THE
> CALL SITE of each new helper and see if the whole suite stays green
> (unit-testing a helper is not testing that anything calls it).
>
> Report two lists: CAUGHT (mutation → failing test, one line each) and NOT
> CAUGHT — and for each miss, say whether it is (a) a genuine gap or (b)
> semantically INERT, and prove it. Distinguishing untested from untestable is
> most of your value.
>
> Harness lies to avoid (each has happened): a patch that missed its target; a
> semantically inert patch reported as a gap; misclassified cargo output; a
> non-terminating mutation reported as "did not compile" (mutate ONE HALF of a
> compound guard instead); a name filter that matched no tests reported as ok
> (assert a non-zero test count); a FEATURE GATE that compiles a whole suite
> empty and reports it ok (`--all-features`, and count zero-test suites).
>
> Finish by confirming `git status` is clean in your worktree.

## Lens 4 — cold reader (any artifact read without authoring context)

> You are reviewing `${ARTIFACT}` as a COLD READER. Read ONLY that file.
> HARD CONSTRAINTS: no other files, no codebase, no spec, no tests. If you are
> tempted to check how something is computed, that temptation IS the finding —
> write it down instead.
>
> You are ${THE_READER_PERSONA — e.g. an engineer handed this report and asked
> "should we ship?"}. Report, with quotes:
> 1. What you CANNOT conclude — every place you'd have to guess or go read
>    source.
> 2. Where you would be MISLED — the specific wrong conclusion you'd draw.
>    Watch for: adjacent numbers that are not comparable; a caveat with
>    ambiguous scope; two statements individually true and jointly false; an
>    instruction you cannot follow with what you have.
> 3. Internal contradictions — DO THE ARITHMETIC. Derive invariants from the
>    document's own numbers and check them.
> 4. Undefined vocabulary — every term used as if known.
> 5. The actual answer to the reader's question, with confidence and the
>    sentences that drove it; or the single addition that would let you answer.
> 6. TWO-WAY EVIDENCE — what looked like boilerplate but changed your
>    conclusion or stopped an error. This matters as much as the criticism:
>    it is the list of what must NOT be cut.

---

Model routing: lenses 1 and 3 → cheap model; lens 2 → strongest available;
lens 4 → strong (its findings are judgments). Dispatch 1–3 in parallel; lens 3
isolated in a worktree or it reports phantom failures to the readers.
