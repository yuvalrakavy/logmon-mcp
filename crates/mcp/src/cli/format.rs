//! The CLI's output helpers — what is left of them.
//!
//! This module used to hold record-block truncation, table layout and level
//! colouring, all of it shaped around particular logmon result types. Those
//! went with the hand-written commands: rendering a result requires knowing
//! what the result *is*, which is the knowledge a daemon-taught shim does not
//! have. Presentation now belongs to the daemon, which may return a `_display`
//! string alongside its structured reply.
//!
//! What survives is the one thing that is genuinely about this process rather
//! than about any tool: how to report that something went wrong.

use serde::Serialize;

/// Pretty-print a serializable value as JSON. Trailing newline included.
pub fn json_string<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string_pretty(value)
        .unwrap_or_else(|e| format!("{{\"error\":\"failed to serialize result: {e}\"}}"));
    s.push('\n');
    s
}

/// Print pretty JSON to stdout.
pub fn print_json<T: Serialize>(value: &T) {
    print!("{}", json_string(value));
}

/// Report a failure.
///
/// Goes to **stderr** in human mode so a shell pipeline reading stdout is not
/// fed an error as if it were data, and to **stdout as JSON** in `--json` mode
/// so a machine consumer finds it where it is already looking.
pub fn error(message: &str, json: bool) {
    if json {
        let v = serde_json::json!({ "error": message });
        print_json(&v);
    } else {
        eprintln!("error: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_output_ends_with_exactly_one_newline() {
        let s = json_string(&json!({ "a": 1 }));
        assert!(s.ends_with("}\n"));
        assert!(!s.ends_with("\n\n"));
    }

    /// A machine consumer reads stdout; a human reads stderr. Putting an error
    /// on the wrong one either corrupts a pipeline or hides the failure.
    #[test]
    fn an_error_is_serialisable_in_json_mode() {
        let s = json_string(&json!({ "error": "boom" }));
        let back: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(back["error"], "boom");
    }
}
