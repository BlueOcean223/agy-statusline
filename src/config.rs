use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRule {
    pub pattern: String,
    pub display_name: String,
    pub short_name: String,
    pub context_limit: u64,
    pub input_per_m: f64,
    pub cache_read_per_m: f64,
    pub output_per_m: f64,
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
            output_per_m: 10.0,
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
    /// Translate Antigravity's UI to Chinese (i18n/, from agy-zh-pack).
    pub ui_zh: bool,
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
            ui_zh: false,
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
    pub ui_zh: Option<bool>,
}

pub fn default_model_rules() -> Vec<ModelRule> {
    let rule = |pat: &str, name: &str, short: &str, win: u64, inp: f64, cache: f64, out: f64| ModelRule {
        pattern: pat.into(),
        display_name: name.into(),
        short_name: short.into(),
        context_limit: win,
        input_per_m: inp,
        cache_read_per_m: cache,
        output_per_m: out,
    };

    vec![
        rule("neon", "Gemini 4 Neon", "Neon", 1_000_000, 3.0, 0.15, 15.0),
        // Orchestrator's executor model; the backend labels it "Gemini 4 Hydrogen (Orchestrator)".
        rule("flash-worker", "Gemini 4 Hydrogen", "Hydrogen", 1_000_000, 1.50, 0.075, 7.50),
        rule("carbon", "Gemini 4 Carbon", "Carbon", 1_000_000, 2.0, 0.10, 10.0),
        rule("argon", "Gemini 4 Argon", "Argon", 1_000_000, 2.0, 0.10, 10.0),
        rule("hydrogen", "Gemini 4 Hydrogen", "Hydrogen", 1_000_000, 1.50, 0.075, 7.50),
        rule("orchestrator", "Gemini Orchestrator", "Orchestrator", 1_000_000, 2.0, 0.10, 10.0),
        rule("flash-lite", "Gemini Flash-Lite", "Flash-Lite", 1_000_000, 0.10, 0.025, 0.40),
        rule("3.8-flash", "Gemini 3.8 Flash", "Flash 3.8", 1_000_000, 0.75, 0.075, 3.75),
        rule("flash", "Gemini Flash", "Flash", 1_000_000, 0.75, 0.075, 3.75),
        rule("gemini", "Gemini Pro", "Gemini Pro", 1_000_000, 1.50, 0.075, 10.0),
        rule("opus-5", "Claude Opus 5.5", "Opus 5.5", 200_000, 4.0, 0.20, 20.0),
        rule("opus 5", "Claude Opus 5.5", "Opus 5.5", 200_000, 4.0, 0.20, 20.0),
        rule("opus", "Claude Opus", "Opus", 200_000, 5.0, 0.50, 25.0),
        rule("sonnet-5", "Claude Sonnet 5.5", "Sonnet 5.5", 200_000, 2.0, 0.10, 10.0),
        rule("sonnet 5", "Claude Sonnet 5.5", "Sonnet 5.5", 200_000, 2.0, 0.10, 10.0),
        rule("sonnet", "Claude Sonnet", "Sonnet", 200_000, 3.0, 0.30, 15.0),
        rule("haiku", "Claude Haiku", "Haiku", 200_000, 1.0, 0.10, 5.0),
        rule("kimi-k2", "Kimi K2", "Kimi K2", 256_000, 0.60, 0.15, 2.50),
        rule("qwen3-coder", "Qwen3 Coder", "Qwen3", 256_000, 0.40, 0.10, 1.60),
        rule("deepseek", "DeepSeek", "DeepSeek", 128_000, 0.28, 0.028, 0.42),
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

pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

pub fn load_config(plugin_data: &Path) -> Config {
    let path = plugin_data.join("config.toml");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(mut cfg) = toml::from_str::<Config>(&text)
    {
        if cfg.models.is_empty() {
            cfg.models = default_model_rules();
        } else {
            // Rules are matched in order and older files end with a broad "gemini" rule,
            // so default rules missing from the file go in front.
            let mut missing: Vec<ModelRule> = default_model_rules()
                .into_iter()
                .filter(|d| !cfg.models.iter().any(|m| m.pattern == d.pattern))
                .collect();
            if !missing.is_empty() {
                missing.append(&mut cfg.models);
                cfg.models = missing;
            }
        }
        return cfg;
    }
    let def = Config::default();
    if let Ok(toml_str) = toml::to_string_pretty(&def) {
        let _ = write_atomic(&path, &toml_str);
    }
    def
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
    if let Some(v) = patch.ui_zh {
        cfg.ui_zh = v;
    }
    let path = plugin_data.join("config.toml");
    if let Ok(toml_str) = toml::to_string_pretty(&cfg) {
        let _ = write_atomic(&path, &toml_str);
    }
    cfg
}
