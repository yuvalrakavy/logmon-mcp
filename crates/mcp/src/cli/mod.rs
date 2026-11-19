//! CLI mode for `logmon-mcp`. Routed when argv carries anything but flags.
//!
//! Commands use the daemon's manifest and the parser in [`generic`]. The
//! progress polling path also needs the shipped query schema so it can report
//! a failed connection before any daemon is available to describe the tool.
//!
//! ## Where a global flag stops being global
//!
//! `--json`, `--help` and `--version` are recognised anywhere: no tool declares
//! a parameter by those names today, checked by a test against the manifest —
//! **not an assumption, because the manifest is the daemon's to change.** The
//! day a tool gains a `json` parameter, that test fails and the choice becomes
//! explicit rather than a flag silently eaten here.
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
            // An unknown leading flag ends the global run. It cannot be
            // reported against a tool's parameter list — it PRECEDES the
            // command, so nothing has been matched yet and the caller reports
            // "no such command <whole argv>". Naming it here is the only place
            // that can say anything useful about it.
            other => {
                g.error = Some(format!(
                    "unknown option `--{other}` — global options are --session, \
                     --domain, --json, --help, --version, and they come before \
                     the command"
                ));
                break;
            }
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

    if !globals.help
        && argv
            .first()
            .is_some_and(|v| v.eq_ignore_ascii_case("test-log"))
    {
        let method = match argv.get(1).map(|v| v.to_ascii_lowercase()).as_deref() {
            Some("status") => Some("test_log.status"),
            Some("list") => Some("test_log.list"),
            _ => None,
        };
        if let Some(method) = method {
            return dispatch_progress(&globals, method, &argv[2..]).await;
        }
    }

    // A status-page read of a particular lane must not rebind another caller's
    // persistent session between its own domains.use and its subsequent RPC.
    let lane_status = globals.domain.is_some()
        && argv
            .first()
            .is_some_and(|a| a.eq_ignore_ascii_case("status"))
        && (argv.len() == 1 || argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case("get")));
    let session_name = (!lane_status).then(|| globals.session.as_deref().unwrap_or("cli"));

    let broker = match connect::connect_cli(session_name, &argv).await {
        Ok(b) => b,
        Err(e) => {
            format::error(&e.to_string(), json);
            return 1;
        }
    };

    // Help answers BEFORE anything is bound. Asking a question must not change
    // server state: `--domain t3 --help` used to rebind the persistent `cli`
    // session to t3 as a side effect of printing help.
    if globals.help {
        // It still needs the daemon — the commands are its to describe.
        let entries = match generic::fetch_manifest(&broker).await {
            Ok(e) => e,
            Err(e) => {
                format::error(&format!("could not read the tool manifest: {e}"), json);
                return 1;
            }
        };
        print!("{}", generic::help_for(&entries, &argv));
        return 0;
    }

    // Bind this invocation's domain BEFORE the command runs — to `--domain` if
    // given, else back to `default`. The CLI connects with a persistent NAMED
    // session ("cli"), so this reset is load-bearing: without it a prior
    // `--domain X` would stick server-side and silently scope later UNFLAGGED
    // invocations to X.
    //
    // Registry-management verbs skip the bind, because binding to a domain you
    // are about to create cannot succeed.
    //
    // **This is a hand-kept list.** An earlier comment here claimed the opposite while
    // sitting directly above the literals. It cannot be derived today: nothing
    // in the manifest says which verbs are domain-agnostic. The honest fix is a
    // manifest hint (`cli.domain_agnostic`), which belongs with the daemon work
    // rather than smuggled in under a comment that denies the problem.
    let skip_bind = argv.first().map(String::as_str) == Some("domains")
        && matches!(
            argv.get(1).map(String::as_str),
            Some("create") | Some("resize") | Some("delete") | Some("list")
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

    generic::dispatch(&broker, &argv, json).await
}

/// Polling passes the domain in the RPC. Binding the shared named CLI session
/// would race two concurrent lane reads. Parse these two commands with the
/// shipped schema so a cold connection failure can still identify the request.
async fn dispatch_progress(globals: &Globals, method: &str, args: &[String]) -> i32 {
    use crate::progress;
    use logmon_broker_protocol::mcp_tools;
    use logmon_broker_sdk::Broker;
    use serde_json::Value;

    let entries = mcp_tools::manifest();
    let Some(entry) = entries.iter().find(|e| e.method == method) else {
        format::error(
            "This build does not support test-log progress queries",
            globals.json,
        );
        return 1;
    };
    let mut args = args.to_vec();
    if let Some(domain) = &globals.domain {
        args.extend(["--domain".to_string(), domain.clone()]);
    }
    let mut params = match generic::build_params(&args, entry) {
        Ok(p) => p,
        Err(e) => {
            format::error(&e, globals.json);
            return 1;
        }
    };
    params
        .entry("domain")
        .or_insert_with(|| Value::from("default"));
    let params = Value::Object(params);
    let connect = Broker::connect()
        .reconnect_max_attempts(0)
        .call_timeout(progress::QUERY_TIMEOUT)
        .open();
    let reply = match tokio::time::timeout(progress::QUERY_TIMEOUT, connect).await {
        Ok(Ok(broker)) => progress::query(&broker, method, params).await,
        Ok(Err(e)) => progress::error(&params, "broker_unreachable", e),
        Err(_) => progress::error(
            &params,
            "broker_unreachable",
            "Broker connection timed out.",
        ),
    };
    let failed = reply.get("error").is_some();
    if !globals.json && failed {
        eprintln!(
            "{}",
            reply["error"]["message"].as_str().unwrap_or("Broker error")
        );
    } else {
        format::print_json(&reply);
    }
    i32::from(failed)
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

    /// The always-global flags are stripped from anywhere in argv, so a tool
    /// declaring a parameter by one of those names would have it silently
    /// eaten. That is a claim about the DAEMON's manifest, which the daemon may
    /// change without rebuilding this binary — so it is checked, not assumed.
    ///
    /// `--session` and `--domain` are deliberately absent from this list:
    /// `bookmarks.list` and `collectors.edit` do declare those, which is why
    /// position decides who owns them.
    #[test]
    fn no_tool_declares_a_parameter_named_like_an_always_global_flag() {
        let clashing: Vec<String> = logmon_broker_protocol::mcp_tools::manifest()
            .iter()
            .filter_map(|e| e.input_schema.as_ref().map(|s| (e, s)))
            .flat_map(|(e, s)| {
                s.get("properties")
                    .and_then(|p| p.as_object())
                    .map(|p| {
                        p.keys()
                            .filter(|k| matches!(k.as_str(), "json" | "help" | "version"))
                            .map(|k| format!("{}.{k}", e.name))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            clashing.is_empty(),
            "these would be swallowed as global flags and never reach the tool: \
             {clashing:?} — either rename the parameter or make that flag \
             position-scoped like --session and --domain"
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
