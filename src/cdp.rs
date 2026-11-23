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

fn i18n_source(dict: Option<&str>, rules: Option<&str>) -> String {
    let d = dict.unwrap_or(I18N_DICT);
    let r = rules.unwrap_or(I18N_RULES);
    format!("try {{ ({I18N_TRANSLATOR})({d}, {r}); }} catch (e) {{ console.warn('agy i18n', e); }}")
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
    let res =
        http::request(&format!("127.0.0.1:{port}"), "GET", "/json/list", &[], &[], Duration::from_millis(1500)).ok()?;
    let v: Value = serde_json::from_slice(&res.body).ok()?;
    Some(v.as_array()?.clone())
}

pub fn run(shared: SharedRef) {
    let connected: Arc<Mutex<HashSet<String>>> = Arc::default();
    loop {
        let (port_ok, status) = match devtools_port() {
            None => (false, "no debugging port: launch Antigravity with --remote-debugging-port=0".to_string()),
            Some(port) => match list_pages(port) {
                None => (false, format!("debugging port {port} not answering (stale DevToolsActivePort?)")),
                Some(targets) => {
                    for t in targets {
                        let kind = t["type"].as_str().unwrap_or("");
                        let url = t["url"].as_str().unwrap_or("");
                        let id = t["id"].as_str().unwrap_or("").to_string();
                        let ws = t["webSocketDebuggerUrl"].as_str().unwrap_or("").to_string();
                        // `data:` pages are Antigravity's splash/overlay windows, never a chat.
                        if kind != "page"
                            || url.starts_with("devtools://")
                            || url.starts_with("data:")
                            || id.is_empty()
                            || ws.is_empty()
                        {
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

fn run_page(id: &str, url: &str, ws_url: &str, shared: &SharedRef) -> Result<(), String> {
    let host = ws_url.strip_prefix("ws://").and_then(|r| r.split('/').next()).ok_or("bad websocket url")?;
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

    let mut pushed_rev = 0u64;
    let mut poll_id: Option<u64> = None;
    // Chinese UI: whether it is applied, which i18n revision is active, and registered script IDs.
    let mut zh_applied: Option<bool> = None;
    let mut zh_rev = u64::MAX;
    let mut zh_add_id: Option<u64> = None;
    let mut zh_script: Option<String> = None;
    let mut zh_old_script: Option<String> = None;
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

        let (rev, payload, want_zh, i18n_rev, i18n_dict, i18n_rules) = match shared.0.lock() {
            Ok(s) => {
                let need_zh_src = s.ui_zh && (zh_applied != Some(true) || zh_rev != s.i18n_rev);
                (
                    s.rev,
                    if s.rev != pushed_rev { s.payload.clone() } else { String::new() },
                    s.ui_zh,
                    s.i18n_rev,
                    if need_zh_src { s.i18n_dict.clone() } else { None },
                    if need_zh_src { s.i18n_rules.clone() } else { None },
                )
            }
            Err(_) => return Err("state poisoned".into()),
        };
        if zh_add_id.is_none() && (zh_applied != Some(want_zh) || (want_zh && zh_rev != i18n_rev)) {
            if want_zh {
                let src = i18n_source(i18n_dict.as_deref(), i18n_rules.as_deref());
                // Register new script first, then remove old once its identifier arrives (zero-gap).
                zh_old_script = zh_script.take();
                page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": src }))?;
                zh_add_id = Some(page.seq);
                page.send("Runtime.evaluate", json!({ "expression": src }))?;
                zh_rev = i18n_rev;
            } else {
                if let Some(sid) = zh_script.take() {
                    page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": sid }))?;
                }
                if let Some(old) = zh_old_script.take() {
                    page.send("Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": old }))?;
                }
                // Translator v10 remembers original English strings in WeakMaps: updating with
                // empty dict/rules restores all translated text immediately before disconnecting.
                page.send(
                    "Runtime.evaluate",
                    json!({
                        "expression": "if (window.__agyI18n) { try { window.__agyI18n.update({}, []); } catch (e) {} window.__agyI18n.disable(); }"
                    }),
                )?;
            }
            zh_applied = Some(want_zh);
        }
        if rev != pushed_rev && !payload.is_empty() {
            page.push(&payload)?;
            pushed_rev = rev;
        }
    }
}
