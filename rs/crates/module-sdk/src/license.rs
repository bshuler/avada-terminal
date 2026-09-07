//! License tokens for commercial modules.
//!
//! The license server at the manifest's `distribution.issuer` speaks the OAuth
//! 2.0 device-authorization profile: the host opens a browser, the user signs in
//! and picks a seat, the host receives a signed JWT ([`LicenseClaims`]) and stores
//! it in the keychain. At most every `checkin_interval_days` the host calls the
//! issuer's introspection endpoint; the module keeps running through a grace
//! period if that call fails, and stops if the issuer says revoked.
//!
//! This module holds the *claims* and the *state machine*. Signature verification
//! (Ed25519 over the JWT) lives in core with its own dependency, so the SDK stays
//! free of crypto beyond HMAC.

use serde::{Deserialize, Serialize};

/// Longest allowed check-in interval; anything larger is clamped.
pub const MAX_CHECKIN_DAYS: u32 = 365;
/// Days a valid license keeps working after the issuer stops answering.
pub const GRACE_DAYS: u32 = 14;

/// The JWT payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseClaims {
    /// Token id; the introspection key.
    pub jti: String,
    /// Product id: the module id (`owner/repo`) or `avada/terminal` for the host.
    pub product: String,
    /// Licensee display (an email or org name).
    pub licensee: String,
    /// Seats covered.
    pub seats: u32,
    /// Not before, unix seconds.
    pub nbf: u64,
    /// Expiry, unix seconds. `0` means perpetual for the purchased major version.
    pub exp: u64,
    /// Highest major version this license covers (perpetual licenses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_major: Option<u64>,
    /// Signing key id.
    pub kid: String,
    /// Where the host may fetch the binary release for this product.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    /// Days between introspection calls.
    #[serde(default = "default_checkin")]
    pub checkin_interval_days: u32,
}

fn default_checkin() -> u32 {
    30
}

impl LicenseClaims {
    /// The check-in interval, clamped to `1..=MAX_CHECKIN_DAYS`.
    pub fn checkin_days(&self) -> u32 {
        self.checkin_interval_days.clamp(1, MAX_CHECKIN_DAYS)
    }
    /// Whether this license covers a module at `major`.
    pub fn covers_major(&self, major: u64) -> bool {
        self.max_major.is_none_or(|m| major <= m)
    }
}

/// What the issuer says when asked about a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrospectionResponse {
    /// RFC 7662 `active`.
    pub active: bool,
    /// Optional reason when inactive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A refreshed token, if the issuer rotated it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// The state the host computes at each launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseState {
    /// Runs.
    Valid,
    /// Runs, with a banner: the issuer has not answered for a while.
    GracePeriod,
    /// Stops: `exp` passed.
    Expired,
    /// Stops: the issuer said so.
    Revoked,
    /// Stops: the issuer has not answered for longer than the grace period.
    StaleCheckin,
    /// Stops: not yet valid.
    NotYet,
}

impl LicenseState {
    /// Whether the module may run.
    pub fn allows_run(self) -> bool {
        matches!(self, LicenseState::Valid | LicenseState::GracePeriod)
    }
}

/// What the host remembers between launches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckinRecord {
    /// Unix seconds of the last successful introspection.
    pub last_ok: u64,
    /// True once the issuer has answered `active: false`.
    pub revoked: bool,
}

/// Compute the state. `now` in unix seconds.
pub fn evaluate(claims: &LicenseClaims, checkin: &CheckinRecord, now: u64) -> LicenseState {
    if checkin.revoked {
        return LicenseState::Revoked;
    }
    if now < claims.nbf {
        return LicenseState::NotYet;
    }
    if claims.exp != 0 && now >= claims.exp {
        return LicenseState::Expired;
    }
    let day = 86_400u64;
    let due = checkin.last_ok + u64::from(claims.checkin_days()) * day;
    if now < due {
        return LicenseState::Valid;
    }
    if now < due + u64::from(GRACE_DAYS) * day {
        return LicenseState::GracePeriod;
    }
    LicenseState::StaleCheckin
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> LicenseClaims {
        LicenseClaims {
            jti: "t1".into(),
            product: "acme/avada-pro".into(),
            licensee: "someone@example.com".into(),
            seats: 1,
            nbf: 1_000,
            exp: 100_000_000,
            max_major: Some(2),
            kid: "k2026".into(),
            download_url: Some("https://avada.to/dl/acme/avada-pro".into()),
            checkin_interval_days: 30,
        }
    }

    #[test]
    fn serde_defaults_and_clamps() {
        let json =
            r#"{"jti":"a","product":"x/y","licensee":"l","seats":1,"nbf":0,"exp":0,"kid":"k"}"#;
        let c: LicenseClaims = serde_json::from_str(json).unwrap();
        assert_eq!(c.checkin_interval_days, 30);
        assert!(c.covers_major(999), "no max_major covers everything");
        let mut c = claims();
        c.checkin_interval_days = 9_999;
        assert_eq!(c.checkin_days(), MAX_CHECKIN_DAYS);
        c.checkin_interval_days = 0;
        assert_eq!(c.checkin_days(), 1);
        assert!(c.covers_major(2) && !c.covers_major(3));
        let back: LicenseClaims =
            serde_json::from_str(&serde_json::to_string(&claims()).unwrap()).unwrap();
        assert_eq!(back, claims());
    }

    #[test]
    fn state_machine() {
        let c = claims();
        let day = 86_400;
        let ok = CheckinRecord {
            last_ok: 10_000,
            revoked: false,
        };
        assert_eq!(evaluate(&c, &ok, 500), LicenseState::NotYet);
        assert_eq!(evaluate(&c, &ok, 10_000), LicenseState::Valid);
        assert_eq!(evaluate(&c, &ok, 10_000 + 29 * day), LicenseState::Valid);
        assert_eq!(
            evaluate(&c, &ok, 10_000 + 30 * day),
            LicenseState::GracePeriod
        );
        assert_eq!(
            evaluate(&c, &ok, 10_000 + 43 * day),
            LicenseState::GracePeriod
        );
        assert_eq!(
            evaluate(&c, &ok, 10_000 + 44 * day),
            LicenseState::StaleCheckin
        );
        assert_eq!(evaluate(&c, &ok, 100_000_000), LicenseState::Expired);
        let revoked = CheckinRecord {
            last_ok: 10_000,
            revoked: true,
        };
        assert_eq!(evaluate(&c, &revoked, 10_000), LicenseState::Revoked);
        let mut perpetual = claims();
        perpetual.exp = 0;
        let fresh = CheckinRecord {
            last_ok: 5_000_000,
            revoked: false,
        };
        assert_eq!(evaluate(&perpetual, &fresh, 5_000_000), LicenseState::Valid);
        for s in [LicenseState::Valid, LicenseState::GracePeriod] {
            assert!(s.allows_run());
        }
        for s in [
            LicenseState::Expired,
            LicenseState::Revoked,
            LicenseState::StaleCheckin,
            LicenseState::NotYet,
        ] {
            assert!(!s.allows_run());
        }
    }

    #[test]
    fn introspection_shape() {
        let r: IntrospectionResponse =
            serde_json::from_str(r#"{"active":false,"reason":"refunded"}"#).unwrap();
        assert!(!r.active);
        assert_eq!(r.reason.as_deref(), Some("refunded"));
        assert_eq!(
            serde_json::to_string(&IntrospectionResponse {
                active: true,
                reason: None,
                token: None
            })
            .unwrap(),
            r#"{"active":true}"#
        );
    }
}
