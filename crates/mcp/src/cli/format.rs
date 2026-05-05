//! Shared output formatters for CLI mode.
//! Full implementation lands in Task 2; this stub is enough to compile.

/// Print an error message in the appropriate format.
pub fn error(message: &str, json: bool) {
    if json {
        let v = serde_json::json!({ "error": message });
        if let Ok(s) = serde_json::to_string_pretty(&v) {
            println!("{s}");
        } else {
            println!("{{\"error\":\"<unprintable>\"}}");
        }
    } else {
        eprintln!("error: {message}");
    }
}
