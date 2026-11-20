use crate::config::{ConfigPatch, apply_config_patch, find_app_data_dir, find_plugin_data_dir, load_config};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLES_CSS: &str = include_str!("../web/styles.css");
const APP_JS: &str = include_str!("../web/app.js");
const PRELOAD_JS: &str = include_str!("../web/preload.js");
const BAR_JS: &str = include_str!("../web/bar.js");

fn url_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let raw = s.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'+' {
            bytes.push(b' ');
            i += 1;
        } else if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(hex) = std::str::from_utf8(&raw[i + 1..i + 3])
                && let Ok(val) = u8::from_str_radix(hex, 16)
            {
                bytes.push(val);
                i += 3;
                continue;
            }
            bytes.push(raw[i]);
            i += 1;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = url_decode(it.next().unwrap_or(""));
        let v = url_decode(it.next().unwrap_or(""));
        if !k.is_empty() {
            map.insert(k, v);
        }
    }
    map
}

fn send_response(mut stream: TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn call_agentapi(args: &[&str], app_data: &Path) -> Value {
    let exe = env::var("ANTIGRAVITY_AGENTAPI_EXE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| app_data.join("bin/agentapi").to_string_lossy().to_string());

    let mut cmd = Command::new(&exe);
    if env::var("ANTIGRAVITY_AGENTAPI_EXE").is_ok() {
        cmd.arg("agentapi");
    }
    cmd.args(args);

    match cmd.output() {
        Ok(out) if out.status.success() => serde_json::from_slice(&out.stdout).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

fn handle_client(stream: TcpStream, ui_token: String) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let Ok(cloned) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(cloned);

    let mut req_line = String::new();
    if reader.read_line(&mut req_line).is_err() || req_line.trim().is_empty() {
        return;
    }
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let target = parts[1];
    let (path, query_str) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let query = parse_query(query_str);

    let mut headers = HashMap::new();
    let mut content_len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_len = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }

    let mut body = vec![0u8; content_len.min(256 * 1024)];
    if !body.is_empty() && reader.read_exact(&mut body).is_err() {
        return;
    }

    // Validate sidecar UI token on non-GET requests (same contract as sidecar_sdk)
    if method != "GET" && !ui_token.is_empty() {
        let provided = headers.get("x-sidecar-token").or_else(|| query.get("token")).map(String::as_str).unwrap_or("");
        if provided != ui_token {
            send_response(stream, 401, "application/json", br#"{"error":"unauthorized"}"#);
            return;
        }
    }

    let app_data = find_app_data_dir();
    let plugin_data = find_plugin_data_dir(&app_data);

    match (method, path) {
        ("GET", "/" | "/index.html") => {
            send_response(stream, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes());
        }
        ("GET", "/styles.css") => {
            send_response(stream, 200, "text/css; charset=utf-8", STYLES_CSS.as_bytes());
        }
        ("GET", "/app.js") => {
            send_response(stream, 200, "application/javascript; charset=utf-8", APP_JS.as_bytes());
        }
        ("GET", "/preload.js") => {
            send_response(stream, 200, "application/javascript; charset=utf-8", PRELOAD_JS.as_bytes());
        }
        ("GET", "/healthz" | "/api/health") => {
            send_response(stream, 200, "application/json", br#"{"ok":true}"#);
        }
        ("GET", "/bar.js") => {
            send_response(stream, 200, "application/javascript; charset=utf-8", BAR_JS.as_bytes());
        }
        ("GET", "/api/panel") => {
            let conv_id = query.get("conversationId").map(String::as_str).filter(|s| !s.is_empty());
            let cfg = load_config(&plugin_data);
            let out = match crate::live::ENGINE.lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(eng) => eng.panel(conv_id, &cfg),
                    None => json!({ "id": null, "error": "language_server not connected" }),
                },
                Err(_) => json!({ "id": null, "error": "engine unavailable" }),
            };
            // Keeps the engine refreshing this conversation while the panel shows it.
            if let (Some(shared), Some(id)) = (LIVE.get(), out["id"].as_str())
                && let Ok(mut s) = shared.0.lock()
            {
                s.panel.insert(id.to_string(), std::time::Instant::now());
            }
            let mut out = out;
            out["config"] = serde_json::to_value(&cfg).unwrap_or(Value::Null);
            send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
        }
        ("GET", "/api/live-status") => {
            let out = match LIVE.get() {
                Some(shared) => {
                    let s = shared.0.lock().map(|s| {
                        json!({
                            "cdp": s.cdp_status,
                            "ls": s.ls_status,
                            "pages": s.pages.len(),
                            "panes": s.panes.values().flatten().collect::<Vec<_>>(),
                            "rev": s.rev,
                            "running": s.running,
                            "debug": !s.pages.is_empty(),
                        })
                    });
                    s.unwrap_or_else(|_| json!({}))
                }
                None => json!({ "cdp": "live engine not running", "ls": "" }),
            };
            send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
        }
        ("GET", "/api/config") => {
            let cfg = load_config(&plugin_data);
            let out = serde_json::to_vec(&cfg).unwrap_or_default();
            send_response(stream, 200, "application/json", &out);
        }
        ("POST", "/api/config") => {
            let patch: ConfigPatch = serde_json::from_slice(&body).unwrap_or_default();
            let cfg = apply_config_patch(&plugin_data, patch);
            let out = serde_json::to_vec(&json!({ "ok": true, "config": cfg })).unwrap_or_default();
            send_response(stream, 200, "application/json", &out);
        }
        ("POST", "/api/restart-debug") => match restart_debug() {
            Ok(()) => send_response(stream, 200, "application/json", br#"{"ok":true}"#),
            Err(e) => {
                let out = serde_json::to_vec(&json!({ "ok": false, "error": e })).unwrap_or_default();
                send_response(stream, 500, "application/json", &out);
            }
        },
        ("POST", "/_sidecar/get-conversation-metadata") => {
            let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let cid = payload.get("conversationId").and_then(Value::as_str).unwrap_or("");
            let res =
                if cid.is_empty() { json!({}) } else { call_agentapi(&["get-conversation-metadata", cid], &app_data) };
            let out = serde_json::to_vec(&res).unwrap_or_default();
            send_response(stream, 200, "application/json", &out);
        }
        _ => {
            send_response(stream, 404, "text/plain; charset=utf-8", b"Not Found");
        }
    }
}

/// Quits Antigravity and reopens it with `--remote-debugging-port=0`. The script runs in its own
/// process group so it outlives this sidecar, which Antigravity stops when it quits.
#[cfg(target_os = "macos")]
fn restart_debug() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    const SCRIPT: &str = r#"
sleep 1
osascript -e 'tell application "Antigravity" to quit'
i=0
while pgrep -xq Antigravity; do
  i=$((i+1))
  [ "$i" -ge 120 ] && exit 1
  sleep 0.5
done
sleep 1
open -a Antigravity --args --remote-debugging-port=0
"#;
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(SCRIPT)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn restart script: {e}"))
}

#[cfg(not(target_os = "macos"))]
fn restart_debug() -> Result<(), String> {
    Err("restart is only implemented on macOS; relaunch Antigravity with --remote-debugging-port=0".into())
}

static LIVE: std::sync::OnceLock<crate::live::SharedRef> = std::sync::OnceLock::new();

/// Starts the live engine (language_server polling) and the CDP painter next to the HTTP server.
fn start_live() {
    let shared: crate::live::SharedRef = std::sync::Arc::default();
    let _ = LIVE.set(shared.clone());
    let s1 = shared.clone();
    thread::spawn(move || crate::live::run(s1));
    thread::spawn(move || crate::cdp::run(shared));
}

pub fn run_server(explicit_port: Option<u16>) -> std::io::Result<()> {
    start_live();
    let port = explicit_port
        .or_else(|| env::var("ANTIGRAVITY_SIDECAR_WEB_PORT").ok().and_then(|p| p.parse::<u16>().ok()))
        .unwrap_or(19847);

    let ui_token = env::var("ANTIGRAVITY_SIDECAR_UI_TOKEN").unwrap_or_default();
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let bound_port = listener.local_addr()?.port();
    eprintln!("[agy-statusline] listening on 127.0.0.1:{bound_port}");

    for stream in listener.incoming().flatten() {
        let tok = ui_token.clone();
        thread::spawn(move || handle_client(stream, tok));
    }
    Ok(())
}
