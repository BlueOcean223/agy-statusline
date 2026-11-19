//! Live data engine for the in-page statusline.
//!
//! Data comes from the language_server's read-only API, not from scraping the page:
//! - `GetAllCascadeTrajectories`: every conversation's run status, step count, parent/root, subagent spec.
//! - `GetCascadeTrajectoryGeneratorMetadata`: one record per model call with token usage,
//!   time to first token, streaming duration, response model and context window usage.
//!   `generatorMetadataOffset = N` returns records N.. so calls are fetched incrementally.
//! - `RetrieveUserQuotaSummary`: 5h / weekly quota buckets per model group.
//!
//! Hooks only tell the engine *when* to look (UDP datagram, see `hook.rs`), so a turn's numbers
//! show up right after the call ends instead of on the next poll.

use crate::config::{Config, ModelRule, find_app_data_dir, find_plugin_data_dir, load_config, write_atomic};
use crate::ls::LsClient;
use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUB_DONE_SHOW_MS: i64 = 10_000;
const MAX_SUBS: usize = 4;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn parse_ts(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

fn num(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Protobuf Duration in JSON form, e.g. "10.085751875s".
fn dur_ms(v: &Value) -> Option<f64> {
    let s = v.as_str()?;
    s.strip_suffix('s')?.parse::<f64>().ok().map(|x| x * 1000.0)
}

// ---------------------------------------------------------------- model calls

#[derive(Clone, Debug, Default)]
pub struct Call {
    pub display: String,
    pub resp_model: String,
    pub input: u64,
    pub cache: u64,
    pub output: u64,
    pub ctx_used: u64,
    pub ctx_max: u64,
    pub checkpoint: i64,
    pub ttft_ms: Option<f64>,
    pub stream_ms: Option<f64>,
    pub steps: Vec<u64>,
    pub has_usage: bool,
    pub at_ms: i64,
}

impl Call {
    fn from_json(g: &Value, seen_at: i64) -> Self {
        let cm = &g["chatModel"];
        let usage = &cm["usage"];
        let ctx = &cm["chatStartMetadata"]["contextWindowMetadata"];
        Call {
            display: cm["modelDisplayName"].as_str().unwrap_or("").to_string(),
            resp_model: cm["responseModel"].as_str().unwrap_or("").to_string(),
            input: num(&usage["inputTokens"]),
            cache: num(&usage["cacheReadTokens"]),
            output: num(&usage["outputTokens"]),
            ctx_used: num(&ctx["estimatedTokensUsed"]),
            ctx_max: num(&ctx["maxContextTokens"]),
            checkpoint: cm["chatStartMetadata"]["checkpointIndex"].as_i64().unwrap_or(-1),
            ttft_ms: dur_ms(&cm["timeToFirstToken"]),
            stream_ms: dur_ms(&cm["streamingDuration"]),
            steps: g["stepIndices"].as_array().map(|a| a.iter().map(num).collect()).unwrap_or_default(),
            has_usage: usage.is_object(),
            at_ms: seen_at,
        }
    }

    fn price_key(&self) -> String {
        format!("{} {}", self.resp_model, self.display).to_lowercase()
    }

    pub fn tps(&self) -> Option<f64> {
        let ms = self.stream_ms?;
        (ms > 50.0 && self.output > 0).then(|| self.output as f64 / (ms / 1000.0))
    }
}

fn price_for<'a>(rules: &'a [ModelRule], call: &Call) -> Option<&'a ModelRule> {
    let key = call.price_key();
    rules.iter().find(|r| !r.pattern.is_empty() && key.contains(&r.pattern.to_lowercase()))
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Cost {
    #[serde(rename = "in")]
    pub input: f64,
    pub cache: f64,
    pub out: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.cache + self.out
    }
}

fn call_cost(rules: &[ModelRule], c: &Call) -> Cost {
    let Some(r) = price_for(rules, c) else { return Cost::default() };
    Cost {
        input: c.input as f64 * r.input_per_m / 1e6,
        cache: c.cache as f64 * r.cache_read_per_m / 1e6,
        out: c.output as f64 * r.output_per_m / 1e6,
    }
}

// ---------------------------------------------------------------- conversations

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub status: String,
    pub step_count: u64,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub root: Option<String>,
    pub sub_type: Option<String>,
    pub sub_role: Option<String>,
}

impl Summary {
    pub fn active(&self) -> bool {
        ["RUNNING", "BUSY", "CANCELING"].iter().any(|s| self.status.contains(s))
    }
}

#[derive(Default)]
struct Conv {
    calls: Vec<Call>,
    loaded_steps: u64,
    loaded_once: bool,
    /// transcript.jsonl mtime → step_index → created_at, used to date calls loaded after the fact.
    times_mtime: i64,
    times: HashMap<u64, i64>,
}

#[derive(Clone, Debug, Default)]
struct HookState {
    event: String,
    at_ms: i64,
}

// ---------------------------------------------------------------- shared state (engine ⇄ CDP threads)

#[derive(Default)]
pub struct Shared {
    /// Conversation ids each connected page currently shows above an input box (page id → ids).
    pub panes: HashMap<String, Vec<String>>,
    /// Latest payload pushed to pages, and its revision.
    pub payload: String,
    pub rev: u64,
    /// Pages currently connected over CDP (page id → url), for the settings panel.
    pub pages: HashMap<String, String>,
    pub cdp_status: String,
    pub ls_status: String,
    /// Hook datagrams received since the engine last looked: (event, conversation id).
    pub hook_events: Vec<(String, String)>,
    pub wake: bool,
}

pub type SharedRef = Arc<(Mutex<Shared>, Condvar)>;

pub fn wake(shared: &SharedRef) {
    let (m, cv) = &**shared;
    if let Ok(mut s) = m.lock() {
        s.wake = true;
    }
    cv.notify_all();
}

// ---------------------------------------------------------------- engine

pub struct Engine {
    ls: LsClient,
    app_data: PathBuf,
    summaries: HashMap<String, Summary>,
    convs: HashMap<String, Conv>,
    hooks: HashMap<String, HookState>,
    dirty: HashSet<String>,
    /// When a subagent was first seen idle after being active (for the short "done" row).
    sub_done: HashMap<String, i64>,
    quota: Value,
    quota_at: Option<Instant>,
    summaries_at: Option<Instant>,
    pub last_error: Option<String>,
}

impl Engine {
    pub fn new(ls: LsClient) -> Self {
        Self {
            ls,
            app_data: find_app_data_dir(),
            summaries: HashMap::new(),
            convs: HashMap::new(),
            hooks: HashMap::new(),
            dirty: HashSet::new(),
            sub_done: HashMap::new(),
            quota: Value::Null,
            quota_at: None,
            summaries_at: None,
            last_error: None,
        }
    }

    pub fn on_hook(&mut self, event: &str, conv: &str) {
        self.hooks.insert(conv.to_string(), HookState { event: event.to_string(), at_ms: now_ms() });
        self.dirty.insert(conv.to_string());
        if event == "Stop" {
            self.quota_at = None;
        }
        self.summaries_at = None;
    }

    fn any_active(&self) -> bool {
        self.summaries.values().any(Summary::active)
            || self.hooks.values().any(|h| h.event != "Stop" && now_ms() - h.at_ms < 120_000)
    }

    fn refresh_summaries(&mut self) -> Result<(), String> {
        let j = self.ls.call("GetAllCascadeTrajectories", &json!({}))?;
        let Some(ts) = j["trajectorySummaries"].as_object() else { return Ok(()) };
        let now = now_ms();
        for (id, s) in ts {
            let tm = &s["trajectoryMetadata"];
            let spec = &tm["subagentSpec"];
            let next = Summary {
                status: s["status"].as_str().unwrap_or("").to_string(),
                step_count: num(&s["stepCount"]),
                created_ms: s["createdTime"].as_str().and_then(parse_ts).unwrap_or(0),
                modified_ms: s["lastModifiedTime"].as_str().and_then(parse_ts).unwrap_or(0),
                root: tm["rootConversationId"].as_str().filter(|r| *r != id).map(str::to_string),
                sub_type: spec["typeName"].as_str().map(str::to_string),
                sub_role: spec["role"].as_str().map(str::to_string),
            };
            if let Some(prev) = self.summaries.get(id) {
                if prev.step_count != next.step_count || prev.status != next.status {
                    self.dirty.insert(id.clone());
                }
                if prev.active() && !next.active() {
                    self.sub_done.insert(id.clone(), now);
                }
            }
            if next.active() {
                self.sub_done.remove(id);
            }
            self.summaries.insert(id.clone(), next);
        }
        Ok(())
    }

    fn refresh_conv(&mut self, id: &str) -> Result<(), String> {
        let now = now_ms();
        let conv = self.convs.entry(id.to_string()).or_default();
        // Re-read the last record: it may have been written before its usage was filled in.
        let offset = conv.calls.len().saturating_sub(1);
        let j = self.ls.call(
            "GetCascadeTrajectoryGeneratorMetadata",
            &json!({ "cascadeId": id, "generatorMetadataOffset": offset }),
        )?;
        let fresh: Vec<Call> = j["generatorMetadata"]
            .as_array()
            .map(|a| a.iter().map(|g| Call::from_json(g, now)).collect())
            .unwrap_or_default();
        let first_load = !conv.loaded_once;
        let prev_at = conv.calls.get(offset).map(|c| c.at_ms);
        conv.calls.truncate(offset);
        conv.calls.extend(fresh);
        if let (Some(t), Some(c)) = (prev_at, conv.calls.get_mut(offset)) {
            c.at_ms = t;
        }
        conv.loaded_once = true;
        if let Some(s) = self.summaries.get(id) {
            conv.loaded_steps = s.step_count;
        }
        if first_load {
            self.date_calls(id);
        }
        Ok(())
    }

    /// Calls loaded on first sight (history) get their real start time from transcript.jsonl;
    /// calls seen live keep the time they were first fetched.
    fn date_calls(&mut self, id: &str) {
        let created = self.summaries.get(id).map(|s| s.created_ms).unwrap_or(0);
        let modified = self.summaries.get(id).map(|s| s.modified_ms).unwrap_or(0);
        let path = self.app_data.join("brain").join(id).join(".system_generated/logs/transcript.jsonl");
        let conv = self.convs.get_mut(id).expect("conv exists");
        let mtime = file_mtime(&path);
        if mtime != 0 && mtime != conv.times_mtime {
            conv.times = read_step_times(&path);
            conv.times_mtime = mtime;
        }
        for c in &mut conv.calls {
            let t = c.steps.iter().filter_map(|s| conv.times.get(s)).copied().min();
            c.at_ms = t.unwrap_or(if modified > 0 { modified } else { created.max(c.at_ms) });
        }
    }

    fn refresh_quota(&mut self) -> Result<(), String> {
        let j = self.ls.call("RetrieveUserQuotaSummary", &json!({}))?;
        self.quota = j.get("response").cloned().unwrap_or(j);
        Ok(())
    }

    /// One engine step: refresh what is stale, then rebuild the payload for the given panes.
    pub fn tick(&mut self, panes: &HashSet<String>) {
        let active = self.any_active();
        let summary_every = if active || !self.dirty.is_empty() { 2 } else { 10 };
        let mut errors = Vec::new();
        if self.summaries_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(summary_every)) {
            match self.refresh_summaries() {
                Ok(()) => self.summaries_at = Some(Instant::now()),
                Err(e) => errors.push(e),
            }
        }
        if self.quota_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(180)) {
            match self.refresh_quota() {
                Ok(()) => self.quota_at = Some(Instant::now()),
                Err(e) => errors.push(e),
            }
        }

        // Which conversations need (re)loading: everything touched today (for today's cost),
        // the panes on screen and their subagents.
        let midnight = local_midnight_ms();
        let mut want: HashSet<String> = HashSet::new();
        for (id, s) in &self.summaries {
            if s.modified_ms >= midnight {
                want.insert(id.clone());
            }
        }
        for p in panes {
            want.insert(p.clone());
            let root = self.root_of(p);
            want.insert(root.clone());
            for (id, s) in &self.summaries {
                if s.root.as_deref() == Some(root.as_str()) {
                    want.insert(id.clone());
                }
            }
        }
        let mut todo: Vec<String> = want
            .into_iter()
            .filter(|id| {
                let conv = self.convs.get(id);
                let loaded = conv.is_some_and(|c| c.loaded_once);
                let steps_changed = match (conv, self.summaries.get(id)) {
                    (Some(c), Some(s)) => c.loaded_steps != s.step_count,
                    _ => false,
                };
                let running_on_screen = self.summaries.get(id).is_some_and(Summary::active) && panes.contains(id);
                !loaded || steps_changed || self.dirty.contains(id) || running_on_screen
            })
            .collect();
        todo.sort();
        for id in todo {
            if let Err(e) = self.refresh_conv(&id) {
                errors.push(e);
            }
            self.dirty.remove(&id);
        }
        self.last_error = errors.into_iter().next();
    }

    pub fn root_of(&self, id: &str) -> String {
        let mut cur = id.to_string();
        for _ in 0..8 {
            match self.summaries.get(&cur).and_then(|s| s.root.clone()) {
                Some(r) if r != cur => cur = r,
                _ => break,
            }
        }
        cur
    }

    fn is_running(&self, id: &str) -> bool {
        if self.summaries.get(id).is_some_and(Summary::active) {
            return true;
        }
        // Hooks fire before the next status poll; trust a fresh PreInvocation/PostToolUse.
        self.hooks.get(id).is_some_and(|h| h.event != "Stop" && now_ms() - h.at_ms < 15_000)
    }

    fn tree(&self, root: &str) -> Vec<String> {
        let mut ids = vec![root.to_string()];
        for (id, s) in &self.summaries {
            if id != root && s.root.as_deref() == Some(root) {
                ids.push(id.clone());
            }
        }
        ids
    }

    fn last_call(&self, id: &str) -> Option<&Call> {
        self.convs.get(id)?.calls.iter().rev().find(|c| c.has_usage)
    }

    fn quota_for(&self, call: Option<&Call>) -> Vec<QuotaView> {
        let groups = self.quota["groups"].as_array().cloned().unwrap_or_default();
        if groups.is_empty() {
            return Vec::new();
        }
        let name = call.map(|c| model_family(&c.display)).unwrap_or_default();
        let group = groups
            .iter()
            .find(|g| !name.is_empty() && g["description"].as_str().unwrap_or("").contains(&name))
            .or_else(|| {
                // Claude/GPT models live in the third-party group.
                let third = call.is_some_and(|c| {
                    let k = c.price_key();
                    k.contains("claude") || k.contains("gpt")
                });
                groups.iter().find(|g| {
                    let d = g["displayName"].as_str().unwrap_or("").to_lowercase();
                    if third { d.contains("claude") } else { d.contains("gemini") }
                })
            })
            .unwrap_or(&groups[0]);
        quota_views(group)
    }

    pub fn build_payload(&self, panes: &HashSet<String>, cfg: &Config) -> Value {
        let rules = &cfg.models;
        let midnight = local_midnight_ms();
        let mut today = 0.0;
        for conv in self.convs.values() {
            for c in &conv.calls {
                if c.at_ms >= midnight {
                    today += call_cost(rules, c).total();
                }
            }
        }

        let mut views = serde_json::Map::new();
        for id in panes {
            views.insert(id.clone(), serde_json::to_value(self.view(id, cfg)).unwrap_or(Value::Null));
        }
        let groups: Vec<Value> = self.quota["groups"]
            .as_array()
            .map(|gs| {
                gs.iter()
                    .map(|g| {
                        json!({
                            "name": g["displayName"],
                            "models": g["description"],
                            "buckets": quota_views(g),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "today": today,
            "quotaGroups": groups,
            "views": views,
            "opts": {
                "enabled": cfg.bar_enabled,
                "cost": cfg.bar_show_cost,
                "subagents": cfg.bar_show_subagents,
                "quota": cfg.bar_show_quota,
            },
            "error": self.last_error,
        })
    }

    fn agent_line(&self, id: &str, cfg: &Config) -> AgentLine {
        let calls = self.convs.get(id).map(|c| c.calls.as_slice()).unwrap_or(&[]);
        let last = self.last_call(id);
        let ctx_max = if cfg.context_limit_override > 0 {
            cfg.context_limit_override
        } else {
            last.map(|c| c.ctx_max).unwrap_or(0)
        };
        // Compaction count, two signals (whichever sees more): checkpointIndex moving forward,
        // or the context estimate falling by more than 40% between consecutive calls.
        // TODO: confirm both against a conversation that actually got compacted.
        let mut by_ck = 0;
        let mut by_drop = 0;
        let mut prev_ck = -1;
        let mut prev_used = 0u64;
        for c in calls.iter().filter(|c| c.has_usage) {
            if c.checkpoint > prev_ck {
                if c.checkpoint >= 0 {
                    by_ck += 1;
                }
                prev_ck = c.checkpoint;
            }
            if prev_used > 100_000 && c.ctx_used > 0 && (c.ctx_used as f64) < prev_used as f64 * 0.6 {
                by_drop += 1;
            }
            if c.ctx_used > 0 {
                prev_used = c.ctx_used;
            }
        }
        let compactions = by_ck.max(by_drop);
        let s = self.summaries.get(id);
        AgentLine {
            id: id.to_string(),
            sub_type: s.and_then(|s| s.sub_type.clone()),
            sub_role: s.and_then(|s| s.sub_role.clone()),
            running: self.is_running(id),
            model: last.map(|c| c.display.clone()),
            resp_model: last.map(|c| c.resp_model.clone()),
            ctx_used: last.map(|c| c.ctx_used),
            ctx_max,
            compactions,
            cache_read: last.map(|c| c.cache),
            cache_hit: last.and_then(|c| {
                let denom = c.cache + c.input;
                (denom > 0).then(|| c.cache as f64 / denom as f64)
            }),
            ttft_ms: last.and_then(|c| c.ttft_ms),
            tps: last.and_then(Call::tps),
            out_tokens: last.map(|c| c.output),
            last_at: last.map(|c| c.at_ms),
            calls: calls.iter().filter(|c| c.has_usage).count(),
        }
    }

    fn view(&self, id: &str, cfg: &Config) -> View {
        let rules = &cfg.models;
        let root = self.root_of(id);
        let is_sub = root != id;
        let main = self.agent_line(id, cfg);
        let ids = if is_sub { vec![id.to_string()] } else { self.tree(&root) };
        let mut cost_conv = 0.0;
        for cid in &ids {
            if let Some(conv) = self.convs.get(cid) {
                cost_conv += conv.calls.iter().map(|c| call_cost(rules, c).total()).sum::<f64>();
            }
        }
        let last = self.last_call(id);
        let cost_last = last.map(|c| call_cost(rules, c));
        let price = last.and_then(|c| price_for(rules, c)).map(|r| r.display_name.clone());

        let mut subs = Vec::new();
        if !is_sub {
            let now = now_ms();
            let mut ids: Vec<&String> = self
                .summaries
                .iter()
                .filter(|(sid, s)| *sid != id && s.root.as_deref() == Some(root.as_str()))
                .filter(|(sid, _)| {
                    self.is_running(sid) || self.sub_done.get(*sid).is_some_and(|t| now - t < SUB_DONE_SHOW_MS)
                })
                .map(|(sid, _)| sid)
                .collect();
            ids.sort_by_key(|sid| (!self.is_running(sid), std::cmp::Reverse(self.last_call(sid).map(|c| c.at_ms))));
            let more = ids.len().saturating_sub(MAX_SUBS);
            subs = ids.into_iter().take(MAX_SUBS).map(|sid| self.agent_line(sid, cfg)).collect();
            if more > 0 {
                subs.push(AgentLine { id: format!("+{more}"), ..Default::default() });
            }
        }

        View {
            main,
            is_sub,
            cost_conv,
            cost_last,
            price_name: price,
            quota: self.quota_for(last),
            subs,
        }
    }

    pub fn summaries(&self) -> &HashMap<String, Summary> {
        &self.summaries
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLine {
    pub id: String,
    pub sub_type: Option<String>,
    pub sub_role: Option<String>,
    pub running: bool,
    pub model: Option<String>,
    pub resp_model: Option<String>,
    pub ctx_used: Option<u64>,
    pub ctx_max: u64,
    pub compactions: usize,
    pub cache_read: Option<u64>,
    pub cache_hit: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub tps: Option<f64>,
    pub out_tokens: Option<u64>,
    pub last_at: Option<i64>,
    pub calls: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub main: AgentLine,
    pub is_sub: bool,
    pub cost_conv: f64,
    pub cost_last: Option<Cost>,
    pub price_name: Option<String>,
    pub quota: Vec<QuotaView>,
    pub subs: Vec<AgentLine>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaView {
    pub window: String,
    pub used: f64,
    pub reset_ms: Option<i64>,
    /// Used fraction minus elapsed fraction of the window; > 0 means burning faster than even pace.
    pub pace: Option<f64>,
}

fn quota_views(group: &Value) -> Vec<QuotaView> {
    let mut out: Vec<QuotaView> = group["buckets"]
        .as_array()
        .map(|bs| {
            bs.iter()
                .map(|b| {
                    let window = b["window"].as_str().unwrap_or("").to_string();
                    let remaining = b["remainingFraction"].as_f64().unwrap_or(1.0).clamp(0.0, 1.0);
                    let reset_ms = b["resetTime"].as_str().and_then(parse_ts);
                    let span_ms = match window.as_str() {
                        "5h" => Some(5 * 3_600_000),
                        "weekly" => Some(7 * 86_400_000),
                        _ => None,
                    };
                    let pace = match (reset_ms, span_ms) {
                        (Some(r), Some(span)) => {
                            let elapsed = 1.0 - ((r - now_ms()) as f64 / span as f64).clamp(0.0, 1.0);
                            Some((1.0 - remaining) - elapsed)
                        }
                        _ => None,
                    };
                    QuotaView { window, used: 1.0 - remaining, reset_ms, pace }
                })
                .collect()
        })
        .unwrap_or_default();
    // 5h first, then weekly, like ccline.
    out.sort_by_key(|q| if q.window == "5h" { 0 } else { 1 });
    out
}

/// "Gemini 4 Neon (XHigh)" → "Gemini 4 Neon"; matches the quota group's model list.
fn model_family(display: &str) -> String {
    display.split(" (").next().unwrap_or(display).trim().to_string()
}

fn local_midnight_ms() -> i64 {
    Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|d| d.and_local_timezone(Local).earliest())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

fn file_mtime(p: &Path) -> i64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn read_step_times(p: &Path) -> HashMap<u64, i64> {
    let mut out = HashMap::new();
    let Ok(text) = fs::read_to_string(p) else { return out };
    for line in text.lines() {
        if !line.contains("\"created_at\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let (Some(i), Some(t)) = (v["step_index"].as_u64(), v["created_at"].as_str().and_then(parse_ts)) {
            out.insert(i, t);
        }
    }
    out
}

// ---------------------------------------------------------------- daemon loop

/// Port file the hook reads to find the UDP socket.
pub fn port_file(plugin_data: &Path) -> PathBuf {
    plugin_data.join("live.port")
}

pub fn run(shared: SharedRef) {
    let app_data = find_app_data_dir();
    let plugin_data = find_plugin_data_dir(&app_data);
    spawn_hook_listener(&plugin_data, shared.clone());

    let mut engine: Option<Engine> = None;
    let mut last_payload = String::new();
    loop {
        if engine.is_none() {
            match LsClient::connect() {
                Some(ls) => {
                    let addr = ls.addr.clone();
                    set_status(&shared, |s| s.ls_status = format!("connected to {addr}"));
                    engine = Some(Engine::new(ls));
                }
                None => set_status(&shared, |s| s.ls_status = "language_server not found".into()),
            }
        }

        let (panes, events) = {
            let (m, _) = &*shared;
            m.lock()
                .map(|mut s| {
                    let panes: HashSet<String> = s.panes.values().flatten().cloned().collect();
                    (panes, std::mem::take(&mut s.hook_events))
                })
                .unwrap_or_default()
        };

        if let Some(eng) = engine.as_mut() {
            for (e, c) in &events {
                eng.on_hook(e, c);
            }
            let cfg = load_config(&plugin_data);
            eng.tick(&panes);
            let payload = eng.build_payload(&panes, &cfg).to_string();
            if payload != last_payload {
                last_payload = payload.clone();
                let (m, cv) = &*shared;
                if let Ok(mut s) = m.lock() {
                    s.payload = payload;
                    s.rev += 1;
                }
                cv.notify_all();
            }
            let err = eng.last_error.clone();
            let addr = eng.ls.addr.clone();
            set_status(&shared, |s| {
                s.ls_status = match &err {
                    Some(e) => format!("error: {e}"),
                    None => format!("connected to {addr}"),
                };
            });
            // language_server restarted (new port/token): rebuild the client next round.
            if err.as_deref().is_some_and(|e| e.contains("refused") || e.contains("HTTP 401")) {
                engine = None;
            }
        }

        // Sleep up to 1s; hook datagrams and page pane changes wake the loop early.
        let (m, cv) = &*shared;
        if let Ok(guard) = m.lock() {
            let (mut g, _) = cv
                .wait_timeout_while(guard, Duration::from_millis(1000), |s| !s.wake)
                .unwrap_or_else(|e| e.into_inner());
            g.wake = false;
        }
    }
}

/// Hooks send `{"e": event, "c": conversationId}` to this socket; the port is written to `live.port`.
fn spawn_hook_listener(plugin_data: &Path, shared: SharedRef) {
    let Ok(sock) = UdpSocket::bind("127.0.0.1:0") else { return };
    if let Ok(addr) = sock.local_addr() {
        let _ = write_atomic(&port_file(plugin_data), &addr.port().to_string());
    }
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok((n, _)) = sock.recv_from(&mut buf) {
            let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) else { continue };
            let (Some(e), Some(c)) = (v["e"].as_str(), v["c"].as_str()) else { continue };
            if let Ok(mut s) = shared.0.lock() {
                s.hook_events.push((e.to_string(), c.to_string()));
                s.wake = true;
            }
            shared.1.notify_all();
        }
    });
}

fn set_status(shared: &SharedRef, f: impl FnOnce(&mut Shared)) {
    if let Ok(mut s) = shared.0.lock() {
        f(&mut s);
    }
}
