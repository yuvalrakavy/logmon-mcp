//! Case documents — spec §5.
//!
//! A case is three files sharing one stem: a markdown **document** and two
//! JSONL **logdata** files.
//!
//! ```text
//! checkout-hang-260731-021530.md
//! checkout-hang-260731-021530.logdata.jsonl     ← log entries
//! checkout-hang-260731-021530.spandata.jsonl    ← spans
//! ```
//!
//! **The split, stated as a rule:** the document is what you read to decide
//! whether this is your bug and what to do about it; the logdata is the evidence
//! you consult once you have decided it is. The test for any future field is
//! which of those two jobs it serves.
//!
//! Two logdata files rather than one, because "sidecar" named a *role* and said
//! nothing about contents. Logs and spans are different shapes with different
//! consumers, and wanting one without the other is the common case — one mixed
//! stream would make every consumer filter by record kind before doing anything.
//!
//! Not compressed, and that is a decision rather than an omission: §2's boundary
//! is that the format is the contract and indexing belongs to whatever walks the
//! directory, so putting a codec between the archive and `grep` costs more than
//! the bytes are worth. Anyone wanting the space back has filesystem-level
//! compression, which is transparent, and gzipping cold files later, which needs
//! no change to this contract.

pub mod bundle;
pub mod document;
pub mod naming;
pub mod read;
pub mod write;

pub use naming::{file_names, resolve_prefix, roll_id, stem, MAX_PREFIX_BYTES};
pub use read::{parse_front_matter, FrontMatter, ParseError};
pub use write::{
    claim, claim_bundle, write_document, write_jsonl, CaseBundleFile, CaseFileStats, CaseFiles,
    CaseWriteError,
};

/// The archive's record format. Carried as the first line of each JSONL file
/// and in the document's front-matter.
///
/// It exists because §2 rules out any query engine over the archive, so the
/// format *is* the contract a reader has years later. The collector path
/// already carries a `FORMAT_VERSION` for the same reason.
pub const FORMAT_VERSION: u32 = 1;
