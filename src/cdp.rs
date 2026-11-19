//! Draws the statusline inside the Antigravity window over the Chrome DevTools Protocol.
//!
//! CDP is used for painting only. The page script (`web/bar.js`) has no data logic: it mounts a bar
//! above every chat input, reports which conversation each input belongs to through a CDP binding,
//! and renders whatever JSON the engine pushes with `Runtime.evaluate`.
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
use std::time::Duration;
use tungstenite::{Message, WebSocket};

const BAR_JS: &str = include_str!("../web/bar.js");
const BINDING: &str = "__agyBarHost";

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
                        if kind != "page" || url.starts_with("devtools://") || id.is_empty() || ws.is_empty() {
                            continue;
                        }
                        if !connected.lock().map(|mut c| c.insert(id.clone())).unwrap_or(false) {
                            continue;
                        }
                        let (shared, connected, url) = (shared.clone(), connected.clone(), url.to_string());
                        thread::spawn(move || {
                            if let Err(e) = run_page(&id, &url, &ws, &shared) {
                                eprintln!("[agy-ccline] cdp page {}: {e}", &id[..id.len().min(8)]);
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

    // Binding events are delivered only while the Runtime domain is enabled.
    page.send("Runtime.enable", json!({}))?;
    page.send("Page.enable", json!({}))?;
    page.send("Runtime.addBinding", json!({ "name": BINDING }))?;
    // Survives reloads and navigations; the evaluate below covers the already-loaded document.
    page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": BAR_JS }))?;
    page.send("Runtime.evaluate", json!({ "expression": BAR_JS }))?;
    if let Ok(mut s) = shared.0.lock() {
        s.pages.insert(id.to_string(), url.to_string());
    }

    let mut pushed_rev = 0u64;
    loop {
        match page.ws.read() {
            Ok(Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                match v["method"].as_str() {
                    Some("Runtime.bindingCalled") if v["params"]["name"] == BINDING => {
                        let panes: Vec<String> = serde_json::from_str::<Value>(v["params"]["payload"].as_str().unwrap_or(""))
                            .ok()
                            .and_then(|p| p["panes"].as_array().cloned())
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect();
                        if let Ok(mut s) = shared.0.lock() {
                            s.panes.insert(id.to_string(), panes);
                        }
                        wake(shared);
                        // The page asks right after (re)mounting, so it gets the current data at once.
                        pushed_rev = 0;
                    }
                    Some("Runtime.executionContextsCleared") => pushed_rev = 0,
                    _ => {}
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }

        let (rev, payload) = match shared.0.lock() {
            Ok(s) => (s.rev, if s.rev != pushed_rev { s.payload.clone() } else { String::new() }),
            Err(_) => return Err("state poisoned".into()),
        };
        if rev != pushed_rev && !payload.is_empty() {
            page.push(&payload)?;
            pushed_rev = rev;
        }
    }
}
