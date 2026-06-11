//! Persistent config: accounts + runtime state, stored at ~/.claudeN/config.json.
//!
//! Mirrors wink's local-dir convention (`.wink/`) — here it's `~/.claudeN/`.
//! A handful of accounts means JSON is simpler to run than a database.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 3131;

/// How the proxy chooses which account serves a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Prefer the `current` pointer, advancing on rotation. Predictable.
    #[default]
    RoundRobin,
    /// Pick the account that has served the fewest requests. Spreads load.
    LeastUsed,
    /// Pin a conversation to one account to preserve Anthropic prompt cache
    /// (cache-hit = cheaper + faster). Falls back to least-used for new
    /// conversations or when the pinned account is cooling down.
    SessionSticky,
}

impl Strategy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "round-robin" | "rr" => Some(Self::RoundRobin),
            "least-used" | "least" | "lu" => Some(Self::LeastUsed),
            "session-sticky" | "sticky" | "ss" => Some(Self::SessionSticky),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::RoundRobin => "round-robin",
            Self::LeastUsed => "least-used",
            Self::SessionSticky => "session-sticky",
        }
    }
}

/// One Claude subscription account authenticated via OAuth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// Human label — usually the account email (disambiguated by org if needed).
    pub name: String,
    #[serde(default)]
    pub account_uuid: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    /// Organization this token is scoped to (lets the same email be added for
    /// multiple Claude orgs as distinct, rotatable accounts).
    #[serde(default)]
    pub org_uuid: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,

    /// OAuth credentials. `access_token` is `sk-ant-oat01-…`.
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch milliseconds when `access_token` expires.
    pub expires_at: i64,

    /// Runtime rotation state: epoch-ms until which this account is on cooldown
    /// after a rate-limit/quota error. `None` means available.
    #[serde(default)]
    pub cooldown_until: Option<i64>,

    /// Number of requests served by this account (drives `least-used`).
    #[serde(default)]
    pub usage_count: u64,

    /// Unified quota utilization (0.0–1.0) for the 5-hour session window,
    /// from `anthropic-ratelimit-unified-5h-utilization`.
    #[serde(default)]
    pub util_5h: Option<f64>,
    /// Utilization for the 7-day weekly window.
    #[serde(default)]
    pub util_7d: Option<f64>,
    /// Epoch-ms when the 5h window resets (utilization becomes stale after).
    #[serde(default)]
    pub reset_5h: Option<i64>,
    /// Epoch-ms when the 7d window resets.
    #[serde(default)]
    pub reset_7d: Option<i64>,
}

impl Account {
    /// True if the account is currently rate-limited (cooling down).
    pub fn is_cooling_down(&self, now_ms: i64) -> bool {
        matches!(self.cooldown_until, Some(until) if until > now_ms)
    }

    /// True if either unified quota window is at/over `threshold` and hasn't
    /// reset yet — i.e. proactively switch away before a hard 429.
    pub fn is_near_quota(&self, now_ms: i64, threshold: f64) -> bool {
        Self::metric_near(self.util_5h, self.reset_5h, now_ms, threshold)
            || Self::metric_near(self.util_7d, self.reset_7d, now_ms, threshold)
    }

    fn metric_near(util: Option<f64>, reset: Option<i64>, now_ms: i64, threshold: f64) -> bool {
        match util {
            Some(u) => {
                // A window past its reset time has refilled — ignore stale value.
                if let Some(r) = reset {
                    if now_ms >= r {
                        return false;
                    }
                }
                u >= threshold
            }
            None => false,
        }
    }

    /// Highest current (non-stale) utilization, for display.
    pub fn peak_utilization(&self, now_ms: i64) -> Option<f64> {
        let live = |u: Option<f64>, r: Option<i64>| -> Option<f64> {
            let u = u?;
            if let Some(r) = r {
                if now_ms >= r {
                    return None;
                }
            }
            Some(u)
        };
        match (live(self.util_5h, self.reset_5h), live(self.util_7d, self.reset_7d)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Index of the currently-preferred account (round-robin pointer).
    #[serde(default)]
    pub current: usize,
    /// Account-selection strategy.
    #[serde(default)]
    pub strategy: Strategy,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            accounts: Vec::new(),
            current: 0,
            strategy: Strategy::default(),
        }
    }
}

impl Config {
    pub fn dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("could not resolve home directory")?;
        Ok(home.join(".claudeN"))
    }

    pub fn path() -> Result<PathBuf> {
        Ok(Self::dir()?.join("config.json"))
    }

    /// Load config from disk, or return defaults if it doesn't exist yet.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Persist config to disk (best-effort directory creation, 0600 perms).
    pub fn save(&self) -> Result<()> {
        let dir = Self::dir()?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = Self::path()?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)
            .with_context(|| format!("writing {}", path.display()))?;
        restrict_permissions(&path);
        Ok(())
    }

    pub fn find_account(&self, name: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.name == name)
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}
