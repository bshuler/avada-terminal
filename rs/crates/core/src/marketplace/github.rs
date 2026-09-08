//! GitHub as the marketplace index: repositories tagged `avada-module`.
//!
//! Everything goes through [`GitHubApi`] so tests point the real client at a local
//! fake (the base URLs are plain config) or substitute the trait entirely. The real
//! client is unauthenticated by default, caches every `GET` on disk ([`super::cache`])
//! and adds a bearer token only when the [`TokenStore`](super::token::TokenStore) has
//! one. Sign-in is GitHub's device flow: `device_start` hands the user a code to type at
//! `github.com/login/device`, `device_poll` asks whether they have.
//!
//! The token never appears in a log line, an error message or a cache key.

use super::cache::{now_secs, Cache, Entry};
use super::token::Token;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// A boxed future for the dyn-compatible async trait.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The topic every marketplace module carries.
pub const TOPIC: &str = "avada-module";

/// One repository as the index shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSummary {
    /// `owner/repo`.
    pub full_name: String,
    /// Repository description.
    #[serde(default)]
    pub description: Option<String>,
    /// Web URL.
    #[serde(default)]
    pub html_url: String,
    /// Stargazers.
    #[serde(default)]
    pub stars: u64,
    /// Last push, ISO-8601.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// One tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagInfo {
    /// `v1.2.3`.
    pub name: String,
    /// The commit the tag points at.
    pub commit: String,
}

/// The device-flow start: what to show the user. `device_code` is the secret half.
#[derive(Debug, Clone)]
pub struct DeviceStart {
    /// The secret the poll presents; never shown, never logged.
    pub device_code: Token,
    /// The code the user types.
    pub user_code: String,
    /// Where the user types it.
    pub verification_uri: String,
    /// Seconds until the codes expire.
    pub expires_in: u64,
    /// Minimum seconds between polls.
    pub interval: u64,
}

/// One poll's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePoll {
    /// The user has not finished yet.
    Pending,
    /// Polling too fast; back off.
    SlowDown,
    /// The codes expired; start over.
    Expired,
    /// The user refused.
    Denied,
    /// Signed in.
    Token(Token),
}

/// GitHub problems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubError {
    /// The request never got an answer.
    Http(String),
    /// An answer with an unexpected status (body trimmed).
    Status {
        /// HTTP status.
        status: u16,
        /// First part of the body.
        body: String,
    },
    /// The body was not the JSON expected.
    Decode(String),
    /// Rate-limited and nothing cached to fall back on.
    RateLimited,
    /// Sign-in needs an OAuth client id this build does not carry.
    NoClientId,
}

impl fmt::Display for GitHubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitHubError::Http(e) => write!(f, "github: {e}"),
            GitHubError::Status { status, body } => write!(f, "github answered {status}: {body}"),
            GitHubError::Decode(e) => write!(f, "github answer did not parse: {e}"),
            GitHubError::RateLimited => {
                write!(f, "github rate limit reached; sign in or try again later")
            }
            GitHubError::NoClientId => write!(
                f,
                "github sign-in is not configured (no OAuth client id; set AVADA_GITHUB_CLIENT_ID)"
            ),
        }
    }
}
impl std::error::Error for GitHubError {}

/// The index, abstracted.
pub trait GitHubApi: Send + Sync {
    /// Repositories matching `query` among those tagged [`TOPIC`], most-starred first.
    fn search<'a>(
        &'a self,
        query: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Vec<RepoSummary>, GitHubError>>;
    /// One repository, or `Ok(None)` when it does not exist.
    fn repo<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Option<RepoSummary>, GitHubError>>;
    /// The repository's tags, newest first as GitHub lists them.
    fn tags<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Vec<TagInfo>, GitHubError>>;
    /// A file's raw content at `reference`, or `Ok(None)` when absent.
    fn file<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        path: &'a str,
        reference: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Option<String>, GitHubError>>;
    /// Start the device flow.
    fn device_start(&self) -> BoxFuture<'_, Result<DeviceStart, GitHubError>>;
    /// Poll the device flow.
    fn device_poll<'a>(
        &'a self,
        device_code: &'a Token,
    ) -> BoxFuture<'a, Result<DevicePoll, GitHubError>>;
}

/// Real-client configuration. Tests override the bases to a local fake.
#[derive(Debug, Clone)]
pub struct GitHubConfig {
    /// REST base, `https://api.github.com`.
    pub api_base: String,
    /// Web base for the device flow, `https://github.com`.
    pub web_base: String,
    /// OAuth app client id for the device flow; empty means sign-in is unavailable.
    pub client_id: String,
    /// How long a cached answer is served without asking GitHub.
    pub ttl: Duration,
    /// `User-Agent` (GitHub refuses requests without one).
    pub user_agent: String,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        GitHubConfig {
            api_base: "https://api.github.com".into(),
            web_base: "https://github.com".into(),
            client_id: std::env::var("AVADA_GITHUB_CLIENT_ID").unwrap_or_default(),
            ttl: Duration::from_secs(5 * 60),
            user_agent: format!("avada-terminal/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// The `reqwest` client with the on-disk cache.
pub struct HttpGitHub {
    client: reqwest::Client,
    cfg: GitHubConfig,
    cache: Cache,
}

impl HttpGitHub {
    /// A client over `cfg`, caching under `cache`.
    pub fn new(cfg: GitHubConfig, cache: Cache) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        HttpGitHub { client, cfg, cache }
    }

    /// The configuration in force.
    pub fn config(&self) -> &GitHubConfig {
        &self.cfg
    }

    fn cache_key(url: &str, auth: Option<&Token>) -> String {
        // Authenticated answers can differ (private repos) — split the key on the
        // *fact* of a token, never its value.
        format!("{url}\n{}", if auth.is_some() { "auth" } else { "anon" })
    }

    /// `GET` through the cache: fresh → served from disk; stale → revalidated with
    /// `If-None-Match`; rate-limited → the stale copy if there is one.
    async fn get_cached(
        &self,
        url: &str,
        accept: &str,
        auth: Option<&Token>,
    ) -> Result<Option<String>, GitHubError> {
        let key = Self::cache_key(url, auth);
        let now = now_secs();
        let cached = self.cache.get(&key);
        if let Some(entry) = &cached {
            if entry.is_fresh(self.cfg.ttl, now) {
                return Ok(Some(entry.body.clone()));
            }
        }
        let mut req = self
            .client
            .get(url)
            .header("accept", accept)
            .header("user-agent", &self.cfg.user_agent)
            .header("x-github-api-version", "2022-11-28");
        if let Some(t) = auth {
            req = req.bearer_auth(t.expose_secret());
        }
        if let Some(etag) = cached.as_ref().and_then(|e| e.etag.clone()) {
            req = req.header("if-none-match", etag);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                // Offline with a stale copy: better than nothing, and marked as such
                // only by its age.
                if let Some(entry) = cached {
                    return Ok(Some(entry.body));
                }
                return Err(GitHubError::Http(e.to_string()));
            }
        };
        let status = resp.status().as_u16();
        match status {
            304 => {
                self.cache.touch(&key, now);
                Ok(cached.map(|e| e.body))
            }
            200 => {
                let etag = resp
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let body = resp
                    .text()
                    .await
                    .map_err(|e| GitHubError::Http(e.to_string()))?;
                self.cache.put(
                    &key,
                    &Entry {
                        etag,
                        body: body.clone(),
                        fetched_at: now,
                    },
                );
                Ok(Some(body))
            }
            404 => Ok(None),
            403 | 429 => match cached {
                Some(entry) => Ok(Some(entry.body)),
                None => Err(GitHubError::RateLimited),
            },
            _ => {
                let body = resp.text().await.unwrap_or_default();
                Err(GitHubError::Status {
                    status,
                    body: body.chars().take(200).collect(),
                })
            }
        }
    }

    async fn get_json(
        &self,
        url: &str,
        auth: Option<&Token>,
    ) -> Result<Option<Value>, GitHubError> {
        match self
            .get_cached(url, "application/vnd.github+json", auth)
            .await?
        {
            None => Ok(None),
            Some(body) => serde_json::from_str(&body)
                .map(Some)
                .map_err(|e| GitHubError::Decode(e.to_string())),
        }
    }

    async fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Value, GitHubError> {
        let body: String = form
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let resp = self
            .client
            .post(url)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("user-agent", &self.cfg.user_agent)
            .body(body)
            .send()
            .await
            .map_err(|e| GitHubError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| GitHubError::Http(e.to_string()))?;
        if status != 200 {
            return Err(GitHubError::Status {
                status,
                body: text.chars().take(200).collect(),
            });
        }
        serde_json::from_str(&text).map_err(|e| GitHubError::Decode(e.to_string()))
    }
}

/// Minimal `application/x-www-form-urlencoded` escaping.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn summary_of(v: &Value) -> RepoSummary {
    RepoSummary {
        full_name: v["full_name"].as_str().unwrap_or("").to_string(),
        description: v["description"].as_str().map(str::to_string),
        html_url: v["html_url"].as_str().unwrap_or("").to_string(),
        stars: v["stargazers_count"].as_u64().unwrap_or(0),
        updated_at: v["pushed_at"]
            .as_str()
            .or_else(|| v["updated_at"].as_str())
            .map(str::to_string),
    }
}

impl GitHubApi for HttpGitHub {
    fn search<'a>(
        &'a self,
        query: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Vec<RepoSummary>, GitHubError>> {
        Box::pin(async move {
            let q = if query.trim().is_empty() {
                format!("topic:{TOPIC}")
            } else {
                format!("{} topic:{TOPIC}", query.trim())
            };
            let url = format!(
                "{}/search/repositories?q={}&sort=stars&order=desc&per_page=50",
                self.cfg.api_base,
                encode(&q)
            );
            let Some(v) = self.get_json(&url, auth).await? else {
                return Ok(Vec::new());
            };
            Ok(v["items"]
                .as_array()
                .map(|items| items.iter().map(summary_of).collect())
                .unwrap_or_default())
        })
    }

    fn repo<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Option<RepoSummary>, GitHubError>> {
        Box::pin(async move {
            let url = format!("{}/repos/{owner}/{repo}", self.cfg.api_base);
            Ok(self.get_json(&url, auth).await?.map(|v| summary_of(&v)))
        })
    }

    fn tags<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Vec<TagInfo>, GitHubError>> {
        Box::pin(async move {
            let url = format!(
                "{}/repos/{owner}/{repo}/tags?per_page=100",
                self.cfg.api_base
            );
            let Some(v) = self.get_json(&url, auth).await? else {
                return Ok(Vec::new());
            };
            Ok(v.as_array()
                .map(|tags| {
                    tags.iter()
                        .filter_map(|t| {
                            Some(TagInfo {
                                name: t["name"].as_str()?.to_string(),
                                commit: t["commit"]["sha"].as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn file<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        path: &'a str,
        reference: &'a str,
        auth: Option<&'a Token>,
    ) -> BoxFuture<'a, Result<Option<String>, GitHubError>> {
        Box::pin(async move {
            let url = format!(
                "{}/repos/{owner}/{repo}/contents/{path}?ref={}",
                self.cfg.api_base,
                encode(reference)
            );
            self.get_cached(&url, "application/vnd.github.raw+json", auth)
                .await
        })
    }

    fn device_start(&self) -> BoxFuture<'_, Result<DeviceStart, GitHubError>> {
        Box::pin(async move {
            if self.cfg.client_id.is_empty() {
                return Err(GitHubError::NoClientId);
            }
            let url = format!("{}/login/device/code", self.cfg.web_base);
            let v = self
                .post_form(
                    &url,
                    &[("client_id", &self.cfg.client_id), ("scope", "read:user")],
                )
                .await?;
            let field = |k: &str| {
                v[k].as_str()
                    .map(str::to_string)
                    .ok_or_else(|| GitHubError::Decode(format!("device flow answer lacks `{k}`")))
            };
            Ok(DeviceStart {
                device_code: Token::new(field("device_code")?),
                user_code: field("user_code")?,
                verification_uri: field("verification_uri")?,
                expires_in: v["expires_in"].as_u64().unwrap_or(900),
                interval: v["interval"].as_u64().unwrap_or(5),
            })
        })
    }

    fn device_poll<'a>(
        &'a self,
        device_code: &'a Token,
    ) -> BoxFuture<'a, Result<DevicePoll, GitHubError>> {
        Box::pin(async move {
            if self.cfg.client_id.is_empty() {
                return Err(GitHubError::NoClientId);
            }
            let url = format!("{}/login/oauth/access_token", self.cfg.web_base);
            let v = self
                .post_form(
                    &url,
                    &[
                        ("client_id", &self.cfg.client_id),
                        ("device_code", device_code.expose_secret()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ],
                )
                .await?;
            if let Some(t) = v["access_token"].as_str() {
                return Ok(DevicePoll::Token(Token::new(t)));
            }
            Ok(match v["error"].as_str() {
                Some("authorization_pending") => DevicePoll::Pending,
                Some("slow_down") => DevicePoll::SlowDown,
                Some("expired_token") => DevicePoll::Expired,
                Some("access_denied") => DevicePoll::Denied,
                other => {
                    return Err(GitHubError::Decode(format!(
                        "device flow answered {}",
                        other.unwrap_or("nothing")
                    )))
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_escapes_what_github_needs() {
        assert_eq!(encode("topic:avada-module"), "topic%3Aavada-module");
        assert_eq!(encode("a b"), "a+b");
        assert_eq!(encode("v1.0.0"), "v1.0.0");
        assert_eq!(encode("urn:x/y"), "urn%3Ax%2Fy");
    }

    #[test]
    fn summary_reads_the_fields_the_ui_shows() {
        let v = serde_json::json!({
            "full_name": "acme/avada-files",
            "description": "Files",
            "html_url": "https://github.com/acme/avada-files",
            "stargazers_count": 7,
            "pushed_at": "2026-01-01T00:00:00Z"
        });
        let s = summary_of(&v);
        assert_eq!(s.full_name, "acme/avada-files");
        assert_eq!(s.stars, 7);
        assert_eq!(s.updated_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        let bare = summary_of(&serde_json::json!({}));
        assert_eq!(bare.full_name, "");
        assert_eq!(bare.stars, 0);
    }

    #[test]
    fn cache_key_splits_on_the_fact_of_a_token_not_its_value() {
        let t = Token::new("ghp_value");
        let anon = HttpGitHub::cache_key("u", None);
        let auth = HttpGitHub::cache_key("u", Some(&t));
        assert_ne!(anon, auth);
        assert!(!auth.contains("ghp_value"));
    }

    #[test]
    fn device_start_errors_are_never_the_token() {
        let e = GitHubError::NoClientId.to_string();
        assert!(e.contains("AVADA_GITHUB_CLIENT_ID"));
        let d = DeviceStart {
            device_code: Token::new("dc-secret"),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 1,
            interval: 1,
        };
        assert!(!format!("{d:?}").contains("dc-secret"));
    }
}
