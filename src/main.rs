//! clauden — multi-account Claude OAuth rotating proxy.
//!
//! Log in to several Claude subscriptions, point Claude Code at the proxy, and
//! it transparently rotates accounts whenever one hits a rate limit.

use anyhow::Result;
use clap::{Parser, Subcommand};

use clauden::config::{Config, Strategy};
use clauden::ui;
use clauden::{login, server};

#[derive(Parser)]
#[command(
    name = "clauden",
    about = "Multi-account Claude OAuth rotating proxy — never hit a rate limit again",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Port to listen on (overrides config).
    #[arg(long, global = true)]
    port: Option<u16>,

    /// Don't auto-launch Claude Code; just run the proxy.
    #[arg(long, global = true)]
    no_launch: bool,

    /// Verbose request logging to stderr.
    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Add a Claude account via browser OAuth login.
    Login {
        /// Force the login/workspace chooser (use to add a different workspace
        /// for an account you're already signed into).
        #[arg(long)]
        fresh: bool,
    },
    /// List configured accounts and their status.
    List,
    /// Show proxy + account status.
    Status,
    /// Switch the active account by name.
    Use { name: String },
    /// Remove an account by name.
    Remove { name: String },
    /// Get or set the account-selection strategy.
    ///
    /// One of: round-robin, least-used, session-sticky. Omit to show current.
    Strategy { name: Option<String> },
    /// Update clauden to the latest version from GitHub.
    Update,
    /// Run the proxy (default if no command given).
    Run,
}

const REPO_URL: &str = "https://github.com/skishore23/clauden";

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        std::env::set_var("CLAUDEN_VERBOSE", "1");
    }

    match cli.command {
        Some(Command::Login { fresh }) => cmd_login(fresh).await,
        Some(Command::List) | Some(Command::Status) => cmd_list(),
        Some(Command::Use { name }) => cmd_use(&name),
        Some(Command::Remove { name }) => cmd_remove(&name),
        Some(Command::Strategy { name }) => cmd_strategy(name),
        Some(Command::Update) => cmd_update(),
        Some(Command::Run) | None => cmd_run(cli.port, cli.no_launch).await,
    }
}

async fn cmd_login(fresh: bool) -> Result<()> {
    let mut cfg = Config::load()?;
    login::run(&mut cfg, fresh).await?;
    cfg.save()?;
    println!(
        "  {} {} account(s) configured. Run {} to see them.\n",
        ui::dim("›"),
        ui::bold(&cfg.accounts.len().to_string()),
        ui::bold("clauden list")
    );
    Ok(())
}

fn cmd_list() -> Result<()> {
    let cfg = Config::load()?;

    // Header banner.
    println!();
    println!(
        "  {}  {}      {} {}",
        ui::magenta("(• ◡ -)"),
        ui::bold("clauden"),
        ui::dim("strategy:"),
        ui::cyan(cfg.strategy.label())
    );
    println!();

    if cfg.accounts.is_empty() {
        println!(
            "  {} No accounts yet. Run {} to add one.\n",
            ui::yellow("›"),
            ui::bold("clauden login")
        );
        return Ok(());
    }

    let now = now_ms();

    // Compute the account-name column width (cap to keep it tidy).
    let name_w = cfg
        .accounts
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(7)
        .clamp(7, 32);

    // Org column width — sized to content, capped; min fits the "Org" header.
    let org_w = cfg
        .accounts
        .iter()
        .filter_map(|a| a.org_name.as_ref())
        .map(|o| o.chars().count())
        .max()
        .unwrap_or(3)
        .clamp(3, 24);

    // Column widths (visible).
    let (w_idx, w_tier, w_status, w_quota, w_reqs) = (3, 4, 14, 16, 6);

    let line = |l: &str, m: &str, r: &str| {
        format!(
            "{l}{}{m}{}{m}{}{m}{}{m}{}{m}{}{m}{}{r}",
            "─".repeat(w_idx + 2),
            "─".repeat(name_w + 2),
            "─".repeat(org_w + 2),
            "─".repeat(w_tier + 2),
            "─".repeat(w_status + 2),
            "─".repeat(w_quota + 2),
            "─".repeat(w_reqs + 2),
        )
    };

    let header = format!(
        "│ {} │ {} │ {} │ {} │ {} │ {} │ {} │",
        ui::dim(&ui::pad_end("#", w_idx)),
        ui::dim(&ui::pad_end("Account", name_w)),
        ui::dim(&ui::pad_end("Org", org_w)),
        ui::dim(&ui::pad_end("Tier", w_tier)),
        ui::dim(&ui::pad_end("Status", w_status)),
        ui::dim(&ui::pad_end("Quota", w_quota)),
        ui::dim(&ui::pad_start("Reqs", w_reqs)),
    );

    println!("  {}", ui::dim(&line("╭", "┬", "╮")));
    println!("  {header}");
    println!("  {}", ui::dim(&line("├", "┼", "┤")));

    for (i, a) in cfg.accounts.iter().enumerate() {
        // "▶1" for the active account, " 2" otherwise (1-based for the user).
        let n = i + 1;
        let idx = if i == cfg.current {
            format!("{}{}", ui::cyan("▶"), n)
        } else {
            format!(" {n}")
        };
        let org = a.org_name.clone().unwrap_or_else(|| "—".into());
        let tier = a.tier.clone().unwrap_or_else(|| "—".into());

        let status = if a.is_cooling_down(now) {
            let secs = (a.cooldown_until.unwrap_or(0) - now).max(0) / 1000;
            ui::status_dot(&format!("cooldown {}", fmt_dur(secs)), ui::Status::Down)
        } else if a.is_near_quota(now, 0.95) {
            ui::status_dot("near-quota", ui::Status::Warn)
        } else {
            ui::status_dot("ready", ui::Status::Ready)
        };

        let quota = ui::quota_bar(a.peak_utilization(now));

        println!(
            "  │ {} │ {} │ {} │ {} │ {} │ {} │ {} │",
            ui::pad_end(&idx, w_idx),
            ui::pad_end(&a.name, name_w),
            ui::pad_end(&org, org_w),
            ui::pad_end(&tier, w_tier),
            ui::pad_end(&status, w_status),
            ui::pad_end(&quota, w_quota),
            ui::pad_start(&a.usage_count.to_string(), w_reqs),
        );
    }

    println!("  {}", ui::dim(&line("╰", "┴", "╯")));
    println!();
    Ok(())
}

fn cmd_strategy(name: Option<String>) -> Result<()> {
    let mut cfg = Config::load()?;
    match name {
        None => {
            println!("  Strategy: {}", ui::cyan(cfg.strategy.label()));
            println!(
                "  {}",
                ui::dim("Options:  round-robin | least-used | session-sticky")
            );
        }
        Some(n) => match Strategy::parse(&n) {
            Some(s) => {
                cfg.strategy = s;
                cfg.save()?;
                println!("  {} Strategy set to {}", ui::green("✓"), ui::cyan(s.label()));
            }
            None => {
                eprintln!("  {} Unknown strategy '{n}'.", ui::red("✗"));
                eprintln!("  {}", ui::dim("Use: round-robin, least-used, session-sticky"));
                std::process::exit(1);
            }
        },
    }
    Ok(())
}

/// Resolve an account selector — either a 1-based index (as shown in `list`) or
/// an exact account name — to an array index.
fn resolve_account(cfg: &Config, selector: &str) -> Option<usize> {
    if let Ok(n) = selector.parse::<usize>() {
        if n >= 1 && n <= cfg.accounts.len() {
            return Some(n - 1);
        }
    }
    cfg.find_account(selector)
}

fn cmd_use(selector: &str) -> Result<()> {
    let mut cfg = Config::load()?;
    match resolve_account(&cfg, selector) {
        Some(i) => {
            let name = cfg.accounts[i].name.clone();
            cfg.current = i;
            cfg.save()?;
            println!("  {} Active account: {}", ui::cyan("▶"), ui::bold(&name));
            Ok(())
        }
        None => {
            eprintln!(
                "  {} No account '{selector}'. Run {} to see names/numbers.",
                ui::red("✗"),
                ui::bold("clauden list")
            );
            std::process::exit(1);
        }
    }
}

fn cmd_remove(selector: &str) -> Result<()> {
    let mut cfg = Config::load()?;
    match resolve_account(&cfg, selector) {
        Some(i) => {
            let name = cfg.accounts[i].name.clone();
            cfg.accounts.remove(i);
            if cfg.current >= cfg.accounts.len() {
                cfg.current = 0;
            }
            cfg.save()?;
            println!("  {} Removed {}", ui::green("✓"), ui::bold(&name));
            Ok(())
        }
        None => {
            eprintln!("  {} No account '{selector}'.", ui::red("✗"));
            std::process::exit(1);
        }
    }
}

async fn cmd_run(port: Option<u16>, no_launch: bool) -> Result<()> {
    let mut cfg = Config::load()?;
    if let Some(p) = port {
        cfg.port = p;
    }
    if cfg.accounts.is_empty() {
        eprintln!(
            "  {} No accounts configured. Run {} first.",
            ui::yellow("›"),
            ui::bold("clauden login")
        );
        std::process::exit(1);
    }

    let port = cfg.port;
    println!(
        "\n  {}  {}  {}",
        ui::magenta("(• ◡ -)"),
        ui::bold("clauden"),
        ui::dim(&format!(
            "{} account(s) · {} strategy",
            cfg.accounts.len(),
            cfg.strategy.label()
        ))
    );
    // Launch Claude Code as a child and tie our lifecycle to it: when Claude
    // exits (or the user hits Ctrl-C) we shut the proxy down too. Without this
    // the proxy outlives Claude and the terminal looks "stuck" — Claude's TUI
    // runs in raw mode, so Ctrl-C reaches it as a keystroke, never as a signal
    // clauden could see.
    let child = if no_launch { None } else { spawn_claude(port) };

    let server = server::serve(cfg);
    tokio::pin!(server);

    match child {
        Some(mut child) => {
            tokio::select! {
                res = &mut server => res,
                _ = child.wait() => {
                    println!(
                        "\n  {} Claude Code exited — shutting down the proxy.",
                        ui::dim("›")
                    );
                    Ok(())
                }
                _ = tokio::signal::ctrl_c() => {
                    let _ = child.start_kill();
                    println!("\n  {} Interrupted — shutting down.", ui::dim("›"));
                    Ok(())
                }
            }
        }
        None => {
            tokio::select! {
                res = &mut server => res,
                _ = tokio::signal::ctrl_c() => {
                    println!("\n  {} Interrupted — shutting down.", ui::dim("›"));
                    Ok(())
                }
            }
        }
    }
}

/// Launch Claude Code pointed at the proxy, if `claude` is on PATH. Returns the
/// child handle so the caller can shut down when Claude Code does. `kill_on_drop`
/// makes sure Claude is reaped if the proxy exits for any other reason.
fn spawn_claude(port: u16) -> Option<tokio::process::Child> {
    use tokio::process::Command;
    let base = format!("http://127.0.0.1:{port}");
    match Command::new("claude")
        .env("ANTHROPIC_BASE_URL", &base)
        .env("ANTHROPIC_API_KEY", "clauden-proxy")
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => {
            println!("  Launched Claude Code → {base}");
            Some(child)
        }
        Err(_) => {
            println!("  (claude not found on PATH; set ANTHROPIC_BASE_URL={base} manually)");
            None
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Self-update: rebuild + reinstall the latest version from GitHub via cargo.
fn cmd_update() -> Result<()> {
    use std::process::Command;

    if Command::new("cargo").arg("--version").output().is_err() {
        eprintln!(
            "  {} cargo not found — install Rust (https://rustup.rs) to update.",
            ui::red("✗")
        );
        std::process::exit(1);
    }

    println!(
        "\n  {} updating clauden from {} (this may take a minute)…\n",
        ui::cyan("⠿"),
        ui::dim(REPO_URL)
    );

    let status = Command::new("cargo")
        .args(["install", "--git", REPO_URL, "--force"])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!(
                "\n  {} Updated. Run {} to confirm the version.\n",
                ui::green("✓"),
                ui::bold("clauden --version")
            );
            Ok(())
        }
        _ => {
            eprintln!("  {} Update failed. Try manually:", ui::red("✗"));
            eprintln!("      cargo install --git {REPO_URL} --force");
            std::process::exit(1);
        }
    }
}

/// Compact human duration: `45s`, `12m`, `2h`.
fn fmt_dur(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}
