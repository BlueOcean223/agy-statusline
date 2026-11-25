//! Persistent daily usage history (up to 400 days), SQLite conversation backfill (desktop + agy CLI),
//! segmented weekly quota accounting, exact cycle-start cap forecasting, and cache-miss day breakdowns.
//!
//! Stores only numeric aggregates and masked account metadata in `plugin_data/`:
//! - `usage-history.json`: up to 400 days of per-model call/token aggregates (desktop & CLI),
//!   per-account segmented weekly quota deltas, early-reset events, and ended-cycle records.
//! - `quota-samples.json`: up to 8 days of periodic weekly quota snapshots for forecasting & replay.
//! - `cli-index.json`: day index of `~/.gemini/antigravity-cli/conversations/*.db` files.

use crate::cli_usage::{CliBackfillMeta, CliScanner, db_mtime_ms};
use crate::config::{ModelRule, ReadFile, any_price_cut_in, read_json_keep, write_atomic};
use crate::miss::{MissCallInput, MissInfo, MissSummary, miss_scan};
pub use crate::quota::{
    CycleRecord, DayQuotaRec, QuotaBucketSample, QuotaSamplePoint, QuotaSamples, ResetEvent, note_cycle_end,
    note_quota, note_reset_event, r4, replay_from_samples, same_period,
};
use crate::transcript::{read_annotation, scan_step_markers};
use chrono::{Local, TimeZone};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

pub const HISTORY_KEEP_DAYS: usize = 400;
const SAMPLES_KEEP_MS: i64 = 8 * 86_400_000;
const BACKFILL_V: u32 = 3;
pub const DEFAULT_DAY_CALLS_LIMIT: usize = 600;

fn r6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

pub fn local_day_str(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string())
}

pub fn today_str() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

pub fn day_start_ms(date: &str) -> Option<i64> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|dt| dt.and_local_timezone(Local).earliest())
        .map(|dt| dt.timestamp_millis())
}

pub fn day_noon_ms(date: &str) -> Option<i64> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(12, 0, 0))
        .and_then(|dt| dt.and_local_timezone(Local).earliest())
        .map(|dt| dt.timestamp_millis())
}

// ---------------------------------------------------------------- account masking

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    /// 8-char lowercase FNV-1a hash of the lowercased email (never stores raw email on disk).
    pub key: String,
    /// Masked email, e.g. `ab***12@gmail.com`.
    pub masked: String,
    /// Subscription tier name, e.g. `Google AI Ultra`.
    pub tier: String,
}

pub fn mask_email(email: &str) -> String {
    let trimmed = email.trim();
    let Some((user, domain)) = trimmed.split_once('@') else {
        return String::new();
    };
    let chars: Vec<char> = user.chars().collect();
    let prefix = if chars.len() >= 6 {
        format!("{}{}***{}{}", chars[0], chars[1], chars[chars.len() - 2], chars[chars.len() - 1])
    } else if !chars.is_empty() {
        format!("{}***", chars[0])
    } else {
        "***".to_string()
    };
    format!("{prefix}@{domain}")
}

pub fn fnv1a_hex(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

pub fn account_from_user_status(v: &Value) -> Option<AccountInfo> {
    let u = &v["userStatus"];
    let email = u["email"].as_str()?.trim();
    if email.is_empty() {
        return None;
    }
    let tier = u["userTier"]["name"].as_str().unwrap_or("").chars().take(60).collect();
    Some(AccountInfo { key: fnv1a_hex(&email.to_lowercase()), masked: mask_email(email), tier })
}

// ---------------------------------------------------------------- model pricing helper

pub fn match_rule<'a>(rules: &'a [ModelRule], key: &str) -> Option<&'a ModelRule> {
    let low = key.to_lowercase();
    rules.iter().find(|r| !r.pattern.is_empty() && low.contains(&r.pattern.to_lowercase()))
}

#[allow(clippy::too_many_arguments)]
pub fn price_tokens(
    rules: &[ModelRule],
    model_key: &str,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    at_ms: i64,
    now_ms: i64,
) -> (Option<String>, f64) {
    let Some(r) = match_rule(rules, model_key) else {
        return (None, 0.0);
    };
    let (p_in, p_cr, p_cw, p_out) = r.rates_at(at_ms, now_ms);
    let usd =
        (input as f64 * p_in + cache_read as f64 * p_cr + cache_write as f64 * p_cw + output as f64 * p_out) / 1e6;
    (Some(r.short_name.clone()), usd)
}

pub fn quota_group_id(display_name: &str, window: &str) -> String {
    let low = display_name.to_lowercase();
    let prefix = if low.contains("claude") || low.contains("gpt") || low.contains("3p") || low.contains("third") {
        "3p"
    } else if low.contains("gemini") {
        "gemini"
    } else {
        "other"
    };
    format!("{prefix}-{window}")
}

fn model_belongs_to_group(model_name: &str, bucket_id: &str) -> bool {
    let low = model_name.to_lowercase();
    let is_3p = low.contains("claude")
        || low.contains("opus")
        || low.contains("sonnet")
        || low.contains("haiku")
        || low.contains("gpt");
    if bucket_id.starts_with("3p-") {
        is_3p
    } else if bucket_id.starts_with("gemini-") {
        !is_3p
    } else {
        true
    }
}

// ---------------------------------------------------------------- on-disk history

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAgg {
    pub calls: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl ModelAgg {
    pub fn add(&mut self, input: u64, cr: u64, cw: u64, out: u64) {
        self.calls += 1;
        self.input += input;
        self.cache_read += cr;
        self.cache_write += cw;
        self.output += out;
    }

    pub fn merge_from(&mut self, other: &ModelAgg) {
        self.calls += other.calls;
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
    }

    pub fn total_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DayRecord {
    /// True if populated by historical SQLite backfill (before live tracking began).
    #[serde(default)]
    pub bf: bool,
    #[serde(default)]
    pub convs: usize,
    /// Desktop raw model key (e.g. `gemini-4-neon`) -> token & call totals.
    #[serde(default)]
    pub raw: HashMap<String, ModelAgg>,
    /// `agy CLI` raw model key -> token & call totals.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub cli_raw: HashMap<String, ModelAgg>,
    #[serde(default)]
    pub cli_convs: usize,
    /// Per-conversation live snapshot for today so restarts never lose today's counts.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub by_conv: HashMap<String, HashMap<String, ModelAgg>>,
    /// Per-account quota records: `acct_key -> bucket_id -> DayQuotaRec`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub qa: HashMap<String, HashMap<String, DayQuotaRec>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccountRecord {
    pub masked: String,
    pub tier: String,
    pub first_day: String,
    pub last_day: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BackfillMeta {
    pub version: u32,
    pub at_ms: i64,
    pub dbs: usize,
    pub calls: usize,
    pub days: usize,
    pub errors: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageHistory {
    pub v: u32,
    #[serde(default)]
    pub live_from: String,
    #[serde(default)]
    pub cur_acct: Option<String>,
    #[serde(default)]
    pub accts: HashMap<String, AccountRecord>,
    #[serde(default)]
    pub days: BTreeMap<String, DayRecord>,
    #[serde(default)]
    pub bf: Option<BackfillMeta>,
    #[serde(default)]
    pub cli_bf: Option<CliBackfillMeta>,
    /// Early weekly quota reset events detected across accounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resets: Vec<ResetEvent>,
    /// Ended weekly quota cycles recorded for cap stability comparison.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cycles: Vec<CycleRecord>,
    /// Quota replay migration version (1 = replayed existing samples through segmented algorithm).
    #[serde(default)]
    pub qr: u32,
    /// False if the on-disk file failed to read due to a transient I/O error: skip writes to avoid overwriting.
    #[serde(skip)]
    pub writable: bool,
}

pub fn load_history(plugin_data: &Path) -> UsageHistory {
    let path = plugin_data.join("usage-history.json");
    match read_json_keep::<UsageHistory>(&path) {
        ReadFile::Ok(mut h) => {
            if h.live_from.is_empty() {
                h.live_from = today_str();
            }
            h.writable = true;
            h
        }
        ReadFile::Missing | ReadFile::Corrupt { kept: true } => {
            UsageHistory { v: 2, live_from: today_str(), writable: true, ..Default::default() }
        }
        ReadFile::Unreadable | ReadFile::Corrupt { kept: false } => {
            UsageHistory { v: 2, live_from: today_str(), writable: false, ..Default::default() }
        }
    }
}

pub fn save_history(plugin_data: &Path, h: &UsageHistory) {
    if !h.writable {
        return;
    }
    let path = plugin_data.join("usage-history.json");
    if let Ok(text) = serde_json::to_string(h) {
        let _ = write_atomic(&path, &text);
    }
}

pub fn load_samples(plugin_data: &Path) -> (QuotaSamples, bool) {
    let path = plugin_data.join("quota-samples.json");
    match read_json_keep::<QuotaSamples>(&path) {
        ReadFile::Ok(s) => (s, true),
        ReadFile::Missing | ReadFile::Corrupt { kept: true } => (QuotaSamples { v: 1, by_acct: HashMap::new() }, true),
        ReadFile::Unreadable | ReadFile::Corrupt { kept: false } => {
            (QuotaSamples { v: 1, by_acct: HashMap::new() }, false)
        }
    }
}

pub fn save_samples(plugin_data: &Path, s: &QuotaSamples, writable: bool) {
    if !writable {
        return;
    }
    let path = plugin_data.join("quota-samples.json");
    if let Ok(text) = serde_json::to_string(s) {
        let _ = write_atomic(&path, &text);
    }
}

/// Runs one-time sample replay (migrating pre-segmented quota history) and seeds `cycles`
/// from earlier recorded early-reset events if `cycles` is empty.
pub fn maybe_replay_samples(h: &mut UsageHistory, samples: &QuotaSamples) -> bool {
    let mut dirty = false;
    if h.qr < 1 {
        let mut days_qa: BTreeMap<String, HashMap<String, HashMap<String, DayQuotaRec>>> =
            h.days.iter().map(|(d, r)| (d.clone(), r.qa.clone())).collect();
        let mut resets = h.resets.clone();
        let mut cycles = h.cycles.clone();
        let changes = replay_from_samples(
            &mut days_qa,
            samples,
            local_day_str,
            |d| day_start_ms(d).unwrap_or(0),
            |acct_key, b, at_ms, ev| {
                note_reset_event(&mut resets, acct_key, b, at_ms, ev);
                note_cycle_end(&mut cycles, acct_key, b, at_ms, ev);
            },
        );
        for (d, qa) in days_qa {
            if let Some(rec) = h.days.get_mut(&d) {
                rec.qa = qa;
            }
        }
        h.resets = resets;
        h.cycles = cycles;
        h.qr = 1;
        if !changes.is_empty() {
            eprintln!("[agy-statusline] quota replay adjusted {} day bucket(s)", changes.len());
        }
        dirty = true;
    }
    if h.cycles.is_empty() && !h.resets.is_empty() {
        for r in &h.resets {
            if r.window == "weekly" && r.was_reset_ms > 0 && r.from_used > 0.0 {
                h.cycles.push(CycleRecord {
                    acct_key: r.acct_key.clone(),
                    bucket_id: r.bucket_id.clone(),
                    name: r.name.clone(),
                    start_ms: r.was_reset_ms - crate::quota::WEEK_MS,
                    end_ms: r.at_ms,
                    used: r.from_used,
                    early: true,
                });
                dirty = true;
            }
        }
    }
    dirty
}

fn prev_quota_of(h: &UsageHistory, today: &str, acct_key: &str, bucket_id: &str) -> Option<DayQuotaRec> {
    h.days
        .range(..today.to_string())
        .rev()
        .take(14)
        .find_map(|(_, d)| d.qa.get(acct_key).and_then(|m| m.get(bucket_id)).cloned())
}

fn conv_calls(m: &HashMap<String, ModelAgg>) -> u64 {
    m.values().map(|a| a.calls).sum()
}

/// Merges today's live per-conversation call aggregates and latest quota/account snapshot into history.
pub fn update_today(
    h: &mut UsageHistory,
    samples: &mut QuotaSamples,
    today: &str,
    live_by_conv: &HashMap<String, HashMap<String, ModelAgg>>,
    buckets: &[QuotaBucketSample],
    acct: Option<&AccountInfo>,
    quota_at_ms: i64,
) -> (bool, bool) {
    let mut hist_dirty = false;
    let mut samp_dirty = false;

    let prev_map: HashMap<String, DayQuotaRec> = if let Some(a) = acct {
        buckets.iter().filter_map(|b| prev_quota_of(h, today, &a.key, &b.id).map(|p| (b.id.clone(), p))).collect()
    } else {
        HashMap::new()
    };

    if h.live_from.is_empty() {
        h.live_from = today.to_string();
        hist_dirty = true;
    }

    for (k, d) in h.days.iter_mut() {
        if k.as_str() < today && !d.by_conv.is_empty() {
            d.by_conv.clear();
            hist_dirty = true;
        }
    }

    let day = h.days.entry(today.to_string()).or_default();
    day.bf = false;
    for (cid, models) in live_by_conv {
        let new_calls = conv_calls(models);
        let old_calls = day.by_conv.get(cid).map(conv_calls).unwrap_or(0);
        if new_calls > old_calls || (new_calls > 0 && new_calls == old_calls && !day.by_conv.contains_key(cid)) {
            day.by_conv.insert(cid.clone(), models.clone());
            hist_dirty = true;
        }
    }
    if !day.by_conv.is_empty() {
        let mut rebuilt: HashMap<String, ModelAgg> = HashMap::new();
        let mut active_convs = 0usize;
        for m in day.by_conv.values() {
            if conv_calls(m) > 0 {
                active_convs += 1;
            }
            for (model, agg) in m {
                rebuilt.entry(model.clone()).or_default().merge_from(agg);
            }
        }
        let prev_calls: u64 = day.raw.values().map(|a| a.calls).sum();
        let next_calls: u64 = rebuilt.values().map(|a| a.calls).sum();
        if next_calls >= prev_calls {
            if next_calls != prev_calls || day.convs != active_convs {
                hist_dirty = true;
            }
            day.raw = rebuilt;
            day.convs = active_convs;
        }
    }

    if let Some(a) = acct
        && !buckets.is_empty()
    {
        let acct_rec = h.accts.entry(a.key.clone()).or_insert_with(|| AccountRecord {
            masked: a.masked.clone(),
            tier: a.tier.clone(),
            first_day: today.to_string(),
            last_day: today.to_string(),
        });
        if acct_rec.masked != a.masked || acct_rec.tier != a.tier || acct_rec.last_day != today {
            acct_rec.masked = a.masked.clone();
            acct_rec.tier = a.tier.clone();
            acct_rec.last_day = today.to_string();
            hist_dirty = true;
        }
        if h.cur_acct.as_deref() != Some(a.key.as_str()) {
            h.cur_acct = Some(a.key.clone());
            hist_dirty = true;
        }

        let mut new_resets = h.resets.clone();
        let mut new_cycles = h.cycles.clone();
        let mut events_changed = false;
        {
            let day_mut = h.days.entry(today.to_string()).or_default();
            let qa = day_mut.qa.entry(a.key.clone()).or_default();
            let before_qa = serde_json::to_string(qa).ok();
            let at = if quota_at_ms > 0 { quota_at_ms } else { crate::live::now_ms() };
            note_quota(
                qa,
                buckets,
                |bid| prev_map.get(bid).cloned(),
                at,
                |b, ev| {
                    if note_reset_event(&mut new_resets, &a.key, b, at, &ev) {
                        events_changed = true;
                    }
                    if note_cycle_end(&mut new_cycles, &a.key, b, at, &ev) {
                        events_changed = true;
                    }
                },
            );
            let after_qa = serde_json::to_string(qa).ok();
            if before_qa != after_qa {
                hist_dirty = true;
            }
        }
        if events_changed {
            h.resets = new_resets;
            h.cycles = new_cycles;
            hist_dirty = true;
        }

        if quota_at_ms > 0 {
            let list = samples.by_acct.entry(a.key.clone()).or_default();
            let last_ts = list.last().map(|p| p.at_ms).unwrap_or(0);
            if quota_at_ms > last_ts {
                for b in buckets.iter().filter(|b| b.window == "weekly") {
                    list.push(QuotaSamplePoint {
                        at_ms: quota_at_ms,
                        bucket_id: b.id.clone(),
                        used: r4(b.used),
                        reset_ms: b.reset_ms,
                    });
                }
                let cut = quota_at_ms - SAMPLES_KEEP_MS;
                list.retain(|p| p.at_ms >= cut);
                samp_dirty = true;
            }
        }
    }

    while h.days.len() > HISTORY_KEEP_DAYS {
        if let Some(oldest) = h.days.keys().next().cloned() {
            h.days.remove(&oldest);
            hist_dirty = true;
        } else {
            break;
        }
    }

    (hist_dirty, samp_dirty)
}

// ---------------------------------------------------------------- lightweight protobuf reader

enum PbVal<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn read_varint(buf: &[u8], i: &mut usize) -> Option<u64> {
    let mut x = 0u64;
    let mut shift = 0u32;
    while *i < buf.len() {
        let c = buf[*i];
        *i += 1;
        if shift < 64 {
            x |= ((c & 0x7f) as u64) << shift;
        }
        if c & 0x80 == 0 {
            return Some(x);
        }
        shift += 7;
    }
    None
}

fn pb_get(buf: &[u8], target: u64) -> Option<PbVal<'_>> {
    let mut i = 0usize;
    while i < buf.len() {
        let key = read_varint(buf, &mut i)?;
        let field = key >> 3;
        let wt = key & 7;
        if field == 0 {
            return None;
        }
        match wt {
            0 => {
                let v = read_varint(buf, &mut i)?;
                if field == target {
                    return Some(PbVal::Varint(v));
                }
            }
            2 => {
                let len = read_varint(buf, &mut i)? as usize;
                let end = i.checked_add(len)?;
                if end > buf.len() {
                    return None;
                }
                let slice = &buf[i..end];
                i = end;
                if field == target {
                    return Some(PbVal::Bytes(slice));
                }
            }
            1 => {
                i = i.checked_add(8)?;
                if i > buf.len() {
                    return None;
                }
            }
            5 => {
                i = i.checked_add(4)?;
                if i > buf.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

fn pb_bytes(buf: &[u8], field: u64) -> Option<&[u8]> {
    match pb_get(buf, field)? {
        PbVal::Bytes(b) => Some(b),
        _ => None,
    }
}

fn pb_u64(buf: &[u8], field: u64) -> Option<u64> {
    match pb_get(buf, field)? {
        PbVal::Varint(v) => Some(v),
        _ => None,
    }
}

fn pb_duration_ms(buf: &[u8]) -> Option<f64> {
    let sec = pb_u64(buf, 1).unwrap_or(0) as f64;
    let nanos = pb_u64(buf, 2).unwrap_or(0) as f64;
    let ms = sec * 1000.0 + nanos / 1_000_000.0;
    (ms > 0.0).then_some(ms)
}

fn pb_varints(buf: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let Some(v) = read_varint(buf, &mut i) else { break };
        out.push(v);
    }
    out
}

// ---------------------------------------------------------------- SQLite backfill & call collection

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub conv_id: String,
    pub at_ms: i64,
    pub step: Option<u64>,
    pub model_raw: String,
    pub model_display: String,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub ttft_ms: Option<f64>,
    pub stream_ms: Option<f64>,
    pub dedup_key: String,
    /// `"desktop"` or `"cli"`.
    pub source: &'static str,
}

pub fn read_conv_db(path: &Path, conv_id: &str, source: &'static str) -> Result<Vec<CallRecord>, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).map_err(|e| e.to_string())?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));

    let mut step_at: HashMap<u64, i64> = HashMap::new();
    let mut exec_first: HashMap<String, i64> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT idx, metadata FROM steps")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)))
    {
        for row in rows.flatten() {
            let (idx, Some(meta)) = row else { continue };
            let sec = pb_bytes(&meta, 1).and_then(|ts| pb_u64(ts, 1)).unwrap_or(0);
            if sec == 0 {
                continue;
            }
            let ms = (sec as i64) * 1000;
            if idx >= 0 {
                step_at.insert(idx as u64, ms);
            }
            if let Some(ex) = pb_bytes(&meta, 12)
                && let Ok(ex_str) = std::str::from_utf8(ex)
            {
                exec_first.entry(ex_str.to_string()).or_insert(ms);
            }
        }
    }

    let mut out = Vec::new();
    let mut stmt = conn.prepare("SELECT data FROM gen_metadata").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| r.get::<_, Option<Vec<u8>>>(0)).map_err(|e| e.to_string())?;
    for row in rows.flatten() {
        let Some(data) = row else { continue };
        let Some(cm) = pb_bytes(&data, 1) else { continue };
        let Some(u) = pb_bytes(cm, 4) else { continue };
        let inp = pb_u64(u, 2).unwrap_or(0);
        let cw = pb_u64(u, 4).unwrap_or(0);
        let cr = pb_u64(u, 5).unwrap_or(0);
        let out_tok =
            pb_u64(u, 3).unwrap_or(0).max(pb_u64(u, 9).unwrap_or(0).saturating_add(pb_u64(u, 10).unwrap_or(0)));
        if inp == 0 && cr == 0 && cw == 0 && out_tok == 0 {
            continue;
        }
        let exec = pb_bytes(&data, 4).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("");
        let step_indices = match pb_get(&data, 2) {
            Some(PbVal::Bytes(b)) => pb_varints(b),
            Some(PbVal::Varint(v)) => vec![v],
            None => Vec::new(),
        };
        let at_ms = step_indices
            .iter()
            .find_map(|idx| step_at.get(idx).copied())
            .or_else(|| exec_first.get(exec).copied())
            .unwrap_or(0);
        if at_ms <= 0 {
            continue;
        }
        let dedup_key = pb_bytes(u, 11)
            .and_then(|b| std::str::from_utf8(b).ok())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if source == "cli" {
                    format!("{conv_id}:{at_ms}:{inp}:{out_tok}")
                } else {
                    format!("{exec}:{inp}:{cr}:{out_tok}")
                }
            });

        let model_raw = pb_bytes(cm, 19)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
            .chars()
            .take(64)
            .collect::<String>();
        let model_display = pb_bytes(cm, 21)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
            .chars()
            .take(64)
            .collect::<String>();
        let ttft_ms = pb_bytes(cm, 11).and_then(pb_duration_ms);
        let stream_ms = pb_bytes(cm, 12).and_then(pb_duration_ms);

        out.push(CallRecord {
            conv_id: conv_id.to_string(),
            at_ms,
            step: step_indices.first().copied(),
            model_raw: if model_raw.is_empty() { model_display.clone() } else { model_raw },
            model_display,
            input: inp,
            cache_read: cr,
            cache_write: cw,
            output: out_tok,
            ttft_ms,
            stream_ms,
            dedup_key,
            source,
        });
    }
    Ok(out)
}

pub fn is_uuid_db(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".db")?;
    (stem.len() == 36 && stem.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(stem)
}

/// Scans `<app_data>/conversations/*.db` once to backfill historical days prior to today.
pub fn needs_backfill(h: &UsageHistory) -> bool {
    h.bf.as_ref().is_none_or(|b| b.version < BACKFILL_V)
}

pub fn run_backfill(app_data: &Path, h: &mut UsageHistory, now_ms: i64) -> bool {
    if !needs_backfill(h) {
        return false;
    }
    let conv_dir = app_data.join("conversations");
    let Ok(rd) = fs::read_dir(&conv_dir) else {
        h.bf = Some(BackfillMeta { version: BACKFILL_V, at_ms: now_ms, ..Default::default() });
        return true;
    };
    let today = today_str();
    let mut seen: HashSet<String> = HashSet::new();
    let mut acc: BTreeMap<String, (HashMap<String, ModelAgg>, HashSet<String>)> = BTreeMap::new();
    let (mut dbs, mut calls, mut errors) = (0usize, 0usize, 0usize);

    for entry in rd.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        let Some(cid) = is_uuid_db(&fname) else { continue };
        match read_conv_db(&entry.path(), cid, "desktop") {
            Ok(recs) => {
                dbs += 1;
                for r in recs {
                    if !seen.insert(r.dedup_key) {
                        continue;
                    }
                    let day = local_day_str(r.at_ms);
                    if day >= today {
                        continue;
                    }
                    let model_key = if !r.model_raw.is_empty() {
                        r.model_raw
                    } else if !r.model_display.is_empty() {
                        r.model_display
                    } else {
                        "unknown".to_string()
                    };
                    let (raw, convs) = acc.entry(day).or_default();
                    raw.entry(model_key).or_default().add(r.input, r.cache_read, r.cache_write, r.output);
                    convs.insert(r.conv_id);
                    calls += 1;
                }
            }
            Err(_) => errors += 1,
        }
    }

    let mut days_added = 0usize;
    for (day, (raw, convs)) in acc {
        let entry = h.days.entry(day.clone()).or_default();
        // Check desktop records only (`entry.raw`), so days that only had CLI records still get desktop backfill.
        let existing_calls: u64 = entry.raw.values().map(|a| a.calls).sum();
        let db_calls: u64 = raw.values().map(|a| a.calls).sum();
        let is_pre_live = day < h.live_from;
        if is_pre_live || entry.bf || existing_calls == 0 || db_calls as f64 > existing_calls as f64 * 1.02 + 1.0 {
            entry.raw = raw;
            entry.convs = entry.convs.max(convs.len());
            if is_pre_live {
                entry.bf = true;
            }
            days_added += 1;
        }
    }

    while h.days.len() > HISTORY_KEEP_DAYS {
        if let Some(oldest) = h.days.keys().next().cloned() {
            h.days.remove(&oldest);
        } else {
            break;
        }
    }

    h.bf = Some(BackfillMeta { version: BACKFILL_V, at_ms: now_ms, dbs, calls, days: days_added, errors });
    true
}

/// Collects deduplicated desktop calls falling inside `[from_ms, to_ms)` from live memory + SQLite DBs.
fn collect_desktop_calls_in_range(
    app_data: &Path,
    from_ms: i64,
    to_ms: i64,
    live_calls: &[CallRecord],
) -> Vec<CallRecord> {
    let mut merged = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for c in live_calls {
        if c.at_ms < from_ms || c.at_ms >= to_ms {
            continue;
        }
        let soft_key = format!("{}:{}:{}:{}", c.conv_id, c.at_ms / 1000, c.input, c.output);
        if seen.insert(c.dedup_key.clone()) {
            seen.insert(soft_key);
            merged.push(c.clone());
        }
    }

    if let Ok(rd) = fs::read_dir(app_data.join("conversations")) {
        for entry in rd.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            let Some(cid) = is_uuid_db(&fname) else { continue };
            let mtime = db_mtime_ms(&entry.path());
            if from_ms > 0 && mtime > 0 && mtime < from_ms - 60_000 {
                continue;
            }
            if let Ok(recs) = read_conv_db(&entry.path(), cid, "desktop") {
                for r in recs {
                    if r.at_ms < from_ms || r.at_ms >= to_ms {
                        continue;
                    }
                    let soft_key = format!("{}:{}:{}:{}", r.conv_id, r.at_ms / 1000, r.input, r.output);
                    if seen.insert(r.dedup_key.clone()) && seen.insert(soft_key) {
                        merged.push(r);
                    }
                }
            }
        }
    }

    merged
}

/// Collects all desktop + CLI calls for a single calendar day (`YYYY-MM-DD`).
fn collect_all_day_calls(
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliScanner,
    date: &str,
    live_calls: &[CallRecord],
) -> Vec<CallRecord> {
    let Some(start) = day_start_ms(date) else {
        return Vec::new();
    };
    // Add 26h buffer then filter by exact local_day_str to stay DST-safe.
    let mut out: Vec<CallRecord> = collect_desktop_calls_in_range(app_data, start, start + 26 * 3_600_000, live_calls)
        .into_iter()
        .filter(|c| local_day_str(c.at_ms) == date)
        .collect();
    out.extend(cli.calls_for_day(cli_dir, plugin_data, date));
    out
}

type AnalyzedCall = (CallRecord, Option<String>, f64, Option<MissInfo>);

/// Runs per-conversation chronological `miss_scan` over a mixed list of desktop & CLI calls,
/// returning `(call, matched_short_name, usd, miss_info)` sorted newest-first.
fn analyze_calls_with_miss(
    app_data: &Path,
    calls: Vec<CallRecord>,
    rules: &[ModelRule],
    now_ms: i64,
) -> (Vec<AnalyzedCall>, MissSummary) {
    let mut by_conv: HashMap<(&'static str, String), Vec<CallRecord>> = HashMap::new();
    for c in calls {
        by_conv.entry((c.source, c.conv_id.clone())).or_default().push(c);
    }

    let mut analyzed = Vec::new();
    let mut summary = MissSummary::default();

    for ((src, cid), mut list) in by_conv {
        list.sort_by_key(|c| (c.at_ms, c.step.unwrap_or(0)));
        let markers = if src == "desktop" { scan_step_markers(app_data, &cid) } else { None };
        let mut inputs = Vec::with_capacity(list.len());
        let mut priced = Vec::with_capacity(list.len());

        for c in &list {
            let price_key = format!("{} {}", c.model_raw, c.model_display);
            if let Some(r) = match_rule(rules, &price_key) {
                let (p_in, p_cr, p_cw, p_out) = r.rates_at(c.at_ms, now_ms);
                let usd = (c.input as f64 * p_in
                    + c.cache_read as f64 * p_cr
                    + c.cache_write as f64 * p_cw
                    + c.output as f64 * p_out)
                    / 1e6;
                inputs.push(MissCallInput {
                    at_ms: c.at_ms,
                    ttft_ms: c.ttft_ms,
                    stream_ms: c.stream_ms,
                    step: c.step,
                    model_name: Some(r.short_name.clone()),
                    input: c.input,
                    cache_read: c.cache_read,
                    cache_write: c.cache_write,
                    p_in,
                    p_cache: p_cr,
                    p_cw,
                });
                priced.push((Some(r.short_name.clone()), usd));
            } else {
                inputs.push(MissCallInput {
                    at_ms: c.at_ms,
                    ttft_ms: c.ttft_ms,
                    stream_ms: c.stream_ms,
                    step: c.step,
                    model_name: None,
                    input: c.input,
                    cache_read: c.cache_read,
                    cache_write: c.cache_write,
                    p_in: 0.0,
                    p_cache: 0.0,
                    p_cw: 0.0,
                });
                priced.push((None, 0.0));
            }
        }

        let misses = miss_scan(&inputs, markers.as_ref());
        for ((c, (short_opt, usd)), miss_opt) in list.into_iter().zip(priced).zip(misses) {
            summary.total_usd += usd;
            if let Some(ref m) = miss_opt {
                summary.add(m);
            }
            analyzed.push((c, short_opt, usd, miss_opt));
        }
    }

    summary.round_fields();
    analyzed.sort_by_key(|(c, _, _, _)| std::cmp::Reverse(c.at_ms));
    (analyzed, summary)
}

/// Sums USD & tokens for a quota group over `[from_ms, to_ms)` with exact millisecond filtering on
/// boundary days (`start_day` and `end_day`) and fast daily aggregates for full days in between.
#[allow(clippy::too_many_arguments)]
fn cycle_spend(
    h: &UsageHistory,
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliScanner,
    live_calls: &[CallRecord],
    bucket_id: &str,
    from_ms: i64,
    to_ms: i64,
    rules: &[ModelRule],
    now_ms: i64,
) -> (f64, u64, bool) {
    if to_ms <= from_ms {
        return (0.0, 0, false);
    }
    let start_day = local_day_str(from_ms);
    let end_day = local_day_str(to_ms.saturating_sub(1).max(from_ms));
    let mut usd = 0.0;
    let mut tokens = 0u64;
    let mut exact = false;

    // 1. Boundary day(s): exact call-level filtering over [from_ms, to_ms).
    let mut boundary_days = vec![start_day.clone()];
    if end_day != start_day {
        boundary_days.push(end_day.clone());
    }
    for bd in &boundary_days {
        let day_calls = collect_all_day_calls(app_data, cli_dir, plugin_data, cli, bd, live_calls);
        let in_win: Vec<&CallRecord> = day_calls.iter().filter(|c| c.at_ms >= from_ms && c.at_ms < to_ms).collect();
        if !in_win.is_empty() || !day_calls.is_empty() {
            exact = true;
            for c in in_win {
                let price_key = format!("{} {}", c.model_raw, c.model_display);
                if !model_belongs_to_group(&price_key, bucket_id) {
                    continue;
                }
                let (_, u) =
                    price_tokens(rules, &price_key, c.input, c.cache_read, c.cache_write, c.output, c.at_ms, now_ms);
                usd += u;
                tokens += c.input + c.cache_read + c.cache_write + c.output;
            }
        } else if let Some(drec) = h.days.get(bd) {
            // Fallback to full-day summary if raw DBs are unavailable for this boundary day.
            let noon = day_noon_ms(bd).unwrap_or(now_ms);
            for (model, agg) in drec.raw.iter().chain(drec.cli_raw.iter()) {
                if !model_belongs_to_group(model, bucket_id) {
                    continue;
                }
                let (_, u) =
                    price_tokens(rules, model, agg.input, agg.cache_read, agg.cache_write, agg.output, noon, now_ms);
                usd += u;
                tokens += agg.total_tokens();
            }
        }
    }

    // 2. Full calendar days strictly between `start_day` and `end_day`.
    for (dk, drec) in h.days.range(start_day.clone()..=end_day.clone()) {
        if dk == &start_day || dk == &end_day {
            continue;
        }
        let noon = day_noon_ms(dk).unwrap_or(now_ms);
        for (model, agg) in drec.raw.iter().chain(drec.cli_raw.iter()) {
            if !model_belongs_to_group(model, bucket_id) {
                continue;
            }
            let (_, u) =
                price_tokens(rules, model, agg.input, agg.cache_read, agg.cache_write, agg.output, noon, now_ms);
            usd += u;
            tokens += agg.total_tokens();
        }
    }

    (usd, tokens, exact)
}

// ---------------------------------------------------------------- API views

/// Builds the `/api/usage?days=7|30|all` JSON payload for the Usage tab.
#[allow(clippy::too_many_arguments)]
pub fn build_usage_view(
    h: &UsageHistory,
    samples: &QuotaSamples,
    span_req: &str,
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliScanner,
    live_calls: &[CallRecord],
    rules: &[ModelRule],
    now_ms: i64,
    price_source: &str,
) -> Value {
    let is_all = span_req.eq_ignore_ascii_case("all");
    let today = today_str();
    let today_date = Local::now().date_naive();
    let acct_key = h.cur_acct.as_deref();

    let span_days: usize = if is_all {
        let earliest = h
            .days
            .iter()
            .find(|(_, d)| !d.raw.is_empty() || !d.cli_raw.is_empty())
            .and_then(|(k, _)| chrono::NaiveDate::parse_from_str(k, "%Y-%m-%d").ok());
        match earliest {
            Some(first) => ((today_date - first).num_days().max(0) as usize + 1).clamp(7, HISTORY_KEEP_DAYS),
            None => 7,
        }
    } else if span_req.parse::<usize>().unwrap_or(7) >= 20 {
        30
    } else {
        7
    };

    let mut date_keys: Vec<String> = (0..span_days)
        .rev()
        .filter_map(|offset| {
            today_date.checked_sub_days(chrono::Days::new(offset as u64)).map(|d| d.format("%Y-%m-%d").to_string())
        })
        .collect();
    if date_keys.is_empty() {
        date_keys.push(today.clone());
    }

    // Index early-reset events by calendar day for the active account.
    let acct_resets: Vec<&ResetEvent> =
        acct_key.map(|ak| h.resets.iter().filter(|r| r.acct_key == ak).collect()).unwrap_or_default();
    let mut resets_by_day: HashMap<String, Vec<Value>> = HashMap::new();
    for r in &acct_resets {
        resets_by_day.entry(local_day_str(r.at_ms)).or_default().push(json!(r));
    }

    let mut model_totals: HashMap<String, (f64, u64, u64)> = HashMap::new();
    let mut days_out: Vec<Value> = Vec::with_capacity(date_keys.len());
    let mut total_usd = 0.0;
    let mut total_tokens = 0u64;
    let mut total_calls = 0u64;
    let mut active_days = 0usize;
    let mut has_cli = false;

    for dk in &date_keys {
        let d_start = day_start_ms(dk).unwrap_or(now_ms);
        let d_noon = day_noon_ms(dk).unwrap_or(now_ms);
        let rec = h.days.get(dk);

        let mut by_model: BTreeMap<String, (f64, ModelAgg)> = BTreeMap::new();
        let mut day_usd = 0.0;
        let mut day_in = 0u64;
        let mut day_cr = 0u64;
        let mut day_cw = 0u64;
        let mut day_out = 0u64;
        let mut day_calls = 0u64;
        let mut day_cli_calls = 0u64;

        // On price-transition days (e.g. 2026-12-01), price each call at its exact timestamp so
        // pre-cut and post-cut calls are never blended under a single midday rate.
        let mut priced_exact = false;
        if rec.is_some() && any_price_cut_in(rules, d_start, d_start + 86_400_000) {
            let exact_calls = collect_all_day_calls(app_data, cli_dir, plugin_data, cli, dk, live_calls);
            if !exact_calls.is_empty() {
                priced_exact = true;
                for c in exact_calls {
                    let price_key = format!("{} {}", c.model_raw, c.model_display);
                    let (short_opt, usd) = price_tokens(
                        rules,
                        &price_key,
                        c.input,
                        c.cache_read,
                        c.cache_write,
                        c.output,
                        c.at_ms,
                        now_ms,
                    );
                    let label = short_opt.unwrap_or_else(|| "Other".to_string());
                    let entry = by_model.entry(label).or_default();
                    entry.0 += usd;
                    entry.1.add(c.input, c.cache_read, c.cache_write, c.output);
                    day_usd += usd;
                    day_in += c.input;
                    day_cr += c.cache_read;
                    day_cw += c.cache_write;
                    day_out += c.output;
                    day_calls += 1;
                    if c.source == "cli" {
                        day_cli_calls += 1;
                        has_cli = true;
                    }
                }
            }
        }

        if !priced_exact && let Some(r) = rec {
            for (raw_name, agg) in r.raw.iter().chain(r.cli_raw.iter()) {
                let (short_opt, usd) = price_tokens(
                    rules,
                    raw_name,
                    agg.input,
                    agg.cache_read,
                    agg.cache_write,
                    agg.output,
                    d_noon,
                    now_ms,
                );
                let label = short_opt.unwrap_or_else(|| "Other".to_string());
                let entry = by_model.entry(label).or_default();
                entry.0 += usd;
                entry.1.merge_from(agg);
                day_usd += usd;
                day_in += agg.input;
                day_cr += agg.cache_read;
                day_cw += agg.cache_write;
                day_out += agg.output;
                day_calls += agg.calls;
            }
            day_cli_calls = r.cli_raw.values().map(|a| a.calls).sum();
            if day_cli_calls > 0 {
                has_cli = true;
            }
        }

        for (label, (usd, agg)) in &by_model {
            let e = model_totals.entry(label.clone()).or_default();
            e.0 += *usd;
            e.1 += agg.calls;
            e.2 += agg.total_tokens();
        }

        let day_tokens = day_in + day_cr + day_cw + day_out;
        let hit = if day_cr + day_in > 0 { Some(day_cr as f64 / (day_cr + day_in) as f64) } else { None };
        if day_calls > 0 {
            active_days += 1;
        }
        total_usd += day_usd;
        total_tokens += day_tokens;
        total_calls += day_calls;

        let (mut q_gemini, mut q_claude) = (None::<f64>, None::<f64>);
        if let (Some(r), Some(ak)) = (rec, acct_key)
            && let Some(qa) = r.qa.get(ak)
        {
            for (bid, q) in qa {
                if q.window != "weekly" {
                    continue;
                }
                if bid.starts_with("gemini-") {
                    q_gemini = Some(q.used);
                } else if bid.starts_with("3p-") {
                    q_claude = Some(q.used);
                }
            }
        }

        let models_json: Vec<Value> = {
            let mut v: Vec<_> = by_model.into_iter().collect();
            v.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap_or(std::cmp::Ordering::Equal));
            v.into_iter()
                .map(|(name, (usd, agg))| {
                    let mh = if agg.cache_read + agg.input > 0 {
                        Some(agg.cache_read as f64 / (agg.cache_read + agg.input) as f64)
                    } else {
                        None
                    };
                    json!({
                        "name": name,
                        "usd": r6(usd),
                        "calls": agg.calls,
                        "in": agg.input,
                        "cr": agg.cache_read,
                        "cw": agg.cache_write,
                        "out": agg.output,
                        "tokens": agg.total_tokens(),
                        "hit": mh,
                    })
                })
                .collect()
        };

        let day_resets = resets_by_day.get(dk).cloned().unwrap_or_default();
        days_out.push(json!({
            "date": dk,
            "isToday": dk == &today,
            "bf": rec.is_some_and(|r| r.bf),
            "convs": rec.map(|r| r.convs + r.cli_convs).unwrap_or(0),
            "cliCalls": day_cli_calls,
            "usd": r6(day_usd),
            "calls": day_calls,
            "in": day_in,
            "cr": day_cr,
            "cw": day_cw,
            "out": day_out,
            "tokens": day_tokens,
            "hit": hit,
            "qGemini": q_gemini,
            "qClaude": q_claude,
            "resets": day_resets,
            "models": models_json,
        }));
    }

    let mut legend: Vec<Value> = {
        let mut v: Vec<_> = model_totals.into_iter().collect();
        v.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .map(|(name, (usd, calls, tokens))| json!({ "name": name, "usd": r6(usd), "calls": calls, "tokens": tokens }))
            .collect()
    };
    if legend.len() > 10 {
        legend.truncate(10);
    }

    let today_row = days_out.last().cloned().unwrap_or_else(|| json!({}));
    let avg_usd = if active_days > 0 { total_usd / active_days as f64 } else { 0.0 };

    let account =
        acct_key.and_then(|ak| h.accts.get(ak).map(|a| json!({ "key": ak, "masked": a.masked, "tier": a.tier })));
    let forecasts = acct_key
        .map(|ak| weekly_forecasts(h, samples, ak, app_data, cli_dir, plugin_data, cli, live_calls, rules, now_ms))
        .unwrap_or_default();

    // Today's cache-miss summary across desktop + CLI calls.
    let today_calls = collect_all_day_calls(app_data, cli_dir, plugin_data, cli, &today, live_calls);
    let (_, miss_today) = analyze_calls_with_miss(app_data, today_calls, rules, now_ms);

    json!({
        "span": if is_all { json!("all") } else { json!(span_days) },
        "spanDays": span_days,
        "hasCli": has_cli,
        "today": today_row,
        "days": days_out,
        "legend": legend,
        "totals": {
            "usd": r6(total_usd),
            "tokens": total_tokens,
            "calls": total_calls,
            "activeDays": active_days,
            "avgUsd": r6(avg_usd),
        },
        "missToday": miss_today,
        "resets": acct_resets,
        "account": account,
        "forecasts": forecasts,
        "priceSource": price_source,
        "backfill": h.bf,
    })
}

#[allow(clippy::too_many_arguments)]
fn weekly_forecasts(
    h: &UsageHistory,
    samples: &QuotaSamples,
    acct_key: &str,
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliScanner,
    live_calls: &[CallRecord],
    rules: &[ModelRule],
    now_ms: i64,
) -> Vec<Value> {
    let Some(list) = samples.by_acct.get(acct_key) else { return Vec::new() };
    let mut latest: BTreeMap<&str, &QuotaSamplePoint> = BTreeMap::new();
    for p in list {
        latest.insert(p.bucket_id.as_str(), p);
    }

    let mut out = Vec::new();
    for (bid, last) in latest {
        if last.reset_ms <= now_ms || now_ms - last.at_ms > 6 * 3_600_000 {
            continue;
        }
        let start_ms = last.reset_ms - crate::quota::WEEK_MS;
        let elapsed_ms = (last.at_ms - start_ms).max(1) as f64;
        let left_ms = (last.reset_ms - last.at_ms).max(0) as f64;
        let elapsed_h = r4(elapsed_ms / 3_600_000.0);
        let proj_week = r4(last.used + (last.used / elapsed_ms) * left_ms);
        let full_at_week = (proj_week > 1.0 && last.used > 0.0)
            .then(|| last.at_ms + ((1.0 - last.used) / (last.used / elapsed_ms)).round() as i64);

        let mut ref_pt: Option<&QuotaSamplePoint> = None;
        for p in list {
            if p.bucket_id != bid || !same_period(p.reset_ms, last.reset_ms) || last.at_ms - p.at_ms < 6 * 3_600_000 {
                continue;
            }
            let diff = ((last.at_ms - p.at_ms) - 86_400_000).abs();
            let better = ref_pt.is_none_or(|r| diff < ((last.at_ms - r.at_ms) - 86_400_000).abs());
            if better {
                ref_pt = Some(p);
            }
        }
        let (span_h, proj_24h, full_at_24h) = if let Some(r) = ref_pt {
            let dt = (last.at_ms - r.at_ms) as f64;
            let rate = (last.used - r.used).max(0.0) / dt;
            let p24 = r4(last.used + rate * left_ms);
            let f24 = (rate > 0.0 && p24 > 1.0).then(|| last.at_ms + ((1.0 - last.used) / rate).round() as i64);
            (Some(r4(dt / 3_600_000.0)), Some(p24), f24)
        } else {
            (None, None, None)
        };

        // Check if this active cycle began after an early reset.
        let early_reset = h
            .resets
            .iter()
            .rev()
            .find(|r| r.acct_key == acct_key && r.bucket_id == bid && (r.at_ms - start_ms).abs() < 12 * 3_600_000)
            .cloned();

        let (cycle_usd, cycle_tokens, cap_exact) =
            cycle_spend(h, app_data, cli_dir, plugin_data, cli, live_calls, bid, start_ms, now_ms, rules, now_ms);
        let cap_usd = (last.used >= 0.005 && cycle_usd > 0.0).then(|| (cycle_usd / last.used).round());
        let cap_tokens = (last.used >= 0.005 && cycle_tokens > 0).then(|| (cycle_tokens as f64 / last.used) as u64);

        // Compute recent past ended cycles (up to 4) for this bucket.
        let mut ended: Vec<&CycleRecord> =
            h.cycles.iter().filter(|c| c.acct_key == acct_key && c.bucket_id == bid).collect();
        ended.sort_by_key(|c| std::cmp::Reverse(c.end_ms));
        let past_cycles: Vec<Value> = ended
            .into_iter()
            .take(4)
            .map(|c| {
                let (p_usd, p_tok, _) = cycle_spend(
                    h,
                    app_data,
                    cli_dir,
                    plugin_data,
                    cli,
                    live_calls,
                    bid,
                    c.start_ms,
                    c.end_ms,
                    rules,
                    now_ms,
                );
                let p_cap_usd = (c.used >= 0.005 && p_usd > 0.0).then(|| (p_usd / c.used).round());
                let p_cap_tok = (c.used >= 0.005 && p_tok > 0).then(|| (p_tok as f64 / c.used) as u64);
                json!({
                    "startMs": c.start_ms,
                    "endMs": c.end_ms,
                    "used": c.used,
                    "early": c.early,
                    "usd": r6(p_usd),
                    "tokens": p_tok,
                    "capUsd": p_cap_usd,
                    "capTokens": p_cap_tok,
                })
            })
            .collect();

        let label = if bid.starts_with("3p-") {
            "Claude / 3P"
        } else if bid.starts_with("gemini-") {
            "Gemini"
        } else {
            bid
        };

        out.push(json!({
            "id": bid,
            "label": label,
            "used": last.used,
            "startMs": start_ms,
            "resetMs": last.reset_ms,
            "elapsedH": elapsed_h,
            "projWeek": proj_week,
            "fullAtWeek": full_at_week,
            "spanH": span_h,
            "proj24h": proj_24h,
            "fullAt24h": full_at_24h,
            "cycleUsd": r6(cycle_usd),
            "cycleTokens": cycle_tokens,
            "capUsd": cap_usd,
            "capTokens": cap_tokens,
            "capExact": cap_exact,
            "earlyReset": early_reset,
            "pastCycles": past_cycles,
        }));
    }
    out
}

/// Returns individual model calls + per-conversation rollup + cache-miss analysis for `date` (`YYYY-MM-DD`).
/// `limit == 0` returns all calls; otherwise caps the `calls` array to `limit` while totals/byConv cover 100%.
#[allow(clippy::too_many_arguments)]
pub fn build_day_calls_view(
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliScanner,
    date: &str,
    live_calls: &[CallRecord],
    rules: &[ModelRule],
    now_ms: i64,
    costly_threshold: f64,
    limit: usize,
) -> Value {
    let raw_calls = collect_all_day_calls(app_data, cli_dir, plugin_data, cli, date, live_calls);
    let cli_titles = cli.titles(cli_dir).clone();
    let (analyzed, miss_summary) = analyze_calls_with_miss(app_data, raw_calls, rules, now_ms);

    let mut titles: HashMap<(&'static str, String), String> = HashMap::new();
    let mut tot_usd = 0.0;
    let mut tot_in = 0u64;
    let mut tot_cr = 0u64;
    let mut tot_cw = 0u64;
    let mut tot_out = 0u64;
    let mut cli_count = 0usize;
    let mut desktop_count = 0usize;
    let total_calls = analyzed.len();

    #[derive(Default)]
    struct ConvAgg {
        title: String,
        source: &'static str,
        calls: usize,
        usd: f64,
        inp: u64,
        cr: u64,
        cw: u64,
        out: u64,
        miss_n: usize,
        miss_usd: f64,
        first_at: i64,
        last_at: i64,
        models: BTreeMap<String, f64>,
    }
    let mut conv_map: HashMap<(&'static str, String), ConvAgg> = HashMap::new();
    let mut model_usd: HashMap<String, f64> = HashMap::new();

    let cap = if limit == 0 { total_calls } else { total_calls.min(limit) };
    let mut rows = Vec::with_capacity(cap);

    for (idx, (c, short_opt, usd, miss_opt)) in analyzed.into_iter().enumerate() {
        tot_usd += usd;
        tot_in += c.input;
        tot_cr += c.cache_read;
        tot_cw += c.cache_write;
        tot_out += c.output;
        if c.source == "cli" {
            cli_count += 1;
        } else {
            desktop_count += 1;
        }

        let title = titles
            .entry((c.source, c.conv_id.clone()))
            .or_insert_with(|| {
                if c.source == "cli" {
                    cli_titles.get(&c.conv_id).cloned().unwrap_or_else(|| c.conv_id.chars().take(8).collect())
                } else {
                    read_annotation(app_data, &c.conv_id)
                        .map(|a| a.title)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| c.conv_id.chars().take(8).collect())
                }
            })
            .clone();

        let model_label = short_opt
            .or_else(|| (!c.model_display.is_empty()).then_some(c.model_display.clone()))
            .unwrap_or_else(|| c.model_raw.clone());
        *model_usd.entry(model_label.clone()).or_default() += usd;

        // Per-conversation rollup across ALL calls of the day.
        let ce = conv_map.entry((c.source, c.conv_id.clone())).or_insert_with(|| ConvAgg {
            title: title.clone(),
            source: c.source,
            first_at: c.at_ms,
            last_at: c.at_ms,
            ..Default::default()
        });
        ce.calls += 1;
        ce.usd += usd;
        ce.inp += c.input;
        ce.cr += c.cache_read;
        ce.cw += c.cache_write;
        ce.out += c.output;
        ce.first_at = ce.first_at.min(c.at_ms);
        ce.last_at = ce.last_at.max(c.at_ms);
        *ce.models.entry(model_label.clone()).or_default() += usd;
        if let Some(ref m) = miss_opt {
            ce.miss_n += 1;
            ce.miss_usd += m.usd;
        }

        if limit > 0 && idx >= limit {
            continue;
        }

        let hit =
            if c.cache_read + c.input > 0 { Some(c.cache_read as f64 / (c.cache_read + c.input) as f64) } else { None };
        let tps = c.stream_ms.filter(|&ms| ms > 50.0 && c.output > 0).map(|ms| c.output as f64 / (ms / 1000.0));
        let costly = costly_threshold > 0.0 && usd >= costly_threshold;

        rows.push(json!({
            "at": c.at_ms,
            "convId": c.conv_id,
            "convTitle": title,
            "source": c.source,
            "model": model_label,
            "usd": r6(usd),
            "costly": costly,
            "in": c.input,
            "cr": c.cache_read,
            "cw": c.cache_write,
            "out": c.output,
            "hit": hit,
            "ttftMs": c.ttft_ms,
            "streamMs": c.stream_ms,
            "tps": tps,
            "miss": miss_opt,
        }));
    }

    let mut by_conv: Vec<Value> = {
        let mut v: Vec<_> = conv_map.into_iter().collect();
        v.sort_by(|a, b| b.1.usd.partial_cmp(&a.1.usd).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .map(|((_, cid), ca)| {
                let hit = if ca.cr + ca.inp > 0 { Some(ca.cr as f64 / (ca.cr + ca.inp) as f64) } else { None };
                let mut ms: Vec<_> = ca.models.into_iter().collect();
                ms.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let mnames: Vec<String> = ms.into_iter().map(|(k, _)| k).collect();
                json!({
                    "convId": cid,
                    "convTitle": ca.title,
                    "source": ca.source,
                    "calls": ca.calls,
                    "usd": r6(ca.usd),
                    "share": if tot_usd > 0.0 { r4(ca.usd / tot_usd) } else { 0.0 },
                    "in": ca.inp,
                    "cr": ca.cr,
                    "cw": ca.cw,
                    "out": ca.out,
                    "tokens": ca.inp + ca.cr + ca.cw + ca.out,
                    "hit": hit,
                    "missCount": ca.miss_n,
                    "missUsd": r6(ca.miss_usd),
                    "firstAt": ca.first_at,
                    "lastAt": ca.last_at,
                    "models": mnames,
                })
            })
            .collect()
    };
    if by_conv.len() > 200 {
        by_conv.truncate(200);
    }

    let models: Vec<Value> = {
        let mut v: Vec<_> = model_usd.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter().map(|(name, usd)| json!({ "name": name, "usd": r6(usd) })).collect()
    };

    let tot_hit = if tot_cr + tot_in > 0 { Some(tot_cr as f64 / (tot_cr + tot_in) as f64) } else { None };
    json!({
        "date": date,
        "totalCalls": total_calls,
        "desktopCalls": desktop_count,
        "cliCalls": cli_count,
        "shown": rows.len(),
        "truncated": limit > 0 && total_calls > limit,
        "costlyThreshold": costly_threshold,
        "models": models,
        "missSummary": miss_summary,
        "totals": {
            "usd": r6(tot_usd),
            "in": tot_in,
            "cr": tot_cr,
            "cw": tot_cw,
            "out": tot_out,
            "tokens": tot_in + tot_cr + tot_cw + tot_out,
            "hit": tot_hit,
        },
        "byConv": by_conv,
        "calls": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_emails_and_hashes_deterministically() {
        assert_eq!(mask_email("alice.smith@gmail.com"), "al***th@gmail.com");
        assert_eq!(mask_email("bob@example.com"), "b***@example.com");
        assert_eq!(fnv1a_hex("test@example.com").len(), 8);
        assert_eq!(fnv1a_hex("test@example.com"), fnv1a_hex("test@example.com"));
    }

    #[test]
    fn corrupt_history_is_backed_up_not_silently_lost() {
        let dir = std::env::temp_dir().join(format!("agy-hist-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let hist_path = dir.join("usage-history.json");
        fs::write(&hist_path, "{broken json").unwrap();

        let h = load_history(&dir);
        assert!(h.writable);
        let has_bad = fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains(".bad-"));
        assert!(has_bad, "corrupt JSON should be renamed to .bad-*");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_conv_db_parses_protobuf_and_handles_cli_dedup() {
        let dir = std::env::temp_dir().join(format!("agy-pb-db-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cid = "00000000-0000-4000-8000-000000000001";
        let db_path = dir.join(format!("{cid}.db"));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE steps (idx INTEGER, metadata BLOB)", []).unwrap();
        conn.execute("CREATE TABLE gen_metadata (data BLOB)", []).unwrap();

        fn varint(mut n: u64) -> Vec<u8> {
            let mut b = Vec::new();
            loop {
                let mut c = (n & 0x7f) as u8;
                n >>= 7;
                if n != 0 {
                    c |= 0x80;
                }
                b.push(c);
                if n == 0 {
                    break;
                }
            }
            b
        }
        fn fv(f: u64, n: u64) -> Vec<u8> {
            [varint(f << 3), varint(n)].concat()
        }
        fn fb(f: u64, data: &[u8]) -> Vec<u8> {
            [varint((f << 3) | 2), varint(data.len() as u64), data.to_vec()].concat()
        }

        let t1 = 1_795_500_000u64;
        let meta1 = [fb(1, &fv(1, t1)), fb(12, b"exec-a")].concat();
        conn.execute("INSERT INTO steps VALUES (2, ?1)", [&meta1]).unwrap();

        let usage = [fv(2, 1000), fv(3, 50), fv(5, 9000), fv(9, 40), fv(10, 30), fb(11, b"req-1")].concat();
        let dur1 = [fv(1, 1), fv(2, 250_000_000)].concat();
        let dur2 = [fv(1, 8), fv(2, 500_000_000)].concat();
        let cm =
            [fb(4, &usage), fb(11, &dur1), fb(12, &dur2), fb(19, b"gemini-4-neon"), fb(21, b"Gemini 4 Neon")].concat();
        let gen_row = [fb(1, &cm), fb(2, &varint(2)), fb(4, b"exec-a")].concat();
        conn.execute("INSERT INTO gen_metadata VALUES (?1)", [&gen_row]).unwrap();
        drop(conn);

        let calls = read_conv_db(&db_path, cid, "cli").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].at_ms, (t1 as i64) * 1000);
        assert_eq!(calls[0].step, Some(2));
        assert_eq!(calls[0].output, 70);
        assert_eq!(calls[0].dedup_key, "req-1");
        assert_eq!(calls[0].source, "cli");
        assert!((calls[0].ttft_ms.unwrap() - 1250.0).abs() < 1.0);
        assert!((calls[0].stream_ms.unwrap() - 8500.0).abs() < 1.0);
        let _ = fs::remove_dir_all(&dir);
    }
}
