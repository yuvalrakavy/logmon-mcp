# Meaningful session names — spec (2026-07-17, user-designed)

## Why

Two concurrent Claude conversations in one project share one project-scoped
MCP shim process → one broker session → each `use_domain` silently retargets
the other's reads. The `rebind_warning` (shipped `5324b5b`) *detects* the
collision; this feature makes sessions **individually named and meaningful**,
so `get_sessions` reads as an ops dashboard and the one-conversation-per-lane
invariant is *enforced* where it can be.

A Claude conversation's context = a git repo + optionally a dev-track lane +
branch. Session names encode exactly that.

## Naming scheme

- **No lane / t0 (home):** `<ProjectName>-Main-<short8>` — `short8` = 8 hex
  chars of a UUID, so two home-rooted conversations in the same project get
  DISTINCT sessions (this alone dissolves the original collision).
- **On a lane:** `<ProjectName>-t<N>-<BranchName>` — branch **sanitized**
  (`/` and any other invalid name chars → `-`, e.g.
  `Store-t1-feat-dev-track-ergonomics`).
- Assigned at shim spawn by the wrapper (see Store-side below) and **changed
  at lane claim** via the new rename call.

## Broker: `session.rename` (+ MCP tool `rename_session`)

Re-keys the calling session to a new name, preserving ALL state (triggers,
filters, bookmarks, domain binding, notification queue).

Conflict semantics (decision #1):
- Target name held by a **connected** session → **ERROR**: "dev-track already
  in use by another conversation" — this is the one-conversation-per-lane
  invariant firing, and the caller must STOP.
- Target name held by a **disconnected** session → displace it (dispose the
  stale session), rename proceeds. A dead conversation must not lock a lane
  name forever. (The broker already distinguishes these: `AlreadyConnected`
  vs `AlreadyExists`.)
- Anonymous → named transitions allowed (generality; with the wrapper naming
  at spawn, the common case is named → named).

## Broker: session TTL (decision #2)

Every session carries a TTL (config, default e.g. 24 h). A session that stays
**disconnected** longer than its TTL is disposed by a periodic sweep.
Connected sessions never expire. This is what keeps unique-per-conversation
`Main-<short8>` names from accumulating forever, and it retires existing
debris (`verify-test*`, `smoke`, …).

## `domains.list`: per-domain `bound_sessions` (decision #4, by derivation)

Each listed domain gains `bound_sessions: [session names/ids]`, derived from
the session registry's current bindings (optionally split
connected/disconnected). NO caller-supplied "usage" parameter — the broker
knows every binding authoritatively; a parameter could lie or go stale.
With meaningful names, `domains.list` answers "which conversation is using
which domain" directly.

`use_domain` semantics are otherwise unchanged; the `rebind_warning` stays.

## Store repo side

- `scripts/logmon-mcp-wrapper.sh`:
  - home path: `--session "<ProjectName>-Main-<short8>"` (project = worktree
    basename; short8 minted per exec). No longer anonymous.
  - lane path: `--session "<ProjectName>-t<N>-<branch>"` (branch read from
    the worktree; sanitized). Keeps `LOGMON_DOMAIN=tN`.
- `using-dev-tracks` skill: after `claim`/`EnterWorktree`, the conversation
  calls `rename_session` to `<Project>-t<N>-<branch>` **and treats the
  connected-conflict error as "this lane is already being worked by another
  conversation — stop."** On `release`, optionally rename back to a Main name
  (or let TTL handle it).

## Tests (two-flavor)

Verification: rename preserves state (bind + a trigger survive); Main-name
uniqueness (two connects, distinct names); disconnected-holder displacement;
`bound_sessions` in domains.list reflects binds and renames.
Adversarial: rename to a CONNECTED holder errors and changes nothing; TTL
sweep disposes only disconnected-past-TTL (a connected long-lived session
survives); rename mid-notification-stream loses nothing; invalid target names
rejected (and the sanitizer never produces one).

## Out of scope

Splitting the shared MCP process itself (Claude app behavior); renaming
DOMAINS (they stay `tN`, anchored to physical ports).
