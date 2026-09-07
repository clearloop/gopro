//! Thin client over api.gopro.com with retry, backoff and token refresh.

use anyhow::{anyhow, bail, Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::auth::{self, Credentials};
use crate::model::{DownloadResponse, MediaSearchResponse, MediaUser};

const DEFAULT_API_BASE: &str = "https://api.gopro.com";

/// Overridable so the integration tests can point at a local stand-in.
pub fn api_base() -> &'static str {
    static BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        std::env::var("GOPRO_API_BASE").unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
    })
}
const UA: &str = concat!("gopro-dl/", env!("CARGO_PKG_VERSION"));

const SEARCH_FIELDS: &str = "captured_at,content_title,content_type,created_at,file_extension,\
file_size,filename,fov,height,id,item_count,moments_count,orientation,play_as,ready_to_edit,\
ready_to_view,resolution,source_duration,token,type,width";

pub struct Api {
    pub http: reqwest::Client,
    creds: Arc<RwLock<Credentials>>,
    creds_path: std::path::PathBuf,
    retries: u32,
}

impl Api {
    pub fn new(creds: Credentials, creds_path: std::path::PathBuf, retries: u32) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            // Large files over flaky links: generous read timeout, no total cap.
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            creds: Arc::new(RwLock::new(creds)),
            creds_path,
            retries: retries.max(1),
        })
    }

    pub async fn access_token(&self) -> String {
        self.creds.read().await.access_token.clone()
    }

    /// Refresh proactively when the token is about to expire, or reactively
    /// after a 401. Returns false when we have no refresh token to work with.
    pub async fn try_refresh(&self) -> Result<bool> {
        let refresh_token = {
            let c = self.creds.read().await;
            match c.refresh_token.clone() {
                Some(t) => t,
                None => return Ok(false),
            }
        };
        let fresh = auth::refresh(&self.http, &refresh_token).await?;
        auth::save(&self.creds_path, &fresh)?;
        *self.creds.write().await = fresh;
        debug!("access token refreshed");
        Ok(true)
    }

    async fn ensure_fresh(&self) -> Result<()> {
        if self.creds.read().await.expires_soon() {
            let _ = self.try_refresh().await?;
        }
        Ok(())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.ensure_fresh().await?;
        let mut delay = Duration::from_millis(700);
        let mut refreshed = false;

        for attempt in 1..=self.retries {
            let token = self.access_token().await;
            let resp = self
                .http
                .get(url)
                .bearer_auth(&token)
                // The web app authenticates by cookie, and some endpoints only
                // honour that. Sending both costs nothing and covers either.
                .header(reqwest::header::COOKIE, format!("gp_access_token={token}"))
                .header(reqwest::header::ACCEPT, "application/vnd.gopro.jk.media+json; version=2.0.0")
                .header("Accept-Charset", "utf-8")
                .header("Origin", "https://plus.gopro.com")
                .header("Referer", "https://plus.gopro.com/")
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) if attempt < self.retries => {
                    warn!("GET {url} failed ({e}); retry {attempt}/{}", self.retries);
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("GET {url}")),
            };

            let status = resp.status();

            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                if self.try_refresh().await.unwrap_or(false) {
                    continue;
                }
                bail!(
                    "GoPro rejected the access token (401) and it could not be refreshed.\n\
                     Grab a fresh bearer token from plus.gopro.com and run:\n  \
                     gopro-dl token --access-token <TOKEN>"
                );
            }

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                if attempt < self.retries {
                    let wait = retry_after(&resp).unwrap_or(delay);
                    warn!("GET {url} -> {status}; backing off {wait:?}");
                    tokio::time::sleep(wait).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                    continue;
                }
                bail!("GET {url} -> {status} after {} attempts", self.retries);
            }

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("GET {url} -> {status}: {}", truncate(&body, 400));
            }

            // Reading the body can time out just like connecting can, and on a
            // multi-hour archive run that must not abort everything.
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) if attempt < self.retries => {
                    warn!("GET {url}: body read failed ({e}); retry {attempt}/{}", self.retries);
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("reading body of {url}")),
            };
            return serde_json::from_slice(&body).with_context(|| {
                format!(
                    "could not parse response from {url}: {}",
                    truncate(&String::from_utf8_lossy(&body), 400)
                )
            });
        }
        Err(anyhow!("GET {url}: exhausted retries"))
    }

    /// One page of the media library, newest capture first.
    pub async fn search_page(&self, page: u32, per_page: u32) -> Result<MediaSearchResponse> {
        let url = format!(
            "{base}/media/search?fields={SEARCH_FIELDS}\
             &processing_states=registered,rendering,pretranscoding,transcoding,failure,ready\
             &order_by=captured_at&per_page={per_page}&page={page}",
            base = api_base()
        );
        self.get_json(&url).await
    }

    /// Signed CDN URLs for one media item. These expire within minutes, so call
    /// this immediately before downloading and re-call on a 403.
    pub async fn download_links(&self, media_id: &str) -> Result<DownloadResponse> {
        let url = format!("{}/media/{media_id}/download", api_base());
        self.get_json(&url).await
    }

    /// Account summary. Note this is `/media/user`, not `/v1/user` — the
    /// latter 404s.
    pub async fn media_user(&self) -> Result<MediaUser> {
        self.get_json(&format!("{}/media/user", api_base())).await
    }

}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s.min(300)))
}

pub fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}
