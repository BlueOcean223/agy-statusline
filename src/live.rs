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

use crate::call::{Call, ModelAgg, ModelId, Tokens};
use crate::cli_usage::{CliScanner, CliSnapshot, apply_cli_delta};
use crate::config::{
    Config, OnlinePrices, find_app_data_dir, find_cli_data_dir, find_plugin_data_dir, load_config, write_atomic,
};
use crate::convdb::{self, ConvMeta, DbSig};
use crate::git::GitInfo;
use crate::ls::LsClient;
use crate::miss::{MissInfo, miss_scan};
use crate::pricing::{Cost, DayCost, Pricer};
use crate::transcript::{Annotation, ReadPos, Transcript, TranscriptChunk};
use crate::usage::{
    AccountInfo, QuotaBucketSample, QuotaSamples, UsageHistory, account_from_user_status, load_history, load_samples,
    maybe_replay_samples, needs_backfill, quota_group_id, run_backfill, save_history, today_str, update_today,
};
use chrono::{DateTime, Local, TimeZone};
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

mod panel;
#[cfg(test)]
mod test_support;
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

/// A call in a conversation the engine tracks, with what the engine learned about it.
#[derive(Clone, Debug, Default)]
pub struct LiveCall {
    pub call: Call,
    /// True once `at_ms` has been resolved from `transcript.jsonl`'s `created_at`.
    pub dated: bool,
    /// True when this call first appeared incrementally after the conversation was already loaded.
    pub live_seen: bool,
    /// From transcript.jsonl: what the model did with this response.
    pub label: Option<String>,
}

impl std::ops::Deref for LiveCall {
    type Target = Call;
    fn deref(&self) -> &Call {
        &self.call
    }
}

impl std::ops::DerefMut for LiveCall {
    fn deref_mut(&mut self) -> &mut Call {
        &mut self.call
    }
}

impl From<Call> for LiveCall {
    fn from(call: Call) -> Self {
        Self { call, ..Default::default() }
    }
}

impl LiveCall {
    fn from_json(g: &Value, conv_id: &str, seen_at: i64, live_seen: bool) -> Self {
        let cm = &g["chatModel"];
        let usage = &cm["usage"];
        let ctx = &cm["chatStartMetadata"]["contextWindowMetadata"];
        let raw_out = num(&usage["outputTokens"]);
        let split_out = num(&usage["thinkingOutputTokens"]).saturating_add(num(&usage["responseOutputTokens"]));
        let call = Call {
            conv_id: conv_id.to_string(),
            source: crate::call::DESKTOP,
            model: ModelId::new(
                cm["responseModel"].as_str().unwrap_or(""),
                cm["modelDisplayName"].as_str().unwrap_or(""),
            ),
            tokens: Tokens {
                input: num(&usage["inputTokens"]),
                cache_read: num(&usage["cacheReadTokens"]),
                cache_write: num(&usage["cacheWriteTokens"]),
                output: raw_out.max(split_out),
            },
            ctx_used: num(&ctx["estimatedTokensUsed"]),
            ctx_max: num(&ctx["maxContextTokens"]),
            checkpoint: cm["chatStartMetadata"]["checkpointIndex"].as_i64().unwrap_or(-1),
            ttft_ms: dur_ms(&cm["timeToFirstToken"]),
            stream_ms: dur_ms(&cm["streamingDuration"]),
            steps: g["stepIndices"].as_array().map(|a| a.iter().map(num).collect()).unwrap_or_default(),
            at_ms: if live_seen { seen_at } else { 0 },
            response_id: usage["responseId"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
        };
        Self { call, live_seen, ..Default::default() }
    }
}

// ---------------------------------------------------------------- conversations & subagent steps

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub status: String,
    pub step_count: u64,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub parent: Option<String>,
    pub root: Option<String>,
    pub sub_type: Option<String>,
    pub sub_role: Option<String>,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub workspace_path: Option<String>,
}

impl From<ConvMeta> for Summary {
    fn from(m: ConvMeta) -> Self {
        Self {
            step_count: m.step_count,
            created_ms: m.created_ms,
            modified_ms: m.modified_ms,
            root: m.root,
            sub_type: m.sub_type,
            sub_role: m.sub_role,
            workspace: m.workspace,
            branch: m.branch,
            workspace_path: m.workspace_path,
            ..Default::default()
        }
    }
}

impl Summary {
    pub fn active(&self) -> bool {
        ["RUNNING", "BUSY", "CANCELING"].iter().any(|s| self.status.contains(s))
    }
}

#[derive(Default)]
pub struct Conv {
    pub calls: Vec<LiveCall>,
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

/// Where a conversation's generator records are read from (see `fetch_conv`).
#[derive(Clone, Debug)]
pub(crate) enum ConvReq {
    /// From the language_server, re-reading from `offset` (the last known record) of `len`.
    /// `steps` is the summary's step count when planned: what the records will be current to.
    Ls { id: String, offset: usize, len: usize, steps: Option<u64> },
    /// From the conversation database; `have` is the signature already read, if any. A result
    /// planned against another signature than the one loaded when it is merged is stale.
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
    Db(Option<(DbSig, Vec<LiveCall>, Box<Summary>)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    Changed,
    Unchanged,
    /// The conversation changed while the data was being fetched; try again next tick.
    Stale,
}

/// Reads what `req` asks for; runs without the ENGINE lock.
fn fetch_conv(ls: &LsClient, app_data: &Path, req: &ConvReq) -> Result<ConvData, String> {
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
            convdb::read(app_data, id, sig).map(|(calls, meta)| {
                // The transcript dates these calls, as it does for the language_server's records.
                let calls = calls.into_iter().map(|c| LiveCall::from(Call { at_ms: 0, ..c })).collect();
                ConvData::Db(Some((sig, calls, Box::new(Summary::from(meta)))))
            })
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
    fn history_snapshot(&mut self) -> Option<Snapshot> {
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

fn write_snapshot(snap: Option<Snapshot>) {
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
    ls: LsClient,
    app_data: PathBuf,
    /// Conversations the language_server has loaded, plus those read from their database.
    summaries: HashMap<String, Summary>,
    /// Ids in the latest `GetAllCascadeTrajectories` reply. Only these may be queried by id:
    /// querying any other conversation makes the language_server load it for good.
    ls_loaded: HashSet<String>,
    convs: HashMap<String, Conv>,
    hooks: HashMap<String, HookState>,
    dirty: HashSet<String>,
    /// When a subagent was first seen idle after being active (for the short "done" row).
    sub_done: HashMap<String, i64>,
    sub_steps: HashMap<String, SubStepCache>,
    quota: Value,
    quota_at: Option<Instant>,
    quota_at_ms: i64,
    account: Option<AccountInfo>,
    user_status_at: Option<Instant>,
    summaries_at: Option<Instant>,
    /// Conversation id → transcript mtime, from `brain/` (see `transcript::scan_brain`).
    disk: HashMap<String, i64>,
    disk_at: Option<Instant>,
    /// Candidate subagent ids from transcripts already probed (see `plan_hints`).
    probed_hints: HashSet<String>,
    /// Workspace path → (when read, git status).
    git: HashMap<String, (Instant, Option<GitInfo>)>,
    last_error: Option<String>,
    /// A hook fired since the last summaries/quota refresh (refreshed sooner, but rate-limited).
    summaries_poke: bool,
    quota_poke: bool,
    /// Conversation id → (annotation file mtime, title and last view), refreshed every 10s.
    annotations: HashMap<String, (i64, Annotation)>,
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

    fn on_hook(&mut self, event: &str, conv: &str) {
        self.hooks.insert(conv.to_string(), HookState { event: event.to_string(), at_ms: now_ms() });
        self.dirty.insert(conv.to_string());
        // Agent teams fire hooks many times a second; `plan_status` rate-limits these refreshes.
        if event == "Stop" {
            self.quota_poke = true;
        }
        self.summaries_poke = true;
    }

    fn any_active(&self) -> bool {
        self.summaries.values().any(Summary::active)
            || self.hooks.values().any(|h| h.event != "Stop" && now_ms() - h.at_ms < 120_000)
    }

    fn apply_summaries(&mut self, j: &Value) {
        let Some(ts) = j["trajectorySummaries"].as_object() else { return };
        let now = now_ms();
        self.ls_loaded = ts.keys().cloned().collect();
        // A failed read was a database read; the language_server can be asked right away.
        let ls_loaded = &self.ls_loaded;
        self.failed.retain(|id, _| !ls_loaded.contains(id));
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
                parent: tm["parentConversationId"].as_str().filter(|p| *p != id).map(str::to_string),
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
    fn conv_req(&self, id: &str) -> ConvReq {
        let conv = self.convs.get(id);
        if self.ls_loaded.contains(id) {
            let len = conv.map_or(0, |c| c.calls.len());
            // Re-read the last record: it may have been written before its usage was filled in.
            let steps = self.summaries.get(id).map(|s| s.step_count);
            ConvReq::Ls { id: id.to_string(), offset: len.saturating_sub(1), len, steps }
        } else {
            ConvReq::Db { id: id.to_string(), have: conv.filter(|c| c.loaded_once).and_then(|c| c.db_sig) }
        }
    }

    /// Merges records fetched for `req`. The caller re-dates changed conversations.
    fn apply_conv(&mut self, req: &ConvReq, data: ConvData) -> Applied {
        let now = Instant::now();
        match (req, data) {
            (ConvReq::Ls { id, offset, len, steps }, ConvData::Ls(j)) => {
                let seen_at = now_ms();
                // As planned, not as now: if the summary moved on during the fetch (the panel and
                // the engine loop fetch concurrently), the next plan sees the steps changed.
                let step_count = *steps;
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
                let fresh: Vec<LiveCall> = j["generatorMetadata"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .enumerate()
                            .map(|(i, g)| {
                                let live_seen = match prev_last {
                                    Some((_, _, ls)) if i == 0 => ls,
                                    _ => was_loaded,
                                };
                                LiveCall::from_json(g, id, seen_at, live_seen)
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
            (ConvReq::Db { id, have }, ConvData::Db(Some((sig, mut calls, summary)))) => {
                if self.ls_loaded.contains(id.as_str()) {
                    // The language_server loaded it meanwhile; read it from there instead.
                    return Applied::Stale;
                }
                if self.convs.get(id.as_str()).is_some_and(|c| c.loaded_once && c.db_sig != *have) {
                    // Another read was merged since this one was planned; it may be newer.
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
                self.summaries.insert(id.clone(), *summary);
                Applied::Changed
            }
            _ => Applied::Stale,
        }
    }

    /// Merges a `GetCascadeTrajectorySteps` page (from step `from`) for a visible subagent, so
    /// the bar can show `waiting`, live elapsed duration, total steps, and a recent-steps timeline.
    fn apply_sub_steps(&mut self, id: &str, from: usize, running: bool, j: &Value) {
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

    /// Merges what was read from transcript.jsonl (see `transcript::read_since`), then dates
    /// calls and labels turns from it; `force` re-dates even if nothing new was read (the records
    /// changed). Undated calls fall back to neighbouring calls or the conversation's modified time
    /// rather than `now`, so loading a historical conversation never pollutes today's cost.
    fn apply_transcript(&mut self, id: &str, chunk: Option<TranscriptChunk>, force: bool) {
        let summary_mod = self.summaries.get(id).map(|s| s.modified_ms).unwrap_or(0);
        let disk_mod = self.disk.get(id).copied().unwrap_or(0);
        let Some(conv) = self.convs.get_mut(id) else { return };
        let t = conv.transcript.get_or_insert_with(Transcript::default);
        let parsed_new = chunk.and_then(|c| t.apply(c)).unwrap_or(false);
        let file_mod = if disk_mod > 0 { disk_mod } else { t.mtime_ms() };
        let conv_mod = summary_mod.max(file_mod);
        if force || parsed_new || conv_mod != conv.last_conv_mod {
            conv.last_conv_mod = conv_mod;
            date_calls(&mut conv.calls, &t.steps, conv_mod);
            conv.rev += 1;
        }
    }

    fn apply_quota(&mut self, j: Value) {
        self.quota = j.get("response").cloned().unwrap_or(j);
        self.quota_at_ms = now_ms();
        self.quota_at = Some(Instant::now());
        self.quota_poke = false;
    }

    fn apply_user_status(&mut self, j: Option<&Value>) {
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
            for c in conv.calls.iter().filter(|c| c.has_usage() && c.at_ms >= midnight) {
                live_by_conv.entry(cid.clone()).or_default().entry(c.model.bucket_key()).or_default().add(
                    c.tokens.input,
                    c.tokens.cache_read,
                    c.tokens.cache_write,
                    c.tokens.output,
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
    pub fn live_call_records(&self) -> Vec<Call> {
        self.convs
            .values()
            .flat_map(|conv| conv.calls.iter())
            .filter(|c| c.has_usage() && c.at_ms > 0)
            .map(|c| c.call.clone())
            .collect()
    }

    fn root_of(&self, id: &str) -> String {
        let mut cur = id.to_string();
        for _ in 0..8 {
            match self.summaries.get(&cur).and_then(|s| s.root.clone()) {
                Some(r) if r != cur => cur = r,
                _ => break,
            }
        }
        cur
    }

    fn is_running(&self, id: &str, now: i64) -> bool {
        if self.summaries.get(id).is_some_and(Summary::active) {
            return true;
        }
        // Hooks fire before the next status poll; trust a fresh PreInvocation/PostInvocation.
        self.hooks.get(id).is_some_and(|h| h.event != "Stop" && now - h.at_ms < 15_000)
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

    fn last_call(&self, id: &str) -> Option<&LiveCall> {
        self.convs.get(id)?.calls.iter().rev().find(|c| c.has_usage())
    }

    fn quota_for(&self, call: Option<&LiveCall>, now: i64) -> Vec<QuotaView> {
        let groups = self.quota["groups"].as_array().cloned().unwrap_or_default();
        if groups.is_empty() {
            return Vec::new();
        }
        let name = call.map(|c| model_family(&c.model.display)).unwrap_or_default();
        let group = groups
            .iter()
            .find(|g| !name.is_empty() && g["description"].as_str().unwrap_or("").contains(&name))
            .or_else(|| {
                // Claude/GPT models live in the third-party group.
                let third = call.is_some_and(|c| {
                    let k = c.model.price_key();
                    k.contains("claude") || k.contains("gpt")
                });
                groups.iter().find(|g| {
                    let d = g["displayName"].as_str().unwrap_or("").to_lowercase();
                    if third { d.contains("claude") } else { d.contains("gemini") }
                })
            })
            .unwrap_or(&groups[0]);
        quota_views(group, now)
    }

    /// Computes today's total cost and tokens (desktop live + persisted snapshot + `agy CLI`),
    /// returning `(total_usd, total_tokens, cli_usd)`.
    fn today_totals(&self, pricer: &Pricer, clock: Clock) -> (f64, u64, f64) {
        let midnight = clock.midnight;
        let fp = pricer.fingerprint();
        let (mut live_cost, mut live_tok) = (0.0, 0u64);
        for conv in self.convs.values() {
            let (cost, tok) = match conv.today_cost.get() {
                Some(t) if t.rev == conv.rev && t.midnight == midnight && t.rules == fp => (t.cost, t.tokens),
                _ => {
                    let (mut cost, mut tok) = (0.0, 0u64);
                    for c in conv.calls.iter().filter(|c| c.has_usage() && c.at_ms >= midnight) {
                        cost += pricer.call_cost(c).total();
                        tok += c.tokens.input + c.tokens.cache_read + c.tokens.cache_write + c.tokens.output;
                    }
                    conv.today_cost.set(Some(TodayCost { rev: conv.rev, midnight, rules: fp, cost, tokens: tok }));
                    (cost, tok)
                }
            };
            live_cost += cost;
            live_tok += tok;
        }
        // Snapshot today's persisted aggregates + CLI snapshot briefly under USAGE lock, then price lock-free.
        let today = crate::usage::local_day_str(midnight);
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
            // On a price-cut day, the calls this engine holds and the CLI's calls are priced at their
            // own time, and the rest of today's aggregates at noon.
            let cut = pricer.is_cut_day(midnight);
            let desk_exact: Vec<Call> = if cut {
                let today_calls = self.convs.values().flat_map(|conv| conv.calls.iter());
                today_calls.filter(|c| c.has_usage() && c.at_ms >= midnight).map(|c| c.call.clone()).collect()
            } else {
                Vec::new()
            };
            let cli_exact =
                if cut && !cli_raw.is_empty() { cli_snap.calls_for_day(&cd, &pd, &today) } else { Vec::new() };
            let sum = |d: DayCost| d.values().fold((0.0, 0u64), |(u, t), (usd, agg)| (u + usd, t + agg.total_tokens()));
            let (desk_usd, desk_tok) = sum(pricer.price_day(&desk_raw, cut.then_some(desk_exact.as_slice()), midnight));
            let (cli_usd, cli_tok) = sum(pricer.price_day(&cli_raw, cut.then_some(cli_exact.as_slice()), midnight));
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
    fn conv_misses(&self, id: &str, pricer: &Pricer) -> Vec<Option<MissInfo>> {
        let Some(conv) = self.convs.get(id) else {
            return Vec::new();
        };
        let markers = conv.transcript.as_ref().map(|t| &t.markers);
        miss_scan(conv.calls.iter().filter(|c| c.has_usage()).map(|c| &c.call), pricer, markers)
    }

    /// The status bar's payload for the conversations in `panes`, as of `clock`.
    pub fn build_payload(
        &self,
        panes: &HashSet<String>,
        cfg: &Config,
        online: Option<&OnlinePrices>,
        clock: Clock,
    ) -> Value {
        let now = clock.now;
        let pricer = Pricer::new(&cfg.models, online, now);
        let (today, _, today_cli) = self.today_totals(&pricer, clock);
        let price_src = pricer.source().to_string();

        let mut views = serde_json::Map::new();
        for id in panes {
            views.insert(id.clone(), serde_json::to_value(self.view(id, cfg, &pricer, clock)).unwrap_or(Value::Null));
        }
        let groups: Vec<Value> = self.quota["groups"]
            .as_array()
            .map(|gs| {
                gs.iter()
                    .map(|g| {
                        json!({
                            "name": g["displayName"],
                            "models": g["description"],
                            "buckets": quota_views(g, now),
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

    fn agent_line(&self, id: &str, cfg: &Config, pricer: &Pricer, now: i64) -> AgentLine {
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
        for c in calls.iter().filter(|c| c.has_usage()) {
            own_cost += pricer.call_cost(c).total();
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
            running: self.is_running(id, now),
            waiting,
            model: last.map(|c| c.model.display.clone()),
            resp_model: last.map(|c| c.model.raw.clone()),
            ctx_used: last.map(|c| c.ctx_used),
            ctx_max,
            compactions,
            cache_read: last.map(|c| c.tokens.cache_read),
            cache_write: last.map(|c| c.tokens.cache_write),
            cache_hit: last.and_then(|c| {
                let denom = c.tokens.cache_read + c.tokens.input;
                (denom > 0).then(|| c.tokens.cache_read as f64 / denom as f64)
            }),
            ttft_ms: last.and_then(|c| c.ttft_ms),
            tps: last.and_then(|c| c.tps()),
            out_tokens: last.map(|c| c.tokens.output),
            cost: own_cost,
            started_ms,
            ended_ms,
            step_total,
            current_step,
            timeline,
            last_at: last.map(|c| c.at_ms),
            calls: calls.iter().filter(|c| c.has_usage()).count(),
        }
    }

    fn view(&self, id: &str, cfg: &Config, pricer: &Pricer, clock: Clock) -> View {
        let now = clock.now;
        let root = self.root_of(id);
        let is_sub = root != id;
        let main = self.agent_line(id, cfg, pricer, now);
        let includes_subagents = !is_sub && cfg.include_subagents;
        let ids = if includes_subagents { self.tree(&root) } else { vec![id.to_string()] };
        let midnight = clock.midnight;
        let mut cost_conv = 0.0;
        let mut cost_conv_today = 0.0;
        let mut multi_day = false;
        for cid in &ids {
            if let Some(conv) = self.convs.get(cid) {
                for c in conv.calls.iter().filter(|c| c.has_usage()) {
                    let cost = pricer.call_cost(c).total();
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
        let cost_last = last.map(|c| pricer.call_cost(c));
        let last_usd = cost_last.map(|k| k.total());
        let last_costly = last_usd.is_some_and(|u| cfg.costly_call_usd > 0.0 && u >= cfg.costly_call_usd);
        let last_call_key = last.map(|c| format!("{}:{}:{}", id, main.calls, c.at_ms));
        let last_miss = self.conv_misses(id, pricer).into_iter().last().flatten();
        let price = last.and_then(|c| pricer.rule(&c.model)).map(|r| r.display_name.clone());

        let mut subs = Vec::new();
        if !is_sub {
            let mut ids: Vec<&String> = self
                .summaries
                .iter()
                .filter(|(sid, s)| *sid != id && s.root.as_deref() == Some(root.as_str()))
                .filter(|(sid, _)| {
                    self.is_running(sid, now) || self.sub_done.get(*sid).is_some_and(|t| now - t < SUB_DONE_SHOW_MS)
                })
                .map(|(sid, _)| sid)
                .collect();
            ids.sort_by_key(|sid| {
                (!self.is_running(sid, now), std::cmp::Reverse(self.last_call(sid).map(|c| c.at_ms)))
            });
            let more = ids.len().saturating_sub(MAX_SUBS);
            subs = ids.into_iter().take(MAX_SUBS).map(|sid| self.agent_line(sid, cfg, pricer, now)).collect();
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
            quota: self.quota_for(last, now),
            subs,
        }
    }

    pub fn summaries(&self) -> &HashMap<String, Summary> {
        &self.summaries
    }
}

fn date_calls(calls: &mut [LiveCall], steps: &HashMap<u64, crate::transcript::StepInfo>, conv_mod_ms: i64) {
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

fn quota_views(group: &Value, now: i64) -> Vec<QuotaView> {
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
                        (Some(r), Some(span)) => Some(1.0 - ((r - now) as f64 / span as f64).clamp(0.0, 1.0)),
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
fn model_family(display: &str) -> String {
    display.split(" (").next().unwrap_or(display).trim().to_string()
}

/// Start of today in local time (see `day_start_in` for days whose 00:00 does not exist).
fn local_midnight_ms() -> i64 {
    local_midnight_at(now_ms())
}

/// When a view is built: the wall clock, read once per request, and the start of its local day.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub now: i64,
    /// Local midnight starting the day that contains `now`.
    pub midnight: i64,
}

impl Clock {
    pub fn read() -> Self {
        let now = now_ms();
        Self { now, midnight: local_midnight_at(now) }
    }
}

/// Local midnight of the day containing `now_ms`.
fn local_midnight_at(now_ms: i64) -> i64 {
    let Some(now) = Local.timestamp_millis_opt(now_ms).earliest() else { return now_ms };
    // `now` is itself a valid instant of today, so the fallback is never reached in practice;
    // if it were, counting nothing as today beats counting all history as today.
    crate::usage::day_start_in(now.date_naive(), &Local).unwrap_or_else(|| now.timestamp_millis())
}

// ---------------------------------------------------------------- daemon loop

/// Port file the hook reads to find the UDP socket.
pub fn port_file(plugin_data: &Path) -> PathBuf {
    plugin_data.join("live.port")
}

/// The engine is shared with the side panel's HTTP handlers, which read details from it directly.
pub static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

/// The side panel's JSON for `requested` (default: the most recent conversation). What it needs
/// and is not loaded yet is read with the ENGINE lock released, as in the engine loop, so a panel
/// request never holds the lock across an RPC or a database read. If the engine loop reads the
/// same conversation meanwhile, whichever result is merged second is dropped as stale (see
/// `ConvReq`).
pub fn panel_json(
    shared: Option<&SharedRef>,
    requested: Option<&str>,
    cfg: &Config,
    online: Option<&OnlinePrices>,
) -> Value {
    let engine = || ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    let target = match engine().as_ref() {
        Some(eng) => eng.panel_target(requested),
        None => return json!({ "id": null, "error": "language_server not connected" }),
    };
    let Some(id) = target else { return json!({ "id": null }) };
    let mut tried = HashSet::new();
    for _ in 0..tick::LOAD_ROUNDS {
        let Some(plan) = engine().as_mut().and_then(|eng| eng.plan_load(&id, &mut tried)) else { break };
        let fetched = plan.fetch();
        if let Some(eng) = engine().as_mut() {
            eng.apply_convs(fetched);
        }
    }
    // Keeps the engine loop refreshing this conversation while the panel shows it. Registered
    // after loading, so the engine loop doesn't read the same conversations alongside this load.
    if let Some(shared) = shared
        && let Ok(mut s) = shared.0.lock()
    {
        s.panel.insert(id.clone(), Instant::now());
    }
    match engine().as_ref() {
        Some(eng) => eng.panel(&id, cfg, online, Clock::read()),
        None => json!({ "id": null, "error": "language_server not connected" }),
    }
}

const LS_GONE_AFTER: Duration = Duration::from_secs(20);

pub fn run(shared: SharedRef) {
    let app_data = find_app_data_dir();
    let plugin_data = find_plugin_data_dir(&app_data);
    init_usage_store(app_data.clone(), plugin_data.clone(), shared.clone());
    spawn_hook_listener(&plugin_data, shared.clone());

    let mut last_payload = String::new();
    // `None` = never synced, so the first tick syncs. Not `Instant::now() - 60s`: on Windows an
    // Instant cannot predate boot, and that subtraction panics in the first minute of uptime.
    let mut last_usage_sync: Option<Instant> = None;
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

        // The ENGINE lock is taken only to plan and to merge; RPCs, SQLite and transcript reads,
        // directory scans and git run unlocked so side-panel requests are never stuck behind them.
        // Finding the language_server spawns processes and probes ports, so it runs before the
        // ENGINE lock is taken; nothing else installs an engine meanwhile.
        let connected = if ENGINE.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            match LsClient::connect() {
                Some(ls) => {
                    let addr = ls.addr.clone();
                    set_status(&shared, |s| s.ls_status = format!("connected to {addr}"));
                    Some(Engine::new(ls))
                }
                None => {
                    set_status(&shared, |s| s.ls_status = "language_server not found".into());
                    None
                }
            }
        } else {
            None
        };
        let status_plan = {
            let mut engine = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            if engine.is_none() {
                *engine = connected;
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
                if last_usage_sync.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
                    writes = eng.sync_usage_store();
                    last_usage_sync = Some(Instant::now());
                }
                let running = eng.summaries.values().filter(|s| s.active()).count();
                let ui_zh = cfg.ui_zh;
                set_status(&shared, |s| {
                    s.ui_zh = ui_zh;
                    s.running = running;
                });
                let active_online = cfg.online_data_sync.then_some(online_prices).flatten();
                let payload = eng.build_payload(&panes, &cfg, active_online.as_ref(), Clock::read()).to_string();
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
        let c = LiveCall::from_json(&empty, "c", 9_999, false);
        assert!(!c.has_usage());
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
                    "responseOutputTokens": "40",
                    "responseId": "resp-9"
                }
            },
            "stepIndices": [1]
        });
        let c2 = LiveCall::from_json(&real, "c", 9_999, true);
        assert!(c2.has_usage());
        assert_eq!(c2.tokens.cache_write, 500);
        assert_eq!(c2.tokens.output, 120);
        assert_eq!(c2.response_id.as_deref(), Some("resp-9"));
        assert_eq!((c2.conv_id.as_str(), c2.at_ms), ("c", 9_999));
        let pricer = Pricer::new(&crate::config::default_model_rules(), None, 0);
        let cost = pricer.call_cost(&c2);
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
            LiveCall::from(Call { steps: vec![1], ..Default::default() }),
            LiveCall::from(Call { steps: vec![], ..Default::default() }),
        ];
        date_calls(&mut old_calls, &steps, day1_end);
        assert_eq!(old_calls[0].at_ms, day1);
        assert!(old_calls[0].dated);
        assert_eq!(old_calls[1].at_ms, day1_end);
        assert!(!old_calls[1].dated);

        let mut cross = vec![
            LiveCall::from(Call { steps: vec![], ..Default::default() }),
            LiveCall::from(Call { steps: vec![1], ..Default::default() }),
            LiveCall::from(Call { steps: vec![], ..Default::default() }),
            LiveCall::from(Call { steps: vec![5], ..Default::default() }),
            LiveCall::from(Call { steps: vec![10], ..Default::default() }),
            LiveCall {
                call: Call { steps: vec![12], at_ms: day2 + 5_000, ..Default::default() },
                live_seen: true,
                ..Default::default()
            },
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

    #[test]
    fn apply_summaries_tracks_parent_root_and_ls_loaded() {
        let mut eng = Engine::new(LsClient::new("127.0.0.1:0", "tok", false));
        eng.apply_summaries(&json!({
            "trajectorySummaries": {
                "root-1": {
                    "status": "CASCADE_RUN_STATUS_IDLE",
                    "stepCount": 4,
                    "trajectoryMetadata": {
                        "rootConversationId": "root-1"
                    }
                },
                "child-1": {
                    "status": "CASCADE_RUN_STATUS_IDLE",
                    "stepCount": 2,
                    "trajectoryMetadata": {
                        "parentConversationId": "root-1",
                        "rootConversationId": "root-1",
                        "subagentSpec": { "typeName": "browser", "role": "Blog Previewer" }
                    }
                },
                "grand-1": {
                    "status": "CASCADE_RUN_STATUS_RUNNING",
                    "stepCount": 1,
                    "trajectoryMetadata": {
                        "parentConversationId": "child-1",
                        "rootConversationId": "root-1",
                        "subagentSpec": { "typeName": "research", "role": "Deep Reader" }
                    }
                }
            }
        }));
        assert_eq!(eng.summaries["root-1"].parent, None);
        assert_eq!(eng.summaries["root-1"].root, None);
        assert_eq!(eng.summaries["child-1"].parent.as_deref(), Some("root-1"));
        assert_eq!(eng.summaries["child-1"].root.as_deref(), Some("root-1"));
        assert_eq!(eng.summaries["grand-1"].parent.as_deref(), Some("child-1"));
        assert_eq!(eng.summaries["grand-1"].root.as_deref(), Some("root-1"));
        assert!(eng.ls_loaded.contains("child-1"));
        assert!(eng.ls_loaded.contains("grand-1"));
        assert!(!eng.ls_loaded.contains("historical-only"));
    }
}
