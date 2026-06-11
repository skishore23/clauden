//! Interactive OAuth login: opens the browser, captures the callback on a
//! local port, exchanges the code, fetches the profile, and saves the account.

use anyhow::{anyhow, Context, Result};
use std::sync::mpsc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::{Account, Config};
use crate::oauth;
use crate::ui;

/// Run the full login flow and persist the new account into `cfg`.
///
/// `fresh` forces the login/workspace chooser (`prompt=login`) so you can pick a
/// different workspace for an account you're already signed into.
pub async fn run(cfg: &mut Config, fresh: bool) -> Result<()> {
    let pkce = oauth::generate_pkce();

    // Bind a local callback server on an ephemeral port.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding local callback server")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/callback");

    let auth_url = oauth::authorize_url(&pkce.challenge, &pkce.state, &redirect_uri, fresh);

    println!(
        "\n  {}  {}",
        ui::magenta("(• ◡ -)"),
        ui::bold("Opening your browser to log in to Claude…")
    );
    if fresh {
        println!(
            "  {}",
            ui::dim("(--fresh) forcing the login/workspace chooser. Tip: a private/incognito window guarantees you can pick a different workspace.")
        );
    }
    println!(
        "  {} {}\n",
        ui::dim("If it doesn't open, paste this URL:"),
        ui::dim(&auth_url)
    );
    let _ = webbrowser::open(&auth_url);
    println!("  {} waiting for you to approve in the browser…", ui::cyan("⠿"));

    // Wait for the browser redirect carrying ?code=…&state=…
    let (code, returned_state) = wait_for_callback(listener).await?;
    if returned_state != pkce.state {
        return Err(anyhow!("OAuth state mismatch — aborting for safety"));
    }

    let client = reqwest::Client::new();
    let tokens = oauth::exchange_code(&client, &code, &pkce.state, &pkce.verifier, &redirect_uri)
        .await
        .context("exchanging authorization code")?;

    let profile = oauth::fetch_profile(&client, &tokens.access_token)
        .await
        .unwrap_or(oauth::Profile {
            uuid: None,
            email: None,
            tier: None,
            org_uuid: None,
            org_name: None,
        });

    let email = profile
        .email
        .clone()
        .unwrap_or_else(|| format!("account-{}", cfg.accounts.len() + 1));

    // Same account *and* same org → update in place (refresh tokens). Different
    // org under the same email → a distinct account.
    //
    // Both org tokens for one user share the same account.uuid, so org identity
    // must distinguish them: prefer org_uuid, fall back to org_name when the
    // profile doesn't return a uuid (otherwise two orgs would collapse).
    let existing = cfg.accounts.iter().position(|a| {
        let same_account = a.account_uuid.is_some() && a.account_uuid == profile.uuid;
        let same_org = match (&a.org_uuid, &profile.org_uuid) {
            (Some(x), Some(y)) => x == y,
            _ => a.org_name == profile.org_name,
        };
        same_account && same_org
    });

    // Pick a unique, human display name.
    let name = match &existing {
        Some(i) => cfg.accounts[*i].name.clone(),
        None => unique_name(cfg, &email),
    };

    let account = Account {
        name: name.clone(),
        account_uuid: profile.uuid.clone(),
        tier: profile.tier.clone(),
        org_uuid: profile.org_uuid.clone(),
        org_name: profile.org_name.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
        cooldown_until: None,
        usage_count: 0,
        util_5h: None,
        util_7d: None,
        reset_5h: None,
        reset_7d: None,
    };

    let updated = existing.is_some();
    match existing {
        Some(i) => cfg.accounts[i] = account,
        None => cfg.accounts.push(account),
    }

    let tier = profile.tier.unwrap_or_else(|| "unknown".into());
    let verb = if updated { "Updated" } else { "Added" };
    let org_label = profile
        .org_name
        .clone()
        .map(|o| format!(" · org: {o}"))
        .unwrap_or_default();
    println!(
        "\n  {} {} {} {}\n",
        ui::green("✓"),
        verb,
        ui::bold(&name),
        ui::dim(&format!("({tier}{org_label})"))
    );
    if updated {
        println!(
            "  {}\n",
            ui::dim("(this account+workspace already existed — tokens refreshed. To add a *different* workspace, use `clauden login --fresh` and pick the other one.)")
        );
    }
    Ok(())
}

/// Build a display name that's unique among existing accounts. Prefers the bare
/// email; on collision (e.g. a second workspace for the same email) appends a
/// short numeric suffix. The workspace itself is shown in the Org column.
fn unique_name(cfg: &Config, email: &str) -> String {
    let taken = |n: &str| cfg.accounts.iter().any(|a| a.name == n);

    if !taken(email) {
        return email.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{email} #{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Accept a single connection and parse the OAuth code/state from the request line.
async fn wait_for_callback(listener: TcpListener) -> Result<(String, String)> {
    // Run the blocking-ish accept in this async fn with a timeout.
    let (result_tx, result_rx) = mpsc::channel();

    let accept = async {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]).to_string();

        let (code, state) = parse_callback(&request)
            .ok_or_else(|| anyhow!("callback missing code/state"))?;

        // Respond to the browser, then redirect to the official success page.
        // Declare UTF-8 both in the header and via <meta> — without it a browser
        // that defaults to windows-1252 renders the ✓ as mojibake ("âœ").
        let body = "<html><head><meta charset=\"utf-8\"></head>\
            <body style=\"font-family:sans-serif;text-align:center;margin-top:4rem\">\
            <h2>✓ Logged in</h2><p>You can close this tab and return to your terminal.</p>\
            <script>setTimeout(function(){location.href='https://platform.claude.com/oauth/code/success?app=claude-code'},1200)</script>\
            </body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;

        let _ = result_tx.send((code, state));
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        res = accept => {
            res?;
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            return Err(anyhow!("login timed out after 5 minutes"));
        }
    }

    result_rx
        .recv()
        .map_err(|_| anyhow!("failed to receive OAuth callback"))
}

/// Extract `code` and `state` query params from the raw HTTP request line.
fn parse_callback(request: &str) -> Option<(String, String)> {
    let first_line = request.lines().next()?;
    // e.g. "GET /callback?code=abc&state=xyz HTTP/1.1"
    let path = first_line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;

    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let decoded = urldecode(v);
            match k {
                "code" => code = Some(decoded),
                "state" => state = Some(decoded),
                _ => {}
            }
        }
    }
    Some((code?, state?))
}

/// Minimal percent-decoding for OAuth callback values.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = hex_val(bytes[i + 1]);
                let l = hex_val(bytes[i + 2]);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
