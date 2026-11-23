//! Plugin self-update (GitHub Releases + SHA256SUMS verification) and optional online sync
//! of `ncdeng/agy-zh-pack`'s dictionary, rules, and official model price table.
//!
//! Safety guarantees:
//! - Release archives are verified against the release's `SHA256SUMS` before extraction.
//! - Archive entries are validated against path traversal (`..` / absolute paths).
//! - Dev checkouts (plugin root containing `.git`) can check for releases but refuse overwrite.
//! - Current files are backed up to `plugin_data/backup/v<old>/` and restored on any error.
//! - On Windows, the running `.exe` is renamed to `.exe.old` before moving the new binary into
//!   place, then cleaned up on the next startup.

use crate::config::{OnlinePrices, load_config, write_atomic};
use crate::live::{SharedRef, now_ms};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

const REPO: &str = "BlueOcean223/agy-statusline";
const UPSTREAM_DATA_REPO: &str = "ncdeng/agy-zh-pack";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MIN_DICT_ENTRIES: usize = 2400;
const MIN_RULE_ENTRIES: usize = 45;

const DATA_MIRRORS: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/ncdeng/agy-zh-pack@main/",
    "https://fastly.jsdelivr.net/gh/ncdeng/agy-zh-pack@main/",
    "https://gcore.jsdelivr.net/gh/ncdeng/agy-zh-pack@main/",
    "https://raw.githubusercontent.com/ncdeng/agy-zh-pack/main/",
];

const BUNDLED_DICT: &str = include_str!("../i18n/zh-CN.json");

// ---------------------------------------------------------------- persisted status

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub latest: String,
    pub published_at: String,
    pub html_url: String,
    pub notes: String,
    pub asset_name: String,
    pub asset_url: String,
    pub sha256_url: String,
    pub checked_at: i64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InstallState {
    /// idle | downloading | verifying | installing | installed | failed
    pub status: String,
    pub version: String,
    pub at_ms: i64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DataSyncState {
    pub synced_at: i64,
    pub source_host: String,
    pub commit: String,
    pub dict_entries: usize,
    pub rule_entries: usize,
    pub prices_updated: String,
    pub prices_models: usize,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Default)]
struct UpdaterRuntime {
    release: ReleaseInfo,
    install: InstallState,
    data_sync: DataSyncState,
    preferred_mirror: String,
    checking_release: bool,
    installing: bool,
    syncing_data: bool,
}

static RUNTIME: OnceLock<Arc<Mutex<UpdaterRuntime>>> = OnceLock::new();

fn runtime() -> &'static Arc<Mutex<UpdaterRuntime>> {
    RUNTIME.get_or_init(|| {
        Arc::new(Mutex::new(UpdaterRuntime {
            install: InstallState { status: "idle".into(), ..Default::default() },
            ..Default::default()
        }))
    })
}

// ---------------------------------------------------------------- semver & proxy-aware HTTP

pub fn cmp_ver(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> [u32; 3] {
        let mut out = [0u32; 3];
        let trimmed = s.trim().trim_start_matches('v');
        let core = trimmed.split('-').next().unwrap_or(trimmed);
        for (i, part) in core.split('.').take(3).enumerate() {
            out[i] = part.parse().unwrap_or(0);
        }
        out
    };
    parse(a).cmp(&parse(b))
}

fn detect_proxy() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(v) = env::var(key) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("scutil").arg("--proxy").output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut kv = std::collections::HashMap::new();
            for line in text.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    kv.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            if kv.get("HTTPSEnable").map(String::as_str) == Some("1")
                && let (Some(h), Some(p)) = (kv.get("HTTPSProxy"), kv.get("HTTPSPort"))
                && !h.is_empty()
            {
                return Some(format!("http://{h}:{p}"));
            }
            if kv.get("HTTPEnable").map(String::as_str) == Some("1")
                && let (Some(h), Some(p)) = (kv.get("HTTPProxy"), kv.get("HTTPPort"))
                && !h.is_empty()
            {
                return Some(format!("http://{h}:{p}"));
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let reg_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
        if let Ok(out) =
            std::process::Command::new("reg").args(["query", reg_key]).creation_flags(CREATE_NO_WINDOW).output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut enabled = false;
            let mut server = String::new();
            for line in text.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 && parts[0].eq_ignore_ascii_case("ProxyEnable") {
                    enabled = parts[2] == "0x1" || parts[2] == "1";
                }
                if parts.len() >= 3 && parts[0].eq_ignore_ascii_case("ProxyServer") {
                    server = parts[2..].join(" ");
                }
            }
            if enabled && !server.is_empty() {
                // Handle either "host:port" or "http=h:p;https=h:p".
                let chosen = server
                    .split(';')
                    .find_map(|seg| seg.strip_prefix("https=").or_else(|| seg.strip_prefix("http=")))
                    .unwrap_or(&server)
                    .trim();
                if !chosen.is_empty() {
                    return Some(if chosen.contains("://") { chosen.to_string() } else { format!("http://{chosen}") });
                }
            }
        }
    }
    None
}

fn make_agent(timeout_secs: u64) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(12))
        .timeout_read(Duration::from_secs(timeout_secs))
        .timeout_write(Duration::from_secs(12))
        .user_agent(concat!("agy-statusline/", env!("CARGO_PKG_VERSION")));
    if let Some(p) = detect_proxy()
        && let Ok(proxy) = ureq::Proxy::new(&p)
    {
        builder = builder.proxy(proxy);
    }
    builder.build()
}

fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let resp = agent
        .get(url)
        .set("Accept", "application/vnd.github+json, application/json, text/plain, */*")
        .call()
        .map_err(|e| format!("{e}"))?;
    let mut text = String::new();
    resp.into_reader().take(16 * 1024 * 1024).read_to_string(&mut text).map_err(|e| format!("read {url}: {e}"))?;
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

// ---------------------------------------------------------------- plugin root & startup cleanup

pub fn plugin_root() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let bin_dir = exe.parent()?;
    if bin_dir.file_name().and_then(|s| s.to_str()) != Some("bin") {
        return None;
    }
    let root = bin_dir.parent()?;
    root.join("plugin.json").exists().then(|| root.to_path_buf())
}

pub fn is_dev_checkout(root: &Path) -> bool {
    root.join(".git").exists()
}

/// Removes leftover `.old` / `.agy-new` files from a prior Windows/Unix self-update.
pub fn cleanup_old_binaries() {
    let Some(root) = plugin_root() else { return };
    let bin_dir = root.join("bin");
    let Ok(rd) = fs::read_dir(&bin_dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".old") || name.ends_with(".agy-new") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn expected_asset_name(ver: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        Some(format!("agy-statusline-v{ver}-macos-universal.tar.gz"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(format!("agy-statusline-v{ver}-windows-x64.zip"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = ver;
        None
    }
}

// ---------------------------------------------------------------- GitHub Releases check & install

pub fn check_release(plugin_data: &Path) -> Result<ReleaseInfo, String> {
    {
        let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
        if rt.checking_release {
            return Ok(rt.release.clone());
        }
        rt.checking_release = true;
    }

    let res = (|| -> Result<ReleaseInfo, String> {
        let agent = make_agent(20);
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let text = fetch_text(&agent, &url)?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("bad release JSON: {e}"))?;
        let tag = v["tag_name"].as_str().ok_or("release missing tag_name")?;
        let ver = tag.trim_start_matches('v').to_string();
        let html_url = v["html_url"].as_str().unwrap_or("").to_string();
        let published_at = v["published_at"].as_str().unwrap_or("").to_string();
        let notes = v["body"].as_str().unwrap_or("").chars().take(1000).collect();

        let target_asset = expected_asset_name(&ver).unwrap_or_default();
        let mut asset_url = String::new();
        let mut sha256_url = String::new();
        for a in v["assets"].as_array().into_iter().flatten() {
            let name = a["name"].as_str().unwrap_or("");
            let dl = a["browser_download_url"].as_str().unwrap_or("");
            if !target_asset.is_empty() && name == target_asset {
                asset_url = dl.to_string();
            } else if name == "SHA256SUMS" {
                sha256_url = dl.to_string();
            }
        }
        Ok(ReleaseInfo {
            latest: ver,
            published_at,
            html_url,
            notes,
            asset_name: target_asset,
            asset_url,
            sha256_url,
            checked_at: now_ms(),
            error: None,
        })
    })();

    let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
    rt.checking_release = false;
    match res {
        Ok(info) => {
            rt.release = info.clone();
            if let Ok(s) = serde_json::to_string_pretty(&info) {
                let _ = write_atomic(&plugin_data.join("release-cache.json"), &s);
            }
            Ok(info)
        }
        Err(e) => {
            rt.release.checked_at = now_ms();
            rt.release.error = Some(e.clone());
            Err(e)
        }
    }
}

fn set_install_state(plugin_data: &Path, status: &str, version: &str, error: Option<String>) {
    let st = InstallState { status: status.into(), version: version.into(), at_ms: now_ms(), error };
    if let Ok(mut rt) = runtime().lock() {
        rt.install = st.clone();
    }
    if let Ok(s) = serde_json::to_string_pretty(&st) {
        let _ = write_atomic(&plugin_data.join("update-state.json"), &s);
    }
}

fn parse_sha256sums(text: &str, asset_name: &str) -> Option<String> {
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        if name == asset_name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn safe_rel_path(p: &Path) -> Option<PathBuf> {
    let mut clean = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(s) => clean.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dest_dir).map_err(|e| format!("create extract dir: {e}"))?;
    let name = archive_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".tar.gz") {
        let f = File::open(archive_path).map_err(|e| format!("open tar.gz: {e}"))?;
        let gz = flate2::read::GzDecoder::new(f);
        let mut ar = tar::Archive::new(gz);
        for entry in ar.entries().map_err(|e| format!("read tar entries: {e}"))? {
            let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
            let rel = entry
                .path()
                .ok()
                .and_then(|p| safe_rel_path(&p))
                .ok_or_else(|| "unsafe path inside tar.gz".to_string())?;
            let out_path = dest_dir.join(&rel);
            if entry.header().entry_type().is_dir() {
                fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            } else if entry.header().entry_type().is_file() {
                if let Some(parent) = out_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut out_file = File::create(&out_path).map_err(|e| e.to_string())?;
                io::copy(&mut entry, &mut out_file).map_err(|e| e.to_string())?;
            }
        }
    } else if name.ends_with(".zip") {
        let f = File::open(archive_path).map_err(|e| format!("open zip: {e}"))?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("parse zip: {e}"))?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| format!("zip entry: {e}"))?;
            let rel = entry
                .enclosed_name()
                .and_then(|p| safe_rel_path(&p))
                .ok_or_else(|| "unsafe path inside zip".to_string())?;
            let out_path = dest_dir.join(&rel);
            if entry.is_dir() {
                fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            } else {
                if let Some(parent) = out_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut out_file = File::create(&out_path).map_err(|e| e.to_string())?;
                io::copy(&mut entry, &mut out_file).map_err(|e| e.to_string())?;
            }
        }
    } else {
        return Err(format!("unsupported archive format: {name}"));
    }

    let root = dest_dir.join("agy-statusline");
    if root.join("plugin.json").exists() {
        Ok(root)
    } else if dest_dir.join("plugin.json").exists() {
        Ok(dest_dir.to_path_buf())
    } else {
        Err("发布包中未找到 plugin.json".into())
    }
}

fn collect_files(base: &Path, cur: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(cur)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            collect_files(base, &p, out)?;
        } else if p.is_file()
            && let Ok(rel) = p.strip_prefix(base)
        {
            out.push(rel.to_path_buf());
        }
    }
    Ok(())
}

/// Downloads, SHA-256 verifies, backs up, and installs the latest GitHub Release, then exits so
/// Antigravity restarts the sidecar with the new binary.
pub fn start_install_release(plugin_data: PathBuf) -> Result<String, String> {
    let root = plugin_root().ok_or_else(|| "未识别到插件安装目录，无法原地更新".to_string())?;
    if is_dev_checkout(&root) {
        return Err("当前运行在开发仓库目录（含 .git），请使用 git pull && cargo build --release 更新".into());
    }
    let rel = {
        let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
        if rt.installing {
            return Err("更新正在进行中".into());
        }
        let info = rt.release.clone();
        if info.latest.is_empty() || cmp_ver(&info.latest, CURRENT_VERSION) != std::cmp::Ordering::Greater {
            return Err("当前已是最新版本，请先检查更新".into());
        }
        if info.asset_url.is_empty() || info.sha256_url.is_empty() {
            return Err("该版本缺少当前平台的安装包或 SHA256SUMS 校验文件".into());
        }
        rt.installing = true;
        info
    };

    let ver = rel.latest.clone();
    set_install_state(&plugin_data, "downloading", &ver, None);

    thread::spawn(move || {
        let res = do_install_release(&plugin_data, &root, &rel);
        if let Ok(mut rt) = runtime().lock() {
            rt.installing = false;
        }
        match res {
            Ok(()) => {
                set_install_state(&plugin_data, "installed", &rel.latest, None);
                eprintln!("[agy-statusline] updated v{CURRENT_VERSION} -> v{}, restarting sidecar", rel.latest);
                thread::sleep(Duration::from_millis(1500));
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[agy-statusline] update failed: {e}");
                set_install_state(&plugin_data, "failed", &rel.latest, Some(e));
            }
        }
    });

    Ok(ver)
}

fn do_install_release(plugin_data: &Path, root: &Path, rel: &ReleaseInfo) -> Result<(), String> {
    let agent = make_agent(120);
    let stage = plugin_data.join("update").join(format!("v{}", rel.latest));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage).map_err(|e| format!("create stage dir: {e}"))?;

    let result = (|| -> Result<(), String> {
        let sums_text = fetch_text(&agent, &rel.sha256_url).map_err(|e| format!("下载 SHA256SUMS 失败: {e}"))?;
        let expected_hash =
            parse_sha256sums(&sums_text, &rel.asset_name).ok_or("SHA256SUMS 中未找到对应安装包的校验和")?;

        let archive_path = stage.join(&rel.asset_name);
        let resp = agent.get(&rel.asset_url).call().map_err(|e| format!("下载安装包失败: {e}"))?;
        let mut reader = resp.into_reader().take(MAX_ARCHIVE_BYTES + 1);
        let mut file = File::create(&archive_path).map_err(|e| format!("write archive: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 32 * 1024];
        let mut total = 0u64;
        loop {
            let n = reader.read(&mut buf).map_err(|e| format!("download stream: {e}"))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_ARCHIVE_BYTES {
                return Err("安装包超出大小限制（64MB）".into());
            }
            hasher.update(&buf[..n]);
            io::Write::write_all(&mut file, &buf[..n]).map_err(|e| format!("write archive: {e}"))?;
        }
        drop(file);

        set_install_state(plugin_data, "verifying", &rel.latest, None);
        let got_hash = format!("{:x}", hasher.finalize());
        if got_hash != expected_hash {
            return Err(format!("SHA256 校验失败（期望 {expected_hash}，实际 {got_hash}）"));
        }

        let extracted_root = extract_archive(&archive_path, &stage.join("extracted"))?;
        let mut files = Vec::new();
        collect_files(&extracted_root, &extracted_root, &mut files).map_err(|e| format!("scan extracted: {e}"))?;
        let bin_rel = if cfg!(target_os = "windows") {
            PathBuf::from("bin/agy-statusline.exe")
        } else {
            PathBuf::from("bin/agy-statusline")
        };
        if !files.contains(&bin_rel) {
            return Err(format!("安装包缺少可执行文件 {}", bin_rel.display()));
        }

        set_install_state(plugin_data, "installing", &rel.latest, None);
        let backup = plugin_data.join("backup").join(format!("v{CURRENT_VERSION}"));
        let _ = fs::remove_dir_all(&backup);
        fs::create_dir_all(&backup).map_err(|e| format!("create backup dir: {e}"))?;
        for rel_p in &files {
            let cur = root.join(rel_p);
            if cur.exists() {
                let b = backup.join(rel_p);
                if let Some(parent) = b.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                fs::copy(&cur, &b).map_err(|e| format!("backup {}: {e}", rel_p.display()))?;
            }
        }

        let mut replaced: Vec<PathBuf> = Vec::new();
        let replace_res = (|| -> Result<(), String> {
            for rel_p in &files {
                let src = extracted_root.join(rel_p);
                let dst = root.join(rel_p);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                if rel_p == &bin_rel {
                    #[cfg(target_os = "windows")]
                    {
                        let old_bin = dst.with_extension("exe.old");
                        let _ = fs::remove_file(&old_bin);
                        if dst.exists() {
                            fs::rename(&dst, &old_bin).map_err(|e| format!("rename running exe: {e}"))?;
                        }
                        fs::copy(&src, &dst).map_err(|e| format!("copy new exe: {e}"))?;
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let tmp = dst.with_extension("agy-new");
                        fs::copy(&src, &tmp).map_err(|e| format!("copy new bin: {e}"))?;
                        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755));
                        fs::rename(&tmp, &dst).map_err(|e| format!("replace bin: {e}"))?;
                    }
                } else {
                    let tmp = dst.with_file_name(format!(
                        "{}.agy-new",
                        dst.file_name().and_then(|s| s.to_str()).unwrap_or("file")
                    ));
                    fs::copy(&src, &tmp).map_err(|e| format!("copy {}: {e}", rel_p.display()))?;
                    fs::rename(&tmp, &dst).map_err(|e| format!("replace {}: {e}", rel_p.display()))?;
                }
                replaced.push(rel_p.clone());
            }
            Ok(())
        })();

        if let Err(e) = replace_res {
            for rel_p in &replaced {
                let b = backup.join(rel_p);
                let dst = root.join(rel_p);
                if b.exists() {
                    let _ = fs::copy(&b, &dst);
                }
            }
            return Err(format!("替换文件失败（已回滚）：{e}"));
        }
        Ok(())
    })();

    let _ = fs::remove_dir_all(&stage);
    result
}

// ---------------------------------------------------------------- online dict & prices sync

fn at_ref(base: &str, sha: &str) -> String {
    if let Some(p) = base.strip_suffix("@main/") {
        format!("{p}@{sha}/")
    } else if let Some(p) = base.strip_suffix("/main/") {
        format!("{p}/{sha}/")
    } else {
        base.to_string()
    }
}

fn host_of(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|s| s.split('/').next())
        .unwrap_or(url)
        .to_string()
}

fn validate_dict(text: &str) -> Result<BTreeMap<String, String>, String> {
    let map: BTreeMap<String, String> = serde_json::from_str(text).map_err(|e| format!("bad dict JSON: {e}"))?;
    if map.len() < MIN_DICT_ENTRIES {
        return Err(format!("词典条数异常（仅 {} 条）", map.len()));
    }
    Ok(map)
}

fn validate_rules(text: &str) -> Result<Vec<(String, String)>, String> {
    let raw: Vec<(String, String)> = serde_json::from_str(text).map_err(|e| format!("bad rules JSON: {e}"))?;
    if raw.len() < MIN_RULE_ENTRIES {
        return Err(format!("规则条数异常（仅 {} 条）", raw.len()));
    }
    let valid: Vec<(String, String)> = raw.into_iter().filter(|(pat, _)| Regex::new(pat).is_ok()).collect();
    if valid.len() < MIN_RULE_ENTRIES {
        return Err("有效正则规则过少".into());
    }
    Ok(valid)
}

fn validate_prices(text: &str) -> Result<OnlinePrices, String> {
    let p: OnlinePrices = serde_json::from_str(text).map_err(|e| format!("bad prices JSON: {e}"))?;
    if p.models.len() < 5 {
        return Err(format!("价格表模型过少（仅 {} 个）", p.models.len()));
    }
    for (k, m) in &p.models {
        if m.t.is_empty() {
            return Err(format!("价格表模型 {k} 缺少档位"));
        }
        for (_, r) in &m.t {
            for val in [r.input, r.cache, r.out, r.cw.unwrap_or(0.0)] {
                if !val.is_finite() || !(0.0..=500.0).contains(&val) {
                    return Err(format!("价格表模型 {k} 单价超出合理范围"));
                }
            }
        }
    }
    Ok(p)
}

fn merge_dict_with_bundled(remote: &BTreeMap<String, String>) -> Result<String, String> {
    let mut base: BTreeMap<String, String> =
        serde_json::from_str(BUNDLED_DICT).map_err(|e| format!("bundled dict: {e}"))?;
    for (k, v) in remote {
        base.insert(k.clone(), v.clone());
    }
    serde_json::to_string(&base).map_err(|e| e.to_string())
}

/// Applies cached remote dict/rules/prices (or clears them when `online_data_sync` is disabled).
pub fn apply_remote_cache_to_shared(plugin_data: &Path, shared: &SharedRef, enabled: bool) {
    if !enabled {
        if let Ok(mut s) = shared.0.lock() {
            let changed = s.i18n_dict.is_some() || s.i18n_rules.is_some() || s.online_prices.is_some();
            s.i18n_dict = None;
            s.i18n_rules = None;
            s.online_prices = None;
            if changed {
                s.i18n_rev += 1;
                s.wake = true;
            }
        }
        shared.1.notify_all();
        return;
    }

    let remote_dir = plugin_data.join("remote");
    let prices = fs::read_to_string(remote_dir.join("prices.json")).ok().and_then(|t| validate_prices(&t).ok());
    let dict_json = fs::read_to_string(remote_dir.join("zh-CN.json"))
        .ok()
        .and_then(|t| validate_dict(&t).ok())
        .and_then(|m| merge_dict_with_bundled(&m).ok());
    let rules_json = fs::read_to_string(remote_dir.join("zh-CN.rules.json"))
        .ok()
        .and_then(|t| validate_rules(&t).ok())
        .and_then(|r| serde_json::to_string(&r).ok());

    if let Ok(mut s) = shared.0.lock() {
        let i18n_changed = s.i18n_dict != dict_json || s.i18n_rules != rules_json;
        s.online_prices = prices;
        s.i18n_dict = dict_json;
        s.i18n_rules = rules_json;
        if i18n_changed {
            s.i18n_rev += 1;
        }
        s.wake = true;
    }
    shared.1.notify_all();
}

/// Fetches latest `zh-CN.json`, `zh-CN.rules.json`, and `prices.json` from `ncdeng/agy-zh-pack`
/// pinned to the latest `main` commit SHA to avoid stale CDN mirrors.
pub fn sync_remote_data(plugin_data: &Path, shared: &SharedRef) -> Result<DataSyncState, String> {
    {
        let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
        if rt.syncing_data {
            return Ok(rt.data_sync.clone());
        }
        rt.syncing_data = true;
    }

    let preferred = runtime().lock().map(|r| r.preferred_mirror.clone()).unwrap_or_default();
    let res = (|| -> Result<(DataSyncState, String), String> {
        let agent = make_agent(20);
        let mut order: Vec<&str> = DATA_MIRRORS.to_vec();
        if let Some(pos) = order.iter().position(|&b| b == preferred) {
            let p = order.remove(pos);
            order.insert(0, p);
        }

        // 1. Ask GitHub API for latest main commit SHA.
        let commit_sha: Option<String> =
            fetch_text(&agent, &format!("https://api.github.com/repos/{UPSTREAM_DATA_REPO}/git/ref/heads/main"))
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v["object"]["sha"].as_str().map(str::to_string))
                .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()));

        // 2. Choose mirror + base URL (commit-pinned if SHA succeeded, else branch fallback).
        let mut chosen: Option<(&str, String)> = None;
        if let Some(ref sha) = commit_sha {
            for &b in &order {
                let pinned = at_ref(b, sha);
                if fetch_text(&agent, &format!("{pinned}data/meta.json")).is_ok() {
                    chosen = Some((b, pinned));
                    break;
                }
            }
        }
        if chosen.is_none() {
            // Fallback: pick mirror with the freshest data/meta.json.
            let mut best: Option<(&str, String, String)> = None;
            for &b in &order {
                if let Ok(t) = fetch_text(&agent, &format!("{b}data/meta.json"))
                    && let Ok(v) = serde_json::from_str::<Value>(&t)
                {
                    let date = v["date"].as_str().unwrap_or("").to_string();
                    let latest = v["latest"].as_str().unwrap_or("").to_string();
                    let newer = match &best {
                        None => true,
                        Some((_, bd, bl)) => date > *bd || (date == *bd && cmp_ver(&latest, bl).is_gt()),
                    };
                    if newer {
                        best = Some((b, date, latest));
                    }
                }
            }
            if let Some((b, _, _)) = best {
                chosen = Some((b, b.to_string()));
            }
        }

        let (mirror_base, file_base) = chosen.ok_or("所有在线镜像均无法连接")?;
        let dict_text = fetch_text(&agent, &format!("{file_base}i18n/zh-CN.json"))?;
        let rules_text = fetch_text(&agent, &format!("{file_base}i18n/zh-CN.rules.json"))?;
        let prices_text = fetch_text(&agent, &format!("{file_base}data/prices.json"))?;

        let dict_map = validate_dict(&dict_text)?;
        let rules_vec = validate_rules(&rules_text)?;
        let prices = validate_prices(&prices_text)?;

        let remote_dir = plugin_data.join("remote");
        write_atomic(&remote_dir.join("zh-CN.json"), &dict_text).map_err(|e| e.to_string())?;
        write_atomic(&remote_dir.join("zh-CN.rules.json"), &rules_text).map_err(|e| e.to_string())?;
        write_atomic(&remote_dir.join("prices.json"), &prices_text).map_err(|e| e.to_string())?;

        let state = DataSyncState {
            synced_at: now_ms(),
            source_host: host_of(mirror_base),
            commit: commit_sha.map(|s| s[..7].to_string()).unwrap_or_else(|| "main".into()),
            dict_entries: dict_map.len(),
            rule_entries: rules_vec.len(),
            prices_updated: prices.updated,
            prices_models: prices.models.len(),
            error: None,
        };
        if let Ok(s) = serde_json::to_string_pretty(&state) {
            let _ = write_atomic(&remote_dir.join("sync-state.json"), &s);
        }
        Ok((state, mirror_base.to_string()))
    })();

    let mut rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
    rt.syncing_data = false;
    match res {
        Ok((st, mirror)) => {
            rt.preferred_mirror = mirror;
            rt.data_sync = st.clone();
            drop(rt);
            apply_remote_cache_to_shared(plugin_data, shared, true);
            Ok(st)
        }
        Err(e) => {
            rt.data_sync.error = Some(e.clone());
            Err(e)
        }
    }
}

/// Loads persisted caches immediately and launches the 6-hour background update & sync loop.
pub fn start_background(plugin_data: PathBuf, shared: SharedRef) {
    cleanup_old_binaries();

    // Restore cached release/sync metadata for instant UI display.
    if let Ok(text) = fs::read_to_string(plugin_data.join("release-cache.json"))
        && let Ok(rel) = serde_json::from_str::<ReleaseInfo>(&text)
        && let Ok(mut rt) = runtime().lock()
    {
        rt.release = rel;
    }
    if let Ok(text) = fs::read_to_string(plugin_data.join("remote/sync-state.json"))
        && let Ok(ds) = serde_json::from_str::<DataSyncState>(&text)
        && let Ok(mut rt) = runtime().lock()
    {
        rt.data_sync = ds;
    }
    if let Ok(text) = fs::read_to_string(plugin_data.join("update-state.json"))
        && let Ok(mut st) = serde_json::from_str::<InstallState>(&text)
        && let Ok(mut rt) = runtime().lock()
    {
        if matches!(st.status.as_str(), "downloading" | "verifying" | "installing") && now_ms() - st.at_ms > 5 * 60_000
        {
            st.status = "idle".into();
        }
        rt.install = st;
    }

    let cfg = load_config(&plugin_data);
    apply_remote_cache_to_shared(&plugin_data, &shared, cfg.online_data_sync);

    thread::spawn(move || {
        // Brief startup delay so startup stays lightweight.
        thread::sleep(Duration::from_secs(20));
        loop {
            let cfg = load_config(&plugin_data);
            if cfg.online_updates {
                let _ = check_release(&plugin_data);
            }
            if cfg.online_data_sync {
                let _ = sync_remote_data(&plugin_data, &shared);
            }
            thread::sleep(CHECK_INTERVAL);
        }
    });
}

pub fn status_json(plugin_data: &Path) -> Value {
    let cfg = load_config(plugin_data);
    let rt = runtime().lock().unwrap_or_else(|e| e.into_inner());
    let root = plugin_root();
    let dev_checkout = root.as_deref().is_some_and(is_dev_checkout);
    let can_self_update = root.is_some()
        && !dev_checkout
        && cfg!(any(target_os = "macos", target_os = "windows"))
        && !rt.release.asset_url.is_empty()
        && !rt.release.sha256_url.is_empty();
    let available =
        !rt.release.latest.is_empty() && cmp_ver(&rt.release.latest, CURRENT_VERSION) == std::cmp::Ordering::Greater;

    json!({
        "current": CURRENT_VERSION,
        "repo": REPO,
        "onlineUpdates": cfg.online_updates,
        "onlineDataSync": cfg.online_data_sync,
        "available": available,
        "canSelfUpdate": can_self_update,
        "devCheckout": dev_checkout,
        "checking": rt.checking_release,
        "installing": rt.installing,
        "syncingData": rt.syncing_data,
        "release": rt.release,
        "install": rt.install,
        "dataSync": rt.data_sync,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_versions() {
        assert_eq!(cmp_ver("0.3.0", "0.2.0"), std::cmp::Ordering::Greater);
        assert_eq!(cmp_ver("v0.2.0", "0.2.0"), std::cmp::Ordering::Equal);
        assert_eq!(cmp_ver("0.2.1", "0.2.10"), std::cmp::Ordering::Less);
    }

    #[test]
    fn parses_sha256sums_lines() {
        let sums = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  agy-statusline-v0.3.0-macos-universal.tar.gz\n\
                    bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *agy-statusline-v0.3.0-windows-x64.zip\n";
        assert_eq!(
            parse_sha256sums(sums, "agy-statusline-v0.3.0-macos-universal.tar.gz").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            parse_sha256sums(sums, "agy-statusline-v0.3.0-windows-x64.zip").as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert!(parse_sha256sums(sums, "missing.zip").is_none());
    }

    #[test]
    fn rejects_path_traversal_entries() {
        assert!(safe_rel_path(Path::new("../evil")).is_none());
        assert!(safe_rel_path(Path::new("/etc/passwd")).is_none());
        assert_eq!(
            safe_rel_path(Path::new("agy-statusline/plugin.json")),
            Some(PathBuf::from("agy-statusline/plugin.json"))
        );
    }

    #[test]
    fn rewrites_mirror_urls_to_commit_sha() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            at_ref("https://cdn.jsdelivr.net/gh/ncdeng/agy-zh-pack@main/", sha),
            format!("https://cdn.jsdelivr.net/gh/ncdeng/agy-zh-pack@{sha}/")
        );
        assert_eq!(
            at_ref("https://raw.githubusercontent.com/ncdeng/agy-zh-pack/main/", sha),
            format!("https://raw.githubusercontent.com/ncdeng/agy-zh-pack/{sha}/")
        );
    }
}
