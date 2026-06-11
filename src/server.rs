//! The rotating proxy. Forwards Anthropic API requests using the active
//! account's OAuth token; on a rate-limit/quota error it cools that account
//! down, rotates to the next, and retries — transparently to the client.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use futures_util::StreamExt;
use tokio::sync::Mutex;

use crate::config::{Config, Strategy};
use crate::oauth;

const UPSTREAM: &str = "https://api.anthropic.com";
/// Default cooldown when the upstream gives no `retry-after`.
const DEFAULT_COOLDOWN_MS: i64 = 60_000;
/// How long a conversation stays pinned to an account (session-sticky).
const STICKY_TTL_MS: i64 = 60 * 60 * 1000;
/// Proactively switch away from an account at/over this quota utilization.
const QUOTA_THRESHOLD: f64 = 0.95;

/// Hop-by-hop headers that must not be forwarded.
const HOP_BY_HOP: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
    "content-length",
];

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Mutex<Config>>,
    pub client: reqwest::Client,
    /// Base URL of the upstream API (overridable for tests).
    pub upstream: String,
    /// Where to persist config on state changes. `None` disables writes
    /// entirely (used by tests so they never touch the user's real config).
    pub config_path: Option<std::path::PathBuf>,
    /// Serializes token refreshes so a rotated refresh-token isn't raced.
    pub refresh_lock: Arc<Mutex<()>>,
    /// session-sticky map: conversation key → (account index, last-seen ms).
    pub sticky: Arc<Mutex<HashMap<u64, (usize, i64)>>>,
    pub verbose: bool,
}

/// Build shared proxy state. `upstream` is the API base URL (no trailing slash).
/// `config_path` of `None` means changes are kept in memory only (tests).
pub fn make_state(
    cfg: Config,
    upstream: String,
    verbose: bool,
    config_path: Option<std::path::PathBuf>,
) -> AppState {
    AppState {
        cfg: Arc::new(Mutex::new(cfg)),
        client: reqwest::Client::builder()
            .build()
            .expect("building http client"),
        upstream,
        config_path,
        refresh_lock: Arc::new(Mutex::new(())),
        sticky: Arc::new(Mutex::new(HashMap::new())),
        verbose,
    }
}

/// Persist config to disk only if this state has a configured path.
fn persist(state: &AppState, cfg: &Config) {
    if let Some(path) = &state.config_path {
        let _ = cfg.save_to(path);
    }
}

/// Build the axum router for the proxy.
pub fn router(state: AppState) -> Router {
    Router::new().fallback(any(handle)).with_state(state)
}

pub async fn serve(cfg: Config) -> Result<()> {
    let port = cfg.port;
    let verbose = std::env::var("CLAUDEN_VERBOSE").is_ok();
    let upstream = std::env::var("CLAUDEN_UPSTREAM").unwrap_or_else(|_| UPSTREAM.to_string());
    println!("  strategy: {}", cfg.strategy.label());

    let config_path = Config::path().ok();
    let state = make_state(cfg, upstream, verbose, config_path);
    let app = router(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("  clauden proxy listening on http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Should this status trigger account rotation? 429 rate-limit, 402 credit,
/// 529 overloaded.
fn is_exhaustion(status: u16) -> bool {
    matches!(status, 429 | 402 | 529)
}

/// An account is usable if it's neither cooling down (reactive) nor near its
/// quota limit (proactive).
fn available(a: &crate::config::Account, now: i64) -> bool {
    !a.is_cooling_down(now) && !a.is_near_quota(now, QUOTA_THRESHOLD)
}

/// Round-robin: next usable account, preferring `current`, skipping cooldowns.
fn pick_round_robin(cfg: &Config, now: i64) -> Option<usize> {
    let n = cfg.accounts.len();
    if n == 0 {
        return None;
    }
    for offset in 0..n {
        let idx = (cfg.current + offset) % n;
        if available(&cfg.accounts[idx], now) {
            return Some(idx);
        }
    }
    None
}

/// Least-used: available account with the lowest `usage_count` (tie → lowest
/// index for determinism).
fn pick_least_used(cfg: &Config, now: i64) -> Option<usize> {
    cfg.accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| available(a, now))
        .min_by_key(|(idx, a)| (a.usage_count, *idx))
        .map(|(idx, _)| idx)
}

/// Derive a stable key for a conversation from the request body, so all turns
/// of the same conversation hash to the same value. Uses the system prompt plus
/// the first user message — the cacheable prefix that stays constant as the
/// conversation grows. Returns `None` if the body isn't parseable JSON.
fn session_key(body: &[u8]) -> Option<u64> {
    use std::hash::{Hash, Hasher};

    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let mut basis = String::new();
    if let Some(system) = v.get("system") {
        basis.push_str(&stringify_content(system));
    }
    if let Some(first) = v.get("messages").and_then(|m| m.as_array()).and_then(|a| a.first()) {
        if let Some(content) = first.get("content") {
            basis.push('\u{1}');
            basis.push_str(&stringify_content(content));
        }
    }
    if basis.is_empty() {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    basis.hash(&mut h);
    Some(h.finish())
}

/// Flatten an Anthropic content field (string or array of blocks) to text.
fn stringify_content(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Choose the account index for this request according to the configured
/// strategy. For session-sticky, honors/updates the sticky map.
async fn select_account(state: &AppState, key: Option<u64>, now: i64) -> Option<usize> {
    let cfg = state.cfg.lock().await;
    if cfg.accounts.is_empty() {
        return None;
    }

    match cfg.strategy {
        Strategy::RoundRobin => pick_round_robin(&cfg, now),
        Strategy::LeastUsed => pick_least_used(&cfg, now),
        Strategy::SessionSticky => {
            let mut sticky = state.sticky.lock().await;
            prune_sticky(&mut sticky, now);

            if let Some(k) = key {
                // Reuse the pinned account if it's still available.
                if let Some(&(idx, _)) = sticky.get(&k) {
                    if idx < cfg.accounts.len() && available(&cfg.accounts[idx], now) {
                        sticky.insert(k, (idx, now));
                        return Some(idx);
                    }
                }
                // New conversation, or pinned account is cooling down / near
                // quota: pick a fresh one (least-used) and pin it.
                let idx = pick_least_used(&cfg, now)?;
                sticky.insert(k, (idx, now));
                Some(idx)
            } else {
                // No identifiable conversation: behave like least-used.
                pick_least_used(&cfg, now)
            }
        }
    }
}

/// Drop sticky entries older than the TTL.
fn prune_sticky(map: &mut HashMap<u64, (usize, i64)>, now: i64) {
    map.retain(|_, (_, last_seen)| now - *last_seen < STICKY_TTL_MS);
}

/// Ensure the account at `idx` has a fresh access token; refresh if expiring.
/// Returns the usable access token.
async fn ensure_fresh(state: &AppState, idx: usize) -> Result<String> {
    // Fast path: token still valid.
    {
        let cfg = state.cfg.lock().await;
        let acct = &cfg.accounts[idx];
        if acct.expires_at - now_ms() > oauth::REFRESH_THRESHOLD_MS {
            return Ok(acct.access_token.clone());
        }
    }

    // Serialize refreshes to avoid racing a rotated refresh token.
    let _guard = state.refresh_lock.lock().await;

    // Re-check after acquiring the lock (another task may have refreshed).
    let refresh_token = {
        let cfg = state.cfg.lock().await;
        let acct = &cfg.accounts[idx];
        if acct.expires_at - now_ms() > oauth::REFRESH_THRESHOLD_MS {
            return Ok(acct.access_token.clone());
        }
        acct.refresh_token.clone()
    };

    let tokens = oauth::refresh(&state.client, &refresh_token).await?;

    let mut cfg = state.cfg.lock().await;
    let acct = &mut cfg.accounts[idx];
    acct.access_token = tokens.access_token.clone();
    acct.refresh_token = tokens.refresh_token;
    acct.expires_at = tokens.expires_at;
    persist(state, &cfg);
    Ok(tokens.access_token)
}

async fn handle(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "failed to read request body"),
    };

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", state.upstream, path_and_query);

    let account_count = { state.cfg.lock().await.accounts.len() };
    if account_count == 0 {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no accounts configured — run `clauden login`",
        );
    }

    // Stable conversation key (for session-sticky); cheap no-op otherwise.
    let skey = session_key(&body_bytes);

    // Try each account at most once.
    for _ in 0..account_count {
        let now = now_ms();
        let idx = match select_account(&state, skey, now).await {
            Some(i) => i,
            None => {
                return error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "all accounts are rate-limited or at quota — try again shortly",
                )
            }
        };

        let access_token = match ensure_fresh(&state, idx).await {
            Ok(t) => t,
            Err(e) => {
                if state.verbose {
                    eprintln!("[clauden] token refresh failed for account {idx}: {e}");
                }
                // Cool this one down briefly and try the next.
                cool_down_and_rotate(&state, idx, DEFAULT_COOLDOWN_MS).await;
                continue;
            }
        };

        let account_name = {
            let cfg = state.cfg.lock().await;
            cfg.accounts[idx].name.clone()
        };

        // Build the upstream request.
        let mut headers = forward_headers(&parts.headers);
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {access_token}"))
                .unwrap_or(HeaderValue::from_static("")),
        );

        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
            .unwrap_or(reqwest::Method::POST);

        let upstream_resp = state
            .client
            .request(method, &url)
            .headers(headers)
            .body(body_bytes.clone())
            .send()
            .await;

        let resp = match upstream_resp {
            Ok(r) => r,
            Err(e) => {
                if state.verbose {
                    eprintln!("[clauden] upstream error on {account_name}: {e}");
                }
                cool_down_and_rotate(&state, idx, DEFAULT_COOLDOWN_MS).await;
                continue;
            }
        };

        let status = resp.status().as_u16();

        if is_exhaustion(status) {
            let cooldown = retry_after_ms(resp.headers()).unwrap_or(DEFAULT_COOLDOWN_MS);
            if state.verbose {
                eprintln!(
                    "[clauden] ⚡ {account_name} hit {status}; cooling {}s and rotating",
                    cooldown / 1000
                );
            }
            cool_down_and_rotate(&state, idx, cooldown).await;
            continue;
        }

        // Success (or a non-rotation error): record usage + quota, stream back.
        {
            let mut cfg = state.cfg.lock().await;
            if let Some(acct) = cfg.accounts.get_mut(idx) {
                acct.usage_count = acct.usage_count.saturating_add(1);
                read_quota(acct, resp.headers());
                if state.verbose && acct.is_near_quota(now_ms(), QUOTA_THRESHOLD) {
                    eprintln!(
                        "[clauden] {} near quota (5h={:?} 7d={:?}); will switch next request",
                        acct.name, acct.util_5h, acct.util_7d
                    );
                }
            }
        }
        return stream_response(resp);
    }

    error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "all accounts exhausted for this request",
    )
}

/// Mark account `idx` as cooling down and advance the round-robin pointer.
async fn cool_down_and_rotate(state: &AppState, idx: usize, cooldown_ms: i64) {
    let mut cfg = state.cfg.lock().await;
    let n = cfg.accounts.len();
    cfg.accounts[idx].cooldown_until = Some(now_ms() + cooldown_ms);
    if n > 0 {
        cfg.current = (idx + 1) % n;
    }
    persist(state, &cfg);
}

/// Parse `retry-after` (seconds) into milliseconds.
fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    let v = headers.get("retry-after")?.to_str().ok()?;
    let secs: i64 = v.trim().parse().ok()?;
    Some(secs * 1000)
}

/// Read Anthropic unified rate-limit headers off a response into account quota
/// state. Utilization arrives pre-normalized (0.0–1.0); resets are epoch seconds.
fn read_quota(acct: &mut crate::config::Account, headers: &reqwest::header::HeaderMap) {
    fn util(h: &reqwest::header::HeaderMap, key: &str) -> Option<f64> {
        h.get(key)?.to_str().ok()?.trim().parse().ok()
    }
    fn reset_ms(h: &reqwest::header::HeaderMap, key: &str) -> Option<i64> {
        let secs: i64 = h.get(key)?.to_str().ok()?.trim().parse().ok()?;
        Some(secs * 1000)
    }
    if let Some(u) = util(headers, "anthropic-ratelimit-unified-5h-utilization") {
        acct.util_5h = Some(u);
    }
    if let Some(u) = util(headers, "anthropic-ratelimit-unified-7d-utilization") {
        acct.util_7d = Some(u);
    }
    if let Some(r) = reset_ms(headers, "anthropic-ratelimit-unified-5h-reset") {
        acct.reset_5h = Some(r);
    }
    if let Some(r) = reset_ms(headers, "anthropic-ratelimit-unified-7d-reset") {
        acct.reset_7d = Some(r);
    }
}

/// Copy client headers to the upstream request, dropping hop-by-hop +
/// auth/encoding headers we manage ourselves.
fn forward_headers(incoming: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in incoming.iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str()) {
            continue;
        }
        if lname == "authorization" || lname == "x-api-key" || lname == "accept-encoding" {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(n, v);
        }
    }
    out
}

/// Wrap an upstream reqwest response as a streaming axum response.
fn stream_response(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);

    let mut builder = Response::builder().status(status);
    for (name, value) in resp.headers().iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str()) || lname == "content-encoding" {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(n, v);
        }
    }

    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    let body = Body::from_stream(stream);

    builder
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "failed to build response"))
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"error\":{{\"type\":\"clauden_proxy\",\"message\":{}}}}}",
            serde_json::to_string(msg).unwrap_or_else(|_| "\"error\"".into())
        )))
        .expect("static error response")
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Account;

    fn acct(name: &str, cooldown_until: Option<i64>) -> Account {
        Account {
            name: name.into(),
            account_uuid: None,
            tier: None,
            org_uuid: None,
            org_name: None,
            access_token: "t".into(),
            refresh_token: "r".into(),
            expires_at: i64::MAX,
            cooldown_until,
            usage_count: 0,
            util_5h: None,
            util_7d: None,
            reset_5h: None,
            reset_7d: None,
        }
    }

    #[test]
    fn exhaustion_statuses_rotate() {
        for s in [429, 402, 529] {
            assert!(is_exhaustion(s), "{s} should rotate");
        }
        for s in [200, 400, 401, 500, 503] {
            assert!(!is_exhaustion(s), "{s} should not rotate");
        }
    }

    #[test]
    fn pick_prefers_current_when_available() {
        let mut cfg = Config::default();
        cfg.accounts = vec![acct("a", None), acct("b", None), acct("c", None)];
        cfg.current = 1;
        assert_eq!(pick_round_robin(&cfg, 1000), Some(1));
    }

    #[test]
    fn pick_skips_cooling_accounts_and_wraps() {
        let now = 1000;
        let mut cfg = Config::default();
        cfg.accounts = vec![
            acct("a", Some(now + 5000)), // cooling
            acct("b", Some(now + 5000)), // cooling
            acct("c", None),             // ready
        ];
        cfg.current = 0;
        assert_eq!(pick_round_robin(&cfg, now), Some(2));
    }

    #[test]
    fn pick_none_when_all_cooling() {
        let now = 1000;
        let mut cfg = Config::default();
        cfg.accounts = vec![acct("a", Some(now + 5000)), acct("b", Some(now + 5000))];
        assert_eq!(pick_round_robin(&cfg, now), None);
    }

    #[test]
    fn pick_none_when_empty() {
        let cfg = Config::default();
        assert_eq!(pick_round_robin(&cfg, 1000), None);
    }

    #[test]
    fn expired_cooldown_is_available_again() {
        let now = 10_000;
        let mut cfg = Config::default();
        cfg.accounts = vec![acct("a", Some(now - 1))]; // cooldown already passed
        cfg.current = 0;
        assert_eq!(pick_round_robin(&cfg, now), Some(0));
    }

    #[test]
    fn least_used_picks_lowest_count() {
        let mut cfg = Config::default();
        cfg.accounts = vec![acct("a", None), acct("b", None), acct("c", None)];
        cfg.accounts[0].usage_count = 5;
        cfg.accounts[1].usage_count = 2;
        cfg.accounts[2].usage_count = 9;
        assert_eq!(pick_least_used(&cfg, 1000), Some(1));
    }

    #[test]
    fn least_used_skips_cooling_and_breaks_ties_by_index() {
        let now = 1000;
        let mut cfg = Config::default();
        cfg.accounts = vec![
            acct("a", Some(now + 5000)), // cooling, even if 0 usage
            acct("b", None),
            acct("c", None),
        ];
        // b and c both 0 → lowest index wins (b = idx 1)
        assert_eq!(pick_least_used(&cfg, now), Some(1));
    }

    #[test]
    fn session_key_stable_across_turns_same_conversation() {
        let turn1 = br#"{"system":"You are Claude Code","messages":[{"role":"user","content":"fix the bug"}]}"#;
        let turn2 = br#"{"system":"You are Claude Code","messages":[{"role":"user","content":"fix the bug"},{"role":"assistant","content":"ok"},{"role":"user","content":"now add tests"}]}"#;
        assert_eq!(session_key(turn1), session_key(turn2));
    }

    #[test]
    fn session_key_differs_across_conversations() {
        let a = br#"{"system":"You are Claude Code","messages":[{"role":"user","content":"task A"}]}"#;
        let b = br#"{"system":"You are Claude Code","messages":[{"role":"user","content":"task B"}]}"#;
        assert_ne!(session_key(a), session_key(b));
    }

    #[test]
    fn session_key_handles_content_blocks_and_bad_json() {
        let blocks = br#"{"system":[{"type":"text","text":"sys"}],"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
        assert!(session_key(blocks).is_some());
        assert_eq!(session_key(b"not json"), None);
    }

    #[test]
    fn sticky_prune_drops_expired_entries() {
        let now = 10_000_000;
        let mut map: std::collections::HashMap<u64, (usize, i64)> = std::collections::HashMap::new();
        map.insert(1, (0, now - STICKY_TTL_MS - 1)); // expired
        map.insert(2, (1, now - 1000)); // fresh
        prune_sticky(&mut map, now);
        assert!(!map.contains_key(&1));
        assert!(map.contains_key(&2));
    }

    #[test]
    fn retry_after_parsed_to_ms() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "30".parse().unwrap());
        assert_eq!(retry_after_ms(&h), Some(30_000));
        assert_eq!(retry_after_ms(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn near_quota_account_is_unavailable() {
        let now = 1000;
        let mut a = acct("a", None);
        a.util_5h = Some(0.96); // over 0.95 threshold
        a.reset_5h = Some(now + 60_000); // window still active
        assert!(a.is_near_quota(now, QUOTA_THRESHOLD));
        assert!(!available(&a, now));
    }

    #[test]
    fn stale_quota_is_ignored_after_reset() {
        let now = 1_000_000;
        let mut a = acct("a", None);
        a.util_5h = Some(0.99);
        a.reset_5h = Some(now - 1); // window already reset → refilled
        assert!(!a.is_near_quota(now, QUOTA_THRESHOLD));
        assert!(available(&a, now));
    }

    #[test]
    fn weekly_quota_triggers_switch() {
        let now = 1000;
        let mut a = acct("a", None);
        a.util_7d = Some(0.97);
        a.reset_7d = Some(now + 10_000);
        assert!(a.is_near_quota(now, QUOTA_THRESHOLD));
    }

    #[test]
    fn selection_skips_account_near_quota() {
        let now = 1000;
        let mut cfg = Config::default();
        cfg.accounts = vec![acct("a", None), acct("b", None)];
        cfg.accounts[0].util_5h = Some(0.98);
        cfg.accounts[0].reset_5h = Some(now + 60_000);
        cfg.current = 0;
        // round-robin would prefer 0, but it's near quota → picks 1
        assert_eq!(pick_round_robin(&cfg, now), Some(1));
        // least-used: 0 has fewer usages but is excluded → picks 1
        assert_eq!(pick_least_used(&cfg, now), Some(1));
    }

    #[test]
    fn read_quota_parses_unified_headers() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-5h-utilization", "0.42".parse().unwrap());
        h.insert("anthropic-ratelimit-unified-7d-utilization", "0.10".parse().unwrap());
        h.insert("anthropic-ratelimit-unified-5h-reset", "1700000000".parse().unwrap());
        let mut a = acct("a", None);
        read_quota(&mut a, &h);
        assert_eq!(a.util_5h, Some(0.42));
        assert_eq!(a.util_7d, Some(0.10));
        assert_eq!(a.reset_5h, Some(1_700_000_000_000)); // seconds → ms
        assert_eq!(a.reset_7d, None); // absent header left untouched
    }
}
