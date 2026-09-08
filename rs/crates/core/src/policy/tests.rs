//! The parts of the policy that decide rather than observe: the [`decide`] matrix,
//! `policy.json` loading, and the verdict recorded beside an installed artifact.

use super::minisign::testkit::Signer32;
use super::*;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("avada-policy-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn built() -> Source {
    Source::Built {
        commit: "a".repeat(40),
    }
}

fn prebuilt() -> Source {
    Source::Prebuilt {
        url: "https://example.invalid/avada-files.tgz".into(),
    }
}

fn id() -> ModuleId {
    ModuleId::new("acme/avada-files").expect("id")
}

fn policy(locally_built: Rule, prebuilt: Rule) -> Policy {
    Policy {
        locally_built,
        prebuilt,
        ..Policy::default()
    }
}

const EVERY_VERDICT: [fn() -> Verdict; 4] = [
    || Verdict::Trusted { by: "Acme".into() },
    || Verdict::Unsigned,
    || Verdict::Invalid {
        reason: "tampered".into(),
    },
    || Verdict::Unavailable {
        reason: "no spctl".into(),
    },
];

// ---- the defaults

#[test]
fn the_default_policy_trusts_the_commit_and_nothing_else() {
    let p = Policy::default();
    assert_eq!(p.locally_built, Rule::Allow);
    assert_eq!(p.prebuilt, Rule::RequireSignature);
    assert!(p.publishers.is_empty());
}

#[test]
fn a_module_built_here_runs_unsigned() {
    // The whole free path compiles from a verified commit; requiring a signature on
    // top of that would refuse every module the host can actually install.
    assert_eq!(
        decide(&Policy::default(), &Verdict::Unsigned, &built()),
        Decision::Run
    );
}

#[test]
fn an_unsigned_prebuilt_artifact_is_refused_by_default() {
    match decide(&Policy::default(), &Verdict::Unsigned, &prebuilt()) {
        Decision::Refuse { reason } => {
            assert!(reason.contains("unsigned"), "{reason}");
            assert!(reason.contains("prebuilt"), "{reason}");
        }
        other => panic!("expected refuse, got {other:?}"),
    }
}

// ---- the matrix

#[test]
fn a_trusted_verdict_runs_under_every_rule() {
    let trusted = Verdict::Trusted { by: "Acme".into() };
    for rule in [Rule::Allow, Rule::Warn, Rule::RequireSignature] {
        assert_eq!(
            decide(&policy(rule, rule), &trusted, &built()),
            Decision::Run,
            "{rule:?} built"
        );
        assert_eq!(
            decide(&policy(rule, rule), &trusted, &prebuilt()),
            Decision::Run,
            "{rule:?} prebuilt"
        );
    }
}

#[test]
fn allow_runs_everything_except_a_broken_signature() {
    let p = policy(Rule::Allow, Rule::Allow);
    assert_eq!(decide(&p, &Verdict::Unsigned, &built()), Decision::Run);
    assert_eq!(
        decide(
            &p,
            &Verdict::Unavailable {
                reason: "no spctl".into()
            },
            &built()
        ),
        Decision::Run
    );
    // A file that carries a signature which does not hold is news even when nothing
    // required one — silence here would hide the one case that is actually alarming.
    match decide(
        &p,
        &Verdict::Invalid {
            reason: "tampered".into(),
        },
        &built(),
    ) {
        Decision::Warn { reason } => assert!(reason.contains("tampered"), "{reason}"),
        other => panic!("expected warn, got {other:?}"),
    }
}

#[test]
fn warn_never_refuses_and_never_stays_silent() {
    let p = policy(Rule::Warn, Rule::Warn);
    for verdict in EVERY_VERDICT {
        let v = verdict();
        let d = decide(&p, &v, &prebuilt());
        match (&v, &d) {
            (Verdict::Trusted { .. }, Decision::Run) => {}
            (_, Decision::Warn { reason }) => assert!(!reason.is_empty(), "{v:?}"),
            _ => panic!("{v:?} under warn gave {d:?}"),
        }
    }
}

#[test]
fn require_signature_refuses_everything_that_is_not_trusted() {
    let p = policy(Rule::RequireSignature, Rule::RequireSignature);
    for verdict in EVERY_VERDICT {
        let v = verdict();
        let d = decide(&p, &v, &prebuilt());
        match (&v, &d) {
            (Verdict::Trusted { .. }, Decision::Run) => {}
            (_, Decision::Refuse { reason }) => assert!(reason.contains("requires a signature")),
            _ => panic!("{v:?} under require-signature gave {d:?}"),
        }
    }
}

#[test]
fn an_unverifiable_artifact_fails_closed() {
    // "I could not check" is not evidence of trust. A host that runs it anyway has no
    // policy at all — so this cell refuses, and it is the cell most likely to be hit
    // by a broken toolchain rather than an attack.
    match decide(
        &Policy::default(),
        &Verdict::Unavailable {
            reason: "spctl is missing".into(),
        },
        &prebuilt(),
    ) {
        Decision::Refuse { reason } => assert!(reason.contains("spctl is missing"), "{reason}"),
        other => panic!("expected refuse, got {other:?}"),
    }
}

#[test]
fn the_two_sources_are_judged_independently() {
    let p = policy(Rule::Allow, Rule::RequireSignature);
    assert_eq!(decide(&p, &Verdict::Unsigned, &built()), Decision::Run);
    assert!(matches!(
        decide(&p, &Verdict::Unsigned, &prebuilt()),
        Decision::Refuse { .. }
    ));
}

#[test]
fn a_refusal_says_what_was_wrong_and_where_it_came_from() {
    let decision = decide(&Policy::default(), &Verdict::Unsigned, &prebuilt());
    let reason = decision.reason();
    assert!(reason.contains("example.invalid"), "{reason}");
}

// ---- policy.json

#[test]
fn a_missing_policy_file_is_the_default() {
    let dir = scratch("missing");
    assert_eq!(Policy::load(&dir).expect("loads"), Policy::default());
}

#[test]
fn a_policy_file_round_trips() {
    let dir = scratch("roundtrip");
    let key = Signer32::new(5, 0x0102_0304_0506_0708);
    let file = PolicyFile {
        locally_built: Rule::Warn,
        prebuilt: Rule::RequireSignature,
        publisher_keys_source: KeysSource::PolicyFileOnly,
        publishers: BTreeMap::from([(
            "acme/avada-files".to_string(),
            vec![key.public().to_base64()],
        )]),
    };
    std::fs::write(
        Policy::path_under(&dir),
        serde_json::to_string_pretty(&file).expect("json"),
    )
    .expect("write");

    let loaded = Policy::load(&dir).expect("loads");
    assert_eq!(loaded.locally_built, Rule::Warn);
    assert_eq!(loaded.publisher_keys_source, KeysSource::PolicyFileOnly);
    assert_eq!(loaded.keys_for(&id()), vec![key.public()]);
    assert!(loaded
        .keys_for(&ModuleId::new("acme/other").expect("id"))
        .is_empty());
}

#[test]
fn the_rules_are_written_in_kebab_case() {
    // The file is edited by hand, so `require-signature` — not `RequireSignature`.
    let json = serde_json::to_string(&PolicyFile::default()).expect("json");
    assert!(json.contains("\"require-signature\""), "{json}");
    assert!(json.contains("\"allow\""), "{json}");
}

#[test]
fn a_malformed_policy_is_reported_and_falls_back_to_the_strict_default() {
    let dir = scratch("malformed");
    std::fs::write(Policy::path_under(&dir), "{ not json").expect("write");
    assert!(Policy::load(&dir).is_err());
    let (p, complaint) = Policy::load_or_default(&dir);
    // Fail closed: the fallback is the one that refuses prebuilt artifacts.
    assert_eq!(p, Policy::default());
    assert!(complaint.expect("complaint").contains("malformed"));
}

#[test]
fn an_unknown_field_is_a_typo_not_a_silent_no_op() {
    let dir = scratch("unknown-field");
    std::fs::write(
        Policy::path_under(&dir),
        r#"{"locally_built":"allow","prebuild":"allow"}"#,
    )
    .expect("write");
    assert!(
        Policy::load(&dir).is_err(),
        "`prebuild` must not be ignored"
    );
}

#[test]
fn a_publisher_key_that_is_not_a_key_names_the_module() {
    let dir = scratch("bad-key");
    std::fs::write(
        Policy::path_under(&dir),
        r#"{"publishers":{"acme/avada-files":["not-a-key"]}}"#,
    )
    .expect("write");
    match Policy::load(&dir) {
        Err(PolicyError::BadKey { module, .. }) => assert_eq!(module, "acme/avada-files"),
        other => panic!("expected a bad-key error, got {other:?}"),
    }
}

#[test]
fn a_context_carries_only_that_modules_keys() {
    let mine = Signer32::new(5, 1);
    let theirs = Signer32::new(6, 2);
    let p = Policy::from_file(PolicyFile {
        publishers: BTreeMap::from([
            (
                "acme/avada-files".to_string(),
                vec![mine.public().to_base64()],
            ),
            ("other/thing".to_string(), vec![theirs.public().to_base64()]),
        ]),
        ..PolicyFile::default()
    })
    .expect("parses");
    let ctx = p.context(&id(), prebuilt());
    assert_eq!(ctx.publisher_keys, vec![mine.public()]);
    assert_eq!(ctx.module, id());
}

// ---- the recorded verdict

#[test]
fn a_recorded_refusal_is_readable_next_to_the_binary() {
    let dir = scratch("verdict");
    let binary = dir.join("avada-files");
    std::fs::write(&binary, b"x").expect("binary");

    let verdict = Verdict::Unsigned;
    let decision = decide(&Policy::default(), &verdict, &prebuilt());
    let recorded = RecordedVerdict::new(&verdict, &decision, &prebuilt(), "minisign");
    write_verdict(&binary, &recorded).expect("write");

    assert_eq!(
        verdict_path(&binary),
        dir.join("avada-files.notarization.json")
    );
    assert_eq!(read_verdict(&binary).expect("read"), recorded);
    assert!(recorded.refused());
    let refusal = recorded_refusal(&binary).expect("refusal");
    assert!(refusal.contains("unsigned"), "{refusal}");
}

#[test]
fn a_recorded_pass_is_not_a_refusal() {
    let dir = scratch("verdict-ok");
    let binary = dir.join("avada-files");
    std::fs::write(&binary, b"x").expect("binary");
    let verdict = Verdict::Trusted { by: "Acme".into() };
    let recorded = RecordedVerdict::new(&verdict, &Decision::Run, &built(), "spctl");
    write_verdict(&binary, &recorded).expect("write");
    assert_eq!(recorded_refusal(&binary), None);
}

#[test]
fn no_recorded_verdict_at_all_is_not_a_refusal() {
    // Deleting the sidecar can only return an artifact to the pre-G7 status quo (the
    // hash check); it can never turn a refusal into a pass, which is the property that
    // makes an unsigned sidecar acceptable. See the module docs.
    let dir = scratch("verdict-absent");
    let binary = dir.join("avada-files");
    std::fs::write(&binary, b"x").expect("binary");
    assert_eq!(read_verdict(&binary), None);
    assert_eq!(recorded_refusal(&binary), None);
}

#[test]
fn a_corrupt_recorded_verdict_is_ignored_rather_than_fatal() {
    let dir = scratch("verdict-corrupt");
    let binary = dir.join("avada-files");
    std::fs::write(&binary, b"x").expect("binary");
    std::fs::write(verdict_path(&binary), "{ nonsense").expect("write");
    assert_eq!(recorded_refusal(&binary), None);
}

// ---- the platform seam

#[test]
fn the_platform_verifier_names_itself() {
    let name = platform_verifier().name();
    assert!(
        ["spctl", "authenticode", "minisign"].contains(&name),
        "unexpected verifier {name}"
    );
    #[cfg(target_os = "macos")]
    assert_eq!(name, "spctl");
}

#[test]
fn verdicts_and_decisions_have_stable_words_for_the_record() {
    assert_eq!(Verdict::Unsigned.kind(), "unsigned");
    assert_eq!(Verdict::Trusted { by: "x".into() }.kind(), "trusted");
    assert_eq!(Decision::Run.kind(), "run");
    assert_eq!(Decision::Refuse { reason: "x".into() }.kind(), "refuse");
    assert_eq!(Rule::RequireSignature.kind(), "require-signature");
    assert_eq!(built().kind(), "built");
    assert_eq!(prebuilt().kind(), "prebuilt");
}
