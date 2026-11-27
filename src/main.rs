mod call;
mod cdp;
mod cli_usage;
mod config;
mod convdb;
mod git;
mod hook;
mod http;
mod live;
mod ls;
mod miss;
mod pricing;
mod prompt_dialog;
mod quota;
mod relaunch;
mod server;
mod transcript;
mod updater;
mod usage;

use clap::{Parser, Subcommand};
use config::{find_app_data_dir, find_plugin_data_dir};

#[derive(Parser)]
#[command(name = "agy-statusline", version, about = "Statusline plugin and sidecar for Antigravity")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as an Antigravity UI Sidecar HTTP server on 127.0.0.1:$ANTIGRAVITY_SIDECAR_WEB_PORT
    Sidecar {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Lifecycle hook receiver (reads event JSON from stdin, writes millisecond timing to bridge)
    Hook,
    /// Print the payload the in-page statusline would receive (reads the language_server API)
    Live {
        /// Conversation ids to build views for (defaults to the most recently modified root)
        ids: Vec<String>,
    },
    /// Print a two-line terminal preview for the specified (or most recently viewed) conversation
    Status {
        #[arg(short, long, default_value = "")]
        conversation: String,
    },
    /// Show the native custom-drawn debug-port relaunch confirmation dialog (internal helper)
    #[command(hide = true)]
    RelaunchPrompt {
        /// Optional theme override for testing (`light` or `dark`)
        #[arg(long)]
        theme: Option<String>,
    },
}

fn fmt_tok(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}k", n / 1000),
        n => n.to_string(),
    }
}

/// Two-line terminal preview of the bar for one conversation (default: the one viewed last).
fn print_status(conv: &str) {
    let Some(ls) = ls::LsClient::connect() else {
        eprintln!("language_server not found");
        std::process::exit(1);
    };
    let cfg = config::load_config(&find_plugin_data_dir(&find_app_data_dir()));
    let mut eng = live::Engine::new(ls);
    eng.tick(&Default::default());
    let Some(id) = eng.panel_target(Some(conv)) else {
        println!("no conversations");
        return;
    };
    eng.load_for_panel(&id);
    let p = eng.panel(&id, &cfg, None);
    let id = id.as_str();
    let v = &p["bar"]["views"][id];
    let m = &v["main"];
    let c = |rgb: (u8, u8, u8), s: String| format!("\x1b[38;2;{};{};{}m{s}\x1b[0m", rgb.0, rgb.1, rgb.2);
    let sep = "\x1b[90m | \x1b[0m";
    let used = m["ctxUsed"].as_u64().unwrap_or(0);
    let max = m["ctxMax"].as_u64().unwrap_or(0);
    let mut l1 = vec![
        c((180, 190, 254), format!("✦ {}", m["model"].as_str().unwrap_or("–"))),
        c(
            (203, 166, 247),
            format!("◔ {}% · {} / {}", (used * 100).checked_div(max).unwrap_or(0), fmt_tok(used), fmt_tok(max)),
        ),
        c(
            (137, 220, 235),
            format!(
                "⛁ {} · {:.0}% hit",
                fmt_tok(m["cacheRead"].as_u64().unwrap_or(0)),
                m["cacheHit"].as_f64().unwrap_or(0.0) * 100.0
            ),
        ),
        c(
            (245, 194, 231),
            format!(
                "$ ${:.2}{} · ${:.2} today",
                v["costConv"].as_f64().unwrap_or(0.0),
                if v["multiDay"].as_bool() == Some(true) { " conv" } else { "" },
                p["bar"]["today"].as_f64().unwrap_or(0.0)
            ),
        ),
    ];
    if m["running"].as_bool() == Some(true) {
        l1.push(c((166, 227, 161), "● running".into()));
    }
    let mut l2 = vec![
        c((242, 205, 205), format!("⧗ {:.1}s", m["ttftMs"].as_f64().unwrap_or(0.0) / 1000.0)),
        c((148, 226, 213), format!("ϟ {:.0} tok/s", m["tps"].as_f64().unwrap_or(0.0))),
    ];
    for q in v["quota"].as_array().into_iter().flatten() {
        let label = if q["window"] == "5h" { "5h" } else { "7d" };
        l2.push(c((250, 179, 135), format!("{label} {:.0}%", q["used"].as_f64().unwrap_or(0.0) * 100.0)));
    }
    println!("{}", l1.join(sep));
    println!("{}", l2.join(sep));
    println!("\x1b[90m{} ({id})\x1b[0m", p["title"].as_str().unwrap_or(""));
}

fn print_live(ids: Vec<String>) {
    let Some(ls) = ls::LsClient::connect() else {
        eprintln!("language_server not found");
        std::process::exit(1);
    };
    let app_data = find_app_data_dir();
    let cfg = config::load_config(&find_plugin_data_dir(&app_data));
    let mut eng = live::Engine::new(ls);
    eng.tick(&Default::default());
    let mut panes: std::collections::HashSet<String> = ids.into_iter().collect();
    if panes.is_empty()
        && let Some((id, _)) =
            eng.summaries().iter().filter(|(_, s)| s.root.is_none()).max_by_key(|(_, s)| s.modified_ms)
    {
        panes.insert(id.clone());
    }
    eng.tick(&panes);
    let payload = eng.build_payload(&panes, &cfg, None);
    println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Sidecar { port }) => {
            if let Err(e) = server::run_server(port) {
                eprintln!("[agy-statusline] server error: {e}");
                std::process::exit(1);
            }
        }
        Some(Commands::Hook) => hook::run_hook(),
        Some(Commands::Live { ids }) => print_live(ids),
        Some(Commands::Status { conversation }) => print_status(&conversation),
        Some(Commands::RelaunchPrompt { theme }) => {
            let res = prompt_dialog::run_prompt_process(theme.as_deref());
            std::process::exit(res.to_exit_code());
        }
        None => {
            if std::env::var("ANTIGRAVITY_SIDECAR_WEB_PORT").is_ok() {
                let _ = server::run_server(None);
            } else {
                print_status("");
            }
        }
    }
}
