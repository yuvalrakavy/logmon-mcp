//! Execution-attributed observations are counted at ingest. Ring snapshots
//! separately preserve the source records needed to investigate a verdict.
mod capture;
mod summary;
pub(crate) use capture::CapturePool;
pub use capture::CaptureSnapshot;
pub use summary::{Selector, Summary};

#[cfg(test)]
mod tests;
