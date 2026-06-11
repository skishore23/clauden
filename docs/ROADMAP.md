# clauden — Feature Ideas & Roadmap

Where clauden could go next. Grouped by effort vs. impact. The current build
already does OAuth login, round-robin rotation on `429/402/529`, token refresh,
streaming passthrough, and a single-binary CLI.

---

## Quick wins (small, high value)

### 1. Proactive quota-aware rotation — ✅ DONE
The proxy reads `anthropic-ratelimit-unified-5h-utilization` /
`-7d-utilization` (and their `-reset` timestamps) off every response and stores
per-account utilization. An account at **≥95%** of either window is treated as
unavailable, so all strategies switch away *before* a hard `429`. Stale
utilization auto-expires at the window reset. `clauden list` shows live
utilization + a `near-quota` state.

**Possible follow-ups:** make the 95% threshold configurable; also parse the
standard `anthropic-ratelimit-tokens/requests-*` headers for API-key accounts
(pairs with #9); surface `unified-status` (`allowed_warning`) as an early hint.

### 2. Mid-stream SSE error rotation
A `429` can arrive *inside* an SSE stream as an `error` event, not just as an
HTTP status. Scan streamed `data:` lines; on an exhaustion error, close with a
`429` so the client retries (and the proxy serves the next account). The
reference implementations all do this.
*Impact: medium. Effort: medium — inspect the stream as it passes through.*

### 3. Import existing Claude Code credentials
Read `~/.claude/.credentials.json` (`claudeAiOauth`) and import that account
without a browser login. One command: `clauden import`.
*Impact: medium. Effort: low.*

### 4. `clauden status` JSON / health endpoint
Expose `GET /clauden/status` on the proxy returning active account, cooldown
timers, and quota — for scripting and dashboards.
*Impact: low–medium. Effort: low.*

### 5. Retry/backoff on transient 5xx + network errors
Currently only `429/402/529` rotate; a `500`/connection-reset is passed straight
through. Add a couple of retries with backoff (optionally on the same account)
before giving up.
*Impact: medium. Effort: low.*

---

## Bigger features

### 6. Priority accounts + auto-fallback
Let accounts have a priority. Always prefer your highest-tier (Max) account;
fall back to others only when it's cooling down, and **automatically return** to
the preferred one once its window resets. Better than flat round-robin for
people with one main account + backups.
*Impact: high. Effort: medium.*

### 7. Load-balancing strategies — ✅ DONE
Selection strategy is now configurable via `clauden strategy`:
- `round-robin` — prefer active account, advance on rotation
- `least-used` — pick the account with the lowest `usage_count`
- `session-sticky` — pin a conversation (keyed by system prompt + first user
  message) to one account to preserve Anthropic's prompt cache; falls back to
  least-used for new conversations or when the pinned account is cooling down

**Follow-up — token-based usage accounting:** `least-used` currently counts
*requests*. Switch it to count *tokens* (parse `usage.input_tokens` /
`output_tokens` from responses, and cached-token fields) so balancing reflects
real consumption, and surface a per-account token/cost breakdown. Pairs well
with #8 (analytics).

### 8. Usage analytics (optional sqlite)
Log per-request token counts, latency, account used, and status. Add
`clauden stats` for a breakdown by account/day. This is where `bun:sqlite` in
wink maps cleanly to `rusqlite` here. Keep it opt-in so the default stays
zero-config.
*Impact: medium. Effort: medium.*

### 9. API-key accounts (mixed pool)
Support console `sk-ant-api03-…` keys alongside OAuth subscriptions (inject via
`x-api-key` instead of `Bearer`). Lets you mix pay-as-you-go keys with
subscriptions in one rotation pool.
*Impact: medium. Effort: low–medium.*

### 10. Remote / VPS mode with proxy auth
Add an optional client API key (`clauden generate-key`) so the proxy can run on
a trusted server and be reached from other machines without exposing your OAuth
tokens. Bind to `0.0.0.0` only when a key is set.
*Impact: medium. Effort: medium — auth middleware + key storage.*

### 11. Hot-reload accounts
Re-read `config.json` on change (or a signal) so `clauden login` in another
terminal takes effect without restarting the running proxy.
*Impact: low–medium. Effort: low.*

---

## Nice-to-have / polish

### 12. Live TUI dashboard
A terminal UI showing per-account quota bars, reset countdowns, active account,
and a request log — like teamclaude's TUI. Run when attached to a TTY, fall back
to plain logs otherwise.
*Impact: medium (delightful). Effort: high — `ratatui`.*

### 13. Token encryption at rest / OS keychain
Store OAuth tokens in the macOS Keychain / Linux secret service instead of a
plaintext JSON file (file is `0600` today, which is decent but not encrypted).
*Impact: medium (security). Effort: medium.*

### 14. Background service (launchd / systemd)
`clauden install-service` to run the proxy as a login daemon so it's always up.
*Impact: low–medium. Effort: low–medium.*

### 15. Multi-provider routing
Beyond Anthropic: route to OpenRouter / Bedrock / Vertex / z.ai as additional
"accounts," translating where needed. This turns clauden from a Claude rotator
into a general resilient gateway (cf. better-ccflare). Big scope — only if you
want that product.
*Impact: high (broadens audience). Effort: high.*

### 16. Docker image + CI
Ship a `Dockerfile` and a GitHub Actions matrix (build/test/release binaries for
macOS/Linux). Mirrors wink's CI-and-publish setup.
*Impact: medium (distribution). Effort: low–medium.*

### 17. Integration tests against a mock upstream — ✅ DONE
`tests/integration.rs` spins up a mock Anthropic server (axum) and drives the
real proxy router (`server::router` + `make_state`, with an injectable upstream).
Covered end-to-end: 429 rotation + cooldown, all-accounts-exhausted → 429,
proactive quota skipping from `unified-5h-utilization` headers, bearer-token
injection, `anthropic-version` passthrough, and session-sticky pinning. The
crate is now lib + bin so tests can import internals.

---

## Suggested next 3

Done so far: **load-balancing strategies (#7)**, **proactive quota-aware
rotation (#1)**, and **integration tests (#17)**. Recommended next, in order —
highest value per unit effort:

1. **Priority + auto-fallback (#6)** — matches how most people actually use a
   main + backup accounts.
2. **Mid-stream SSE error rotation (#2)** — close the one remaining rotation
   gap (limits that arrive inside a stream).
3. **Import existing Claude Code creds (#3)** — frictionless onboarding without
   a browser login.
