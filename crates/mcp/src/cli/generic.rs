//! The CLI, built from the daemon's manifest rather than from knowledge of it.
//!
//! **No tool name, parameter name, result shape or rendering appears in this
//! file.** Everything it needs arrives at runtime from `tools.manifest`: the
//! command paths, the arguments, their types, their accepted values, and the
//! few command-line facts a JSON Schema cannot carry. A daemon that gains a
//! tool gains a command; one that renames a parameter renames a flag; and
//! neither requires this binary to be rebuilt.
//!
//! ## Command paths are derived, never declared
//!
//! `collectors.list` becomes `logmon-mcp collectors list`. Derivation was
//! chosen over a manifest field on the grounds that **a declared path can be
//! declared wrong and a derived one cannot** — there is no second place for the
//! mapping to disagree with itself.
//!
//! ## Why clap is not used here
//!
//! clap builds its tree before argv is parsed, and the tree is not known until
//! the daemon has answered. The two-pass alternative — parse globals with clap,
//! connect, rebuild, re-parse — buys `--help` rendering at the cost of a parser
//! that runs twice over the same argv with different grammars. Parsing directly
//! is smaller and has one grammar.

use logmon_broker_protocol::mcp_tools::ManifestEntry;
use logmon_broker_sdk::Broker;
use serde_json::{Map, Value};

use super::format;

/// `collectors.list` -> `["collectors", "list"]`.
///
/// The dot is the only separator: underscores inside a segment are part of the
/// word (`domain_data.update` is `domain-data update`), and hyphens are what a
/// CLI user types.
pub fn command_path(method: &str) -> Vec<String> {
    method.split('.').map(|s| s.replace('_', "-")).collect()
}

/// A tool matched from argv, with the arguments that follow it.
pub struct Matched<'a> {
    pub entry: &'a ManifestEntry,
    pub rest: Vec<String>,
}

/// Find the tool whose derived path is the longest prefix of `argv`.
///
/// Longest-prefix rather than fixed-depth because nothing guarantees every
/// method has exactly two segments, and a two-segment assumption would quietly
/// mis-route the first one that does not.
pub fn match_command<'a>(entries: &'a [ManifestEntry], argv: &[String]) -> Option<Matched<'a>> {
    let mut best: Option<Matched<'a>> = None;
    for entry in entries {
        let path = command_path(&entry.method);
        if path.len() > argv.len() {
            continue;
        }
        if argv[..path.len()]
            .iter()
            .zip(&path)
            .all(|(a, p)| a.eq_ignore_ascii_case(p))
        {
            let longer = best
                .as_ref()
                .is_none_or(|b| command_path(&b.entry.method).len() < path.len());
            if longer {
                best = Some(Matched {
                    entry,
                    rest: argv[path.len()..].to_vec(),
                });
            }
        }
    }
    if best.is_some() {
        return best;
    }

    // Nothing matched in full. If what was typed is a *prefix* of exactly one
    // command, that is the one meant: `status` can only be `status get`.
    //
    // A generic rule rather than an alias table — an alias would be knowledge
    // of a particular tool, and would have to be revised whenever a second
    // command joined the group. This revises itself: the day `status.reset`
    // exists, bare `status` becomes ambiguous and says so.
    let leading: Vec<&String> = argv.iter().take_while(|a| !a.starts_with('-')).collect();
    if leading.is_empty() {
        return None;
    }
    let mut hits = entries.iter().filter(|e| {
        let path = command_path(&e.method);
        path.len() > leading.len()
            && leading
                .iter()
                .zip(&path)
                .all(|(a, p)| a.eq_ignore_ascii_case(p))
    });
    let only = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some(Matched {
        entry: only,
        rest: argv[leading.len()..].to_vec(),
    })
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Walk a possibly-`anyOf` schema node to the object arm that describes a value.
///
/// `Option<T>` renders as `anyOf: [T, null]`, so the useful half is behind an
/// arm. A caller that reads the node directly sees neither the type nor the
/// properties of an optional parameter — which is most of them.
fn unwrap_optional(node: &Value) -> &Value {
    if let Some(arms) = node.get("anyOf").and_then(|a| a.as_array()) {
        for arm in arms {
            if arm.get("type").is_some_and(|t| t != "null") || arm.get("properties").is_some() {
                return arm;
            }
        }
    }
    node
}

/// The JSON type a value should be sent as.
///
/// `["string","null"]` is how an optional string renders, so `null` is stripped
/// before deciding — otherwise every optional parameter falls through to the
/// string default and `--count 50` sends `"50"`.
fn json_type(node: &Value) -> Option<String> {
    match unwrap_optional(node).get("type")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(vs) => vs
            .iter()
            .filter_map(|v| v.as_str())
            .find(|s| *s != "null")
            .map(str::to_string),
        _ => None,
    }
}

fn props(schema: &Value) -> Option<&Map<String, Value>> {
    unwrap_optional(schema)
        .get("properties")
        .and_then(|p| p.as_object())
}

/// Coerce one argv token to the type the schema declares.
fn coerce(raw: &str, node: Option<&Value>) -> Result<Value, String> {
    let ty = node.and_then(json_type);
    match ty.as_deref() {
        // `integer` and `number` are NOT interchangeable: an integer sent as
        // f64 renders `3.0`, and a daemon reading it as u64 refuses that.
        Some("integer") => raw
            .parse::<i64>()
            .map(|n| serde_json::json!(n))
            .map_err(|_| format!("`{raw}` is not a whole number")),
        Some("number") => raw
            .parse::<f64>()
            .map(|n| serde_json::json!(n))
            .map_err(|_| format!("`{raw}` is not a number")),
        Some("boolean") => match raw {
            "true" | "1" | "yes" => Ok(Value::Bool(true)),
            "false" | "0" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("`{raw}` is not true or false")),
        },
        // An array element's own type is what matters; the array is built by
        // repetition, one element per occurrence.
        Some("array") => {
            let item = node.and_then(|n| unwrap_optional(n).get("items"));
            coerce(raw, item)
        }
        _ => Ok(Value::String(raw.to_string())),
    }
}

/// Insert, accumulating repeats into an array.
///
/// `is_list` comes from the schema, and it is what makes a **single** value for
/// an array parameter arrive as `["a"]` rather than `"a"`. Accumulating only on
/// the second occurrence looks right and is wrong for every one-element list —
/// `--group-keys a` and `document a@base` both hit it, and the daemon rejects
/// them with "must be an array of strings, not a string".
fn insert(out: &mut Map<String, Value>, key: &str, value: Value, is_list: bool) {
    match out.remove(key) {
        None if is_list => {
            out.insert(key.to_string(), Value::Array(vec![value]));
        }
        None => {
            out.insert(key.to_string(), value);
        }
        Some(Value::Array(mut existing)) => {
            existing.push(value);
            out.insert(key.to_string(), Value::Array(existing));
        }
        Some(first) => {
            out.insert(key.to_string(), Value::Array(vec![first, value]));
        }
    }
}

/// Whether the schema says this parameter is a list.
fn is_list(node: Option<&Value>) -> bool {
    node.and_then(json_type).as_deref() == Some("array")
}

/// `--threshold.metric avg_ms` builds `{"threshold": {"metric": "avg_ms"}}`.
///
/// **Without this, an object parameter is unreachable.** `add_collector`'s
/// `threshold` is five fields behind one name, and a flat `--key value` grammar
/// can express neither the name nor the fields. The hand-written CLI solved it
/// by inventing `--threshold-metric` and friends — knowledge of the tool baked
/// into the front-end, which is exactly what is being removed.
fn insert_nested(out: &mut Map<String, Value>, path: &[&str], value: Value, is_list: bool) {
    let Some((head, tail)) = path.split_first() else {
        return;
    };
    if tail.is_empty() {
        insert(out, head, value, is_list);
        return;
    }
    let slot = out
        .entry(head.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    insert_nested(
        slot.as_object_mut().expect("just made an object"),
        tail,
        value,
        is_list,
    );
}

/// Resolve `threshold.metric` to the schema node describing `metric`.
fn node_for<'a>(schema: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut node = schema;
    for seg in path {
        node = props(node)?.get(*seg)?;
    }
    Some(node)
}

/// Turn argv into the request body.
pub fn build_params(args: &[String], entry: &ManifestEntry) -> Result<Map<String, Value>, String> {
    let empty = Value::Null;
    let schema = entry.input_schema.as_ref().unwrap_or(&empty);
    let mut out: Map<String, Value> = Map::new();
    let mut positional_taken = 0usize;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        let Some(flag) = arg.strip_prefix("--") else {
            // Positional, if this tool declares any.
            let hints = &entry.cli;
            let Some(name) = hints.positional.get(positional_taken) else {
                return Err(format!(
                    "`{}` takes no positional argument here — `{arg}` should be a `--flag`",
                    entry.name
                ));
            };
            let is_last = positional_taken + 1 == hints.positional.len();
            let node = node_for(schema, &[name.as_str()]);
            insert(&mut out, name, coerce(arg, node)?, is_list(node));
            // A variadic tail keeps absorbing into the same key rather than
            // advancing, so `document a b c` fills one list.
            if !(is_last && hints.variadic) {
                positional_taken += 1;
            }
            i += 1;
            continue;
        };

        let (flag, inline) = match flag.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (flag.to_string(), None),
        };
        // Hyphens are what a CLI user types; underscores are what the wire
        // uses. Dots separate nested fields and survive the swap.
        let normalised = flag.replace('-', "_");
        let path: Vec<&str> = normalised.split('.').collect();

        // A local output path is a real flag with **no schema entry**, because
        // it is never sent: `export_logs.path` is client-side by construction,
        // so the daemon's request type does not and should not describe it.
        // Without this it is rejected as unknown before it can be honoured.
        let is_output_path = entry
            .cli
            .file_output
            .as_ref()
            .is_some_and(|f| path.len() == 1 && path[0] == f.path_param);

        let node = node_for(schema, &path);
        if node.is_none() && !is_output_path {
            return Err(unknown_key_error(entry, schema, &flag));
        }

        let raw = match inline {
            Some(v) => v,
            None => {
                let next_is_flag = args.get(i + 1).is_none_or(|n| n.starts_with("--"));
                // `node` is None only for a local output path, which always
                // takes a value — so the boolean shorthand cannot apply.
                let is_bool = node.and_then(json_type).as_deref() == Some("boolean");
                if is_bool && next_is_flag {
                    // A bare `--flag` on a boolean means true.
                    insert_nested(&mut out, &path, Value::Bool(true), false);
                    i += 1;
                    continue;
                }
                match args.get(i + 1) {
                    Some(v) => {
                        i += 1;
                        v.clone()
                    }
                    None => return Err(format!("`--{flag}` was given no value")),
                }
            }
        };
        i += 1;
        insert_nested(&mut out, &path, coerce(&raw, node)?, is_list(node));
    }
    Ok(out)
}

fn dashed(k: &str) -> String {
    format!("--{}", k.replace('_', "-"))
}

/// An unknown flag names the ones that exist, because the manifest came from
/// the daemon being called and is therefore authoritative about key names —
/// and most handlers read parameters raw, so a forwarded typo is silently
/// ignored rather than refused.
fn unknown_key_error(entry: &ManifestEntry, schema: &Value, flag: &str) -> String {
    let known: Vec<String> = props(schema)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    let normalised = flag.replace('-', "_");
    let stem = normalised.split('.').next().unwrap_or(&normalised);
    let close: Vec<String> = known
        .iter()
        .filter(|k| k.starts_with(stem) || stem.starts_with(k.as_str()))
        .map(|k| dashed(k))
        .collect();

    let hint = if known.is_empty() {
        "it takes no parameters".to_string()
    } else if close.is_empty() {
        format!(
            "known parameters: {}",
            known
                .iter()
                .map(|k| dashed(k))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!("did you mean {}", close.join(" or "))
    };
    format!("`{}` takes no `{}` — {hint}", entry.name, dashed(flag))
}

// ---------------------------------------------------------------------------
// Help, rendered from the manifest
// ---------------------------------------------------------------------------

/// Every command, grouped by its first path segment.
pub fn help_index(entries: &[ManifestEntry]) -> String {
    let mut out = String::from("logmon broker CLI — commands come from the daemon.\n\n");
    let mut last_group = String::new();
    let mut rows: Vec<(Vec<String>, &ManifestEntry)> = entries
        .iter()
        .map(|e| (command_path(&e.method), e))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, e) in rows {
        if path[0] != last_group {
            out.push_str(&format!("\n{}\n", path[0]));
            last_group = path[0].clone();
        }
        let summary = e
            .description
            .split_once(". ")
            .map(|(h, _)| h)
            .unwrap_or(&e.description);
        let verb = path[1..].join(" ");
        out.push_str(&format!("  {verb:<22} {}\n", summary.trim()));
    }
    out.push_str("\nlogmon-mcp <group> <verb> --help   for a command's arguments\n");
    out
}

/// One command's arguments, from its schema.
pub fn help_command(entry: &ManifestEntry) -> String {
    let path = command_path(&entry.method).join(" ");
    let mut out = format!("{path}\n\n{}\n", entry.description);

    let empty = Value::Null;
    let schema = entry.input_schema.as_ref().unwrap_or(&empty);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if !entry.cli.positional.is_empty() {
        out.push_str(&format!(
            "\npositional: {}{}\n",
            entry.cli.positional.join(" "),
            if entry.cli.variadic { " ..." } else { "" }
        ));
    }

    if let Some(f) = &entry.cli.file_output {
        let extra = if f.sidecar.is_some() {
            " (and its companion file beside it)"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n  {:<22} local file to write the result to{extra} — never sent to the daemon\n",
            dashed(&f.path_param)
        ));
    }

    match props(schema) {
        None => out.push_str("\ntakes no arguments\n"),
        Some(p) => {
            out.push_str("\narguments:\n");
            for (name, node) in p {
                let ty = json_type(node).unwrap_or_else(|| "string".into());
                let req = if required.contains(&name.as_str()) {
                    " (required)"
                } else {
                    ""
                };
                out.push_str(&format!("  {:<22} {ty}{req}\n", dashed(name)));
                if let Some(vs) = unwrap_optional(node).get("enum").and_then(|e| e.as_array()) {
                    let shown: Vec<String> = vs
                        .iter()
                        .filter(|v| !v.is_null())
                        .map(|v| v.as_str().unwrap_or_default().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    out.push_str(&format!("  {:<22}   one of: {}\n", "", shown.join(", ")));
                }
                // Nested objects list their fields as dotted flags, which is
                // the only way a caller could discover they exist.
                if let Some(inner) = props(node) {
                    for k in inner.keys() {
                        // Shown exactly as it must be typed. A nested field
                        // rendered with the wire's underscore is a flag the
                        // reader will copy and then have rejected.
                        out.push_str(&format!(
                            "  {:<22}\n",
                            format!("--{}.{}", name.replace('_', "-"), k.replace('_', "-"))
                        ));
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub async fn fetch_manifest(broker: &Broker) -> Result<Vec<ManifestEntry>, String> {
    let reply = broker
        .call("tools.manifest", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(reply.get("tools").cloned().unwrap_or_default())
        .map_err(|e| e.to_string())
}

/// Print the reply.
///
/// **Presentation belongs to the daemon.** If the reply carries a `_display`
/// string, that is what a human sees; otherwise it is JSON. This is the whole
/// of the shim's rendering policy — anything richer would be knowledge of a
/// particular result shape, which is what this file exists not to have.
fn emit(reply: &Value, json: bool) {
    if !json {
        if let Some(text) = reply.get("_display").and_then(|d| d.as_str()) {
            println!("{text}");
            return;
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(reply).unwrap_or_else(|e| format!("{e}"))
    );
}

/// Run one command. `argv` is everything after the global flags.
pub async fn dispatch(broker: &Broker, argv: &[String], json: bool) -> i32 {
    let entries = match fetch_manifest(broker).await {
        Ok(e) => e,
        Err(e) => {
            format::error(
                &format!("could not read the daemon's tool manifest: {e}"),
                json,
            );
            return 1;
        }
    };

    if argv.is_empty() || argv[0] == "help" || argv[0] == "--help" || argv[0] == "-h" {
        print!("{}", help_index(&entries));
        return 0;
    }

    let Some(m) = match_command(&entries, argv) else {
        // A named group with several commands is a different mistake from a
        // name that does not exist, and the useful reply is different too: one
        // wants the group's verbs, the other wants the groups.
        let head = argv.first().map(String::as_str).unwrap_or_default();
        let verbs: Vec<String> = entries
            .iter()
            .map(|e| command_path(&e.method))
            .filter(|p| p[0] == head && p.len() > 1)
            .map(|p| p[1..].join(" "))
            .collect();

        let message = if verbs.is_empty() {
            let groups: std::collections::BTreeSet<String> = entries
                .iter()
                .map(|e| command_path(&e.method)[0].clone())
                .collect();
            format!(
                "no such command `{}` — groups: {}",
                argv.join(" "),
                groups.into_iter().collect::<Vec<_>>().join(", ")
            )
        } else {
            let mut v = verbs;
            v.sort();
            format!("`{head}` needs a verb: {}", v.join(", "))
        };
        format::error(&message, json);
        return 1;
    };

    if m.rest.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", help_command(m.entry));
        return 0;
    }

    let mut params = match build_params(&m.rest, m.entry) {
        Ok(p) => p,
        Err(e) => {
            format::error(&e, json);
            return 1;
        }
    };

    // A local output path is consumed here, never forwarded: the daemon would
    // resolve it against its own working directory.
    let out_path = m
        .entry
        .cli
        .file_output
        .as_ref()
        .and_then(|f| params.remove(f.path_param.as_str()))
        .and_then(|v| v.as_str().map(str::to_string));

    let reply = match broker.call(&m.entry.method, Value::Object(params)).await {
        Ok(r) => r,
        Err(e) => {
            format::error(&e.to_string(), json);
            return 1;
        }
    };

    // `--json` asks for the reply, not for a side effect on disk.
    if json {
        emit(&reply, true);
        return 0;
    }

    let files = logmon_broker_protocol::mcp_tools::files_to_write(
        &m.entry.cli,
        &reply,
        out_path.as_deref(),
    );
    if files.is_empty() {
        // A tool that CAN write a file but was given no path prints its body
        // rather than the envelope — `collectors document` with no `--path` is
        // meant to show you the document.
        if let Some(field) = m
            .entry
            .cli
            .file_output
            .as_ref()
            .and_then(|f| f.content_field.as_ref())
        {
            if let Some(v) = reply.get(field) {
                match v.as_str() {
                    Some(s) => println!("{s}"),
                    None => emit(v, false),
                }
                warn_about_unwritten_sidecar(m.entry, &reply);
                return 0;
            }
        }
        emit(&reply, false);
        return 0;
    }

    for f in files {
        if let Err(e) = std::fs::write(&f.path, &f.body) {
            format::error(&format!("failed to write {}: {e}", f.path.display()), json);
            return 1;
        }
        println!("wrote {} ({} bytes)", f.path.display(), f.body.len());
    }
    0
}

/// A companion file was produced and thrown away because no path was given.
///
/// Silence here is what makes a sidecar go missing without anyone noticing —
/// the document's front-matter still refers to a file that was never written.
fn warn_about_unwritten_sidecar(entry: &ManifestEntry, reply: &Value) {
    let Some(side) = entry
        .cli
        .file_output
        .as_ref()
        .and_then(|f| f.sidecar.as_ref())
    else {
        return;
    };
    if reply
        .get(&side.name_field)
        .and_then(|v| v.as_str())
        .is_some()
    {
        let path_param = entry
            .cli
            .file_output
            .as_ref()
            .map(|f| f.path_param.as_str())
            .unwrap_or("path");
        eprintln!(
            "note: a companion file was produced but not written — pass --{} to write both",
            path_param.replace('_', "-")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logmon_broker_protocol::mcp_tools;

    fn real(name: &str) -> ManifestEntry {
        mcp_tools::manifest()
            .into_iter()
            .find(|e| e.name == name)
            .expect("in the manifest")
    }

    fn parse(name: &str, args: &[&str]) -> Map<String, Value> {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        build_params(&owned, &real(name)).expect("parses")
    }

    fn err(name: &str, args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        build_params(&owned, &real(name)).expect_err("refused")
    }

    #[test]
    fn paths_are_derived_from_the_method() {
        assert_eq!(command_path("collectors.list"), ["collectors", "list"]);
        assert_eq!(command_path("status.get"), ["status", "get"]);
        assert_eq!(
            command_path("domain_data.update"),
            ["domain-data", "update"]
        );
    }

    #[test]
    fn a_command_is_matched_by_its_derived_path() {
        let m = mcp_tools::manifest();
        let argv: Vec<String> = ["collectors", "list"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let hit = match_command(&m, &argv).expect("matched");
        assert_eq!(hit.entry.method, "collectors.list");
        assert!(hit.rest.is_empty());
    }

    /// The gap that made a generic CLI impossible: five fields behind one name.
    /// `status.get` is the only command in its group, so bare `status` means it.
    /// Without this, deriving paths would have turned `logmon-mcp status` into
    /// `logmon-mcp status get` — a working command replaced by a clumsier one.
    #[test]
    fn a_group_with_one_command_is_reachable_by_the_group_name() {
        let m = mcp_tools::manifest();
        let argv: Vec<String> = ["status"].iter().map(|s| s.to_string()).collect();
        let hit = match_command(&m, &argv).expect("resolved");
        assert_eq!(hit.entry.method, "status.get");
        assert!(hit.rest.is_empty());
    }

    /// And an ambiguous prefix stays ambiguous rather than picking one.
    #[test]
    fn a_group_with_several_commands_is_not_guessed() {
        let m = mcp_tools::manifest();
        let argv: Vec<String> = ["collectors"].iter().map(|s| s.to_string()).collect();
        assert!(
            match_command(&m, &argv).is_none(),
            "`collectors` names a group, not a command"
        );
    }

    /// The shortened form still carries its arguments.
    #[test]
    fn a_shortened_command_keeps_its_arguments() {
        let m = mcp_tools::manifest();
        let argv: Vec<String> = ["status", "--json"].iter().map(|s| s.to_string()).collect();
        let hit = match_command(&m, &argv).expect("resolved");
        assert_eq!(hit.entry.method, "status.get");
        assert_eq!(hit.rest, ["--json"]);
    }

    #[test]
    fn a_nested_object_is_built_from_dotted_flags() {
        let p = parse(
            "add_collector",
            &[
                "--name",
                "c",
                "--filter",
                "ALL",
                "--threshold.metric",
                "avg_ms",
                "--threshold.op",
                "gte",
                "--threshold.value",
                "250",
                "--threshold.window-ms",
                "60000",
            ],
        );
        assert_eq!(p["threshold"]["metric"], "avg_ms");
        assert_eq!(p["threshold"]["op"], "gte");
        // Typed from the nested schema, not guessed: a string here is refused
        // by the daemon.
        assert_eq!(p["threshold"]["value"], serde_json::json!(250.0));
        assert_eq!(p["threshold"]["window_ms"], serde_json::json!(60000));
    }

    #[test]
    fn an_integer_does_not_go_out_as_a_float() {
        let p = parse("get_recent_logs", &["--count", "3"]);
        assert_eq!(serde_json::to_string(&p["count"]).unwrap(), "3");
    }

    /// The bug: accumulating only on the SECOND occurrence looks right and is
    /// wrong for every one-element list. A live daemon refused both of these
    /// with "must be an array of strings, not a string" — the unit tests all
    /// passed two values, so none of them could see it.
    #[test]
    fn a_single_value_for_a_list_parameter_is_still_a_list() {
        let p = parse(
            "add_collector",
            &["--name", "c", "--filter", "ALL", "--group-keys", "x"],
        );
        assert_eq!(p["group_keys"], serde_json::json!(["x"]));

        // Same for a variadic positional given exactly one arm.
        let d = parse("document_collectors", &["a@base"]);
        assert_eq!(d["names"], serde_json::json!(["a@base"]));
    }

    /// And a scalar parameter must NOT be wrapped just because it appears once.
    #[test]
    fn a_scalar_parameter_is_not_wrapped_in_a_list() {
        let p = parse("add_collector", &["--name", "c", "--filter", "ALL"]);
        assert_eq!(p["name"], serde_json::json!("c"));
        assert!(!p["name"].is_array());
    }

    #[test]
    fn a_repeated_flag_becomes_an_array() {
        let p = parse(
            "add_collector",
            &[
                "--name",
                "c",
                "--filter",
                "ALL",
                "--group-keys",
                "a",
                "--group-keys",
                "b",
            ],
        );
        assert_eq!(p["group_keys"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn positionals_come_from_the_manifest_hints() {
        let p = parse("diff_collectors", &["base@*", "cand@*"]);
        assert_eq!(p["a"], "base@*");
        assert_eq!(p["b"], "cand@*");
    }

    #[test]
    fn a_variadic_positional_absorbs_the_rest() {
        let p = parse("document_collectors", &["a", "b", "c"]);
        assert_eq!(p["names"], serde_json::json!(["a", "b", "c"]));
    }

    /// `export_logs.path` names a LOCAL file, so the daemon's request type does
    /// not describe it — and the unknown-key check would reject it as a typo if
    /// the hint were not consulted first.
    #[test]
    fn a_local_output_path_is_accepted_though_no_schema_declares_it() {
        let p = parse("export_logs", &["--path", "/tmp/x.json", "--count", "5"]);
        assert_eq!(p["path"], "/tmp/x.json");
        assert_eq!(serde_json::to_string(&p["count"]).unwrap(), "5");
        // And it is still refused on a tool that does not declare one.
        assert!(err("get_recent_logs", &["--path", "/tmp/x"]).contains("takes no"));
    }

    #[test]
    fn help_names_the_local_output_path() {
        let h = help_command(&real("export_logs"));
        assert!(h.contains("--path"), "{h}");
        assert!(h.contains("never sent to the daemon"), "{h}");
    }

    #[test]
    fn a_misspelled_flag_names_the_real_one() {
        assert!(err("add_collector", &["--group-key", "a"]).contains("--group-keys"));
        assert!(err("list_collectors", &["--bogus", "1"]).contains("takes no parameters"));
    }

    #[test]
    fn help_shows_accepted_values_and_nested_fields() {
        let h = help_command(&real("add_collector"));
        assert!(h.contains("--level"), "{h}");
        assert!(h.contains("one of:"), "enums must be visible: {h}");
        assert!(h.contains("tree"), "{h}");
        assert!(h.contains("--threshold.metric"), "nested fields: {h}");
        assert!(h.contains("(required)"), "{h}");
    }

    #[test]
    fn the_index_lists_every_command() {
        let m = mcp_tools::manifest();
        let idx = help_index(&m);
        for e in &m {
            let path = command_path(&e.method);
            assert!(
                idx.contains(&path[1..].join(" ")),
                "{} missing from the index",
                e.method
            );
        }
    }
}
