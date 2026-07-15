//! Shared output formatters for CLI mode.

use logmon_broker_protocol::Level;
use serde::Serialize;

/// Default truncation thresholds for block-formatted output. Tuned to stay
/// well under typical Claude `Bash` output budgets.
pub const DEFAULT_MAX_BLOCK_RECORDS: usize = 50;
pub const DEFAULT_MAX_BLOCK_BYTES: usize = 16 * 1024;

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

/// Combine pre-formatted record blocks into a single human-readable string,
/// applying record-count and byte-count limits with a "... N more" marker
/// when truncation occurs.
pub fn truncate_blocks(blocks: Vec<String>, max_records: usize, max_bytes: usize) -> String {
    let total = blocks.len();
    let mut out = String::new();
    let mut emitted_records = 0usize;

    for (i, block) in blocks.iter().enumerate() {
        if emitted_records >= max_records {
            break;
        }
        // Check byte limit before adding the next block + separator.
        let separator_len = if i > 0 { 2 } else { 0 }; // "\n\n"
        if !out.is_empty() && out.len() + separator_len + block.len() > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(block);
        emitted_records += 1;
    }

    let remaining = total.saturating_sub(emitted_records);
    if remaining > 0 {
        out.push_str(&format!(
            "\n\n... {remaining} more record{plural}, use --json or refine the filter to see them",
            plural = if remaining == 1 { "" } else { "s" },
        ));
    }
    out
}

/// Print a list of pre-formatted blocks to stdout with default truncation.
/// When `blocks` is empty, prints `empty_marker` instead of a blank line so
/// human callers see e.g. `(no logs)` rather than nothing.
pub fn print_blocks(blocks: Vec<String>, empty_marker: &str) {
    if blocks.is_empty() {
        println!("{empty_marker}");
        return;
    }
    let out = truncate_blocks(blocks, DEFAULT_MAX_BLOCK_RECORDS, DEFAULT_MAX_BLOCK_BYTES);
    println!("{out}");
}

/// Print a comfy-table to stdout. Caller passes pre-stringified cells.
pub fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    use comfy_table::{ContentArrangement, Table};

    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(headers.iter().copied());

    for row in rows {
        table.add_row(row);
    }

    println!("{table}");
}

/// Render a log level as an uppercase static string. Shared by every CLI
/// renderer that prints log lines so the wire-level surface stays uniform.
pub fn format_level(l: &Level) -> &'static str {
    match l {
        Level::Trace => "TRACE",
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    }
}

/// Print an error message in the appropriate format. In `--json` mode emits
/// `{"error":"..."}` to stdout (so jq pipelines see structured output);
/// otherwise emits `error: ...` to stderr (UNIX convention for human users).
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
    fn json_emit_pretty_serializes_value() {
        let v = json!({"a": 1, "b": [2, 3]});
        let out = json_string(&v);
        // Pretty-printed output spans multiple lines.
        assert!(out.contains('\n'));
        assert!(out.contains("\"a\": 1"));
    }

    #[test]
    fn truncate_blocks_under_limit_unchanged() {
        let blocks = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = truncate_blocks(blocks.clone(), 100, 100);
        assert_eq!(out, "a\n\nb\n\nc");
    }

    #[test]
    fn truncate_blocks_over_record_limit_appends_more_marker() {
        let blocks = (0..10).map(|i| format!("rec-{i}")).collect();
        let out = truncate_blocks(blocks, 5, usize::MAX);
        // First 5 records present, last 5 hidden behind marker.
        assert!(out.contains("rec-0"));
        assert!(out.contains("rec-4"));
        assert!(!out.contains("rec-5"));
        assert!(out.contains("... 5 more"));
    }

    #[test]
    fn truncate_blocks_over_byte_limit_truncates_records() {
        // 20 records of ~10 chars each = ~200 bytes; cap at 50 bytes.
        let blocks: Vec<String> = (0..20).map(|i| format!("record_{i:02}")).collect();
        let out = truncate_blocks(blocks, usize::MAX, 50);
        assert!(out.contains("record_00"));
        assert!(out.contains("more"));
        assert!(out.len() < 200);
    }

    #[test]
    fn error_human_writes_to_stderr_format() {
        // We can't easily capture stderr in a unit test; just ensure it doesn't panic.
        error("test error", false);
    }

    #[test]
    fn format_level_uppercase_for_all_variants() {
        assert_eq!(format_level(&Level::Trace), "TRACE");
        assert_eq!(format_level(&Level::Debug), "DEBUG");
        assert_eq!(format_level(&Level::Info), "INFO");
        assert_eq!(format_level(&Level::Warn), "WARN");
        assert_eq!(format_level(&Level::Error), "ERROR");
    }
}
