use super::*;
use crate::domain_data::path::MAX_PATH_BYTES;

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

fn validate(path: &str) -> DataEntry {
    DataEntry {
        path: path.into(),
        value: None,
        ttl: None,
    }
}

fn outcomes(r: &mut Registry, es: &[DataEntry], now: &str) -> Vec<Outcome> {
    r.update(es, t(now))
        .into_iter()
        .map(|o| o.outcome)
        .collect()
}

// --- §3.2/§3.3: the two timestamps ------------------------------------------

#[test]
fn a_new_key_is_created_and_both_timestamps_are_the_same_instant() {
    let mut r = Registry::new();
    assert_eq!(
        outcomes(
            &mut r,
            &[set("/Build/commit", "abc")],
            "2026-07-31T12:00:00Z"
        ),
        vec![Outcome::Created]
    );
    let e = r.get("/Build/commit").unwrap();
    assert_eq!(e.created_at, e.validated_at);
}

#[test]
fn restating_the_same_value_moves_only_validated_at() {
    let mut r = Registry::new();
    outcomes(&mut r, &[set("/a", "1")], "2026-07-31T12:00:00Z");
    let created = r.get("/a").unwrap().created_at;

    assert_eq!(
        outcomes(&mut r, &[set("/a", "1")], "2026-07-31T13:00:00Z"),
        vec![Outcome::Validated],
        "same value is a confirmation, not an update"
    );
    let e = r.get("/a").unwrap();
    assert_eq!(e.created_at, created, "created_at must not move");
    assert_eq!(e.validated_at, t("2026-07-31T13:00:00Z"));
}

#[test]
fn a_different_value_moves_both() {
    let mut r = Registry::new();
    outcomes(&mut r, &[set("/a", "1")], "2026-07-31T12:00:00Z");
    assert_eq!(
        outcomes(&mut r, &[set("/a", "2")], "2026-07-31T13:00:00Z"),
        vec![Outcome::Updated]
    );
    let e = r.get("/a").unwrap();
    assert_eq!(e.created_at, t("2026-07-31T13:00:00Z"));
    assert_eq!(e.validated_at, t("2026-07-31T13:00:00Z"));
}

#[test]
fn validated_at_is_never_before_created_at_after_any_operation() {
    let mut r = Registry::new();
    let ops = [
        set("/a", "1"),
        validate("/a"),
        set("/a", "2"),
        set("/a", "2"),
        validate("/a"),
    ];
    for (i, op) in ops.iter().enumerate() {
        r.update(
            std::slice::from_ref(op),
            t("2026-07-31T12:00:00Z") + chrono::Duration::minutes(i as i64),
        );
        let e = r.get("/a").unwrap();
        assert!(e.validated_at >= e.created_at, "broken after op {i}");
    }
}

#[test]
fn key_only_validates_an_existing_key() {
    let mut r = Registry::new();
    outcomes(&mut r, &[set("/a", "1")], "2026-07-31T12:00:00Z");
    assert_eq!(
        outcomes(&mut r, &[validate("/a")], "2026-07-31T13:00:00Z"),
        vec![Outcome::Validated]
    );
    assert_eq!(r.get("/a").unwrap().value, "1");
}

#[test]
fn key_only_on_a_missing_key_does_not_create() {
    let mut r = Registry::new();
    let got = outcomes(&mut r, &[validate("/a")], "2026-07-31T12:00:00Z");
    assert!(matches!(got[0], Outcome::Unknown { .. }));
    assert!(r.get("/a").is_none(), "must not invent a valueless key");
}

// --- §3.3: absent ≡ null ≡ unchanged ----------------------------------------

#[test]
fn an_absent_value_and_an_explicit_none_behave_identically() {
    // `{path}` and `{path, value: null}` reach this layer as the same
    // `DataEntry`; the point of the test is that nothing downstream treats a
    // deserialised null as an erasure.
    let mut r = Registry::new();
    outcomes(&mut r, &[set("/a", "1")], "2026-07-31T12:00:00Z");
    let explicit = DataEntry {
        path: "/a".into(),
        value: None,
        ttl: None,
    };
    assert_eq!(
        outcomes(&mut r, &[explicit], "2026-07-31T13:00:00Z"),
        vec![Outcome::Validated]
    );
    assert_eq!(
        r.get("/a").unwrap().value,
        "1",
        "erasure is remove, not this"
    );
}

// --- §3.2: ttl ---------------------------------------------------------------

#[test]
fn a_validate_only_entry_leaves_an_existing_ttl_intact() {
    let mut r = Registry::new();
    r.update(
        &[DataEntry {
            path: "/a".into(),
            value: Some("1".into()),
            ttl: Some(TtlSpec::Set(1800)),
        }],
        t("2026-07-31T12:00:00Z"),
    );
    outcomes(&mut r, &[validate("/a")], "2026-07-31T13:00:00Z");
    assert_eq!(r.get("/a").unwrap().ttl_secs, Some(1800));
}

#[test]
fn restating_a_value_without_a_ttl_leaves_it_intact() {
    let mut r = Registry::new();
    r.update(
        &[DataEntry {
            path: "/a".into(),
            value: Some("1".into()),
            ttl: Some(TtlSpec::Set(1800)),
        }],
        t("2026-07-31T12:00:00Z"),
    );
    outcomes(&mut r, &[set("/a", "2")], "2026-07-31T13:00:00Z");
    assert_eq!(
        r.get("/a").unwrap().ttl_secs,
        Some(1800),
        "one absent rule for value and ttl; a restate must not drop a lifetime"
    );
}

#[test]
fn ttl_false_clears_in_place_and_leaves_created_at_alone() {
    // The whole reason `ttl: false` exists. The alternative the draft
    // prescribed — remove then re-set — resets created_at, laundering a
    // months-old confirmed fact into a fresh-looking one.
    let mut r = Registry::new();
    r.update(
        &[DataEntry {
            path: "/a".into(),
            value: Some("1".into()),
            ttl: Some(TtlSpec::Set(1800)),
        }],
        t("2026-07-01T12:00:00Z"),
    );
    let created = r.get("/a").unwrap().created_at;

    r.update(
        &[DataEntry {
            path: "/a".into(),
            value: None,
            ttl: Some(TtlSpec::Clear),
        }],
        t("2026-07-31T12:00:00Z"),
    );
    let e = r.get("/a").unwrap();
    assert_eq!(e.ttl_secs, None);
    assert_eq!(e.created_at, created, "clearing a ttl is not a new fact");
    assert_eq!(e.value, "1");
}

#[test]
fn a_ttl_only_entry_both_validates_and_sets_the_lifetime() {
    let mut r = Registry::new();
    outcomes(&mut r, &[set("/a", "1")], "2026-07-31T12:00:00Z");
    let got = r.update(
        &[DataEntry {
            path: "/a".into(),
            value: None,
            ttl: Some(TtlSpec::Set(60)),
        }],
        t("2026-07-31T13:00:00Z"),
    );
    assert_eq!(got[0].outcome, Outcome::Validated);
    let e = r.get("/a").unwrap();
    assert_eq!(e.ttl_secs, Some(60));
    assert_eq!(e.validated_at, t("2026-07-31T13:00:00Z"));
}

#[test]
fn validating_an_expired_key_makes_it_current_again() {
    // Expiry runs from validated_at, so confirmation must be able to refresh
    // it. Anchored on created_at this would be impossible and the marker would
    // read expired forever on the best-maintained keys in the registry.
    let mut r = Registry::new();
    r.update(
        &[DataEntry {
            path: "/a".into(),
            value: Some("1".into()),
            ttl: Some(TtlSpec::Set(60)),
        }],
        t("2026-07-31T12:00:00Z"),
    );
    assert_eq!(
        r.get("/a").unwrap().is_expired(t("2026-07-31T12:02:00Z")),
        Some(true)
    );
    outcomes(&mut r, &[validate("/a")], "2026-07-31T12:02:00Z");
    assert_eq!(
        r.get("/a").unwrap().is_expired(t("2026-07-31T12:02:30Z")),
        Some(false)
    );
}

#[test]
fn expiry_never_mutates_what_it_reports_on() {
    let mut r = Registry::new();
    r.update(
        &[DataEntry {
            path: "/a".into(),
            value: Some("1".into()),
            ttl: Some(TtlSpec::Set(60)),
        }],
        t("2026-07-31T12:00:00Z"),
    );
    let before = r.get("/a").unwrap().clone();
    let _ = r.get("/a").unwrap().is_expired(t("2027-01-01T00:00:00Z"));
    assert_eq!(
        r.get("/a").unwrap(),
        &before,
        "a past-ttl key keeps its value and both timestamps"
    );
}

#[test]
fn ttl_true_is_refused_rather_than_guessed_at() {
    assert!(parse_ttl(&serde_json::json!(true)).is_err());
    assert_eq!(
        parse_ttl(&serde_json::json!(false)),
        Ok(Some(TtlSpec::Clear))
    );
    assert_eq!(parse_ttl(&serde_json::json!(null)), Ok(None));
    assert_eq!(
        parse_ttl(&serde_json::json!("30m")),
        Ok(Some(TtlSpec::Set(1800)))
    );
    assert!(parse_ttl(&serde_json::json!(1800)).is_err());
}

// --- §3.7: the reservation, both directions ---------------------------------

#[test]
fn update_rejects_the_reserved_prefix_and_the_bare_segment() {
    let mut r = Registry::new();
    let got = outcomes(
        &mut r,
        &[set("/logmon/version", "9"), set("/logmon", "x")],
        "2026-07-31T12:00:00Z",
    );
    for o in &got {
        assert_eq!(
            *o,
            Outcome::Rejected {
                reason: RejectReason::ReservedPrefix
            }
        );
    }
    assert!(r.entries().is_empty());
}

#[test]
fn remove_refuses_a_pattern_that_would_wipe_the_reserved_subtree() {
    // `remove(["/logmon"])` under segment matching deletes first_seen and
    // incarnation — H3's and H6's entire defence — from a call that looks like
    // tidying up, and remove has no undo.
    let mut r = Registry::new();
    r.set_reserved("/logmon/first_seen", "x".into(), t("2026-07-31T12:00:00Z"))
        .unwrap();
    r.set_reserved("/logmon/incarnation", "2".into(), t("2026-07-31T12:00:00Z"))
        .unwrap();

    let got = r.remove(&["/logmon".to_string(), "/logmon/first_seen".to_string()]);
    for o in &got {
        assert_eq!(o.rejected, Some(RejectReason::ReservedPrefix));
        assert_eq!(o.removed, 0);
    }
    assert!(r.get("/logmon/first_seen").is_some());
    assert!(r.get("/logmon/incarnation").is_some());
}

#[test]
fn set_reserved_is_the_only_route_in_and_refuses_everything_else() {
    let mut r = Registry::new();
    assert_eq!(
        r.set_reserved("/Build/commit", "abc".into(), t("2026-07-31T12:00:00Z")),
        Err(RegistryError::Reserved)
    );
    assert!(r
        .set_reserved("/logmon/version", "0.9.0".into(), t("2026-07-31T12:00:00Z"))
        .is_ok());
}

#[test]
fn a_reserved_lookalike_is_not_reserved() {
    let mut r = Registry::new();
    assert_eq!(
        outcomes(&mut r, &[set("/logmonster", "x")], "2026-07-31T12:00:00Z"),
        vec![Outcome::Created]
    );
}

// --- §3.9: the sigil ---------------------------------------------------------

#[test]
fn a_sigil_key_is_refused_on_a_registry_surface() {
    let mut r = Registry::new();
    assert_eq!(
        outcomes(
            &mut r,
            &[set("@/Data/seed", "8814")],
            "2026-07-31T12:00:00Z"
        ),
        vec![Outcome::Rejected {
            reason: RejectReason::SigilNotAllowedHere
        }]
    );
    assert!(r.entries().is_empty());
}

#[test]
fn a_scoped_key_never_enters_the_registry_at_all() {
    // The bug this replaced: the registry held scoped keys in the same map, so
    // they persisted to the registry file and the NEXT case on the domain
    // inherited the last one's seed — the exact laundering the sigil exists to
    // prevent, reintroduced by the storage layout.
    let (registry, scoped, rejected) =
        partition_scoped(&[set("/Env/host", "ci-7"), set("@/Data/seed", "8814")]);
    assert!(rejected.is_empty());
    assert_eq!(registry.len(), 1);
    assert_eq!(registry[0].path, "/Env/host");
    assert_eq!(scoped.facts.len(), 1);
    assert_eq!(scoped.facts[0].key, "@/Data/seed");

    let mut r = Registry::new();
    r.update(&registry, t("2026-07-31T12:00:00Z"));
    assert!(
        r.entries().keys().all(|k| !k.starts_with('@')),
        "nothing scoped may reach the persisted map"
    );
}

#[test]
fn a_scoped_key_and_its_unsigilled_twin_are_two_different_facts() {
    let (registry, scoped, _) =
        partition_scoped(&[set("/Data/seed", "1"), set("@/Data/seed", "2")]);
    assert_eq!(registry[0].value.as_deref(), Some("1"));
    assert_eq!(scoped.facts[0].value, "2");
    assert_eq!(scoped.facts[0].key, "@/Data/seed", "the sigil is kept");
}

#[test]
fn a_sigil_does_not_launder_the_reservation() {
    let (registry, scoped, rejected) = partition_scoped(&[set("@/logmon/version", "9")]);
    assert!(registry.is_empty());
    assert!(scoped.facts.is_empty());
    assert_eq!(
        rejected[0].outcome,
        Outcome::Rejected {
            reason: RejectReason::ReservedPrefix
        }
    );
}

#[test]
fn a_scoped_key_with_no_value_has_nothing_to_assert() {
    // "Validate" is meaningless against a document that does not exist yet.
    let (_, scoped, rejected) = partition_scoped(&[validate("@/Data/seed")]);
    assert!(scoped.facts.is_empty());
    assert!(matches!(rejected[0].outcome, Outcome::Rejected { .. }));
}

#[test]
fn scoped_keys_have_their_own_cap() {
    let batch: Vec<DataEntry> = (0..MAX_SIGIL_KEYS + 1)
        .map(|i| set(&format!("@/s{i:04}"), "v"))
        .collect();
    let (_, scoped, rejected) = partition_scoped(&batch);
    assert_eq!(scoped.facts.len(), MAX_SIGIL_KEYS);
    assert_eq!(
        rejected[0].outcome,
        Outcome::Rejected {
            reason: RejectReason::RegistryFull
        }
    );
}

#[test]
fn the_scoped_cap_does_not_consume_the_registrys() {
    let mut batch: Vec<DataEntry> = (0..MAX_SIGIL_KEYS)
        .map(|i| set(&format!("@/s{i:04}"), "v"))
        .collect();
    batch.extend((0..MAX_KEYS).map(|i| set(&format!("/k{i:04}"), "v")));
    let (registry, scoped, rejected) = partition_scoped(&batch);
    assert!(rejected.is_empty());
    assert_eq!(scoped.facts.len(), MAX_SIGIL_KEYS);

    let mut r = Registry::new();
    let got: Vec<Outcome> = r
        .update(&registry, t("2026-07-31T12:00:00Z"))
        .into_iter()
        .map(|o| o.outcome)
        .collect();
    assert!(got.iter().all(|o| *o == Outcome::Created));
}

#[test]
fn partition_leaves_unsigilled_entries_untouched_and_in_order() {
    let (registry, scoped, rejected) = partition_scoped(&[
        set("/a", "1"),
        set("@/x", "9"),
        set("/b", "2"),
        validate("/c"),
    ]);
    assert!(rejected.is_empty());
    let paths: Vec<_> = registry.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["/a", "/b", "/c"]);
    assert_eq!(scoped.facts.len(), 1);
}

#[test]
fn a_repeated_scoped_key_takes_the_last_value_in_the_call() {
    let (_, scoped, _) = partition_scoped(&[set("@/x", "1"), set("@/x", "2")]);
    assert_eq!(scoped.facts.len(), 1);
    assert_eq!(scoped.facts[0].value, "2");
}

#[test]
fn remove_cannot_be_aimed_at_a_scoped_key() {
    // There is nothing there to remove, and accepting the pattern would report
    // a confident `removed: 0` for a key that lives somewhere else entirely.
    let mut r = Registry::new();
    let got = r.remove(&["@/Data/seed".to_string()]);
    assert_eq!(got[0].rejected, Some(RejectReason::SigilNotAllowedHere));
}

// --- §3.3: rejection, ordering, caps -----------------------------------------

#[test]
fn one_malformed_entry_does_not_reject_the_batch() {
    let mut r = Registry::new();
    let got = outcomes(
        &mut r,
        &[set("/good", "1"), set("bad", "2"), set("/also-good", "3")],
        "2026-07-31T12:00:00Z",
    );
    assert_eq!(got[0], Outcome::Created);
    assert_eq!(
        got[1],
        Outcome::Rejected {
            reason: RejectReason::MalformedPath
        }
    );
    assert_eq!(got[2], Outcome::Created);
    assert_eq!(r.entries().len(), 2);
}

#[test]
fn each_rejection_reason_is_distinguishable() {
    let mut r = Registry::new();
    let long_path = format!("/{}", "a".repeat(MAX_PATH_BYTES));
    let got = outcomes(
        &mut r,
        &[
            set("nope", "1"),
            set(&long_path, "1"),
            DataEntry {
                path: "/v".into(),
                value: Some("x".repeat(MAX_VALUE_BYTES + 1)),
                ttl: None,
            },
            set("/logmon/x", "1"),
            set("@/x", "1"),
        ],
        "2026-07-31T12:00:00Z",
    );
    let reasons: Vec<_> = got
        .iter()
        .map(|o| match o {
            Outcome::Rejected { reason } => *reason,
            other => panic!("expected rejection, got {other:?}"),
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            RejectReason::MalformedPath,
            RejectReason::PathTooLong,
            RejectReason::ValueTooLong,
            RejectReason::ReservedPrefix,
            RejectReason::SigilNotAllowedHere,
        ]
    );
}

#[test]
fn a_value_exactly_at_the_cap_is_accepted() {
    let mut r = Registry::new();
    let got = outcomes(
        &mut r,
        &[DataEntry {
            path: "/v".into(),
            value: Some("x".repeat(MAX_VALUE_BYTES)),
            ttl: None,
        }],
        "2026-07-31T12:00:00Z",
    );
    assert_eq!(got, vec![Outcome::Created]);
}

#[test]
fn the_cap_applies_in_the_order_given_so_two_callers_agree() {
    // Sorted or all-or-nothing, the same set sent in two orders would produce
    // two different registries with nothing in the archive to explain it.
    let batch: Vec<DataEntry> = (0..MAX_KEYS + 2)
        .map(|i| set(&format!("/k{i:04}"), "v"))
        .collect();
    let mut r = Registry::new();
    let got = outcomes(&mut r, &batch, "2026-07-31T12:00:00Z");
    assert_eq!(got[MAX_KEYS - 1], Outcome::Created);
    for o in &got[MAX_KEYS..] {
        assert_eq!(
            *o,
            Outcome::Rejected {
                reason: RejectReason::RegistryFull
            }
        );
    }
    assert!(r.get("/k0255").is_some());
    assert!(r.get("/k0256").is_none());
}

#[test]
fn updating_an_existing_key_at_the_cap_still_works() {
    // The cap bounds distinct keys, not writes. Refusing an update to a key
    // already present would make a full registry unmaintainable.
    let batch: Vec<DataEntry> = (0..MAX_KEYS)
        .map(|i| set(&format!("/k{i:04}"), "v"))
        .collect();
    let mut r = Registry::new();
    outcomes(&mut r, &batch, "2026-07-31T12:00:00Z");
    assert_eq!(
        outcomes(&mut r, &[set("/k0000", "w")], "2026-07-31T13:00:00Z"),
        vec![Outcome::Updated]
    );
}

#[test]
fn reserved_keys_do_not_count_against_the_agents_cap() {
    let mut r = Registry::new();
    for i in 0..8 {
        r.set_reserved(
            &format!("/logmon/k{i}"),
            "v".into(),
            t("2026-07-31T12:00:00Z"),
        )
        .unwrap();
    }
    let batch: Vec<DataEntry> = (0..MAX_KEYS)
        .map(|i| set(&format!("/k{i:04}"), "v"))
        .collect();
    let got = outcomes(&mut r, &batch, "2026-07-31T12:00:00Z");
    assert!(
        got.iter().all(|o| *o == Outcome::Created),
        "logmon must not be able to lock an agent out of its own registry"
    );
}

// --- §3.3: the unknown cause -------------------------------------------------

#[test]
fn unknown_says_undetermined_rather_than_guessing_never_set() {
    // The two causes are not distinguishable from the registry alone, because
    // first_seen is written identically for a new domain and a lost one.
    let mut r = Registry::new();
    let got = outcomes(&mut r, &[validate("/a")], "2026-07-31T12:00:00Z");
    assert_eq!(
        got[0],
        Outcome::Unknown {
            cause: UnknownCause::Undetermined
        }
    );
}

#[test]
fn unknown_says_never_set_when_the_registry_demonstrably_exists() {
    let mut r = Registry::new();
    outcomes(&mut r, &[set("/other", "1")], "2026-07-31T12:00:00Z");
    let got = outcomes(&mut r, &[validate("/a")], "2026-07-31T12:00:00Z");
    assert_eq!(
        got[0],
        Outcome::Unknown {
            cause: UnknownCause::NeverSet
        }
    );
}

#[test]
fn unknown_says_no_registry_only_with_evidence() {
    let mut r = Registry::new();
    r.note_quarantined();
    let got = outcomes(&mut r, &[validate("/a")], "2026-07-31T12:00:00Z");
    assert_eq!(
        got[0],
        Outcome::Unknown {
            cause: UnknownCause::NoRegistry
        }
    );
}

// --- §3.1/§3.4: remove and query ---------------------------------------------

#[test]
fn remove_by_prefix_reports_a_count_per_pattern() {
    let mut r = Registry::new();
    outcomes(
        &mut r,
        &[
            set("/Versions/a", "1"),
            set("/Versions/b", "2"),
            set("/VersionsOld/c", "3"),
            set("/Env/host", "h"),
        ],
        "2026-07-31T12:00:00Z",
    );
    let got = r.remove(&["/Versions".to_string(), "/Nothing".to_string()]);
    assert_eq!(got[0].removed, 2);
    assert_eq!(got[1].removed, 0);
    assert!(
        r.get("/VersionsOld/c").is_some(),
        "segment-boundary matching, or /Ver would delete /Versions/*"
    );
    assert!(r.get("/Env/host").is_some());
}

#[test]
fn a_malformed_remove_pattern_is_reported_not_ignored() {
    let mut r = Registry::new();
    let got = r.remove(&["oops".to_string()]);
    assert_eq!(got[0].rejected, Some(RejectReason::MalformedPath));
}

#[test]
fn query_filters_by_prefix_and_by_staleness() {
    let mut r = Registry::new();
    outcomes(&mut r, &[set("/Versions/a", "1")], "2026-07-01T12:00:00Z");
    outcomes(&mut r, &[set("/Versions/b", "2")], "2026-07-31T12:00:00Z");
    outcomes(&mut r, &[set("/Env/host", "h")], "2026-07-31T12:00:00Z");

    assert_eq!(r.query(Some("/Versions"), None).len(), 2);
    assert_eq!(r.query(None, None).len(), 3);
    let stale = r.query(None, Some(t("2026-07-15T00:00:00Z")));
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].0, "/Versions/a");
}

#[test]
fn iteration_order_is_stable_so_two_renders_are_byte_identical() {
    let mut a = Registry::new();
    let mut b = Registry::new();
    outcomes(
        &mut a,
        &[set("/z", "1"), set("/a", "2"), set("/m", "3")],
        "2026-07-31T12:00:00Z",
    );
    outcomes(
        &mut b,
        &[set("/m", "3"), set("/z", "1"), set("/a", "2")],
        "2026-07-31T12:00:00Z",
    );
    let ka: Vec<_> = a.entries().keys().collect();
    let kb: Vec<_> = b.entries().keys().collect();
    assert_eq!(ka, kb, "insertion order must not reach the archive");
}
