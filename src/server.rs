use crate::config::{
    ConfigPatch, OnlinePrices, apply_config_patch, find_app_data_dir, find_plugin_data_dir, load_config,
};
use crate::live::{ENGINE, USAGE, now_ms, wake};
use crate::pricing::Pricer;
use crate::relaunch;
use crate::transcript;
use crate::updater;
use crate::usage;
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
        400 => "Bad Request",
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

fn active_online_prices(cfg_enabled: bool) -> Option<OnlinePrices> {
    if !cfg_enabled {
        return None;
    }
    LIVE.get().and_then(|sh| sh.0.lock().ok().and_then(|s| s.online_prices.clone()))
}

fn requires_ui_token(method: &str, path: &str) -> bool {
    method != "GET" || path == "/api/checkpoint"
}

fn handle_client(stream: TcpStream, ui_token: String) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

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

    // Validate sidecar UI token on non-GET requests and sensitive GET endpoints (e.g. checkpoint summary).
    if requires_ui_token(method, path) && !ui_token.is_empty() {
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
            let online = active_online_prices(cfg.online_data_sync);
            let out = match ENGINE.lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(eng) => eng.panel(conv_id, &cfg, online.as_ref()),
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
        ("GET", "/api/checkpoint") => {
            let conv_id = query.get("conversationId").map(String::as_str).unwrap_or("");
            let out = transcript::read_checkpoint(&app_data, conv_id);
            send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
        }
        ("GET", "/api/usage") => {
            let span = query.get("days").map(String::as_str).unwrap_or("7");
            let cfg = load_config(&plugin_data);
            let online = active_online_prices(cfg.online_data_sync);
            let pricer = Pricer::new(&cfg.models, online.as_ref(), now_ms());
            let (live_calls, writes) = if let Ok(guard) = ENGINE.lock()
                && let Some(eng) = guard.as_ref()
            {
                let writes = eng.sync_usage_store();
                (eng.live_call_records(), writes)
            } else {
                (Vec::new(), Default::default())
            };
            writes.write();
            // Snapshot history, samples, and CLI cache briefly under USAGE lock, then run all disk/SQLite reads lock-free.
            let snap = USAGE.lock().ok().and_then(|g| {
                let store = g.as_ref()?;
                Some((
                    store.history.clone(),
                    store.samples.clone(),
                    store.cli_snap.clone(),
                    store.app_data.clone(),
                    store.cli_data.clone(),
                    store.plugin_data.clone(),
                ))
            });
            let out = match snap {
                Some((history, samples, mut cli_snap, ad, cd, pd)) => usage::build_usage_view(
                    &history,
                    &samples,
                    span,
                    &ad,
                    &cd,
                    &pd,
                    &mut cli_snap,
                    &live_calls,
                    &pricer,
                    now_ms(),
                ),
                None => json!({ "days": [], "error": "usage history initializing" }),
            };
            send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
        }
        ("GET", "/api/usage/day") => {
            let date = query.get("date").map(String::as_str).unwrap_or("");
            if date.len() != 10 || !date.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                send_response(stream, 400, "application/json", br#"{"error":"bad date"}"#);
                return;
            }
            let limit =
                query.get("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(usage::DEFAULT_DAY_CALLS_LIMIT);
            let cfg = load_config(&plugin_data);
            let online = active_online_prices(cfg.online_data_sync);
            let pricer = Pricer::new(&cfg.models, online.as_ref(), now_ms());
            let live_calls =
                ENGINE.lock().ok().and_then(|g| g.as_ref().map(|e| e.live_call_records())).unwrap_or_default();
            let snap = USAGE.lock().ok().and_then(|g| {
                let store = g.as_ref()?;
                Some((
                    store.cli_snap.clone(),
                    store.app_data.clone(),
                    store.cli_data.clone(),
                    store.plugin_data.clone(),
                ))
            });
            let out = match snap {
                Some((mut cli_snap, ad, cd, pd)) => usage::build_day_calls_view(
                    &ad,
                    &cd,
                    &pd,
                    &mut cli_snap,
                    date,
                    &live_calls,
                    &pricer,
                    cfg.costly_call_usd,
                    limit,
                ),
                None => json!({ "date": date, "calls": [], "error": "usage store initializing" }),
            };
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
                            "swarmFrames": s.swarm_frames.len(),
                            "panes": s.panes.values().flatten().collect::<Vec<_>>(),
                            "rev": s.rev,
                            "running": s.running,
                            "debug": !s.pages.is_empty(),
                            "uiZh": s.ui_zh,
                            "zhPaused": s.ui_zh_paused,
                            "relaunch": relaunch::status_json(&plugin_data),
                        })
                    });
                    s.unwrap_or_else(|_| json!({}))
                }
                None => json!({ "cdp": "live engine not running", "ls": "" }),
            };
            send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
        }
        ("POST", "/api/i18n/toggle-english") => {
            let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let out = match LIVE.get() {
                Some(shared) => {
                    let paused = if let Ok(mut s) = shared.0.lock() {
                        let next = payload["paused"].as_bool().unwrap_or(!s.ui_zh_paused);
                        s.ui_zh_paused = next;
                        s.ui_zh_pause_rev += 1;
                        s.ui_zh_pause_origin = None;
                        s.ui_zh_pause_quiet = false;
                        next
                    } else {
                        false
                    };
                    wake(shared);
                    json!({ "ok": true, "paused": paused })
                }
                None => json!({ "ok": false, "error": "engine not started" }),
            };
            send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
        }
        ("GET", "/api/update/status") => {
            let out = updater::status_json(&plugin_data);
            send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
        }
        ("POST", "/api/update/check") => match updater::check_release(&plugin_data) {
            Ok(_) => {
                let out = updater::status_json(&plugin_data);
                send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
            }
            Err(e) => {
                let out = serde_json::to_vec(
                    &json!({ "ok": false, "error": e, "status": updater::status_json(&plugin_data) }),
                )
                .unwrap_or_default();
                send_response(stream, 500, "application/json", &out);
            }
        },
        ("POST", "/api/update/install") => match updater::start_install_release(plugin_data.clone()) {
            Ok(ver) => {
                let out = serde_json::to_vec(&json!({ "ok": true, "version": ver })).unwrap_or_default();
                send_response(stream, 200, "application/json", &out);
            }
            Err(e) => {
                let out = serde_json::to_vec(&json!({ "ok": false, "error": e })).unwrap_or_default();
                send_response(stream, 400, "application/json", &out);
            }
        },
        ("POST", "/api/data-sync") => {
            let Some(shared) = LIVE.get() else {
                send_response(stream, 500, "application/json", br#"{"ok":false,"error":"engine not started"}"#);
                return;
            };
            match updater::sync_remote_data(&plugin_data, shared) {
                Ok(_) => {
                    let out = updater::status_json(&plugin_data);
                    send_response(stream, 200, "application/json", &serde_json::to_vec(&out).unwrap_or_default());
                }
                Err(e) => {
                    let out = serde_json::to_vec(
                        &json!({ "ok": false, "error": e, "status": updater::status_json(&plugin_data) }),
                    )
                    .unwrap_or_default();
                    send_response(stream, 500, "application/json", &out);
                }
            }
        }
        ("GET", "/api/config") => {
            let cfg = load_config(&plugin_data);
            let out = serde_json::to_vec(&cfg).unwrap_or_default();
            send_response(stream, 200, "application/json", &out);
        }
        ("POST", "/api/config") => {
            let patch: ConfigPatch = serde_json::from_slice(&body).unwrap_or_default();
            let toggle_sync = patch.online_data_sync;
            let prev_ui_zh = load_config(&plugin_data).ui_zh;
            let cfg = apply_config_patch(&plugin_data, patch);
            if let Some(shared) = LIVE.get() {
                if cfg.ui_zh != prev_ui_zh
                    && let Ok(mut s) = shared.0.lock()
                {
                    s.ui_zh_paused = false;
                    s.ui_zh_pause_rev += 1;
                    s.ui_zh_pause_origin = None;
                }
                if let Some(en) = toggle_sync {
                    updater::apply_remote_cache_to_shared(&plugin_data, shared, en);
                }
                wake(shared);
            }
            let out = serde_json::to_vec(&json!({ "ok": true, "config": cfg })).unwrap_or_default();
            send_response(stream, 200, "application/json", &out);
        }
        ("POST", "/api/restart-debug") => match relaunch::relaunch_now(&plugin_data) {
            Ok(target) => {
                let out = serde_json::to_vec(&json!({ "ok": true, "target": target })).unwrap_or_default();
                send_response(stream, 200, "application/json", &out);
            }
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

static LIVE: std::sync::OnceLock<crate::live::SharedRef> = std::sync::OnceLock::new();

/// Starts the live engine (language_server polling), CDP painter, relaunch watcher, and updater.
fn start_live(panel_port: u16) {
    let app_data = find_app_data_dir();
    let plugin_data = find_plugin_data_dir(&app_data);
    let shared: crate::live::SharedRef = std::sync::Arc::default();
    if let Ok(mut s) = shared.0.lock() {
        s.panel_port = panel_port;
    }
    let _ = LIVE.set(shared.clone());

    let s1 = shared.clone();
    thread::spawn(move || crate::live::run(s1));
    let s2 = shared.clone();
    thread::spawn(move || crate::cdp::run(s2));
    relaunch::start_watch(shared.clone(), plugin_data.clone());
    updater::start_background(plugin_data, shared);
}

pub fn run_server(explicit_port: Option<u16>) -> std::io::Result<()> {
    let port = explicit_port
        .or_else(|| env::var("ANTIGRAVITY_SIDECAR_WEB_PORT").ok().and_then(|p| p.parse::<u16>().ok()))
        .unwrap_or(19847);

    let ui_token = env::var("ANTIGRAVITY_SIDECAR_UI_TOKEN").unwrap_or_default();
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let bound_port = listener.local_addr()?.port();
    start_live(bound_port);
    eprintln!("[agy-statusline] listening on 127.0.0.1:{bound_port}");

    for stream in listener.incoming().flatten() {
        let tok = ui_token.clone();
        thread::spawn(move || handle_client(stream, tok));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_ui_token_for_mutations_and_checkpoint_endpoint() {
        assert!(requires_ui_token("POST", "/api/config"));
        assert!(requires_ui_token("GET", "/api/checkpoint"));
        assert!(!requires_ui_token("GET", "/api/panel"));
        assert!(!requires_ui_token("GET", "/api/usage"));
    }
}
