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
use crate::git::GitInfo;
use crate::ls::LsClient;
use crate::transcript::Transcript;
use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUB_DONE_SHOW_MS: i64 = 10_000;
const MAX_SUBS: usize = 4;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn parse_ts(s: &str) -> Option<i64> {
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
    /// True once `at_ms` has been resolved from `transcript.jsonl`'s `created_at`.
    pub dated: bool,
    /// True when this call first appeared incrementally after the conversation was already loaded.
    pub live_seen: bool,
    /// From transcript.jsonl: what the model did with this response.
    pub label: Option<String>,
}

impl Call {
    pub(crate) fn from_json(g: &Value, seen_at: i64, live_seen: bool) -> Self {
        let cm = &g["chatModel"];
        let usage = &cm["usage"];
        let ctx = &cm["chatStartMetadata"]["contextWindowMetadata"];
        let input = num(&usage["inputTokens"]);
        let cache = num(&usage["cacheReadTokens"]);
        let output = num(&usage["outputTokens"]);
        Call {
            display: cm["modelDisplayName"].as_str().unwrap_or("").to_string(),
            resp_model: cm["responseModel"].as_str().unwrap_or("").to_string(),
            input,
            cache,
            output,
            ctx_used: num(&ctx["estimatedTokensUsed"]),
            ctx_max: num(&ctx["maxContextTokens"]),
            checkpoint: cm["chatStartMetadata"]["checkpointIndex"].as_i64().unwrap_or(-1),
            ttft_ms: dur_ms(&cm["timeToFirstToken"]),
            stream_ms: dur_ms(&cm["streamingDuration"]),
            steps: g["stepIndices"].as_array().map(|a| a.iter().map(num).collect()).unwrap_or_default(),
            // Aborted/placeholder records can carry an empty `usage: {}` object with all-zero tokens.
            has_usage: usage.is_object() && (input > 0 || cache > 0 || output > 0),
            at_ms: if live_seen { seen_at } else { 0 },
            dated: false,
            live_seen,
            label: None,
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

pub fn call_cost(rules: &[ModelRule], c: &Call) -> Cost {
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
    pub modified_ms: i64,
    pub root: Option<String>,
    pub sub_type: Option<String>,
    pub sub_role: Option<String>,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub workspace_path: Option<String>,
}

impl Summary {
    pub fn active(&self) -> bool {
        ["RUNNING", "BUSY", "CANCELING"].iter().any(|s| self.status.contains(s))
    }
}

#[derive(Default)]
pub struct Conv {
    pub calls: Vec<Call>,
    loaded_steps: u64,
    loaded_once: bool,
    last_conv_mod: i64,
    transcript: Option<Transcript>,
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
    /// Mirrors `config.ui_zh` for the CDP threads.
    pub ui_zh: bool,
    /// Conversations currently running, for the restart button's warning.
    pub running: usize,
    /// Conversations the side panel asked about, with the time of its last poll.
    pub panel: HashMap<String, Instant>,
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
    pub(crate) ls: LsClient,
    pub(crate) app_data: PathBuf,
    pub(crate) summaries: HashMap<String, Summary>,
    pub(crate) convs: HashMap<String, Conv>,
    hooks: HashMap<String, HookState>,
    pub(crate) dirty: HashSet<String>,
    /// When a subagent was first seen idle after being active (for the short "done" row).
    pub(crate) sub_done: HashMap<String, i64>,
    pub(crate) quota: Value,
    pub(crate) quota_at: Option<Instant>,
    pub(crate) summaries_at: Option<Instant>,
    /// Conversation id → transcript mtime, from `brain/` (see `transcript::scan_brain`).
    pub(crate) disk: HashMap<String, i64>,
    disk_at: Option<Instant>,
    /// Candidate subagent ids from transcripts already probed via `refresh_conv`.
    probed_hints: HashSet<String>,
    /// Workspace path → (when read, git status).
    git: HashMap<String, (Instant, Option<GitInfo>)>,
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
            disk: HashMap::new(),
            disk_at: None,
            probed_hints: HashSet::new(),
            git: HashMap::new(),
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

    pub(crate) fn any_active(&self) -> bool {
        self.summaries.values().any(Summary::active)
            || self.hooks.values().any(|h| h.event != "Stop" && now_ms() - h.at_ms < 120_000)
    }

    pub(crate) fn refresh_summaries(&mut self) -> Result<(), String> {
        let j = self.ls.call("GetAllCascadeTrajectories", &json!({}))?;
        let Some(ts) = j["trajectorySummaries"].as_object() else { return Ok(()) };
        let now = now_ms();
        for (id, s) in ts {
            let tm = &s["trajectoryMetadata"];
            let spec = &tm["subagentSpec"];
            let next = Summary {
                status: s["status"].as_str().unwrap_or("").to_string(),
                step_count: num(&s["stepCount"]),
                modified_ms: s["lastModifiedTime"].as_str().and_then(parse_ts).unwrap_or(0),
                root: tm["rootConversationId"].as_str().filter(|r| *r != id).map(str::to_string),
                sub_type: spec["typeName"].as_str().map(str::to_string),
                sub_role: spec["role"].as_str().map(str::to_string),
                workspace: s["workspaces"][0]["repository"]["computedName"]
                    .as_str()
                    .or_else(|| {
                        s["workspaces"][0]["workspaceFolderAbsoluteUri"].as_str().and_then(|u| u.rsplit('/').next())
                    })
                    .map(str::to_string),
                branch: s["workspaces"][0]["branchName"].as_str().map(str::to_string),
                workspace_path: s["workspaces"][0]["workspaceFolderAbsoluteUri"]
                    .as_str()
                    .and_then(crate::git::uri_to_path),
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

    pub(crate) fn refresh_conv(&mut self, id: &str) -> Result<(), String> {
        let now = now_ms();
        let conv = self.convs.entry(id.to_string()).or_default();
        let was_loaded = conv.loaded_once;
        // Re-read the last record: it may have been written before its usage was filled in.
        let offset = conv.calls.len().saturating_sub(1);
        let j = self.ls.call(
            "GetCascadeTrajectoryGeneratorMetadata",
            &json!({ "cascadeId": id, "generatorMetadataOffset": offset }),
        )?;
        let prev_last = conv.calls.get(offset).map(|c| (c.at_ms, c.dated, c.live_seen));
        let fresh: Vec<Call> = j["generatorMetadata"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, g)| {
                        let live_seen = match prev_last {
                            Some((_, _, ls)) if i == 0 => ls,
                            _ => was_loaded,
                        };
                        Call::from_json(g, now, live_seen)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if fresh.is_empty() && offset < conv.calls.len() {
            return Ok(());
        }
        conv.calls.truncate(offset);
        conv.calls.extend(fresh);
        if let (Some((t, dated, live_seen)), Some(c)) = (prev_last, conv.calls.get_mut(offset)) {
            c.at_ms = t;
            c.dated = dated;
            c.live_seen = live_seen;
        }
        conv.loaded_once = true;
        if let Some(s) = self.summaries.get(id) {
            conv.loaded_steps = s.step_count;
        }
        self.sync_transcript(id, true);
        Ok(())
    }

    /// Dates calls and labels turns from transcript.jsonl (read incrementally).
    /// Undated calls fall back to neighbouring calls or the conversation's modified time rather
    /// than `now`, so loading a historical conversation never pollutes today's cost.
    pub(crate) fn sync_transcript(&mut self, id: &str, force: bool) {
        let app_data = self.app_data.clone();
        let summary_mod = self.summaries.get(id).map(|s| s.modified_ms).unwrap_or(0);
        let disk_mod = self.disk.get(id).copied().unwrap_or(0);
        let Some(conv) = self.convs.get_mut(id) else { return };
        let t = conv.transcript.get_or_insert_with(|| Transcript::new(&app_data, id));
        let parsed_new = t.sync();
        let file_mod = if disk_mod > 0 { disk_mod } else { t.mtime_ms() };
        let conv_mod = summary_mod.max(file_mod);
        if force || parsed_new || conv_mod != conv.last_conv_mod {
            conv.last_conv_mod = conv_mod;
            date_calls(&mut conv.calls, &t.steps, conv_mod);
        }
    }

    /// Probes subagent ids referenced in the transcripts of `seeds` that the language_server has
    /// not loaded yet (e.g. historical subagents of a conversation opened after restart).
    /// Returns true if any new conversation was loaded.
    pub(crate) fn load_subagent_hints(&mut self, seeds: &[String]) -> bool {
        let mut loaded_any = false;
        let mut queue: Vec<String> = seeds.to_vec();
        let mut seen: HashSet<String> = queue.iter().cloned().collect();
        for _ in 0..3 {
            let mut next_queue = Vec::new();
            for cid in queue {
                let hints: Vec<String> = self
                    .convs
                    .get(&cid)
                    .and_then(|c| c.transcript.as_ref())
                    .map(|t| t.subagent_hints.iter().cloned().collect())
                    .unwrap_or_default();
                for sid in hints {
                    if sid == cid || self.convs.contains_key(&sid) || !self.probed_hints.insert(sid.clone()) {
                        continue;
                    }
                    if !self.disk.contains_key(&sid) && !self.app_data.join("brain").join(&sid).is_dir() {
                        continue;
                    }
                    if self.refresh_conv(&sid).is_ok() {
                        loaded_any = true;
                        if seen.insert(sid.clone()) {
                            next_queue.push(sid);
                        }
                    }
                }
            }
            if next_queue.is_empty() {
                break;
            }
            queue = next_queue;
        }
        loaded_any
    }

    pub(crate) fn refresh_quota(&mut self) -> Result<(), String> {
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
        if self.disk_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(30)) {
            self.disk = crate::transcript::scan_brain(&self.app_data);
            self.disk_at = Some(Instant::now());
        }
        let midnight = local_midnight_ms();
        let mut want: HashSet<String> = HashSet::new();
        for (id, s) in &self.summaries {
            if s.modified_ms >= midnight {
                want.insert(id.clone());
            }
        }
        for (id, mtime) in &self.disk {
            if *mtime >= midnight {
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
        let mut loaded_unknown = false;
        for id in todo {
            let was_in_summaries = self.summaries.contains_key(&id);
            match self.refresh_conv(&id) {
                Ok(()) => {
                    if !was_in_summaries {
                        loaded_unknown = true;
                    }
                }
                Err(e) => errors.push(e),
            }
            self.dirty.remove(&id);
        }

        // Backfill historical subagents for panes on screen so restarts don't omit subagent cost.
        let mut seeds: Vec<String> = Vec::new();
        for p in panes {
            seeds.push(p.clone());
            let root = self.root_of(p);
            if root != *p {
                seeds.push(root);
            }
        }
        if self.load_subagent_hints(&seeds) {
            loaded_unknown = true;
        }
        if loaded_unknown {
            match self.refresh_summaries() {
                Ok(()) => {
                    self.summaries_at = Some(Instant::now());
                    let mut extra: Vec<String> = Vec::new();
                    for p in panes {
                        let root = self.root_of(p);
                        for cid in self.tree(&root) {
                            if !self.convs.contains_key(&cid) {
                                extra.push(cid);
                            }
                        }
                    }
                    extra.sort();
                    extra.dedup();
                    for cid in extra {
                        if let Err(e) = self.refresh_conv(&cid) {
                            errors.push(e);
                        }
                    }
                }
                Err(e) => errors.push(e),
            }
        }

        // Pick up late-flushed transcript.jsonl entries (timestamps/labels) without an LS RPC.
        let sync_ids: Vec<String> = self
            .convs
            .iter()
            .filter(|(_, c)| c.calls.iter().any(|call| call.has_usage && (!call.dated || call.label.is_none())))
            .map(|(id, _)| id.clone())
            .collect();
        for id in sync_ids {
            self.sync_transcript(&id, false);
        }
        self.last_error = errors.into_iter().next();

        // Git status for the workspaces on screen, at most every 5s per workspace.
        let paths: HashSet<String> =
            panes.iter().filter_map(|id| self.summaries.get(id)?.workspace_path.clone()).collect();
        for path in paths {
            if self.git.get(&path).is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(5)) {
                let info = crate::git::read(&path);
                self.git.insert(path, (Instant::now(), info));
            }
        }
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

    pub(crate) fn is_running(&self, id: &str) -> bool {
        if self.summaries.get(id).is_some_and(Summary::active) {
            return true;
        }
        // Hooks fire before the next status poll; trust a fresh PreInvocation/PostToolUse.
        self.hooks.get(id).is_some_and(|h| h.event != "Stop" && now_ms() - h.at_ms < 15_000)
    }

    pub(crate) fn tree(&self, root: &str) -> Vec<String> {
        let mut ids = vec![root.to_string()];
        for (id, s) in &self.summaries {
            if id != root && s.root.as_deref() == Some(root) {
                ids.push(id.clone());
            }
        }
        ids
    }

    pub(crate) fn last_call(&self, id: &str) -> Option<&Call> {
        self.convs.get(id)?.calls.iter().rev().find(|c| c.has_usage)
    }

    pub(crate) fn quota_for(&self, call: Option<&Call>) -> Vec<QuotaView> {
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
            for c in conv.calls.iter().filter(|c| c.has_usage && c.at_ms >= midnight) {
                today += call_cost(rules, c).total();
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
                "seg": cfg.bar_segments,
            },
            "error": self.last_error,
        })
    }

    pub(crate) fn agent_line(&self, id: &str, cfg: &Config) -> AgentLine {
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

    pub(crate) fn view(&self, id: &str, cfg: &Config) -> View {
        let rules = &cfg.models;
        let root = self.root_of(id);
        let is_sub = root != id;
        let main = self.agent_line(id, cfg);
        let includes_subagents = !is_sub && cfg.include_subagents;
        let ids = if includes_subagents { self.tree(&root) } else { vec![id.to_string()] };
        let midnight = local_midnight_ms();
        let mut cost_conv = 0.0;
        let mut cost_conv_today = 0.0;
        let mut multi_day = false;
        for cid in &ids {
            if let Some(conv) = self.convs.get(cid) {
                for c in conv.calls.iter().filter(|c| c.has_usage) {
                    let cost = call_cost(rules, c).total();
                    cost_conv += cost;
                    if c.at_ms >= midnight {
                        cost_conv_today += cost;
                    } else if c.at_ms > 0 {
                        multi_day = true;
                    }
                }
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

        let summary = self.summaries.get(id);
        let git =
            summary.and_then(|s| s.workspace_path.as_ref()).and_then(|p| self.git.get(p)).and_then(|(_, g)| g.clone());
        View {
            git,
            workspace: summary.and_then(|s| s.workspace_path.clone()),
            main,
            is_sub,
            cost_conv,
            cost_conv_today,
            multi_day,
            includes_subagents,
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

pub(crate) fn date_calls(calls: &mut [Call], steps: &HashMap<u64, crate::transcript::StepInfo>, conv_mod_ms: i64) {
    for c in calls.iter_mut() {
        let infos: Vec<_> = c.steps.iter().filter_map(|s| steps.get(s)).collect();
        if let Some(at) = infos.iter().map(|i| i.at_ms).filter(|&t| t > 0).min() {
            c.at_ms = at;
            c.dated = true;
        }
        if let Some(l) = infos.iter().find_map(|i| i.label.clone()) {
            c.label = Some(l);
        }
    }
    let last_dated_idx = calls.iter().rposition(|c| c.dated);
    let mut prev_dated = 0i64;
    for i in 0..calls.len() {
        if calls[i].dated {
            prev_dated = calls[i].at_ms;
            continue;
        }
        if calls[i].live_seen && calls[i].at_ms > 0 {
            continue;
        }
        let fallback = if last_dated_idx.is_none_or(|last| i > last) {
            conv_mod_ms.max(prev_dated)
        } else if prev_dated > 0 {
            prev_dated
        } else {
            calls[i + 1..].iter().find(|c| c.dated).map(|c| c.at_ms).unwrap_or(conv_mod_ms)
        };
        calls[i].at_ms = fallback;
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
    pub cost_conv_today: f64,
    pub multi_day: bool,
    pub includes_subagents: bool,
    pub cost_last: Option<Cost>,
    pub price_name: Option<String>,
    pub quota: Vec<QuotaView>,
    pub subs: Vec<AgentLine>,
    pub git: Option<GitInfo>,
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaView {
    pub window: String,
    pub used: f64,
    pub reset_ms: Option<i64>,
    /// Used fraction minus elapsed fraction of the window; > 0 means burning faster than even pace.
    pub pace: Option<f64>,
    /// Fraction of the window already elapsed.
    pub elapsed: Option<f64>,
}

pub(crate) fn quota_views(group: &Value) -> Vec<QuotaView> {
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
                    let elapsed = match (reset_ms, span_ms) {
                        (Some(r), Some(span)) => Some(1.0 - ((r - now_ms()) as f64 / span as f64).clamp(0.0, 1.0)),
                        _ => None,
                    };
                    // Rounded so the pushed payload only changes when the shown numbers can.
                    let elapsed = elapsed.map(|e| (e * 1000.0).round() / 1000.0);
                    let pace = elapsed.map(|e| (((1.0 - remaining) - e) * 1000.0).round() / 1000.0);
                    QuotaView { window, used: 1.0 - remaining, reset_ms, pace, elapsed }
                })
                .collect()
        })
        .unwrap_or_default();
    // 5h first, then weekly.
    out.sort_by_key(|q| if q.window == "5h" { 0 } else { 1 });
    out
}

/// "Gemini 4 Neon (XHigh)" → "Gemini 4 Neon"; matches the quota group's model list.
pub(crate) fn model_family(display: &str) -> String {
    display.split(" (").next().unwrap_or(display).trim().to_string()
}

pub(crate) fn local_midnight_ms() -> i64 {
    Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|d| d.and_local_timezone(Local).earliest())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- daemon loop

/// Port file the hook reads to find the UDP socket.
pub fn port_file(plugin_data: &Path) -> PathBuf {
    plugin_data.join("live.port")
}

/// The engine is shared with the side panel's HTTP handlers, which read details from it directly.
pub static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

const LS_GONE_AFTER: Duration = Duration::from_secs(20);

pub fn run(shared: SharedRef) {
    let app_data = find_app_data_dir();
    let plugin_data = find_plugin_data_dir(&app_data);
    spawn_hook_listener(&plugin_data, shared.clone());

    let mut last_payload = String::new();
    loop {
        let mut engine = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        if engine.is_none() {
            match LsClient::connect() {
                Some(ls) => {
                    let addr = ls.addr.clone();
                    set_status(&shared, |s| s.ls_status = format!("connected to {addr}"));
                    *engine = Some(Engine::new(ls));
                }
                None => set_status(&shared, |s| s.ls_status = "language_server not found".into()),
            }
        }

        let (panes, interest, events) = {
            let (m, _) = &*shared;
            m.lock()
                .map(|mut s| {
                    let panes: HashSet<String> = s.panes.values().flatten().cloned().collect();
                    s.panel.retain(|_, at| at.elapsed() < Duration::from_secs(15));
                    let mut interest = panes.clone();
                    interest.extend(s.panel.keys().cloned());
                    (panes, interest, std::mem::take(&mut s.hook_events))
                })
                .unwrap_or_default()
        };

        if let Some(eng) = engine.as_mut() {
            for (e, c) in &events {
                eng.on_hook(e, c);
            }
            let cfg = load_config(&plugin_data);
            eng.tick(&interest);
            let running = eng.summaries.values().filter(|s| s.active()).count();
            let ui_zh = cfg.ui_zh;
            set_status(&shared, |s| {
                s.ui_zh = ui_zh;
                s.running = running;
            });
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
            // The language_server went away or moved to a new port.
            let down = eng.ls.unreachable_for();
            if eng.ls.from_env {
                // The address from the environment never changes in this process. If the language
                // server that started this sidecar is still running, it starts a fresh one with the
                // new address; if it is gone (Windows leaves children running), this one stops.
                if down.is_some_and(|d| d >= LS_GONE_AFTER) {
                    eprintln!(
                        "[agy-statusline] language_server at {addr} unreachable for {}s, exiting",
                        LS_GONE_AFTER.as_secs()
                    );
                    std::process::exit(1);
                }
            } else if down.is_some() || err.as_deref().is_some_and(|e| e.contains("HTTP 401")) {
                // Found via ps/lsof: look it up again next round.
                *engine = None;
            }
        }
        drop(engine);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::StepInfo;

    #[test]
    fn empty_usage_object_not_counted_as_has_usage() {
        let empty = json!({
            "chatModel": {
                "modelDisplayName": "Gemini 4 Neon",
                "usage": {}
            },
            "stepIndices": []
        });
        let c = Call::from_json(&empty, 9_999, false);
        assert!(!c.has_usage);
        assert_eq!(c.at_ms, 0);

        let real = json!({
            "chatModel": {
                "modelDisplayName": "Gemini 4 Neon",
                "usage": { "inputTokens": "100", "cacheReadTokens": "900", "outputTokens": "50" }
            },
            "stepIndices": [1]
        });
        let c2 = Call::from_json(&real, 9_999, true);
        assert!(c2.has_usage);
        assert_eq!(c2.at_ms, 9_999);
    }

    #[test]
    fn date_calls_handles_historical_cross_day_and_live_calls() {
        let day1 = 1_795_079_391_000i64;
        let day1_end = 1_795_079_455_000i64;
        let day2 = 1_795_409_479_000i64;

        let mut steps = HashMap::new();
        steps.insert(1, StepInfo { at_ms: day1, label: Some("step 1".into()) });
        steps.insert(5, StepInfo { at_ms: day1 + 10_000, label: Some("step 5".into()) });
        steps.insert(10, StepInfo { at_ms: day2, label: Some("step 10".into()) });

        // Old conversation with trailing undated call (e.g. e3094148 loaded days later):
        let mut old_calls = vec![
            Call { steps: vec![1], has_usage: true, ..Default::default() },
            Call { steps: vec![], has_usage: true, ..Default::default() },
        ];
        date_calls(&mut old_calls, &steps, day1_end);
        assert_eq!(old_calls[0].at_ms, day1);
        assert!(old_calls[0].dated);
        assert_eq!(old_calls[1].at_ms, day1_end);
        assert!(!old_calls[1].dated);

        // Cross-day conversation with leading & mid-conv undated calls and a live-seen trailing call:
        let mut cross = vec![
            Call { steps: vec![], has_usage: true, ..Default::default() },
            Call { steps: vec![1], has_usage: true, ..Default::default() },
            Call { steps: vec![], has_usage: true, ..Default::default() },
            Call { steps: vec![5], has_usage: true, ..Default::default() },
            Call { steps: vec![10], has_usage: true, ..Default::default() },
            Call { steps: vec![12], has_usage: true, at_ms: day2 + 5_000, live_seen: true, ..Default::default() },
        ];
        date_calls(&mut cross, &steps, day2);
        assert_eq!(cross[0].at_ms, day1); // leading -> next dated (day1), not day2
        assert_eq!(cross[1].at_ms, day1);
        assert_eq!(cross[2].at_ms, day1); // middle -> prev dated (day1), not day2
        assert_eq!(cross[3].at_ms, day1 + 10_000);
        assert_eq!(cross[4].at_ms, day2);
        assert_eq!(cross[5].at_ms, day2 + 5_000); // live-seen kept until transcript flushes

        // Once step 12 flushes to transcript.jsonl, exact created_at replaces seen_at:
        steps.insert(12, StepInfo { at_ms: day2 + 4_500, label: Some("Final reply".into()) });
        date_calls(&mut cross, &steps, day2 + 5_000);
        assert_eq!(cross[5].at_ms, day2 + 4_500);
        assert!(cross[5].dated);
        assert_eq!(cross[5].label.as_deref(), Some("Final reply"));
    }
}
