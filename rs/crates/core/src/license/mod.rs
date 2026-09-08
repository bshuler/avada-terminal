//! Licensing (docs/modules-fanout-plan.md §2 "Free vs commercial", track G8): the
//! offline EdDSA JWT verifier, the RFC 7662 introspection client, the three install
//! paths, expiry/grace evaluation and the in-process stub issuer used by tests.
//!
//! Claims and the state machine are `avada_module_sdk::license`; this module owns the
//! crypto and the I/O. The full story is `docs/licensing.md`.
//!
//! ```text
//! license/
//!   verify.rs        EdDSA JWT against a JWKS (RFC 7517 OKP/Ed25519), kid, nbf/exp, product
//!   store.rs         <modules root>/licenses/<owner__repo>/{license.jwt,meta.json,checkin.json}
//!   introspect.rs    RFC 8414 discovery, RFC 7662 introspection, RFC 9701 signed answers
//!   stub_issuer.rs   an in-process axum issuer for tests and local development
//!   mod.rs           LicenseService: state, gate, install (file / URL / device flow), revoke
//! ```
//!
//! The token is a bearer credential. It lives in a [`LicenseToken`] whose `Debug` is
//! redacted and whose bytes are wiped on drop, is never placed in an error, a log line,
//! or a route answer, and reaches the wire in exactly two places: the `token` form field
//! and the `Authorization` header of an introspection call.

pub mod gate;
pub mod introspect;
pub mod store;
pub mod stub_issuer;
#[cfg(test)]
mod tests;
pub mod verify;

pub use gate::CachedGate;
pub use introspect::{Discovery, IntrospectionClient};
pub use store::{FileLicenseStore, LicenseStore, MemoryLicenseStore, StoreError, StoredLicense};
pub use verify::{Jwk, Jwks, Verifier, VerifyError};

use avada_module_sdk::license::{evaluate, CheckinRecord, LicenseClaims, LicenseState, GRACE_DAYS};
use avada_module_sdk::ModuleId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// The OAuth `client_id` the host presents to an issuer (device grant, introspection).
pub const CLIENT_ID: &str = "avada-terminal";
/// RFC 8628 §3.4 grant type.
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A license token. `Debug` never prints it; the bytes are wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct LicenseToken(Vec<u8>);

impl LicenseToken {
    /// Wrap a token string (surrounding whitespace trimmed).
    pub fn new(s: impl AsRef<str>) -> Self {
        LicenseToken(s.as_ref().trim().as_bytes().to_vec())
    }

    /// The token itself, for the places that put it on the wire or on disk. Named so a
    /// reviewer sees every use.
    pub fn expose_secret(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl Drop for LicenseToken {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // Volatile so the wipe survives dead-store elimination.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl fmt::Debug for LicenseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LicenseToken([redacted; {} bytes])", self.0.len())
    }
}

/// Seconds since the epoch. Injectable so tests can move time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The wall clock.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}

/// An HTTP answer, status and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The future an [`LicenseHttp`] call returns.
pub type HttpFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HttpResponse, LicenseError>> + Send + 'a>>;

/// The two HTTP shapes the service needs. Injectable so tests need no network and the
/// e2e tests can script failures.
pub trait LicenseHttp: Send + Sync {
    /// `GET url`.
    fn get(&self, url: String) -> HttpFuture<'_>;
    /// `POST url` with a form body, an optional bearer credential and an `Accept`.
    fn post_form(
        &self,
        url: String,
        bearer: Option<LicenseToken>,
        form: Vec<(String, String)>,
        accept: String,
    ) -> HttpFuture<'_>;
}

/// The real thing, over reqwest.
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestHttp {
    /// A client with a 20 s timeout.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        ReqwestHttp { client }
    }
}

fn transport_error(e: reqwest::Error) -> LicenseError {
    // `without_url`: the URL of a device-flow poll carries nothing secret, but a
    // download URL might carry a one-time path; keep both out.
    LicenseError::Network(e.without_url().to_string())
}

impl LicenseHttp for ReqwestHttp {
    fn get(&self, url: String) -> HttpFuture<'_> {
        Box::pin(async move {
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(transport_error)?;
            let status = resp.status().as_u16();
            let body = resp.bytes().await.map_err(transport_error)?.to_vec();
            Ok(HttpResponse { status, body })
        })
    }

    fn post_form(
        &self,
        url: String,
        bearer: Option<LicenseToken>,
        form: Vec<(String, String)>,
        accept: String,
    ) -> HttpFuture<'_> {
        Box::pin(async move {
            let mut req = self.client.post(&url).header("accept", accept).form(&form);
            if let Some(t) = &bearer {
                req = req.bearer_auth(t.expose_secret());
            }
            let resp = req.send().await.map_err(transport_error)?;
            let status = resp.status().as_u16();
            let body = resp.bytes().await.map_err(transport_error)?.to_vec();
            Ok(HttpResponse { status, body })
        })
    }
}

/// Why a license operation failed. Products, issuers and paths; never a token.
#[derive(Debug)]
pub enum LicenseError {
    /// The store said no.
    Store(StoreError),
    /// The token does not verify.
    Verify(VerifyError),
    /// The issuer could not be reached.
    Network(String),
    /// The issuer answered with an error.
    Issuer {
        /// HTTP status.
        status: u16,
        /// The issuer's `error`/`error_description`, or a short body excerpt.
        message: String,
    },
    /// An answer or a document did not have the expected shape.
    Malformed(String),
    /// No license is installed for this product.
    NotLicensed(String),
    /// The token names no issuer and none is registered for the product.
    IssuerUnknown(String),
    /// The product's manifest names one issuer, the token (or caller) another.
    IssuerMismatch {
        /// The product.
        product: String,
        /// The issuer the product is bound to.
        expected: String,
        /// The one offered.
        actual: String,
    },
    /// A file could not be read.
    Io {
        /// The path.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },
    /// No such device-flow handle.
    UnknownFlow(String),
}

impl fmt::Display for LicenseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LicenseError::Store(e) => write!(f, "license store: {e}"),
            LicenseError::Verify(e) => write!(f, "license: {e}"),
            LicenseError::Network(e) => write!(f, "issuer unreachable: {e}"),
            LicenseError::Issuer { status, message } => {
                write!(f, "issuer answered {status}: {message}")
            }
            LicenseError::Malformed(e) => write!(f, "malformed: {e}"),
            LicenseError::NotLicensed(p) => write!(f, "no license installed for {p}"),
            LicenseError::IssuerUnknown(p) => write!(
                f,
                "no issuer known for {p}: the token names none and the product has none registered"
            ),
            LicenseError::IssuerMismatch {
                product,
                expected,
                actual,
            } => write!(f, "{product} is licensed by {expected}, not {actual}"),
            LicenseError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            LicenseError::UnknownFlow(c) => write!(f, "no device flow {c}"),
        }
    }
}

impl std::error::Error for LicenseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LicenseError::Store(e) => Some(e),
            LicenseError::Verify(e) => Some(e),
            LicenseError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<StoreError> for LicenseError {
    fn from(e: StoreError) -> Self {
        LicenseError::Store(e)
    }
}

impl From<VerifyError> for LicenseError {
    fn from(e: VerifyError) -> Self {
        LicenseError::Verify(e)
    }
}

impl LicenseError {
    /// The HTTP status a control route answers with.
    pub fn http_status(&self) -> u16 {
        match self {
            LicenseError::Store(_) => 500,
            LicenseError::Verify(_) => 422,
            LicenseError::Network(_) | LicenseError::Issuer { .. } | LicenseError::Malformed(_) => {
                502
            }
            LicenseError::NotLicensed(_) | LicenseError::UnknownFlow(_) => 404,
            LicenseError::IssuerUnknown(_) => 400,
            LicenseError::IssuerMismatch { .. } => 409,
            LicenseError::Io { source, .. } => {
                if source.kind() == std::io::ErrorKind::NotFound {
                    404
                } else {
                    400
                }
            }
        }
    }
}

/// What the host does with a module at spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "gate", content = "reason", rename_all = "snake_case")]
pub enum Gate {
    /// Licensed: run.
    Run,
    /// Licensed but in the grace period: run and show the reason.
    RunWithBanner(String),
    /// Not licensed: do not run.
    Refuse(String),
}

impl Gate {
    /// `Run` or `RunWithBanner`.
    pub fn allows_run(&self) -> bool {
        !matches!(self, Gate::Refuse(_))
    }
}

/// What the UI shows about a license. The token is not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseSummary {
    /// `owner/repo`.
    pub product: String,
    /// The issuer URL.
    pub issuer: String,
    /// Display name of the licensee.
    pub licensee: String,
    /// Seats.
    pub seats: u32,
    /// Not before (epoch seconds).
    pub nbf: u64,
    /// Expiry; `None` is perpetual.
    pub exp: Option<u64>,
    /// Highest covered major, if bounded.
    pub max_major: Option<u64>,
    /// Check-in interval in days (clamped).
    pub checkin_interval_days: u32,
    /// The check-in record.
    pub checkin: CheckinRecord,
    /// `valid`, `grace_period`, `expired`, `revoked`, `stale_checkin`, `not_yet`, or
    /// `invalid` when the token no longer verifies.
    pub state: String,
    /// Why, when the state is anything but `valid`.
    pub reason: Option<String>,
}

/// The state's name: the SDK's snake_case serialization.
pub fn state_name(state: LicenseState) -> &'static str {
    match state {
        LicenseState::Valid => "valid",
        LicenseState::GracePeriod => "grace_period",
        LicenseState::Expired => "expired",
        LicenseState::Revoked => "revoked",
        LicenseState::StaleCheckin => "stale_checkin",
        LicenseState::NotYet => "not_yet",
    }
}

/// A sentence for the banner or the refusal.
pub fn state_reason(
    state: LicenseState,
    claims: &LicenseClaims,
    checkin: &CheckinRecord,
    now: u64,
) -> String {
    let day = 86_400u64;
    let due = checkin.last_ok + u64::from(claims.checkin_days()) * day;
    match state {
        LicenseState::Valid => "licensed".into(),
        LicenseState::GracePeriod => {
            let left = (due + u64::from(GRACE_DAYS) * day)
                .saturating_sub(now)
                .div_ceil(day);
            format!(
                "license check-in overdue; the issuer must confirm {} within {left} day{} or the module stops",
                claims.product,
                if left == 1 { "" } else { "s" }
            )
        }
        LicenseState::Expired => format!("license for {} expired", claims.product),
        LicenseState::Revoked => {
            format!("license for {} was revoked by the issuer", claims.product)
        }
        LicenseState::StaleCheckin => format!(
            "license for {} has not been confirmed by the issuer for more than {} days",
            claims.product,
            u64::from(claims.checkin_days()) + u64::from(GRACE_DAYS)
        ),
        LicenseState::NotYet => format!("license for {} is not valid yet", claims.product),
    }
}

/// What a check-in did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckinOutcome {
    /// The interval has not elapsed; nothing was sent.
    NotDue,
    /// The issuer answered and the record was updated.
    Answered {
        /// The issuer's verdict.
        active: bool,
        /// Its reason, when inactive.
        reason: Option<String>,
    },
    /// The issuer could not be reached or its answer did not verify; the record is
    /// untouched.
    Unreachable(String),
}

/// A device flow the host started (RFC 8628 §3.2). `code` is the host's handle for
/// polling; the issuer's `device_code` stays inside the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCode {
    /// The host's handle: `GET /license/device/{code}`.
    pub code: String,
    /// The product being licensed.
    pub product: String,
    /// What the user types at the verification page.
    pub user_code: String,
    /// Where the user goes.
    pub verification_uri: String,
    /// The same with the code filled in, when the issuer gives one.
    pub verification_uri_complete: Option<String>,
    /// Seconds until the flow lapses.
    pub expires_in: u64,
    /// Minimum seconds between polls.
    pub interval: u64,
}

/// One poll of a device flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DevicePoll {
    /// The user has not decided; poll again after `interval`.
    Pending,
    /// The user has not decided; poll less often.
    SlowDown,
    /// The flow lapsed; start another.
    Expired,
    /// The user refused.
    Denied,
    /// The license was granted and installed.
    Installed {
        /// The installed license.
        license: LicenseSummary,
    },
}

#[derive(Clone)]
struct PendingFlow {
    issuer: String,
    product: String,
    device_code: LicenseToken,
    token_endpoint: String,
    expires_at: u64,
}

fn norm_issuer(issuer: &str) -> String {
    issuer.trim().trim_end_matches('/').to_string()
}

/// The licensing service: one per host, shared by the control plane and the module
/// host. Verifies offline from cached keys; talks to an issuer only to install, to
/// refresh keys and to check in.
pub struct LicenseService {
    store: Arc<dyn LicenseStore>,
    http: Arc<dyn LicenseHttp>,
    clock: Clock,
    verifier: Verifier,
    issuers: RwLock<HashMap<String, String>>,
    flows: Mutex<HashMap<String, PendingFlow>>,
}

impl LicenseService {
    /// A service over these parts.
    pub fn new(store: Arc<dyn LicenseStore>, http: Arc<dyn LicenseHttp>, clock: Clock) -> Self {
        LicenseService {
            store,
            http,
            clock,
            verifier: Verifier::default(),
            issuers: RwLock::new(HashMap::new()),
            flows: Mutex::new(HashMap::new()),
        }
    }

    /// The host's service: the file store under the modules root, reqwest, the wall
    /// clock.
    pub fn host() -> Self {
        Self::new(
            Arc::new(FileLicenseStore::host()),
            Arc::new(ReqwestHttp::new()),
            system_clock(),
        )
    }

    /// The current time by the service's clock.
    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    /// The store behind this service, so a caller can build a second service over the
    /// same licenses (the CLI and the control plane share one directory).
    pub fn store(&self) -> Arc<dyn LicenseStore> {
        self.store.clone()
    }

    /// The verifier (skew settings).
    pub fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    /// Bind a product to its issuer (the manifest's `distribution.issuer`). Tokens from
    /// any other issuer are refused for that product.
    pub fn register_issuer(&self, product: &str, issuer: &str) {
        self.issuers
            .write()
            .unwrap()
            .insert(product.to_string(), norm_issuer(issuer));
    }

    /// The issuer a product is bound to, if any.
    pub fn issuer_for(&self, product: &str) -> Option<String> {
        self.issuers.read().unwrap().get(product).cloned()
    }

    fn check_issuer(&self, product: &str, issuer: &str) -> Result<(), LicenseError> {
        match self.issuer_for(product) {
            Some(expected) if expected != norm_issuer(issuer) => {
                Err(LicenseError::IssuerMismatch {
                    product: product.to_string(),
                    expected,
                    actual: norm_issuer(issuer),
                })
            }
            _ => Ok(()),
        }
    }

    fn resolve_issuer(
        &self,
        product: &str,
        explicit: Option<&str>,
        iss: Option<&str>,
    ) -> Result<String, LicenseError> {
        let registered = self.issuer_for(product);
        match (explicit, registered) {
            (Some(e), Some(r)) if norm_issuer(e) != r => Err(LicenseError::IssuerMismatch {
                product: product.to_string(),
                expected: r,
                actual: norm_issuer(e),
            }),
            (Some(e), _) => Ok(norm_issuer(e)),
            (None, Some(r)) => match iss {
                Some(i) if norm_issuer(i) != r => Err(LicenseError::IssuerMismatch {
                    product: product.to_string(),
                    expected: r,
                    actual: norm_issuer(i),
                }),
                _ => Ok(r),
            },
            (None, None) => iss
                .map(norm_issuer)
                .filter(|i| !i.is_empty())
                .ok_or_else(|| LicenseError::IssuerUnknown(product.to_string())),
        }
    }

    /// The issuer's keys: from the cache when it holds `kid`, otherwise fetched (and
    /// cached); the cache stands in when the fetch fails.
    async fn keys_for(&self, issuer: &str, kid: Option<&str>) -> Result<Jwks, LicenseError> {
        let cached = self.store.load_keys(issuer)?;
        if let Some(keys) = &cached {
            if kid.is_none_or(|k| keys.has(k)) {
                return Ok(keys.clone());
            }
        }
        match self.refresh_keys(issuer).await {
            Ok(keys) => Ok(keys),
            Err(e) => cached.ok_or(e),
        }
    }

    /// Fetch and cache the issuer's key set.
    pub async fn refresh_keys(&self, issuer: &str) -> Result<Jwks, LicenseError> {
        let discovery = introspect::discover(self.http.as_ref(), issuer).await?;
        let keys = introspect::fetch_jwks(self.http.as_ref(), &discovery.jwks_uri).await?;
        self.store.save_keys(issuer, &keys)?;
        tracing::debug!(
            issuer,
            keys = keys.keys.len(),
            "license issuer keys refreshed"
        );
        Ok(keys)
    }

    async fn claims_of(
        &self,
        stored: &StoredLicense,
    ) -> Result<(LicenseClaims, Jwks), LicenseError> {
        let header = verify::peek_header(stored.token.expose_secret())?;
        let keys = self.keys_for(&stored.issuer, header.kid.as_deref()).await?;
        let claims = self.verifier.verify_signature(
            stored.token.expose_secret(),
            &keys,
            Some(&stored.product),
        )?;
        Ok((claims, keys))
    }

    /// The state of a product's license at `now`.
    pub async fn state(&self, product: &str, now: u64) -> Result<LicenseState, LicenseError> {
        let stored = self
            .store
            .load(product)?
            .ok_or_else(|| LicenseError::NotLicensed(product.to_string()))?;
        self.check_issuer(product, &stored.issuer)?;
        let (claims, _) = self.claims_of(&stored).await?;
        Ok(evaluate(&claims, &stored.checkin, now))
    }

    /// Run, run with a banner, or refuse.
    pub async fn gate(&self, product: &str) -> Gate {
        self.gate_for_major(product, None).await
    }

    /// [`gate`](Self::gate) that also checks the module's major against `max_major`.
    pub async fn gate_for_major(&self, product: &str, major: Option<u64>) -> Gate {
        let stored = match self.store.load(product) {
            Ok(Some(s)) => s,
            Ok(None) => return Gate::Refuse(format!("no license installed for {product}")),
            Err(e) => return Gate::Refuse(e.to_string()),
        };
        if let Err(e) = self.check_issuer(product, &stored.issuer) {
            return Gate::Refuse(e.to_string());
        }
        let claims = match self.claims_of(&stored).await {
            Ok((c, _)) => c,
            Err(e) => return Gate::Refuse(format!("license for {product} does not verify: {e}")),
        };
        if let Some(m) = major {
            if !claims.covers_major(m) {
                return Gate::Refuse(format!(
                    "license for {product} covers versions up to major {}; this is major {m}",
                    claims.max_major.unwrap_or(0)
                ));
            }
        }
        let now = self.now();
        match evaluate(&claims, &stored.checkin, now) {
            LicenseState::Valid => Gate::Run,
            state @ LicenseState::GracePeriod => {
                Gate::RunWithBanner(state_reason(state, &claims, &stored.checkin, now))
            }
            state => Gate::Refuse(state_reason(state, &claims, &stored.checkin, now)),
        }
    }

    /// One product's summary.
    pub async fn show(&self, product: &str) -> Result<LicenseSummary, LicenseError> {
        let stored = self
            .store
            .load(product)?
            .ok_or_else(|| LicenseError::NotLicensed(product.to_string()))?;
        Ok(self.summarize(&stored).await)
    }

    /// Every installed license, sorted by product.
    pub async fn list(&self) -> Result<Vec<LicenseSummary>, LicenseError> {
        let mut out = Vec::new();
        for product in self.store.products()? {
            if let Some(stored) = self.store.load(&product)? {
                out.push(self.summarize(&stored).await);
            }
        }
        Ok(out)
    }

    async fn summarize(&self, stored: &StoredLicense) -> LicenseSummary {
        let now = self.now();
        let verified = match self.check_issuer(&stored.product, &stored.issuer) {
            Ok(()) => self.claims_of(stored).await.map(|(c, _)| c),
            Err(e) => Err(e),
        };
        match verified {
            Ok(claims) => {
                let state = evaluate(&claims, &stored.checkin, now);
                LicenseSummary {
                    product: stored.product.clone(),
                    issuer: stored.issuer.clone(),
                    licensee: claims.licensee.clone(),
                    seats: claims.seats,
                    nbf: claims.nbf,
                    exp: (claims.exp != 0).then_some(claims.exp),
                    max_major: claims.max_major,
                    checkin_interval_days: claims.checkin_days(),
                    checkin: stored.checkin.clone(),
                    state: state_name(state).into(),
                    reason: (state != LicenseState::Valid)
                        .then(|| state_reason(state, &claims, &stored.checkin, now)),
                }
            }
            Err(e) => LicenseSummary {
                product: stored.product.clone(),
                issuer: stored.issuer.clone(),
                licensee: String::new(),
                seats: 0,
                nbf: 0,
                exp: None,
                max_major: None,
                checkin_interval_days: 0,
                checkin: stored.checkin.clone(),
                state: "invalid".into(),
                reason: Some(e.to_string()),
            },
        }
    }

    /// Install the token in a file (a `.jwt` the user was sent).
    pub async fn install_file(&self, path: &Path) -> Result<LicenseSummary, LicenseError> {
        let bytes = std::fs::read(path).map_err(|e| LicenseError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let token = LicenseToken::new(String::from_utf8_lossy(&bytes));
        self.install_token(token, None).await
    }

    /// Fetch a token from a URL (a one-time download link) and install it.
    pub async fn install_from_url(&self, url: &str) -> Result<LicenseSummary, LicenseError> {
        let resp = self.http.get(url.to_string()).await?;
        if !resp.is_success() {
            return Err(issuer_error(&resp));
        }
        let token = LicenseToken::new(resp.text());
        self.install_token(token, None).await
    }

    /// Verify and store a token. The issuer is `issuer` when given, else the product's
    /// registered issuer, else the token's `iss`; any disagreement between those is an
    /// error. A best-effort check-in follows so a token revoked before install is caught.
    pub async fn install_token(
        &self,
        token: LicenseToken,
        issuer: Option<&str>,
    ) -> Result<LicenseSummary, LicenseError> {
        let peek = verify::peek_claims(token.expose_secret())?;
        let product = peek.product.clone();
        ModuleId::new(&product).map_err(|_| {
            LicenseError::Malformed(format!("token product {product:?} is not owner/repo"))
        })?;
        let issuer = self.resolve_issuer(&product, issuer, peek.iss.as_deref())?;
        let header = verify::peek_header(token.expose_secret())?;
        let keys = self.keys_for(&issuer, header.kid.as_deref()).await?;
        let now = self.now();
        let claims =
            self.verifier
                .verify_license(token.expose_secret(), &keys, Some(&product), now)?;
        let stored = StoredLicense {
            product: product.clone(),
            issuer,
            token,
            checkin: CheckinRecord {
                last_ok: now,
                revoked: false,
            },
        };
        self.store.save(&stored)?;
        tracing::info!(
            product = %stored.product,
            issuer = %stored.issuer,
            licensee = %claims.licensee,
            "license installed"
        );
        if let Err(e) = self.checkin(&product, true).await {
            tracing::debug!(product = %product, error = %e, "post-install check-in skipped");
        }
        self.show(&product).await
    }

    /// Forget a product's license. `Ok(false)` when there was none.
    pub fn revoke_local(&self, product: &str) -> Result<bool, LicenseError> {
        let removed = self.store.remove(product)?;
        if removed {
            tracing::info!(product, "license removed");
        }
        Ok(removed)
    }

    async fn introspect_once(
        &self,
        stored: &StoredLicense,
        keys: &Jwks,
    ) -> Result<avada_module_sdk::license::IntrospectionResponse, LicenseError> {
        let discovery = introspect::discover(self.http.as_ref(), &stored.issuer).await?;
        IntrospectionClient::new(self.http.as_ref(), &self.verifier)
            .introspect(&discovery, &stored.issuer, &stored.token, keys)
            .await
    }

    /// Ask the issuer whether the license is still active. Unless `force`, only when
    /// `checkin_days()` have passed since the last confirmed check-in. A failed or
    /// unverifiable exchange leaves the record untouched.
    pub async fn checkin(
        &self,
        product: &str,
        force: bool,
    ) -> Result<CheckinOutcome, LicenseError> {
        let stored = self
            .store
            .load(product)?
            .ok_or_else(|| LicenseError::NotLicensed(product.to_string()))?;
        let (claims, keys) = self.claims_of(&stored).await?;
        let now = self.now();
        if !force && !introspect::checkin_due(&claims, &stored.checkin, now) {
            return Ok(CheckinOutcome::NotDue);
        }
        let attempt = match self.introspect_once(&stored, &keys).await {
            Err(LicenseError::Verify(VerifyError::UnknownKey(_))) => {
                match self.refresh_keys(&stored.issuer).await {
                    Ok(keys) => self.introspect_once(&stored, &keys).await,
                    Err(e) => Err(e),
                }
            }
            other => other,
        };
        let answer = match attempt {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(product, error = %e, "license check-in failed; record left as is");
                return Ok(CheckinOutcome::Unreachable(e.to_string()));
            }
        };
        let record = introspect::apply(&stored.checkin, &claims, &answer, now);
        if answer.active {
            tracing::debug!(product, "license check-in confirmed");
        } else {
            tracing::warn!(product, reason = ?answer.reason, "issuer reports the license inactive");
        }
        self.store.save_checkin(product, &record)?;
        if answer.active {
            if let Some(fresh) = answer.token.as_deref() {
                let fresh = LicenseToken::new(fresh);
                if self
                    .verifier
                    .verify_license(fresh.expose_secret(), &keys, Some(product), now)
                    .is_ok()
                {
                    self.store.save(&StoredLicense {
                        product: stored.product.clone(),
                        issuer: stored.issuer.clone(),
                        token: fresh,
                        checkin: record,
                    })?;
                    tracing::info!(product, "license token renewed by the issuer");
                }
            }
        }
        Ok(CheckinOutcome::Answered {
            active: answer.active,
            reason: answer.reason,
        })
    }

    /// Check in every installed license that is due.
    pub async fn checkin_all(&self) -> Vec<(String, Result<CheckinOutcome, LicenseError>)> {
        let products = match self.store.products() {
            Ok(p) => p,
            Err(e) => return vec![(String::new(), Err(e.into()))],
        };
        let mut out = Vec::with_capacity(products.len());
        for product in products {
            let outcome = self.checkin(&product, false).await;
            out.push((product, outcome));
        }
        out
    }

    /// Start an RFC 8628 device flow at `issuer` for `product`.
    pub async fn device_flow_start(
        &self,
        issuer: &str,
        product: &str,
    ) -> Result<DeviceCode, LicenseError> {
        let issuer = norm_issuer(issuer);
        ModuleId::new(product)
            .map_err(|_| LicenseError::Malformed(format!("{product:?} is not owner/repo")))?;
        self.check_issuer(product, &issuer)?;
        let discovery = introspect::discover(self.http.as_ref(), &issuer).await?;
        let endpoint = discovery
            .device_authorization_endpoint
            .clone()
            .ok_or_else(|| {
                LicenseError::Malformed("issuer offers no device authorization endpoint".into())
            })?;
        let token_endpoint = discovery
            .token_endpoint
            .clone()
            .ok_or_else(|| LicenseError::Malformed("issuer offers no token endpoint".into()))?;
        let resp = self
            .http
            .post_form(
                endpoint,
                None,
                vec![
                    ("client_id".into(), CLIENT_ID.into()),
                    ("scope".into(), product.to_string()),
                ],
                "application/json".into(),
            )
            .await?;
        if !resp.is_success() {
            return Err(issuer_error(&resp));
        }
        #[derive(Deserialize)]
        struct Auth {
            device_code: String,
            user_code: String,
            verification_uri: String,
            #[serde(default)]
            verification_uri_complete: Option<String>,
            expires_in: u64,
            #[serde(default = "default_interval")]
            interval: u64,
        }
        fn default_interval() -> u64 {
            5
        }
        let auth: Arc<Auth> = serde_json::from_slice(&resp.body)
            .map(Arc::new)
            .map_err(|e| LicenseError::Malformed(format!("device authorization answer: {e}")))?;
        let code = uuid::Uuid::new_v4().simple().to_string();
        self.flows.lock().unwrap().insert(
            code.clone(),
            PendingFlow {
                issuer,
                product: product.to_string(),
                device_code: LicenseToken::new(&auth.device_code),
                token_endpoint,
                expires_at: self.now().saturating_add(auth.expires_in),
            },
        );
        tracing::info!(product, "license device flow started");
        Ok(DeviceCode {
            code,
            product: product.to_string(),
            user_code: auth.user_code.clone(),
            verification_uri: auth.verification_uri.clone(),
            verification_uri_complete: auth.verification_uri_complete.clone(),
            expires_in: auth.expires_in,
            interval: auth.interval.max(1),
        })
    }

    /// Poll a device flow once. `Installed` when the user approved: the token is
    /// verified and stored before this returns.
    pub async fn device_flow_poll(&self, code: &str) -> Result<DevicePoll, LicenseError> {
        let flow = self
            .flows
            .lock()
            .unwrap()
            .get(code)
            .cloned()
            .ok_or_else(|| LicenseError::UnknownFlow(code.to_string()))?;
        if self.now() >= flow.expires_at {
            self.flows.lock().unwrap().remove(code);
            return Ok(DevicePoll::Expired);
        }
        let resp = self
            .http
            .post_form(
                flow.token_endpoint.clone(),
                None,
                vec![
                    ("grant_type".into(), DEVICE_GRANT.into()),
                    (
                        "device_code".into(),
                        flow.device_code.expose_secret().to_string(),
                    ),
                    ("client_id".into(), CLIENT_ID.into()),
                ],
                "application/json".into(),
            )
            .await?;
        if resp.is_success() {
            #[derive(Deserialize)]
            struct Grant {
                access_token: String,
            }
            let grant: Grant = serde_json::from_slice(&resp.body)
                .map_err(|e| LicenseError::Malformed(format!("token answer: {e}")))?;
            let token = LicenseToken::new(&grant.access_token);
            let peek = verify::peek_claims(token.expose_secret())?;
            if peek.product != flow.product {
                return Err(LicenseError::Malformed(format!(
                    "issuer granted a license for {} while {} was requested",
                    peek.product, flow.product
                )));
            }
            let license = self.install_token(token, Some(&flow.issuer)).await?;
            self.flows.lock().unwrap().remove(code);
            return Ok(DevicePoll::Installed { license });
        }
        match error_code(&resp).as_str() {
            "authorization_pending" => Ok(DevicePoll::Pending),
            "slow_down" => Ok(DevicePoll::SlowDown),
            "expired_token" => {
                self.flows.lock().unwrap().remove(code);
                Ok(DevicePoll::Expired)
            }
            "access_denied" => {
                self.flows.lock().unwrap().remove(code);
                Ok(DevicePoll::Denied)
            }
            _ => Err(issuer_error(&resp)),
        }
    }
}

/// The `error` member of an OAuth error answer, or `""`.
pub(crate) fn error_code(resp: &HttpResponse) -> String {
    serde_json::from_slice::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or_default()
}

/// A non-2xx answer as an error: the OAuth `error_description`/`error` when the body
/// has one, otherwise a short excerpt. Never the whole body.
pub(crate) fn issuer_error(resp: &HttpResponse) -> LicenseError {
    let text = resp.text();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("error_description")
                .or_else(|| v.get("error"))
                .or_else(|| v.get("message"))
                .and_then(|e| e.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| text.chars().take(120).collect());
    LicenseError::Issuer {
        status: resp.status,
        message,
    }
}
