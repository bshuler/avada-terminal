//! The licensing path end to end, against the in-process [`StubIssuer`] and a clock the
//! test moves by hand: install, run, drift past the check-in interval into the grace
//! window, fall out of it, check in, get revoked, and be refused. Nothing here opens a
//! socket or reads the wall clock, so the whole life of a license takes microseconds and
//! the outcome does not depend on when the suite runs.

use super::store::write_private;
use super::stub_issuer::{Grant, StubIssuer, DEVICE_CODE_TTL};
use super::*;
use avada_module_sdk::license::{LicenseState, GRACE_DAYS};
use std::path::PathBuf;

const DAY: u64 = 86_400;
/// Every test starts here rather than at 0, so "an hour ago" does not underflow.
const T0: u64 = 1_700_000_000;

/// A clock the test sets.
#[derive(Clone)]
struct Dial(Arc<Mutex<u64>>);

impl Dial {
    fn new(at: u64) -> Self {
        Dial(Arc::new(Mutex::new(at)))
    }

    fn clock(&self) -> Clock {
        let inner = self.0.clone();
        Arc::new(move || *inner.lock().unwrap())
    }

    fn set(&self, at: u64) {
        *self.0.lock().unwrap() = at;
    }

    fn advance_days(&self, days: u64) {
        let mut now = self.0.lock().unwrap();
        *now += days * DAY;
    }

    fn now(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}

/// An issuer, a service talking to it, and the dial they share.
struct World {
    dial: Dial,
    issuer: Arc<StubIssuer>,
    service: LicenseService,
}

fn world() -> World {
    world_at(T0)
}

fn world_at(now: u64) -> World {
    let dial = Dial::new(now);
    let issuer = Arc::new(StubIssuer::with_clock("https://issuer.test", dial.clock()));
    let http: Arc<dyn LicenseHttp> = issuer.clone();
    let service = LicenseService::new(Arc::new(MemoryLicenseStore::new()), http, dial.clock());
    World {
        dial,
        issuer,
        service,
    }
}

impl World {
    /// Mint `grant` and install it, as a user who was sent a `.jwt` would.
    async fn install(&self, grant: &Grant) -> LicenseSummary {
        let token = LicenseToken::new(self.issuer.issue(grant).expect("the stub signs"));
        self.service
            .install_token(token, None)
            .await
            .expect("a freshly minted license installs")
    }

    async fn gate(&self, product: &str) -> Gate {
        self.service.gate(product).await
    }

    async fn state(&self, product: &str) -> LicenseState {
        self.service
            .state(product, self.dial.now())
            .await
            .expect("an installed license has a state")
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "avada-license-e2e-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn a_license_is_installed_verified_and_summarised() {
    let w = world();
    let summary = w
        .install(&Grant::for_product("acme/pro").licensed_to("dev@example.test"))
        .await;
    assert_eq!(summary.product, "acme/pro");
    assert_eq!(summary.issuer, "https://issuer.test");
    assert_eq!(summary.licensee, "dev@example.test");
    assert_eq!(summary.state, "valid");
    assert_eq!(summary.reason, None);
    assert_eq!(summary.exp, None, "a perpetual license has no expiry");
    assert_eq!(summary.checkin.last_ok, T0);
    assert!(!summary.checkin.revoked);
    assert_eq!(w.gate("acme/pro").await, Gate::Run);

    // The issuer was discovered from the token's `iss` and remembered with the license.
    assert_eq!(w.service.list().await.unwrap().len(), 1);
    // Nothing leaks the token: the summary carries claims, never the credential.
    let shown = format!("{summary:?}");
    assert!(!shown.contains("eyJ"), "{shown}");
}

#[tokio::test]
async fn an_unlicensed_product_is_refused_by_name() {
    let w = world();
    match w.gate("acme/pro").await {
        Gate::Refuse(why) => assert!(why.contains("acme/pro"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(matches!(
        w.service.state("acme/pro", T0).await,
        Err(LicenseError::NotLicensed(p)) if p == "acme/pro"
    ));
    assert!(!w.service.revoke_local("acme/pro").unwrap());
}

#[tokio::test]
async fn drifting_past_the_check_in_interval_gives_a_banner_then_a_refusal() {
    let w = world();
    w.install(&Grant::for_product("acme/pro").checking_in_every(7))
        .await;
    assert_eq!(w.state("acme/pro").await, LicenseState::Valid);

    // Day 6: still inside the interval.
    w.dial.advance_days(6);
    assert_eq!(w.gate("acme/pro").await, Gate::Run);

    // Day 8: the check-in is overdue, so the module still runs but says why.
    w.dial.advance_days(2);
    assert_eq!(w.state("acme/pro").await, LicenseState::GracePeriod);
    match w.gate("acme/pro").await {
        Gate::RunWithBanner(why) => {
            assert!(why.contains("check-in overdue"), "{why}");
            assert!(why.contains("acme/pro"), "{why}");
        }
        other => panic!("expected a banner, got {other:?}"),
    }

    // Day 7 + GRACE_DAYS: out of the window, and the gate closes.
    w.dial.advance_days(u64::from(GRACE_DAYS));
    assert_eq!(w.state("acme/pro").await, LicenseState::StaleCheckin);
    match w.gate("acme/pro").await {
        Gate::Refuse(why) => assert!(why.contains("has not been confirmed"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_check_in_inside_the_grace_window_restores_the_licence() {
    let w = world();
    w.install(&Grant::for_product("acme/pro").checking_in_every(7))
        .await;
    w.dial.advance_days(8);
    assert!(
        !w.gate("acme/pro").await.allows_run()
            || matches!(w.gate("acme/pro").await, Gate::RunWithBanner(_))
    );

    // Not forced, and due: the issuer is asked and says yes.
    match w.service.checkin("acme/pro", false).await.unwrap() {
        CheckinOutcome::Answered { active, reason } => {
            assert!(active);
            assert_eq!(reason, None);
        }
        other => panic!("expected an answer, got {other:?}"),
    }
    assert_eq!(w.state("acme/pro").await, LicenseState::Valid);
    assert_eq!(w.gate("acme/pro").await, Gate::Run);

    // A second check-in on the same day is not due, and does not touch the issuer.
    assert_eq!(
        w.service.checkin("acme/pro", false).await.unwrap(),
        CheckinOutcome::NotDue
    );
}

#[tokio::test]
async fn a_revoked_license_is_refused_from_the_next_check_in_on() {
    let w = world();
    let token = LicenseToken::new(w.issuer.issue(&Grant::for_product("acme/pro")).unwrap());
    let jti = w.issuer.jti_of(token.expose_secret()).unwrap();
    w.service.install_token(token, None).await.unwrap();
    assert_eq!(w.gate("acme/pro").await, Gate::Run);

    assert!(w.issuer.revoke(&jti));
    // Revocation is only learned at a check-in: until then the license still runs.
    assert_eq!(w.gate("acme/pro").await, Gate::Run);

    match w.service.checkin("acme/pro", true).await.unwrap() {
        CheckinOutcome::Answered { active, reason } => {
            assert!(!active);
            assert_eq!(reason.as_deref(), Some("revoked by the issuer"));
        }
        other => panic!("expected an answer, got {other:?}"),
    }
    assert_eq!(w.state("acme/pro").await, LicenseState::Revoked);
    match w.gate("acme/pro").await {
        Gate::Refuse(why) => assert!(why.contains("revoked"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let summary = w.service.show("acme/pro").await.unwrap();
    assert_eq!(summary.state, "revoked");
    assert!(summary.checkin.revoked);
}

#[tokio::test]
async fn an_unreachable_issuer_leaves_the_record_alone() {
    let w = world();
    w.install(&Grant::for_product("acme/pro").checking_in_every(7))
        .await;
    let before = w.service.show("acme/pro").await.unwrap().checkin;

    // The same store, seen by a process whose network cannot reach that issuer at all:
    // every request goes to a stub that serves a different base and answers 404.
    let offline = LicenseService::new(
        w.service.store(),
        Arc::new(StubIssuer::with_clock(
            "https://elsewhere.test",
            w.dial.clock(),
        )),
        w.dial.clock(),
    );
    w.dial.advance_days(8);
    match offline.checkin("acme/pro", true).await.unwrap() {
        CheckinOutcome::Unreachable(why) => assert!(!why.is_empty(), "a reason is given"),
        other => panic!("expected an unreachable issuer, got {other:?}"),
    }
    // Nothing was learned, so nothing was written: the license is still in its grace
    // window rather than being marked revoked because a network was down.
    let after = w.service.show("acme/pro").await.unwrap().checkin;
    assert_eq!(after, before);
    assert_eq!(w.state("acme/pro").await, LicenseState::GracePeriod);

    // With the issuer reachable again, the same check-in succeeds and the record moves.
    assert!(matches!(
        w.service.checkin("acme/pro", true).await.unwrap(),
        CheckinOutcome::Answered { active: true, .. }
    ));
    assert!(w.service.show("acme/pro").await.unwrap().checkin.last_ok > before.last_ok);
}

#[tokio::test]
async fn a_license_for_another_issuer_is_refused() {
    let w = world();
    w.service.register_issuer("acme/pro", "https://issuer.test");
    // A second issuer, at a different URL, mints a license for the same product.
    let other = StubIssuer::with_clock("https://rogue.test", w.dial.clock());
    let token = LicenseToken::new(other.issue(&Grant::for_product("acme/pro")).unwrap());
    match w.service.install_token(token, None).await {
        Err(LicenseError::IssuerMismatch {
            product,
            expected,
            actual,
        }) => {
            assert_eq!(product, "acme/pro");
            assert_eq!(expected, "https://issuer.test");
            assert_eq!(actual, "https://rogue.test");
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
    assert!(matches!(w.gate("acme/pro").await, Gate::Refuse(_),));
}

#[tokio::test]
async fn a_token_signed_by_a_stranger_does_not_verify() {
    let w = world();
    // Same issuer URL, different key: the stub's key set will not know the `kid`.
    let impostor = StubIssuer::with_clock("https://issuer.test", w.dial.clock());
    let token = LicenseToken::new(impostor.issue(&Grant::for_product("acme/pro")).unwrap());
    match w.service.install_token(token, None).await {
        Err(LicenseError::Verify(VerifyError::UnknownKey(kid))) => {
            assert_eq!(kid, impostor.kid());
        }
        other => panic!("expected an unknown key, got {other:?}"),
    }
}

#[tokio::test]
async fn nbf_inside_the_skew_installs_but_does_not_run_yet() {
    let w = world();
    // Signed to start in a hundred seconds: inside the verifier's skew, so the token is
    // accepted, but the state machine is exact and says not yet.
    let summary = w
        .install(&Grant::for_product("acme/pro").starting_at(T0 + 100))
        .await;
    assert_eq!(summary.state, "not_yet");
    assert_eq!(w.state("acme/pro").await, LicenseState::NotYet);
    assert!(matches!(w.gate("acme/pro").await, Gate::Refuse(_)));

    w.dial.set(T0 + 200);
    assert_eq!(w.gate("acme/pro").await, Gate::Run);
}

#[tokio::test]
async fn an_expired_license_stops_running_and_still_reports_itself() {
    let w = world();
    w.install(&Grant::for_product("acme/pro").expiring_at(T0 + 30 * DAY))
        .await;
    assert_eq!(w.gate("acme/pro").await, Gate::Run);
    w.dial.advance_days(31);
    assert_eq!(w.state("acme/pro").await, LicenseState::Expired);
    match w.gate("acme/pro").await {
        Gate::Refuse(why) => assert!(why.contains("expired"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Expiry does not erase the license: `avada license show` still explains it.
    let summary = w.service.show("acme/pro").await.unwrap();
    assert_eq!(summary.state, "expired");
    assert_eq!(summary.exp, Some(T0 + 30 * DAY));
}

#[tokio::test]
async fn a_major_beyond_the_licensed_range_is_refused() {
    let w = world();
    w.install(&Grant::for_product("acme/pro").up_to_major(2))
        .await;
    assert_eq!(
        w.service.gate_for_major("acme/pro", Some(1)).await,
        Gate::Run
    );
    assert_eq!(
        w.service.gate_for_major("acme/pro", Some(2)).await,
        Gate::Run
    );
    match w.service.gate_for_major("acme/pro", Some(3)).await {
        Gate::Refuse(why) => assert!(why.contains("major 3"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_stale_key_cache_is_refreshed_when_the_kid_is_unknown() {
    let dial = Dial::new(T0);
    let issuer = Arc::new(StubIssuer::with_clock("https://issuer.test", dial.clock()));
    let store = Arc::new(MemoryLicenseStore::new());
    // Seed the cache with a key set that does not contain the issuer's `kid`.
    let stranger = verify::SigningKey::generate_with_kid("stale");
    store
        .save_keys(
            "https://issuer.test",
            &Jwks {
                keys: vec![stranger.jwk()],
            },
        )
        .unwrap();
    let http: Arc<dyn LicenseHttp> = issuer.clone();
    let service = LicenseService::new(store.clone(), http, dial.clock());
    let token = LicenseToken::new(issuer.issue(&Grant::for_product("acme/pro")).unwrap());
    // Installing succeeds because the cache miss triggers a fetch.
    service.install_token(token, None).await.unwrap();
    let cached = store.load_keys("https://issuer.test").unwrap().unwrap();
    assert!(cached.has(issuer.kid()));
}

#[tokio::test]
async fn the_issuer_can_hand_back_a_renewed_token_at_a_check_in() {
    let w = world();
    let token = LicenseToken::new(w.issuer.issue(&Grant::for_product("acme/pro")).unwrap());
    let original = token.expose_secret().to_string();
    let jti = w.issuer.jti_of(&original).unwrap();
    w.service.install_token(token, None).await.unwrap();
    assert!(w.issuer.rotate_at_next_checkin(&jti));

    w.dial.advance_days(31);
    let outcome = w.service.checkin("acme/pro", false).await.unwrap();
    assert!(matches!(
        outcome,
        CheckinOutcome::Answered { active: true, .. }
    ));
    // The stored credential is the new one, and it still verifies and runs.
    assert_eq!(w.gate("acme/pro").await, Gate::Run);
    assert_eq!(w.state("acme/pro").await, LicenseState::Valid);
}

#[tokio::test]
async fn a_license_arrives_by_download_link_or_by_file() {
    let w = world();
    let token = w.issuer.issue(&Grant::for_product("acme/pro")).unwrap();
    let jti = w.issuer.jti_of(&token).unwrap();
    let summary = w
        .service
        .install_from_url(&format!("https://issuer.test/download/{jti}"))
        .await
        .unwrap();
    assert_eq!(summary.product, "acme/pro");
    assert_eq!(summary.state, "valid");
    // A link that names nothing is an issuer error, not a panic.
    let missing = w
        .service
        .install_from_url("https://issuer.test/download/nope")
        .await;
    assert!(matches!(
        missing,
        Err(LicenseError::Issuer { status: 404, .. })
    ));

    // The same token, delivered as the `.jwt` file a customer is emailed. It is written
    // owner-only and removed with the scratch directory at the end of the test.
    let dir = scratch("install-file");
    let path = dir.join("acme-pro.jwt");
    let second = w.issuer.issue(&Grant::for_product("acme/other")).unwrap();
    write_private(&path, second.as_bytes()).unwrap();
    let from_file = w.service.install_file(&path).await.unwrap();
    assert_eq!(from_file.product, "acme/other");
    assert_eq!(w.service.list().await.unwrap().len(), 2);
    std::fs::remove_dir_all(&dir).unwrap();

    // A file that is not a JWT at all is malformed, not a crash.
    let dir = scratch("install-junk");
    let junk = dir.join("junk.jwt");
    std::fs::write(&junk, b"not a token").unwrap();
    assert!(matches!(
        w.service.install_file(&junk).await,
        Err(LicenseError::Verify(_))
    ));
    assert!(matches!(
        w.service.install_file(&dir.join("absent.jwt")).await,
        Err(LicenseError::Io { .. })
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn the_device_flow_pends_then_installs_what_the_user_approved() {
    let w = world();
    let flow = w
        .service
        .device_flow_start("https://issuer.test/", "acme/pro")
        .await
        .unwrap();
    assert_eq!(flow.product, "acme/pro");
    assert!(flow.verification_uri.starts_with("https://issuer.test/"));
    assert_eq!(
        flow.verification_uri_complete.as_deref(),
        Some(format!("https://issuer.test/activate?user_code={}", flow.user_code).as_str())
    );
    assert!(flow.interval >= 1);
    assert_eq!(
        w.service.device_flow_poll(&flow.code).await.unwrap(),
        DevicePoll::Pending
    );
    // An issuer that thinks we are polling too fast asks us to back off, which is not an
    // error and does not end the flow.
    w.issuer.throttle_next_poll();
    assert_eq!(
        w.service.device_flow_poll(&flow.code).await.unwrap(),
        DevicePoll::SlowDown
    );

    assert!(w.issuer.approve_device(&flow.user_code));
    match w.service.device_flow_poll(&flow.code).await.unwrap() {
        DevicePoll::Installed { license } => {
            assert_eq!(license.product, "acme/pro");
            assert_eq!(license.state, "valid");
        }
        other => panic!("expected an install, got {other:?}"),
    }
    assert_eq!(w.gate("acme/pro").await, Gate::Run);
    // The flow is spent: polling it again does not re-install.
    assert!(matches!(
        w.service.device_flow_poll(&flow.code).await,
        Err(LicenseError::UnknownFlow(_))
    ));
}

#[tokio::test]
async fn a_refused_or_abandoned_device_flow_ends_without_a_license() {
    let w = world();
    let flow = w
        .service
        .device_flow_start("https://issuer.test", "acme/pro")
        .await
        .unwrap();
    assert!(w.issuer.deny_device(&flow.user_code));
    assert_eq!(
        w.service.device_flow_poll(&flow.code).await.unwrap(),
        DevicePoll::Denied
    );
    assert!(matches!(w.gate("acme/pro").await, Gate::Refuse(_)));

    // A flow nobody ever answers lapses.
    let flow = w
        .service
        .device_flow_start("https://issuer.test", "acme/pro")
        .await
        .unwrap();
    w.dial.set(T0 + DEVICE_CODE_TTL + 1);
    assert_eq!(
        w.service.device_flow_poll(&flow.code).await.unwrap(),
        DevicePoll::Expired
    );
    assert!(matches!(
        w.service.device_flow_poll(&flow.code).await,
        Err(LicenseError::UnknownFlow(_))
    ));
}

#[tokio::test]
async fn a_device_flow_for_a_bad_product_or_the_wrong_issuer_never_starts() {
    let w = world();
    assert!(matches!(
        w.service
            .device_flow_start("https://issuer.test", "nope")
            .await,
        Err(LicenseError::Malformed(_))
    ));
    w.service.register_issuer("acme/pro", "https://issuer.test");
    assert!(matches!(
        w.service
            .device_flow_start("https://rogue.test", "acme/pro")
            .await,
        Err(LicenseError::IssuerMismatch { .. })
    ));
}

#[tokio::test]
async fn checking_in_everything_visits_only_what_is_due() {
    let w = world();
    w.install(&Grant::for_product("acme/pro").checking_in_every(7))
        .await;
    w.install(&Grant::for_product("acme/lite").checking_in_every(90))
        .await;
    w.dial.advance_days(8);
    let mut results = w.service.checkin_all().await;
    results.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "acme/lite");
    assert_eq!(*results[0].1.as_ref().unwrap(), CheckinOutcome::NotDue);
    assert_eq!(results[1].0, "acme/pro");
    assert!(matches!(
        results[1].1.as_ref().unwrap(),
        CheckinOutcome::Answered { active: true, .. }
    ));
}

#[tokio::test]
async fn a_removed_license_is_gone_and_the_gate_closes() {
    let w = world();
    w.install(&Grant::for_product("acme/pro")).await;
    assert!(w.service.revoke_local("acme/pro").unwrap());
    assert!(!w.service.revoke_local("acme/pro").unwrap());
    assert!(w.service.list().await.unwrap().is_empty());
    assert!(matches!(w.gate("acme/pro").await, Gate::Refuse(_)));
}

#[tokio::test]
async fn a_license_stored_on_disk_survives_a_restart() {
    let dir = scratch("restart");
    let dial = Dial::new(T0);
    let issuer = Arc::new(StubIssuer::with_clock("https://issuer.test", dial.clock()));
    let http: Arc<dyn LicenseHttp> = issuer.clone();
    let first = LicenseService::new(
        Arc::new(FileLicenseStore::under(&dir)),
        http.clone(),
        dial.clock(),
    );
    first
        .install_token(
            LicenseToken::new(issuer.issue(&Grant::for_product("acme/pro")).unwrap()),
            None,
        )
        .await
        .unwrap();
    drop(first);

    // A new process, same directory.
    let second = LicenseService::new(Arc::new(FileLicenseStore::under(&dir)), http, dial.clock());
    assert_eq!(second.gate("acme/pro").await, Gate::Run);
    let summary = second.show("acme/pro").await.unwrap();
    assert_eq!(summary.issuer, "https://issuer.test");
    assert_eq!(summary.state, "valid");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_gate_serialises_as_a_tagged_reason() {
    let run = serde_json::to_value(Gate::Run).unwrap();
    assert_eq!(run, serde_json::json!({ "gate": "run" }));
    let banner = serde_json::to_value(Gate::RunWithBanner("please check in".into())).unwrap();
    assert_eq!(
        banner,
        serde_json::json!({ "gate": "run_with_banner", "reason": "please check in" })
    );
    assert!(Gate::Run.allows_run());
    assert!(Gate::RunWithBanner(String::new()).allows_run());
    assert!(!Gate::Refuse(String::new()).allows_run());
}
