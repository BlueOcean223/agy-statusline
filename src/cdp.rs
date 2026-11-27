//! Draws the statusline inside the Antigravity window over the Chrome DevTools Protocol,
//! injects optional Chinese UI (including Swarm panel iframes and temporary English toggle),
//! and auto-reloads stuck `"plugin unavailable"` side-panel iframes after sidecar restarts.
//!
//! CDP is used for painting and i18n only. The page script (`web/bar.js`) has no data logic:
//! it mounts a bar above every chat input, exposes which conversation each input belongs to
//! (`__agyBar.panes()`, polled every second), and renders whatever JSON the engine pushes.
//!
//! Antigravity 2.33+ only opens the debugging port when launched with `--remote-debugging-port`;
//! the port is then written to `<userData>/DevToolsActivePort`.

use crate::http;
use crate::live::{SharedRef, wake};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

const BAR_JS: &str = include_str!("../web/bar.js");
// Chinese UI from agy-zh-pack (MIT, see i18n/LICENSE): translator v12 with sentence fallback,
// temporary English toggle (Ctrl+Alt+E / Ctrl+Option+E), and per-frame options.
const I18N_TRANSLATOR: &str = include_str!("../i18n/translator.js");
const I18N_DICT: &str = include_str!("../i18n/zh-CN.json");
const I18N_RULES: &str = include_str!("../i18n/zh-CN.rules.json");

const MAIN_I18N_OPTS: &str = r#"{"noMissing":true}"#;
const SWARM_I18N_OPTS: &str = r##"{"noMissing":true,"exclude":["[data-testid=\"swarm-message\"]","[data-testid=\"swarm-running-activity\"]","[data-testid=\"swarm-member-chip\"]",".o2-runt",".o2-thg",".o2-thn",".msg-body","#swarm-title","#raw"],"allow":["[data-testid=\"swarm-message-more\"]",".o2-thn > span","[data-testid=\"swarm-filters\"] option","[data-testid=\"swarm-channel-filter\"] option"],"rules":[["^From (\\S+)$","来自 $1"],["^To (@?\\S+)$","发给 $1"]]}"##;

const ASK_BINDING: &str = "__agyZhAsk";
const POLL_EVERY: Duration = Duration::from_millis(1000);
const PANEL_CHECK_SECS: u64 = 30;
const PANEL_MAX_TRIES: u32 = 6;
const FRAME_RETRY_COOLDOWN: Duration = Duration::from_secs(60);
/// `Runtime.enable` (needed for `Runtime.addBinding`) makes V8 retain every `console.*` argument
/// and thrown exception for the life of the session; releasing them periodically bounds that.
const DISCARD_CONSOLE_EVERY: Duration = Duration::from_secs(60);
/// Bounds the WebSocket handshake and writes so a stalled target cannot hang its thread forever.
const WS_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// High-volume events delivered because of `Runtime.enable` that we never act on.
const IGNORED_EVENTS: [&str; 2] = ["\"method\":\"Runtime.consoleAPICalled\"", "\"method\":\"Runtime.exceptionThrown\""];

/// Cheaply recognizes ignored CDP events without JSON-parsing them. Chromium serializes `method`
/// first in event messages, so only the head is scanned; anything else falls through to parsing.
fn is_ignored_event(text: &str) -> bool {
    let head = &text.as_bytes()[..text.len().min(64)];
    IGNORED_EVENTS.iter().any(|m| head.windows(m.len()).any(|w| w == m.as_bytes()))
}

/// Opens a CDP WebSocket with read/write timeouts already applied to the handshake.
fn connect_ws(ws_url: &str, read_timeout: Duration) -> Result<WebSocket<TcpStream>, String> {
    let host = ws_url.strip_prefix("ws://").and_then(|r| r.split('/').next()).ok_or("bad websocket url")?;
    let stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(WS_IO_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(WS_IO_TIMEOUT)).map_err(|e| e.to_string())?;
    let (ws, _) = tungstenite::client(ws_url, stream).map_err(|e| e.to_string())?;
    ws.get_ref().set_read_timeout(Some(read_timeout)).map_err(|e| e.to_string())?;
    Ok(ws)
}

fn i18n_source(dict: Option<&str>, rules: Option<&str>, opts: &str) -> String {
    let d = dict.unwrap_or(I18N_DICT);
    let r = rules.unwrap_or(I18N_RULES);
    format!("try {{ ({I18N_TRANSLATOR})({d}, {r}, {opts}); }} catch (e) {{ console.warn('agy i18n', e); }}")
}

fn disable_i18n_expr() -> &'static str {
    "if (window.__agyI18n) { try { if (window.__agyI18n.restore) window.__agyI18n.restore(); else if (window.__agyI18n.setPaused) window.__agyI18n.setPaused(true, true); else window.__agyI18n.update({}, []); } catch (e) {} window.__agyI18n.disable(); }"
}

fn pause_i18n_expr(paused: bool, quiet: bool) -> String {
    format!(
        "if (window.__agyI18n && window.__agyI18n.setPaused) {{ try {{ window.__agyI18n.setPaused({paused}, {quiet}); }} catch (e) {{}} }}"
    )
}

fn devtools_port_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AGY_DEVTOOLS_PORT_FILE") {
        return Some(PathBuf::from(p));
    }
    #[cfg(target_os = "macos")]
    let base = dirs::home_dir()?.join("Library/Application Support/Antigravity");
    #[cfg(target_os = "windows")]
    let base = PathBuf::from(std::env::var("APPDATA").ok()?).join("Antigravity");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = dirs::config_dir()?.join("Antigravity");
    Some(base.join("DevToolsActivePort"))
}

fn devtools_port() -> Option<u16> {
    let text = std::fs::read_to_string(devtools_port_file()?).ok()?;
    text.lines().next()?.trim().parse().ok()
}

fn list_pages(port: u16) -> Option<Vec<Value>> {
    // 4s timeout avoids false port-down readings when the machine is temporarily busy.
    let res =
        http::request(&format!("127.0.0.1:{port}"), "GET", "/json/list", &[], &[], Duration::from_millis(4000)).ok()?;
    let v: Value = serde_json::from_slice(&res.body).ok()?;
    Some(v.as_array()?.clone())
}

/// Extracts the sidecar HTTP port from an Antigravity plugin panel iframe URL,
/// e.g. `plugin://agy-statusline--core-19847/` → `Some(19847)`.
pub(crate) fn panel_iframe_port(url: &str) -> Option<u16> {
    let rest = url.strip_prefix("plugin://agy-statusline--")?;
    let host = rest.split('/').next()?;
    let (_, port_str) = host.rsplit_once('-')?;
    port_str.parse::<u16>().ok()
}

#[derive(Default)]
struct PanelCheck {
    at: Option<Instant>,
    tries: u32,
    failed: bool,
}

impl PanelCheck {
    fn wait_duration(&self) -> Duration {
        if self.tries > 0 {
            let secs = 10u64.saturating_mul(1u64 << (self.tries - 1).min(5)).min(160);
            Duration::from_secs(secs)
        } else if self.failed {
            Duration::from_secs(10)
        } else if self.at.is_some() {
            Duration::from_secs(PANEL_CHECK_SECS)
        } else {
            Duration::ZERO
        }
    }
}

/// Connects briefly to a CDP target, evaluates one expression, and optionally reloads it.
fn check_and_reload_panel(ws_url: &str, check: &mut PanelCheck) {
    let Some(host) = ws_url.strip_prefix("ws://").and_then(|r| r.split('/').next()) else {
        check.failed = true;
        return;
    };
    let Ok(stream) = TcpStream::connect(host) else {
        check.failed = true;
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok((mut ws, _)) = tungstenite::client(ws_url, stream) else {
        check.failed = true;
        return;
    };

    let expr = "JSON.stringify({ ok: !!document.getElementById('panel-overview'), ready: document.readyState })";
    let msg = json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": { "expression": expr, "returnByValue": true }
    })
    .to_string();
    if ws.send(Message::text(msg)).is_err() {
        check.failed = true;
        return;
    }

    let mut status_json: Option<Value> = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Text(t)) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str())
                    && v["id"].as_u64() == Some(1)
                {
                    if let Some(s) = v["result"]["result"]["value"].as_str() {
                        status_json = serde_json::from_str(s).ok();
                    }
                    break;
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    let Some(s) = status_json else {
        check.failed = true;
        return;
    };
    if s["ok"].as_bool() == Some(true) {
        check.tries = 0;
        check.failed = false;
        let _ = ws.close(None);
        return;
    }
    if s["ready"].as_str() == Some("loading") {
        check.failed = true;
        let _ = ws.close(None);
        return;
    }
    if check.tries >= PANEL_MAX_TRIES {
        let _ = ws.close(None);
        return;
    }

    check.tries += 1;
    check.failed = false;
    eprintln!(
        "[agy-statusline] panel iframe appears stuck (plugin unavailable), reloading ({}/{})",
        check.tries, PANEL_MAX_TRIES
    );
    let reload = json!({ "id": 2, "method": "Page.reload", "params": { "ignoreCache": true } }).to_string();
    let _ = ws.send(Message::text(reload));
    let fallback =
        json!({ "id": 3, "method": "Runtime.evaluate", "params": { "expression": "location.reload()" } }).to_string();
    let _ = ws.send(Message::text(fallback));
    let _ = ws.close(None);
}

pub fn run(shared: SharedRef) {
    let connected: Arc<Mutex<HashSet<String>>> = Arc::default();
    let connected_frames: Arc<Mutex<HashSet<String>>> = Arc::default();
    let frame_fail_at: Arc<Mutex<HashMap<String, Instant>>> = Arc::default();
    let mut panel_checks: HashMap<String, PanelCheck> = HashMap::new();

    loop {
        let panel_port = shared.0.lock().map(|s| s.panel_port).unwrap_or(0);
        let (port_ok, status) = match devtools_port() {
            None => (false, "no debugging port: launch Antigravity with --remote-debugging-port=0".to_string()),
            Some(port) => match list_pages(port) {
                None => (false, format!("debugging port {port} not answering (stale DevToolsActivePort?)")),
                Some(targets) => {
                    let mut seen_panels: HashSet<String> = HashSet::new();
                    for t in targets {
                        let kind = t["type"].as_str().unwrap_or("");
                        let url = t["url"].as_str().unwrap_or("");
                        let id = t["id"].as_str().unwrap_or("").to_string();
                        let ws = t["webSocketDebuggerUrl"].as_str().unwrap_or("").to_string();
                        if id.is_empty() || ws.is_empty() {
                            continue;
                        }

                        // 1. Main Antigravity pages: statusline + i18n
                        if kind == "page" && !url.starts_with("devtools://") && !url.starts_with("data:") {
                            if !connected.lock().map(|mut c| c.insert(id.clone())).unwrap_or(false) {
                                continue;
                            }
                            let (shared, connected, url) = (shared.clone(), connected.clone(), url.to_string());
                            thread::spawn(move || {
                                if let Err(e) = run_page(&id, &url, &ws, &shared) {
                                    eprintln!("[agy-statusline] cdp page {}: {e}", &id[..id.len().min(8)]);
                                }
                                if let Ok(mut s) = shared.0.lock() {
                                    s.panes.remove(&id);
                                    s.pages.remove(&id);
                                }
                                if let Ok(mut c) = connected.lock() {
                                    c.remove(&id);
                                }
                            });
                            continue;
                        }

                        // 2. Swarm panel iframes (`plugin://swarm--...`): optional Chinese UI
                        if kind == "iframe" && url.starts_with("plugin://swarm--") {
                            let in_cooldown = frame_fail_at
                                .lock()
                                .ok()
                                .and_then(|m| m.get(&id).copied())
                                .is_some_and(|at| at.elapsed() < FRAME_RETRY_COOLDOWN);
                            if in_cooldown
                                || !connected_frames.lock().map(|mut c| c.insert(id.clone())).unwrap_or(false)
                            {
                                continue;
                            }
                            let (shared, connected_frames, frame_fail_at) =
                                (shared.clone(), connected_frames.clone(), frame_fail_at.clone());
                            thread::spawn(move || {
                                if let Err(e) = run_swarm_frame(&id, &ws, &shared) {
                                    eprintln!("[agy-statusline] swarm frame {}: {e}", &id[..id.len().min(8)]);
                                    if let Ok(mut m) = frame_fail_at.lock() {
                                        m.insert(id.clone(), Instant::now());
                                    }
                                }
                                if let Ok(mut s) = shared.0.lock() {
                                    s.swarm_frames.remove(&id);
                                }
                                if let Ok(mut c) = connected_frames.lock() {
                                    c.remove(&id);
                                }
                            });
                            continue;
                        }

                        // 3. Our own side-panel iframe: auto-reload if stuck on "plugin unavailable"
                        if kind == "iframe" && panel_port > 0 && panel_iframe_port(url) == Some(panel_port) {
                            seen_panels.insert(id.clone());
                            let entry = panel_checks.entry(id).or_default();
                            if entry.at.is_none_or(|t| t.elapsed() >= entry.wait_duration()) {
                                entry.at = Some(Instant::now());
                                check_and_reload_panel(&ws, entry);
                            }
                        }
                    }
                    panel_checks.retain(|id, _| seen_panels.contains(id));
                    if let Ok(mut m) = frame_fail_at.lock() {
                        m.retain(|_, at| at.elapsed() < FRAME_RETRY_COOLDOWN);
                    }

                    let n = connected.lock().map(|c| c.len()).unwrap_or(0);
                    (true, format!("port {port}, {n} page(s) attached"))
                }
            },
        };
        if let Ok(mut s) = shared.0.lock() {
            s.cdp_status = status;
            if s.cdp_port_ok != Some(port_ok) {
                s.cdp_port_ok = Some(port_ok);
                s.cdp_port_since = Some(Instant::now());
            }
        }
        thread::sleep(Duration::from_secs(3));
    }
}

struct Page {
    ws: WebSocket<TcpStream>,
    seq: u64,
}

impl Page {
    fn send(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.seq += 1;
        let msg = json!({ "id": self.seq, "method": method, "params": params }).to_string();
        self.ws.send(Message::text(msg)).map_err(|e| format!("{method}: {e}"))
    }

    fn push(&mut self, payload: &str) -> Result<(), String> {
        let expr = format!("window.__agyBar && window.__agyBar.update({payload})");
        self.send("Runtime.evaluate", json!({ "expression": expr }))
    }
}

/// Handles `window.__agyZhAsk(...)` calls from translator v12 (e.g. Ctrl+Alt+E temporary English toggle).
fn handle_ask_binding(origin_id: &str, payload: &str, shared: &SharedRef) {
    let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
    if v["t"].as_str() == Some("english") {
        let on = v["on"].as_bool().unwrap_or(false);
        if let Ok(mut s) = shared.0.lock() {
            s.ui_zh_paused = on;
            s.ui_zh_pause_rev += 1;
            s.ui_zh_pause_origin = Some(origin_id.to_string());
            s.ui_zh_pause_quiet = true;
        }
        wake(shared);
    }
}

fn run_page(id: &str, url: &str, ws_url: &str, shared: &SharedRef) -> Result<(), String> {
    let ws = connect_ws(ws_url, Duration::from_millis(200))?;
    let mut page = Page { ws, seq: 0 };

    page.send("Page.enable", json!({}))?;
    let _ = page.send("Runtime.enable", json!({}));
    let _ = page.send("Runtime.addBinding", json!({ "name": ASK_BINDING }));
    // Survives reloads and navigations; the evaluate below covers the already-loaded document.
    page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": BAR_JS }))?;
    page.send("Runtime.evaluate", json!({ "expression": BAR_JS }))?;
    if let Ok(mut s) = shared.0.lock() {
        s.pages.insert(id.to_string(), url.to_string());
    }

    let mut pushed_rev = 0u64;
    let mut poll_id: Option<u64> = None;
    let mut zh_applied: Option<bool> = None;
    let mut zh_rev = u64::MAX;
    let mut zh_pause_rev = 0u64;
    let mut zh_add_id: Option<u64> = None;
    let mut zh_script: Option<String> = None;
    let mut zh_old_script: Option<String> = None;
    // `None` = never polled, so the first loop polls. Not `Instant::now() - POLL_EVERY`: on
    // Windows an Instant cannot predate boot, and that subtraction panics right after boot.
    let mut polled_at: Option<Instant> = None;
    let mut discarded_at = Instant::now();
    loop {
        if discarded_at.elapsed() >= DISCARD_CONSOLE_EVERY {
            discarded_at = Instant::now();
            page.send("Runtime.discardConsoleEntries", json!({}))?;
        }
        if poll_id.is_none() && polled_at.is_none_or(|t| t.elapsed() >= POLL_EVERY) {
            polled_at = Some(Instant::now());
            page.send(
                "Runtime.evaluate",
                json!({ "expression": "window.__agyBar ? window.__agyBar.panes() : null", "returnByValue": true }),
            )?;
            poll_id = Some(page.seq);
        }

        match page.ws.read() {
            // Falls through (no `continue`) so a console flood cannot starve the work below.
            Ok(Message::Text(t)) if is_ignored_event(t.as_str()) => {}
            Ok(Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                if v["method"].as_str() == Some("Runtime.bindingCalled")
                    && v["params"]["name"].as_str() == Some(ASK_BINDING)
                    && let Some(payload) = v["params"]["payload"].as_str()
                {
                    handle_ask_binding(id, payload, shared);
                    continue;
                }
                if zh_add_id.is_some() && v["id"].as_u64() == zh_add_id {
                    zh_add_id = None;
                    zh_script = v["result"]["identifier"].as_str().map(str::to_string);
                    if let Some(old) = zh_old_script.take() {
                        let _ = page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": old }));
                    }
                }
                if poll_id.is_some() && v["id"].as_u64() == poll_id {
                    poll_id = None;
                    let r = &v["result"]["result"]["value"];
                    let panes: Vec<String> = r["panes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect();
                    let changed = shared
                        .0
                        .lock()
                        .map(|mut s| s.panes.insert(id.to_string(), panes.clone()).as_ref() != Some(&panes))
                        .unwrap_or(false);
                    if changed {
                        wake(shared);
                    }
                    // A reloaded page (or one not injected yet) has no data: push again.
                    if r["hasData"] != true {
                        pushed_rev = 0;
                    }
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }

        let (rev, payload, want_zh, i18n_rev, i18n_dict, i18n_rules, pause_rev, paused, pause_origin, pause_quiet) =
            match shared.0.lock() {
                Ok(s) => {
                    let need_zh_src = s.ui_zh && (zh_applied != Some(true) || zh_rev != s.i18n_rev);
                    (
                        s.rev,
                        if s.rev != pushed_rev { s.payload.clone() } else { String::new() },
                        s.ui_zh,
                        s.i18n_rev,
                        if need_zh_src { s.i18n_dict.clone() } else { None },
                        if need_zh_src { s.i18n_rules.clone() } else { None },
                        s.ui_zh_pause_rev,
                        s.ui_zh_paused,
                        s.ui_zh_pause_origin.clone(),
                        s.ui_zh_pause_quiet,
                    )
                }
                Err(_) => return Err("state poisoned".into()),
            };
        if zh_add_id.is_none() && (zh_applied != Some(want_zh) || (want_zh && zh_rev != i18n_rev)) {
            if want_zh {
                let src = i18n_source(i18n_dict.as_deref(), i18n_rules.as_deref(), MAIN_I18N_OPTS);
                // Register new script first, then remove old once its identifier arrives (zero-gap).
                zh_old_script = zh_script.take();
                page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": src }))?;
                zh_add_id = Some(page.seq);
                page.send("Runtime.evaluate", json!({ "expression": src }))?;
                zh_rev = i18n_rev;
                if paused {
                    page.send("Runtime.evaluate", json!({ "expression": pause_i18n_expr(true, true) }))?;
                }
                zh_pause_rev = pause_rev;
            } else {
                if let Some(sid) = zh_script.take() {
                    page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": sid }))?;
                }
                if let Some(old) = zh_old_script.take() {
                    page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": old }))?;
                }
                // Translator v12 remembers original English strings in WeakMaps: updating with
                // empty dict/rules restores all translated text immediately before disconnecting.
                page.send("Runtime.evaluate", json!({ "expression": disable_i18n_expr() }))?;
                zh_pause_rev = pause_rev;
            }
            zh_applied = Some(want_zh);
        } else if want_zh && zh_applied == Some(true) && zh_pause_rev != pause_rev {
            zh_pause_rev = pause_rev;
            if pause_origin.as_deref() != Some(id) {
                page.send("Runtime.evaluate", json!({ "expression": pause_i18n_expr(paused, pause_quiet) }))?;
            }
        }
        if rev != pushed_rev && !payload.is_empty() {
            page.push(&payload)?;
            pushed_rev = rev;
        }
    }
}

/// Injects translator v12 with Swarm-specific exclude/allow/rules into `plugin://swarm--` iframes.
fn run_swarm_frame(id: &str, ws_url: &str, shared: &SharedRef) -> Result<(), String> {
    let ws = connect_ws(ws_url, Duration::from_millis(300))?;
    let mut page = Page { ws, seq: 0 };

    let _ = page.send("Page.enable", json!({}));
    let _ = page.send("Runtime.enable", json!({}));
    let _ = page.send("Runtime.addBinding", json!({ "name": ASK_BINDING }));
    if let Ok(mut s) = shared.0.lock() {
        s.swarm_frames.insert(id.to_string());
    }

    let mut zh_applied: Option<bool> = None;
    let mut zh_rev = u64::MAX;
    let mut zh_pause_rev = 0u64;
    let mut zh_add_id: Option<u64> = None;
    let mut zh_script: Option<String> = None;
    let mut zh_old_script: Option<String> = None;
    let mut discarded_at = Instant::now();

    loop {
        if discarded_at.elapsed() >= DISCARD_CONSOLE_EVERY {
            discarded_at = Instant::now();
            page.send("Runtime.discardConsoleEntries", json!({}))?;
        }
        match page.ws.read() {
            // Falls through (no `continue`) so a console flood cannot starve the work below.
            Ok(Message::Text(t)) if is_ignored_event(t.as_str()) => {}
            Ok(Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                if v["method"].as_str() == Some("Runtime.bindingCalled")
                    && v["params"]["name"].as_str() == Some(ASK_BINDING)
                    && let Some(payload) = v["params"]["payload"].as_str()
                {
                    handle_ask_binding(id, payload, shared);
                    continue;
                }
                if zh_add_id.is_some() && v["id"].as_u64() == zh_add_id {
                    zh_add_id = None;
                    zh_script = v["result"]["identifier"].as_str().map(str::to_string);
                    if let Some(old) = zh_old_script.take() {
                        let _ = page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": old }));
                    }
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }

        let (want_zh, i18n_rev, i18n_dict, i18n_rules, pause_rev, paused, pause_origin) = match shared.0.lock() {
            Ok(s) => {
                let need_zh_src = s.ui_zh && (zh_applied != Some(true) || zh_rev != s.i18n_rev);
                (
                    s.ui_zh,
                    s.i18n_rev,
                    if need_zh_src { s.i18n_dict.clone() } else { None },
                    if need_zh_src { s.i18n_rules.clone() } else { None },
                    s.ui_zh_pause_rev,
                    s.ui_zh_paused,
                    s.ui_zh_pause_origin.clone(),
                )
            }
            Err(_) => return Err("state poisoned".into()),
        };

        if zh_add_id.is_none() && (zh_applied != Some(want_zh) || (want_zh && zh_rev != i18n_rev)) {
            if want_zh {
                let src = i18n_source(i18n_dict.as_deref(), i18n_rules.as_deref(), SWARM_I18N_OPTS);
                zh_old_script = zh_script.take();
                if page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": src })).is_ok() {
                    zh_add_id = Some(page.seq);
                }
                page.send("Runtime.evaluate", json!({ "expression": src }))?;
                zh_rev = i18n_rev;
                if paused {
                    page.send("Runtime.evaluate", json!({ "expression": pause_i18n_expr(true, true) }))?;
                }
                zh_pause_rev = pause_rev;
            } else {
                if let Some(sid) = zh_script.take() {
                    let _ = page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": sid }));
                }
                if let Some(old) = zh_old_script.take() {
                    let _ = page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": old }));
                }
                page.send("Runtime.evaluate", json!({ "expression": disable_i18n_expr() }))?;
                zh_pause_rev = pause_rev;
            }
            zh_applied = Some(want_zh);
        } else if want_zh && zh_applied == Some(true) && zh_pause_rev != pause_rev {
            zh_pause_rev = pause_rev;
            if pause_origin.as_deref() != Some(id) {
                page.send("Runtime.evaluate", json!({ "expression": pause_i18n_expr(paused, true) }))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_panel_iframe_port_urls() {
        assert_eq!(panel_iframe_port("plugin://agy-statusline--core-19847/"), Some(19847));
        assert_eq!(panel_iframe_port("plugin://agy-statusline--statusline-54321/index.html"), Some(54321));
        assert_eq!(panel_iframe_port("plugin://swarm--core-19847/"), None);
        assert_eq!(panel_iframe_port("plugin://agy-zh-pack--core-19847/"), None);
    }

    #[test]
    fn recognizes_ignored_runtime_events() {
        assert!(is_ignored_event(r#"{"method":"Runtime.consoleAPICalled","params":{"type":"log","args":[]}}"#));
        assert!(is_ignored_event(r#"{"method":"Runtime.exceptionThrown","params":{"timestamp":1}}"#));
        assert!(!is_ignored_event(r#"{"method":"Runtime.bindingCalled","params":{"name":"__agyZhAsk"}}"#));
        assert!(!is_ignored_event(r#"{"id":3,"result":{"result":{"value":"Runtime.consoleAPICalled"}}}"#));
        assert!(!is_ignored_event(""));
    }

    #[test]
    fn swarm_i18n_opts_is_valid_json() {
        let v: Value = serde_json::from_str(SWARM_I18N_OPTS).expect("valid Swarm opts JSON");
        assert_eq!(v["noMissing"], true);
        assert!(v["exclude"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(v["allow"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(v["rules"].as_array().is_some_and(|a| a.len() == 2));
    }
}
