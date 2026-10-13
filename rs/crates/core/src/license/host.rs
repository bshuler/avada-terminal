//! The commercial edition's own licence: the host as a licensed product.
//!
//! Every other licence in this module gates a *module*. This one gates the app. The
//! product is [`HOST_PRODUCT`] (`avada/terminal`, the id the SDK documents for the host),
//! issued by [`HOST_ISSUER`]. A free build asks for nothing; a commercial build runs only
//! while [`gate`] says so.
//!
//! The edition is an argument, not a `cfg!`, for the reason `edition.rs` gives: the free
//! build's own tests can drive the commercial branch, which is the only way it is tested
//! before a commercial binary ships. So is the issuer, so a test can bind the product to
//! the in-process stub instead of the real server.

use super::{Gate, LicenseError, LicenseService, LicenseSummary};
use crate::edition::Edition;
use std::path::Path;

/// The product id of the host itself. Matches `avada_module_sdk::license::LicenseClaims`'
/// documented "`avada/terminal` for the host".
pub const HOST_PRODUCT: &str = "avada/terminal";

/// The issuer every host licence is bound to. It is baked into each token, so the address
/// is permanent (see the `avada-license` README).
pub const HOST_ISSUER: &str = "https://license.avada.to";

/// Where a refused user is sent for a licence.
pub const GET_A_LICENSE_URL: &str = "https://avada.to/commercial";

/// Bind [`HOST_PRODUCT`] to `issuer`, so a token for it from anyone else is refused.
///
/// Binding is the security half: without it the service trusts whatever issuer the stored
/// token names, and a self-signed token would pass.
pub fn bind(service: &LicenseService, issuer: &str) {
    service.register_issuer(HOST_PRODUCT, issuer);
}

/// May this build run?
///
/// - `Free` is never asked: [`Gate::Run`].
/// - `Commercial` needs a licence for [`HOST_PRODUCT`] that verifies against `issuer`,
///   covers `major`, is inside its dates and has been checked in on schedule (the grace
///   window runs with a banner, then refuses).
///
/// A check-in is attempted first when one is due, so a licence the issuer has revoked stops
/// the app at its next launch rather than only after the grace window. An unreachable
/// issuer changes nothing: an offline laptop keeps running on the clock, which is what the
/// grace window is for.
pub async fn gate(edition: Edition, service: &LicenseService, issuer: &str, major: u64) -> Gate {
    if !edition.is_commercial() {
        return Gate::Run;
    }
    bind(service, issuer);
    // Best effort and not forced: `NotDue` and `Unreachable` are both fine, and a missing
    // licence is `gate_for_major`'s to report.
    let _ = service.checkin(HOST_PRODUCT, false).await;
    service.gate_for_major(HOST_PRODUCT, Some(major)).await
}

/// Install a licence file for the host, bound to `issuer` first.
pub async fn install(
    service: &LicenseService,
    issuer: &str,
    path: &Path,
) -> Result<LicenseSummary, LicenseError> {
    bind(service, issuer);
    let summary = service.install_file(path).await?;
    if summary.product != HOST_PRODUCT {
        // `install_file` stores whatever product the token names. A licence for another
        // product is not an error in general, but handing one to *this* command is a
        // mistake worth naming rather than a silent "installed" that unlocks nothing.
        return Err(LicenseError::Malformed(format!(
            "this licence is for {}, not {HOST_PRODUCT}",
            summary.product
        )));
    }
    Ok(summary)
}

/// The sentence a refused user reads. One place, so the dialog, stderr and the tests agree.
pub fn refusal_text(reason: &str) -> String {
    format!(
        "This is the commercial edition of Avada Terminal, and it needs a licence.\n\n\
         {reason}\n\n\
         Install one with:  avada --install-license <file>\n\
         Get a licence, or the free edition, at {GET_A_LICENSE_URL}"
    )
}

#[cfg(test)]
mod tests {
    use super::super::stub_issuer::{Grant, StubIssuer};
    use super::super::{Clock, LicenseHttp, LicenseToken, MemoryLicenseStore};
    use super::*;
    use std::sync::{Arc, Mutex};

    const T0: u64 = 1_700_000_000;
    const DAY: u64 = 86_400;
    const ISSUER: &str = "https://issuer.test";

    struct World {
        now: Arc<Mutex<u64>>,
        issuer: Arc<StubIssuer>,
        service: LicenseService,
    }

    fn world() -> World {
        let now = Arc::new(Mutex::new(T0));
        let clock: Clock = {
            let now = now.clone();
            Arc::new(move || *now.lock().unwrap())
        };
        let issuer = Arc::new(StubIssuer::with_clock(ISSUER, clock.clone()));
        let http: Arc<dyn LicenseHttp> = issuer.clone();
        let service = LicenseService::new(Arc::new(MemoryLicenseStore::new()), http, clock);
        World {
            now,
            issuer,
            service,
        }
    }

    impl World {
        fn token(&self, grant: &Grant) -> LicenseToken {
            LicenseToken::new(self.issuer.issue(grant).expect("the stub signs"))
        }

        async fn install(&self, grant: &Grant) {
            bind(&self.service, ISSUER);
            self.service
                .install_token(self.token(grant), None)
                .await
                .expect("installs");
        }

        async fn gate(&self, edition: Edition) -> Gate {
            gate(edition, &self.service, ISSUER, 1).await
        }

        fn advance_days(&self, d: u64) {
            *self.now.lock().unwrap() += d * DAY;
        }
    }

    fn refused(g: &Gate) -> &str {
        match g {
            Gate::Refuse(why) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_free_edition_never_asks_for_a_licence() {
        let w = world();
        assert_eq!(w.gate(Edition::Free).await, Gate::Run);
    }

    #[tokio::test]
    async fn the_commercial_edition_refuses_without_a_licence() {
        let w = world();
        let g = w.gate(Edition::Commercial).await;
        assert!(refused(&g).contains(HOST_PRODUCT), "{g:?}");
    }

    #[tokio::test]
    async fn the_commercial_edition_runs_with_a_host_licence() {
        let w = world();
        w.install(&Grant::for_product(HOST_PRODUCT)).await;
        assert_eq!(w.gate(Edition::Commercial).await, Gate::Run);
    }

    #[tokio::test]
    async fn a_licence_for_another_product_does_not_unlock_the_host() {
        let w = world();
        w.install(&Grant::for_product("acme/pro")).await;
        let g = w.gate(Edition::Commercial).await;
        assert!(refused(&g).contains(HOST_PRODUCT), "{g:?}");
    }

    #[tokio::test]
    async fn a_licence_from_an_unbound_issuer_is_refused() {
        let w = world();
        w.install(&Grant::for_product(HOST_PRODUCT)).await;
        let g = gate(Edition::Commercial, &w.service, "https://evil.test", 1).await;
        assert!(!g.allows_run(), "{g:?}");
    }

    #[tokio::test]
    async fn a_revoked_licence_stops_the_next_launch() {
        let w = world();
        let token = w.token(&Grant::for_product(HOST_PRODUCT));
        let jti = w.issuer.jti_of(token.expose_secret()).unwrap();
        bind(&w.service, ISSUER);
        w.service.install_token(token, None).await.unwrap();
        assert_eq!(w.gate(Edition::Commercial).await, Gate::Run);

        assert!(w.issuer.revoke(&jti));
        // The launch's own check-in is what learns of it, once one is due.
        w.advance_days(31);
        let g = w.gate(Edition::Commercial).await;
        assert!(refused(&g).contains("revoked"), "{g:?}");
    }

    #[tokio::test]
    async fn an_expired_licence_is_refused() {
        let w = world();
        w.install(&Grant::for_product(HOST_PRODUCT).expiring_at(T0 + DAY))
            .await;
        assert_eq!(w.gate(Edition::Commercial).await, Gate::Run);
        w.advance_days(2);
        let g = w.gate(Edition::Commercial).await;
        assert!(!g.allows_run(), "{g:?}");
    }

    #[tokio::test]
    async fn a_licence_for_an_older_major_does_not_cover_a_newer_one() {
        let w = world();
        w.install(&Grant::for_product(HOST_PRODUCT).up_to_major(1))
            .await;
        assert_eq!(
            gate(Edition::Commercial, &w.service, ISSUER, 1).await,
            Gate::Run
        );
        let g = gate(Edition::Commercial, &w.service, ISSUER, 2).await;
        assert!(!g.allows_run(), "{g:?}");
    }

    #[tokio::test]
    async fn install_refuses_a_licence_for_another_product() {
        let w = world();
        let dir = std::env::temp_dir().join(format!("avada-host-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("other.jwt");
        std::fs::write(
            &file,
            w.token(&Grant::for_product("acme/pro")).expose_secret(),
        )
        .unwrap();
        let err = install(&w.service, ISSUER, &file).await.unwrap_err();
        assert!(err.to_string().contains(HOST_PRODUCT), "{err}");

        std::fs::write(
            &file,
            w.token(&Grant::for_product(HOST_PRODUCT)).expose_secret(),
        )
        .unwrap();
        let ok = install(&w.service, ISSUER, &file).await.unwrap();
        assert_eq!(ok.product, HOST_PRODUCT);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_refusal_names_the_reason_and_the_way_out() {
        let t = refusal_text("no licence");
        assert!(t.contains("no licence"));
        assert!(t.contains("--install-license"));
        assert!(t.contains(GET_A_LICENSE_URL));
    }
}
