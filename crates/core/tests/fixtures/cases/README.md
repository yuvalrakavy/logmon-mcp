# Case fixtures — real bytes, not hand-written

Every file here was **produced by `cases.create` on a running broker**, not
constructed by hand. That is the point: the loader's premises are assertions
about what the writer emits, and a hand-written fixture encodes the author's
belief about the format rather than the format.

Captured 2026-08-03 during duty 0 of the case-loading design, from an isolated
broker (`LOGMON_CONFIG_DIR`, GELF 22201 / OTLP 24317 / 24318) fed synthetic
checkout traffic. Broker version 0.10.0, `logmon_format: 1`.

**Regenerate rather than edit.** If the format changes, produce new fixtures
from a broker at that version. Hand-editing a fixture to match a code change
destroys its only property — that a real writer really emitted it.

| Stem | What it is | Why it is here |
|---|---|---|
| `with-spans-260803-214501` | 9 logs, 3 spans, `verdict: complete` | The primary happy-path fixture. Contains a full trace: `POST /checkout` (30450ms, error), `inventory.reserve`, `payment.charge`. Exercises hex trace/span ids, adjacently-tagged `status`, and a non-empty `attributes` map |
| `checkout-hang-260803-214121` | 6 logs, **0 spans**, `verdict: complete` | The writer defect this design fixes. Anchored on the error log, it captured none of the three spans of the trace it is about — they sat at seqs 1007–1009, above the log-derived window, stored 95 seconds *before* the capture. Its document nonetheless says "seq 1006 was the newest record stored when the capture was taken ... nothing was lost here". Keep it: it is the regression evidence, and after the §4 fix a re-capture of the same traffic must not look like this |
| `nasty-260803-214626` | 3 logs, hostile `reason` | The escaping fixture. Its `reason` carries `"`, `\n`, `\t`, a BEL (`\x07`) and a U+2028 that entered the probe by accident and came out as `\L`. Any front-matter parser must round-trip this one |

## Notes for whoever writes tests against these

- **The round trip is lossless but NOT byte-identical.** `additional_fields` and
  `attributes` are `HashMap`s, so key order varies. Compare as
  `serde_json::Value`, never as strings.
- A span id that looks decimal is **silently reinterpreted as hex** —
  `from_str_radix(s, 16)` accepts `1234567890123456` and yields
  `0x1234567890123456`. A corrupt id is not always a parse error.
- Both `.spandata.jsonl` files with zero records are **38 bytes**: the header
  line and nothing else. Absent cannot be told from "we captured none", which is
  why the writer emits the file either way.
