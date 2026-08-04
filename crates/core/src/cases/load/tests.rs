use super::*;

const STEM: &str = "with-spans-260803-214501";

fn fixture_dir() -> PathBuf {
    PathBuf::from(format!("{}/tests/fixtures/cases", env!("CARGO_MANIFEST_DIR")))
}

/// Copy the fixture into a temp dir so a test can doctor it without touching
/// the committed bytes.
fn scratch(stem: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let [md, logdata, spandata] = super::super::naming::file_names(stem);
    for n in [&md, &logdata, &spandata] {
        std::fs::copy(fixture_dir().join(n), dir.path().join(n)).unwrap();
    }
    let md_path = dir.path().join(&md);
    (dir, md_path)
}

#[test]
fn a_real_case_loads_from_loose_files() {
    let (_d, md) = scratch(STEM);
    let c = load(&md).expect("the fixture must load");
    assert_eq!(c.stem, STEM);
    assert_eq!(c.logs.as_ref().expect("declared").len(), 9);
    assert_eq!(c.spans.as_ref().expect("declared").len(), 3);
    assert_eq!(c.from_seq(), 1001);
    assert_eq!(c.to_seq(), 1012);
    assert!(
        c.registry.is_none(),
        "the fixture predates the registry file, which reads as absent"
    );
}

#[test]
fn a_real_case_loads_from_a_bundle() {
    let (_d, md) = scratch(STEM);
    let dir = md.parent().unwrap();
    let [md_n, log_n, span_n] = super::super::naming::file_names(STEM);
    let entries: Vec<(String, Vec<u8>)> = [md_n, log_n, span_n]
        .into_iter()
        .map(|n| {
            let b = std::fs::read(dir.join(&n)).unwrap();
            (n, b)
        })
        .collect();
    let zip_path = dir.join(bundle::bundle_name(STEM));
    let f = std::fs::File::create(&zip_path).unwrap();
    bundle::pack(f, STEM, &entries, true).unwrap();

    let c = load(&zip_path).expect("a bundle must load the same as loose files");
    assert_eq!(c.logs.as_ref().unwrap().len(), 9);
    assert_eq!(c.spans.as_ref().unwrap().len(), 3);
}

#[test]
fn a_relative_path_is_refused() {
    assert!(matches!(
        load(Path::new("some/case.md")),
        Err(LoadError::NotAbsolute(_))
    ));
}

#[test]
fn a_foreign_evidence_pointer_is_refused() {
    // The daemon follows these, so they are checked against the case's own stem
    // rather than sanitised. `yaml_str` round-trips `../../etc/passwd`
    // perfectly — a string codec is not a path validator.
    let (_d, md) = scratch(STEM);
    let doc = std::fs::read_to_string(&md).unwrap().replace(
        &format!("{STEM}.logdata.jsonl"),
        "../../../../etc/passwd",
    );
    std::fs::write(&md, doc).unwrap();
    match load(&md) {
        Err(LoadError::ForeignPointer { key, .. }) => assert_eq!(key, "logdata"),
        other => panic!("expected ForeignPointer, got {other:?}"),
    }
}

#[test]
fn a_declared_but_missing_file_is_corruption_not_omission() {
    let (_d, md) = scratch(STEM);
    let [_, logdata, _] = super::super::naming::file_names(STEM);
    std::fs::remove_file(md.parent().unwrap().join(&logdata)).unwrap();
    assert!(
        matches!(load(&md), Err(LoadError::DeclaredButMissing(_))),
        "a declared file that is gone must not read as `omitted at capture`"
    );
}

#[test]
fn an_absent_key_with_no_file_is_omission() {
    let (_d, md) = scratch(STEM);
    let [_, _, spandata] = super::super::naming::file_names(STEM);
    std::fs::remove_file(md.parent().unwrap().join(&spandata)).unwrap();
    let doc = std::fs::read_to_string(&md)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("spandata:"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&md, doc).unwrap();

    let c = load(&md).expect("an omitted-evidence case is a valid case");
    assert!(c.spans.is_none(), "omitted reads as None");
    assert!(c.logs.is_some(), "and the other half still loads");
}

#[test]
fn swapped_data_files_are_caught_by_the_kind_check() {
    // Both files are format 1, so the version rule cannot see this. Without the
    // `kind` check the case loads cleanly with logs in the span store.
    let (_d, md) = scratch(STEM);
    let dir = md.parent().unwrap();
    let [_, logdata, spandata] = super::super::naming::file_names(STEM);
    let l = std::fs::read(dir.join(&logdata)).unwrap();
    let s = std::fs::read(dir.join(&spandata)).unwrap();
    std::fs::write(dir.join(&logdata), &s).unwrap();
    std::fs::write(dir.join(&spandata), &l).unwrap();

    assert!(
        matches!(load(&md), Err(LoadError::BadHeader(_))),
        "a swapped pair must be named as such, not parsed as itself"
    );
}

#[test]
fn a_truncated_evidence_file_is_caught_by_its_declared_count() {
    let (_d, md) = scratch(STEM);
    let [_, logdata, _] = super::super::naming::file_names(STEM);
    let p = md.parent().unwrap().join(&logdata);
    let text = std::fs::read_to_string(&p).unwrap();
    let kept: Vec<&str> = text.lines().take(5).collect(); // header + 4 of 9
    std::fs::write(&p, kept.join("\n")).unwrap();

    match load(&md) {
        Err(LoadError::CountMismatch {
            declared, parsed, ..
        }) => assert_eq!((declared, parsed), (9, 4)),
        other => panic!("expected CountMismatch, got {other:?}"),
    }
}

#[test]
fn a_headerless_evidence_file_is_refused() {
    // The writer emits a header even for zero records, so its absence is
    // positive evidence of truncation rather than an empty capture.
    let (_d, md) = scratch(STEM);
    let [_, logdata, _] = super::super::naming::file_names(STEM);
    let p = md.parent().unwrap().join(&logdata);
    std::fs::write(&p, "").unwrap();
    assert!(matches!(load(&md), Err(LoadError::BadHeader(_))));
}

#[test]
fn out_of_window_and_duplicate_seqs_are_refused() {
    let (_d, md) = scratch(STEM);
    let [_, logdata, _] = super::super::naming::file_names(STEM);
    let p = md.parent().unwrap().join(&logdata);
    let text = std::fs::read_to_string(&p).unwrap();

    // A duplicate: the deque keeps both and `seq_set` keeps one, after which
    // `len()` and `contains_seq` disagree and every windowed read is wrong.
    let dup = text.replace("\"seq\":1002", "\"seq\":1001");
    std::fs::write(&p, &dup).unwrap();
    assert!(
        matches!(load(&md), Err(LoadError::BadRecords(_))),
        "a duplicate seq must be refused"
    );

    // Outside the declared window.
    let outside = text.replace("\"seq\":1002", "\"seq\":99999");
    std::fs::write(&p, &outside).unwrap();
    assert!(
        matches!(load(&md), Err(LoadError::BadRecords(_))),
        "a seq outside the declared window must be refused"
    );
}

#[test]
fn a_newer_evidence_format_is_refused() {
    let (_d, md) = scratch(STEM);
    let [_, logdata, _] = super::super::naming::file_names(STEM);
    let p = md.parent().unwrap().join(&logdata);
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replacen(r#""logmon_format":1"#, r#""logmon_format":2"#, 1);
    std::fs::write(&p, text).unwrap();
    assert!(matches!(load(&md), Err(LoadError::BadHeader(_))));
}
