use super::*;
use crate::domain_data::store::{DataEntry, Outcome};

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn set(path: &str, value: &str) -> DataEntry {
    DataEntry {
        path: path.into(),
        value: Some(value.into()),
        ttl: None,
    }
}

/// A scratch config dir. Tests must never write into `~/.config/logmon`.
fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn open(dir: &Path, domain: &str) -> DomainRegistry {
    let path = path_for(dir, domain);
    DomainRegistry::new(domain.to_string(), path.clone(), load(&path))
}

// --- H9: the case-folding filesystem ----------------------------------------

#[test]
fn two_domains_differing_only_in_case_get_two_files() {
    // On APFS and on Windows, `Build.json` and `build.json` are one file, and
    // two distinct domains would silently overwrite each other's provenance.
    assert_ne!(file_name_for("Build"), file_name_for("build"));
    let a = file_name_for("Build").to_lowercase();
    let b = file_name_for("build").to_lowercase();
    assert_ne!(
        a, b,
        "the names must differ even after the filesystem folds them"
    );
}

#[test]
fn the_file_name_is_deterministic_across_runs() {
    assert_eq!(file_name_for("t3"), file_name_for("t3"));
    // Pinned, because this lands in a filename: a change orphans every registry
    // on disk. FNV-1a is fixed by definition, unlike DefaultHasher.
    assert_eq!(file_name_for("default"), "default-8620c5fe.json");
    assert_eq!(
        file_name_for("default").len(),
        "default-".len() + 8 + ".json".len()
    );
}

#[test]
fn the_registry_lives_in_its_own_subdirectory() {
    let dir = scratch();
    let p = path_for(dir.path(), "state");
    assert_eq!(p.parent().unwrap().file_name().unwrap(), DOMAIN_DATA_DIR);
    // `state` and `config` are legal domain names, and DaemonState parses any
    // JSON object successfully — a flat layout would lose every session.
    assert_ne!(p, dir.path().join("state.json"));
}

// --- §3.5: round-trip --------------------------------------------------------

#[test]
fn a_registry_survives_a_restart_with_both_timestamps() {
    let dir = scratch();
    {
        let reg = open(dir.path(), "t3");
        let (got, saved) =
            reg.mutate(|r| r.update(&[set("/Build/commit", "abc")], t("2026-07-31T12:00:00Z")));
        assert_eq!(got[0].outcome, Outcome::Created);
        saved.expect("must persist");
    }
    let reg = open(dir.path(), "t3");
    reg.read(|r| {
        let e = r.get("/Build/commit").expect("survives");
        assert_eq!(e.value, "abc");
        assert_eq!(e.created_at, t("2026-07-31T12:00:00Z"));
        assert_eq!(e.validated_at, t("2026-07-31T12:00:00Z"));
    });
}

#[test]
fn a_ttl_survives_a_restart() {
    let dir = scratch();
    {
        let reg = open(dir.path(), "t3");
        let (_, saved) = reg.mutate(|r| {
            r.update(
                &[DataEntry {
                    path: "/Action".into(),
                    value: Some("suite".into()),
                    ttl: Some(crate::domain_data::TtlSpec::Set(1800)),
                }],
                t("2026-07-31T12:00:00Z"),
            )
        });
        saved.unwrap();
    }
    let reg = open(dir.path(), "t3");
    reg.read(|r| assert_eq!(r.get("/Action").unwrap().ttl_secs, Some(1800)));
}

#[test]
fn a_missing_file_is_a_new_registry_not_an_error() {
    let dir = scratch();
    let reg = open(dir.path(), "never-written");
    reg.read(|r| assert!(r.entries().is_empty()));
}

// --- §3.5: corruption --------------------------------------------------------

#[test]
fn a_corrupt_file_is_quarantined_and_the_cause_is_recorded() {
    let dir = scratch();
    let path = path_for(dir.path(), "t3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ this is not json").unwrap();

    let reg = open(dir.path(), "t3");
    let cause = reg.read(|r| {
        let mut r = r.clone();
        r.update(
            &[DataEntry {
                path: "/a".into(),
                value: None,
                ttl: None,
            }],
            t("2026-07-31T12:00:00Z"),
        )
    });
    assert_eq!(
        cause[0].outcome,
        Outcome::Unknown {
            cause: crate::domain_data::UnknownCause::NoRegistry
        },
        "a quarantine artifact is the only evidence that tells a lost registry \
         from a new one"
    );
    let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("corrupt"))
        .collect();
    assert_eq!(
        siblings.len(),
        1,
        "the bad file is moved aside, not deleted"
    );
}

#[test]
fn a_file_from_a_newer_build_is_quarantined_rather_than_guessed_at() {
    let dir = scratch();
    let path = path_for(dir.path(), "t3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "format_version": FORMAT_VERSION + 1,
            "domain": "t3",
            "entries": {}
        }))
        .unwrap(),
    )
    .unwrap();

    let reg = open(dir.path(), "t3");
    reg.read(|r| assert!(r.entries().is_empty()));
}

#[test]
fn a_corrupt_registry_does_not_stop_the_domain_working() {
    let dir = scratch();
    let path = path_for(dir.path(), "t3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"garbage").unwrap();

    let reg = open(dir.path(), "t3");
    let (got, saved) = reg.mutate(|r| r.update(&[set("/a", "1")], t("2026-07-31T12:00:00Z")));
    assert_eq!(got[0].outcome, Outcome::Created);
    saved.expect("a fresh registry writes over the quarantined one");
}

// --- §3.5: the write path ----------------------------------------------------

#[test]
fn a_write_failure_is_returned_rather_than_swallowed() {
    // A caller that changed memory and could not durably record it must be able
    // to say so — the change will not survive a restart.
    let dir = scratch();
    let path = path_for(dir.path(), "t3");
    // Make the parent a *file*, so create_dir_all inside atomic_write fails.
    std::fs::create_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    std::fs::write(path.parent().unwrap(), b"not a directory").unwrap();

    let reg = DomainRegistry::new("t3".into(), path, Registry::new());
    let (got, saved) = reg.mutate(|r| r.update(&[set("/a", "1")], t("2026-07-31T12:00:00Z")));
    assert_eq!(got[0].outcome, Outcome::Created, "memory still changed");
    assert!(saved.is_err(), "and the loss is reported");
}

#[test]
fn concurrent_writers_do_not_land_out_of_order() {
    // The lock-order rule. Serialize before taking the mutex and the older
    // snapshot can land last, dropping the newer key from disk while memory
    // keeps serving it — invisible until a restart.
    use std::sync::Arc;
    let dir = scratch();
    let reg = Arc::new(open(dir.path(), "t3"));
    let n = 32;
    let threads: Vec<_> = (0..n)
        .map(|i| {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || {
                let (_, saved) = reg.mutate(|r| {
                    r.update(&[set(&format!("/k{i:02}"), "v")], t("2026-07-31T12:00:00Z"))
                });
                saved.expect("each write must succeed");
            })
        })
        .collect();
    for th in threads {
        th.join().unwrap();
    }

    let reloaded = open(dir.path(), "t3");
    let on_disk = reloaded.read(|r| r.entries().len());
    let in_memory = reg.read(|r| r.entries().len());
    assert_eq!(in_memory, n, "all writers changed memory");
    assert_eq!(
        on_disk, n,
        "and every one of them reached disk — a lost update here is invisible \
         until a restart"
    );
}

// --- §3.7 / §3.5: logmon's own keys ------------------------------------------

#[test]
fn boot_stamps_the_broker_keys_and_seeds_era_one() {
    let dir = scratch();
    let reg = open(dir.path(), "t3");
    stamp_boot(&reg, "0.9.0", t("2026-07-28T09:00:00Z")).unwrap();
    reg.read(|r| {
        assert_eq!(r.get("/logmon/domain").unwrap().value, "t3");
        assert_eq!(r.get("/logmon/version").unwrap().value, "0.9.0");
        assert!(r.get("/logmon/first_seen").is_some());
        assert_eq!(r.get("/logmon/incarnation").unwrap().value, "1");
    });
}

#[test]
fn a_same_version_boot_moves_only_validated_at() {
    let dir = scratch();
    let reg = open(dir.path(), "t3");
    stamp_boot(&reg, "0.9.0", t("2026-07-28T09:00:00Z")).unwrap();
    stamp_boot(&reg, "0.9.0", t("2026-07-31T09:14:22Z")).unwrap();
    reg.read(|r| {
        let v = r.get("/logmon/version").unwrap();
        assert_eq!(v.created_at, t("2026-07-28T09:00:00Z"));
        assert_eq!(v.validated_at, t("2026-07-31T09:14:22Z"));
    });
}

#[test]
fn an_upgrade_moves_both_timestamps() {
    let dir = scratch();
    let reg = open(dir.path(), "t3");
    stamp_boot(&reg, "0.9.0", t("2026-07-28T09:00:00Z")).unwrap();
    stamp_boot(&reg, "0.10.0", t("2026-07-31T09:14:22Z")).unwrap();
    reg.read(|r| {
        let v = r.get("/logmon/version").unwrap();
        assert_eq!(v.created_at, t("2026-07-31T09:14:22Z"));
        assert_eq!(v.validated_at, t("2026-07-31T09:14:22Z"));
    });
}

#[test]
fn first_seen_is_not_rewritten_on_a_later_boot() {
    let dir = scratch();
    let first = t("2026-07-28T09:00:00Z");
    {
        let reg = open(dir.path(), "t3");
        stamp_boot(&reg, "0.9.0", first).unwrap();
    }
    let reg = open(dir.path(), "t3");
    stamp_boot(&reg, "0.9.0", t("2026-07-31T09:00:00Z")).unwrap();
    reg.read(|r| {
        assert_eq!(
            r.get("/logmon/first_seen").unwrap().value,
            first.to_rfc3339()
        );
    });
}

#[test]
fn a_seq_origin_on_an_existing_registry_advances_the_era() {
    // Two unrelated `t3`s a week apart share a registry, and a re-created
    // domain restarts seq at 0 — so without this the two eras' seq ranges are
    // indistinguishable in the archive.
    let dir = scratch();
    {
        let reg = open(dir.path(), "t3");
        stamp_boot(&reg, "0.9.0", t("2026-07-24T09:00:00Z")).unwrap();
    }
    let reg = open(dir.path(), "t3");
    note_seq_origin(&reg, t("2026-07-31T09:00:00Z")).unwrap();
    reg.read(|r| {
        assert_eq!(r.get("/logmon/incarnation").unwrap().value, "2");
        assert_eq!(
            r.get("/logmon/incarnation_started").unwrap().value,
            t("2026-07-31T09:00:00Z").to_rfc3339()
        );
    });
}

#[test]
fn a_seq_origin_on_a_brand_new_registry_does_not_advance_past_one() {
    // The counter marks *reuse*. A first-ever domain is era 1, not era 2.
    let dir = scratch();
    let reg = open(dir.path(), "t3");
    note_seq_origin(&reg, t("2026-07-31T09:00:00Z")).unwrap();
    reg.read(|r| assert!(r.get("/logmon/incarnation").is_none()));
    stamp_boot(&reg, "0.9.0", t("2026-07-31T09:00:00Z")).unwrap();
    reg.read(|r| assert_eq!(r.get("/logmon/incarnation").unwrap().value, "1"));
}

#[test]
fn the_incarnation_counter_is_monotone_across_many_eras() {
    let dir = scratch();
    {
        let reg = open(dir.path(), "t3");
        stamp_boot(&reg, "0.9.0", t("2026-07-01T09:00:00Z")).unwrap();
    }
    for i in 0..4 {
        let reg = open(dir.path(), "t3");
        note_seq_origin(&reg, t("2026-07-31T09:00:00Z") + chrono::Duration::days(i)).unwrap();
    }
    let reg = open(dir.path(), "t3");
    reg.read(|r| assert_eq!(r.get("/logmon/incarnation").unwrap().value, "5"));
}

#[test]
fn agents_cannot_reach_the_keys_boot_writes() {
    let dir = scratch();
    let reg = open(dir.path(), "t3");
    stamp_boot(&reg, "0.9.0", t("2026-07-28T09:00:00Z")).unwrap();
    let (got, _) = reg.mutate(|r| {
        r.update(
            &[set("/logmon/version", "99"), set("/logmon", "x")],
            t("2026-07-31T12:00:00Z"),
        )
    });
    for o in &got {
        assert!(matches!(o.outcome, Outcome::Rejected { .. }));
    }
    reg.read(|r| assert_eq!(r.get("/logmon/version").unwrap().value, "0.9.0"));
}
