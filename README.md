<div align="center">

<h1>claudeN</h1>

**Multi-account Claude OAuth rotating proxy — never hit a rate limit again**

*Log in to several Claude subscriptions · point Claude Code at the proxy · it rotates accounts automatically when one is rate-limited*

</div>

---

## What it does

`clauden` is a tiny, single-binary proxy that sits between Claude Code (or any
Anthropic-compatible client) and `api.anthropic.com`. You log in to multiple
Claude **subscription** accounts via OAuth; the proxy forwards requests using
the active account's token and **transparently switches accounts** the moment
one returns a rate-limit (`429`), credit (`402`), or overloaded (`529`) error.

- **OAuth subscriptions** — stacks your Claude Max/Pro logins (no per-token API billing)
- **Automatic rotation** — cools down a limited account and retries on the next, mid-request
- **Proactive quota awareness** — reads Anthropic's `anthropic-ratelimit-unified-*` headers and switches away from an account at ≥95% utilization *before* it 429s
- **Load-balancing strategies** — `round-robin`, `least-used`, or `session-sticky` (pins a conversation to one account to preserve Anthropic's prompt cache → cheaper + faster)
- **Auto token refresh** — refreshes OAuth tokens ~5 min before expiry
- **True streaming** — SSE responses pipe straight through
- **Single static binary** — no Node, no runtime, ~3.5 MB, ~10 MB RAM

```diagram
  Claude Code ──▶ clauden (:3131) ──▶ api.anthropic.com
                     │ picks active account, injects Bearer token
                     │ on 429/402/529 → cooldown + rotate + retry
                     ▼
              ~/.claudeN/config.json   (accounts + state)
```

## Install

**Requires [Rust](https://rustup.rs) (1.86+).**

### One-liner (recommended)

```bash
cargo install --git https://github.com/skishore23/clauden
```

This builds and installs the `clauden` binary into `~/.cargo/bin` (already on
your PATH if you used rustup).

### Install script

```bash
curl -fsSL https://raw.githubusercontent.com/skishore23/clauden/main/install.sh | bash
```

### From source

```bash
git clone https://github.com/skishore23/clauden
cd clauden
cargo build --release
cp target/release/clauden ~/.local/bin/   # or /usr/local/bin
```

## Update

```bash
clauden update      # rebuilds + reinstalls the latest from GitHub
```

(Equivalent to `cargo install --git https://github.com/skishore23/clauden --force`.)

## Use

```bash
clauden login          # browser OAuth — repeat for each account
clauden list           # show accounts + status
clauden                # run proxy on :3131 and launch Claude Code
```

In one-command mode, clauden keeps the terminal clean for Claude Code's UI and
writes its own logs to `~/.claudeN/clauden.log`. Watch rotation live with:

```bash
tail -f ~/.claudeN/clauden.log
```

Other commands:

| Command | Effect |
|---|---|
| `clauden run --no-launch` | Proxy only; connect a client manually |
| `clauden --port 8080` | Custom port |
| `clauden --verbose` | Log rotations/errors to stderr |
| `clauden use <name|#>` | Manually switch active account (by name or list number) |
| `clauden remove <name|#>` | Remove an account |
| `clauden rename <name|#> <new>` | Rename an account's display name |
| `clauden strategy <name>` | Set strategy: `round-robin` / `least-used` / `session-sticky` |
| `clauden update` | Update to the latest version from GitHub |

To point a client at the proxy manually:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3131
claude
```

## How rotation works

1. Pick the active account (round-robin pointer), skipping any on cooldown.
2. Refresh its OAuth token if it expires within 5 minutes.
3. Forward the request with `Authorization: Bearer <token>`.
4. If the upstream returns `429` / `402` / `529`, cool that account down
   (honoring `retry-after` when present), advance to the next account, and retry.
5. If every account is cooling down, return `429` with a try-again hint.

## Config

Stored at `~/.claudeN/config.json` (created on first login, `0600` perms):

```json
{
  "port": 3131,
  "current": 0,
  "accounts": [
    {
      "name": "you@example.com",
      "tier": "max",
      "access_token": "sk-ant-oat01-...",
      "refresh_token": "sk-ant-ort01-...",
      "expires_at": 1774384968427,
      "cooldown_until": null
    }
  ]
}
```

## Docs

- [docs/USAGE.md](docs/USAGE.md) — full install/login/run walkthrough + troubleshooting
- [docs/ROADMAP.md](docs/ROADMAP.md) — feature ideas grouped by effort vs. impact

## Development

```bash
cargo test          # unit tests (rotation/quota/strategies) + integration tests
cargo clippy --all-targets
cargo build --release
```

Tests:
- **Unit** (`src/`) — selection strategies, quota math, session keys, header parsing
- **Integration** (`tests/integration.rs`) — a mock Anthropic upstream + the real
  proxy router, asserting 429 rotation, proactive quota skipping, bearer
  injection, header passthrough, and session stickiness end-to-end

## License

MIT
