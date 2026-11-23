//! Cross-platform Antigravity debug-port relaunch and false-restart guard.
//!
//! Antigravity 2.33+ only opens its Chromium debugging port when launched with
//! `--remote-debugging-port`. Before prompting or auto-relaunching, this module inspects
//! the running Antigravity main process command line so a temporarily slow or busy port
//! never triggers an accidental restart.
//!
//! Because Antigravity stops plugin sidecars when quitting, the relaunch script detaches:
//! - macOS: `nohup /bin/sh` script in a new process group (`osascript quit` → `pkill` → `open -a`).
//! - Windows: two-stage PowerShell script escaping Antigravity's Job Object via Explorer COM
//!   `ShellExecute` → WMI `Win32_Process.Create` → `Start-Process`, guarded by a named mutex.

use crate::config::{load_config, write_atomic};
use crate::live::{SharedRef, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEBUG_ARG: &str = "--remote-debugging-port=0";
const PROMPT_AFTER: Duration = Duration::from_secs(20);
const COOLDOWN_MS: i64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchFlag {
    Flag,
    NoFlag,
    NotRunning,
    Unknown,
}

impl LaunchFlag {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::NoFlag => "noflag",
            Self::NotRunning => "none",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RelaunchState {
    #[serde(default)]
    prompted_for: String,
    #[serde(default)]
    last_prompt_at: i64,
    #[serde(default)]
    last_relaunch_at: i64,
    #[serde(default)]
    last_flag: String,
}

fn state_path(plugin_data: &Path) -> PathBuf {
    plugin_data.join("relaunch-state.json")
}

fn load_state(plugin_data: &Path) -> RelaunchState {
    fs::read_to_string(state_path(plugin_data)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_state(plugin_data: &Path, st: &RelaunchState) {
    if let Ok(text) = serde_json::to_string_pretty(st) {
        let _ = write_atomic(&state_path(plugin_data), &text);
    }
}

fn log_path(plugin_data: &Path) -> PathBuf {
    let p = plugin_data.join("relaunch.log");
    if fs::metadata(&p).map(|m| m.len()).unwrap_or(0) > 65_536 {
        let _ = fs::write(&p, "");
    }
    p
}

fn append_log(plugin_data: &Path, msg: &str) {
    let _ = fs::create_dir_all(plugin_data);
    let p = log_path(plugin_data);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "{ts} {msg}");
    }
}

/// Environment for child processes without Electron/Antigravity sidecar variables.
#[allow(dead_code)]
fn clean_env() -> HashMap<String, String> {
    const DROP: &[&str] = &["ELECTRON_", "NODE_", "ANTIGRAVITY_", "AGY_", "VSCODE_"];
    env::vars()
        .filter(|(k, _)| {
            let up = k.to_ascii_uppercase();
            !DROP.iter().any(|p| up.starts_with(p))
        })
        .collect()
}

pub fn is_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

pub fn target_app() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = Command::new("/bin/ps").args(["-axo", "args="]).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(idx) = line.find(".app/Contents/MacOS/Antigravity") {
                    let candidate = line[..idx + 4].trim();
                    if Path::new(candidate).exists() {
                        return Some(candidate.to_string());
                    }
                }
            }
        }
        for p in [
            PathBuf::from("/Applications/Antigravity.app"),
            dirs::home_dir().unwrap_or_default().join("Applications/Antigravity.app"),
        ] {
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local).join("Programs").join("antigravity").join("Antigravity.exe");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Inspects running Antigravity main processes to determine whether `--remote-debugging-port` was passed.
pub fn started_with_flag(target: &str) -> LaunchFlag {
    if target.is_empty() {
        return LaunchFlag::Unknown;
    }
    #[cfg(target_os = "macos")]
    {
        let main = format!("{target}/Contents/MacOS/Antigravity");
        let Ok(out) = Command::new("/bin/ps").args(["-axo", "args="]).output() else {
            return LaunchFlag::Unknown;
        };
        if !out.status.success() {
            return LaunchFlag::Unknown;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mains: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| (*l == main || l.starts_with(&format!("{main} "))) && !l.contains("--type="))
            .collect();
        if mains.is_empty() {
            return LaunchFlag::NotRunning;
        }
        if mains.iter().any(|l| !has_debug_port_flag(l)) { LaunchFlag::NoFlag } else { LaunchFlag::Flag }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let ps = "$t = $env:AGY_RELAUNCH_TARGET; $n = [IO.Path]::GetFileName($t); \
            $m = @(Get-CimInstance Win32_Process -Filter (\"Name='\" + $n + \"'\") | \
            Where-Object { $_.ExecutablePath -eq $t -and $_.CommandLine -notmatch '--type=|--import=' }); \
            if (-not $m.Count) { 'none' } \
            elseif (@($m | Where-Object { $_.CommandLine -notmatch '--remote-debugging-port' }).Count) { 'noflag' } \
            else { 'flag' }";
        let mut cmd = Command::new(powershell_exe());
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", ps])
            .env_clear()
            .envs(clean_env())
            .env("AGY_RELAUNCH_TARGET", target)
            .creation_flags(CREATE_NO_WINDOW);
        let Ok(out) = cmd.output() else {
            return LaunchFlag::Unknown;
        };
        if !out.status.success() {
            return LaunchFlag::Unknown;
        }
        match String::from_utf8_lossy(&out.stdout).trim() {
            "flag" => LaunchFlag::Flag,
            "noflag" => LaunchFlag::NoFlag,
            "none" => LaunchFlag::NotRunning,
            _ => LaunchFlag::Unknown,
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        LaunchFlag::Unknown
    }
}

#[cfg(any(target_os = "macos", test))]
fn has_debug_port_flag(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|tok| tok == "--remote-debugging-port" || tok.starts_with("--remote-debugging-port="))
}

#[cfg(target_os = "windows")]
fn powershell_exe() -> PathBuf {
    let root = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    PathBuf::from(root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe")
}

#[cfg(target_os = "windows")]
const WIN_SCRIPT: &str = r#"# agy-statusline: restart Antigravity with --remote-debugging-port=0
param(
  [string]$Stage = 'launch',
  [string]$Target,
  [string]$Log,
  [string]$Token = ''
)
$ErrorActionPreference = 'Stop'

function Write-Note([string]$Text) {
  try { [IO.File]::AppendAllText($Log, (Get-Date -Format 'yyyy-MM-dd HH:mm:ss') + ' [' + $Stage + '] ' + $Text + [Environment]::NewLine) } catch { }
}
function Get-MarkerPath([string]$Id, [string]$Kind = 'started') {
  [IO.Path]::Combine([IO.Path]::GetDirectoryName($Log), 'relaunch-' + $Id + '.' + $Kind)
}

$selfPath = $MyInvocation.MyCommand.Path
$powershell = [IO.Path]::Combine($PSHOME, 'powershell.exe')

if ($Stage -eq 'launch') {
  try {
    Get-ChildItem -LiteralPath ([IO.Path]::GetDirectoryName($Log)) -Filter 'relaunch-*.*' |
      Where-Object { $_.Name -match '^relaunch-[0-9a-f]+\.(started|done)$' -and $_.LastWriteTime -lt (Get-Date).AddMinutes(-10) } |
      ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
  } catch { }
  $Token = [Guid]::NewGuid().ToString('N')
  $marker = Get-MarkerPath $Token
  $line = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File "' + $selfPath + '" -Stage relaunch -Target "' + $Target + '" -Log "' + $Log + '" -Token ' + $Token
  foreach ($r in @('explorer', 'wmi', 'direct')) {
    try {
      switch ($r) {
        'explorer' {
          $windows = (New-Object -ComObject Shell.Application).Windows()
          $handle = 0
          $desktop = $windows.FindWindowSW(0, 0, 8, [ref]$handle, 1)
          if ($null -eq $desktop) { throw 'desktop window not found' }
          $desktop.Document.Application.ShellExecute($powershell, $line, $PSHOME, 'open', 0)
        }
        'wmi' {
          $result = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = ('"' + $powershell + '" ' + $line) }
          if ($result.ReturnValue -ne 0) { throw ('Win32_Process.Create returned ' + $result.ReturnValue) }
        }
        'direct' {
          Start-Process -FilePath $powershell -ArgumentList $line -WindowStyle Hidden
        }
      }
    } catch {
      Write-Note ($r + ' failed: ' + $_.Exception.Message)
      continue
    }
    $deadline = (Get-Date).AddSeconds(8)
    while (-not [IO.File]::Exists($marker) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
    if ([IO.File]::Exists($marker)) {
      Write-Note ('handed over via ' + $r)
      try { [IO.File]::Delete($marker) } catch { }
      exit 0
    }
    Write-Note ($r + ': no sign of relaunch script after 8s, trying next route')
  }
  Write-Note 'gave up: relaunch script could not be started'
  exit 1
}

trap { Write-Note ('error: ' + $_.Exception.Message); exit 1 }
if ($Token) { try { [IO.File]::WriteAllText((Get-MarkerPath $Token), [string]$PID) } catch { } }
$mutex = New-Object Threading.Mutex($false, 'Local\agy-statusline-relaunch')
$owned = $false
try { $owned = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $owned = $true }
if (-not $owned) {
  Write-Note ('another relaunch script is already running, pid=' + $PID)
  exit 0
}
if ($Token -and [IO.File]::Exists((Get-MarkerPath $Token 'done'))) {
  Write-Note ('relaunch already completed by another copy, pid=' + $PID)
  exit 0
}
foreach ($name in 'ELECTRON_RUN_AS_NODE', 'ELECTRON_NO_ATTACH_CONSOLE', 'NODE_OPTIONS', 'NODE_PATH') {
  [Environment]::SetEnvironmentVariable($name, $null, 'Process')
}

$exeName = [IO.Path]::GetFileName($Target)
function Get-AntigravityProcs {
  @(Get-CimInstance Win32_Process -Filter ("Name='" + $exeName + "'") | Where-Object { $_.ExecutablePath -eq $Target })
}
function Get-AntigravityMain {
  @(Get-AntigravityProcs | Where-Object { $_.CommandLine -notmatch '--type=|--import=' })
}
function Wait-AllGone([int]$Seconds) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  while (@(Get-AntigravityProcs).Count -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 300 }
  Get-AntigravityProcs
}

Start-Sleep -Milliseconds 500
$main = @(Get-AntigravityMain)
Write-Note ('asking Antigravity to close, main processes: ' + $main.Count)
foreach ($m in $main) {
  try { [void][Diagnostics.Process]::GetProcessById([int]$m.ProcessId).CloseMainWindow() } catch { }
}
$left = @(Wait-AllGone 15)
if ($left.Count) {
  Write-Note ('still running after 15s (' + $left.Count + '), forcing exit')
  foreach ($m in $left) { try { Stop-Process -Id $m.ProcessId -Force -ErrorAction Stop } catch { } }
  $left = @(Wait-AllGone 5)
  if ($left.Count) { Write-Note ('warning: ' + $left.Count + ' processes still running') }
}
Start-Sleep -Milliseconds 300
try {
  $psi = New-Object Diagnostics.ProcessStartInfo
  $psi.FileName = $Target
  $psi.Arguments = '--remote-debugging-port=0'
  $psi.WorkingDirectory = [IO.Path]::GetDirectoryName($Target)
  $psi.UseShellExecute = $true
  [void][Diagnostics.Process]::Start($psi)
  Write-Note 'Antigravity started with --remote-debugging-port=0'
} catch {
  Write-Note ('start failed: ' + $_.Exception.Message)
}
if ($Token) {
  try { [IO.File]::WriteAllText((Get-MarkerPath $Token 'done'), [string]$PID) } catch { }
  try { [IO.File]::Delete((Get-MarkerPath $Token)) } catch { }
}
Remove-Item -LiteralPath $selfPath -Force -ErrorAction SilentlyContinue
"#;

/// Schedules a detached restart of Antigravity with `--remote-debugging-port=0`.
pub fn relaunch_now(plugin_data: &Path) -> Result<String, String> {
    let target = target_app().ok_or_else(|| "未找到 Antigravity 安装路径".to_string())?;
    let mut st = load_state(plugin_data);
    st.last_relaunch_at = now_ms();
    save_state(plugin_data, &st);
    append_log(plugin_data, &format!("relaunching Antigravity with {DEBUG_ARG}: {target}"));
    let log_file = log_path(plugin_data);

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        let script_path = plugin_data.join("relaunch-antigravity.sh");
        let body = format!(
            "#!/bin/sh\n\
             unset ELECTRON_RUN_AS_NODE ELECTRON_NO_ATTACH_CONSOLE NODE_OPTIONS NODE_PATH\n\
             L() {{ echo \"$(date '+%Y-%m-%d %H:%M:%S') $*\" >> \"$AGY_LOG\"; }}\n\
             L \"script start pid=$$ ppid=$PPID\"\n\
             sleep 0.5\n\
             PAT=\"$AGY_APP/Contents/MacOS/Antigravity\"\n\
             L \"closing, running=$(pgrep -f \"$PAT\" | wc -l | tr -d ' ')\"\n\
             osascript -e \"quit app \\\"$AGY_APP\\\"\" >/dev/null 2>&1\n\
             i=0; while [ $i -lt 40 ] && pgrep -f \"$PAT\" >/dev/null; do sleep 0.25; i=$((i+1)); done\n\
             if pgrep -f \"$PAT\" >/dev/null; then L \"force kill\"; pkill -f \"$PAT\" >/dev/null 2>&1; fi\n\
             i=0; while [ $i -lt 24 ] && pgrep -f \"$PAT\" >/dev/null; do sleep 0.25; i=$((i+1)); done\n\
             sleep 0.5\n\
             open -a \"$AGY_APP\" --args {DEBUG_ARG}\n\
             L \"open exit=$?\"\n\
             rm -f \"$0\"\n"
        );
        let _ = fs::create_dir_all(plugin_data);
        fs::write(&script_path, body).map_err(|e| format!("write restart script: {e}"))?;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "nohup /bin/sh \"$1\" </dev/null >/dev/null 2>&1 &", "sh"])
            .arg(&script_path)
            .env_clear()
            .envs(clean_env())
            .env("AGY_APP", &target)
            .env("AGY_LOG", &log_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        cmd.spawn().map_err(|e| format!("spawn restart script: {e}"))?;
        Ok(target)
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = fs::create_dir_all(plugin_data);
        let script_path = plugin_data.join("relaunch-antigravity.ps1");
        let content = format!("\u{feff}{}", WIN_SCRIPT.replace('\n', "\r\n"));
        fs::write(&script_path, content).map_err(|e| format!("write restart script: {e}"))?;
        let mut cmd = Command::new(powershell_exe());
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
            .arg(&script_path)
            .args(["-Stage", "launch", "-Target", &target, "-Log"])
            .arg(&log_file)
            .env_clear()
            .envs(clean_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        cmd.spawn().map_err(|e| format!("spawn powershell: {e}"))?;
        Ok(target)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (&target, &log_file);
        Err("当前系统暂不支持自动重启，请手动附加 --remote-debugging-port=0 启动 Antigravity".into())
    }
}

#[allow(dead_code)]
const ASK_MESSAGE: &str = "这次 Antigravity 启动时未带调试端口参数（--remote-debugging-port=0），AGY Statusline 的状态栏和中文界面暂时无法注入。\n\n要现在重启 Antigravity 吗？对话记录会保留，正在运行的任务会被中断。\n\n（在侧边栏 Settings 中可修改该行为。）";

fn ask_user_confirm() -> bool {
    #[cfg(target_os = "macos")]
    {
        let escaped = serde_json::to_string(ASK_MESSAGE).unwrap_or_else(|_| "\"Restart Antigravity?\"".into());
        let script = format!(
            "display dialog {escaped} with title \"AGY Statusline\" buttons {{\"稍后\", \"立即重启\"}} default button \"立即重启\" with icon caution"
        );
        let Ok(out) = Command::new("osascript").args(["-e", &script]).env_clear().envs(clean_env()).output() else {
            return false;
        };
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("立即重启")
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let ps = "$r = (New-Object -ComObject WScript.Shell).Popup($env:AGY_PROMPT_MSG, 0, 'AGY Statusline', 1 + 32 + 4096); if ($r -eq 1) { exit 10 } else { exit 0 }";
        let Ok(status) = Command::new(powershell_exe())
            .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", ps])
            .env_clear()
            .envs(clean_env())
            .env("AGY_PROMPT_MSG", ASK_MESSAGE)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
        else {
            return false;
        };
        status.code() == Some(10)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Watches CDP connectivity; when unreachable for >= 20s and `relaunch_mode` is `ask` or `auto`,
/// verifies the Antigravity main process truly lacks `--remote-debugging-port` before acting.
pub fn start_watch(shared: SharedRef, plugin_data: PathBuf) {
    if !is_supported() {
        return;
    }
    thread::spawn(move || {
        let session_key = env::var("ANTIGRAVITY_LS_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("pid-{}", std::process::id()));
        let mut last_check: Option<(Instant, LaunchFlag)> = None;

        loop {
            thread::sleep(Duration::from_secs(5));
            let (port_ok, port_since) = match shared.0.lock() {
                Ok(s) => (s.cdp_port_ok, s.cdp_port_since),
                Err(_) => continue,
            };
            if port_ok != Some(false) || port_since.is_none_or(|t| t.elapsed() < PROMPT_AFTER) {
                continue;
            }
            let mode = load_config(&plugin_data).relaunch_mode;
            if mode == "never" {
                continue;
            }
            let st = load_state(&plugin_data);
            if st.prompted_for == session_key {
                continue;
            }
            if let Some((at, prev_flag)) = last_check
                && prev_flag != LaunchFlag::NoFlag
                && at.elapsed() < Duration::from_secs(60)
            {
                continue;
            }
            let Some(target) = target_app() else { continue };
            let flag = started_with_flag(&target);
            let prev_same = last_check.is_some_and(|(_, f)| f == flag);
            last_check = Some((Instant::now(), flag));

            let mut st = load_state(&plugin_data);
            if st.last_flag != flag.as_str() {
                st.last_flag = flag.as_str().to_string();
                save_state(&plugin_data, &st);
            }

            if flag != LaunchFlag::NoFlag {
                if !prev_same {
                    append_log(
                        &plugin_data,
                        &format!(
                            "debug port unreachable, but process check returned '{}'; skipping relaunch",
                            flag.as_str()
                        ),
                    );
                }
                continue;
            }

            // Re-check port health after running the process check.
            let still_down = shared.0.lock().map(|s| s.cdp_port_ok == Some(false)).unwrap_or(false);
            if !still_down {
                continue;
            }
            let mode = load_config(&plugin_data).relaunch_mode;
            if mode == "never" {
                continue;
            }

            let now = now_ms();
            let recent = now - st.last_relaunch_at < COOLDOWN_MS;
            st.prompted_for = session_key.clone();
            st.last_prompt_at = now;
            save_state(&plugin_data, &st);

            append_log(
                &plugin_data,
                &format!(
                    "no debug port & main process missing flag; mode={mode}{}",
                    if recent && mode == "auto" { " (cooldown: asking instead)" } else { "" }
                ),
            );

            let should_go = if mode == "auto" && !recent { true } else { ask_user_confirm() };
            if should_go {
                if let Err(e) = relaunch_now(&plugin_data) {
                    append_log(&plugin_data, &format!("relaunch failed: {e}"));
                }
            } else {
                append_log(&plugin_data, "relaunch declined by user");
            }
        }
    });
}

pub fn status_json(plugin_data: &Path) -> Value {
    let st = load_state(plugin_data);
    json!({
        "supported": is_supported(),
        "target": target_app(),
        "lastRelaunchAt": st.last_relaunch_at,
        "lastFlag": st.last_flag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_debug_port_flag_variants() {
        assert!(has_debug_port_flag(
            "/Applications/Antigravity.app/Contents/MacOS/Antigravity --remote-debugging-port=0"
        ));
        assert!(has_debug_port_flag(
            "/Applications/Antigravity.app/Contents/MacOS/Antigravity --remote-debugging-port 9222"
        ));
        assert!(!has_debug_port_flag("/Applications/Antigravity.app/Contents/MacOS/Antigravity --user-data-dir=/tmp"));
    }
}
