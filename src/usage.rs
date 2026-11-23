//! Persistent daily usage history, SQLite conversation backfill, and weekly quota forecasting.
//!
//! Stores only numeric aggregates and masked account metadata in `plugin_data/`:
//! - `usage-history.json`: up to 90 days of per-model call/token aggregates and per-account
//!   weekly quota net deltas (cycle net-change algorithm so intra-cycle dips don't inflate totals).
//! - `quota-samples.json`: up to 8 days of periodic weekly quota snapshots used to forecast
//!   end-of-week utilization (`est X% at reset`) and full-cycle value (`cap ≈ $X`).
//! - Read-only SQLite backfill of `<app_data>/conversations/*.db` parses protobuf rows in
//!   `steps` and `gen_metadata` so historical days and per-day call breakdowns work immediately.

use crate::config::{ModelRule, write_atomic};
use crate::transcript::read_annotation;
use chrono::{Local, TimeZone};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

const HISTORY_KEEP_DAYS: usize = 90;
const SAMPLES_KEEP_MS: i64 = 8 * 86_400_000;
const BACKFILL_V: u32 = 2;
const DAY_CALLS_LIMIT: usize = 600;

fn r4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

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

pub fn same_period(r1: i64, r2: i64) -> bool {
    (r1 - r2).abs() < 3_600_000
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
pub struct DayQuotaRec {
    pub name: String,
    pub window: String,
    /// Used fraction at the start of the current period segment today.
    pub start: f64,
    /// Net delta settled in earlier period segments today (if quota reset mid-day).
    pub carried: f64,
    /// Last observed used fraction today.
    pub last: f64,
    /// Max observed used fraction today.
    pub max: f64,
    /// Net used fraction consumed today: `carried + max(0, last - start)`.
    pub used: f64,
    pub reset_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DayRecord {
    /// True if populated by historical SQLite backfill (before live tracking began).
    #[serde(default)]
    pub bf: bool,
    #[serde(default)]
    pub convs: usize,
    /// Raw model key (e.g. `gemini-4-neon`) -> token & call totals.
    #[serde(default)]
    pub raw: HashMap<String, ModelAgg>,
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
}

#[derive(Clone, Debug)]
pub struct QuotaBucketSample {
    pub id: String,
    pub name: String,
    pub window: String,
    pub used: f64,
    pub reset_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuotaSamplePoint {
    pub at_ms: i64,
    pub bucket_id: String,
    pub used: f64,
    pub reset_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuotaSamples {
    pub v: u32,
    #[serde(default)]
    pub by_acct: HashMap<String, Vec<QuotaSamplePoint>>,
}

pub fn load_history(plugin_data: &Path) -> UsageHistory {
    let path = plugin_data.join("usage-history.json");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(mut h) = serde_json::from_str::<UsageHistory>(&text)
    {
        if h.live_from.is_empty() {
            h.live_from = today_str();
        }
        return h;
    }
    UsageHistory { v: 1, live_from: today_str(), ..Default::default() }
}

pub fn save_history(plugin_data: &Path, h: &UsageHistory) {
    let path = plugin_data.join("usage-history.json");
    if let Ok(text) = serde_json::to_string(h) {
        let _ = write_atomic(&path, &text);
    }
}

pub fn load_samples(plugin_data: &Path) -> QuotaSamples {
    let path = plugin_data.join("quota-samples.json");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(s) = serde_json::from_str::<QuotaSamples>(&text)
    {
        return s;
    }
    QuotaSamples { v: 1, by_acct: HashMap::new() }
}

pub fn save_samples(plugin_data: &Path, s: &QuotaSamples) {
    let path = plugin_data.join("quota-samples.json");
    if let Ok(text) = serde_json::to_string(s) {
        let _ = write_atomic(&path, &text);
    }
}

fn prev_quota_of<'a>(h: &'a UsageHistory, today: &str, acct_key: &str, bucket_id: &str) -> Option<&'a DayQuotaRec> {
    h.days
        .range(..today.to_string())
        .rev()
        .take(14)
        .find_map(|(_, d)| d.qa.get(acct_key).and_then(|m| m.get(bucket_id)))
}

fn note_quota_map(
    qs: &mut HashMap<String, DayQuotaRec>,
    buckets: &[QuotaBucketSample],
    prev_map: &HashMap<String, DayQuotaRec>,
) {
    for b in buckets {
        match qs.get_mut(&b.id) {
            None => {
                let start = match prev_map.get(&b.id) {
                    Some(p) if same_period(p.reset_ms, b.reset_ms) => p.last,
                    Some(_) => 0.0,
                    None => b.used,
                };
                let last = r4(b.used);
                let used = r4((last - start).max(0.0));
                qs.insert(
                    b.id.clone(),
                    DayQuotaRec {
                        name: b.name.clone(),
                        window: b.window.clone(),
                        start: r4(start),
                        carried: 0.0,
                        last,
                        max: last,
                        used,
                        reset_ms: b.reset_ms,
                    },
                );
            }
            Some(q) => {
                if !same_period(q.reset_ms, b.reset_ms) {
                    q.carried = r4(q.carried + (q.last - q.start).max(0.0));
                    q.start = 0.0;
                }
                q.last = r4(b.used);
                q.used = r4(q.carried + (q.last - q.start).max(0.0));
                q.max = r4(q.max.max(b.used));
                q.reset_ms = b.reset_ms;
                if !b.name.is_empty() {
                    q.name = b.name.clone();
                }
                if !b.window.is_empty() {
                    q.window = b.window.clone();
                }
            }
        }
    }
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

    // Gather previous-day quota lookup before borrowing `h.days` mutably.
    let prev_map: HashMap<String, DayQuotaRec> = if let Some(a) = acct {
        buckets
            .iter()
            .filter_map(|b| prev_quota_of(h, today, &a.key, &b.id).map(|p| (b.id.clone(), p.clone())))
            .collect()
    } else {
        HashMap::new()
    };

    if h.live_from.is_empty() {
        h.live_from = today.to_string();
        hist_dirty = true;
    }

    // Drop old `by_conv` maps on past days to keep file compact.
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
        let day_mut = h.days.entry(today.to_string()).or_default();
        let qa = day_mut.qa.entry(a.key.clone()).or_default();
        let before_qa = serde_json::to_string(qa).ok();
        note_quota_map(qa, buckets, &prev_map);
        let after_qa = serde_json::to_string(qa).ok();
        if before_qa != after_qa {
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

    // Trim history to last HISTORY_KEEP_DAYS days.
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

// ---------------------------------------------------------------- SQLite backfill & day-calls query

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub conv_id: String,
    pub at_ms: i64,
    pub model_raw: String,
    pub model_display: String,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub ttft_ms: Option<f64>,
    pub stream_ms: Option<f64>,
    pub dedup_key: String,
}

fn read_conv_db(path: &Path, conv_id: &str) -> Result<Vec<CallRecord>, String> {
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
        let dedup_key = pb_bytes(u, 11)
            .and_then(|b| std::str::from_utf8(b).ok())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{exec}:{inp}:{cr}:{out_tok}"));

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
            model_raw: if model_raw.is_empty() { model_display.clone() } else { model_raw },
            model_display,
            input: inp,
            cache_read: cr,
            cache_write: cw,
            output: out_tok,
            ttft_ms,
            stream_ms,
            dedup_key,
        });
    }
    Ok(out)
}

fn is_uuid_db(name: &str) -> Option<&str> {
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
        match read_conv_db(&entry.path(), cid) {
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
        let existing_calls: u64 = entry.raw.values().map(|a| a.calls).sum();
        let db_calls: u64 = raw.values().map(|a| a.calls).sum();
        let is_pre_live = day < h.live_from;
        if is_pre_live || entry.bf || db_calls as f64 > existing_calls as f64 * 1.02 + 1.0 {
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

// ---------------------------------------------------------------- API views

/// Builds the `/api/usage?days=7|30` JSON payload for the Usage tab.
pub fn build_usage_view(
    h: &UsageHistory,
    samples: &QuotaSamples,
    days_req: usize,
    rules: &[ModelRule],
    now_ms: i64,
    price_source: &str,
) -> Value {
    let span = if days_req >= 20 { 30 } else { 7 };
    let today = today_str();
    let today_date = Local::now().date_naive();
    let acct_key = h.cur_acct.as_deref();

    // Generate contiguous date keys for the requested range (oldest → today).
    let mut date_keys: Vec<String> = (0..span)
        .rev()
        .filter_map(|offset| {
            today_date.checked_sub_days(chrono::Days::new(offset as u64)).map(|d| d.format("%Y-%m-%d").to_string())
        })
        .collect();
    if date_keys.is_empty() {
        date_keys.push(today.clone());
    }

    let mut model_totals: HashMap<String, (f64, u64, u64)> = HashMap::new(); // short_name -> (usd, calls, tokens)
    let mut days_out: Vec<Value> = Vec::with_capacity(date_keys.len());
    let mut total_usd = 0.0;
    let mut total_tokens = 0u64;
    let mut total_calls = 0u64;
    let mut active_days = 0usize;

    for dk in &date_keys {
        // Use noon local time of that calendar day for tiered pricing of daily aggregates.
        let day_noon_ms = chrono::NaiveDate::parse_from_str(dk, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(12, 0, 0))
            .and_then(|dt| dt.and_local_timezone(Local).earliest())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(now_ms);

        let rec = h.days.get(dk);
        let mut by_model: BTreeMap<String, (f64, ModelAgg)> = BTreeMap::new();
        let mut day_usd = 0.0;
        let mut day_in = 0u64;
        let mut day_cr = 0u64;
        let mut day_cw = 0u64;
        let mut day_out = 0u64;
        let mut day_calls = 0u64;

        if let Some(r) = rec {
            for (raw_name, agg) in &r.raw {
                let (short_opt, usd) = price_tokens(
                    rules,
                    raw_name,
                    agg.input,
                    agg.cache_read,
                    agg.cache_write,
                    agg.output,
                    day_noon_ms,
                    now_ms,
                );
                let label = short_opt.unwrap_or_else(|| "Other".to_string());
                let entry = by_model.entry(label.clone()).or_default();
                entry.0 += usd;
                entry.1.merge_from(agg);
                day_usd += usd;
                day_in += agg.input;
                day_cr += agg.cache_read;
                day_cw += agg.cache_write;
                day_out += agg.output;
                day_calls += agg.calls;
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

        // Per-account weekly quota deltas (Gemini & Claude/3P).
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

        days_out.push(json!({
            "date": dk,
            "isToday": dk == &today,
            "bf": rec.is_some_and(|r| r.bf),
            "convs": rec.map(|r| r.convs).unwrap_or(0),
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
    let forecasts = acct_key.map(|ak| weekly_forecasts(h, samples, ak, rules, now_ms)).unwrap_or_default();

    json!({
        "span": span,
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
        "account": account,
        "forecasts": forecasts,
        "priceSource": price_source,
        "backfill": h.bf,
    })
}

fn weekly_forecasts(
    h: &UsageHistory,
    samples: &QuotaSamples,
    acct_key: &str,
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
        let start_ms = last.reset_ms - 7 * 86_400_000;
        let elapsed_ms = (last.at_ms - start_ms).max(1) as f64;
        let left_ms = (last.reset_ms - last.at_ms).max(0) as f64;
        let elapsed_h = r4(elapsed_ms / 3_600_000.0);
        let proj_week = r4(last.used + (last.used / elapsed_ms) * left_ms);
        let full_at_week = (proj_week > 1.0 && last.used > 0.0)
            .then(|| last.at_ms + ((1.0 - last.used) / (last.used / elapsed_ms)).round() as i64);

        // Closest sample ~24h ago (at least 6h ago) in the same weekly period.
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

        // Estimate full-week USD and token cap from this machine's usage during the current 7-day cycle.
        let start_day = local_day_str(start_ms);
        let mut cycle_usd = 0.0;
        let mut cycle_tokens = 0u64;
        for (dk, drec) in h.days.range(start_day..) {
            let day_noon_ms = chrono::NaiveDate::parse_from_str(dk, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(12, 0, 0))
                .and_then(|dt| dt.and_local_timezone(Local).earliest())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(now_ms);
            for (model, agg) in &drec.raw {
                if !model_belongs_to_group(model, bid) {
                    continue;
                }
                let (_, usd) = price_tokens(
                    rules,
                    model,
                    agg.input,
                    agg.cache_read,
                    agg.cache_write,
                    agg.output,
                    day_noon_ms,
                    now_ms,
                );
                cycle_usd += usd;
                cycle_tokens += agg.total_tokens();
            }
        }
        let cap_usd = (last.used >= 0.005 && cycle_usd > 0.0).then(|| (cycle_usd / last.used).round());
        let cap_tokens = (last.used >= 0.005 && cycle_tokens > 0).then(|| (cycle_tokens as f64 / last.used) as u64);
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
        }));
    }
    out
}

/// Returns individual model calls for `date` (`YYYY-MM-DD`), combining live in-memory calls and
/// read-only SQLite conversation databases (newest first, up to 600 rows, totals over all calls).
pub fn build_day_calls_view(
    app_data: &Path,
    date: &str,
    live_calls: &[CallRecord],
    rules: &[ModelRule],
    now_ms: i64,
) -> Value {
    let mut merged: Vec<CallRecord> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for c in live_calls {
        if local_day_str(c.at_ms) != date {
            continue;
        }
        if seen.insert(c.dedup_key.clone()) {
            merged.push(c.clone());
        }
    }

    // Also scan conversation DBs modified on or after this calendar day (for past days or restarted convs).
    let day_start_ms = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|dt| dt.and_local_timezone(Local).earliest())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    if let Ok(rd) = fs::read_dir(app_data.join("conversations")) {
        for entry in rd.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            let Some(cid) = is_uuid_db(&fname) else { continue };
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if day_start_ms > 0 && mtime > 0 && mtime < day_start_ms - 60_000 {
                continue;
            }
            if let Ok(recs) = read_conv_db(&entry.path(), cid) {
                for r in recs {
                    if local_day_str(r.at_ms) != date {
                        continue;
                    }
                    // Also skip if a live call already covered this exact conversation + timestamp + tokens.
                    let soft_key = format!("{}:{}:{}:{}", r.conv_id, r.at_ms / 1000, r.input, r.output);
                    if seen.insert(r.dedup_key.clone()) && seen.insert(soft_key) {
                        merged.push(r);
                    }
                }
            }
        }
    }

    merged.sort_by_key(|b| std::cmp::Reverse(b.at_ms));

    let mut titles: HashMap<String, String> = HashMap::new();
    let mut tot_usd = 0.0;
    let mut tot_in = 0u64;
    let mut tot_cr = 0u64;
    let mut tot_cw = 0u64;
    let mut tot_out = 0u64;
    let total_calls = merged.len();

    let mut rows = Vec::with_capacity(total_calls.min(DAY_CALLS_LIMIT));
    for (idx, c) in merged.into_iter().enumerate() {
        let price_key = format!("{} {}", c.model_raw, c.model_display);
        let (short_opt, usd) =
            price_tokens(rules, &price_key, c.input, c.cache_read, c.cache_write, c.output, c.at_ms, now_ms);
        tot_usd += usd;
        tot_in += c.input;
        tot_cr += c.cache_read;
        tot_cw += c.cache_write;
        tot_out += c.output;

        if idx >= DAY_CALLS_LIMIT {
            continue;
        }
        let title = titles
            .entry(c.conv_id.clone())
            .or_insert_with(|| {
                read_annotation(app_data, &c.conv_id)
                    .map(|a| a.title)
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| c.conv_id.chars().take(8).collect())
            })
            .clone();
        let hit =
            if c.cache_read + c.input > 0 { Some(c.cache_read as f64 / (c.cache_read + c.input) as f64) } else { None };
        let tps = c.stream_ms.filter(|&ms| ms > 50.0 && c.output > 0).map(|ms| c.output as f64 / (ms / 1000.0));
        let model_label =
            short_opt.or_else(|| (!c.model_display.is_empty()).then_some(c.model_display)).unwrap_or(c.model_raw);

        rows.push(json!({
            "at": c.at_ms,
            "convId": c.conv_id,
            "convTitle": title,
            "model": model_label,
            "usd": r6(usd),
            "in": c.input,
            "cr": c.cache_read,
            "cw": c.cache_write,
            "out": c.output,
            "hit": hit,
            "ttftMs": c.ttft_ms,
            "streamMs": c.stream_ms,
            "tps": tps,
        }));
    }

    let tot_hit = if tot_cr + tot_in > 0 { Some(tot_cr as f64 / (tot_cr + tot_in) as f64) } else { None };
    json!({
        "date": date,
        "totalCalls": total_calls,
        "shown": rows.len(),
        "truncated": total_calls > DAY_CALLS_LIMIT,
        "totals": {
            "usd": r6(tot_usd),
            "in": tot_in,
            "cr": tot_cr,
            "cw": tot_cw,
            "out": tot_out,
            "tokens": tot_in + tot_cr + tot_cw + tot_out,
            "hit": tot_hit,
        },
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
    fn quota_net_change_ignores_intra_cycle_oscillation_and_handles_rollover() {
        let mut qs = HashMap::new();
        let prev = HashMap::new();
        let r1 = 1_800_000_000_000i64;
        // Start at 1.0%, rise to 3.9%, dip back to 1.6% → net delta is 0.6%, NOT 2.9%+.
        note_quota_map(
            &mut qs,
            &[QuotaBucketSample {
                id: "gemini-weekly".into(),
                name: "Gemini".into(),
                window: "weekly".into(),
                used: 0.010,
                reset_ms: r1,
            }],
            &prev,
        );
        note_quota_map(
            &mut qs,
            &[QuotaBucketSample {
                id: "gemini-weekly".into(),
                name: "Gemini".into(),
                window: "weekly".into(),
                used: 0.039,
                reset_ms: r1,
            }],
            &prev,
        );
        note_quota_map(
            &mut qs,
            &[QuotaBucketSample {
                id: "gemini-weekly".into(),
                name: "Gemini".into(),
                window: "weekly".into(),
                used: 0.016,
                reset_ms: r1,
            }],
            &prev,
        );
        assert!((qs["gemini-weekly"].used - 0.006).abs() < 1e-4);

        // Period rolls over (+7d) and uses 2.0% in the new cycle → 0.6% + 2.0% = 2.6%.
        let r2 = r1 + 7 * 86_400_000;
        note_quota_map(
            &mut qs,
            &[QuotaBucketSample {
                id: "gemini-weekly".into(),
                name: "Gemini".into(),
                window: "weekly".into(),
                used: 0.020,
                reset_ms: r2,
            }],
            &prev,
        );
        assert!((qs["gemini-weekly"].used - 0.026).abs() < 1e-4);
    }
}
