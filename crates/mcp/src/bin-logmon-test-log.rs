//! Offline inspection of nightly JSONL after its broker domain is gone.
mod nightly_report;

use anyhow::Result;
use clap::{Parser, Subcommand};
use logmon_broker_sdk::test_log_file::{compare_summaries, TestLogFile};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "logmon-test-log",
    about = "Read one local nightly test-log, or compare two executions"
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Read one execution's latest summary, timing and case disposition.
    Lookup {
        file: PathBuf,
        execution: String,
        /// Select by stable name when there is only one matching attempt.
        #[arg(long)]
        by_name: bool,
    },
    /// Export the latest revisions for the whole run, with gap and settlement diagnostics.
    Export { file: PathBuf },
    /// Render the morning report from the journal, including provisional results.
    Summary {
        file: PathBuf,
        #[arg(long, default_value_t = 0)]
        unresolved_ends: usize,
    },
    /// Publish a settled journal and its expanded cases as one portable ZIP.
    Archive {
        file: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 0)]
        unresolved_ends: usize,
    },
    /// Compare observed counters, span durations and collected attributes.
    Compare {
        before_file: PathBuf,
        before: String,
        after_file: PathBuf,
        after: String,
        /// Resolve both selectors as stable names instead of execution IDs.
        #[arg(long)]
        by_name: bool,
    },
}

fn main() -> Result<()> {
    let value = match Cli::parse().action {
        Action::Summary {
            file,
            unresolved_ends,
        } => {
            let report = nightly_report::Report::open(&file, unresolved_ends)?;
            print!("{}", report.markdown());
            return Ok(());
        }
        Action::Archive {
            file,
            output,
            unresolved_ends,
        } => {
            nightly_report::archive(&file, &output, unresolved_ends)?;
            serde_json::json!({"archive":output})
        }
        Action::Export { file } => serde_json::to_value(TestLogFile::open(&file)?)?,
        Action::Lookup {
            file,
            execution,
            by_name,
        } => {
            let log = TestLogFile::open(&file)?;
            let mut result = serde_json::to_value(if by_name {
                log.lookup_name(&execution)?
            } else {
                log.lookup(&execution)?
            })?;
            result["test_log_status"] = log.journal_status();
            result
        }
        Action::Compare {
            before_file,
            before,
            after_file,
            after,
            by_name,
        } => {
            let a = TestLogFile::open(&before_file)?;
            let b = TestLogFile::open(&after_file)?;
            let (x, y) = if by_name {
                (a.lookup_name(&before)?, b.lookup_name(&after)?)
            } else {
                (a.lookup(&before)?, b.lookup(&after)?)
            };
            let mut result = compare_summaries(x, y);
            result["before"]["test_log_status"] = a.journal_status();
            result["after"]["test_log_status"] = b.journal_status();
            result
        }
    };
    serde_json::to_writer_pretty(std::io::stdout().lock(), &value)?;
    println!();
    Ok(())
}
