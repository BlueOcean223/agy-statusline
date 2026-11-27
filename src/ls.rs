//! Read-only client for the Antigravity language_server Connect JSON API.
//!
//! Inside the sidecar, Antigravity passes `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN`.
//! Outside it (the `status`/`live` CLI), [`LsClient::discover`] looks them up on this machine:
//! - older language_servers take `--csrf_token` on the command line;
//! - newer ones (2.35+, `--standalone --subclient_type hub`) read their token from stdin and write
//!   `<app data>/daemon/hub_<port>.json` with `pid`, `httpPort` and `csrfToken`;
//! - as a last resort, a running agy-statusline sidecar's environment holds both values.
//!
//! Only loopback addresses are contacted, and only `GetAllCascadeTrajectories` is used to probe.

use crate::http;
use serde_json::Value;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

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

    /// Finds the desktop app's language_server on this machine (see the module docs for sources).
    pub fn discover() -> Option<Self> {
        let ps = Command::new("ps").args(["-axo", "pid=,args="]).output().ok()?;
        let ps = String::from_utf8_lossy(&ps.stdout);
        let servers = language_servers(&ps);

        // Older language_servers: `--csrf_token` on the command line, port found with `lsof`.
        for (pid, args) in &servers {
            let Some(csrf) = arg_value(args, "--csrf_token") else { continue };
            for port in listening_ports(*pid) {
                if let Some(c) = Self::probe(&format!("127.0.0.1:{port}"), &csrf) {
                    return Some(c);
                }
            }
        }

        // Newer language_servers: the discovery file under `<app data>/daemon/`, trusted only
        // when its pid is a running language_server.
        for info in daemon_files(&crate::config::find_app_data_dir().join("daemon")) {
            if !servers.iter().any(|(pid, _)| *pid == info.pid) {
                continue;
            }
            if let Some(c) = Self::probe(&format!("127.0.0.1:{}", info.http_port), &info.csrf) {
                return Some(c);
            }
        }

        // Fallback: a running sidecar was handed the address and token by Antigravity.
        for pid in sidecar_pids(&ps) {
            let Some(env) = process_env(pid) else { continue };
            let (Some(addr), Some(csrf)) =
                (env_value(&env, "ANTIGRAVITY_LS_ADDRESS"), env_value(&env, "ANTIGRAVITY_CSRF_TOKEN"))
            else {
                continue;
            };
            let Some(addr) = loopback_addr(addr) else { continue };
            if let Some(c) = Self::probe(addr, csrf) {
                return Some(c);
            }
        }
        None
    }

    /// Returns a client for `addr` if `GetAllCascadeTrajectories` succeeds there. That call only
    /// lists conversations the language_server has already loaded, so it loads nothing new.
    fn probe(addr: &str, csrf: &str) -> Option<Self> {
        let c = Self::new(addr, csrf, false);
        c.call("GetAllCascadeTrajectories", &Value::Object(Default::default())).is_ok().then_some(c)
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

fn listening_ports(pid: u32) -> Vec<u16> {
    let pid = pid.to_string();
    let Ok(out) = Command::new("lsof").args(["-nP", "-a", "-p", &pid, "-iTCP", "-sTCP:LISTEN"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.rsplit_once(':'))
        .filter_map(|(_, rest)| rest.split_whitespace().next()?.parse().ok())
        .collect()
}

/// Splits `ps -axo pid=,args=` output into `(pid, args)` pairs.
fn ps_entries(ps: &str) -> impl Iterator<Item = (u32, &str)> {
    ps.lines().filter_map(|l| {
        let (pid, args) = l.trim().split_once(' ')?;
        Some((pid.parse().ok()?, args.trim_start()))
    })
}

/// The Antigravity language_server processes in `ps -axo pid=,args=` output.
fn language_servers(ps: &str) -> Vec<(u32, String)> {
    ps_entries(ps)
        .filter(|(_, args)| args.contains("Antigravity.app/Contents/Resources/bin/language_server"))
        .map(|(pid, args)| (pid, args.to_string()))
        .collect()
}

/// The agy-statusline sidecar processes (`agy-statusline sidecar`) in `ps -axo pid=,args=` output.
fn sidecar_pids(ps: &str) -> Vec<u32> {
    ps_entries(ps)
        .filter(|(_, args)| {
            let mut it = args.split_whitespace();
            let exe = it.next().unwrap_or_default();
            let name = exe.rsplit(['/', '\\']).next().unwrap_or_default();
            matches!(name, "agy-statusline" | "agy-statusline.exe") && it.next() == Some("sidecar")
        })
        .map(|(pid, _)| pid)
        .collect()
}

/// A language_server discovery file (`<app data>/daemon/hub_<port>.json`).
#[derive(Debug, PartialEq)]
struct DaemonInfo {
    pid: u32,
    http_port: u16,
    csrf: String,
}

fn parse_daemon_file(json: &str) -> Option<DaemonInfo> {
    let v: Value = serde_json::from_str(json).ok()?;
    let pid = u32::try_from(v.get("pid")?.as_u64()?).ok()?;
    let http_port = u16::try_from(v.get("httpPort")?.as_u64()?).ok().filter(|p| *p != 0)?;
    let csrf = v.get("csrfToken")?.as_str()?.trim();
    (!csrf.is_empty()).then(|| DaemonInfo { pid, http_port, csrf: csrf.to_string() })
}

/// Parsed discovery files in `dir`, newest first.
fn daemon_files(dir: &Path) -> Vec<DaemonInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(SystemTime, DaemonInfo)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, parse_daemon_file(&std::fs::read_to_string(e.path()).ok()?)?))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, info)| info).collect()
}

/// The environment of a process owned by this user: `/proc/<pid>/environ` (NUL-separated) on
/// Linux, otherwise `ps -wwE`, which prints the arguments followed by `KEY=value` words.
fn process_env(pid: u32) -> Option<String> {
    if let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) {
        return Some(String::from_utf8_lossy(&raw).into_owned());
    }
    let out = Command::new("ps").args(["-wwE", "-o", "args=", "-p", &pid.to_string()]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The value of `key` in an environment listing split on whitespace or NUL. The values looked
/// up here (an address and a token) contain neither.
fn env_value<'a>(env: &'a str, key: &str) -> Option<&'a str> {
    env.split(|c: char| c.is_whitespace() || c == '\0')
        .filter_map(|w| w.strip_prefix(key)?.strip_prefix('='))
        .find(|v| !v.is_empty())
}

/// `addr` if it is `host:port` with a loopback host (`localhost`, `127.x.x.x` or `[::1]`).
fn loopback_addr(addr: &str) -> Option<&str> {
    let addr = addr.trim();
    let (host, port) = addr.rsplit_once(':')?;
    port.parse::<u16>().ok().filter(|p| *p != 0)?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .is_some_and(|h| h.parse::<Ipv6Addr>().is_ok_and(|ip| ip.is_loopback()));
    loopback.then_some(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS_OLD: &str = "  311 /Applications/Antigravity.app/Contents/Resources/bin/language_server_macos_arm --enable_lsp --csrf_token abc-123 --extension_server_port 6100\n";
    const PS_NEW: &str = concat!(
        "    1 /sbin/launchd\n",
        "42095 /Applications/Antigravity.app/Contents/Resources/bin/language_server --standalone ",
        "--subclient_type hub --https_server_port 0 --host_bridge_url=http://127.0.0.1:51740 ",
        "--read_host_bridge_token_from_stdin\n",
        "42111 ../../bin/agy-statusline sidecar\n",
        "50000 /usr/local/bin/agy-statusline status\n",
        "50001 vim agy-statusline sidecar\n",
    );

    #[test]
    fn old_language_server_csrf_from_args() {
        let servers = language_servers(PS_OLD);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, 311);
        assert_eq!(arg_value(&servers[0].1, "--csrf_token").as_deref(), Some("abc-123"));
        assert_eq!(arg_value("x --csrf_token=t0k y", "--csrf_token").as_deref(), Some("t0k"));
    }

    #[test]
    fn new_language_server_has_no_csrf_arg() {
        let servers = language_servers(PS_NEW);
        assert_eq!(servers.iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec![42095]);
        assert_eq!(arg_value(&servers[0].1, "--csrf_token"), None);
    }

    #[test]
    fn finds_only_sidecar_processes() {
        assert_eq!(sidecar_pids(PS_NEW), vec![42111]);
        assert_eq!(sidecar_pids("7 C:\\Tools\\agy-statusline.exe sidecar\n"), vec![7]);
        assert!(sidecar_pids(PS_OLD).is_empty());
    }

    #[test]
    fn parses_daemon_file() {
        let json = r#"{"pid":42095,"httpsPort":51741,"httpPort":51742,"lsVersion":"2.35.2","csrfToken":"tok"}"#;
        assert_eq!(parse_daemon_file(json), Some(DaemonInfo { pid: 42095, http_port: 51742, csrf: "tok".into() }));
        assert_eq!(parse_daemon_file(r#"{"pid":1,"httpPort":51742,"csrfToken":""}"#), None);
        assert_eq!(parse_daemon_file(r#"{"pid":1,"httpPort":0,"csrfToken":"t"}"#), None);
        assert_eq!(parse_daemon_file(r#"{"pid":1,"httpPort":70000,"csrfToken":"t"}"#), None);
        assert_eq!(parse_daemon_file(r#"{"pid":1,"csrfToken":"t"}"#), None);
        assert_eq!(parse_daemon_file("not json"), None);
    }

    #[test]
    fn reads_env_values_from_ps_and_proc() {
        let ps = "../../bin/agy-statusline sidecar PATH=/usr/bin ANTIGRAVITY_LS_ADDRESS=localhost:51742 \
                  ANTIGRAVITY_SOURCE_METADATA={\"a\": 1} ANTIGRAVITY_CSRF_TOKEN=t-1\n";
        assert_eq!(env_value(ps, "ANTIGRAVITY_LS_ADDRESS"), Some("localhost:51742"));
        assert_eq!(env_value(ps, "ANTIGRAVITY_CSRF_TOKEN"), Some("t-1"));
        assert_eq!(env_value(ps, "ANTIGRAVITY_LS"), None);
        let proc = "HOME=/home/u\0ANTIGRAVITY_CSRF_TOKEN=t-2\0ANTIGRAVITY_LS_ADDRESS=127.0.0.1:9\0";
        assert_eq!(env_value(proc, "ANTIGRAVITY_CSRF_TOKEN"), Some("t-2"));
        assert_eq!(env_value(proc, "ANTIGRAVITY_LS_ADDRESS"), Some("127.0.0.1:9"));
        assert_eq!(env_value("ANTIGRAVITY_CSRF_TOKEN= X=1", "ANTIGRAVITY_CSRF_TOKEN"), None);
    }

    #[test]
    fn accepts_only_loopback_addresses() {
        for ok in ["localhost:51742", "LOCALHOST:1", "127.0.0.1:51742", "127.1.2.3:80", "[::1]:51742"] {
            assert_eq!(loopback_addr(ok), Some(ok), "{ok}");
        }
        for bad in [
            "10.0.0.5:51742",
            "0.0.0.0:51742",
            "example.com:80",
            "localhost",
            "localhost:0",
            "localhost:x",
            "[::2]:1",
            "::1:5",
        ] {
            assert_eq!(loopback_addr(bad), None, "{bad}");
        }
    }
}
