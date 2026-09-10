//! The synchronous face of licensing that the module host can actually call.
//!
//! [`LicenseService::gate_for_major`] is `async` because verifying a token may need the
//! issuer's JWKS, and fetching that is network I/O. `module::host::Slot::start` is
//! synchronous and runs on whichever thread asked for the module, so it cannot await
//! anything: a spawn that waited on an issuer would stall the caller whenever the
//! network is slow, and would refuse on a laptop that is merely offline --- the exact
//! case offline verification exists to serve.
//!
//! [`CachedGate`] bridges the two. It holds the answers `LicenseService` has already
//! given, hands them out synchronously, and is refreshed from async code: at startup,
//! after a licence is installed or removed, and on whatever check-in schedule the app
//! keeps.

use super::{Gate, LicenseService};
use crate::module::host::Licensing;
use avada_module_sdk::ModuleId;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A [`Licensing`] gate answering from decisions [`LicenseService`] made earlier.
///
/// Keyed by `(product, major)` because a licence may cover majors up to some bound, so
/// the same product can be licensed at one major and not the next.
pub struct CachedGate {
    service: Arc<LicenseService>,
    decided: Mutex<BTreeMap<(String, u64), Gate>>,
}

/// What a miss answers, so the message names its own cause instead of reading like a
/// licence problem.
fn undecided(product: &str) -> Gate {
    Gate::Refuse(format!(
        "no license decision for {product} yet; the host has a license gate but nothing \
         has refreshed it"
    ))
}

/// What a decision outlived by its own licence answers.
///
/// Distinct from [`undecided`] because the cause is: there *was* a decision, and the licence
/// it was decided from is gone. "Nothing has refreshed it" would send the reader looking for
/// a bug in the refresh path instead of at the removal they just performed.
fn removed(product: &str) -> Gate {
    Gate::Refuse(format!(
        "no license for {product} on this machine; it was removed after this decision was \
         cached"
    ))
}

impl CachedGate {
    /// A gate with nothing decided yet. Every commercial module is refused until
    /// [`refresh`](Self::refresh) has been called for it.
    pub fn new(service: Arc<LicenseService>) -> Arc<CachedGate> {
        Arc::new(CachedGate {
            service,
            decided: Mutex::new(BTreeMap::new()),
        })
    }

    /// Ask the service about `product` at `major` and remember the answer.
    pub async fn refresh(&self, product: &str, major: u64) -> Gate {
        let gate = self.service.gate_for_major(product, Some(major)).await;
        lock(&self.decided).insert((product.to_string(), major), gate.clone());
        gate
    }

    /// Refresh every `(product, major)` in one pass. This is what the app calls at
    /// startup, with one entry per installed commercial module.
    pub async fn refresh_all(&self, products: &[(String, u64)]) {
        for (product, major) in products {
            self.refresh(product, *major).await;
        }
    }

    /// Forget `product` entirely, at every major.
    ///
    /// A removed licence must not keep a stale `Run` alive, and re-refreshing would
    /// only give back a `Refuse` with a worse message than [`undecided`]'s.
    pub fn forget(&self, product: &str) {
        lock(&self.decided).retain(|(p, _), _| p != product);
    }

    /// The remembered answer for `(product, major)`, if there is one.
    pub fn decided(&self, product: &str, major: u64) -> Option<Gate> {
        lock(&self.decided)
            .get(&(product.to_string(), major))
            .cloned()
    }

    /// Is the licence behind a cached decision definitely absent from the store?
    ///
    /// `false` on a read error, deliberately. A store that cannot be read says nothing about
    /// whether a licence exists, and turning an unreadable directory into a refusal would
    /// disable a paying customer's module over a transient failure --- exactly the shape of
    /// outage the grace window exists to survive.
    fn gone(&self, product: &str) -> bool {
        matches!(self.service.store().load(product), Ok(None))
    }
}

impl Licensing for CachedGate {
    fn gate(&self, product: &ModuleId, major: u64) -> Gate {
        let product = product.to_string();
        let Some(decided) = lock(&self.decided).get(&(product.clone(), major)).cloned() else {
            return undecided(&product);
        };
        // A cached `Run` must not outlive the licence it was decided from. In-process,
        // `LicenseService::on_removed` calls `forget` the moment a licence goes --- but the
        // CLI removes licences in a *different process*, and the app runs a second
        // `LicenseService` over the same directory for its control routes, so neither of
        // those removals can reach this map. Without the check below the module the human
        // just unlicensed keeps running until the next hourly sweep.
        //
        // The cost is one small read at spawn time, which is the only time this is asked.
        if !matches!(decided, Gate::Refuse(_)) && self.gone(&product) {
            return removed(&product);
        }
        decided
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::super::stub_issuer::{Grant, StubIssuer};
    use super::super::{Clock, LicenseHttp, LicenseService, LicenseToken, MemoryLicenseStore};
    use super::*;

    const T0: u64 = 1_700_000_000;

    fn clock() -> Clock {
        Arc::new(|| T0)
    }

    /// A service over an in-process issuer and a store that never touches the disk.
    fn service() -> (Arc<StubIssuer>, Arc<LicenseService>) {
        let issuer = Arc::new(StubIssuer::with_clock("https://issuer.test", clock()));
        let http: Arc<dyn LicenseHttp> = issuer.clone();
        let service = LicenseService::new(Arc::new(MemoryLicenseStore::new()), http, clock());
        (issuer, Arc::new(service))
    }

    async fn install(issuer: &StubIssuer, service: &LicenseService, grant: Grant) {
        let token = LicenseToken::new(issuer.issue(&grant).expect("the stub signs"));
        service
            .install_token(token, None)
            .await
            .expect("a freshly minted license installs");
    }

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).unwrap()
    }

    #[tokio::test]
    async fn a_miss_refuses_and_says_the_cache_is_why() {
        let (_issuer, service) = service();
        let cache = CachedGate::new(service);
        assert_eq!(cache.decided("acme/pro", 1), None);
        match cache.gate(&id("acme/pro"), 1) {
            Gate::Refuse(why) => {
                assert!(why.contains("acme/pro"), "{why}");
                assert!(why.contains("refreshed"), "{why}");
            }
            other => panic!("a miss must refuse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_remembers_what_the_service_decided() {
        let (issuer, service) = service();
        install(
            &issuer,
            &service,
            Grant::for_product("acme/pro").up_to_major(2),
        )
        .await;
        let cache = CachedGate::new(service);
        assert_eq!(cache.refresh("acme/pro", 1).await, Gate::Run);
        assert_eq!(cache.decided("acme/pro", 1), Some(Gate::Run));
        assert_eq!(cache.gate(&id("acme/pro"), 1), Gate::Run);
    }

    #[tokio::test]
    async fn the_cache_is_keyed_by_major_so_one_major_does_not_answer_for_another() {
        let (issuer, service) = service();
        install(
            &issuer,
            &service,
            Grant::for_product("acme/pro").up_to_major(2),
        )
        .await;
        let cache = CachedGate::new(service);
        cache.refresh("acme/pro", 1).await;

        // Major 9 was never asked about: it misses rather than inheriting major 1's Run.
        match cache.gate(&id("acme/pro"), 9) {
            Gate::Refuse(why) => assert!(why.contains("refreshed"), "{why}"),
            other => panic!("{other:?}"),
        }
        // And once asked, the service refuses it on its own terms.
        match cache.refresh("acme/pro", 9).await {
            Gate::Refuse(why) => assert!(why.contains("major"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(cache.gate(&id("acme/pro"), 1), Gate::Run, "still licensed");
    }

    #[tokio::test]
    async fn refresh_all_decides_every_pair_it_is_given() {
        let (issuer, service) = service();
        install(&issuer, &service, Grant::for_product("acme/pro")).await;
        install(&issuer, &service, Grant::for_product("acme/team")).await;
        let cache = CachedGate::new(service);
        cache
            .refresh_all(&[
                ("acme/pro".to_string(), 1),
                ("acme/team".to_string(), 4),
                ("acme/nope".to_string(), 1),
            ])
            .await;
        assert_eq!(cache.gate(&id("acme/pro"), 1), Gate::Run);
        assert_eq!(cache.gate(&id("acme/team"), 4), Gate::Run);
        match cache.gate(&id("acme/nope"), 1) {
            // Decided, and by the service --- not the cache's own miss message.
            Gate::Refuse(why) => assert!(why.contains("no license installed"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn forget_drops_one_product_at_every_major_and_leaves_the_rest() {
        let (issuer, service) = service();
        install(&issuer, &service, Grant::for_product("acme/pro")).await;
        install(&issuer, &service, Grant::for_product("acme/team")).await;
        let cache = CachedGate::new(service);
        cache
            .refresh_all(&[
                ("acme/pro".to_string(), 1),
                ("acme/pro".to_string(), 2),
                ("acme/team".to_string(), 1),
            ])
            .await;

        cache.forget("acme/pro");
        assert_eq!(cache.decided("acme/pro", 1), None);
        assert_eq!(cache.decided("acme/pro", 2), None);
        assert_eq!(
            cache.decided("acme/team", 1),
            Some(Gate::Run),
            "forgetting one product must not clear another"
        );
    }
}
