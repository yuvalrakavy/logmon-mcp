//! CLI mode for `logmon-mcp`. Routed when argv carries anything but flags.
//!
//! **There are no per-command modules.** Every command is built from the
//! daemon's `tools.manifest` at runtime by [`generic`], so this file's whole
//! job is to separate the flags that belong to *this process* from the ones
//! that belong to the tool, open a connection, and hand over.
//!
//! ## Where a global flag stops being global
//!
//! `--json`, `--help` and `--version` are recognised anywhere: no tool declares
//! a parameter by those names, so there is nothing to collide with.
//!
//! `--session` and `--domain` are different — `bookmarks.list` takes a
//! `session` and `collectors.edit` takes a `domain`. They are global **only
//! before the command path**, and belong to the tool after it:
//!
//! ```text
//! logmon-mcp --domain t3 collectors edit --name c --domain default
//!            ^^^^^^^^^^^ binds this invocation   ^^^^^^^^^^^^^^^^ re-pins the collector
//! ```
//!
//! The alternative — resolving the ambiguity from the manifest — needs the
//! manifest, which needs a connection, which needs `--session`. Position is the
//! only signal available before the daemon has spoken.

pub mod connect;
pub mod format;
pub mod generic;

/// Flags that belong to this process rather than to a tool.
#[derive(Debug, Default)]
pub struct Globals {
    pub session: Option<String>,
    pub domain: Option<String>,
    pub json: bool,
    pub help: bool,
    pub version: bool,
    /// A malformed global flag, reported before anything connects.
    pub error: Option<String>,
}

/// Split argv into this process's flags and the command that follows.
///
/// Returns `(globals, command_argv)`. `command_argv` starts at the first bare
/// word — everything from there is the command path and the tool's own
/// arguments, untouched.
pub fn split_globals(args: &[String]) -> (Globals, Vec<String>) {
    let mut g = Globals::default();
    let mut i = 0;

    // Leading flags: everything is global here, because no command has started.
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with('-') {
            break;
        }
        let (name, inline) = match a.trim_start_matches('-').split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (a.trim_start_matches('-').to_string(), None),
        };
        match name.as_str() {
            "session" | "domain" => {
                // **Must not swallow the next flag.** `--session --json …`
                // otherwise connects with a persistent named session literally
                // called `--json`, and `--json` never takes effect. A missing
                // value at end-of-argv used to be silently absent too, which
                // bound `default` without a word.
                let value = match inline {
                    Some(v) => Some(v),
                    None => match args.get(i + 1) {
                        Some(v) if !v.starts_with('-') => {
                            i += 1;
                            Some(v.clone())
                        }
                        other => {
                            g.error = Some(match other {
                                Some(v) => format!(
                                    "`--{name}` was given no value — `{v}` looks like another flag"
                                ),
                                None => format!("`--{name}` was given no value"),
                            });
                            None
                        }
                    },
                };
                if name == "session" {
                    g.session = value;
                } else {
                    g.domain = value;
                }
            }
            "json" => g.json = true,
            "help" | "h" => g.help = true,
            "version" | "V" => g.version = true,
            // An unknown leading flag is left for the command, which reports it
            // against the tool's own parameter list — a better message than
            // anything this layer could produce.
            _ => break,
        }
        i += 1;
    }

    // From the first bare word on, only the collision-free globals are still
    // recognised; `--session` and `--domain` now belong to the tool.
    let mut rest = Vec::new();
    while i < args.len() {
        match args[i].as_str() {
            "--json" => g.json = true,
            "--help" | "-h" => g.help = true,
            "--version" | "-V" => g.version = true,
            other => rest.push(other.to_string()),
        }
        i += 1;
    }
    (g, rest)
}

/// Top-level CLI dispatch. Returns the process exit code.
pub async fn dispatch(globals: Globals, argv: Vec<String>) -> i32 {
    let json = globals.json;

    // Reported before connecting: a bad `--session` would otherwise open a
    // session under a nonsense name, and a bad `--domain` would bind `default`
    // silently — both side effects that outlive the failed command.
    if let Some(e) = &globals.error {
        format::error(e, json);
        return 1;
    }

    let session_name = globals.session.clone().unwrap_or_else(|| "cli".to_string());

    let broker = match connect::connect_cli(&session_name, &argv).await {
        Ok(b) => b,
        Err(e) => {
            format::error(&e.to_string(), json);
            return 1;
        }
    };

    // Bind this invocation's domain BEFORE the command runs — to `--domain` if
    // given, else back to `default`. The CLI connects with a persistent NAMED
    // session ("cli"), so this reset is load-bearing: without it a prior
    // `--domain X` would stick server-side and silently scope later UNFLAGGED
    // invocations to X.
    //
    // Registry-management verbs skip the bind, because binding to a domain you
    // are about to create cannot succeed. Read off the derived command path
    // rather than a hand-kept list of clap variants.
    let skip_bind = argv.first().map(String::as_str) == Some("domains")
        && matches!(
            argv.get(1).map(String::as_str),
            Some("create") | Some("delete") | Some("list")
        );
    if !skip_bind {
        let target = globals.domain.unwrap_or_else(|| "default".to_string());
        if let Err(e) = broker
            .domains_use(logmon_broker_protocol::DomainsUse { name: target })
            .await
        {
            format::error(&format!("--domain bind failed: {e}"), json);
            return 1;
        }
    }

    if globals.help {
        // Help still needs the daemon: the commands are its to describe.
        let entries = match generic::fetch_manifest(&broker).await {
            Ok(e) => e,
            Err(e) => {
                format::error(&format!("could not read the tool manifest: {e}"), json);
                return 1;
            }
        };
        match generic::match_command(&entries, &argv) {
            Some(m) => print!("{}", generic::help_command(m.entry)),
            None => print!("{}", generic::help_index(&entries)),
        }
        return 0;
    }

    generic::dispatch(&broker, &argv, json).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(args: &[&str]) -> (Globals, Vec<String>) {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        split_globals(&owned)
    }

    #[test]
    fn leading_flags_are_global() {
        let (g, rest) = split(&["--domain", "t3", "--json", "collectors", "list"]);
        assert_eq!(g.domain.as_deref(), Some("t3"));
        assert!(g.json);
        assert_eq!(rest, ["collectors", "list"]);
    }

    /// The collision that made position the rule: `collectors.edit` takes a
    /// `domain`, so a `--domain` after the command must reach the tool.
    #[test]
    fn domain_after_the_command_belongs_to_the_tool() {
        let (g, rest) = split(&[
            "--domain",
            "t3",
            "collectors",
            "edit",
            "--name",
            "c",
            "--domain",
            "default",
        ]);
        assert_eq!(
            g.domain.as_deref(),
            Some("t3"),
            "the leading one still binds the invocation"
        );
        assert_eq!(
            rest,
            ["collectors", "edit", "--name", "c", "--domain", "default"],
            "the trailing one must survive untouched"
        );
    }

    /// Same shape for `bookmarks.list --session`.
    #[test]
    fn session_after_the_command_belongs_to_the_tool() {
        let (g, rest) = split(&["bookmarks", "list", "--session", "other"]);
        assert!(g.session.is_none());
        assert_eq!(rest, ["bookmarks", "list", "--session", "other"]);
    }

    /// `--json` is habitually typed last and collides with nothing.
    #[test]
    fn json_is_recognised_anywhere() {
        assert!(split(&["collectors", "list", "--json"]).0.json);
        assert!(split(&["--json", "collectors", "list"]).0.json);
        assert_eq!(
            split(&["collectors", "list", "--json"]).1,
            ["collectors", "list"],
            "and is removed rather than forwarded"
        );
    }

    /// `--session --json collectors list` used to connect with a persistent
    /// named session literally called `--json`, and `--json` never took effect.
    /// A missing value at end-of-argv silently bound `default`.
    #[test]
    fn a_global_flag_does_not_swallow_the_next_flag() {
        let (g, _) = split(&["--session", "--json", "collectors", "list"]);
        assert!(g.session.is_none(), "must not be named `--json`");
        assert!(g.error.is_some(), "and must be reported: {:?}", g.error);
        assert!(g.error.unwrap().contains("--json"));

        let (g, _) = split(&["--domain"]);
        assert!(
            g.error.is_some(),
            "a trailing --domain is an error, not `default`"
        );
    }

    #[test]
    fn inline_values_are_accepted() {
        let (g, _) = split(&["--domain=t3", "--session=cli2", "status", "get"]);
        assert_eq!(g.domain.as_deref(), Some("t3"));
        assert_eq!(g.session.as_deref(), Some("cli2"));
    }

    #[test]
    fn help_is_seen_before_and_after_a_command() {
        assert!(split(&["--help"]).0.help);
        assert!(split(&["collectors", "add", "--help"]).0.help);
    }
}
