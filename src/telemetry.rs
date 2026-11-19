use crate::config::{Config, ModelRule, load_config};
use crate::hook::BridgeState;
use chrono::{DateTime, Local, TimeZone};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const FIVE_HOURS_SEC: i64 = 5 * 3600;
const SEVEN_DAYS_SEC: i64 = 7 * 86_400;

#[derive(Clone, Debug, Serialize)]
pub struct RecentConversation {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SubagentSummary {
    pub id: String,
    pub model: String,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct QuotaWindow {
    pub pct: u64,
    pub ratio: f64,
    pub elapsed_ratio: f64,
    pub pace_delta_pct: i64,
    pub used_fmt: String,
    pub limit_fmt: String,
    pub left_fmt: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TurnItem {
    pub step_index: u64,
    pub time_hms: String,
    pub prompt_context: u64,
    pub hit_pct: f64,
    pub out_tokens: u64,
    pub cost_usd: f64,
    pub tps: f64,
    pub duration_sec: f64,
    pub label: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TelemetrySummary {
    pub model_name: String,
    pub model_short: String,
    pub effort: String,
    pub price_in: f64,
    pub price_cache: f64,
    pub price_out: f64,
    pub state: String,
    pub activity_label: String,
    pub workspace_dir: String,
    pub git_branch: Option<String>,
    pub git_dirty: bool,
    pub git_ahead: u64,
    pub git_behind: u64,
    pub git_added: u64,
    pub git_deleted: u64,
    pub turns: usize,
    pub steps: usize,
    pub compactions: usize,
    pub context_tokens: u64,
    pub context_limit: u64,
    pub context_pct: f64,
    pub context_left: u64,
    pub last_context_delta: i64,
    pub turns_to_compact: Option<u64>,
    pub context_series: Vec<u64>,
    pub last_cache_tokens: u64,
    pub last_cache_hit_pct: f64,
    pub session_cache_hit_pct: f64,
    pub cache_misses: usize,
    pub cache_warm: bool,
    pub cache_warm_left_fmt: String,
    pub cache_saved_usd: f64,
    pub fresh_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub today_tokens: u64,
    pub session_cost_usd: f64,
    pub cost_in_usd: f64,
    pub cost_cache_usd: f64,
    pub cost_out_usd: f64,
    pub cost_sub_usd: f64,
    pub cost_rate_per_hr: Option<f64>,
    pub today_cost_usd: f64,
    pub last_tps: f64,
    pub avg_tps: f64,
    pub last_duration_sec: f64,
    pub tps_source: String,
    pub tps_series: Vec<f64>,
    pub session_duration_fmt: String,
    pub subagents: Vec<SubagentSummary>,
    pub quota_5h: QuotaWindow,
    pub quota_7d: QuotaWindow,
}

#[derive(Clone, Debug, Serialize)]
pub struct TelemetryResponse {
    pub etag: String,
    pub conversation_id: String,
    pub conversation_title: String,
    pub recent_conversations: Vec<RecentConversation>,
    pub summary: TelemetrySummary,
    pub turns: Vec<TurnItem>,
    pub config: Config,
}

#[derive(Clone, Debug)]
struct PlannerCall {
    step_index: u64,
    created_ms: i64,
    end_ms: i64,
    duration_ms: i64,
    fresh_in: u64,
    cache_read: u64,
    out: u64,
    prompt_context: u64,
    label: String,
}

#[derive(Clone, Debug)]
struct ParsedTranscript {
    mtime_ms: i64,
    total_steps: usize,
    first_step_ms: i64,
    last_step_ms: i64,
    detected_model: String,
    detected_workspace: String,
    subagent_ids: Vec<String>,
    calls: Vec<PlannerCall>,
    compactions: usize,
    activity_state: String,
    activity_label: String,
}

#[derive(Clone, Debug, Default)]
struct GitStatus {
    branch: String,
    dirty: bool,
    ahead: u64,
    behind: u64,
    added: u64,
    deleted: u64,
}

#[derive(Default)]
struct EngineCache {
    transcripts: HashMap<PathBuf, (String, ParsedTranscript)>,
    annotations: HashMap<PathBuf, (String, String, i64)>,
    git: HashMap<String, (i64, Option<GitStatus>)>,
    global_win: Option<(i64, GlobalWindows)>,
}

#[derive(Clone, Debug, Default)]
struct GlobalWindows {
    today_tokens: u64,
    today_cost_usd: f64,
    five_h_tokens: u64,
    five_h_cost_usd: f64,
    five_h_earliest_ms: i64,
    seven_d_tokens: u64,
    seven_d_cost_usd: f64,
    seven_d_earliest_ms: i64,
}

static CACHE: OnceLock<Mutex<EngineCache>> = OnceLock::new();

fn cache() -> &'static Mutex<EngineCache> {
    CACHE.get_or_init(|| Mutex::new(EngineCache::default()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn file_key(path: &Path) -> Option<(String, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((format!("{mtime_ms}:{}", meta.len()), mtime_ms))
}

fn is_valid_id(id: &str) -> bool {
    id.len() >= 8
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        let s = if m >= 10.0 {
            format!("{m:.1}")
        } else {
            format!("{m:.2}")
        };
        let clean = s.trim_end_matches('0').trim_end_matches('.');
        format!("{clean}M")
    } else if n >= 1_000 {
        let k = n as f64 / 1_000.0;
        if k >= 100.0 {
            format!("{:.0}k", k)
        } else {
            let s = format!("{k:.1}");
            format!("{}k", s.strip_suffix(".0").unwrap_or(&s))
        }
    } else {
        n.to_string()
    }
}

pub fn fmt_usd(v: f64) -> String {
    if v <= 0.0 {
        "$0.00".into()
    } else if v < 0.01 {
        format!("${v:.4}")
    } else if v < 1.0 {
        format!("${v:.3}")
    } else {
        format!("${v:.2}")
    }
}

pub fn fmt_duration(sec: i64) -> String {
    let s = sec.max(0);
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    let rem_s = s % 60;
    if m < 60 {
        return if m < 10 && rem_s > 0 {
            format!("{m}m{rem_s}s")
        } else {
            format!("{m}m")
        };
    }
    let h = m / 60;
    let rem_m = m % 60;
    if h < 24 {
        return if rem_m > 0 {
            format!("{h}h{rem_m}m")
        } else {
            format!("{h}h")
        };
    }
    let d = h / 24;
    let rem_h = h % 24;
    if rem_h > 0 {
        format!("{d}d{rem_h}h")
    } else {
        format!("{d}d")
    }
}

fn resolve_model(raw: &str, cfg: &Config) -> (ModelRule, String) {
    static EFFORT_PAREN_RE: OnceLock<Regex> = OnceLock::new();
    static EFFORT_SLUG_RE: OnceLock<Regex> = OnceLock::new();
    let re_paren = EFFORT_PAREN_RE.get_or_init(|| {
        Regex::new(r"(?i)\((high|medium|low|max|minimal|thinking)\)\s*$").unwrap()
    });
    let re_slug = EFFORT_SLUG_RE.get_or_init(|| {
        Regex::new(r"(?i)[-_](high|medium|low|max|minimal|thinking)\s*$").unwrap()
    });

    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "None" || trimmed == "auto" {
        let mut def = cfg.models.first().cloned().unwrap_or_default();
        if cfg.context_limit_override > 0 {
            def.context_limit = cfg.context_limit_override;
        }
        return (def, String::new());
    }

    let cap_effort = |s: &str| -> String {
        let mut chars = s.chars();
        match chars.next() {
            Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
            None => String::new(),
        }
    };

    let mut effort = String::new();
    let mut base = trimmed.to_string();
    if let Some(caps) = re_paren.captures(trimmed)
        && let Some(m) = caps.get(1)
    {
        effort = cap_effort(m.as_str());
        base = trimmed[..caps.get(0).unwrap().start()].trim().to_string();
    } else if let Some(caps) = re_slug.captures(trimmed)
        && let Some(m) = caps.get(1)
    {
        effort = cap_effort(m.as_str());
        base = trimmed[..caps.get(0).unwrap().start()].trim().to_string();
    }

    let is_slug = base.contains('-') || base.contains('_');
    let lower = base.to_lowercase();
    for r in &cfg.models {
        if !r.pattern.is_empty() && lower.contains(&r.pattern.to_lowercase()) {
            let mut matched = r.clone();
            if !is_slug && !base.is_empty() && base.len() <= 28 {
                matched.display_name = base;
            }
            if cfg.context_limit_override > 0 {
                matched.context_limit = cfg.context_limit_override;
            }
            return (matched, effort);
        }
    }

    let mut fallback = ModelRule {
        pattern: String::new(),
        display_name: base.clone(),
        short_name: base
            .strip_prefix("Gemini ")
            .or_else(|| base.strip_prefix("Claude "))
            .unwrap_or(&base)
            .to_string(),
        context_limit: 256_000,
        input_per_m: 2.0,
        cache_read_per_m: 0.10,
        output_per_m: 10.0,
    };
    if cfg.context_limit_override > 0 {
        fallback.context_limit = cfg.context_limit_override;
    }
    (fallback, effort)
}

fn calc_step_cost(fresh_in: u64, cache_read: u64, out: u64, rule: &ModelRule) -> (f64, f64, f64, f64) {
    let in_usd = (fresh_in as f64 / 1_000_000.0) * rule.input_per_m;
    let cache_usd = (cache_read as f64 / 1_000_000.0) * rule.cache_read_per_m;
    let out_usd = (out as f64 / 1_000_000.0) * rule.output_per_m;
    let saved_usd = ((cache_read as f64 / 1_000_000.0) * (rule.input_per_m - rule.cache_read_per_m)).max(0.0);
    (in_usd, cache_usd, out_usd, saved_usd)
}

fn parse_iso_ms(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        serde_json::from_str::<String>(t).unwrap_or_else(|_| t[1..t.len() - 1].to_string())
    } else {
        t.to_string()
    }
}

fn describe_tool_call(tc: &Value) -> String {
    let name = tc.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        return String::new();
    }
    if let Some(args) = tc.get("args").and_then(Value::as_object) {
        for key in ["toolSummary", "Description", "Role"] {
            if let Some(v) = args.get(key).and_then(Value::as_str) {
                let u = unquote(v);
                if !u.is_empty() {
                    return u.chars().take(48).collect();
                }
            }
        }
        for key in ["Path", "TargetFile"] {
            if let Some(v) = args.get(key).and_then(Value::as_str) {
                let u = unquote(v);
                if let Some(fname) = Path::new(&u).file_name().and_then(|f| f.to_str()) {
                    return format!("{name} {fname}");
                }
            }
        }
    }
    name.to_string()
}

fn parse_content_times(content: &str) -> (i64, i64) {
    static CREATED_RE: OnceLock<Regex> = OnceLock::new();
    static COMPLETED_RE: OnceLock<Regex> = OnceLock::new();
    let r1 = CREATED_RE.get_or_init(|| Regex::new(r"Created At:\s*([^\r\n]+)").unwrap());
    let r2 = COMPLETED_RE.get_or_init(|| Regex::new(r"Completed At:\s*([^\r\n]+)").unwrap());

    let c1 = r1
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| parse_iso_ms(m.as_str()))
        .unwrap_or(0);
    let c2 = r2
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| parse_iso_ms(m.as_str()))
        .unwrap_or(0);
    (c1, c2)
}

fn read_annotation(ann_path: &Path) -> (String, i64) {
    let Some((key, mtime_ms)) = file_key(ann_path) else {
        return (String::new(), 0);
    };
    if let Ok(guard) = cache().lock()
        && let Some((k, title, vms)) = guard.annotations.get(ann_path)
        && *k == key
    {
        return (title.clone(), *vms);
    }

    static TITLE_RE: OnceLock<Regex> = OnceLock::new();
    static VIEW_RE: OnceLock<Regex> = OnceLock::new();
    let tre = TITLE_RE.get_or_init(|| Regex::new(r#"title:\s*"((?:[^"\\]|\\.)*)""#).unwrap());
    let vre = VIEW_RE.get_or_init(|| {
        Regex::new(r"last_user_view_time:\s*\{\s*seconds:\s*(\d+)(?:\s+nanos:\s*(\d+))?").unwrap()
    });

    let text = fs::read_to_string(ann_path).unwrap_or_default();
    let title = tre
        .captures(&text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().replace("\\\"", "\""))
        .unwrap_or_default();
    let view_ms = vre
        .captures(&text)
        .map(|c| {
            let sec: i64 = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let nanos: i64 = c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            sec * 1000 + nanos / 1_000_000
        })
        .unwrap_or(mtime_ms);

    if let Ok(mut guard) = cache().lock() {
        guard
            .annotations
            .insert(ann_path.to_path_buf(), (key, title.clone(), view_ms));
    }
    (title, view_ms)
}

fn parse_transcript(app_data: &Path, conv_id: &str) -> Option<ParsedTranscript> {
    if !is_valid_id(conv_id) {
        return None;
    }
    let path = app_data
        .join("brain")
        .join(conv_id)
        .join(".system_generated/logs/transcript.jsonl");
    let (key, mtime_ms) = file_key(&path)?;

    if let Ok(guard) = cache().lock()
        && let Some((k, parsed)) = guard.transcripts.get(&path)
        && *k == key
    {
        return Some(parsed.clone());
    }

    let raw = fs::read_to_string(&path).ok()?;
    let steps: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();

    static MODEL_RE: OnceLock<Regex> = OnceLock::new();
    static SUB_RE: OnceLock<Regex> = OnceLock::new();
    let model_re = MODEL_RE.get_or_init(|| {
        Regex::new(r"changed setting `Model Selection` from [^\n]+? to ([^\n.]+)\.").unwrap()
    });
    let sub_re = SUB_RE.get_or_init(|| {
        Regex::new(r"(?i)conversationId[\x22':\s]+([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})").unwrap()
    });

    let mut detected_model = String::new();
    let mut detected_workspace = String::new();
    let mut first_step_ms = 0i64;
    let mut last_step_ms = 0i64;
    let mut subagents = HashSet::new();
    let mut calls: Vec<PlannerCall> = Vec::new();
    let mut compactions = 0usize;

    for (i, s) in steps.iter().enumerate() {
        let stype = s.get("type").and_then(Value::as_str).unwrap_or("");
        let created_ms = s
            .get("created_at")
            .and_then(Value::as_str)
            .map(parse_iso_ms)
            .unwrap_or(0);

        if created_ms > 0 {
            if first_step_ms == 0 || created_ms < first_step_ms {
                first_step_ms = created_ms;
            }
            if created_ms > last_step_ms {
                last_step_ms = created_ms;
            }
        }

        if stype == "USER_INPUT"
            && let Some(content) = s.get("content").and_then(Value::as_str)
            && let Some(caps) = model_re.captures(content)
            && let Some(m) = caps.get(1)
        {
            detected_model = m.as_str().trim().to_string();
        }

        if stype == "GENERIC"
            && let Some(content) = s.get("content").and_then(Value::as_str)
        {
            for caps in sub_re.captures_iter(content) {
                if let Some(m) = caps.get(1) {
                    let sub_id = m.as_str().to_lowercase();
                    if sub_id != conv_id.to_lowercase() {
                        subagents.insert(sub_id);
                    }
                }
            }
        }

        if stype == "PLANNER_RESPONSE" {
            let fresh_in = s.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let cache_read = s
                .get("cache_read_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let out = s.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
            let prompt_context = fresh_in + cache_read;

            if let Some(prev) = calls.last()
                && prev.prompt_context > 60_000
                && prompt_context > 0
                && (prompt_context as f64) < (prev.prompt_context as f64) * 0.72
            {
                compactions += 1;
            }

            let tcs = s
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            for tc in &tcs {
                if detected_workspace.is_empty()
                    && let Some(p) = tc
                        .get("args")
                        .and_then(|a| a.get("Path"))
                        .and_then(Value::as_str)
                {
                    let up = unquote(p);
                    if up.starts_with("/Users/") && !up.contains("/.gemini/") {
                        let parts: Vec<&str> = up.split('/').collect();
                        if parts.len() >= 4 {
                            detected_workspace = parts[..4].join("/");
                        }
                    }
                }
            }

            let mut start_ms = created_ms;
            if i > 0
                && let Some(prev_content) = steps[i - 1].get("content").and_then(Value::as_str)
            {
                let (_, completed_ms) = parse_content_times(prev_content);
                if completed_ms > start_ms {
                    start_ms = completed_ms;
                }
            }

            let mut end_ms = 0i64;
            if i + 1 < steps.len() {
                let next = &steps[i + 1];
                if let Some(nc) = next.get("content").and_then(Value::as_str) {
                    let (c1, _) = parse_content_times(nc);
                    end_ms = c1;
                }
                if end_ms == 0 {
                    end_ms = next
                        .get("created_at")
                        .and_then(Value::as_str)
                        .map(parse_iso_ms)
                        .unwrap_or(0);
                }
            } else if mtime_ms > start_ms {
                end_ms = mtime_ms;
            }

            let duration_ms = if end_ms > start_ms {
                end_ms - start_ms
            } else {
                0
            };

            let label = tcs
                .last()
                .map(describe_tool_call)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Final reply".into());

            let step_idx = s
                .get("step_index")
                .and_then(Value::as_u64)
                .unwrap_or(i as u64);

            calls.push(PlannerCall {
                step_index: step_idx,
                created_ms: if created_ms > 0 { created_ms } else { mtime_ms },
                end_ms,
                duration_ms,
                fresh_in,
                cache_read,
                out,
                prompt_context,
                label,
            });
        }
    }

    let now = now_ms();
    let mut activity_state = "idle".to_string();
    let mut activity_label = "Idle".to_string();
    let mut generic_after = 0usize;
    for s in steps.iter().rev() {
        let stype = s.get("type").and_then(Value::as_str).unwrap_or("");
        if stype == "GENERIC" {
            generic_after += 1;
            continue;
        }
        if stype == "USER_INPUT" {
            if now - mtime_ms < 180_000 {
                activity_state = "thinking".into();
                activity_label = "Thinking…".into();
            }
            break;
        }
        if stype == "PLANNER_RESPONSE" {
            let tcs = s
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if tcs > 0 && generic_after < tcs && now - mtime_ms < 300_000 {
                activity_state = "tool".into();
                activity_label = calls
                    .last()
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| "Running tool…".into());
            } else if tcs > 0 && generic_after >= tcs && now - mtime_ms < 180_000 {
                activity_state = "thinking".into();
                activity_label = "Thinking…".into();
            }
            break;
        }
    }

    let parsed = ParsedTranscript {
        mtime_ms,
        total_steps: steps.len(),
        first_step_ms,
        last_step_ms: if last_step_ms > 0 { last_step_ms } else { mtime_ms },
        detected_model,
        detected_workspace,
        subagent_ids: subagents.into_iter().collect(),
        calls,
        compactions,
        activity_state,
        activity_label,
    };

    if let Ok(mut guard) = cache().lock() {
        if guard.transcripts.len() > 400 {
            guard.transcripts.clear();
        }
        guard.transcripts.insert(path, (key, parsed.clone()));
    }
    Some(parsed)
}

fn read_git_cached(cwd: &str) -> Option<GitStatus> {
    if cwd.is_empty() || !Path::new(cwd).exists() {
        return None;
    }
    let now = now_ms();
    if let Ok(guard) = cache().lock()
        && let Some((ts, info)) = guard.git.get(cwd)
        && now - *ts < 3000
    {
        return info.clone();
    }

    let out = Command::new("git")
        .args(["-C", cwd, "status", "--porcelain=v1", "-b"])
        .output()
        .ok()?;
    if !out.status.success() {
        if let Ok(mut guard) = cache().lock() {
            guard.git.insert(cwd.to_string(), (now, None));
        }
        return None;
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let header = lines.next().unwrap_or("");
    let mut branch = "HEAD".to_string();
    let mut ahead = 0u64;
    let mut behind = 0u64;

    if let Some(rest) = header.strip_prefix("## ") {
        let br_part = rest.split("...").next().unwrap_or(rest);
        branch = br_part.split_whitespace().next().unwrap_or("HEAD").to_string();
        if let Some(idx) = rest.find('[')
            && let Some(end) = rest.find(']')
        {
            let inside = &rest[idx + 1..end];
            for part in inside.split(',') {
                let p = part.trim();
                if let Some(n) = p.strip_prefix("ahead ") {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = p.strip_prefix("behind ") {
                    behind = n.parse().unwrap_or(0);
                }
            }
        }
    }

    let dirty = lines.any(|l| !l.trim().is_empty());

    let mut added = 0u64;
    let mut deleted = 0u64;
    if let Ok(diff) = Command::new("git")
        .args(["-C", cwd, "diff", "--shortstat", "HEAD"])
        .output()
        && diff.status.success()
    {
        let ds = String::from_utf8_lossy(&diff.stdout);
        for part in ds.split(',') {
            let p = part.trim();
            if p.contains("insertion") {
                added = p
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if p.contains("deletion") {
                deleted = p
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
        }
    }

    let status = Some(GitStatus {
        branch,
        dirty,
        ahead,
        behind,
        added,
        deleted,
    });
    if let Ok(mut guard) = cache().lock() {
        guard.git.insert(cwd.to_string(), (now, status.clone()));
    }
    status
}

fn compute_global_windows(app_data: &Path, cfg: &Config, now: i64) -> GlobalWindows {
    if let Ok(guard) = cache().lock()
        && let Some((ts, ref gw)) = guard.global_win
        && now - ts < 2500
    {
        return gw.clone();
    }

    let today_start_ms = Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|ndt| Local.from_local_datetime(&ndt).single())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(now - 86_400_000);

    let five_h_cutoff = now - FIVE_HOURS_SEC * 1000;
    let seven_d_cutoff = now - SEVEN_DAYS_SEC * 1000;

    let mut gw = GlobalWindows::default();
    let brain_dir = app_data.join("brain");
    let entries = fs::read_dir(&brain_dir).ok();
    if let Some(entries) = entries {
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().to_string();
            if !is_valid_id(&id) {
                continue;
            }
            let tr_path = entry.path().join(".system_generated/logs/transcript.jsonl");
            let Some((_, mtime_ms)) = file_key(&tr_path) else {
                continue;
            };
            if mtime_ms < seven_d_cutoff {
                continue;
            }
            let Some(tr) = parse_transcript(app_data, &id) else {
                continue;
            };
            let (rule, _) = resolve_model(&tr.detected_model, cfg);
            for c in &tr.calls {
                let t = if c.created_ms > 0 { c.created_ms } else { tr.mtime_ms };
                if t < seven_d_cutoff {
                    continue;
                }
                let tok = c.fresh_in + c.cache_read + c.out;
                let (i_u, c_u, o_u, _) = calc_step_cost(c.fresh_in, c.cache_read, c.out, &rule);
                let usd = i_u + c_u + o_u;

                gw.seven_d_tokens += tok;
                gw.seven_d_cost_usd += usd;
                if gw.seven_d_earliest_ms == 0 || t < gw.seven_d_earliest_ms {
                    gw.seven_d_earliest_ms = t;
                }

                if t >= five_h_cutoff {
                    gw.five_h_tokens += tok;
                    gw.five_h_cost_usd += usd;
                    if gw.five_h_earliest_ms == 0 || t < gw.five_h_earliest_ms {
                        gw.five_h_earliest_ms = t;
                    }
                }

                if t >= today_start_ms {
                    gw.today_tokens += tok;
                    gw.today_cost_usd += usd;
                }
            }
        }
    }

    if let Ok(mut guard) = cache().lock() {
        guard.global_win = Some((now, gw.clone()));
    }
    gw
}

fn make_quota_window(
    used_tokens: u64,
    used_usd: f64,
    earliest_ms: i64,
    win_sec: i64,
    limit_tokens: u64,
    limit_usd: f64,
    mode: &str,
    now: i64,
) -> QuotaWindow {
    let is_tok = mode == "tokens";
    let ratio = if is_tok {
        used_tokens as f64 / (limit_tokens.max(1) as f64)
    } else {
        used_usd / limit_usd.max(0.01)
    };
    let clamped = ratio.clamp(0.0, 1.0);
    let elapsed_sec = if earliest_ms > 0 {
        ((now - earliest_ms) / 1000).clamp(60, win_sec)
    } else {
        0
    };
    let left_sec = if earliest_ms > 0 {
        (win_sec - elapsed_sec).max(0)
    } else {
        win_sec
    };
    let elapsed_ratio = (elapsed_sec as f64 / win_sec as f64).clamp(0.0, 1.0);
    let pace_delta_pct = if earliest_ms > 0 {
        ((clamped - elapsed_ratio) * 100.0).round() as i64
    } else {
        0
    };

    QuotaWindow {
        pct: (ratio * 100.0).round() as u64,
        ratio: clamped,
        elapsed_ratio,
        pace_delta_pct,
        used_fmt: if is_tok {
            fmt_tokens(used_tokens)
        } else {
            fmt_usd(used_usd)
        },
        limit_fmt: if is_tok {
            fmt_tokens(limit_tokens)
        } else {
            fmt_usd(limit_usd)
        },
        left_fmt: fmt_duration(left_sec),
    }
}

pub fn list_recent_conversations(app_data: &Path) -> Vec<(String, String, bool, i64, i64)> {
    let brain_dir = app_data.join("brain");
    let ann_dir = app_data.join("annotations");
    let mut out = Vec::new();

    if let Ok(entries) = fs::read_dir(&brain_dir) {
        for e in entries.flatten() {
            let id = e.file_name().to_string_lossy().to_string();
            if !is_valid_id(&id) {
                continue;
            }
            let tr_path = e.path().join(".system_generated/logs/transcript.jsonl");
            let Some((_, mtime_ms)) = file_key(&tr_path) else {
                continue;
            };
            let ann_path = ann_dir.join(format!("{id}.pbtxt"));
            let has_ann = ann_path.exists();
            let (title, view_ms) = if has_ann {
                read_annotation(&ann_path)
            } else {
                (String::new(), 0)
            };
            let display_title = if title.is_empty() {
                id.chars().take(8).collect()
            } else {
                title
            };
            out.push((id, display_title, has_ann, view_ms, mtime_ms));
        }
    }

    out.sort_by(|a, b| {
        if a.2 != b.2 {
            return b.2.cmp(&a.2);
        }
        if (b.3 - a.3).abs() > 2000 {
            return b.3.cmp(&a.3);
        }
        b.4.cmp(&a.4)
    });
    out.truncate(16);
    out
}

pub fn build_snapshot(
    app_data: &Path,
    plugin_data: &Path,
    requested_conv_id: &str,
    workspace_uris: &[String],
) -> TelemetryResponse {
    let now = now_ms();
    let cfg = load_config(plugin_data);
    let recents = list_recent_conversations(app_data);

    let conv_id = if is_valid_id(requested_conv_id) {
        requested_conv_id.to_string()
    } else {
        recents.first().map(|r| r.0.clone()).unwrap_or_default()
    };

    let conv_title = recents
        .iter()
        .find(|r| r.0 == conv_id)
        .map(|r| r.1.clone())
        .unwrap_or_else(|| {
            let ann_path = app_data.join("annotations").join(format!("{conv_id}.pbtxt"));
            let (t, _) = read_annotation(&ann_path);
            if t.is_empty() {
                conv_id.chars().take(8).collect()
            } else {
                t
            }
        });

    let tr = parse_transcript(app_data, &conv_id);
    let bridge: Option<BridgeState> = if is_valid_id(&conv_id) {
        fs::read_to_string(plugin_data.join("bridge").join(format!("{conv_id}.json")))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    } else {
        None
    };

    let mut ws_path = String::new();
    if let Some(first_uri) = workspace_uris.first()
        && let Some(stripped) = first_uri.strip_prefix("file://")
    {
        ws_path = stripped.to_string();
    }
    if ws_path.is_empty() {
        ws_path = bridge
            .as_ref()
            .and_then(|b| b.cwd.clone().or_else(|| b.workspace_paths.first().cloned()))
            .or_else(|| tr.as_ref().map(|t| t.detected_workspace.clone()))
            .unwrap_or_default();
    }

    let git = read_git_cached(&ws_path);
    let model_raw = bridge
        .as_ref()
        .and_then(|b| b.model_name.as_deref())
        .filter(|m| !m.is_empty() && *m != "auto" && *m != "None")
        .or_else(|| tr.as_ref().map(|t| t.detected_model.as_str()))
        .unwrap_or("Gemini Pro");
    let (rule, effort) = resolve_model(model_raw, &cfg);

    let calls = tr.as_ref().map(|t| t.calls.clone()).unwrap_or_default();
    let mut sum_in = 0u64;
    let mut sum_cache = 0u64;
    let mut sum_out = 0u64;
    let mut cost_in = 0.0f64;
    let mut cost_cache = 0.0f64;
    let mut cost_out = 0.0f64;
    let mut cache_saved = 0.0f64;
    let mut cache_misses = 0usize;
    let mut context_series = Vec::with_capacity(calls.len());
    let mut tps_series = Vec::new();
    let mut cost_samples: Vec<(i64, f64)> = Vec::new();
    let mut turn_items = Vec::with_capacity(calls.len());

    for (idx, c) in calls.iter().enumerate() {
        sum_in += c.fresh_in;
        sum_cache += c.cache_read;
        sum_out += c.out;

        let (iu, cu, ou, su) = calc_step_cost(c.fresh_in, c.cache_read, c.out, &rule);
        let step_usd = iu + cu + ou;
        cost_in += iu;
        cost_cache += cu;
        cost_out += ou;
        cache_saved += su;

        context_series.push(c.prompt_context);
        cost_samples.push((c.created_ms, cost_in + cost_cache + cost_out));

        if idx > 0 && c.prompt_context > 2000 {
            let prev_ctx = calls[idx - 1].prompt_context;
            if c.cache_read == 0 || (prev_ctx > 4000 && (c.cache_read as f64) < (prev_ctx as f64) * 0.25) {
                cache_misses += 1;
            }
        }

        let dur_sec = if c.duration_ms >= 250 && c.duration_ms <= 600_000 {
            c.duration_ms as f64 / 1000.0
        } else {
            0.0
        };
        let step_tps = if dur_sec > 0.0 && c.out > 0 {
            let t = c.out as f64 / dur_sec;
            tps_series.push(t);
            t
        } else {
            0.0
        };

        let time_hms = Local
            .timestamp_millis_opt(c.created_ms)
            .single()
            .map(|dt| dt.format("%H:%M:%S").to_string())
            .unwrap_or_default();

        let hit_pct = if c.prompt_context > 0 {
            (c.cache_read as f64 / c.prompt_context as f64) * 100.0
        } else {
            0.0
        };

        turn_items.push(TurnItem {
            step_index: c.step_index,
            time_hms,
            prompt_context: c.prompt_context,
            hit_pct,
            out_tokens: c.out,
            cost_usd: step_usd,
            tps: step_tps,
            duration_sec: dur_sec,
            label: c.label.clone(),
        });
    }

    let mut last_tps = tps_series.last().copied().unwrap_or(0.0);
    let mut last_dur_sec = calls
        .last()
        .map(|c| c.duration_ms as f64 / 1000.0)
        .unwrap_or(0.0);
    let mut tps_source = "transcript".to_string();

    if let (Some(b), Some(last_call)) = (&bridge, calls.last())
        && let (Some(inf_ms), Some(ended_ms)) = (b.last_inference_ms, b.last_inference_ended_ms)
        && (ended_ms - last_call.end_ms.max(last_call.created_ms)).abs() < 30_000
        && last_call.out > 0
        && inf_ms >= 100
    {
        last_dur_sec = inf_ms as f64 / 1000.0;
        last_tps = last_call.out as f64 / last_dur_sec;
        tps_source = "hook".into();
        if let Some(last_elem) = tps_series.last_mut() {
            *last_elem = last_tps;
        } else {
            tps_series.push(last_tps);
        }
        if let Some(last_item) = turn_items.last_mut() {
            last_item.tps = last_tps;
            last_item.duration_sec = last_dur_sec;
        }
    }

    let avg_tps = if tps_series.is_empty() {
        0.0
    } else {
        tps_series.iter().sum::<f64>() / tps_series.len() as f64
    };

    // Subagents
    let mut sub_in = 0u64;
    let mut sub_cache = 0u64;
    let mut sub_out = 0u64;
    let mut cost_sub = 0.0f64;
    let mut subagents = Vec::new();

    if let Some(ref main_tr) = tr {
        for sub_id in &main_tr.subagent_ids {
            if let Some(sub_tr) = parse_transcript(app_data, sub_id) {
                let (s_rule, _) = resolve_model(
                    if sub_tr.detected_model.is_empty() {
                        model_raw
                    } else {
                        &sub_tr.detected_model
                    },
                    &cfg,
                );
                let mut si = 0u64;
                let mut sc = 0u64;
                let mut so = 0u64;
                let mut su = 0.0f64;
                for c in &sub_tr.calls {
                    si += c.fresh_in;
                    sc += c.cache_read;
                    so += c.out;
                    let (a, b, d, _) = calc_step_cost(c.fresh_in, c.cache_read, c.out, &s_rule);
                    su += a + b + d;
                }
                sub_in += si;
                sub_cache += sc;
                sub_out += so;
                cost_sub += su;
                subagents.push(SubagentSummary {
                    id: sub_id.clone(),
                    model: s_rule.short_name,
                    total_tokens: si + sc + so,
                    cost_usd: su,
                    state: sub_tr.activity_state,
                });
            }
        }
    }

    let eff_in = sum_in + if cfg.include_subagents { sub_in } else { 0 };
    let eff_cache = sum_cache + if cfg.include_subagents { sub_cache } else { 0 };
    let eff_out = sum_out + if cfg.include_subagents { sub_out } else { 0 };
    let eff_total = eff_in + eff_cache + eff_out;
    let session_cost_usd = cost_in + cost_cache + cost_out + if cfg.include_subagents { cost_sub } else { 0.0 };

    let context_tokens = calls.last().map(|c| c.prompt_context).unwrap_or(0);
    let context_limit = rule.context_limit.max(1);
    let context_pct = (context_tokens as f64 / context_limit as f64) * 100.0;
    let context_left = context_limit.saturating_sub(context_tokens);
    let last_context_delta = if context_series.len() >= 2 {
        context_series[context_series.len() - 1] as i64 - context_series[context_series.len() - 2] as i64
    } else {
        context_tokens as i64
    };

    let turns_to_compact = if context_series.len() >= 3 && context_left > 0 {
        let slice = &context_series[context_series.len().saturating_sub(6)..];
        let delta = slice.last().copied().unwrap_or(0) as f64 - slice.first().copied().unwrap_or(0) as f64;
        let per_turn = delta / (slice.len() - 1) as f64;
        if per_turn > 300.0 {
            Some((context_left as f64 / per_turn).ceil() as u64)
        } else {
            None
        }
    } else {
        None
    };

    let last_cache_tokens = calls.last().map(|c| c.cache_read).unwrap_or(0);
    let last_cache_hit_pct = calls
        .last()
        .filter(|c| c.prompt_context > 0)
        .map(|c| (c.cache_read as f64 / c.prompt_context as f64) * 100.0)
        .unwrap_or(0.0);
    let session_cache_hit_pct = if sum_in + sum_cache > 0 {
        (sum_cache as f64 / (sum_in + sum_cache) as f64) * 100.0
    } else {
        0.0
    };

    let idle_sec = calls
        .last()
        .map(|c| ((now - c.end_ms.max(c.created_ms)) / 1000).max(0))
        .unwrap_or(999_999);
    let warm_left = (cfg.cache_ttl_sec as i64 - idle_sec).max(0);
    let cache_warm = !calls.is_empty() && warm_left > 0;

    // Burn rate ($/h)
    let hour_ago = now - 3_600_000;
    let recent_samples: Vec<&(i64, f64)> = cost_samples.iter().filter(|(t, _)| *t >= hour_ago).collect();
    let cost_rate_per_hr = if recent_samples.len() >= 2 {
        let (t0, c0) = *recent_samples[0];
        let span_sec = (now - t0) as f64 / 1000.0;
        let cur_c = cost_in + cost_cache + cost_out;
        if span_sec >= 60.0 && cur_c > c0 {
            Some(((cur_c - c0) / span_sec) * 3600.0)
        } else {
            None
        }
    } else {
        None
    };

    let session_dur_sec = tr
        .as_ref()
        .filter(|t| t.first_step_ms > 0)
        .map(|t| ((t.last_step_ms - t.first_step_ms) / 1000).max(0))
        .unwrap_or(0);

    let mut state = tr
        .as_ref()
        .map(|t| t.activity_state.clone())
        .unwrap_or_else(|| "idle".into());
    let mut activity_label = tr
        .as_ref()
        .map(|t| t.activity_label.clone())
        .unwrap_or_else(|| "Idle".into());
    if let Some(ref b) = bridge
        && b.req_pending
        && now - b.req_start_ms.unwrap_or(0) < 180_000
    {
        state = "thinking".into();
        activity_label = "Generating…".into();
    }

    let gw = compute_global_windows(app_data, &cfg, now);
    let quota_5h = make_quota_window(
        gw.five_h_tokens,
        gw.five_h_cost_usd,
        gw.five_h_earliest_ms,
        FIVE_HOURS_SEC,
        cfg.five_hour_token_limit,
        cfg.five_hour_usd_limit,
        &cfg.quota_mode,
        now,
    );
    let quota_7d = make_quota_window(
        gw.seven_d_tokens,
        gw.seven_d_cost_usd,
        gw.seven_d_earliest_ms,
        SEVEN_DAYS_SEC,
        cfg.seven_day_token_limit,
        cfg.seven_day_usd_limit,
        &cfg.quota_mode,
        now,
    );

    let etag = format!(
        "{conv_id}:{}:{}:{}:{}:{:.4}:{}:{}",
        tr.as_ref().map(|t| t.mtime_ms).unwrap_or(0),
        eff_total,
        state,
        warm_left / 5,
        gw.today_cost_usd,
        cfg.context_limit_override,
        git.as_ref().map(|g| format!("{}:{}:{}", g.branch, g.dirty, g.added)).unwrap_or_default()
    );

    TelemetryResponse {
        etag,
        conversation_id: conv_id,
        conversation_title: conv_title,
        recent_conversations: recents
            .into_iter()
            .map(|(id, title, _, _, _)| RecentConversation { id, title })
            .collect(),
        summary: TelemetrySummary {
            model_name: rule.display_name,
            model_short: rule.short_name,
            effort,
            price_in: rule.input_per_m,
            price_cache: rule.cache_read_per_m,
            price_out: rule.output_per_m,
            state,
            activity_label,
            workspace_dir: Path::new(&ws_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&ws_path)
                .to_string(),
            git_branch: git.as_ref().map(|g| g.branch.clone()),
            git_dirty: git.as_ref().is_some_and(|g| g.dirty),
            git_ahead: git.as_ref().map(|g| g.ahead).unwrap_or(0),
            git_behind: git.as_ref().map(|g| g.behind).unwrap_or(0),
            git_added: git.as_ref().map(|g| g.added).unwrap_or(0),
            git_deleted: git.as_ref().map(|g| g.deleted).unwrap_or(0),
            turns: calls.len(),
            steps: tr.as_ref().map(|t| t.total_steps).unwrap_or(0),
            compactions: tr.as_ref().map(|t| t.compactions).unwrap_or(0),
            context_tokens,
            context_limit,
            context_pct,
            context_left,
            last_context_delta,
            turns_to_compact,
            context_series,
            last_cache_tokens,
            last_cache_hit_pct,
            session_cache_hit_pct,
            cache_misses,
            cache_warm,
            cache_warm_left_fmt: fmt_duration(warm_left),
            cache_saved_usd: cache_saved,
            fresh_input_tokens: eff_in,
            cache_read_tokens: eff_cache,
            output_tokens: eff_out,
            total_tokens: eff_total,
            today_tokens: gw.today_tokens,
            session_cost_usd,
            cost_in_usd: cost_in,
            cost_cache_usd: cost_cache,
            cost_out_usd: cost_out,
            cost_sub_usd: cost_sub,
            cost_rate_per_hr,
            today_cost_usd: gw.today_cost_usd,
            last_tps,
            avg_tps,
            last_duration_sec: last_dur_sec,
            tps_source,
            tps_series,
            session_duration_fmt: fmt_duration(session_dur_sec),
            subagents,
            quota_5h,
            quota_7d,
        },
        turns: turn_items,
        config: cfg,
    }
}
