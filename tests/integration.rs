//! End-to-end integration tests: a mock Anthropic upstream + the real proxy
//! router, asserting rotation, proactive quota skipping, auth injection, and
//! header passthrough.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, extract::State, response::Response, routing::any, Router};
use tokio::net::TcpListener;

use clauden::config::{Account, Config, Strategy};

// ----------------------------------------------------------------------------
// Mock upstream
// ----------------------------------------------------------------------------

/// Per-token response rule: (status code, optional 5h utilization to report).
type Rule = (u16, Option<f64>);
/// Ordered log of (bearer token, anthropic-version header) per request.
type CallLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

#[derive(Clone)]
struct MockState {
    rules: Arc<Mutex<HashMap<String, Rule>>>,
    calls: CallLog,
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

async fn mock_handler(State(s): State<MockState>, req: Request) -> Response {
    let headers = req.headers();
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let version = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    s.calls.lock().unwrap().push((token.clone(), version));

    let (status, util) = s
        .rules
        .lock()
        .unwrap()
        .get(&token)
        .copied()
        .unwrap_or((200, None));

    let mut builder = Response::builder().status(status);
    if let Some(u) = util {
        builder = builder
            .header("anthropic-ratelimit-unified-5h-utilization", u.to_string())
            .header(
                "anthropic-ratelimit-unified-5h-reset",
                (now_secs() + 3600).to_string(),
            );
    }
    builder
        .header("content-type", "application/json")
        .body(Body::from(r#"{"ok":true}"#))
        .unwrap()
}

async fn start_mock(rules: &[(&str, Rule)]) -> (String, MockState) {
    let state = MockState {
        rules: Arc::new(Mutex::new(
            rules.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        )),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new().fallback(any(mock_handler)).with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

// ----------------------------------------------------------------------------
// Proxy under test
// ----------------------------------------------------------------------------

async fn start_proxy(cfg: Config, upstream: String) -> (String, Arc<tokio::sync::Mutex<Config>>) {
    let state = clauden::server::make_state(cfg, upstream, false);
    let cfg_handle = state.cfg.clone();
    let app = clauden::server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), cfg_handle)
}

fn account(name: &str, token: &str) -> Account {
    Account {
        name: name.into(),
        account_uuid: None,
        tier: Some("max".into()),
        access_token: token.into(),
        refresh_token: "refresh".into(),
        expires_at: 32_503_680_000_000, // year ~3000: never refresh during tests
        cooldown_until: None,
        usage_count: 0,
        util_5h: None,
        util_7d: None,
        reset_5h: None,
        reset_7d: None,
    }
}

fn cfg_with(accounts: Vec<Account>, strategy: Strategy) -> Config {
    Config {
        accounts,
        strategy,
        ..Config::default()
    }
}

async fn post_messages(base: &str, version: Option<&str>) -> reqwest::Response {
    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("{base}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#);
    if let Some(v) = version {
        req = req.header("anthropic-version", v);
    }
    req.send().await.unwrap()
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[tokio::test]
async fn rotates_on_429_and_cools_down_account() {
    let (upstream, mock) = start_mock(&[("tok-A", (429, None)), ("tok-B", (200, None))]).await;
    let cfg = cfg_with(
        vec![account("A", "tok-A"), account("B", "tok-B")],
        Strategy::RoundRobin,
    );
    let (proxy, cfg_handle) = start_proxy(cfg, upstream).await;

    let resp = post_messages(&proxy, None).await;
    assert_eq!(resp.status(), 200, "client should see success after rotation");

    // Upstream saw A (429) then B (200) — proxy retried transparently.
    let tokens: Vec<String> = {
        let calls = mock.calls.lock().unwrap();
        calls.iter().map(|(t, _)| t.clone()).collect()
    };
    assert_eq!(tokens, vec!["tok-A", "tok-B"]);

    let cfg = cfg_handle.lock().await;
    assert!(cfg.accounts[0].cooldown_until.is_some(), "A should be cooling down");
    assert_eq!(cfg.accounts[1].usage_count, 1, "B served one request");
    assert_eq!(cfg.current, 1, "pointer advanced to B");
}

#[tokio::test]
async fn all_accounts_limited_returns_429() {
    let (upstream, mock) = start_mock(&[("tok-A", (429, None)), ("tok-B", (429, None))]).await;
    let cfg = cfg_with(
        vec![account("A", "tok-A"), account("B", "tok-B")],
        Strategy::RoundRobin,
    );
    let (proxy, cfg_handle) = start_proxy(cfg, upstream).await;

    let resp = post_messages(&proxy, None).await;
    assert_eq!(resp.status(), 429, "all exhausted → 429");
    let body = resp.text().await.unwrap();
    assert!(body.contains("clauden_proxy"), "proxy-generated error; got: {body}");
    assert!(body.contains("exhausted"), "got: {body}");

    // Each account tried exactly once this request, and both ended up cooling.
    let call_count = { mock.calls.lock().unwrap().len() };
    assert_eq!(call_count, 2);

    let cfg = cfg_handle.lock().await;
    assert!(cfg.accounts[0].cooldown_until.is_some(), "A cooled down");
    assert!(cfg.accounts[1].cooldown_until.is_some(), "B cooled down");
}

#[tokio::test]
async fn proactively_skips_account_over_quota() {
    // A succeeds but reports 99% utilization; next request must avoid A.
    let (upstream, mock) =
        start_mock(&[("tok-A", (200, Some(0.99))), ("tok-B", (200, None))]).await;
    let cfg = cfg_with(
        vec![account("A", "tok-A"), account("B", "tok-B")],
        Strategy::RoundRobin,
    );
    let (proxy, cfg_handle) = start_proxy(cfg, upstream).await;

    // First request → served by A, records 0.99 utilization.
    let r1 = post_messages(&proxy, None).await;
    assert_eq!(r1.status(), 200);
    {
        let cfg = cfg_handle.lock().await;
        assert_eq!(cfg.accounts[0].util_5h, Some(0.99), "quota header recorded");
    }

    // Second request → A is near-quota, must be skipped in favor of B.
    let r2 = post_messages(&proxy, None).await;
    assert_eq!(r2.status(), 200);

    let calls = mock.calls.lock().unwrap();
    let tokens: Vec<&str> = calls.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(tokens, vec!["tok-A", "tok-B"], "2nd request avoided over-quota A");
}

#[tokio::test]
async fn injects_bearer_token_and_forwards_headers() {
    let (upstream, mock) = start_mock(&[("tok-A", (200, None))]).await;
    let cfg = cfg_with(vec![account("A", "tok-A")], Strategy::RoundRobin);
    let (proxy, _cfg) = start_proxy(cfg, upstream).await;

    let resp = post_messages(&proxy, Some("2023-06-01")).await;
    assert_eq!(resp.status(), 200);

    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (token, version) = &calls[0];
    assert_eq!(token, "tok-A", "proxy injected the account's bearer token");
    assert_eq!(
        version.as_deref(),
        Some("2023-06-01"),
        "client anthropic-version forwarded upstream"
    );
}

#[tokio::test]
async fn session_sticky_pins_conversation_to_one_account() {
    let (upstream, mock) = start_mock(&[("tok-A", (200, None)), ("tok-B", (200, None))]).await;
    let cfg = cfg_with(
        vec![account("A", "tok-A"), account("B", "tok-B")],
        Strategy::SessionSticky,
    );
    let (proxy, _cfg) = start_proxy(cfg, upstream).await;

    // Same conversation body twice → both turns must hit the same account.
    let _ = post_messages(&proxy, None).await;
    let _ = post_messages(&proxy, None).await;

    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, calls[1].0, "both turns pinned to same account");
}
