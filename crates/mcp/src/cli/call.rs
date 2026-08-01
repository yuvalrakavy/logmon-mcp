//! `logmon-mcp call <tool> --param value …` — reach any tool the daemon declares.
//!
//! **The CLI half of §11.0's goal.** The thirteen hand-written command groups
//! each know one tool intimately and render its result for a human; this verb
//! knows none of them and reaches all of them. A tool the daemon gains is
//! callable from the CLI the moment it is declared, with no rebuild — which is
//! what the hand-written groups cannot offer, because each new tool needs a new
//! clap variant, a new dispatch arm and a new renderer.
//!
//! It is the floor, not the ceiling. A tool graduates to its own group when
//! someone wants nice output for it; until then it is usable rather than
//! unreachable. Generating the *whole* CLI from the schema was measured and
//! rejected (spec §11.5.2): it would have replaced 158 lines of `#[arg]` and put
//! 315 lines of human rendering at risk.
//!
//! Arguments are parsed against the manifest's schema only far enough to build
//! JSON of the right *shape* — string, number, boolean, array. Whether the
//! values are *valid* is the daemon's question, and it answers it with full
//! knowledge of the tool; a second, shallower check here could only reject
//! things the daemon would have accepted.

use clap::Args;
use logmon_broker_protocol::mcp_tools::ManifestEntry;
use logmon_broker_sdk::Broker;
use serde_json::{Map, Value};

use super::format;

#[derive(Args, Debug)]
pub struct CallCmd {
    /// Tool name as the daemon lists it, e.g. `list_collectors`. Omit to list
    /// every tool this daemon serves.
    tool: Option<String>,

    /// `--key value`, repeatable. Values are typed from the tool's schema, so
    /// `--count 50` sends a number and `--oneshot true` sends a boolean.
    /// Repeat a key to build an array.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

pub async fn dispatch(broker: &Broker, cmd: CallCmd, json: bool) -> i32 {
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

    let Some(name) = cmd.tool else {
        list_tools(&entries, json);
        return 0;
    };

    let Some(entry) = entries.iter().find(|e| e.name == name) else {
        // Naming the near-misses, because a wrong tool name and a tool this
        // daemon is too old to have are different problems with the same
        // symptom, and the list is what tells them apart.
        let close: Vec<&str> = entries
            .iter()
            .map(|e| e.name.as_str())
            .filter(|n| n.contains(&name) || name.contains(n))
            .collect();
        let hint = if close.is_empty() {
            "run `logmon-mcp call` with no tool to list them".to_string()
        } else {
            format!("did you mean: {}", close.join(", "))
        };
        format::error(
            &format!("this daemon serves no tool `{name}` — {hint}"),
            json,
        );
        return 1;
    };

    let params = match build_params(&cmd.args, entry) {
        Ok(p) => p,
        Err(e) => {
            format::error(&e, json);
            return 1;
        }
    };

    match broker.call(&entry.method, Value::Object(params)).await {
        Ok(reply) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&reply).unwrap_or_else(|e| format!("{e}"))
            );
            0
        }
        Err(e) => {
            format::error(&e.to_string(), json);
            1
        }
    }
}

async fn fetch_manifest(broker: &Broker) -> Result<Vec<ManifestEntry>, String> {
    let reply = broker
        .call("tools.manifest", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(reply.get("tools").cloned().unwrap_or_default())
        .map_err(|e| e.to_string())
}

fn list_tools(entries: &[ManifestEntry], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        );
        return;
    }
    println!("{} tools served by this daemon:\n", entries.len());
    for e in entries {
        // First sentence only: the list is for finding a tool, not reading
        // about one. `call <tool> --help`-style detail is the schema itself.
        let summary = e
            .description
            .split_once(". ")
            .map(|(head, _)| head)
            .unwrap_or(&e.description);
        println!("  {:<24} {}", e.name, summary);
    }
    println!("\nCall one with: logmon-mcp call <tool> --key value");
}

/// The JSON type a parameter should be sent as, read from the tool's schema.
///
/// `["string","null"]` is how schemars renders `Option<String>`, so the null is
/// stripped before deciding — otherwise every optional parameter would fall
/// through to the string default and `--count 50` would send `"50"`.
fn json_type_of(schema: &Value, key: &str) -> Option<String> {
    let prop = schema.get("properties")?.get(key)?;
    match prop.get("type")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(vs) => vs
            .iter()
            .filter_map(|v| v.as_str())
            .find(|s| *s != "null")
            .map(str::to_string),
        _ => None,
    }
}

/// The parameter names a tool declares.
///
/// `None` only when there is **no schema at all**, which is the one case where
/// nothing can be checked. A schema with no `properties` returns an empty list,
/// not `None`: schemars omits the key entirely for a request type with no
/// fields (`CollectorsList`, `StatusGet`, `LogsClear` are all like this), so
/// absent means "takes no parameters" — a fact worth enforcing rather than a
/// gap to wave through.
fn known_keys(schema: &Value) -> Option<Vec<String>> {
    if !schema.is_object() {
        return None;
    }
    Some(match schema.get("properties").and_then(|p| p.as_object()) {
        Some(props) => props.keys().cloned().collect(),
        None => Vec::new(),
    })
}

fn coerce(raw: &str, ty: Option<&str>) -> Result<Value, String> {
    match ty {
        // `integer` and `number` are NOT interchangeable on the wire. Sending
        // an integer parameter as f64 renders it `3.0`, and the daemon's
        // `opt_u64` refuses that with "must be a non-negative integer, not a
        // number" — found by calling a live daemon, not by any unit test here,
        // because the tests had encoded this function's own assumption.
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
        // Unknown or absent type: send it as a string and let the daemon judge.
        // It reads these with full knowledge of the tool, and since the
        // parameter-typing fix a wrong type is an error rather than a different
        // answer — so a bad guess here is reported, not silently absorbed.
        _ => Ok(Value::String(raw.to_string())),
    }
}

/// Turn `--key value --key value2 --flag` into the request body.
///
/// A key repeated becomes an array, because that is how every repeatable
/// parameter in this protocol is shaped (`group_keys`, `names`, `paths`), and
/// requiring a different spelling for the second occurrence would be a trap.
fn build_params(args: &[String], entry: &ManifestEntry) -> Result<Map<String, Value>, String> {
    let empty = Value::Null;
    let schema = entry.input_schema.as_ref().unwrap_or(&empty);
    let mut out: Map<String, Value> = Map::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        let Some(key) = arg.strip_prefix("--") else {
            return Err(format!(
                "expected `--key value`, found `{arg}` — every argument to `call` is a named \
                 parameter of the tool"
            ));
        };
        // `--key=value` as well as `--key value`, because both are habitual and
        // the difference is not one a caller should have to remember.
        let (key, inline) = match key.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (key.to_string(), None),
        };
        // Underscores are what the schema uses; hyphens are what a CLI user
        // types. Accept both rather than making the wire spelling a usability
        // problem.
        let key = key.replace('-', "_");

        // **An unknown key is an error, not something to forward.** The schema
        // came from the daemon we are about to call, so it is authoritative
        // about key names — and most handlers read parameters raw, meaning an
        // unrecognised one is silently ignored rather than refused. Found by
        // calling a live daemon: `--group-key a --group-key b` came back as
        // `group_keys: []`, the flags dropped without a word, because the
        // property is `group_keys` and nothing was there to say so.
        if let Some(known) = known_keys(schema) {
            if !known.contains(&key) {
                let close: Vec<&str> = known
                    .iter()
                    .filter(|k| k.starts_with(&key) || key.starts_with(k.as_str()))
                    .map(String::as_str)
                    .collect();
                // Rendered with hyphens, because that is what a CLI user typed
                // and echoing the wire spelling back invites typing it again.
                let dashed = |k: &str| format!("--{}", k.replace('_', "-"));
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
                    format!(
                        "did you mean {}",
                        close
                            .iter()
                            .map(|k| dashed(k))
                            .collect::<Vec<_>>()
                            .join(" or ")
                    )
                };
                return Err(format!(
                    "`{}` takes no `{}` — {hint}",
                    entry.name,
                    dashed(&key)
                ));
            }
        }

        let ty = json_type_of(schema, &key);

        let raw = match inline {
            Some(v) => v,
            None => match args.get(i + 1) {
                // A bare `--flag` on a boolean parameter means true.
                _ if ty.as_deref() == Some("boolean")
                    && args.get(i + 1).is_none_or(|n| n.starts_with("--")) =>
                {
                    i += 1;
                    insert(&mut out, key, Value::Bool(true));
                    continue;
                }
                Some(v) => {
                    i += 1;
                    v.clone()
                }
                None => return Err(format!("`--{key}` was given no value")),
            },
        };
        i += 1;

        let value = coerce(&raw, ty.as_deref())?;
        insert(&mut out, key, value);
    }
    Ok(out)
}

/// Second and later occurrences of a key accumulate into an array.
fn insert(out: &mut Map<String, Value>, key: String, value: Value) {
    match out.remove(&key) {
        None => {
            out.insert(key, value);
        }
        Some(Value::Array(mut existing)) => {
            existing.push(value);
            out.insert(key, Value::Array(existing));
        }
        Some(first) => {
            out.insert(key, Value::Array(vec![first, value]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(schema: Value) -> ManifestEntry {
        ManifestEntry {
            name: "t".into(),
            method: "m".into(),
            description: "d".into(),
            input_schema: Some(schema),
        }
    }

    fn sample() -> ManifestEntry {
        entry_with(serde_json::json!({
            "type": "object",
            "properties": {
                "name":       { "type": "string" },
                "count":      { "type": ["integer", "null"] },
                "ratio":      { "type": ["number", "null"] },
                "oneshot":    { "type": ["boolean", "null"] },
                "group_keys": { "type": ["array", "null"] },
            }
        }))
    }

    fn parse(args: &[&str]) -> Map<String, Value> {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        build_params(&owned, &sample()).expect("parses")
    }

    /// The defect this guards: `["integer","null"]` is how every optional number
    /// is rendered, so reading only a bare `"integer"` sends `"50"` as a string
    /// for essentially every numeric parameter in the protocol.
    #[test]
    fn an_optional_number_is_sent_as_a_number() {
        assert_eq!(parse(&["--count", "50"])["count"], serde_json::json!(50));
    }

    /// **`integer` and `number` are not interchangeable**, and this test exists
    /// because the first version of this file treated them as one: `--count 3`
    /// went out as `3.0` and the daemon refused it with "must be a
    /// non-negative integer, not a number". Seven unit tests passed through
    /// that, because they asserted what the code did rather than what the
    /// daemon accepts; one call to a live daemon found it immediately.
    #[test]
    fn an_integer_parameter_does_not_go_out_as_a_float() {
        let v = &parse(&["--count", "3"])["count"];
        assert_eq!(
            serde_json::to_string(v).unwrap(),
            "3",
            "an integer parameter serialised as 3.0 is rejected by the daemon"
        );
        assert!(v.is_i64());

        // A genuinely fractional parameter still travels as one.
        let r = &parse(&["--ratio", "0.5"])["ratio"];
        assert_eq!(serde_json::to_string(r).unwrap(), "0.5");
    }

    /// A whole number is required where the schema says integer — otherwise the
    /// truncation happens silently somewhere further down.
    #[test]
    fn a_fractional_value_for_an_integer_parameter_is_refused() {
        let owned = vec!["--count".to_string(), "1.5".to_string()];
        let err = build_params(&owned, &sample()).expect_err("refused");
        assert!(err.contains("whole number"), "{err}");
    }

    #[test]
    fn a_bare_boolean_flag_means_true() {
        assert_eq!(parse(&["--oneshot"])["oneshot"], Value::Bool(true));
        assert_eq!(
            parse(&["--oneshot", "false"])["oneshot"],
            Value::Bool(false)
        );
    }

    #[test]
    fn a_repeated_key_becomes_an_array() {
        let p = parse(&["--group-keys", "a", "--group-keys", "b"]);
        assert_eq!(p["group_keys"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn hyphens_and_equals_are_both_accepted() {
        assert_eq!(parse(&["--name=x"])["name"], serde_json::json!("x"));
        assert_eq!(
            parse(&["--group-keys", "a"])["group_keys"],
            serde_json::json!("a"),
            "a CLI user types hyphens; the wire uses underscores"
        );
    }

    /// The bug a live daemon found: `--group-key` (singular) is silently
    /// dropped, because the property is `group_keys` and `collectors.add` reads
    /// its parameters raw, so an unknown key is ignored rather than refused.
    /// A CLI that drops a flag without a word is the exact failure this whole
    /// line of work exists to remove.
    #[test]
    fn a_misspelled_parameter_is_refused_and_the_right_one_named() {
        let owned = vec!["--group-key".to_string(), "a".to_string()];
        let err = build_params(&owned, &sample()).expect_err("refused");
        assert!(
            err.contains("--group-keys"),
            "must name the real one: {err}"
        );

        let owned = vec!["--nonsense".to_string(), "v".to_string()];
        let err = build_params(&owned, &sample()).expect_err("refused");
        assert!(
            err.contains("known parameters"),
            "with nothing close, list them: {err}"
        );
    }

    /// schemars omits `properties` entirely for a request type with no fields,
    /// so `list_collectors --bogus 1` used to be forwarded and quietly ignored.
    /// Absent properties means "takes no parameters", which is a fact to
    /// enforce rather than a gap to wave through.
    #[test]
    fn a_tool_that_takes_no_parameters_says_so() {
        let bare = entry_with(serde_json::json!({ "type": "object" }));
        let owned = vec!["--bogus".to_string(), "1".to_string()];
        let err = build_params(&owned, &bare).expect_err("refused");
        assert!(err.contains("takes no parameters"), "{err}");

        // And with no schema at all there is genuinely nothing to check.
        let unknown = ManifestEntry {
            input_schema: None,
            ..entry_with(Value::Null)
        };
        let owned = vec!["--whatever".to_string(), "v".to_string()];
        let p = build_params(&owned, &unknown).expect("forwarded");
        assert_eq!(p["whatever"], serde_json::json!("v"));
    }

    #[test]
    fn a_positional_argument_is_refused_with_a_reason() {
        let owned = vec!["oops".to_string()];
        let err = build_params(&owned, &sample()).expect_err("refused");
        assert!(err.contains("named parameter"), "{err}");
    }

    #[test]
    fn a_value_of_the_wrong_shape_is_named() {
        let owned = vec!["--count".to_string(), "many".to_string()];
        let err = build_params(&owned, &sample()).expect_err("refused");
        assert!(err.contains("whole number"), "{err}");

        let owned = vec!["--ratio".to_string(), "many".to_string()];
        let err = build_params(&owned, &sample()).expect_err("refused");
        assert!(err.contains("not a number"), "{err}");
    }
}
