//! One case, one file.
//!
//! Three files sharing a stem desync or lose a sibling in transit, and moving
//! them between machines is the scenario the whole feature exists for. So a case
//! travels as a single `<stem>.case.zip`.
//!
//! **This reverses a recorded decision, and the reason it may be reversed is the
//! reason it was recorded.** [`super`]'s module docs decline compression because
//! *"indexing belongs to whatever walks the directory, so putting a codec between
//! the archive and `grep` costs more than the bytes are worth."* `grep` is no
//! longer the consumer: [`super::read`] reads a case and an external document
//! store manages the collection. The premise expired.
//!
//! Measured on real fixture bytes at 5000 records: **18.9×** when every message
//! is unique, **69.5×** on repetitive service traffic. Even the floor turns a
//! 1.9 MB capture into 100 KB, which is what makes email a real transport rather
//! than a hypothetical one.
//!
//! **Entries are stem-prefixed**, so `unzip` reproduces exactly the loose layout:
//! the bundle is a container, not a second format. A reader accepts either.

use std::io::{Read, Seek, Write};
use std::path::Path;

use super::naming::file_names;

/// What a case is called on disk when bundled.
pub const BUNDLE_SUFFIX: &str = ".case.zip";

/// Ceiling on one entry, declared or actual.
///
/// A 5000-record capture is ~2 MB uncompressed, so this is two orders of
/// magnitude of headroom over anything logmon writes — it exists to bound what
/// an archive from elsewhere can ask this process to allocate, not to constrain
/// legitimate cases.
pub const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

/// The fourth entry — provenance as data. Not a loose sibling: it exists because
/// the bundle gave the registry somewhere to live that the document's own size
/// budget does not govern.
pub fn registry_entry_name(stem: &str) -> String {
    format!("{stem}.registry.json")
}

/// The bundle's own file name for a stem.
pub fn bundle_name(stem: &str) -> String {
    format!("{stem}{BUNDLE_SUFFIX}")
}

#[derive(Debug)]
pub enum BundleError {
    Io(std::io::Error),
    Zip(String),
    /// An entry the front matter declares but the archive does not hold.
    MissingEntry(String),
    /// An entry name that does not belong to this case's stem. **Refused rather
    /// than resolved**: an archive that arrived from another machine must not be
    /// able to name a path, and `..` inside a zip entry is the oldest trick
    /// there is.
    ForeignEntry(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Zip(e) => write!(f, "{e}"),
            Self::MissingEntry(n) => write!(
                f,
                "the case declares `{n}` but the bundle does not contain it — \
                 the archive is truncated or was assembled by hand"
            ),
            Self::ForeignEntry(n) => write!(
                f,
                "bundle entry `{n}` does not belong to this case's stem; a case \
                 bundle holds exactly its own four files and nothing else"
            ),
        }
    }
}

impl std::error::Error for BundleError {}

impl From<std::io::Error> for BundleError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<zip::result::ZipError> for BundleError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e.to_string())
    }
}

/// The entries a bundle may hold, and nothing else.
///
/// **An allowlist rather than a sanitiser.** Rejecting `..` and absolute paths
/// is a blocklist, and blocklists on path syntax are a losing game; a case knows
/// exactly the four names it can contain, so anything else is refused by name.
fn permitted(stem: &str) -> [String; 4] {
    let [md, logdata, spandata] = file_names(stem);
    [md, logdata, spandata, registry_entry_name(stem)]
}

/// Write a bundle. `entries` is `(name, bytes)`; every name must belong to `stem`.
pub fn pack<W: Write + Seek>(
    out: W,
    stem: &str,
    entries: &[(String, Vec<u8>)],
    compressed: bool,
) -> Result<u64, BundleError> {
    let allowed = permitted(stem);
    let mut zw = zip::ZipWriter::new(out);
    // Stored is a real choice, not a debug flag: it keeps `unzip -p` cheap for
    // whoever is walking an archive, which is what the original no-codec
    // decision was protecting.
    let method = if compressed {
        zip::CompressionMethod::Deflated
    } else {
        zip::CompressionMethod::Stored
    };
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(method);

    for (name, bytes) in entries {
        if !allowed.contains(name) {
            return Err(BundleError::ForeignEntry(name.clone()));
        }
        zw.start_file(name.as_str(), opts)?;
        zw.write_all(bytes)?;
    }
    let mut w = zw.finish()?;
    w.flush()?;
    Ok(w.stream_position()?)
}

/// A case's bytes, however they arrived.
pub struct CaseBytes {
    pub stem: String,
    pub document: Vec<u8>,
    /// `None` when the capture omitted this evidence — see the front matter's
    /// absent key. A declared-but-missing entry is [`BundleError::MissingEntry`],
    /// never `None`.
    pub logdata: Option<Vec<u8>>,
    pub spandata: Option<Vec<u8>>,
    pub registry: Option<Vec<u8>>,
}

/// Read a bundle.
///
/// Every entry is checked against the stem's allowlist **before** it is read, so
/// a hostile archive cannot get its bytes in front of a parser by naming them
/// something else.
pub fn unpack<R: Read + Seek>(src: R, stem: &str) -> Result<CaseBytes, BundleError> {
    let allowed = permitted(stem);
    let mut zr = zip::ZipArchive::new(src)?;
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();

    for i in 0..zr.len() {
        let mut e = zr.by_index(i)?;
        // `name` is the raw archive path; the allowlist is exact-match, so a
        // traversal attempt fails here rather than being normalised into
        // something that looks fine.
        let name = e.name().to_string();
        if !allowed.contains(&name) {
            return Err(BundleError::ForeignEntry(name));
        }
        // **`e.size()` is the header's CLAIM, not a measurement**, and the
        // header arrived from another machine. Sizing an allocation from it is
        // the exact hazard `load::MAX_CASE_RECORDS` documents one module over:
        // `u64::MAX` panics on `capacity overflow`, and a plausible-looking
        // 16 GiB asks for more than the host will give and aborts the process,
        // taking every live domain's buffers with it. A truncated transfer
        // produces a garbage size field without anyone being hostile.
        //
        // So the declared size is a HINT bounded by a real ceiling, and the read
        // is capped independently — a deflate bomb declares a small size and
        // expands anyway.
        if e.size() > MAX_ENTRY_BYTES {
            return Err(BundleError::Zip(format!(
                "entry `{name}` declares {} bytes, above the {MAX_ENTRY_BYTES}-byte ceiling",
                e.size()
            )));
        }
        let hint = e.size().min(1 << 20) as usize;
        let mut buf = Vec::with_capacity(hint);
        let read = e.by_ref().take(MAX_ENTRY_BYTES + 1).read_to_end(&mut buf)?;
        if read as u64 > MAX_ENTRY_BYTES {
            return Err(BundleError::Zip(format!(
                "entry `{name}` expands past the {MAX_ENTRY_BYTES}-byte ceiling; \
                 a small entry that decompresses without bound is a bomb, not a case"
            )));
        }
        found.push((name, buf));
    }

    let take = |n: &str| -> Option<Vec<u8>> {
        found
            .iter()
            .find(|(name, _)| name == n)
            .map(|(_, b)| b.clone())
    };
    let [md, logdata, spandata] = file_names(stem);
    let document = take(&md).ok_or_else(|| BundleError::MissingEntry(md.clone()))?;

    Ok(CaseBytes {
        stem: stem.to_string(),
        document,
        logdata: take(&logdata),
        spandata: take(&spandata),
        registry: take(&registry_entry_name(stem)),
    })
}

/// The stem a bundle path names, or `None` if it is not a bundle path.
pub fn stem_of_bundle(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(BUNDLE_SUFFIX).map(str::to_string)
}

#[cfg(test)]
mod tests;
