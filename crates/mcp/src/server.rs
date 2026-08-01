//! The MCP surface, built from the daemon's manifest rather than from knowledge
//! of it.
//!
//! **No tool name, parameter struct or result shape appears in this file.**
//! There were 45 `#[rmcp::tool]` attributes and 37 parameter structs here; they
//! described a surface the daemon already describes, and every tool added to
//! the broker had to be re-declared here before an agent could reach it. The
//! router is now assembled at startup from `tools.manifest`, so a daemon that
//! gains a tool gains an MCP tool, with no reinstall of this binary.
//!
//! The two "client-side" exceptions went with them. `export_logs` writes a file
//! because a daemon resolves a relative path against its own working directory
//! — but *which* parameter names that file is a fact the manifest states, so
//! the generic forwarder can honour it without knowing the tool. And the
//! version-skew note existed only because this binary kept its own tool list to
//! compare against; with no list, there is nothing to skew, and the concept
//! disappears rather than needing a home.

use logmon_broker_protocol::mcp_tools::ManifestEntry;
use logmon_broker_sdk::Broker;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::{ToolCallContext, ToolRouter};
use rmcp::model::*;
use rmcp::ServerHandler;

#[derive(Clone)]
pub struct GelfMcpServer {
    broker: Broker,
    tool_router: ToolRouter<Self>,
}

/// Build a route that forwards a manifest-declared tool to the daemon, knowing
/// nothing about it but its name, its method and its schema.
///
/// Validation is deliberately absent. The daemon reads these parameters with
/// full knowledge of the tool; a second, shallower check here could only ever
/// reject things the daemon would have accepted.
///
/// Returns `None` when the entry carries no schema — a tool nothing describes
/// cannot be offered honestly, and offering it with an empty schema would
/// advertise "takes no arguments" for one that takes several.
pub fn forwarding_route(entry: &ManifestEntry) -> Option<ToolRoute<GelfMcpServer>> {
    let mut schema = entry.input_schema.as_ref()?.as_object()?.clone();
    // `$schema` and `title` come from the generator describing a Rust type; an
    // MCP client wants the argument shape, not the provenance of the file it
    // was cut from.
    schema.remove("$schema");
    schema.remove("title");

    // A local output path is not a parameter of the method — it is consumed
    // here — but it must still be *offered*, or the caller has no way to say
    // where the file goes. Added to the published schema, removed from the
    // arguments before they are forwarded.
    if let Some(key) = entry.cli.file_output.as_ref().map(|f| &f.path_param) {
        // Into `properties`, not the schema root. A key at the root is not a
        // parameter — it is a sibling of `type` and `required`, and an MCP
        // client looking for arguments never sees it.
        schema
            .entry("properties")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .map(|props| {
                props.insert(
                    key.clone(),
                    serde_json::json!({
                        "type": "string",
                        "description": "Local file path to write the result to. \
                                        Resolved by this client, never sent to the daemon.",
                    }),
                )
            });
    }

    let method = entry.method.clone();
    let hints = entry.cli.clone();

    // The argument names this tool accepts, taken from the schema AFTER the
    // local output path was added — that one is a real argument here even
    // though the daemon has never heard of it. `None` when the schema declares
    // no `properties`, which is a tool that takes nothing.
    // The argument names this tool accepts, taken from the schema AFTER the
    // local output path was added — that one is a real argument here even
    // though the daemon has never heard of it.
    //
    // **Absent `properties` means "takes no arguments", not "unknown".**
    // schemars omits the key entirely for a request type with no fields, so
    // treating it as unknown would wave every argument through on exactly the
    // tools that accept none — `list_collectors --nonsense` among them.
    let declared: std::sync::Arc<std::collections::BTreeSet<String>> = std::sync::Arc::new(
        schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default(),
    );

    Some(ToolRoute::new_dyn(
        Tool::new(
            entry.name.clone(),
            entry.description.clone(),
            std::sync::Arc::new(schema),
        ),
        move |ctx: ToolCallContext<'_, GelfMcpServer>| {
            let method = method.clone();
            let hints = hints.clone();
            let declared = declared.clone();
            Box::pin(async move {
                let mut args = ctx.arguments.clone().unwrap_or_default();

                // **A misspelled argument is an error, not something to
                // forward.** The daemon reads most parameters by key, so an
                // unrecognised one is silently ignored and the tool answers a
                // different question than the one asked. Phase 0 closed this
                // with `deny_unknown_fields` on 37 hand-written param structs;
                // deleting those structs reopened it on all 45 tools, and the
                // published `additionalProperties: false` only *declares* the
                // rule for clients that validate. This enforces it.
                //
                // Key NAMES only. Values are the daemon's business — it reads
                // them with full knowledge of the tool.
                let unknown: Vec<&str> = args
                    .keys()
                    .map(String::as_str)
                    .filter(|k| !declared.contains(*k))
                    .collect();
                if !unknown.is_empty() {
                    let accepts = if declared.is_empty() {
                        "it takes no arguments".to_string()
                    } else {
                        format!(
                            "it accepts: {}",
                            declared
                                .iter()
                                .map(String::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    return Err(rmcp::ErrorData::invalid_params(
                        format!("`{method}` got unknown argument(s) {unknown:?} — {accepts}"),
                        None,
                    ));
                }

                // Taken out before the call: the daemon does not describe it and
                // would reject it, and it means nothing on the daemon's side of
                // the filesystem anyway.
                let out_path = hints
                    .file_output
                    .as_ref()
                    .and_then(|f| args.remove(f.path_param.as_str()))
                    .and_then(|v| v.as_str().map(str::to_string));

                let result = ctx
                    .service
                    .broker
                    .call(&method, serde_json::Value::Object(args))
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

                // Which files, and what goes in them, is the protocol's answer
                // rather than this function's — so the CLI and this surface
                // cannot write different files for the same tool.
                let files = logmon_broker_protocol::mcp_tools::files_to_write(
                    &hints,
                    &result,
                    out_path.as_deref(),
                );
                if files.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        serde_json::to_string_pretty(&result)
                            .unwrap_or_else(|e| format!("unserialisable reply: {e}")),
                    )]));
                }

                let mut wrote = Vec::new();
                for f in files {
                    std::fs::write(&f.path, &f.body).map_err(|e| {
                        rmcp::ErrorData::internal_error(
                            format!("failed to write {}: {e}", f.path.display()),
                            None,
                        )
                    })?;
                    wrote.push(format!("{} ({} bytes)", f.path.display(), f.body.len()));
                }
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "wrote {}",
                    wrote.join(", ")
                ))]))
            })
        },
    ))
}

/// Assemble a router from what the daemon says it serves.
///
/// Every tool is a forwarding route; there is nothing else to be. An entry
/// without a schema is skipped rather than published broken, and the caller
/// reports how many were dropped — silent truncation would read as "this is
/// the whole surface" when it is not.
pub fn router_from_manifest(entries: &[ManifestEntry]) -> (ToolRouter<GelfMcpServer>, Vec<String>) {
    let mut router = ToolRouter::default();
    let mut skipped = Vec::new();
    for entry in entries {
        match forwarding_route(entry) {
            Some(route) => router.add_route(route),
            None => skipped.push(entry.name.clone()),
        }
    }
    (router, skipped)
}

impl GelfMcpServer {
    /// Ask the daemon what it serves, and serve exactly that.
    ///
    /// **Fails rather than starting empty.** With no compiled-in tools there is
    /// no degraded mode to fall back to: a shim that starts with zero tools
    /// looks to an MCP client like a server that legitimately offers none, and
    /// clients cache that. Refusing to start surfaces the real problem — the
    /// broker is unreachable — at the moment it can still be acted on.
    pub async fn taught_by(broker: Broker) -> anyhow::Result<Self> {
        let reply = broker
            .call("tools.manifest", serde_json::json!({}))
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "could not read the broker's tool manifest, so this shim has no tools \
                     to offer.\n\nThe broker may be older than this shim — `tools.manifest` \
                     was added in protocol v1; upgrade it with `cargo install --path \
                     crates/broker` and restart it.\n\nUnderlying error: {e}"
                )
            })?;

        let entries: Vec<ManifestEntry> =
            serde_json::from_value(reply.get("tools").cloned().unwrap_or_default())
                .map_err(|e| anyhow::anyhow!("the broker's tool manifest did not parse: {e}"))?;

        if entries.is_empty() {
            anyhow::bail!("the broker's tool manifest is empty, so there is nothing to serve");
        }

        let (tool_router, skipped) = router_from_manifest(&entries);
        if !skipped.is_empty() {
            tracing::warn!(
                tools = ?skipped,
                "the broker described these tools without a parameter schema; they are \
                 not being offered"
            );
        }
        tracing::info!(
            tools = tool_router.list_all().len(),
            "registered tools from the broker"
        );
        Ok(Self {
            broker,
            tool_router,
        })
    }
}

/// Skill content shipped as MCP `instructions` so any compliant client
/// (Claude Code, Cursor, etc.) surfaces it as server-level guidance
/// without the user installing the file by hand. The file lives at
/// `skill/logmon.md` at the workspace root and is embedded at compile
/// time.
const SKILL_INSTRUCTIONS: &str = include_str!("../../../skill/logmon.md");

#[rmcp::tool_handler]
impl ServerHandler for GelfMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(SKILL_INSTRUCTIONS)
    }
}

// ---------------------------------------------------------------------------
// What is still worth guarding, now that the router is the manifest
// ---------------------------------------------------------------------------
//
// Four guards were retired with the attributes they watched, and the reason is
// worth stating rather than leaving as an absence. Each compared the shared
// table against the router the attributes generated; the router is now BUILT
// from that table, so each would compare a value with itself:
//
//   * `the_shared_tool_table_matches_the_router`
//   * `every_description_in_the_table_is_the_one_the_router_publishes`
//   * `every_tool_calls_the_method_the_shared_table_pairs_it_with`
//   * `param_drift_tests` (router schema vs protocol schema)
//
// **A tautological test is worse than a deleted one** — it keeps reporting
// green from a position where it cannot fail, and the suite's count hides that
// nothing is checked.
//
// The risk they covered did not vanish with them. It moved to
// `crates/core/tests/capability_skew.rs`, which probes every method in the
// table against a LIVE daemon and is now the only check that reaches outside
// the protocol crate — and to `crates/core/tests/schema_matches_daemon.rs`,
// which holds the declared values against the real parsers.

#[cfg(test)]
mod forwarding_route_tests {
    use super::{forwarding_route, router_from_manifest, GelfMcpServer};
    use logmon_broker_protocol::mcp_tools::{self, CliHints, ManifestEntry};
    use rmcp::handler::server::tool::ToolRouter;

    fn router_with(entry: &ManifestEntry) -> ToolRouter<GelfMcpServer> {
        let mut r: ToolRouter<GelfMcpServer> = Default::default();
        r.add_route(forwarding_route(entry).expect("a schema-carrying entry"));
        r
    }

    fn real(name: &str) -> ManifestEntry {
        mcp_tools::manifest()
            .into_iter()
            .find(|e| e.name == name)
            .expect("in the manifest")
    }

    /// The claim the architecture rests on: a manifest entry alone is a tool.
    #[test]
    fn a_manifest_entry_alone_is_enough_to_register_a_working_tool() {
        let entry = real("add_collector");
        let listed = router_with(&entry).list_all();
        assert_eq!(listed.len(), 1);

        let tool = &listed[0];
        assert_eq!(tool.name, "add_collector");
        assert_eq!(
            tool.description.as_deref(),
            Some(entry.description.as_str()),
            "an agent must read the sentence the manifest carried"
        );

        let props = tool.input_schema["properties"]
            .as_object()
            .expect("parameters are described");
        assert!(props.contains_key("filter"));
        assert!(
            props["level"]["enum"]
                .as_array()
                .is_some_and(|v| v.iter().any(|x| x == "tree")),
            "accepted values must survive the trip: {}",
            props["level"]
        );
        // Resolved, so an agent can fill in the nested object.
        assert!(
            props["threshold"].to_string().contains("metric"),
            "a nested parameter must arrive resolved: {}",
            props["threshold"]
        );
    }

    /// `$schema` and `title` describe the file the definition was cut from, not
    /// the arguments. Left in, every tool claims to be titled after a Rust type.
    #[test]
    fn generator_provenance_is_stripped_from_the_published_schema() {
        let listed = router_with(&real("get_status")).list_all();
        let schema = &listed[0].input_schema;
        assert!(!schema.contains_key("$schema"), "{schema:?}");
        assert!(!schema.contains_key("title"), "{schema:?}");
    }

    /// A local output path is consumed by this process, but an agent still has
    /// to be able to *ask* for it — so it is published even though the daemon's
    /// request type does not describe it.
    #[test]
    fn a_local_output_path_is_offered_even_though_the_daemon_has_no_such_parameter() {
        let entry = real("export_logs");
        assert_eq!(
            entry
                .cli
                .file_output
                .as_ref()
                .map(|f| f.path_param.as_str()),
            Some("path")
        );

        let listed = router_with(&entry).list_all();
        let props = listed[0].input_schema["properties"]
            .as_object()
            .expect("parameters");
        assert!(
            props.contains_key("path"),
            "an agent cannot say where the file goes: {props:?}"
        );
        assert!(
            props["path"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("never sent")),
            "and must be told it is local: {}",
            props["path"]
        );

        // The daemon's own request type still does not describe it, which is
        // why publishing it has to be deliberate rather than inherited.
        let daemon_side = entry.input_schema.as_ref().unwrap()["properties"]
            .as_object()
            .unwrap();
        assert!(!daemon_side.contains_key("path"));
    }

    /// Every tool the manifest describes must be buildable, or the shim offers
    /// a smaller surface than the daemon has and says nothing about it.
    #[test]
    fn the_whole_manifest_becomes_a_router() {
        let entries = mcp_tools::manifest();
        let (router, skipped) = router_from_manifest(&entries);
        assert!(skipped.is_empty(), "not buildable: {skipped:?}");
        assert_eq!(router.list_all().len(), entries.len());
    }

    /// A tool this build never heard of is registered from its description
    /// alone — the property that makes "enhance the daemon without reinstalling
    /// the shim" true.
    #[test]
    fn a_tool_this_build_never_heard_of_is_registered() {
        let invented = ManifestEntry {
            name: "a_future_tool".into(),
            method: "future.thing".into(),
            description: "Something this shim was not compiled with".into(),
            input_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "target": { "type": "string" } }
            })),
            cli: CliHints::default(),
        };
        let listed = router_with(&invented).list_all();
        assert_eq!(listed[0].name, "a_future_tool");
        assert_eq!(
            listed[0].description.as_deref(),
            Some("Something this shim was not compiled with")
        );
        assert!(listed[0].input_schema["properties"]
            .as_object()
            .is_some_and(|p| p.contains_key("target")));
    }

    /// An entry with no schema is skipped and *named*, not published with an
    /// empty one: "takes no arguments" and "nothing describes this" are
    /// different claims, and the first gets every call rejected.
    #[test]
    fn a_schemaless_entry_is_skipped_and_reported() {
        let bare = ManifestEntry {
            name: "undescribed".into(),
            method: "x.y".into(),
            description: "d".into(),
            input_schema: None,
            cli: CliHints::default(),
        };
        assert!(forwarding_route(&bare).is_none());
        let (router, skipped) = router_from_manifest(&[bare]);
        assert_eq!(skipped, ["undescribed"]);
        assert!(router.list_all().is_empty());
    }
}

#[cfg(test)]
mod instructions_tests {
    #[test]
    fn skill_instructions_is_embedded_and_non_empty() {
        assert!(
            super::SKILL_INSTRUCTIONS.len() > 500,
            "the skill ships as MCP instructions; an empty one silently removes \
             every piece of guidance a client would have surfaced"
        );
        assert!(super::SKILL_INSTRUCTIONS.contains("logmon"));
    }
}
