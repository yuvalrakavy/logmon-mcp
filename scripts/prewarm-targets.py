"""Print the test harnesses `cargo test --no-run` just built, one path per line.

Reads cargo's `--message-format=json` on stdin.

**Filtering on `profile.test` is the whole point.** `cargo test --no-run` also
reports the ordinary binaries it built -- for this workspace, `logmon-broker`
and `logmon-mcp` -- and those are not libtest harnesses. Handing them `--list`
gets an argument error at best, and at worst starts something: one of them is a
daemon and the other reads a protocol off stdin.

That is not hypothetical. The first version of this took every `executable`
cargo reported, so it ran the broker and the shim too; BSD xargs aborts the
whole batch when a child exits 255, and the pre-warm silently cleared about half
the binaries and stopped.

    profile.test=True   kind=('test',)   62     <- integration tests
    profile.test=True   kind=('lib',)     3     <- unit tests
    profile.test=True   kind=('bin',)     3     <- bin unit tests
    profile.test=False  kind=('bin',)     2     <- logmon-broker, logmon-mcp
"""

import json
import sys

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    executable = msg.get("executable")
    if executable and msg.get("profile", {}).get("test"):
        print(executable)
