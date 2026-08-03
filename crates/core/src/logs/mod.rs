//! Projections over the log store — the log-side counterpart to
//! `collector::project`.
//!
//! Separate from `store`, which owns retention and retrieval. These modules
//! answer questions *about* a population rather than returning records from it,
//! and they all read through `InMemoryStore::for_each_matching` so that a
//! description of "the buffer" is a description of the whole buffer.
//!
//! `fields` is the map (which dimensions exist); the profile that measures
//! along one of them lands here beside it.

pub mod fields;
