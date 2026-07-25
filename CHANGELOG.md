# Changelog

Notable changes per release. Versions are `0.x`, so the MINOR component carries
anything behaviour-visible; PATCH is reserved for fixes nobody has to know about.

## 0.3.0 — 2026-07-25

### Fixed

- **A trigger firing no longer blinds the other triggers in its session.** The
  post-window was a single counter on the session, and the log processor skipped
  evaluating that whole session while it was positive. For `post_window` entries
  (200 by default) after ANY trigger matched, NO trigger in that session was
  evaluated — a session-wide duty cycle of at most one firing per 200 entries.

  The effect was backwards from what you want: a frequently-matching trigger
  starved the quiet ones, and quiet triggers are exactly what you arm to catch
  something rare. Observed in practice with the built-in `l>=ERROR` trigger
  firing throughout a 70-minute test run while a trigger armed for a rare event
  recorded zero matches, with matching entries confirmed in the buffer.

  Firing suppression is now **per trigger**: each debounces only itself. The
  session-level counter is unchanged and still governs storage (capture context
  after a fire, bypassing filters).

- **A short `post_window` no longer truncates a longer one already in flight.**
  Now that a match can land inside an open window, the storage window extends
  rather than being overwritten.

- **`edit` applies to a window already in flight.** Shrinking `post_window` —
  notably to `0`, the documented way to ask for "count every match" — took
  effect only after the old window drained, which on a quiet stream could be
  indefinitely. Changing a trigger's *filter* now also re-arms it immediately.

- **Rebinding a session to another domain clears in-flight debounce windows**,
  completing the existing F3 invariant for firing as well as storage.

- **Span triggers now count their matches.** A span trigger's `match_count`
  stayed `0` forever, so "has this ever fired?" was unanswerable.

### Added

- **`post_remaining` on `TriggerInfo`** (`triggers.list` / `add` / `edit`):
  entries still to pass before a trigger can fire again; `0` means armed and
  live. If a trigger looks stuck, this distinguishes "debounced" from "broken".
  Additive and defaulted, so older clients are unaffected.

### Changed

- **Expect more notifications** in sessions with several triggers, and
  `match_count` to start moving on triggers that previously read `0` for their
  entire lifetime. Those fires were being silently dropped; the new numbers are
  the corrected ones, not a regression.

### Notes

- Span triggers still fire on **every** matching span and are never debounced,
  so their `post_remaining` is always `0` and carries no information. Unifying
  the log and span evaluation paths is deferred: span triggers always fire, so
  the gap costs extra notifications rather than silent misses.
- Documentation for `matched_filters` was corrected. It is populated only when
  `source == Filter`; an entry stored because a trigger fired (`PreTrigger` /
  `PostTrigger`) matched no filter, so an empty value there is correct. Read it
  together with `source`, never alone.
