mod config;
mod hook;
mod server;
mod telemetry;

use clap::{Parser, Subcommand};
use config::{find_app_data_dir, find_plugin_data_dir};
use telemetry::{build_snapshot, fmt_tokens, fmt_usd};

#[derive(Parser)]
#[command(name = "agy-ccline", version, about = "Pure-Rust CCometixLine plugin & sidecar for Antigravity")]
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
    /// Print live statusline & telemetry summary for the active or specified conversation
    Status {
        #[arg(short, long, default_value = "")]
        conversation: String,
    },
}

fn print_status(conv: &str) {
    let app_data = find_app_data_dir();
    let plugin_data = find_plugin_data_dir(&app_data);
    let snap = build_snapshot(&app_data, &plugin_data, conv, &[]);
    let s = &snap.summary;

    // Two-line ANSI CCometixLine preview in terminal
    println!(
        "\x1b[38;2;180;190;254m✦ {}{}\x1b[0m \x1b[90m|\x1b[0m \x1b[38;2;137;220;235m◔ {:.0}% · {}/{}{}\x1b[0m \x1b[90m|\x1b[0m \x1b[38;2;148;226;213m⛁ {} · {:.0}% hit\x1b[0m \x1b[90m|\x1b[0m \x1b[38;2;245;194;231m{} · {} today\x1b[0m",
        s.model_short,
        if s.effort.is_empty() { String::new() } else { format!("·{}", s.effort) },
        s.context_pct,
        fmt_tokens(s.context_tokens),
        fmt_tokens(s.context_limit),
        if s.compactions > 0 { format!(" · {}× compact", s.compactions) } else { String::new() },
        fmt_tokens(s.last_cache_tokens),
        s.last_cache_hit_pct,
        fmt_usd(s.session_cost_usd),
        fmt_usd(s.today_cost_usd),
    );
    println!(
        "\x1b[38;2;186;194;222mΣ {} tok\x1b[0m \x1b[90m|\x1b[0m \x1b[38;2;166;227;161m⏲ {:.1} tok/s (avg {:.1})\x1b[0m \x1b[90m|\x1b[0m \x1b[38;2;250;179;135m5h {}% ({}/{}) · 7d {}% ({}/{})\x1b[0m",
        fmt_tokens(s.total_tokens),
        s.last_tps,
        s.avg_tps,
        s.quota_5h.pct,
        s.quota_5h.used_fmt,
        s.quota_5h.limit_fmt,
        s.quota_7d.pct,
        s.quota_7d.used_fmt,
        s.quota_7d.limit_fmt,
    );
    println!(
        "\x1b[90mSession: {} ({}) · State: {}\x1b[0m",
        snap.conversation_title, snap.conversation_id, s.activity_label
    );
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Sidecar { port }) => {
            if let Err(e) = server::run_server(port) {
                eprintln!("[agy-ccline] server error: {e}");
                std::process::exit(1);
            }
        }
        Some(Commands::Hook) => hook::run_hook(),
        Some(Commands::Status { conversation }) => print_status(&conversation),
        None => {
            if std::env::var("ANTIGRAVITY_SIDECAR_WEB_PORT").is_ok() {
                let _ = server::run_server(None);
            } else {
                print_status("");
            }
        }
    }
}
