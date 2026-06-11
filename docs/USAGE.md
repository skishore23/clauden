# clauden — Usage Guide

A step-by-step guide to installing, logging in, running, and troubleshooting the
multi-account Claude OAuth rotating proxy.

---

## 1. Install

You need Rust (1.86+) to build. The result is a single static binary with no
runtime dependencies.

```bash
git clone <your-repo> clauden
cd clauden
cargo build --release
```

The binary is at `target/release/clauden`. Put it on your `PATH`:

```bash
# macOS / Linux
sudo cp target/release/clauden /usr/local/bin/
# or, no sudo:
mkdir -p ~/.local/bin && cp target/release/clauden ~/.local/bin/
```

Verify:

```bash
clauden --help
```

---

## 2. Log in to your Claude accounts

Run `login` once per account. Each run opens your browser to Claude's OAuth
page; approve, and the account is saved.

```bash
clauden login        # account #1 — log in as your first Claude user
clauden login        # account #2 — log out / use a different Claude login
clauden login        # account #3 …
```

> **Tip:** To add a *different* account, make sure you're logged into that
> account in your browser (or use a private/incognito window) before running
> `clauden login`. The proxy stores whichever account you authorize.

Accounts are deduped by email/UUID, so re-logging into the same account just
refreshes its tokens instead of creating a duplicate.

Check what you have:

```bash
clauden list
```

```
  Accounts (3 total):

  ▶ you@work.com                     max    ready
    you@personal.com                 pro    ready
    side@project.com                 max    cooldown 47s
```

`▶` marks the currently-active account. `cooldown Ns` means that account is
rate-limited and will be skipped until the timer expires.

---

## 3. Run the proxy

### The easy way (auto-launch Claude Code)

```bash
clauden
```

This starts the proxy on `:3131` and launches Claude Code already pointed at it.
Use Claude Code normally — when an account hits a limit, clauden silently
switches to the next one.

### Proxy only (connect any client yourself)

```bash
clauden run --no-launch
```

Then point any Anthropic-compatible client at it:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3131
export ANTHROPIC_API_KEY=clauden-proxy   # placeholder; the proxy injects the real token
claude
```

### Options

| Flag | Effect |
|------|--------|
| `--port <N>` | Listen on a different port (default `3131`) |
| `--no-launch` | Don't spawn Claude Code; just run the proxy |
| `--verbose` | Print rotations, refreshes, and errors to stderr |

Example:

```bash
clauden run --no-launch --port 8080 --verbose
```

---

## 4. Managing accounts

```bash
clauden list                 # show all accounts + status (with a # index column)
clauden use 2                # select by the number shown in `list`
clauden use you@personal.com # …or by exact name
clauden remove 2             # remove by number or name
```

`clauden use` is handy if you want to pin traffic to one account temporarily —
the proxy still rotates away from it on a rate limit.

### Multiple accounts under the same email

If one Claude login belongs to several organizations, you can add each org as a
separate, rotatable account — log in once per org. clauden keeps them distinct
by organization and disambiguates the display name, e.g.:

```
  1  you@example.com
  2  you@example.com (Acme Inc)
```

Select either by its number (`clauden use 1`) or full name. Logging in again
with the same account *and* org just refreshes its tokens in place.

**If the browser skips the workspace chooser:** Claude scopes the token to
whichever workspace is *active* in your `claude.ai` browser session, and an
existing session cookie makes the authorize page skip the chooser. To add a
*different* workspace for the same account:

```bash
clauden login --fresh    # forces the login/workspace chooser (prompt=login)
```

If it still lands on the same workspace, either switch the active workspace in
`claude.ai` first (account menu → workspace switcher) and run `clauden login`,
or paste the printed auth URL into a **private/incognito window** and pick the
workspace there.

---

## 5. Choosing a load-balancing strategy

The proxy can pick the serving account three ways:

```bash
clauden strategy                  # show current strategy
clauden strategy least-used       # spread load evenly
clauden strategy session-sticky   # preserve prompt cache (recommended)
clauden strategy round-robin      # simple, predictable (default)
```

| Strategy | Behavior | Best for |
|----------|----------|----------|
| `round-robin` | Prefer the active account, advance on rotation | Simple, predictable |
| `least-used` | Pick the account that has served the fewest requests | Spreading load evenly across many accounts |
| `session-sticky` | **Pin each conversation to one account** so Anthropic's prompt cache keeps hitting (cheaper + faster); falls back to least-used for new conversations or when the pinned account is cooling down | Day-to-day coding with multi-turn chats |

**Why session-sticky saves money:** Anthropic caches the prompt *prefix*
(system prompt + early messages). If every turn of a conversation goes to the
*same* account, those turns reuse the cache — you pay the cheaper cached-input
rate and get faster responses. Bouncing turns across accounts would re-pay full
price each time. clauden derives a stable key from the system prompt + first
user message so all turns of a conversation route to the same account.

All strategies still **rotate on rate limits** — stickiness/least-used only
choose among accounts that are currently available.

`clauden list` shows the active strategy and each account's request count:

```
  Strategy: session-sticky
  Accounts (2 total):

  ▶ you@work.com                     max    ready          128 reqs
    you@personal.com                 pro    ready          37 reqs
```

---

## 6. How rotation behaves

The proxy switches accounts two ways — *proactively* (before a limit) and
*reactively* (after one):

1. The proxy selects an **available** account — one that is neither cooling down
   nor near its quota — using your chosen strategy.
2. If the token is within 5 minutes of expiry, it's **refreshed** first.
3. The request is forwarded with that account's OAuth token.
4. From each response, the proxy reads Anthropic's
   `anthropic-ratelimit-unified-5h-utilization` / `-7d-utilization` headers.
   Once an account reaches **≥95%** of either its 5-hour or 7-day window, it's
   marked **near-quota** and the *next* request transparently uses a different
   account — so you switch *before* ever hitting a hard limit.
5. If the upstream still returns:
   - `429` (rate limit), `402` (out of credit), or `529` (overloaded) →
     the account is put on **cooldown** (honoring the `retry-after` header if
     present, else 60s), the proxy advances to the **next available account**,
     and **retries the same request** — your client never sees the error.
   - any other status (200, 400, 401, 500, …) → streamed straight back to the
     client unchanged (no rotation).
6. Quota utilization auto-expires at the window's reset time, so an account
   becomes available again on its own.
7. If **every** account is on cooldown or at quota, the proxy returns `429`
   with a "try again shortly" message.

`clauden list` shows each account's peak utilization and whether it's
`ready` / `near-quota` / `cooldown`:

```
  ▶ you@work.com      max    near-quota   quota  97%   120 reqs
    you@personal.com  pro    ready        quota  42%    30 reqs
```

---

## 7. Where state lives

Everything is in `~/.claudeN/config.json` (created on first login, `0600`
permissions so only you can read it):

```json
{
  "port": 3131,
  "current": 0,
  "accounts": [
    {
      "name": "you@work.com",
      "tier": "max",
      "access_token": "sk-ant-oat01-...",
      "refresh_token": "sk-ant-ort01-...",
      "expires_at": 1774384968427,
      "cooldown_until": null
    }
  ]
}
```

You can hand-edit it (e.g. to change the default port) while the proxy is
stopped. **Don't commit this file** — it contains live OAuth tokens.

---

## 8. Troubleshooting

**`No accounts configured`**
Run `clauden login` at least once.

**Browser didn't open during login**
Copy the printed URL into your browser manually. The terminal waits up to 5
minutes for the callback.

**`401 Invalid bearer token` coming back through the proxy**
The account's token is invalid/expired and refresh failed. Re-run
`clauden login` for that account.

**All requests return `429` immediately**
Every account is on cooldown. Run `clauden list` to see the timers; wait, or
add another account with `clauden login`.

**See what the proxy is doing**
Run with `--verbose` to log every rotation, refresh, and upstream error:

```
[clauden] ⚡ you@work.com hit 429; cooling 60s and rotating
```

**Port already in use**
Another process is on `:3131`. Use `--port` to pick a free one.

**"Both claude.ai and ANTHROPIC_API_KEY set · auth may not work as expected"**
This warning comes from *Claude Code*, not clauden. clauden launches Claude Code
with `ANTHROPIC_API_KEY=clauden-proxy` (a placeholder that routes through the
proxy), while Claude Code also sees your own `claude.ai` login — two credentials
at once. clauden still works (the proxy replaces whatever Claude Code sends with
the real OAuth token), but to clear the warning, run `/logout` **inside Claude
Code** so it stops using its own login, then approve the API key on next launch.
This only clears Claude Code's creds in `~/.claude/.credentials.json`; your
clauden accounts in `~/.claudeN/config.json` are untouched.
