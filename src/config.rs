use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// Price rates (USD per 1M tokens) effective before `until_ms` (or open-ended when `None`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PriceTier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<i64>,
    pub input_per_m: f64,
    pub cache_read_per_m: f64,
    pub cache_write_per_m: f64,
    pub output_per_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRule {
    pub pattern: String,
    pub display_name: String,
    pub short_name: String,
    pub context_limit: u64,
    pub input_per_m: f64,
    pub cache_read_per_m: f64,
    pub cache_write_per_m: f64,
    pub output_per_m: f64,
    /// Optional model key in online `data/prices.json` (e.g. `gemini-4-carbon`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub online_key: String,
    /// Optional date-tiered pricing (evaluated in order against the call's timestamp).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<PriceTier>,
}

impl Default for ModelRule {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            display_name: "Gemini".into(),
            short_name: "Gemini".into(),
            context_limit: 256_000,
            input_per_m: 2.0,
            cache_read_per_m: 0.10,
            cache_write_per_m: 0.0,
            output_per_m: 10.0,
            online_key: String::new(),
            tiers: Vec::new(),
        }
    }
}

impl ModelRule {
    /// Returns `(input, cache_read, cache_write, output)` USD/1M rates active at `at_ms`.
    pub fn rates_at(&self, at_ms: i64, now_ms: i64) -> (f64, f64, f64, f64) {
        if !self.tiers.is_empty() {
            let when = if at_ms > 0 { at_ms } else { now_ms };
            let tier =
                self.tiers.iter().find(|t| t.until_ms.is_none_or(|u| when < u)).or_else(|| self.tiers.last()).unwrap();
            let cw = if tier.cache_write_per_m > 0.0 { tier.cache_write_per_m } else { tier.input_per_m };
            return (tier.input_per_m, tier.cache_read_per_m, cw, tier.output_per_m);
        }
        let cw = if self.cache_write_per_m > 0.0 { self.cache_write_per_m } else { self.input_per_m };
        (self.input_per_m, self.cache_read_per_m, cw, self.output_per_m)
    }
}

/// Online price table matching `agy-zh-pack`'s `data/prices.json` schema.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OnlinePrices {
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub models: HashMap<String, OnlineModelPrice>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OnlineModelPrice {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub t: Vec<(Option<String>, OnlineRate)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OnlineRate {
    #[serde(rename = "in", default)]
    pub input: f64,
    #[serde(default)]
    pub cache: f64,
    #[serde(default)]
    pub cw: Option<f64>,
    #[serde(default)]
    pub out: f64,
}

/// Pacific time midnight (`YYYY-MM-DDT08:00:00Z`) matching Google AI Studio price transition times.
pub fn pt_midnight_ms(date: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(&format!("{date}T08:00:00Z")).ok().map(|d| d.timestamp_millis())
}

impl OnlineModelPrice {
    pub fn to_tiers(&self) -> Vec<PriceTier> {
        self.t
            .iter()
            .map(|(until, r)| PriceTier {
                until_ms: until.as_deref().and_then(pt_midnight_ms),
                input_per_m: r.input,
                cache_read_per_m: r.cache,
                cache_write_per_m: r.cw.unwrap_or(0.0),
                output_per_m: r.out,
            })
            .collect()
    }
}

/// Overlays online `prices.json` onto configured `ModelRule`s, appending any brand-new online models.
pub fn effective_model_rules(base: &[ModelRule], online: Option<&OnlinePrices>) -> Vec<ModelRule> {
    let Some(online) = online else { return base.to_vec() };
    if online.models.is_empty() {
        return base.to_vec();
    }
    let mut known_keys = std::collections::HashSet::new();
    let mut out: Vec<ModelRule> = base
        .iter()
        .map(|r| {
            let mut m = r.clone();
            if !m.online_key.is_empty() {
                known_keys.insert(m.online_key.clone());
                if let Some(op) = online.models.get(&m.online_key) {
                    let tiers = op.to_tiers();
                    if !tiers.is_empty() {
                        m.tiers = tiers;
                    }
                }
            }
            m
        })
        .collect();

    // Extra models published in online prices.json that aren't in our built-in list yet.
    let mut extras: Vec<(&String, &OnlineModelPrice)> =
        online.models.iter().filter(|(k, _)| !known_keys.contains(k.as_str())).collect();
    // Longer IDs first so e.g. `claude-opus-5-5` precedes `claude-opus-5`.
    extras.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
    for (id, op) in extras {
        let tiers = op.to_tiers();
        if tiers.is_empty() {
            continue;
        }
        let title = if op.title.is_empty() { id.clone() } else { op.title.clone() };
        let short = title
            .strip_prefix("Gemini 4 ")
            .or_else(|| title.strip_prefix("Gemini "))
            .or_else(|| title.strip_prefix("Claude "))
            .unwrap_or(&title)
            .to_string();
        let last = tiers.last().unwrap();
        out.push(ModelRule {
            pattern: id.clone(),
            display_name: title,
            short_name: short,
            context_limit: if id.contains("claude") { 200_000 } else { 1_000_000 },
            input_per_m: last.input_per_m,
            cache_read_per_m: last.cache_read_per_m,
            cache_write_per_m: last.cache_write_per_m,
            output_per_m: last.output_per_m,
            online_key: id.clone(),
            tiers,
        });
    }
    out
}

/// Visual appearance settings for the statusline (ccline v2 layered design).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarLook {
    /// cometix | catppuccin | tokyo-night | rose-pine | gruvbox | nord | dracula | mono
    pub palette: String,
    /// plain | pill | powerline
    pub skin: String,
    /// thin | cells | ring | none
    pub bars: String,
    /// compact | full
    pub density: String,
    /// solid | outline | unicode | off
    pub icons: String,
}

impl Default for BarLook {
    fn default() -> Self {
        Self {
            palette: "cometix".into(),
            skin: "plain".into(),
            bars: "thin".into(),
            density: "compact".into(),
            icons: "solid".into(),
        }
    }
}

impl BarLook {
    pub fn sanitize(&mut self) {
        const PALETTES: &[&str] =
            &["cometix", "catppuccin", "tokyo-night", "rose-pine", "gruvbox", "nord", "dracula", "mono"];
        const SKINS: &[&str] = &["plain", "pill", "powerline"];
        const BARS: &[&str] = &["thin", "cells", "ring", "none"];
        const DENSITIES: &[&str] = &["compact", "full"];
        const ICONS: &[&str] = &["solid", "outline", "unicode", "off"];
        if !PALETTES.contains(&self.palette.as_str()) {
            self.palette = "cometix".into();
        }
        if !SKINS.contains(&self.skin.as_str()) {
            self.skin = "plain".into();
        }
        if !BARS.contains(&self.bars.as_str()) {
            self.bars = "thin".into();
        }
        if !DENSITIES.contains(&self.density.as_str()) {
            self.density = "compact".into();
        }
        if !ICONS.contains(&self.icons.as_str()) {
            self.icons = "solid".into();
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 0 means use the limit the language_server reports (`maxContextTokens`).
    pub context_limit_override: u64,
    pub cache_ttl_sec: u64,
    /// Count subagents' calls in a root conversation's token and cost totals (bar and side panel).
    pub include_subagents: bool,
    /// In-page statusline above the chat input (drawn over CDP).
    pub bar_enabled: bool,
    pub bar_segments: BarSegments,
    pub bar_look: BarLook,
    /// Translate Antigravity's UI to Chinese (i18n/, from agy-zh-pack).
    pub ui_zh: bool,
    /// Behavior when Antigravity was launched without `--remote-debugging-port`: never | ask | auto.
    pub relaunch_mode: String,
    /// Periodically check GitHub Releases for new versions of agy-statusline.
    pub online_updates: bool,
    /// Periodically sync i18n dictionary & prices.json from ncdeng/agy-zh-pack.
    pub online_data_sync: bool,
    /// Highlight single model calls at or above this USD threshold (0 disables the alert).
    pub costly_call_usd: f64,
    pub models: Vec<ModelRule>,
}

/// Which parts of the statusline to show (in-page bar and the panel preview).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BarSegments {
    pub model: bool,
    pub git: bool,
    pub context: bool,
    pub cache: bool,
    pub cost: bool,
    pub today: bool,
    pub ttft: bool,
    pub speed: bool,
    pub quota_5h: bool,
    pub quota_7d: bool,
    pub running: bool,
    pub subagents: bool,
}

impl Default for BarSegments {
    fn default() -> Self {
        Self {
            model: true,
            git: true,
            context: true,
            cache: true,
            cost: true,
            today: true,
            ttft: true,
            speed: true,
            quota_5h: true,
            quota_7d: true,
            running: true,
            subagents: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            context_limit_override: 0,
            cache_ttl_sec: 300,
            include_subagents: true,
            bar_enabled: true,
            bar_segments: BarSegments::default(),
            bar_look: BarLook::default(),
            ui_zh: false,
            relaunch_mode: "never".into(),
            online_updates: true,
            online_data_sync: true,
            costly_call_usd: 0.50,
            models: default_model_rules(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ConfigPatch {
    pub context_limit_override: Option<u64>,
    pub cache_ttl_sec: Option<u64>,
    pub include_subagents: Option<bool>,
    pub bar_enabled: Option<bool>,
    pub bar_segments: Option<BarSegments>,
    pub bar_look: Option<BarLook>,
    pub ui_zh: Option<bool>,
    pub relaunch_mode: Option<String>,
    pub online_updates: Option<bool>,
    pub online_data_sync: Option<bool>,
    pub costly_call_usd: Option<f64>,
}

fn sanitize_relaunch_mode(s: &str) -> String {
    match s {
        "ask" | "auto" | "never" => s.to_string(),
        _ => "never".to_string(),
    }
}

pub fn default_model_rules() -> Vec<ModelRule> {
    let ca_tiers = || {
        vec![
            PriceTier {
                until_ms: pt_midnight_ms("2026-12-01"),
                input_per_m: 2.0,
                cache_read_per_m: 0.10,
                cache_write_per_m: 0.0,
                output_per_m: 10.0,
            },
            PriceTier {
                until_ms: None,
                input_per_m: 4.0,
                cache_read_per_m: 0.20,
                cache_write_per_m: 0.0,
                output_per_m: 20.0,
            },
        ]
    };
    let f38_tiers = || {
        vec![
            PriceTier {
                until_ms: pt_midnight_ms("2027-01-01"),
                input_per_m: 0.75,
                cache_read_per_m: 0.075,
                cache_write_per_m: 0.0,
                output_per_m: 3.75,
            },
            PriceTier {
                until_ms: None,
                input_per_m: 1.50,
                cache_read_per_m: 0.15,
                cache_write_per_m: 0.0,
                output_per_m: 7.50,
            },
        ]
    };

    #[allow(clippy::too_many_arguments)]
    let rule = |pat: &str,
                name: &str,
                short: &str,
                win: u64,
                inp: f64,
                cache: f64,
                cw: f64,
                out: f64,
                key: &str,
                tiers: Vec<PriceTier>| ModelRule {
        pattern: pat.into(),
        display_name: name.into(),
        short_name: short.into(),
        context_limit: win,
        input_per_m: inp,
        cache_read_per_m: cache,
        cache_write_per_m: cw,
        output_per_m: out,
        online_key: key.into(),
        tiers,
    };

    vec![
        // Orchestrator sub-variants must match before the generic "orchestrator" rule.
        rule("orchestrator-neon", "Gemini 4 Neon", "Neon", 1_000_000, 3.0, 0.15, 0.0, 15.0, "gemini-4-neon", vec![]),
        rule(
            "orchestrator-argon",
            "Gemini 4 Argon",
            "Argon",
            1_000_000,
            2.0,
            0.10,
            0.0,
            10.0,
            "gemini-4-argon",
            ca_tiers(),
        ),
        // Orchestrator's executor model; backend labels it "Gemini 4 Hydrogen (Orchestrator)".
        rule(
            "flash-worker",
            "Gemini 4 Hydrogen",
            "Hydrogen",
            1_000_000,
            1.50,
            0.075,
            0.0,
            7.50,
            "gemini-4-hydrogen",
            vec![],
        ),
        rule(
            "gemini-4-hydrogen-tiered",
            "Gemini 4 Hydrogen",
            "Hydrogen",
            1_000_000,
            1.50,
            0.075,
            0.0,
            7.50,
            "gemini-4-hydrogen",
            vec![],
        ),
        rule("neon", "Gemini 4 Neon", "Neon", 1_000_000, 3.0, 0.15, 0.0, 15.0, "gemini-4-neon", vec![]),
        rule("carbon", "Gemini 4 Carbon", "Carbon", 1_000_000, 2.0, 0.10, 0.0, 10.0, "gemini-4-carbon", ca_tiers()),
        rule("argon", "Gemini 4 Argon", "Argon", 1_000_000, 2.0, 0.10, 0.0, 10.0, "gemini-4-argon", ca_tiers()),
        rule(
            "hydrogen",
            "Gemini 4 Hydrogen",
            "Hydrogen",
            1_000_000,
            1.50,
            0.075,
            0.0,
            7.50,
            "gemini-4-hydrogen",
            vec![],
        ),
        rule(
            "orchestrator",
            "Gemini 4 Carbon",
            "Carbon",
            1_000_000,
            2.0,
            0.10,
            0.0,
            10.0,
            "gemini-4-carbon",
            ca_tiers(),
        ),
        rule(
            "3.5-flash-lite",
            "Gemini 3.5 Flash-Lite",
            "Flash-Lite",
            1_000_000,
            0.30,
            0.03,
            0.0,
            2.50,
            "gemini-3.5-flash-lite",
            vec![],
        ),
        rule("flash-lite", "Gemini Flash-Lite", "Flash-Lite", 1_000_000, 0.30, 0.03, 0.0, 2.50, "", vec![]),
        rule(
            "3.8-flash",
            "Gemini 3.8 Flash",
            "3.8 Flash",
            1_000_000,
            0.75,
            0.075,
            0.0,
            3.75,
            "gemini-3.8-flash",
            f38_tiers(),
        ),
        rule(
            "3-8-flash",
            "Gemini 3.8 Flash",
            "3.8 Flash",
            1_000_000,
            0.75,
            0.075,
            0.0,
            3.75,
            "gemini-3.8-flash",
            f38_tiers(),
        ),
        rule(
            "3.6-flash",
            "Gemini 3.6 Flash",
            "3.6 Flash",
            1_000_000,
            0.75,
            0.075,
            0.0,
            3.75,
            "gemini-3.6-flash",
            f38_tiers(),
        ),
        rule("flash", "Gemini Flash", "Flash", 1_000_000, 0.75, 0.075, 0.0, 3.75, "", f38_tiers()),
        rule("gemini", "Gemini Pro", "Gemini Pro", 1_000_000, 1.50, 0.075, 0.0, 10.0, "", vec![]),
        // Claude 5.5 must match before Claude 5!
        rule("opus-5-5", "Claude Opus 5.5", "Opus 5.5", 200_000, 4.0, 0.20, 5.0, 20.0, "claude-opus-5-5", vec![]),
        rule("opus-5.5", "Claude Opus 5.5", "Opus 5.5", 200_000, 4.0, 0.20, 5.0, 20.0, "claude-opus-5-5", vec![]),
        rule("opus 5.5", "Claude Opus 5.5", "Opus 5.5", 200_000, 4.0, 0.20, 5.0, 20.0, "claude-opus-5-5", vec![]),
        rule("opus-5", "Claude Opus 5", "Opus 5", 200_000, 5.0, 0.50, 6.25, 25.0, "claude-opus-5", vec![]),
        rule("opus 5", "Claude Opus 5", "Opus 5", 200_000, 5.0, 0.50, 6.25, 25.0, "claude-opus-5", vec![]),
        rule("opus", "Claude Opus", "Opus", 200_000, 5.0, 0.50, 6.25, 25.0, "claude-opus-4-8", vec![]),
        rule(
            "sonnet-5-5",
            "Claude Sonnet 5.5",
            "Sonnet 5.5",
            200_000,
            2.0,
            0.10,
            2.50,
            10.0,
            "claude-sonnet-5-5",
            vec![],
        ),
        rule(
            "sonnet-5.5",
            "Claude Sonnet 5.5",
            "Sonnet 5.5",
            200_000,
            2.0,
            0.10,
            2.50,
            10.0,
            "claude-sonnet-5-5",
            vec![],
        ),
        rule(
            "sonnet 5.5",
            "Claude Sonnet 5.5",
            "Sonnet 5.5",
            200_000,
            2.0,
            0.10,
            2.50,
            10.0,
            "claude-sonnet-5-5",
            vec![],
        ),
        rule("sonnet-5", "Claude Sonnet 5", "Sonnet 5", 200_000, 2.0, 0.20, 2.50, 10.0, "claude-sonnet-5", vec![]),
        rule("sonnet 5", "Claude Sonnet 5", "Sonnet 5", 200_000, 2.0, 0.20, 2.50, 10.0, "claude-sonnet-5", vec![]),
        rule("sonnet", "Claude Sonnet", "Sonnet", 200_000, 3.0, 0.30, 3.75, 15.0, "claude-sonnet-4-6", vec![]),
        rule("haiku", "Claude Haiku", "Haiku", 200_000, 1.0, 0.10, 1.25, 5.0, "claude-haiku-4-5", vec![]),
        rule("kimi-k2", "Kimi K2", "Kimi K2", 256_000, 0.60, 0.15, 0.0, 2.50, "", vec![]),
        rule("qwen3-coder", "Qwen3 Coder", "Qwen3", 256_000, 0.40, 0.10, 0.0, 1.60, "", vec![]),
        rule("deepseek", "DeepSeek", "DeepSeek", 128_000, 0.28, 0.028, 0.0, 0.42, "", vec![]),
    ]
}

pub fn find_app_data_dir() -> PathBuf {
    if let Ok(exec_data) = env::var("ANTIGRAVITY_EXECUTABLE_DATA_DIR") {
        let mut p = PathBuf::from(exec_data);
        while let Some(parent) = p.parent() {
            if p.file_name().and_then(|s| s.to_str()) == Some("sidecar_data") {
                return parent.to_path_buf();
            }
            p = parent.to_path_buf();
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".gemini").join("antigravity")
}

pub fn find_plugin_data_dir(app_data: &Path) -> PathBuf {
    if let Ok(pd) = env::var("PLUGIN_DATA") {
        let trimmed = pd.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    app_data.join("plugin_data").join("agy-statusline")
}

/// Locates `~/.gemini/antigravity-cli` (sibling of `~/.gemini/antigravity`), or `AGY_CLI_DATA_DIR`.
pub fn find_cli_data_dir(app_data: &Path) -> PathBuf {
    if let Ok(d) = env::var("AGY_CLI_DATA_DIR") {
        let trimmed = d.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    app_data.parent().unwrap_or(app_data).join("antigravity-cli")
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("data");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{fname}.tmp.{}.{seq}", std::process::id()));
    fs::write(&tmp, content)?;
    let mut last_err = None;
    for attempt in 0..5 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt < 4 {
                    thread::sleep(Duration::from_millis(40 * (attempt + 1)));
                }
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    // Never fall back to truncating `path` directly: preserve the existing intact file on failure.
    Err(last_err.unwrap_or_else(|| std::io::Error::other("atomic rename failed")))
}

/// Outcome of reading a persisted file with retry + corruption preservation (review A1).
#[derive(Debug)]
pub enum ReadFile<T> {
    Ok(T),
    /// File does not exist yet (`ENOENT`): safe to initialize and persist fresh data.
    Missing,
    /// File existed but failed to parse; backed up to `*.bad-YYYYMMDDHHMM.*` if possible.
    Corrupt {
        kept: bool,
    },
    /// Transient I/O error (`EBUSY`, `EPERM`, etc.) persisted across retries: caller MUST NOT overwrite.
    Unreadable,
}

fn backup_corrupt_file(path: &Path, reason: &str) -> bool {
    let ts = chrono::Local::now().format("%Y%m%d%H%M");
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("json");
    let bad = path.with_file_name(format!("{stem}.bad-{ts}.{ext}"));
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    match fs::rename(path, &bad) {
        Ok(()) => {
            eprintln!(
                "[agy-statusline] {fname} unreadable ({reason}), kept as {}",
                bad.file_name().and_then(|s| s.to_str()).unwrap_or("bad")
            );
            true
        }
        Err(e) => {
            eprintln!("[agy-statusline] {fname} unreadable ({reason}), could not rename backup: {e}");
            false
        }
    }
}

/// Reads a UTF-8 file with up to 4 retries on transient errors, distinguishes `NotFound`,
/// and backs up unparseable files as `.bad-<timestamp>` so data is never silently overwritten.
pub fn read_file_keep<T, E: std::fmt::Display>(path: &Path, parse: impl FnOnce(&str) -> Result<T, E>) -> ReadFile<T> {
    let mut text = None;
    for attempt in 0..=4 {
        match fs::read_to_string(path) {
            Ok(s) => {
                text = Some(s);
                break;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return ReadFile::Missing,
            Err(e) => {
                if attempt >= 4 {
                    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
                    eprintln!("[agy-statusline] {fname} could not be read ({e}), skipping writes this session");
                    return ReadFile::Unreadable;
                }
                thread::sleep(Duration::from_millis(150));
            }
        }
    }
    let raw = text.unwrap_or_default();
    let clean = raw.trim_start_matches('\u{feff}');
    match parse(clean) {
        Ok(v) => ReadFile::Ok(v),
        Err(e) => ReadFile::Corrupt { kept: backup_corrupt_file(path, &e.to_string()) },
    }
}

pub fn read_json_keep<T: serde::de::DeserializeOwned>(path: &Path) -> ReadFile<T> {
    read_file_keep(path, |s| serde_json::from_str(s))
}

fn upgrade_model_rules(existing: &mut Vec<ModelRule>) {
    let defaults = default_model_rules();
    if existing.is_empty() {
        *existing = defaults;
        return;
    }
    let def_map: HashMap<&str, &ModelRule> = defaults.iter().map(|d| (d.pattern.as_str(), d)).collect();
    for m in existing.iter_mut() {
        if let Some(d) = def_map.get(m.pattern.as_str()) {
            // Backfill newly introduced fields on existing configs without clobbering custom base rates.
            if m.cache_write_per_m == 0.0 && d.cache_write_per_m > 0.0 {
                m.cache_write_per_m = d.cache_write_per_m;
            }
            if m.online_key.is_empty() && !d.online_key.is_empty() {
                m.online_key = d.online_key.clone();
            }
            if m.tiers.is_empty() && !d.tiers.is_empty() {
                m.tiers = d.tiers.clone();
            }
            // Earlier default configs mapped `opus-5` / `sonnet-5` to 5.5; fix when untouched.
            if matches!(m.pattern.as_str(), "opus-5" | "opus 5")
                && m.display_name == "Claude Opus 5.5"
                && (m.input_per_m - 4.0).abs() < 1e-9
            {
                *m = (*d).clone();
            }
            if matches!(m.pattern.as_str(), "sonnet-5" | "sonnet 5")
                && m.display_name == "Claude Sonnet 5.5"
                && (m.cache_read_per_m - 0.10).abs() < 1e-9
            {
                *m = (*d).clone();
            }
        }
    }
    // Insert any new default rules in front so specific patterns match before older broad ones.
    let mut missing: Vec<ModelRule> =
        defaults.into_iter().filter(|d| !existing.iter().any(|m| m.pattern == d.pattern)).collect();
    if !missing.is_empty() {
        missing.append(existing);
        *existing = missing;
    }
}

pub fn load_config(plugin_data: &Path) -> Config {
    let path = plugin_data.join("config.toml");
    match read_file_keep(&path, |s| toml::from_str::<Config>(s)) {
        ReadFile::Ok(mut cfg) => {
            cfg.bar_look.sanitize();
            cfg.relaunch_mode = sanitize_relaunch_mode(&cfg.relaunch_mode);
            if !cfg.costly_call_usd.is_finite() || cfg.costly_call_usd < 0.0 {
                cfg.costly_call_usd = 0.50;
            }
            upgrade_model_rules(&mut cfg.models);
            cfg
        }
        ReadFile::Missing | ReadFile::Corrupt { kept: true } => {
            let def = Config::default();
            if let Ok(toml_str) = toml::to_string_pretty(&def) {
                let _ = write_atomic(&path, &toml_str);
            }
            def
        }
        ReadFile::Unreadable | ReadFile::Corrupt { kept: false } => Config::default(),
    }
}

pub fn apply_config_patch(plugin_data: &Path, patch: ConfigPatch) -> Config {
    let mut cfg = load_config(plugin_data);
    if let Some(v) = patch.context_limit_override {
        cfg.context_limit_override = v;
    }
    if let Some(v) = patch.cache_ttl_sec {
        cfg.cache_ttl_sec = v.max(30);
    }
    if let Some(v) = patch.include_subagents {
        cfg.include_subagents = v;
    }
    if let Some(v) = patch.bar_enabled {
        cfg.bar_enabled = v;
    }
    if let Some(v) = patch.bar_segments {
        cfg.bar_segments = v;
    }
    if let Some(mut v) = patch.bar_look {
        v.sanitize();
        cfg.bar_look = v;
    }
    if let Some(v) = patch.ui_zh {
        cfg.ui_zh = v;
    }
    if let Some(v) = patch.relaunch_mode {
        cfg.relaunch_mode = sanitize_relaunch_mode(&v);
    }
    if let Some(v) = patch.online_updates {
        cfg.online_updates = v;
    }
    if let Some(v) = patch.online_data_sync {
        cfg.online_data_sync = v;
    }
    if let Some(v) = patch.costly_call_usd
        && v.is_finite()
        && v >= 0.0
    {
        cfg.costly_call_usd = (v * 100.0).round() / 100.0;
    }
    let path = plugin_data.join("config.toml");
    if let Ok(toml_str) = toml::to_string_pretty(&cfg) {
        let _ = write_atomic(&path, &toml_str);
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carbon_price_tier_switches_at_pt_midnight() {
        let rules = default_model_rules();
        let carbon = rules.iter().find(|r| r.pattern == "carbon").unwrap();
        let before = pt_midnight_ms("2026-12-01").unwrap() - 1000;
        let after = pt_midnight_ms("2026-12-01").unwrap() + 1000;
        let (in_b, cr_b, _, out_b) = carbon.rates_at(before, after);
        let (in_a, cr_a, _, out_a) = carbon.rates_at(after, after);
        assert_eq!((in_b, cr_b, out_b), (2.0, 0.10, 10.0));
        assert_eq!((in_a, cr_a, out_a), (4.0, 0.20, 20.0));
    }

    #[test]
    fn claude_opus_5_5_precedes_opus_5_and_includes_cache_write() {
        let rules = default_model_rules();
        let m55 = rules.iter().find(|r| "claude-opus-5-5".contains(&r.pattern)).unwrap();
        assert_eq!(m55.display_name, "Claude Opus 5.5");
        assert_eq!(m55.rates_at(0, 0), (4.0, 0.20, 5.0, 20.0));

        let m5 = rules.iter().find(|r| "claude-opus-5".contains(&r.pattern)).unwrap();
        assert_eq!(m5.display_name, "Claude Opus 5");
        assert_eq!(m5.rates_at(0, 0), (5.0, 0.50, 6.25, 25.0));
    }

    #[test]
    fn online_prices_overlay_and_append_new_models() {
        let json = r#"{
            "updated": "2026-11-23",
            "models": {
                "gemini-4-neon": { "title": "Gemini 4 Neon", "t": [[null, {"in": 3.5, "cache": 0.175, "out": 17.5}]] },
                "gemini-5-alpha": { "title": "Gemini 5 Alpha", "t": [[null, {"in": 6.0, "cache": 0.3, "out": 30.0}]] }
            }
        }"#;
        let online: OnlinePrices = serde_json::from_str(json).unwrap();
        let eff = effective_model_rules(&default_model_rules(), Some(&online));
        let neon = eff.iter().find(|r| r.pattern == "neon").unwrap();
        assert_eq!(neon.rates_at(0, 0).0, 3.5);
        let alpha = eff.iter().find(|r| r.pattern == "gemini-5-alpha").unwrap();
        assert_eq!(alpha.short_name, "5 Alpha");
        assert_eq!(alpha.rates_at(0, 0).3, 30.0);
    }

    #[test]
    fn detects_price_cut_inside_day_window_and_backs_up_corrupt_config() {
        let pricer = crate::pricing::Pricer::from_rules(default_model_rules(), 0);
        let cut = pt_midnight_ms("2026-12-01").unwrap();
        assert!(pricer.is_cut_day(cut - 3_600_000));
        assert!(!pricer.is_cut_day(cut));

        let dir = std::env::temp_dir().join(format!("agy-cfg-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), "[[[invalid toml").unwrap();
        let cfg = load_config(&dir);
        assert!((cfg.costly_call_usd - 0.50).abs() < 1e-9);
        let has_bad = fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains(".bad-"));
        assert!(has_bad, "corrupt config.toml should be backed up to .bad-*");
        let _ = fs::remove_dir_all(&dir);
    }
}
