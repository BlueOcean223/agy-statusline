//! Live data engine for the in-page statusline and side panel.
//!
//! Data comes from the language_server's read-only API, not from scraping the page:
//! - `GetAllCascadeTrajectories`: every conversation's run status, step count, parent/root, subagent spec.
//! - `GetCascadeTrajectoryGeneratorMetadata`: one record per model call with token usage,
//!   time to first token, streaming duration, response model and context window usage.
//!   `generatorMetadataOffset = N` returns records N.. so calls are fetched incrementally.
//! - `GetCascadeTrajectorySteps`: incremental steps for active/recent subagents (waiting state,
//!   elapsed time, current action and recent timeline).
//! - `RetrieveUserQuotaSummary`: 5h / weekly quota buckets per model group.
//! - `GetUserStatus`: masked account identity and subscription tier for per-account quota history.
//!
//! Hooks only tell the engine *when* to look (UDP datagram, see `hook.rs`), so a turn's numbers
//! show up right after the call ends instead of on the next poll.

use crate::cli_usage::{CliScanner, CliSnapshot, apply_cli_delta};
use crate::config::{
    Config, ModelRule, OnlinePrices, effective_model_rules, find_app_data_dir, find_cli_data_dir, find_plugin_data_dir,
    load_config, write_atomic,
};
use crate::convdb::{self, DbSig};
use crate::git::GitInfo;
use crate::ls::LsClient;
use crate::miss::{MissCallInput, MissInfo, miss_scan};
use crate::transcript::{Annotation, Transcript};
use crate::usage::{
    AccountInfo, CallRecord, ModelAgg, QuotaBucketSample, QuotaSamples, UsageHistory, account_from_user_status,
    load_history, load_samples, maybe_replay_samples, needs_backfill, quota_group_id, run_backfill, save_history,
    today_str, update_today,
};
use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUB_DONE_SHOW_MS: i64 = 10_000;
const MAX_SUBS: usize = 4;
const SUB_STEPS_KEEP: usize = 30;

mod tick;

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

fn clip(s: &str, max_chars: usize) -> String {
    let compact: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = compact.chars().count();
    if count <= max_chars {
        compact
    } else {
        format!("{}…", compact.chars().take(max_chars.saturating_sub(1)).collect::<String>())
    }
}

fn file_base(p: &str) -> String {
    let stripped = p.trim_start_matches("file://");
    stripped.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or("").to_string()
}

// ---------------------------------------------------------------- model calls

#[derive(Clone, Debug, Default)]
pub struct Call {
    pub display: String,
    pub resp_model: String,
    pub input: u64,
    pub cache: u64,
    pub cache_write: u64,
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
    /// Optional requestId for cross-source deduplication in day-call breakdowns.
    pub request_id: Option<String>,
}

impl Call {
    pub(crate) fn from_json(g: &Value, seen_at: i64, live_seen: bool) -> Self {
        let cm = &g["chatModel"];
        let usage = &cm["usage"];
        let ctx = &cm["chatStartMetadata"]["contextWindowMetadata"];
        let input = num(&usage["inputTokens"]);
        let cache = num(&usage["cacheReadTokens"]);
        let cache_write = num(&usage["cacheWriteTokens"]);
        let raw_out = num(&usage["outputTokens"]);
        let split_out = num(&usage["thinkingOutputTokens"]).saturating_add(num(&usage["responseOutputTokens"]));
        let output = raw_out.max(split_out);
        Call {
            display: cm["modelDisplayName"].as_str().unwrap_or("").to_string(),
            resp_model: cm["responseModel"].as_str().unwrap_or("").to_string(),
            input,
            cache,
            cache_write,
            output,
            ctx_used: num(&ctx["estimatedTokensUsed"]),
            ctx_max: num(&ctx["maxContextTokens"]),
            checkpoint: cm["chatStartMetadata"]["checkpointIndex"].as_i64().unwrap_or(-1),
            ttft_ms: dur_ms(&cm["timeToFirstToken"]),
            stream_ms: dur_ms(&cm["streamingDuration"]),
            steps: g["stepIndices"].as_array().map(|a| a.iter().map(num).collect()).unwrap_or_default(),
            // Aborted/placeholder records can carry an empty `usage: {}` object with all-zero tokens.
            has_usage: usage.is_object() && (input > 0 || cache > 0 || cache_write > 0 || output > 0),
            at_ms: if live_seen { seen_at } else { 0 },
            dated: false,
            live_seen,
            label: None,
            request_id: usage["requestId"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
        }
    }

    pub(crate) fn price_key(&self) -> String {
        format!("{} {}", self.resp_model, self.display).to_lowercase()
    }

    pub(crate) fn model_bucket_key(&self) -> String {
        if !self.resp_model.is_empty() {
            self.resp_model.clone()
        } else if !self.display.is_empty() {
            self.display.clone()
        } else {
            "unknown".to_string()
        }
    }

    pub fn tps(&self) -> Option<f64> {
        let ms = self.stream_ms?;
        (ms > 50.0 && self.output > 0).then(|| self.output as f64 / (ms / 1000.0))
    }
}

pub fn price_for<'a>(rules: &'a [ModelRule], call: &Call) -> Option<&'a ModelRule> {
    let key = call.price_key();
    rules.iter().find(|r| !r.pattern.is_empty() && contains_lowercased(&key, &r.pattern))
}

/// `hay` is already lowercase. Same as `hay.contains(&pat.to_lowercase())`, without allocating
/// for ASCII patterns (this runs for every call and rule on every tick).
fn contains_lowercased(hay: &str, pat: &str) -> bool {
    if !pat.is_ascii() {
        return hay.contains(&pat.to_lowercase());
    }
    let (h, p) = (hay.as_bytes(), pat.as_bytes());
    if p.is_empty() {
        return true;
    }
    p.len() <= h.len() && h.windows(p.len()).any(|w| w.iter().zip(p).all(|(a, b)| *a == b.to_ascii_lowercase()))
}

/// Changes whenever any pricing rule changes (rules are rebuilt from config on every tick).
fn rules_fingerprint(rules: &[ModelRule]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{rules:?}").hash(&mut h);
    h.finish()
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Cost {
    #[serde(rename = "in")]
    pub input: f64,
    pub cache: f64,
    pub cw: f64,
    pub out: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.cache + self.cw + self.out
    }
}

pub fn call_cost(rules: &[ModelRule], c: &Call) -> Cost {
    let Some(r) = price_for(rules, c) else { return Cost::default() };
    let (p_in, p_cr, p_cw, p_out) = r.rates_at(c.at_ms, now_ms());
    Cost {
        input: c.input as f64 * p_in / 1e6,
        cache: c.cache as f64 * p_cr / 1e6,
        cw: c.cache_write as f64 * p_cw / 1e6,
        out: c.output as f64 * p_out / 1e6,
    }
}

// ---------------------------------------------------------------- conversations & subagent steps

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub status: String,
    pub step_count: u64,
    pub created_ms: i64,
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
    /// Set when `calls` came from the conversation database instead of the language_server.
    db_sig: Option<DbSig>,
    /// Last successful fetch, and last time it brought new or changed records.
    fetched_at: Option<Instant>,
    changed_at: Option<Instant>,
    /// Bumped whenever `calls` change (new records or re-dating); keys `today_cost`.
    rev: u64,
    today_cost: std::cell::Cell<Option<TodayCost>>,
}

/// A conversation's cost and tokens today, valid while `rev`, local midnight and the pricing
/// rules' fingerprint are unchanged.
#[derive(Clone, Copy, Debug)]
struct TodayCost {
    rev: u64,
    midnight: i64,
    rules: u64,
    cost: f64,
    tokens: u64,
}

/// Where `refresh_conv` reads a conversation's generator records from.
#[derive(Clone, Debug)]
pub(crate) enum ConvReq {
    /// From the language_server, re-reading from `offset` (the last known record) of `len`.
    Ls { id: String, offset: usize, len: usize },
    /// From the conversation database; `have` is the signature already read, if any.
    Db { id: String, have: Option<DbSig> },
}

impl ConvReq {
    pub(crate) fn id(&self) -> &str {
        match self {
            ConvReq::Ls { id, .. } | ConvReq::Db { id, .. } => id,
        }
    }
}

pub(crate) enum ConvData {
    Ls(Value),
    /// `None` when the database is unchanged since `have`.
    Db(Option<(DbSig, Vec<Call>, Summary)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    Changed,
    Unchanged,
    /// The conversation changed while the data was being fetched; try again next tick.
    Stale,
}

/// Network/disk half of `refresh_conv`; runs without the ENGINE lock.
pub(crate) fn fetch_conv(ls: &LsClient, app_data: &Path, req: &ConvReq) -> Result<ConvData, String> {
    match req {
        ConvReq::Ls { id, offset, .. } => ls
            .call(
                "GetCascadeTrajectoryGeneratorMetadata",
                &json!({ "cascadeId": id, "generatorMetadataOffset": offset }),
            )
            .map(ConvData::Ls),
        ConvReq::Db { id, have } => {
            let sig = convdb::signature(app_data, id)
                .ok_or_else(|| format!("{id}: not loaded by the language_server and has no database"))?;
            if *have == Some(sig) {
                return Ok(ConvData::Db(None));
            }
            convdb::read(app_data, id, sig).map(|(calls, summary)| ConvData::Db(Some((sig, calls, summary))))
        }
    }
}

#[derive(Clone, Debug, Default)]
struct HookState {
    event: String,
    at_ms: i64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SubStep {
    pub status: String,
    pub text: String,
    pub t0: i64,
    pub t1: i64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SubStepCache {
    pub next_offset: usize,
    pub total: usize,
    pub first_at: i64,
    pub steps: BTreeMap<usize, SubStep>,
    pub fetched_at: Option<Instant>,
    pub final_fetched: bool,
}

fn parse_sub_step(st: &Value) -> Option<SubStep> {
    let md = &st["metadata"];
    let raw_type = st["type"].as_str().unwrap_or("").trim_start_matches("CORTEX_STEP_TYPE_");
    if raw_type == "SYSTEM_MESSAGE" {
        return None;
    }
    let tc = &md["toolCall"];
    let tool_name = tc["name"].as_str().unwrap_or("");
    let kind = match (raw_type, tool_name) {
        ("GENERIC", "run_command") => "RUN_COMMAND",
        ("GENERIC", "read_file" | "view_file") => "VIEW_FILE",
        ("GENERIC", "edit_file" | "write_file") => "CODE_ACTION",
        ("GENERIC", "search_web") => "SEARCH_WEB",
        ("GENERIC", "invoke_subagent") => "INVOKE_SUBAGENT",
        (t, _) => t,
    };
    let status = st["status"].as_str().unwrap_or("").trim_start_matches("CORTEX_STEP_STATUS_").to_string();

    let args_val: Value = st["generic"]["args"]
        .as_object()
        .map(|o| Value::Object(o.clone()))
        .or_else(|| tc["argumentsJson"].as_str().and_then(|s| serde_json::from_str(s).ok()))
        .unwrap_or(Value::Null);
    let payload = if st["generic"]["result"]["payload"].is_object() { &st["generic"]["result"]["payload"] } else { st };

    let text = if let Some(s) = args_val["toolSummary"].as_str().filter(|s| !s.is_empty()) {
        clip(s, 64)
    } else {
        match kind {
            "RUN_COMMAND" => {
                let cmd = payload["runCommand"]["commandLine"]
                    .as_str()
                    .or_else(|| args_val["CommandLine"].as_str())
                    .unwrap_or("");
                if cmd.is_empty() { "run_command".into() } else { format!("run_command: {}", clip(cmd, 52)) }
            }
            "VIEW_FILE" => {
                let p =
                    payload["viewFile"]["absolutePathUri"].as_str().or_else(|| args_val["Path"].as_str()).unwrap_or("");
                let name = if tool_name.is_empty() { "read_file" } else { tool_name };
                let base = file_base(p);
                if base.is_empty() { name.into() } else { format!("{name}: {base}") }
            }
            "CODE_ACTION" => {
                let p = args_val["Path"]
                    .as_str()
                    .or_else(|| args_val["TargetFile"].as_str())
                    .or_else(|| args_val["path"].as_str())
                    .unwrap_or("");
                let name = if tool_name.is_empty() { "edit_file" } else { tool_name };
                let base = file_base(p);
                if base.is_empty() { name.into() } else { format!("{name}: {base}") }
            }
            "SEARCH_WEB" => {
                let q = payload["searchWeb"]["query"].as_str().or_else(|| args_val["query"].as_str()).unwrap_or("");
                if q.is_empty() { "search_web".into() } else { format!("search_web: {}", clip(q, 48)) }
            }
            "INVOKE_SUBAGENT" => {
                let r = args_val["Role"].as_str().or_else(|| args_val["TypeName"].as_str()).unwrap_or("");
                if r.is_empty() { "invoke_subagent".into() } else { format!("subagent: {}", clip(r, 44)) }
            }
            "PLANNER_RESPONSE" => {
                let writing = st["plannerResponse"]["response"].as_str().is_some_and(|s| !s.is_empty());
                if status == "GENERATING" {
                    if writing { "writing response".into() } else { "thinking".into() }
                } else {
                    "planner response".into()
                }
            }
            "USER_INPUT" => "prompt".into(),
            _ => {
                let desc = args_val["Description"]
                    .as_str()
                    .or_else(|| args_val["Task"].as_str())
                    .or_else(|| args_val["task"].as_str())
                    .unwrap_or("");
                let label = if !tool_name.is_empty() { tool_name.to_string() } else { kind.to_lowercase() };
                if desc.is_empty() { label } else { format!("{label}: {}", clip(desc, 48)) }
            }
        }
    };

    let t0 = md["startedAt"].as_str().or_else(|| md["createdAt"].as_str()).and_then(parse_ts).unwrap_or(0);
    let t1 = md["completedAt"].as_str().and_then(parse_ts).unwrap_or(0);
    Some(SubStep { status, text, t0, t1 })
}

// ---------------------------------------------------------------- shared state (engine ⇄ CDP / HTTP threads)

#[derive(Default)]
pub struct Shared {
    /// Conversation ids each connected page currently shows above an input box (page id → ids).
    pub panes: HashMap<String, Vec<String>>,
    /// Latest payload pushed to pages, and its revision.
    pub payload: String,
    pub rev: u64,
    /// Pages currently connected over CDP (page id → url), for the settings panel.
    pub pages: HashMap<String, String>,
    /// Swarm panel iframes currently connected over CDP for optional Chinese UI.
    pub swarm_frames: HashSet<String>,
    /// Bound HTTP port of this sidecar's UI panel (used to auto-reload stuck "plugin unavailable" iframes).
    pub panel_port: u16,
    pub cdp_status: String,
    /// CDP debugging port reachability & timestamp of last state transition (for relaunch guard).
    pub cdp_port_ok: Option<bool>,
    pub cdp_port_since: Option<Instant>,
    pub ls_status: String,
    /// Hook datagrams received since the engine last looked: (event, conversation id).
    pub hook_events: Vec<(String, String)>,
    /// Mirrors `config.ui_zh` for the CDP threads.
    pub ui_zh: bool,
    /// Temporary English override (Ctrl+Alt+E / Ctrl+Option+E or panel button), synced across pages & Swarm frames.
    pub ui_zh_paused: bool,
    pub ui_zh_pause_rev: u64,
    pub ui_zh_pause_origin: Option<String>,
    pub ui_zh_pause_quiet: bool,
    /// Optional online i18n dictionary/rules (merged with bundled), pushed when `i18n_rev` increments.
    pub i18n_dict: Option<String>,
    pub i18n_rules: Option<String>,
    pub i18n_rev: u64,
    /// Optional online price table from `ncdeng/agy-zh-pack`'s `data/prices.json`.
    pub online_prices: Option<OnlinePrices>,
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

// ---------------------------------------------------------------- usage store (shared with HTTP)

pub struct UsageStore {
    pub history: UsageHistory,
    pub samples: QuotaSamples,
    pub samples_writable: bool,
    pub cli_snap: CliSnapshot,
    pub app_data: PathBuf,
    pub cli_data: PathBuf,
    pub plugin_data: PathBuf,
    pub last_saved: Instant,
    /// History changed since the last save (saves are throttled, so this must outlive a tick).
    pub history_pending: bool,
    save_gen: u64,
}

pub static USAGE: Mutex<Option<UsageStore>> = Mutex::new(None);

/// Last snapshot generation written per file, so a slower writer never replaces newer data.
static HISTORY_WRITTEN: Mutex<u64> = Mutex::new(0);
static SAMPLES_WRITTEN: Mutex<u64> = Mutex::new(0);

/// A file's contents serialized under the USAGE lock, written by `write_snapshot` after the lock
/// is released (an atomic write can retry and sleep on Windows).
pub(crate) struct Snapshot {
    path: PathBuf,
    text: String,
    generation: u64,
    written: &'static Mutex<u64>,
}

impl UsageStore {
    pub(crate) fn history_snapshot(&mut self) -> Option<Snapshot> {
        self.last_saved = Instant::now();
        self.history_pending = false;
        if !self.history.writable {
            return None;
        }
        self.save_gen += 1;
        Some(Snapshot {
            path: self.plugin_data.join("usage-history.json"),
            text: serde_json::to_string(&self.history).ok()?,
            generation: self.save_gen,
            written: &HISTORY_WRITTEN,
        })
    }

    fn samples_snapshot(&mut self) -> Option<Snapshot> {
        if !self.samples_writable {
            return None;
        }
        self.save_gen += 1;
        Some(Snapshot {
            path: self.plugin_data.join("quota-samples.json"),
            text: serde_json::to_string(&self.samples).ok()?,
            generation: self.save_gen,
            written: &SAMPLES_WRITTEN,
        })
    }
}

/// Snapshots to write once the caller has released its locks (ENGINE included).
#[must_use]
#[derive(Default)]
pub struct PendingWrites(Vec<Snapshot>);

impl PendingWrites {
    pub fn write(self) {
        for s in self.0 {
            write_snapshot(Some(s));
        }
    }
}

pub(crate) fn write_snapshot(snap: Option<Snapshot>) {
    let Some(s) = snap else { return };
    let Ok(mut written) = s.written.lock() else { return };
    if s.generation <= *written {
        return;
    }
    let _ = write_atomic(&s.path, &s.text);
    *written = s.generation;
}

fn init_usage_store(app_data: PathBuf, plugin_data: PathBuf, shared: SharedRef) {
    let cli_data = find_cli_data_dir(&app_data);
    let mut history = load_history(&plugin_data);
    let (samples, samples_writable) = load_samples(&plugin_data);
    if maybe_replay_samples(&mut history, &samples) {
        save_history(&plugin_data, &history);
    }
    let need_bf = needs_backfill(&history);
    if let Ok(mut guard) = USAGE.lock() {
        *guard = Some(UsageStore {
            history,
            samples,
            samples_writable,
            cli_snap: CliSnapshot::default(),
            app_data: app_data.clone(),
            cli_data: cli_data.clone(),
            plugin_data: plugin_data.clone(),
            last_saved: Instant::now(),
            history_pending: false,
            save_gen: 0,
        });
    }
    if need_bf {
        let app_bf = app_data.clone();
        let pd_bf = plugin_data.clone();
        std::thread::spawn(move || {
            let mut local_h = USAGE
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|u| u.history.clone()))
                .unwrap_or_else(|| load_history(&pd_bf));
            let changed = run_backfill(&app_bf, &mut local_h, now_ms());
            let mut snap = None;
            if changed
                && let Ok(mut guard) = USAGE.lock()
                && let Some(store) = guard.as_mut()
            {
                let today = today_str();
                for (day, rec) in local_h.days {
                    if day < today {
                        let dst = store.history.days.entry(day).or_default();
                        dst.raw = rec.raw;
                        dst.convs = rec.convs;
                        dst.bf = rec.bf;
                    }
                }
                store.history.bf = local_h.bf;
                snap = store.history_snapshot();
            }
            write_snapshot(snap);
        });
    }
    // Background agy CLI scanner: runs all SQLite reads outside USAGE lock, then merges briefly.
    std::thread::spawn(move || {
        let mut scanner = CliScanner::default();
        loop {
            let meta = USAGE.lock().ok().and_then(|g| {
                g.as_ref().map(|s| (s.cli_data.clone(), s.plugin_data.clone(), s.history.cli_bf.clone()))
            });
            if let Some((cd, pd, cli_bf)) = meta
                && let Some(delta) = scanner.scan_delta(&cd, &pd, cli_bf.as_ref(), now_ms())
            {
                let snap = delta.snapshot.clone();
                let mut file = None;
                let changed = if let Ok(mut guard) = USAGE.lock()
                    && let Some(store) = guard.as_mut()
                {
                    store.cli_snap = snap;
                    let ch = apply_cli_delta(&mut store.history, delta);
                    if ch {
                        // Same 15s throttle as live updates; `sync_usage_store` flushes the rest.
                        store.history_pending = true;
                        if store.last_saved.elapsed() >= Duration::from_secs(15) {
                            file = store.history_snapshot();
                        }
                    }
                    ch
                } else {
                    false
                };
                write_snapshot(file);
                if changed {
                    wake(&shared);
                }
            }
            std::thread::sleep(Duration::from_secs(30));
        }
    });
}

// ---------------------------------------------------------------- engine

pub struct Engine {
    pub(crate) ls: LsClient,
    pub(crate) app_data: PathBuf,
    /// Conversations the language_server has loaded, plus those read from their database.
    pub(crate) summaries: HashMap<String, Summary>,
    /// Ids in the latest `GetAllCascadeTrajectories` reply. Only these may be queried by id:
    /// querying any other conversation makes the language_server load it for good.
    ls_loaded: HashSet<String>,
    pub(crate) convs: HashMap<String, Conv>,
    hooks: HashMap<String, HookState>,
    pub(crate) dirty: HashSet<String>,
    /// When a subagent was first seen idle after being active (for the short "done" row).
    pub(crate) sub_done: HashMap<String, i64>,
    pub(crate) sub_steps: HashMap<String, SubStepCache>,
    pub(crate) quota: Value,
    pub(crate) quota_at: Option<Instant>,
    pub(crate) quota_at_ms: i64,
    pub(crate) account: Option<AccountInfo>,
    user_status_at: Option<Instant>,
    pub(crate) summaries_at: Option<Instant>,
    /// Conversation id → transcript mtime, from `brain/` (see `transcript::scan_brain`).
    pub(crate) disk: HashMap<String, i64>,
    disk_at: Option<Instant>,
    /// Candidate subagent ids from transcripts already probed via `refresh_conv`.
    probed_hints: HashSet<String>,
    /// Workspace path → (when read, git status).
    git: HashMap<String, (Instant, Option<GitInfo>)>,
    pub last_error: Option<String>,
    /// A hook fired since the last summaries/quota refresh (refreshed sooner, but rate-limited).
    summaries_poke: bool,
    quota_poke: bool,
    /// Conversation id → (annotation file mtime, title and last view), refreshed every 10s.
    pub(crate) annotations: HashMap<String, (i64, Annotation)>,
    annotations_at: Option<Instant>,
    /// Conversations whose last read failed (no database yet), retried after a backoff.
    failed: HashMap<String, Instant>,
    evict_at: Option<Instant>,
    evict_day: String,
    tick_errors: Vec<String>,
}

impl Engine {
    pub fn new(ls: LsClient) -> Self {
        Self {
            ls,
            app_data: find_app_data_dir(),
            summaries: HashMap::new(),
            ls_loaded: HashSet::new(),
            convs: HashMap::new(),
            hooks: HashMap::new(),
            dirty: HashSet::new(),
            sub_done: HashMap::new(),
            sub_steps: HashMap::new(),
            quota: Value::Null,
            quota_at: None,
            quota_at_ms: 0,
            account: None,
            user_status_at: None,
            summaries_at: None,
            disk: HashMap::new(),
            disk_at: None,
            probed_hints: HashSet::new(),
            git: HashMap::new(),
            last_error: None,
            summaries_poke: false,
            quota_poke: false,
            annotations: HashMap::new(),
            annotations_at: None,
            failed: HashMap::new(),
            evict_at: None,
            evict_day: String::new(),
            tick_errors: Vec::new(),
        }
    }

    pub fn on_hook(&mut self, event: &str, conv: &str) {
        self.hooks.insert(conv.to_string(), HookState { event: event.to_string(), at_ms: now_ms() });
        self.dirty.insert(conv.to_string());
        // Agent teams fire hooks many times a second; `plan_status` rate-limits these refreshes.
        if event == "Stop" {
            self.quota_poke = true;
        }
        self.summaries_poke = true;
    }

    pub(crate) fn any_active(&self) -> bool {
        self.summaries.values().any(Summary::active)
            || self.hooks.values().any(|h| h.event != "Stop" && now_ms() - h.at_ms < 120_000)
    }

    pub(crate) fn apply_summaries(&mut self, j: &Value) {
        let Some(ts) = j["trajectorySummaries"].as_object() else { return };
        let now = now_ms();
        self.ls_loaded = ts.keys().cloned().collect();
        for (id, s) in ts {
            // Read from its database earlier; the language_server has it now, so switch over.
            if self.convs.get(id).is_some_and(|c| c.db_sig.is_some()) {
                self.dirty.insert(id.clone());
            }
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
                    if let Some(sc) = self.sub_steps.get_mut(id) {
                        sc.final_fetched = false;
                    }
                }
            }
            if next.active() {
                self.sub_done.remove(id);
                if let Some(sc) = self.sub_steps.get_mut(id) {
                    sc.final_fetched = false;
                }
            }
            self.summaries.insert(id.clone(), next);
        }
    }

    /// What to read for `id`: the language_server if it already has the conversation loaded,
    /// otherwise the conversation database (see `convdb`).
    pub(crate) fn conv_req(&self, id: &str) -> ConvReq {
        let conv = self.convs.get(id);
        if self.ls_loaded.contains(id) {
            let len = conv.map_or(0, |c| c.calls.len());
            // Re-read the last record: it may have been written before its usage was filled in.
            ConvReq::Ls { id: id.to_string(), offset: len.saturating_sub(1), len }
        } else {
            ConvReq::Db { id: id.to_string(), have: conv.filter(|c| c.loaded_once).and_then(|c| c.db_sig) }
        }
    }

    /// Loads new generator records for `id` synchronously (panel requests and the CLI).
    pub(crate) fn refresh_conv(&mut self, id: &str) -> Result<(), String> {
        let req = self.conv_req(id);
        let data = fetch_conv(&self.ls, &self.app_data, &req)?;
        if self.apply_conv(&req, data) == Applied::Changed {
            self.sync_transcript(id, true);
        }
        Ok(())
    }

    /// Merges records fetched for `req`. The caller re-dates changed conversations.
    pub(crate) fn apply_conv(&mut self, req: &ConvReq, data: ConvData) -> Applied {
        let now = Instant::now();
        match (req, data) {
            (ConvReq::Ls { id, offset, len }, ConvData::Ls(j)) => {
                let seen_at = now_ms();
                let step_count = self.summaries.get(id.as_str()).map(|s| s.step_count);
                let conv = self.convs.entry(id.clone()).or_default();
                if conv.calls.len() != *len {
                    return Applied::Stale;
                }
                let offset = *offset;
                // Database rows match the language_server's records index for index, so this
                // continues incrementally from what was read there.
                conv.db_sig = None;
                conv.fetched_at = Some(now);
                let was_loaded = conv.loaded_once;
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
                                Call::from_json(g, seen_at, live_seen)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if fresh.is_empty() && offset < conv.calls.len() {
                    return Applied::Unchanged;
                }
                conv.calls.truncate(offset);
                conv.calls.extend(fresh);
                if let (Some((t, dated, live_seen)), Some(c)) = (prev_last, conv.calls.get_mut(offset)) {
                    c.at_ms = t;
                    c.dated = dated;
                    c.live_seen = live_seen;
                }
                conv.loaded_once = true;
                conv.changed_at = Some(now);
                conv.rev += 1;
                if let Some(n) = step_count {
                    conv.loaded_steps = n;
                }
                Applied::Changed
            }
            (ConvReq::Db { id, .. }, ConvData::Db(None)) => {
                if let Some(c) = self.convs.get_mut(id.as_str()) {
                    c.fetched_at = Some(now);
                }
                Applied::Unchanged
            }
            (ConvReq::Db { id, .. }, ConvData::Db(Some((sig, mut calls, summary)))) => {
                if self.ls_loaded.contains(id.as_str()) {
                    // The language_server loaded it meanwhile; read it from there instead.
                    return Applied::Stale;
                }
                let conv = self.convs.entry(id.clone()).or_default();
                conv.fetched_at = Some(now);
                if conv.loaded_once && conv.db_sig == Some(sig) {
                    return Applied::Unchanged;
                }
                for (new, old) in calls.iter_mut().zip(&conv.calls) {
                    new.at_ms = old.at_ms;
                    new.dated = old.dated;
                    new.live_seen = old.live_seen;
                }
                conv.calls = calls;
                conv.loaded_once = true;
                conv.loaded_steps = summary.step_count;
                conv.db_sig = Some(sig);
                conv.changed_at = Some(now);
                conv.rev += 1;
                self.summaries.insert(id.clone(), summary);
                Applied::Changed
            }
            _ => Applied::Stale,
        }
    }

    /// Merges a `GetCascadeTrajectorySteps` page (from step `from`) for a visible subagent, so
    /// the bar can show `waiting`, live elapsed duration, total steps, and a recent-steps timeline.
    pub(crate) fn apply_sub_steps(&mut self, id: &str, from: usize, running: bool, j: &Value) {
        let cache = self.sub_steps.entry(id.to_string()).or_default();
        if !running {
            cache.final_fetched = true;
        }
        let Some(arr) = j["steps"].as_array() else { return };
        if arr.is_empty() {
            return;
        }
        for (i, st) in arr.iter().enumerate() {
            let idx = from + i;
            if let Some(parsed) = parse_sub_step(st) {
                if parsed.t0 > 0 && (cache.first_at == 0 || parsed.t0 < cache.first_at) {
                    cache.first_at = parsed.t0;
                }
                cache.steps.insert(idx, parsed);
            }
        }
        cache.total = cache.total.max(from + arr.len());
        cache.next_offset = from.max((from + arr.len()).saturating_sub(2));
        while cache.steps.len() > SUB_STEPS_KEEP {
            if let Some(&first_k) = cache.steps.keys().next() {
                cache.steps.remove(&first_k);
            } else {
                break;
            }
        }
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
            conv.rev += 1;
        }
    }

    /// Probes subagent ids referenced in the transcripts of `seeds` that the language_server has
    /// not loaded yet (e.g. historical subagents of a conversation opened after restart).
    pub(crate) fn load_subagent_hints(&mut self, seeds: &[String]) {
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
                    if self.refresh_conv(&sid).is_ok() && seen.insert(sid.clone()) {
                        next_queue.push(sid);
                    }
                }
            }
            if next_queue.is_empty() {
                break;
            }
            queue = next_queue;
        }
    }

    pub(crate) fn apply_quota(&mut self, j: Value) {
        self.quota = j.get("response").cloned().unwrap_or(j);
        self.quota_at_ms = now_ms();
        self.quota_at = Some(Instant::now());
        self.quota_poke = false;
    }

    pub(crate) fn apply_user_status(&mut self, j: Option<&Value>) {
        if let Some(acct) = j.and_then(account_from_user_status) {
            self.account = Some(acct);
        }
        self.user_status_at = Some(Instant::now());
    }

    /// Merges today's live usage into the usage store. Returns the files to write; call
    /// `PendingWrites::write` after releasing the ENGINE lock.
    pub fn sync_usage_store(&self) -> PendingWrites {
        let midnight = local_midnight_ms();
        let today = today_str();
        let mut live_by_conv: HashMap<String, HashMap<String, ModelAgg>> = HashMap::new();
        for (cid, conv) in &self.convs {
            for c in conv.calls.iter().filter(|c| c.has_usage && c.at_ms >= midnight) {
                live_by_conv.entry(cid.clone()).or_default().entry(c.model_bucket_key()).or_default().add(
                    c.input,
                    c.cache,
                    c.cache_write,
                    c.output,
                );
            }
        }
        let buckets = self.quota_bucket_samples();

        let (mut hist_file, mut samples_file) = (None, None);
        if let Ok(mut guard) = USAGE.lock()
            && let Some(store) = guard.as_mut()
        {
            let (hd, sd) = update_today(
                &mut store.history,
                &mut store.samples,
                &today,
                &live_by_conv,
                &buckets,
                self.account.as_ref(),
                self.quota_at_ms,
            );
            if sd {
                samples_file = store.samples_snapshot();
            }
            store.history_pending |= hd;
            if store.history_pending && store.last_saved.elapsed() >= Duration::from_secs(15) {
                hist_file = store.history_snapshot();
            }
        }
        PendingWrites(samples_file.into_iter().chain(hist_file).collect())
    }

    fn quota_bucket_samples(&self) -> Vec<QuotaBucketSample> {
        let mut out = Vec::new();
        for g in self.quota["groups"].as_array().into_iter().flatten() {
            let name = g["displayName"].as_str().unwrap_or("");
            for b in g["buckets"].as_array().into_iter().flatten() {
                let window = b["window"].as_str().unwrap_or("");
                let Some(rem) = b["remainingFraction"].as_f64() else { continue };
                let reset_ms = b["resetTime"].as_str().and_then(parse_ts).unwrap_or(0);
                if window.is_empty() || !(0.0..=1.0).contains(&rem) {
                    continue;
                }
                out.push(QuotaBucketSample {
                    id: quota_group_id(name, window),
                    name: name.to_string(),
                    window: window.to_string(),
                    used: (1.0 - rem).clamp(0.0, 1.0),
                    reset_ms,
                });
            }
        }
        out
    }

    /// Exports live in-memory calls for the `/api/usage/day` call-breakdown view.
    pub fn live_call_records(&self) -> Vec<CallRecord> {
        let mut out = Vec::new();
        for (cid, conv) in &self.convs {
            for c in conv.calls.iter().filter(|c| c.has_usage && c.at_ms > 0) {
                let dedup_key = c
                    .request_id
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}:{}:{}", cid, c.at_ms / 1000, c.input, c.output));
                out.push(CallRecord {
                    conv_id: cid.clone(),
                    at_ms: c.at_ms,
                    step: c.steps.first().copied(),
                    model_raw: c.resp_model.clone(),
                    model_display: c.display.clone(),
                    input: c.input,
                    cache_read: c.cache,
                    cache_write: c.cache_write,
                    output: c.output,
                    ttft_ms: c.ttft_ms,
                    stream_ms: c.stream_ms,
                    dedup_key,
                    source: "desktop",
                });
            }
        }
        out
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
        // Hooks fire before the next status poll; trust a fresh PreInvocation/PostInvocation.
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

    /// Computes today's total cost and tokens (desktop live + persisted snapshot + `agy CLI`),
    /// returning `(total_usd, total_tokens, cli_usd)`.
    pub(crate) fn today_totals(&self, rules: &[ModelRule]) -> (f64, u64, f64) {
        let midnight = local_midnight_ms();
        let fp = rules_fingerprint(rules);
        let (mut live_cost, mut live_tok) = (0.0, 0u64);
        for conv in self.convs.values() {
            let (cost, tok) = match conv.today_cost.get() {
                Some(t) if t.rev == conv.rev && t.midnight == midnight && t.rules == fp => (t.cost, t.tokens),
                _ => {
                    let (mut cost, mut tok) = (0.0, 0u64);
                    for c in conv.calls.iter().filter(|c| c.has_usage && c.at_ms >= midnight) {
                        cost += call_cost(rules, c).total();
                        tok += c.input + c.cache + c.cache_write + c.output;
                    }
                    conv.today_cost.set(Some(TodayCost { rev: conv.rev, midnight, rules: fp, cost, tokens: tok }));
                    (cost, tok)
                }
            };
            live_cost += cost;
            live_tok += tok;
        }
        // Snapshot today's persisted aggregates + CLI snapshot briefly under USAGE lock, then price lock-free.
        let today = today_str();
        let now = now_ms();
        let snap = USAGE.lock().ok().and_then(|g| {
            let store = g.as_ref()?;
            let day = store.history.days.get(&today)?;
            Some((
                day.raw.clone(),
                day.cli_raw.clone(),
                store.cli_snap.clone(),
                store.cli_data.clone(),
                store.plugin_data.clone(),
            ))
        });
        let persisted = snap.map(|(desk_raw, cli_raw, mut cli_snap, cd, pd)| {
            let (mut desk_usd, mut desk_tok) = (0.0, 0u64);
            let (mut cli_usd, mut cli_tok) = (0.0, 0u64);
            for (model, agg) in &desk_raw {
                let (_, u) = crate::usage::price_tokens(
                    rules,
                    model,
                    agg.input,
                    agg.cache_read,
                    agg.cache_write,
                    agg.output,
                    now,
                    now,
                );
                desk_usd += u;
                desk_tok += agg.total_tokens();
            }
            if !cli_raw.is_empty() {
                // On price-transition days, price CLI calls at their exact timestamps outside USAGE lock.
                if crate::config::any_price_cut_in(rules, midnight, midnight + 86_400_000) {
                    for c in cli_snap.calls_for_day(&cd, &pd, &today) {
                        let key = format!("{} {}", c.model_raw, c.model_display);
                        let (_, u) = crate::usage::price_tokens(
                            rules,
                            &key,
                            c.input,
                            c.cache_read,
                            c.cache_write,
                            c.output,
                            c.at_ms,
                            now,
                        );
                        cli_usd += u;
                        cli_tok += c.input + c.cache_read + c.cache_write + c.output;
                    }
                } else {
                    for (model, agg) in &cli_raw {
                        let (_, u) = crate::usage::price_tokens(
                            rules,
                            model,
                            agg.input,
                            agg.cache_read,
                            agg.cache_write,
                            agg.output,
                            now,
                            now,
                        );
                        cli_usd += u;
                        cli_tok += agg.total_tokens();
                    }
                }
            }
            (desk_usd, desk_tok, cli_usd, cli_tok)
        });
        match persisted {
            Some((p_desk_cost, p_desk_tok, cli_cost, cli_tok)) => {
                (live_cost.max(p_desk_cost) + cli_cost, live_tok.max(p_desk_tok) + cli_tok, cli_cost)
            }
            None => (live_cost, live_tok, 0.0),
        }
    }

    /// Runs 6-cause cache-miss analysis over a conversation's usage calls.
    pub(crate) fn conv_misses(&self, id: &str, rules: &[ModelRule]) -> Vec<Option<MissInfo>> {
        let Some(conv) = self.convs.get(id) else {
            return Vec::new();
        };
        let now = now_ms();
        let inputs: Vec<MissCallInput> = conv
            .calls
            .iter()
            .filter(|c| c.has_usage)
            .map(|c| {
                if let Some(r) = price_for(rules, c) {
                    let (p_in, p_cr, p_cw, _) = r.rates_at(c.at_ms, now);
                    MissCallInput {
                        at_ms: c.at_ms,
                        ttft_ms: c.ttft_ms,
                        stream_ms: c.stream_ms,
                        step: c.steps.first().copied(),
                        model_name: Some(r.short_name.clone()),
                        input: c.input,
                        cache_read: c.cache,
                        cache_write: c.cache_write,
                        p_in,
                        p_cache: p_cr,
                        p_cw,
                    }
                } else {
                    MissCallInput {
                        at_ms: c.at_ms,
                        ttft_ms: c.ttft_ms,
                        stream_ms: c.stream_ms,
                        step: c.steps.first().copied(),
                        model_name: None,
                        input: c.input,
                        cache_read: c.cache,
                        cache_write: c.cache_write,
                        p_in: 0.0,
                        p_cache: 0.0,
                        p_cw: 0.0,
                    }
                }
            })
            .collect();
        let markers = conv.transcript.as_ref().map(|t| &t.markers);
        miss_scan(&inputs, markers)
    }

    pub fn build_payload(&self, panes: &HashSet<String>, cfg: &Config, online: Option<&OnlinePrices>) -> Value {
        let rules = effective_model_rules(&cfg.models, online);
        let (today, _, today_cli) = self.today_totals(&rules);
        let price_src = match online {
            Some(p) if !p.updated.is_empty() => format!("online ({})", p.updated),
            Some(_) => "online".to_string(),
            None => "bundled".to_string(),
        };

        let mut views = serde_json::Map::new();
        for id in panes {
            views.insert(id.clone(), serde_json::to_value(self.view(id, cfg, &rules)).unwrap_or(Value::Null));
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
            "todayCli": today_cli,
            "costlyThreshold": cfg.costly_call_usd,
            "priceSource": price_src,
            "quotaGroups": groups,
            "views": views,
            "opts": {
                "enabled": cfg.bar_enabled,
                "seg": cfg.bar_segments,
                "look": cfg.bar_look,
            },
            "error": self.last_error,
        })
    }

    pub(crate) fn agent_line(&self, id: &str, cfg: &Config, rules: &[ModelRule]) -> AgentLine {
        let calls = self.convs.get(id).map(|c| c.calls.as_slice()).unwrap_or(&[]);
        let last = self.last_call(id);
        let ctx_max = if cfg.context_limit_override > 0 {
            cfg.context_limit_override
        } else {
            last.map(|c| c.ctx_max).unwrap_or(0)
        };
        let mut by_ck = 0;
        let mut by_drop = 0;
        let mut prev_ck = -1;
        let mut prev_used = 0u64;
        let mut own_cost = 0.0;
        let mut first_call_at = 0i64;
        for c in calls.iter().filter(|c| c.has_usage) {
            own_cost += call_cost(rules, c).total();
            if c.at_ms > 0 && (first_call_at == 0 || c.at_ms < first_call_at) {
                first_call_at = c.at_ms;
            }
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
        let sc = self.sub_steps.get(id);

        let started_ms = [s.map(|x| x.created_ms).unwrap_or(0), sc.map(|x| x.first_at).unwrap_or(0), first_call_at]
            .into_iter()
            .filter(|&t| t > 0)
            .min();

        let (waiting, current_step, timeline, ended_ms, step_total) = if let Some(sc) = sc {
            let active_step = sc
                .steps
                .iter()
                .rev()
                .find(|(_, st)| st.status.contains("WAIT") || st.status == "RUNNING" || st.status == "GENERATING")
                .or_else(|| sc.steps.iter().next_back());
            let waiting = active_step.is_some_and(|(_, st)| st.status.contains("WAIT"));
            let cur = active_step.map(|(k, st)| format!("{} (step {})", st.text, k + 1));
            let tl: Vec<String> = sc
                .steps
                .iter()
                .rev()
                .take(5)
                .rev()
                .map(|(k, st)| {
                    let sym = if st.status.contains("WAIT") {
                        "?"
                    } else if st.status == "ERROR" {
                        "✗"
                    } else if st.status == "DONE" {
                        "✓"
                    } else {
                        "●"
                    };
                    format!("{sym} #{} {}", k + 1, st.text)
                })
                .collect();
            let end = sc.steps.values().map(|st| st.t1.max(st.t0)).max().filter(|&t| t > 0);
            (waiting, cur, tl, end, sc.total)
        } else {
            (false, None, Vec::new(), None, s.map(|x| x.step_count as usize).unwrap_or(0))
        };

        let ended_ms = ended_ms.or_else(|| last.map(|c| c.at_ms).filter(|&t| t > 0));

        AgentLine {
            id: id.to_string(),
            sub_type: s.and_then(|s| s.sub_type.clone()),
            sub_role: s.and_then(|s| s.sub_role.clone()),
            running: self.is_running(id),
            waiting,
            model: last.map(|c| c.display.clone()),
            resp_model: last.map(|c| c.resp_model.clone()),
            ctx_used: last.map(|c| c.ctx_used),
            ctx_max,
            compactions,
            cache_read: last.map(|c| c.cache),
            cache_write: last.map(|c| c.cache_write),
            cache_hit: last.and_then(|c| {
                let denom = c.cache + c.input;
                (denom > 0).then(|| c.cache as f64 / denom as f64)
            }),
            ttft_ms: last.and_then(|c| c.ttft_ms),
            tps: last.and_then(Call::tps),
            out_tokens: last.map(|c| c.output),
            cost: own_cost,
            started_ms,
            ended_ms,
            step_total,
            current_step,
            timeline,
            last_at: last.map(|c| c.at_ms),
            calls: calls.iter().filter(|c| c.has_usage).count(),
        }
    }

    pub(crate) fn view(&self, id: &str, cfg: &Config, rules: &[ModelRule]) -> View {
        let root = self.root_of(id);
        let is_sub = root != id;
        let main = self.agent_line(id, cfg, rules);
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
        let last_usd = cost_last.map(|k| k.total());
        let last_costly = last_usd.is_some_and(|u| cfg.costly_call_usd > 0.0 && u >= cfg.costly_call_usd);
        let last_call_key = last.map(|c| format!("{}:{}:{}", id, main.calls, c.at_ms));
        let last_miss = self.conv_misses(id, rules).into_iter().last().flatten();
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
            subs = ids.into_iter().take(MAX_SUBS).map(|sid| self.agent_line(sid, cfg, rules)).collect();
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
            last_call_costly: last_costly,
            last_call_key,
            last_call_miss: last_miss,
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
    pub waiting: bool,
    pub model: Option<String>,
    pub resp_model: Option<String>,
    pub ctx_used: Option<u64>,
    pub ctx_max: u64,
    pub compactions: usize,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub cache_hit: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub tps: Option<f64>,
    pub out_tokens: Option<u64>,
    pub cost: f64,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub step_total: usize,
    pub current_step: Option<String>,
    pub timeline: Vec<String>,
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
    pub last_call_costly: bool,
    pub last_call_key: Option<String>,
    pub last_call_miss: Option<MissInfo>,
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
    init_usage_store(app_data.clone(), plugin_data.clone(), shared.clone());
    spawn_hook_listener(&plugin_data, shared.clone());

    let mut last_payload = String::new();
    let mut last_usage_sync = Instant::now() - Duration::from_secs(60);
    loop {
        let tick_started = Instant::now();
        let (panes, interest, events, online_prices) = {
            let (m, _) = &*shared;
            m.lock()
                .map(|mut s| {
                    let panes: HashSet<String> = s.panes.values().flatten().cloned().collect();
                    s.panel.retain(|_, at| at.elapsed() < Duration::from_secs(15));
                    let mut interest = panes.clone();
                    interest.extend(s.panel.keys().cloned());
                    (panes, interest, std::mem::take(&mut s.hook_events), s.online_prices.clone())
                })
                .unwrap_or_default()
        };

        // The ENGINE lock is taken only to plan and to merge; RPCs, SQLite, directory scans and
        // git run unlocked so side-panel requests are never stuck behind a slow call.
        let status_plan = {
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
            engine.as_mut().map(|eng| {
                for (e, c) in &events {
                    eng.on_hook(e, c);
                }
                eng.plan_status()
            })
        };
        if let Some(plan) = status_plan {
            let fetched = plan.fetch();
            let conv_plan = ENGINE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|eng| {
                eng.apply_status(fetched);
                eng.plan_convs(&interest)
            });
            let fetched = conv_plan.map(tick::ConvPlan::fetch);
            let cfg = load_config(&plugin_data);

            let mut writes = PendingWrites::default();
            let mut engine = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            if let (Some(eng), Some(fetched)) = (engine.as_mut(), fetched) {
                eng.apply_convs(fetched);
                if last_usage_sync.elapsed() >= Duration::from_secs(5) {
                    writes = eng.sync_usage_store();
                    last_usage_sync = Instant::now();
                }
                let running = eng.summaries.values().filter(|s| s.active()).count();
                let ui_zh = cfg.ui_zh;
                set_status(&shared, |s| {
                    s.ui_zh = ui_zh;
                    s.running = running;
                });
                let active_online = cfg.online_data_sync.then_some(online_prices).flatten();
                let payload = eng.build_payload(&panes, &cfg, active_online.as_ref()).to_string();
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
                    if down.is_some_and(|d| d >= LS_GONE_AFTER) {
                        eprintln!(
                            "[agy-statusline] language_server at {addr} unreachable for {}s, exiting",
                            LS_GONE_AFTER.as_secs()
                        );
                        drop(engine);
                        writes.write();
                        std::process::exit(1);
                    }
                } else if down.is_some() || err.as_deref().is_some_and(|e| e.contains("HTTP 401")) {
                    *engine = None;
                }
            }
            drop(engine);
            writes.write();
        }

        // Sleep up to 1s; hook datagrams and page pane changes wake the loop early, but not more
        // often than every 250ms (agent teams can send many hooks a second).
        let (m, cv) = &*shared;
        if let Ok(guard) = m.lock() {
            let (mut g, _) = cv
                .wait_timeout_while(guard, Duration::from_millis(1000), |s| !s.wake)
                .unwrap_or_else(|e| e.into_inner());
            g.wake = false;
        }
        let min_gap = Duration::from_millis(250);
        if let Some(rest) = min_gap.checked_sub(tick_started.elapsed()) {
            std::thread::sleep(rest);
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
    #[test]
    fn price_pattern_matching_is_case_insensitive_without_allocating() {
        use super::contains_lowercased;
        assert!(contains_lowercased("gemini-4-neon gemini 4 neon (xhigh)", "Gemini 4 Neon"));
        assert!(contains_lowercased("claude-opus", "OPUS"));
        assert!(!contains_lowercased("gemini", "gemini-4"));
        assert!(contains_lowercased("flash", ""));
        assert!(contains_lowercased("模型 ä", "Ä"));
    }

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
                "modelDisplayName": "Claude Opus 5.5",
                "responseModel": "claude-opus-5-5",
                "usage": {
                    "inputTokens": "100",
                    "cacheReadTokens": "900",
                    "cacheWriteTokens": "500",
                    "thinkingOutputTokens": "80",
                    "responseOutputTokens": "40"
                }
            },
            "stepIndices": [1]
        });
        let c2 = Call::from_json(&real, 9_999, true);
        assert!(c2.has_usage);
        assert_eq!(c2.cache_write, 500);
        assert_eq!(c2.output, 120);
        let rules = crate::config::default_model_rules();
        let cost = call_cost(&rules, &c2);
        // 500 tokens * $5.0/M = $0.0025
        assert!((cost.cw - 0.0025).abs() < 1e-9);
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

        let mut old_calls = vec![
            Call { steps: vec![1], has_usage: true, ..Default::default() },
            Call { steps: vec![], has_usage: true, ..Default::default() },
        ];
        date_calls(&mut old_calls, &steps, day1_end);
        assert_eq!(old_calls[0].at_ms, day1);
        assert!(old_calls[0].dated);
        assert_eq!(old_calls[1].at_ms, day1_end);
        assert!(!old_calls[1].dated);

        let mut cross = vec![
            Call { steps: vec![], has_usage: true, ..Default::default() },
            Call { steps: vec![1], has_usage: true, ..Default::default() },
            Call { steps: vec![], has_usage: true, ..Default::default() },
            Call { steps: vec![5], has_usage: true, ..Default::default() },
            Call { steps: vec![10], has_usage: true, ..Default::default() },
            Call { steps: vec![12], has_usage: true, at_ms: day2 + 5_000, live_seen: true, ..Default::default() },
        ];
        date_calls(&mut cross, &steps, day2);
        assert_eq!(cross[0].at_ms, day1);
        assert_eq!(cross[1].at_ms, day1);
        assert_eq!(cross[2].at_ms, day1);
        assert_eq!(cross[3].at_ms, day1 + 10_000);
        assert_eq!(cross[4].at_ms, day2);
        assert_eq!(cross[5].at_ms, day2 + 5_000);

        steps.insert(12, StepInfo { at_ms: day2 + 4_500, label: Some("Final reply".into()) });
        date_calls(&mut cross, &steps, day2 + 5_000);
        assert_eq!(cross[5].at_ms, day2 + 4_500);
        assert!(cross[5].dated);
        assert_eq!(cross[5].label.as_deref(), Some("Final reply"));
    }

    #[test]
    fn parses_subagent_waiting_step() {
        let st = json!({
            "type": "CORTEX_STEP_TYPE_RUN_COMMAND",
            "status": "CORTEX_STEP_STATUS_WAITING",
            "runCommand": { "commandLine": "cargo test" },
            "metadata": { "createdAt": "2026-11-23T06:00:00Z" }
        });
        let p = parse_sub_step(&st).unwrap();
        assert_eq!(p.status, "WAITING");
        assert!(p.text.contains("cargo test"));
    }
}
