//! An in-process license issuer: RFC 8414 metadata, a JWKS, RFC 7662 introspection
//! with RFC 9701 signed answers, the RFC 8628 device grant, and an admin surface that
//! issues and revokes (docs/modules-fanout-plan.md §9).
//!
//! It exists so the whole licensing path can be exercised without a server: the test
//! suite installs a [`StubIssuer`] as the [`LicenseHttp`] a [`LicenseService`] talks
//! through, and every request is answered by a function call. The same object also
//! renders an [`axum::Router`], so `avada` can serve it on loopback while a module
//! author develops against it.
//!
//! Not `#[cfg(test)]`: local development needs it too. It is emphatically **not** a
//! production issuer — the signing key is generated per instance and lives in memory,
//! and it grants whatever it is asked for. The admin routes still require a bearer
//! credential, compared in constant time, so a stub left running on loopback is not an
//! open license printer for anything else on the machine.
//!
//! [`LicenseService`]: super::LicenseService

use super::introspect::{Discovery, JWKS_PATH, METADATA_PATH, OIDC_METADATA_PATH};
use super::verify::{Jwks, SigningKey, VerifyError};
use super::{Clock, HttpFuture, HttpResponse, LicenseHttp, LicenseToken, CLIENT_ID, DEVICE_GRANT};
use avada_module_sdk::license::LicenseClaims;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

/// How long a device flow stays open, in seconds.
pub const DEVICE_CODE_TTL: u64 = 600;
/// The poll interval the stub asks for.
pub const DEVICE_POLL_INTERVAL: u64 = 1;

/// What to put in a license. `Grant::for_product` gives a perpetual single-seat
/// license that checks in monthly; the `with_*` methods narrow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// `owner/repo`.
    pub product: String,
    /// Who it is for.
    pub licensee: String,
    /// Seats.
    pub seats: u32,
    /// Not before.
    pub nbf: u64,
    /// Expiry; `0` is perpetual.
    pub exp: u64,
    /// Highest covered major.
    pub max_major: Option<u64>,
    /// Days between check-ins.
    pub checkin_interval_days: u32,
    /// Where the binary lives.
    pub download_url: Option<String>,
}

impl Grant {
    /// A perpetual single-seat license for `product`.
    pub fn for_product(product: &str) -> Self {
        Grant {
            product: product.to_string(),
            licensee: "tester@example.test".into(),
            seats: 1,
            nbf: 0,
            exp: 0,
            max_major: None,
            checkin_interval_days: 30,
            download_url: None,
        }
    }

    /// Name the licensee.
    pub fn licensed_to(mut self, who: &str) -> Self {
        self.licensee = who.to_string();
        self
    }

    /// Expire at this instant.
    pub fn expiring_at(mut self, exp: u64) -> Self {
        self.exp = exp;
        self
    }

    /// Begin at this instant.
    pub fn starting_at(mut self, nbf: u64) -> Self {
        self.nbf = nbf;
        self
    }

    /// Ask for a check-in every `days`.
    pub fn checking_in_every(mut self, days: u32) -> Self {
        self.checkin_interval_days = days;
        self
    }

    /// Cover majors up to `major`.
    pub fn up_to_major(mut self, major: u64) -> Self {
        self.max_major = Some(major);
        self
    }
}

#[derive(Debug, Clone)]
struct Issued {
    product: String,
    token: String,
    revoked: bool,
    /// Hand back a fresh token at the next check-in (an issuer rotating a key).
    rotate: bool,
}

#[derive(Debug, Clone)]
struct Device {
    user_code: String,
    product: String,
    expires_at: u64,
    /// `None` while the user has not decided.
    approved: Option<bool>,
    token: Option<String>,
}

#[derive(Default)]
struct Ledger {
    /// Answer the next token poll with `slow_down`.
    throttle: bool,
    /// `jti` -> what was issued.
    issued: HashMap<String, Issued>,
    /// `device_code` -> flow.
    devices: HashMap<String, Device>,
}

/// The stub. Cheap to build; every method takes `&self`.
pub struct StubIssuer {
    base: String,
    key: SigningKey,
    clock: Clock,
    admin: String,
    ledger: Mutex<Ledger>,
}

impl std::fmt::Debug for StubIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StubIssuer")
            .field("base", &self.base)
            .field("kid", &self.key.kid())
            .finish_non_exhaustive()
    }
}

impl StubIssuer {
    /// An issuer at `base` (an absolute URL) on the wall clock.
    pub fn new(base: &str) -> Self {
        Self::with_clock(base, super::system_clock())
    }

    /// An issuer at `base` whose notion of now is `clock` — the same clock the service
    /// under test uses, so both sides agree about expiry.
    pub fn with_clock(base: &str, clock: Clock) -> Self {
        StubIssuer {
            base: base.trim().trim_end_matches('/').to_string(),
            key: SigningKey::generate(),
            clock,
            admin: uuid::Uuid::new_v4().simple().to_string(),
            ledger: Mutex::new(Ledger::default()),
        }
    }

    /// The issuer URL, without a trailing slash.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The signing key id.
    pub fn kid(&self) -> &str {
        self.key.kid()
    }

    /// The published key set.
    pub fn jwks(&self) -> Jwks {
        Jwks {
            keys: vec![self.key.jwk()],
        }
    }

    /// The metadata document this issuer publishes.
    pub fn discovery(&self) -> Discovery {
        Discovery::for_issuer(&self.base)
    }

    /// The credential the admin routes require. Handed to whatever drives the stub;
    /// like any credential it belongs in no log line.
    pub fn admin_token(&self) -> &str {
        &self.admin
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    /// Mint a license and remember it, so introspection can answer about it. Returns
    /// the JWT.
    pub fn issue(&self, grant: &Grant) -> Result<String, VerifyError> {
        let jti = uuid::Uuid::new_v4().simple().to_string();
        let claims = LicenseClaims {
            jti: jti.clone(),
            product: grant.product.clone(),
            licensee: grant.licensee.clone(),
            seats: grant.seats,
            nbf: grant.nbf,
            exp: grant.exp,
            max_major: grant.max_major,
            kid: self.key.kid().to_string(),
            download_url: grant.download_url.clone(),
            checkin_interval_days: grant.checkin_interval_days,
        };
        let token = self.key.sign(
            &Licensed {
                iss: &self.base,
                claims: &claims,
            },
            "JWT",
        )?;
        self.ledger.lock().unwrap().issued.insert(
            jti,
            Issued {
                product: grant.product.clone(),
                token: token.clone(),
                revoked: false,
                rotate: false,
            },
        );
        Ok(token)
    }

    /// The `jti` of a token this issuer minted, for the admin calls below.
    pub fn jti_of(&self, token: &str) -> Option<String> {
        let ledger = self.ledger.lock().unwrap();
        ledger
            .issued
            .iter()
            .find(|(_, i)| i.token == token)
            .map(|(jti, _)| jti.clone())
    }

    /// Stop answering `active` for this token. `false` when it was never issued here.
    pub fn revoke(&self, jti: &str) -> bool {
        let mut ledger = self.ledger.lock().unwrap();
        match ledger.issued.get_mut(jti) {
            Some(i) => {
                i.revoked = true;
                true
            }
            None => false,
        }
    }

    /// Revoke every license for a product. Returns how many.
    pub fn revoke_product(&self, product: &str) -> usize {
        let mut ledger = self.ledger.lock().unwrap();
        let mut n = 0;
        for i in ledger.issued.values_mut() {
            if i.product == product && !i.revoked {
                i.revoked = true;
                n += 1;
            }
        }
        n
    }

    /// Undo a revocation.
    pub fn reinstate(&self, jti: &str) -> bool {
        let mut ledger = self.ledger.lock().unwrap();
        match ledger.issued.get_mut(jti) {
            Some(i) => {
                i.revoked = false;
                true
            }
            None => false,
        }
    }

    /// Hand a freshly minted token back at the next check-in, the way an issuer that
    /// rotates tokens would.
    pub fn rotate_at_next_checkin(&self, jti: &str) -> bool {
        let mut ledger = self.ledger.lock().unwrap();
        match ledger.issued.get_mut(jti) {
            Some(i) => {
                i.rotate = true;
                true
            }
            None => false,
        }
    }

    /// The user code of the one flow waiting for a decision, if there is exactly one.
    /// The host never sees a `device_code`, so this is how a test plays the user.
    pub fn pending_user_code(&self) -> Option<String> {
        let ledger = self.ledger.lock().unwrap();
        let mut waiting = ledger.devices.values().filter(|d| d.approved.is_none());
        let first = waiting.next()?;
        waiting.next().is_none().then(|| first.user_code.clone())
    }

    /// The user approves a device flow: a license is minted for the product it asked
    /// for. `false` when no flow is waiting under that user code.
    pub fn approve_device(&self, user_code: &str) -> bool {
        let product = {
            let ledger = self.ledger.lock().unwrap();
            match ledger.devices.values().find(|d| d.user_code == user_code) {
                Some(d) => d.product.clone(),
                None => return false,
            }
        };
        // Minting takes the lock itself, so it happens outside the one above.
        let token = match self.issue(&Grant::for_product(&product)) {
            Ok(t) => t,
            Err(_) => return false,
        };
        let mut ledger = self.ledger.lock().unwrap();
        match ledger
            .devices
            .values_mut()
            .find(|d| d.user_code == user_code)
        {
            Some(d) => {
                d.approved = Some(true);
                d.token = Some(token);
                true
            }
            None => false,
        }
    }

    /// Answer the next `/token` poll with RFC 8628's `slow_down`, whatever its state.
    pub fn throttle_next_poll(&self) {
        self.ledger.lock().unwrap().throttle = true;
    }

    /// The user refuses.
    pub fn deny_device(&self, user_code: &str) -> bool {
        let mut ledger = self.ledger.lock().unwrap();
        match ledger
            .devices
            .values_mut()
            .find(|d| d.user_code == user_code)
        {
            Some(d) => {
                d.approved = Some(false);
                true
            }
            None => false,
        }
    }

    /// The stub as an axum service, for serving on loopback during development.
    /// Every route delegates to the same functions the in-process transport uses, so
    /// there is one implementation of the issuer's behaviour and not two.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(METADATA_PATH, get(stub_get))
            .route(OIDC_METADATA_PATH, get(stub_get))
            .route(JWKS_PATH, get(stub_get))
            .route("/download/{jti}", get(stub_get))
            .route("/introspect", post(stub_post))
            .route("/device_authorization", post(stub_post))
            .route("/token", post(stub_post))
            .route("/admin/issue", post(stub_post))
            .route("/admin/revoke", post(stub_post))
            .with_state(self)
    }

    /// The path part of a URL addressed to this issuer, or `None` for anything else.
    fn path_of<'u>(&self, url: &'u str) -> Option<&'u str> {
        let rest = url.strip_prefix(&self.base)?;
        let path = rest.split('?').next().unwrap_or("");
        Some(if path.is_empty() { "/" } else { path })
    }

    /// `GET path`.
    pub fn handle_get(&self, path: &str) -> HttpResponse {
        match path {
            METADATA_PATH | OIDC_METADATA_PATH => ok(&self.discovery()),
            JWKS_PATH => ok(&self.jwks()),
            p => match p.strip_prefix("/download/") {
                Some(jti) => match self.ledger.lock().unwrap().issued.get(jti) {
                    Some(i) => HttpResponse {
                        status: 200,
                        body: i.token.clone().into_bytes(),
                    },
                    None => oauth_error(404, "not_found", "no such license"),
                },
                None => oauth_error(404, "not_found", "no such endpoint"),
            },
        }
    }

    /// `POST path` with a form body.
    pub fn handle_post(
        &self,
        path: &str,
        bearer: Option<&str>,
        form: &[(String, String)],
    ) -> HttpResponse {
        match path {
            "/introspect" => self.introspect(form),
            "/device_authorization" => self.device_authorization(form),
            "/token" => self.token(form),
            "/admin/issue" | "/admin/revoke" => {
                if !self.admin_ok(bearer) {
                    return oauth_error(401, "invalid_client", "admin credential required");
                }
                if path == "/admin/issue" {
                    self.admin_issue(form)
                } else {
                    self.admin_revoke(form)
                }
            }
            _ => oauth_error(404, "not_found", "no such endpoint"),
        }
    }

    fn admin_ok(&self, bearer: Option<&str>) -> bool {
        let Some(given) = bearer else { return false };
        let given = given.as_bytes();
        let want = self.admin.as_bytes();
        // Constant time, and length-independent: `ct_eq` needs equal lengths.
        given.len() == want.len() && bool::from(given.ct_eq(want))
    }

    /// RFC 7662, answered as an RFC 9701 signed JWT because the metadata promises one.
    fn introspect(&self, form: &[(String, String)]) -> HttpResponse {
        let Some(token) = field(form, "token") else {
            return oauth_error(400, "invalid_request", "no token");
        };
        let (active, reason, fresh) = {
            let mut ledger = self.ledger.lock().unwrap();
            let entry = ledger
                .issued
                .iter_mut()
                .find(|(_, i)| i.token == token)
                .map(|(jti, i)| (jti.clone(), i.clone()));
            match entry {
                None => (false, Some("unknown token".to_string()), None),
                Some((_, i)) if i.revoked => {
                    (false, Some("revoked by the issuer".to_string()), None)
                }
                Some((jti, i)) if i.rotate => {
                    ledger.issued.get_mut(&jti).expect("just read").rotate = false;
                    (true, None, Some(i.product))
                }
                Some(_) => (true, None, None),
            }
        };
        // Minting a replacement takes the lock again, so it happens after it is dropped.
        let replacement = fresh.and_then(|p| self.issue(&Grant::for_product(&p)).ok());
        let mut body = json!({ "active": active });
        if let Some(r) = reason {
            body["reason"] = Value::String(r);
        }
        if let Some(t) = replacement {
            body["token"] = Value::String(t);
        }
        let envelope = json!({
            "iss": self.base,
            "aud": CLIENT_ID,
            "iat": self.now(),
            "token_introspection": body,
        });
        match self.key.sign(&envelope, "token-introspection+jwt") {
            Ok(jwt) => HttpResponse {
                status: 200,
                body: jwt.into_bytes(),
            },
            Err(_) => oauth_error(500, "server_error", "could not sign the answer"),
        }
    }

    /// RFC 8628 §3.2.
    fn device_authorization(&self, form: &[(String, String)]) -> HttpResponse {
        if field(form, "client_id").as_deref() != Some(CLIENT_ID) {
            return oauth_error(400, "invalid_client", "unknown client");
        }
        let Some(product) = field(form, "scope").filter(|s| !s.is_empty()) else {
            return oauth_error(400, "invalid_scope", "no product in scope");
        };
        let device_code = uuid::Uuid::new_v4().simple().to_string();
        let raw = uuid::Uuid::new_v4().simple().to_string().to_uppercase();
        let user_code = format!("{}-{}", &raw[..4], &raw[4..8]);
        let expires_at = self.now().saturating_add(DEVICE_CODE_TTL);
        self.ledger.lock().unwrap().devices.insert(
            device_code.clone(),
            Device {
                user_code: user_code.clone(),
                product,
                expires_at,
                approved: None,
                token: None,
            },
        );
        ok(&json!({
            "device_code": device_code,
            "user_code": user_code,
            "verification_uri": format!("{}/activate", self.base),
            "verification_uri_complete": format!("{}/activate?user_code={user_code}", self.base),
            "expires_in": DEVICE_CODE_TTL,
            "interval": DEVICE_POLL_INTERVAL,
        }))
    }

    /// RFC 8628 §3.4/§3.5.
    fn token(&self, form: &[(String, String)]) -> HttpResponse {
        if field(form, "grant_type").as_deref() != Some(DEVICE_GRANT) {
            return oauth_error(400, "unsupported_grant_type", "device grant only");
        }
        let Some(code) = field(form, "device_code") else {
            return oauth_error(400, "invalid_request", "no device_code");
        };
        let now = self.now();
        let mut ledger = self.ledger.lock().unwrap();
        if std::mem::take(&mut ledger.throttle) {
            return oauth_error(400, "slow_down", "polling too often");
        }
        let Some(flow) = ledger.devices.get(&code).cloned() else {
            return oauth_error(400, "invalid_grant", "no such device code");
        };
        if now >= flow.expires_at {
            ledger.devices.remove(&code);
            return oauth_error(400, "expired_token", "the device code lapsed");
        }
        match flow.approved {
            None => oauth_error(400, "authorization_pending", "the user has not decided"),
            Some(false) => {
                ledger.devices.remove(&code);
                oauth_error(400, "access_denied", "the user refused")
            }
            Some(true) => {
                ledger.devices.remove(&code);
                match flow.token {
                    Some(t) => ok(&json!({
                        "access_token": t,
                        "token_type": "Bearer",
                        "scope": flow.product,
                    })),
                    None => oauth_error(500, "server_error", "approved with no license"),
                }
            }
        }
    }

    fn admin_issue(&self, form: &[(String, String)]) -> HttpResponse {
        let Some(product) = field(form, "product") else {
            return oauth_error(400, "invalid_request", "no product");
        };
        let mut grant = Grant::for_product(&product);
        if let Some(l) = field(form, "licensee") {
            grant.licensee = l;
        }
        for (key, slot) in [("nbf", &mut grant.nbf), ("exp", &mut grant.exp)] {
            if let Some(v) = field(form, key).and_then(|v| v.parse().ok()) {
                *slot = v;
            }
        }
        if let Some(v) = field(form, "seats").and_then(|v| v.parse().ok()) {
            grant.seats = v;
        }
        if let Some(v) = field(form, "checkin_interval_days").and_then(|v| v.parse().ok()) {
            grant.checkin_interval_days = v;
        }
        grant.max_major = field(form, "max_major").and_then(|v| v.parse().ok());
        match self.issue(&grant) {
            Ok(token) => {
                let jti = self.jti_of(&token).unwrap_or_default();
                ok(&json!({ "token": token, "jti": jti, "product": grant.product }))
            }
            Err(_) => oauth_error(500, "server_error", "could not sign the license"),
        }
    }

    fn admin_revoke(&self, form: &[(String, String)]) -> HttpResponse {
        if let Some(jti) = field(form, "jti") {
            return if self.revoke(&jti) {
                ok(&json!({ "revoked": 1 }))
            } else {
                oauth_error(404, "not_found", "no such license")
            };
        }
        match field(form, "product") {
            Some(p) => ok(&json!({ "revoked": self.revoke_product(&p) })),
            None => oauth_error(400, "invalid_request", "no jti or product"),
        }
    }
}

impl LicenseHttp for StubIssuer {
    fn get(&self, url: String) -> HttpFuture<'_> {
        Box::pin(async move {
            Ok(match self.path_of(&url) {
                Some(path) => self.handle_get(path),
                None => oauth_error(404, "not_found", "not this issuer"),
            })
        })
    }

    fn post_form(
        &self,
        url: String,
        bearer: Option<LicenseToken>,
        form: Vec<(String, String)>,
        _accept: String,
    ) -> HttpFuture<'_> {
        Box::pin(async move {
            Ok(match self.path_of(&url) {
                Some(path) => {
                    self.handle_post(path, bearer.as_ref().map(|b| b.expose_secret()), &form)
                }
                None => oauth_error(404, "not_found", "not this issuer"),
            })
        })
    }
}

/// A license JWT: the SDK's claims plus the `iss` the host reads before verifying.
#[derive(serde::Serialize)]
struct Licensed<'a> {
    iss: &'a str,
    #[serde(flatten)]
    claims: &'a LicenseClaims,
}

fn field(form: &[(String, String)], key: &str) -> Option<String> {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn ok<T: serde::Serialize>(body: &T) -> HttpResponse {
    match serde_json::to_vec(body) {
        Ok(body) => HttpResponse { status: 200, body },
        Err(_) => oauth_error(500, "server_error", "could not encode the answer"),
    }
}

fn oauth_error(status: u16, code: &str, description: &str) -> HttpResponse {
    HttpResponse {
        status,
        body: serde_json::to_vec(&json!({ "error": code, "error_description": description }))
            .unwrap_or_default(),
    }
}

fn into_axum(r: HttpResponse) -> Response {
    let status =
        axum::http::StatusCode::from_u16(r.status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    let kind = if r.body.starts_with(b"{") {
        "application/json"
    } else {
        "application/jwt"
    };
    (status, [(axum::http::header::CONTENT_TYPE, kind)], r.body).into_response()
}

fn bearer_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|s| s.trim().to_string())
}

async fn stub_get(State(issuer): State<Arc<StubIssuer>>, uri: Uri) -> Response {
    into_axum(issuer.handle_get(uri.path()))
}

async fn stub_post(
    State(issuer): State<Arc<StubIssuer>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let bearer = bearer_of(&headers);
    into_axum(issuer.handle_post(uri.path(), bearer.as_deref(), &form))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::license::introspect::{discover, fetch_jwks, IntrospectionClient};
    use crate::license::verify::Verifier;

    fn issuer() -> Arc<StubIssuer> {
        Arc::new(StubIssuer::new("https://issuer.test"))
    }

    #[tokio::test]
    async fn discovery_and_keys_come_back_over_the_in_process_transport() {
        let stub = issuer();
        let http: Arc<dyn LicenseHttp> = stub.clone();
        let doc = discover(http.as_ref(), "https://issuer.test")
            .await
            .unwrap();
        assert_eq!(doc.issuer, "https://issuer.test");
        assert!(doc.signs_introspection());
        let keys = fetch_jwks(http.as_ref(), &doc.jwks_uri).await.unwrap();
        assert_eq!(keys.keys.len(), 1);
        assert_eq!(keys.keys[0].kid, stub.kid());
    }

    #[tokio::test]
    async fn introspection_tracks_issue_revoke_and_reinstate() {
        let stub = issuer();
        let http: Arc<dyn LicenseHttp> = stub.clone();
        let token = LicenseToken::new(stub.issue(&Grant::for_product("acme/pro")).unwrap());
        let jti = stub.jti_of(token.expose_secret()).unwrap();
        let doc = stub.discovery();
        let keys = stub.jwks();
        let verifier = Verifier::default();
        let ask = || {
            let http = http.clone();
            let doc = doc.clone();
            let keys = keys.clone();
            let token = token.clone();
            let verifier = verifier.clone();
            async move {
                IntrospectionClient::new(http.as_ref(), &verifier)
                    .introspect(&doc, "https://issuer.test", &token, &keys)
                    .await
                    .unwrap()
            }
        };
        assert!(ask().await.active);
        assert!(stub.revoke(&jti));
        let answer = ask().await;
        assert!(!answer.active);
        assert_eq!(answer.reason.as_deref(), Some("revoked by the issuer"));
        assert!(stub.reinstate(&jti));
        assert!(ask().await.active);

        let stranger = LicenseToken::new("a.b.c");
        let unknown = IntrospectionClient::new(http.as_ref(), &verifier)
            .introspect(&doc, "https://issuer.test", &stranger, &keys)
            .await
            .unwrap();
        assert!(!unknown.active);
    }

    #[tokio::test]
    async fn a_rotating_issuer_hands_back_a_fresh_token_once() {
        let stub = issuer();
        let http: Arc<dyn LicenseHttp> = stub.clone();
        let token = LicenseToken::new(stub.issue(&Grant::for_product("acme/pro")).unwrap());
        let jti = stub.jti_of(token.expose_secret()).unwrap();
        assert!(stub.rotate_at_next_checkin(&jti));
        let verifier = Verifier::default();
        let first = IntrospectionClient::new(http.as_ref(), &verifier)
            .introspect(
                &stub.discovery(),
                "https://issuer.test",
                &token,
                &stub.jwks(),
            )
            .await
            .unwrap();
        let fresh = first.token.expect("a rotated token");
        assert_ne!(fresh, token.expose_secret());
        assert!(verifier
            .verify_signature(&fresh, &stub.jwks(), Some("acme/pro"))
            .is_ok());
        let second = IntrospectionClient::new(http.as_ref(), &verifier)
            .introspect(
                &stub.discovery(),
                "https://issuer.test",
                &token,
                &stub.jwks(),
            )
            .await
            .unwrap();
        assert!(second.token.is_none(), "rotation happens once");
    }

    #[test]
    fn the_admin_surface_needs_its_credential() {
        let stub = issuer();
        let form = vec![("product".to_string(), "acme/pro".to_string())];
        assert_eq!(stub.handle_post("/admin/issue", None, &form).status, 401);
        assert_eq!(
            stub.handle_post("/admin/issue", Some("not-it"), &form)
                .status,
            401
        );
        let good = stub.handle_post("/admin/issue", Some(stub.admin_token()), &form);
        assert_eq!(good.status, 200);
        let v: Value = serde_json::from_slice(&good.body).unwrap();
        let jti = v["jti"].as_str().unwrap().to_string();
        assert_eq!(
            stub.handle_post(
                "/admin/revoke",
                Some("not-it"),
                &[("jti".into(), jti.clone())]
            )
            .status,
            401
        );
        assert_eq!(
            stub.handle_post(
                "/admin/revoke",
                Some(stub.admin_token()),
                &[("jti".into(), jti)]
            )
            .status,
            200
        );
    }

    #[test]
    fn the_device_grant_walks_pending_then_approved() {
        let stub = issuer();
        let start = stub.handle_post(
            "/device_authorization",
            None,
            &[
                ("client_id".into(), CLIENT_ID.into()),
                ("scope".into(), "acme/pro".into()),
            ],
        );
        assert_eq!(start.status, 200);
        let v: Value = serde_json::from_slice(&start.body).unwrap();
        let device_code = v["device_code"].as_str().unwrap().to_string();
        let poll = |code: &str| {
            stub.handle_post(
                "/token",
                None,
                &[
                    ("grant_type".into(), DEVICE_GRANT.into()),
                    ("device_code".into(), code.into()),
                    ("client_id".into(), CLIENT_ID.into()),
                ],
            )
        };
        let pending = poll(&device_code);
        assert_eq!(pending.status, 400);
        assert!(pending.text().contains("authorization_pending"));
        let user_code = stub.pending_user_code().unwrap();
        assert_eq!(user_code, v["user_code"].as_str().unwrap());
        assert!(stub.approve_device(&user_code));
        let granted = poll(&device_code);
        assert_eq!(granted.status, 200);
        let g: Value = serde_json::from_slice(&granted.body).unwrap();
        assert!(g["access_token"].as_str().unwrap().contains('.'));
        assert_eq!(poll(&device_code).status, 400, "a code is redeemed once");
    }

    #[test]
    fn a_denied_flow_says_access_denied_and_a_lapsed_one_says_expired() {
        let stub = issuer();
        let begin = |s: &StubIssuer| {
            let r = s.handle_post(
                "/device_authorization",
                None,
                &[
                    ("client_id".into(), CLIENT_ID.into()),
                    ("scope".into(), "acme/pro".into()),
                ],
            );
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            v["device_code"].as_str().unwrap().to_string()
        };
        let code = begin(&stub);
        assert!(stub.deny_device(&stub.pending_user_code().unwrap()));
        let denied = stub.handle_post(
            "/token",
            None,
            &[
                ("grant_type".into(), DEVICE_GRANT.into()),
                ("device_code".into(), code.clone()),
                ("client_id".into(), CLIENT_ID.into()),
            ],
        );
        assert!(denied.text().contains("access_denied"));

        let now = Arc::new(Mutex::new(1_000u64));
        let ticking = {
            let n = now.clone();
            StubIssuer::with_clock("https://issuer.test", Arc::new(move || *n.lock().unwrap()))
        };
        let code = begin(&ticking);
        *now.lock().unwrap() = 1_000 + DEVICE_CODE_TTL + 1;
        let lapsed = ticking.handle_post(
            "/token",
            None,
            &[
                ("grant_type".into(), DEVICE_GRANT.into()),
                ("device_code".into(), code),
                ("client_id".into(), CLIENT_ID.into()),
            ],
        );
        assert!(lapsed.text().contains("expired_token"), "{}", lapsed.text());
    }

    #[test]
    fn a_url_for_another_host_is_not_answered() {
        let stub = issuer();
        let none = stub.path_of("https://elsewhere.test/.well-known/jwks.json");
        assert!(none.is_none());
        assert_eq!(stub.handle_get("/nope").status, 404);
        assert_eq!(stub.handle_get("/download/nope").status, 404);
    }

    #[test]
    fn a_download_link_serves_the_token_it_was_issued_for() {
        let stub = issuer();
        let token = stub.issue(&Grant::for_product("acme/pro")).unwrap();
        let jti = stub.jti_of(&token).unwrap();
        let served = stub.handle_get(&format!("/download/{jti}"));
        assert_eq!(served.status, 200);
        assert_eq!(served.text(), token);
    }
}
