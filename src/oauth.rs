//! Claude subscription OAuth: PKCE login, token exchange, refresh, profile.
//!
//! Constants match the Claude Code OAuth application so subscription
//! (Max/Pro) tokens are accepted by api.anthropic.com.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Refresh tokens this many ms before they expire.
pub const REFRESH_THRESHOLD_MS: i64 = 5 * 60 * 1000;

/// PKCE verifier/challenge pair plus the random state.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

pub fn generate_pkce() -> Pkce {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut verifier_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let verifier = b64.encode(verifier_bytes);

    let challenge = b64.encode(Sha256::digest(verifier.as_bytes()));

    let mut state_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut state_bytes);
    let state = b64.encode(state_bytes);

    Pkce {
        verifier,
        challenge,
        state,
    }
}

/// Build the authorize URL the user opens in their browser.
pub fn authorize_url(challenge: &str, state: &str, redirect_uri: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("valid base url");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

/// Tokens returned by the OAuth token endpoint.
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// API may return expires_at in seconds or ms; normalize to ms.
fn normalize_expires_at(resp: &TokenResponse) -> i64 {
    if let Some(at) = resp.expires_at {
        if at < 1_000_000_000_000 {
            at * 1000
        } else {
            at
        }
    } else {
        now_ms() + resp.expires_in.unwrap_or(3600) * 1000
    }
}

/// Exchange an authorization code for tokens (PKCE authorization_code grant).
pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    state: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("token exchange failed: {status} {text}");
    }

    let tr: TokenResponse = resp.json().await.context("parsing token response")?;
    let expires_at = normalize_expires_at(&tr);
    Ok(Tokens {
        refresh_token: tr
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("no refresh_token in response"))?,
        access_token: tr.access_token,
        expires_at,
    })
}

/// Refresh an access token. The server may rotate the refresh token.
pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> Result<Tokens> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    });

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", "axios/1.13.6")
        .json(&body)
        .send()
        .await
        .context("refresh request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("token refresh failed: {status} {text}");
    }

    let tr: TokenResponse = resp.json().await.context("parsing refresh response")?;
    let expires_at = normalize_expires_at(&tr);
    let refresh_token = tr
        .refresh_token
        .clone()
        .unwrap_or_else(|| refresh_token.to_string());
    Ok(Tokens {
        access_token: tr.access_token,
        refresh_token,
        expires_at,
    })
}

/// Account profile returned after login (used to label the account).
pub struct Profile {
    pub uuid: Option<String>,
    pub email: Option<String>,
    pub tier: Option<String>,
}

pub async fn fetch_profile(client: &reqwest::Client, access_token: &str) -> Result<Profile> {
    #[derive(Deserialize)]
    struct AccountInfo {
        uuid: Option<String>,
        email: Option<String>,
        #[serde(default)]
        has_claude_max: bool,
        #[serde(default)]
        has_claude_pro: bool,
    }
    #[derive(Deserialize)]
    struct ProfileResponse {
        account: Option<AccountInfo>,
    }

    let resp = client
        .get(PROFILE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .context("profile request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("profile fetch failed: {status} {text}");
    }

    let pr: ProfileResponse = resp.json().await.context("parsing profile response")?;
    let account = pr.account;
    Ok(Profile {
        uuid: account.as_ref().and_then(|a| a.uuid.clone()),
        email: account.as_ref().and_then(|a| a.email.clone()),
        tier: account.as_ref().map(|a| {
            if a.has_claude_max {
                "max".to_string()
            } else if a.has_claude_pro {
                "pro".to_string()
            } else {
                "free".to_string()
            }
        }),
    })
}
