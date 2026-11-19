//! Read-only client for the Antigravity language_server Connect JSON API.
//!
//! Inside the sidecar, Antigravity passes `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN`.
//! Outside it (the `status` CLI), the address and token are read from the running process.

use crate::http;
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

const SERVICE: &str = "/exa.language_server_pb.LanguageServerService/";

#[derive(Clone, Debug)]
pub struct LsClient {
    pub addr: String,
    csrf: String,
}

impl LsClient {
    pub fn from_env() -> Option<Self> {
        let addr = std::env::var("ANTIGRAVITY_LS_ADDRESS").ok()?;
        let csrf = std::env::var("ANTIGRAVITY_CSRF_TOKEN").ok()?;
        if addr.trim().is_empty() || csrf.trim().is_empty() {
            return None;
        }
        Some(Self { addr: addr.trim().to_string(), csrf: csrf.trim().to_string() })
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
                let c = Self { addr: format!("127.0.0.1:{port}"), csrf: csrf.clone() };
                if c.call("GetAllCascadeTrajectories", &Value::Object(Default::default())).is_ok() {
                    return Some(c);
                }
            }
        }
        None
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
        )
        .map_err(|e| format!("{method}: {e}"))?;
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
