//! Talking to an issuer: RFC 8414 discovery, RFC 7662 introspection, and RFC 9701
//! signed introspection answers.
//!
//! Three things happen over the network and nothing else does: the metadata document
//! is fetched to learn the endpoints, the key set is fetched so tokens can be verified
//! offline afterwards, and a check-in asks whether a token is still active.
//!
//! A check-in is the only thing that can turn a working license off, so its answer is
//! held to the same bar as the license itself: when the issuer advertises signed
//! introspection (RFC 9701), an unsigned answer is refused rather than believed. TLS
//! alone would make the answer only as trustworthy as the connection.

use super::{issuer_error, norm_issuer, LicenseError, LicenseHttp, LicenseToken, CLIENT_ID};
use crate::license::verify::{Jwks, Verifier};
use avada_module_sdk::license::{CheckinRecord, IntrospectionResponse, LicenseClaims};
use serde::{Deserialize, Serialize};

/// RFC 8414 §3: the authorization server metadata path.
pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
/// The OpenID Connect spelling, tried when the RFC 8414 path is not there.
pub const OIDC_METADATA_PATH: &str = "/.well-known/openid-configuration";
/// The default key set path, used when the metadata names none.
pub const JWKS_PATH: &str = "/.well-known/jwks.json";
/// RFC 9701 §4: the media type of a signed introspection answer.
pub const INTROSPECTION_JWT: &str = "application/token-introspection+jwt";
/// A day, in seconds.
const DAY: u64 = 86_400;

/// An issuer's metadata document (RFC 8414 §2). Only the members the host uses are
/// modelled; the rest of the document is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovery {
    /// The issuer identifier the document claims to describe.
    #[serde(default)]
    pub issuer: String,
    /// Where the signing keys are (RFC 7517).
    #[serde(default)]
    pub jwks_uri: String,
    /// RFC 7662 introspection endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introspection_endpoint: Option<String>,
    /// RFC 8628 §3.1 device authorization endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_authorization_endpoint: Option<String>,
    /// The token endpoint the device grant is redeemed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// RFC 9701 §5: the algorithms the issuer will sign introspection answers with.
    /// A non-empty list is a promise, and this client holds the issuer to it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub introspection_signing_alg_values_supported: Vec<String>,
}

impl Discovery {
    /// The document an issuer at `base` would publish if it used every default path.
    /// The stub issuer serves this; a real issuer's document replaces it.
    pub fn for_issuer(base: &str) -> Self {
        let base = norm_issuer(base);
        Discovery {
            jwks_uri: format!("{base}{JWKS_PATH}"),
            introspection_endpoint: Some(format!("{base}/introspect")),
            device_authorization_endpoint: Some(format!("{base}/device_authorization")),
            token_endpoint: Some(format!("{base}/token")),
            introspection_signing_alg_values_supported: vec!["EdDSA".into()],
            issuer: base,
        }
    }

    /// Whether the issuer promised signed introspection answers.
    pub fn signs_introspection(&self) -> bool {
        !self.introspection_signing_alg_values_supported.is_empty()
    }
}

/// Fetch an issuer's metadata. The RFC 8414 path first, then the OpenID Connect one,
/// so an issuer that only publishes the latter still works. `jwks_uri` is filled in
/// with the default path when the document omits it.
pub async fn discover(http: &dyn LicenseHttp, issuer: &str) -> Result<Discovery, LicenseError> {
    let base = norm_issuer(issuer);
    if base.is_empty() {
        return Err(LicenseError::Malformed("empty issuer URL".into()));
    }
    let mut last = None;
    for path in [METADATA_PATH, OIDC_METADATA_PATH] {
        let resp = http.get(format!("{base}{path}")).await?;
        if !resp.is_success() {
            last = Some(issuer_error(&resp));
            continue;
        }
        let mut doc: Discovery = serde_json::from_slice(&resp.body)
            .map_err(|e| LicenseError::Malformed(format!("issuer metadata: {e}")))?;
        // RFC 8414 §3.3: the document must describe the issuer it was fetched from.
        // Without this check a redirect could hand us another server's endpoints.
        if !doc.issuer.is_empty() && norm_issuer(&doc.issuer) != base {
            return Err(LicenseError::IssuerMismatch {
                product: String::new(),
                expected: base,
                actual: norm_issuer(&doc.issuer),
            });
        }
        if doc.jwks_uri.trim().is_empty() {
            doc.jwks_uri = format!("{base}{JWKS_PATH}");
        }
        if doc.issuer.is_empty() {
            doc.issuer = base;
        }
        return Ok(doc);
    }
    Err(last.unwrap_or_else(|| LicenseError::Malformed("issuer publishes no metadata".into())))
}

/// Fetch a key set.
pub async fn fetch_jwks(http: &dyn LicenseHttp, url: &str) -> Result<Jwks, LicenseError> {
    let resp = http.get(url.to_string()).await?;
    if !resp.is_success() {
        return Err(issuer_error(&resp));
    }
    let keys: Jwks = serde_json::from_slice(&resp.body)
        .map_err(|e| LicenseError::Malformed(format!("issuer key set: {e}")))?;
    if keys.keys.iter().all(|k| !k.is_ed25519()) {
        return Err(LicenseError::Malformed(
            "issuer key set holds no Ed25519 key".into(),
        ));
    }
    Ok(keys)
}

/// Whether `checkin_interval_days` have passed since the last confirmed check-in.
/// A revoked record is still due: an issuer may reinstate a license, and the state
/// machine keeps refusing to run until it does.
pub fn checkin_due(claims: &LicenseClaims, checkin: &CheckinRecord, now: u64) -> bool {
    now >= checkin
        .last_ok
        .saturating_add(u64::from(claims.checkin_days()).saturating_mul(DAY))
}

/// The record a check-in leaves behind.
///
/// An inactive answer sets `revoked` and leaves `last_ok` alone: the moment the
/// license stopped being good is not a successful check-in. An active answer stamps
/// `now`, but never earlier than the stamp already there and never past the license's
/// own `exp` — a clock that jumped backwards must not shorten a window already earned,
/// and one that jumped forward must not buy grace beyond the license.
pub fn apply(
    prev: &CheckinRecord,
    claims: &LicenseClaims,
    answer: &IntrospectionResponse,
    now: u64,
) -> CheckinRecord {
    if !answer.active {
        return CheckinRecord {
            last_ok: prev.last_ok,
            revoked: true,
        };
    }
    let capped = if claims.exp != 0 {
        now.min(claims.exp)
    } else {
        now
    };
    CheckinRecord {
        last_ok: capped.max(prev.last_ok),
        revoked: false,
    }
}

/// The RFC 7662 client. Borrows the transport and the verifier; holds no state.
pub struct IntrospectionClient<'a> {
    http: &'a dyn LicenseHttp,
    verifier: &'a Verifier,
}

impl<'a> IntrospectionClient<'a> {
    /// A client over this transport and verifier.
    pub fn new(http: &'a dyn LicenseHttp, verifier: &'a Verifier) -> Self {
        IntrospectionClient { http, verifier }
    }

    /// Ask the issuer about `token`. The token goes out twice, as RFC 7662 §2.1 wants:
    /// as the `token` form field (what is being asked about) and as the bearer
    /// credential (who is asking). It is in nothing this returns.
    pub async fn introspect(
        &self,
        discovery: &Discovery,
        issuer: &str,
        token: &LicenseToken,
        keys: &Jwks,
    ) -> Result<IntrospectionResponse, LicenseError> {
        let endpoint = discovery.introspection_endpoint.clone().ok_or_else(|| {
            LicenseError::Malformed("issuer offers no introspection endpoint".into())
        })?;
        let resp = self
            .http
            .post_form(
                endpoint,
                Some(token.clone()),
                vec![
                    ("token".into(), token.expose_secret().to_string()),
                    ("token_type_hint".into(), "access_token".into()),
                    ("client_id".into(), CLIENT_ID.into()),
                ],
                format!("{INTROSPECTION_JWT}, application/json"),
            )
            .await?;
        if !resp.is_success() {
            return Err(issuer_error(&resp));
        }
        let text = resp.text();
        let body = text.trim();
        if body.starts_with('{') {
            if discovery.signs_introspection() {
                return Err(LicenseError::Malformed(
                    "issuer advertises signed introspection but answered unsigned".into(),
                ));
            }
            return serde_json::from_str(body)
                .map_err(|e| LicenseError::Malformed(format!("introspection answer: {e}")));
        }
        self.verify_answer(body, issuer, keys)
    }

    /// An RFC 9701 signed answer: an EdDSA JWT from the issuer whose
    /// `token_introspection` claim holds the RFC 7662 object.
    fn verify_answer(
        &self,
        jwt: &str,
        issuer: &str,
        keys: &Jwks,
    ) -> Result<IntrospectionResponse, LicenseError> {
        let (_, value): (_, serde_json::Value) = self.verifier.decode(jwt, keys)?;
        let claimed = value
            .get("iss")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if !claimed.is_empty() && norm_issuer(claimed) != norm_issuer(issuer) {
            return Err(LicenseError::IssuerMismatch {
                product: String::new(),
                expected: norm_issuer(issuer),
                actual: norm_issuer(claimed),
            });
        }
        let inner = value.get("token_introspection").cloned().unwrap_or(value);
        serde_json::from_value(inner)
            .map_err(|e| LicenseError::Malformed(format!("signed introspection answer: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::license::verify::SigningKey;
    use crate::license::{HttpFuture, HttpResponse};
    use std::sync::Mutex;

    /// A transport that answers from a script keyed by URL, and remembers what it was
    /// asked. Nothing here reaches the network.
    #[derive(Default)]
    struct ScriptHttp {
        answers: Mutex<Vec<(String, HttpResponse)>>,
        seen: Mutex<Vec<String>>,
        forms: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl ScriptHttp {
        fn with(mut pairs: Vec<(&str, u16, String)>) -> Self {
            let answers = pairs
                .drain(..)
                .map(|(u, status, body)| {
                    (
                        u.to_string(),
                        HttpResponse {
                            status,
                            body: body.into_bytes(),
                        },
                    )
                })
                .collect();
            ScriptHttp {
                answers: Mutex::new(answers),
                ..Default::default()
            }
        }

        fn answer(&self, url: &str) -> HttpResponse {
            self.seen.lock().unwrap().push(url.to_string());
            self.answers
                .lock()
                .unwrap()
                .iter()
                .find(|(u, _)| u == url)
                .map(|(_, r)| r.clone())
                .unwrap_or(HttpResponse {
                    status: 404,
                    body: br#"{"error":"not_found"}"#.to_vec(),
                })
        }
    }

    impl LicenseHttp for ScriptHttp {
        fn get(&self, url: String) -> HttpFuture<'_> {
            Box::pin(async move { Ok(self.answer(&url)) })
        }
        fn post_form(
            &self,
            url: String,
            _bearer: Option<LicenseToken>,
            form: Vec<(String, String)>,
            _accept: String,
        ) -> HttpFuture<'_> {
            Box::pin(async move {
                self.forms.lock().unwrap().push(form);
                Ok(self.answer(&url))
            })
        }
    }

    fn claims(exp: u64) -> LicenseClaims {
        LicenseClaims {
            jti: "j1".into(),
            product: "acme/pro".into(),
            licensee: "dev@example.test".into(),
            seats: 1,
            nbf: 0,
            exp,
            max_major: None,
            kid: String::new(),
            download_url: None,
            checkin_interval_days: 30,
        }
    }

    #[tokio::test]
    async fn discovery_falls_back_to_the_openid_path_and_the_default_jwks_uri() {
        let http = ScriptHttp::with(vec![(
            "https://issuer.test/.well-known/openid-configuration",
            200,
            r#"{"issuer":"https://issuer.test/","introspection_endpoint":"https://issuer.test/i"}"#
                .into(),
        )]);
        let doc = discover(&http, "https://issuer.test/").await.unwrap();
        assert_eq!(doc.jwks_uri, "https://issuer.test/.well-known/jwks.json");
        assert_eq!(
            doc.introspection_endpoint.as_deref(),
            Some("https://issuer.test/i")
        );
        assert!(!doc.signs_introspection());
        let seen = http.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "the RFC 8414 path is tried first: {seen:?}");
    }

    #[tokio::test]
    async fn a_metadata_document_for_another_issuer_is_refused() {
        let http = ScriptHttp::with(vec![(
            "https://issuer.test/.well-known/oauth-authorization-server",
            200,
            r#"{"issuer":"https://evil.test","jwks_uri":"https://evil.test/jwks"}"#.into(),
        )]);
        let e = discover(&http, "https://issuer.test").await.unwrap_err();
        assert!(matches!(e, LicenseError::IssuerMismatch { .. }), "{e:?}");
    }

    #[tokio::test]
    async fn a_key_set_without_an_ed25519_key_is_refused() {
        let http = ScriptHttp::with(vec![(
            "https://issuer.test/jwks",
            200,
            r#"{"keys":[{"kty":"RSA","kid":"r1"}]}"#.into(),
        )]);
        let e = fetch_jwks(&http, "https://issuer.test/jwks")
            .await
            .unwrap_err();
        assert!(matches!(e, LicenseError::Malformed(_)), "{e:?}");
    }

    #[tokio::test]
    async fn a_signed_answer_verifies_and_an_unsigned_one_is_refused_when_promised() {
        let key = SigningKey::generate_with_kid("k1");
        let keys = Jwks {
            keys: vec![key.jwk()],
        };
        let signed = key
            .sign(
                &serde_json::json!({
                    "iss": "https://issuer.test",
                    "aud": CLIENT_ID,
                    "iat": 1_000,
                    "token_introspection": { "active": true },
                }),
                "token-introspection+jwt",
            )
            .unwrap();
        let discovery = Discovery::for_issuer("https://issuer.test");
        let verifier = Verifier::default();
        let token = LicenseToken::new("a.b.c");

        let http = ScriptHttp::with(vec![("https://issuer.test/introspect", 200, signed)]);
        let answer = IntrospectionClient::new(&http, &verifier)
            .introspect(&discovery, "https://issuer.test", &token, &keys)
            .await
            .unwrap();
        assert!(answer.active);
        // RFC 7662 §2.1: the token is what is being asked about, so it is the form field.
        let form = http.forms.lock().unwrap()[0].clone();
        assert!(form.iter().any(|(k, _)| k == "token"));

        let plain = ScriptHttp::with(vec![(
            "https://issuer.test/introspect",
            200,
            r#"{"active":false}"#.into(),
        )]);
        let e = IntrospectionClient::new(&plain, &verifier)
            .introspect(&discovery, "https://issuer.test", &token, &keys)
            .await
            .unwrap_err();
        assert!(
            matches!(&e, LicenseError::Malformed(m) if m.contains("unsigned")),
            "an issuer that promised signatures must not be believed unsigned: {e:?}"
        );
    }

    #[tokio::test]
    async fn a_signed_answer_from_the_wrong_issuer_is_refused() {
        let key = SigningKey::generate_with_kid("k1");
        let keys = Jwks {
            keys: vec![key.jwk()],
        };
        let signed = key
            .sign(
                &serde_json::json!({
                    "iss": "https://evil.test",
                    "token_introspection": { "active": false, "reason": "revoked" },
                }),
                "token-introspection+jwt",
            )
            .unwrap();
        let http = ScriptHttp::with(vec![("https://issuer.test/introspect", 200, signed)]);
        let verifier = Verifier::default();
        let e = IntrospectionClient::new(&http, &verifier)
            .introspect(
                &Discovery::for_issuer("https://issuer.test"),
                "https://issuer.test",
                &LicenseToken::new("a.b.c"),
                &keys,
            )
            .await
            .unwrap_err();
        assert!(matches!(e, LicenseError::IssuerMismatch { .. }), "{e:?}");
    }

    #[tokio::test]
    async fn an_answer_signed_by_a_stranger_is_refused() {
        let issuer_key = SigningKey::generate_with_kid("k1");
        let stranger = SigningKey::generate_with_kid("k1");
        let signed = stranger
            .sign(
                &serde_json::json!({ "token_introspection": { "active": false } }),
                "token-introspection+jwt",
            )
            .unwrap();
        let http = ScriptHttp::with(vec![("https://issuer.test/introspect", 200, signed)]);
        let verifier = Verifier::default();
        let e = IntrospectionClient::new(&http, &verifier)
            .introspect(
                &Discovery::for_issuer("https://issuer.test"),
                "https://issuer.test",
                &LicenseToken::new("a.b.c"),
                &Jwks {
                    keys: vec![issuer_key.jwk()],
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                e,
                LicenseError::Verify(crate::license::VerifyError::BadSignature)
            ),
            "{e:?}"
        );
    }

    #[test]
    fn due_dates_and_the_record_a_check_in_leaves() {
        let c = claims(0);
        let rec = CheckinRecord {
            last_ok: 1_000,
            revoked: false,
        };
        assert!(!checkin_due(&c, &rec, 1_000 + 29 * DAY));
        assert!(checkin_due(&c, &rec, 1_000 + 30 * DAY));
        let revoked = CheckinRecord {
            last_ok: 1_000,
            revoked: true,
        };
        assert!(
            checkin_due(&c, &revoked, 1_000 + 30 * DAY),
            "a revoked license still asks, so the issuer can reinstate it"
        );

        let active = IntrospectionResponse {
            active: true,
            reason: None,
            token: None,
        };
        let inactive = IntrospectionResponse {
            active: false,
            reason: Some("seat released".into()),
            token: None,
        };
        assert_eq!(
            apply(&rec, &c, &active, 5_000),
            CheckinRecord {
                last_ok: 5_000,
                revoked: false
            }
        );
        assert_eq!(
            apply(&rec, &c, &active, 500).last_ok,
            1_000,
            "a clock that went backwards must not shorten the window"
        );
        assert_eq!(
            apply(&rec, &claims(2_000), &active, 9_999).last_ok,
            2_000,
            "a clock that ran ahead must not buy grace past exp"
        );
        assert_eq!(
            apply(&rec, &c, &inactive, 5_000),
            CheckinRecord {
                last_ok: 1_000,
                revoked: true
            },
            "the moment a license stopped being good is not a successful check-in"
        );
        assert!(
            !apply(&rec, &c, &active, 5_000).revoked,
            "an issuer that says active again lifts the revocation"
        );
    }
}
