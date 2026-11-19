//! Draws the statusline inside the Antigravity window over the Chrome DevTools Protocol.
//!
//! CDP is used for painting only. The page script (`web/bar.js`) has no data logic: it mounts a bar
//! above every chat input, exposes which conversation each input belongs to (`__agyBar.panes()`,
//! polled every second), and renders whatever JSON the engine pushes with `Runtime.evaluate`.
//!
//! Antigravity 2.33+ only opens the debugging port when launched with `--remote-debugging-port`;
//! the port is then written to `<userData>/DevToolsActivePort`.

use crate::http;
use crate::live::{SharedRef, wake};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

const BAR_JS: &str = include_str!("../web/bar.js");
// Chinese UI from agy-zh-pack (MIT, see i18n/LICENSE): a translator that rewrites text nodes
// found in the dictionary or matching a rule, and skips conversation content.
const I18N_TRANSLATOR: &str = include_str!("../i18n/translator.js");
const I18N_DICT: &str = include_str!("../i18n/zh-CN.json");
const I18N_RULES: &str = include_str!("../i18n/zh-CN.rules.json");

fn i18n_source() -> String {
    format!("try {{ ({I18N_TRANSLATOR})({I18N_DICT}, {I18N_RULES}); }} catch (e) {{ console.warn('agy i18n', e); }}")
}
const POLL_EVERY: Duration = Duration::from_millis(1000);

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
    let res = http::request(&format!("127.0.0.1:{port}"), "GET", "/json/list", &[], &[], Duration::from_millis(1500)).ok()?;
    let v: Value = serde_json::from_slice(&res.body).ok()?;
    Some(v.as_array()?.clone())
}

pub fn run(shared: SharedRef) {
    let connected: Arc<Mutex<HashSet<String>>> = Arc::default();
    loop {
        let status = match devtools_port() {
            None => "no debugging port: launch Antigravity with --remote-debugging-port=0".to_string(),
            Some(port) => match list_pages(port) {
                None => format!("debugging port {port} not answering (stale DevToolsActivePort?)"),
                Some(targets) => {
                    for t in targets {
                        let kind = t["type"].as_str().unwrap_or("");
                        let url = t["url"].as_str().unwrap_or("");
                        let id = t["id"].as_str().unwrap_or("").to_string();
                        let ws = t["webSocketDebuggerUrl"].as_str().unwrap_or("").to_string();
                        // `data:` pages are Antigravity's splash/overlay windows, never a chat.
                        if kind != "page" || url.starts_with("devtools://") || url.starts_with("data:") || id.is_empty() || ws.is_empty() {
                            continue;
                        }
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
                    }
                    let n = connected.lock().map(|c| c.len()).unwrap_or(0);
                    format!("port {port}, {n} page(s) attached")
                }
            },
        };
        if let Ok(mut s) = shared.0.lock() {
            s.cdp_status = status;
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

fn run_page(id: &str, url: &str, ws_url: &str, shared: &SharedRef) -> Result<(), String> {
    let host = ws_url
        .strip_prefix("ws://")
        .and_then(|r| r.split('/').next())
        .ok_or("bad websocket url")?;
    let stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
    let (ws, _) = tungstenite::client(ws_url, stream).map_err(|e| e.to_string())?;
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(200))).map_err(|e| e.to_string())?;
    let mut page = Page { ws, seq: 0 };

    page.send("Page.enable", json!({}))?;
    // Survives reloads and navigations; the evaluate below covers the already-loaded document.
    page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": BAR_JS }))?;
    page.send("Runtime.evaluate", json!({ "expression": BAR_JS }))?;
    if let Ok(mut s) = shared.0.lock() {
        s.pages.insert(id.to_string(), url.to_string());
    }

    // The page is polled for the conversations its inputs show rather than calling back through a
    // Runtime binding: a binding stays tied to the CDP session that added it, so after the sidecar
    // restarts, a page injected by the previous process would keep calling a dead binding.
    let mut pushed_rev = 0u64;
    let mut poll_id: Option<u64> = None;
    // Chinese UI: whether it is applied, and the id of its registered new-document script.
    // None on attach, so a page left translated by an earlier sidecar run is also switched off.
    let mut zh_applied: Option<bool> = None;
    let mut zh_add_id: Option<u64> = None;
    let mut zh_script: Option<String> = None;
    let mut polled_at = Instant::now() - POLL_EVERY;
    loop {
        if poll_id.is_none() && polled_at.elapsed() >= POLL_EVERY {
            polled_at = Instant::now();
            page.send(
                "Runtime.evaluate",
                json!({ "expression": "window.__agyBar ? window.__agyBar.panes() : null", "returnByValue": true }),
            )?;
            poll_id = Some(page.seq);
        }

        match page.ws.read() {
            Ok(Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                if zh_add_id.is_some() && v["id"].as_u64() == zh_add_id {
                    zh_add_id = None;
                    zh_script = v["result"]["identifier"].as_str().map(str::to_string);
                }
                if poll_id.is_some() && v["id"].as_u64() == poll_id {
                    poll_id = None;
                    let r = &v["result"]["result"]["value"];
                    let panes: Vec<String> =
                        r["panes"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
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

        let (rev, payload, want_zh) = match shared.0.lock() {
            Ok(s) => (s.rev, if s.rev != pushed_rev { s.payload.clone() } else { String::new() }, s.ui_zh),
            Err(_) => return Err("state poisoned".into()),
        };
        if zh_applied != Some(want_zh) && zh_add_id.is_none() {
            if want_zh {
                let src = i18n_source();
                page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": src }))?;
                zh_add_id = Some(page.seq);
                page.send("Runtime.evaluate", json!({ "expression": src }))?;
            } else {
                if let Some(sid) = zh_script.take() {
                    page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": sid }))?;
                }
                // Stops translating new text; text already translated stays until the window reloads.
                page.send("Runtime.evaluate", json!({ "expression": "window.__agyI18n && window.__agyI18n.disable()" }))?;
            }
            zh_applied = Some(want_zh);
        }
        if rev != pushed_rev && !payload.is_empty() {
            page.push(&payload)?;
            pushed_rev = rev;
        }
    }
}
