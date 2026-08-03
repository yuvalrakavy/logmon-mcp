# The docs surface — what "done" means for a user-visible change

Enumerated once, from the primary source (`ls README.md CHANGELOG.md
crates/*/README.md skill/logmon.md docs/medium-article.md`), and walked before
any feature is called done.

**Walk the list, not the diff.** A completeness check that starts from "files I
edited" covers the code surface and stops there — which is how 0.9.0 shipped
believing itself finished with its entire user-facing story in the CHANGELOG,
and how `logs.profile` updated skill, README and article while missing the
CHANGELOG and leaving a tool count stale by two. Neither omission is visible
from a diff; both are obvious from a list.

| Surface | Who reads it | What a feature owes it |
|---|---|---|
| `README.md` | someone deciding whether to use logmon | a tool-table row, and any framing the feature changes (the three-questions table, the tool COUNT) |
| `skill/logmon.md` | **an agent, at the moment of use** | when to reach for it, the call shape, and the traps — `include_str!`'d into the broker, so editing it changes a binary |
| `CHANGELOG.md` | someone upgrading | an `Unreleased → Added/Fixed` section saying what changed and why it matters |
| `docs/medium-article.md` | someone deciding whether the design is sound | only when the feature makes or changes an ARGUMENT. Not every feature does; say so rather than padding |
| `crates/mcp/README.md` | someone embedding the shim | shim-visible behaviour: CLI grammar, MCP surface, env |
| `crates/sdk/README.md` | someone writing a client | anything the SDK exposes or should |

## Checks that are mechanical, and therefore should not be skipped

- **The tool count in `README.md`** must equal `mcp_tools::TOOLS.len()`. It has
  been stale twice; the pin lives at `crates/protocol/src/mcp_tools.rs`.
- **Every command and rendering pasted into a doc was EXECUTED**, not
  reconstructed. Run an isolated broker — `LOGMON_CONFIG_DIR` plus a non-default
  port — so the live one is untouched, and paste that run's output.
- **The skill ships from the daemon.** Promote it before `cargo install`, or the
  binary carries the old instructions with no error.

## Not on this list, deliberately

`docs/superpowers/specs/` and `docs/process/` are working artifacts, not
surfaces a user reads. They are owed accuracy, not completeness.
