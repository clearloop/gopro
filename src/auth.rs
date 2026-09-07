//! Credential storage and token refresh.
//!
//! Two ways in:
//!   1. `gopro-dl token --access-token ...` — paste a bearer token lifted from
//!      the browser (Network tab on <https://plus.gopro.com>, any request to
//!      api.gopro.com, `Authorization: Bearer ...`). Always works.
//!   2. `gopro-dl login` — OAuth password grant. Convenient, but GoPro gates it
//!      behind rotating client credentials, captchas and 2FA, so treat it as
//!      best-effort and fall back to (1).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const OAUTH_URL: &str = "https://api.gopro.com/v1/oauth2/token";

/// Public web-client credentials used by the GoPro Plus web app. GoPro rotates
/// these; override with `GOPRO_CLIENT_ID` / `GOPRO_CLIENT_SECRET` if login
/// starts returning `invalid_client`.
pub const DEFAULT_CLIENT_ID: &str =
    "71611e67ea968cfacf45e2b6936c81156fcf5dbe553a2bf2d342da1562d05f46";
pub const DEFAULT_CLIENT_SECRET: &str =
    "3863c9b438c07b82f39ab3eeeef9c24fefa50c6856253e3f1d37e0e3b1ead68d";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl Credentials {
    pub fn expires_soon(&self) -> bool {
        match self.expires_at {
            // Refresh with slack so a long download does not die mid-stream.
            Some(at) => at - Duration::minutes(5) <= Utc::now(),
            None => false,
        }
    }
}

/// Pull an access token out of whatever the user pasted.
///
/// GoPro's web app carries the token in a `gp_access_token` cookie rather than
/// an `Authorization` header, so the useful thing to accept is: a bare token, a
/// full header line, a `name=value` cookie pair, or an entire `Cookie:` header
/// with the token buried among other pairs.
pub fn normalize_token(raw: &str) -> Option<String> {
    let raw = raw.trim();

    // A whole cookie jar, or a single `gp_access_token=...` pair.
    if let Some(idx) = raw.find("gp_access_token=") {
        let rest = &raw[idx + "gp_access_token=".len()..];
        let value = rest.split(&[';', ' ', ','][..]).next().unwrap_or(rest);
        return clean(value);
    }

    let rest = strip_prefix_ci(raw, "authorization:").trim();
    clean(strip_prefix_ci(rest, "bearer"))
}

/// `trim_start_matches` is case-sensitive and needs the exact separator; header
/// names and the auth scheme are neither.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> &'a str {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        s[prefix.len()..].trim_start()
    } else {
        s
    }
}

fn clean(s: &str) -> Option<String> {
    let t = s.trim().trim_matches(['"', '\'']).trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gopro-dl")
        .join("credentials.json")
}

pub fn load(path: &Path) -> Result<Credentials> {
    if let Ok(tok) = std::env::var("GOPRO_ACCESS_TOKEN") {
        if !tok.trim().is_empty() {
            return Ok(Credentials {
                access_token: tok.trim().to_string(),
                refresh_token: std::env::var("GOPRO_REFRESH_TOKEN").ok(),
                expires_at: None,
            });
        }
    }
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "no credentials at {}. Run `gopro-dl login`, or `gopro-dl token --access-token <TOKEN>` \
             with a bearer token copied from plus.gopro.com",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("{} is not valid JSON", path.display()))
}

pub fn save(path: &Path, creds: &Credentials) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(creds)?)?;
    restrict(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn client_id() -> String {
    std::env::var("GOPRO_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string())
}

fn client_secret() -> String {
    std::env::var("GOPRO_CLIENT_SECRET").unwrap_or_else(|_| DEFAULT_CLIENT_SECRET.to_string())
}

fn into_credentials(t: TokenResponse) -> Credentials {
    Credentials {
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: t.expires_in.map(|s| Utc::now() + Duration::seconds(s)),
    }
}

pub async fn password_login(
    http: &reqwest::Client,
    email: &str,
    password: &str,
    two_factor: Option<&str>,
) -> Result<Credentials> {
    let cid = client_id();
    let csec = client_secret();
    let mut form = vec![
        ("grant_type", "password"),
        ("client_id", cid.as_str()),
        ("client_secret", csec.as_str()),
        ("username", email),
        ("password", password),
        ("scope", "root root:channels public me upload media_library:edit live"),
    ];
    if let Some(code) = two_factor {
        form.push(("two_factor_code", code));
    }

    let resp = http
        .post(OAUTH_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("Origin", "https://gopro.com")
        .form(&form)
        .send()
        .await
        .context("could not reach api.gopro.com")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let parsed: TokenResponse =
            serde_json::from_str(&body).context("unexpected token response shape")?;
        return Ok(into_credentials(parsed));
    }

    if body.contains("two_factor") && two_factor.is_none() {
        return Err(anyhow!("TWO_FACTOR_REQUIRED"));
    }
    Err(anyhow!(
        "login failed ({status}): {}\n\nGoPro frequently blocks scripted logins. \
         Copy a bearer token from plus.gopro.com instead and run:\n  \
         gopro-dl token --access-token <TOKEN>",
        body.trim()
    ))
}

pub async fn refresh(http: &reqwest::Client, refresh_token: &str) -> Result<Credentials> {
    let cid = client_id();
    let csec = client_secret();
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", cid.as_str()),
        ("client_secret", csec.as_str()),
        ("refresh_token", refresh_token),
    ];
    let resp = http
        .post(OAUTH_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("token refresh failed ({status}): {}", body.trim()));
    }
    let mut creds = into_credentials(serde_json::from_str(&body)?);
    // GoPro sometimes omits refresh_token on refresh; keep the old one.
    if creds.refresh_token.is_none() {
        creds.refresh_token = Some(refresh_token.to_string());
    }
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::normalize_token;

    const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc-_123";

    #[test]
    fn accepts_a_bare_token() {
        assert_eq!(normalize_token(JWT).as_deref(), Some(JWT));
        assert_eq!(normalize_token(&format!("  {JWT}\n")).as_deref(), Some(JWT));
    }

    #[test]
    fn strips_an_authorization_header() {
        assert_eq!(normalize_token(&format!("Bearer {JWT}")).as_deref(), Some(JWT));
        assert_eq!(
            normalize_token(&format!("Authorization: Bearer {JWT}")).as_deref(),
            Some(JWT)
        );
        assert_eq!(
            normalize_token(&format!("authorization: bearer {JWT}")).as_deref(),
            Some(JWT)
        );
    }

    #[test]
    fn extracts_the_gopro_cookie() {
        assert_eq!(
            normalize_token(&format!("gp_access_token={JWT}")).as_deref(),
            Some(JWT)
        );
        // A whole cookie jar copied out of DevTools, token in the middle.
        assert_eq!(
            normalize_token(&format!("_ga=GA1.2.9; gp_access_token={JWT}; gp_user=42")).as_deref(),
            Some(JWT)
        );
        assert_eq!(
            normalize_token(&format!("Cookie: gp_access_token=\"{JWT}\"")).as_deref(),
            Some(JWT)
        );
    }

    #[test]
    fn rejects_nothing_useful() {
        assert_eq!(normalize_token(""), None);
        assert_eq!(normalize_token("   \n "), None);
        assert_eq!(normalize_token("Bearer "), None);
        assert_eq!(normalize_token("gp_access_token="), None);
    }
}
