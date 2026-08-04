use super::*;
use std::io::Cursor;

const STEM: &str = "checkout-hang-260803-214121";

fn parts() -> Vec<(String, Vec<u8>)> {
    let [md, logdata, spandata] = file_names(STEM);
    vec![
        (md, b"---\ncase: x\n---\n".to_vec()),
        (logdata, b"{\"logmon_format\":1,\"kind\":\"logdata\"}\n".to_vec()),
        (spandata, b"{\"logmon_format\":1,\"kind\":\"spandata\"}\n".to_vec()),
        (registry_entry_name(STEM), b"{\"facts\":[]}".to_vec()),
    ]
}

fn round_trip(compressed: bool) -> CaseBytes {
    let mut buf = Cursor::new(Vec::new());
    pack(&mut buf, STEM, &parts(), compressed).expect("pack");
    buf.set_position(0);
    unpack(buf, STEM).expect("unpack")
}

#[test]
fn a_bundle_round_trips_compressed_and_stored() {
    for compressed in [true, false] {
        let c = round_trip(compressed);
        assert_eq!(c.stem, STEM);
        assert!(c.document.starts_with(b"---"), "compressed={compressed}");
        assert!(c.logdata.is_some());
        assert!(c.spandata.is_some());
        assert!(c.registry.is_some());
    }
}

#[test]
fn omitted_evidence_is_an_absent_entry() {
    // The same rule as the front matter's absent key: nothing to read is a real
    // answer, distinct from an entry holding only its header.
    let [md, _, spandata] = file_names(STEM);
    let some = vec![
        (md, b"---\ncase: x\n---\n".to_vec()),
        // spandata present but empty of records — "we captured none"
        (spandata, b"{\"logmon_format\":1,\"kind\":\"spandata\"}\n".to_vec()),
    ];
    let mut buf = Cursor::new(Vec::new());
    pack(&mut buf, STEM, &some, true).unwrap();
    buf.set_position(0);
    let c = unpack(buf, STEM).unwrap();

    assert!(c.logdata.is_none(), "absent entry reads as omitted");
    assert!(
        c.spandata.is_some(),
        "a header-only entry is `we captured none`, NOT omitted"
    );
}

#[test]
fn a_bundle_without_its_document_is_refused() {
    let [_, logdata, _] = file_names(STEM);
    let mut buf = Cursor::new(Vec::new());
    pack(&mut buf, STEM, &[(logdata, b"x".to_vec())], true).unwrap();
    buf.set_position(0);
    assert!(
        matches!(unpack(buf, STEM), Err(BundleError::MissingEntry(_))),
        "evidence without a document is not a case"
    );
}

#[test]
fn a_traversal_entry_is_refused_by_the_allowlist() {
    // The reason for an allowlist rather than a sanitiser: this is checked
    // BEFORE the bytes reach any parser, and it cannot be defeated by a spelling
    // the blocklist did not anticipate.
    for hostile in [
        "../../../../etc/passwd",
        "/etc/passwd",
        "..\\..\\windows\\system32",
        "other-case-260101-000000.md",
        "checkout-hang-260803-214121.md/../evil",
    ] {
        let mut buf = Cursor::new(Vec::new());
        let r = pack(&mut buf, STEM, &[(hostile.to_string(), b"x".to_vec())], true);
        assert!(
            matches!(r, Err(BundleError::ForeignEntry(_))),
            "packing {hostile} should be refused, got {r:?}"
        );
    }
}

#[test]
fn a_hostile_entry_smuggled_into_an_archive_is_refused_on_read() {
    // pack() is ours; unpack() reads whatever arrived by scp. Build the archive
    // with the writer bypassed so the read path is what is under test.
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        let [md, _, _] = file_names(STEM);
        zw.start_file(md.as_str(), opts).unwrap();
        zw.write_all(b"---\ncase: x\n---\n").unwrap();
        zw.start_file("../../../../etc/cron.d/pwn", opts).unwrap();
        zw.write_all(b"* * * * * root sh").unwrap();
        zw.finish().unwrap();
    }
    buf.set_position(0);
    assert!(
        matches!(unpack(buf, STEM), Err(BundleError::ForeignEntry(_))),
        "a foreign entry must be refused before its bytes reach a parser"
    );
}

#[test]
fn compression_actually_compresses_real_evidence() {
    // The measurement that justified reversing the no-codec decision. If this
    // ratio ever collapses, the reversal should be revisited rather than kept
    // out of habit.
    let logdata = std::fs::read(format!(
        "{}/tests/fixtures/cases/with-spans-260803-214501.logdata.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    // One capture's worth of the same shape, which is what a real ring holds.
    let mut bulk = Vec::new();
    for _ in 0..200 {
        bulk.extend_from_slice(&logdata);
    }
    let [_, name, _] = file_names(STEM);
    let [md, _, _] = file_names(STEM);

    let mut small = Cursor::new(Vec::new());
    let deflated = pack(
        &mut small,
        STEM,
        &[
            (md.clone(), b"---\n---\n".to_vec()),
            (name.clone(), bulk.clone()),
        ],
        true,
    )
    .unwrap();

    let mut big = Cursor::new(Vec::new());
    let stored = pack(
        &mut big,
        STEM,
        &[(md, b"---\n---\n".to_vec()), (name, bulk)],
        false,
    )
    .unwrap();

    let ratio = stored as f64 / deflated as f64;
    assert!(
        ratio > 5.0,
        "structured log evidence should compress hard; got {ratio:.1}x \
         ({stored} stored vs {deflated} deflated)"
    );
}

#[test]
fn a_bundle_path_names_its_stem() {
    assert_eq!(
        stem_of_bundle(Path::new("/tmp/checkout-hang-260803-214121.case.zip")),
        Some(STEM.to_string())
    );
    assert_eq!(stem_of_bundle(Path::new("/tmp/notacase.md")), None);
    assert_eq!(stem_of_bundle(Path::new("/tmp/x.zip")), None);
}
