//! Read-only client for the Antigravity language_server Connect JSON API.
//!
//! Inside the sidecar, Antigravity passes `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN`.
//! Outside it (the `status` CLI), the address and token are read from the running process.

use crate::http;
use serde_json::Value;
use std::io::ErrorKind;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SERVICE: &str = "/exa.language_server_pb.LanguageServerService/";

#[derive(Clone, Debug)]
pub struct LsClient {
    pub addr: String,
    csrf: String,
    /// The address came from the sidecar environment, so it can't be looked up again in-process.
    pub from_env: bool,
    /// Since when every call has failed to connect (nothing listening at `addr`).
    down_since: Arc<Mutex<Option<Instant>>>,
}

impl LsClient {
    pub fn from_env() -> Option<Self> {
        let addr = std::env::var("ANTIGRAVITY_LS_ADDRESS").ok()?;
        let csrf = std::env::var("ANTIGRAVITY_CSRF_TOKEN").ok()?;
        if addr.trim().is_empty() || csrf.trim().is_empty() {
            return None;
        }
        Some(Self::new(addr.trim(), csrf.trim(), true))
    }

    /// Finds the desktop app's language_server via `ps`/`lsof` and probes its listening ports.
    pub fn discover() -> Option<Self> {
        let ps = Command::new("ps").args(["-axo", "pid=,args="]).output().ok()?;
        let text = String::from_utf8_lossy(&ps.stdout);
        for line in text.lines() {
            let line = line.trim();
            if !line.contains("Antigravity.app/Contents/Resources/bin/language_server") {
                continue;
            }
            let (pid, args) = line.split_once(' ')?;
            let csrf = arg_value(args, "--csrf_token")?;
            for port in listening_ports(pid) {
                let c = Self::new(&format!("127.0.0.1:{port}"), &csrf, false);
                if c.call("GetAllCascadeTrajectories", &Value::Object(Default::default())).is_ok() {
                    return Some(c);
                }
            }
        }
        None
    }

    pub(crate) fn new(addr: &str, csrf: &str, from_env: bool) -> Self {
        Self { addr: addr.to_string(), csrf: csrf.to_string(), from_env, down_since: Arc::default() }
    }

    /// How long the language_server has refused connections, if it currently does.
    pub fn unreachable_for(&self) -> Option<Duration> {
        self.down_since.lock().ok()?.map(|t| t.elapsed())
    }

    pub fn connect() -> Option<Self> {
        Self::from_env().or_else(Self::discover)
    }

    pub fn call(&self, method: &str, body: &Value) -> Result<Value, String> {
        let payload = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let path = format!("{SERVICE}{method}");
        let res = http::request(
            &self.addr,
            "POST",
            &path,
            &[("Content-Type", "application/json"), ("x-codeium-csrf-token", &self.csrf)],
            &payload,
            Duration::from_secs(8),
        );
        // Judged by error kind: the OS message is localized (e.g. Chinese on Windows).
        let refused =
            matches!(&res, Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::AddrNotAvailable));
        if let Ok(mut d) = self.down_since.lock() {
            if !refused {
                *d = None;
            } else if d.is_none() {
                *d = Some(Instant::now());
            }
        }
        let res = res.map_err(|e| {
            if refused {
                format!("{method}: nothing is listening at {} ({e})", self.addr)
            } else {
                format!("{method}: {e}")
            }
        })?;
        if res.status != 200 {
            let snippet = String::from_utf8_lossy(&res.body[..res.body.len().min(200)]).to_string();
            return Err(format!("{method}: HTTP {} {snippet}", res.status));
        }
        serde_json::from_slice(&res.body).map_err(|e| format!("{method}: bad JSON: {e}"))
    }
}

fn arg_value(args: &str, flag: &str) -> Option<String> {
    let mut it = args.split_whitespace();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().map(str::to_string);
        }
        if let Some(v) = a.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(v.to_string());
        }
    }
    None
}

fn listening_ports(pid: &str) -> Vec<u16> {
    let Ok(out) = Command::new("lsof").args(["-nP", "-a", "-p", pid, "-iTCP", "-sTCP:LISTEN"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.rsplit_once(':'))
        .filter_map(|(_, rest)| rest.split_whitespace().next()?.parse().ok())
        .collect()
}
