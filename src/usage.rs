//! Persistent daily usage history (up to 400 days), SQLite conversation backfill (desktop + agy CLI),
//! segmented weekly quota accounting, exact cycle-start cap forecasting, and cache-miss day breakdowns.
//!
//! Stores only numeric aggregates and masked account metadata in `plugin_data/`:
//! - `usage-history.json`: up to 400 days of per-model call/token aggregates (desktop & CLI),
//!   per-account segmented weekly quota deltas, early-reset events, and ended-cycle records.
//! - `quota-samples.json`: up to 8 days of periodic weekly quota snapshots for forecasting & replay.
//! - `cli-index.json`: day index of `~/.gemini/antigravity-cli/conversations/*.db` files.

use crate::call::{CLI, Call, DESKTOP, DedupSet, ModelAgg, ModelId, Tokens};
use crate::cli_usage::{CliBackfillMeta, CliSnapshot, read_cli_titles};
use crate::config::{ReadFile, read_json_keep, write_atomic};
use crate::convdb::{DbSig, is_uuid_db, read_calls, signature};
use crate::day::LocalDay;
use crate::miss::{MissInfo, MissSummary, miss_scan};
use crate::pricing::{DayCost, Pricer, day_sum};
use crate::quota::QuotaKind;
pub use crate::quota::{
    CycleRecord, DayQuotaRec, QuotaBucketSample, QuotaSamplePoint, QuotaSamples, ResetEvent, note_cycle_end,
    note_quota, note_reset_event, r4, replay_from_samples, same_period,
};
use crate::transcript::{read_annotation, scan_step_markers};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

pub const HISTORY_KEEP_DAYS: usize = 400;
const SAMPLES_KEEP_MS: i64 = 8 * 86_400_000;
const BACKFILL_V: u32 = 3;
pub const DEFAULT_DAY_CALLS_LIMIT: usize = 600;

fn r6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
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

pub fn quota_group_id(display_name: &str, window: &str) -> String {
    QuotaKind::of_group(display_name).bucket_id(window)
}

// ---------------------------------------------------------------- on-disk history

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
                h.live_from = LocalDay::today().key();
            }
            h.writable = true;
            h
        }
        ReadFile::Missing | ReadFile::Corrupt { kept: true } => {
            UsageHistory { v: 2, live_from: LocalDay::today().key(), writable: true, ..Default::default() }
        }
        ReadFile::Unreadable | ReadFile::Corrupt { kept: false } => {
            UsageHistory { v: 2, live_from: LocalDay::today().key(), writable: false, ..Default::default() }
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
            |ms| LocalDay::of(ms).key(),
            |d| LocalDay::parse(d).map_or(0, |d| d.start_ms()),
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

/// Drops quota samples older than `cut` for every account (not only the active one), then drops
/// accounts left without samples, so `quota-samples.json` stays bounded after account switches.
fn trim_samples(samples: &mut QuotaSamples, cut: i64) {
    samples.by_acct.retain(|_, list| {
        list.retain(|p| p.at_ms >= cut);
        !list.is_empty()
    });
}

fn prev_quota_of(h: &UsageHistory, today: &str, acct_key: &str, bucket_id: &str) -> Option<DayQuotaRec> {
    h.days
        .range(..today.to_string())
        .rev()
        .take(14)
        .find_map(|(_, d)| d.qa.get(acct_key).and_then(|m| m.get(bucket_id)).cloned())
}

/// Calls summed over a map of per-model aggregates.
pub(crate) fn conv_calls(m: &HashMap<String, ModelAgg>) -> u64 {
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
        let (prev_calls, next_calls) = (conv_calls(&day.raw), conv_calls(&rebuilt));
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
                trim_samples(samples, quota_at_ms - SAMPLES_KEEP_MS);
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

// ---------------------------------------------------------------- SQLite backfill & call collection

/// Calls of one conversation database that count toward usage: with tokens and a time.
pub(crate) fn read_usage_calls(path: &Path, conv_id: &str, source: &'static str) -> Result<Vec<Call>, String> {
    let mut calls = read_calls(path, conv_id, source)?;
    calls.retain(|c| c.has_usage() && c.at_ms > 0);
    Ok(calls)
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
    let today = LocalDay::today();
    let mut seen = DedupSet::default();
    let mut acc: BTreeMap<String, (HashMap<String, ModelAgg>, HashSet<String>)> = BTreeMap::new();
    let (mut dbs, mut calls, mut errors) = (0usize, 0usize, 0usize);

    for entry in rd.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        let Some(cid) = is_uuid_db(&fname) else { continue };
        match read_usage_calls(&entry.path(), cid, DESKTOP) {
            Ok(recs) => {
                dbs += 1;
                for r in recs {
                    if !seen.insert(&r) {
                        continue;
                    }
                    let day = LocalDay::of(r.at_ms);
                    if day >= today {
                        continue;
                    }
                    let (raw, convs) = acc.entry(day.key()).or_default();
                    raw.entry(r.model.bucket_key()).or_default().add_tokens(&r.tokens);
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
        let (existing_calls, db_calls) = (conv_calls(&entry.raw), conv_calls(&raw));
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

/// Parsed conversation DBs keyed by path, reused while the DB's [`DbSig`] (mtime + length of
/// `.db` and `.db-wal`) is unchanged. Any write to the DB or its WAL changes the signature, so a
/// hit holds what reading the DB would return now.
///
/// Memory bound: each DB weighs its call-record count + 1, and the least recently used DBs are
/// evicted once the total would exceed `MAX`; a single DB heavier than `MAX` is never cached.
pub(crate) struct DbCache<T, const MAX: usize> {
    entries: HashMap<PathBuf, DbCacheEntry<T>>,
    weight: usize,
    tick: u64,
}

struct DbCacheEntry<T> {
    sig: DbSig,
    val: Arc<T>,
    weight: usize,
    used: u64,
}

impl<T, const MAX: usize> Default for DbCache<T, MAX> {
    fn default() -> Self {
        Self { entries: HashMap::new(), weight: 0, tick: 0 }
    }
}

impl<T, const MAX: usize> DbCache<T, MAX> {
    /// Returns the cached value for `path` if it was read at signature `sig`.
    pub(crate) fn get(&mut self, path: &Path, sig: DbSig) -> Option<Arc<T>> {
        self.tick += 1;
        let e = self.entries.get_mut(path).filter(|e| e.sig == sig)?;
        e.used = self.tick;
        Some(Arc::clone(&e.val))
    }

    /// Stores `val` (holding `records` call records) read at `sig`, evicting least recently used DBs.
    pub(crate) fn insert(&mut self, path: PathBuf, sig: DbSig, val: Arc<T>, records: usize) {
        self.remove(&path);
        let weight = records.saturating_add(1);
        if weight > MAX {
            return;
        }
        while self.weight + weight > MAX {
            let Some(lru) = self.entries.iter().min_by_key(|(_, e)| e.used).map(|(p, _)| p.clone()) else { break };
            self.remove(&lru);
        }
        self.tick += 1;
        self.weight += weight;
        self.entries.insert(path, DbCacheEntry { sig, val, weight, used: self.tick });
    }

    fn remove(&mut self, path: &Path) {
        if let Some(e) = self.entries.remove(path) {
            self.weight -= e.weight;
        }
    }

    /// Keeps only DBs whose path satisfies `keep` (used to forget deleted conversations).
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&Path) -> bool) {
        let weight = &mut self.weight;
        self.entries.retain(|p, e| {
            let k = keep(p);
            if !k {
                *weight -= e.weight;
            }
            k
        });
    }

    #[cfg(test)]
    pub(crate) fn weight(&self) -> usize {
        self.weight
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }
}

/// Desktop call records kept across `/api/usage` requests. A record is ~300 bytes with its
/// strings, so the bound is roughly 30 MB; one Mac with 161 MB of DBs held ~1.2k records.
const DESKTOP_CACHE_MAX_RECORDS: usize = 100_000;

struct DesktopDbCalls {
    calls: Vec<Call>,
    min_at: i64,
    max_at: i64,
}

type DesktopDbCache = DbCache<DesktopDbCalls, DESKTOP_CACHE_MAX_RECORDS>;

static DESKTOP_DB_CACHE: LazyLock<Mutex<DesktopDbCache>> = LazyLock::new(Mutex::default);

fn desktop_cache() -> MutexGuard<'static, DesktopDbCache> {
    DESKTOP_DB_CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One `<app_data>/conversations/<uuid>.db`, listed once per request.
struct DesktopDb {
    path: PathBuf,
    cid: String,
    sig: Option<DbSig>,
}

/// Lists desktop conversation DBs in `read_dir` order with their signatures, and forgets cached
/// DBs from the same directory that no longer exist.
fn list_desktop_dbs(app_data: &Path) -> Vec<DesktopDb> {
    let conv_dir = app_data.join("conversations");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&conv_dir) {
        for entry in rd.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            let Some(cid) = is_uuid_db(&fname) else { continue };
            out.push(DesktopDb { path: entry.path(), cid: cid.to_string(), sig: signature(app_data, cid) });
        }
    }
    let present: HashSet<&Path> = out.iter().map(|d| d.path.as_path()).collect();
    desktop_cache().retain(|p| p.parent() != Some(conv_dir.as_path()) || present.contains(p));
    out
}

/// Returns the DB's calls from the process-wide cache, reading the DB only when its signature changed.
fn desktop_db_calls(db: &DesktopDb, sig: DbSig) -> Option<Arc<DesktopDbCalls>> {
    let hit = desktop_cache().get(&db.path, sig);
    if hit.is_some() {
        return hit;
    }
    let calls = read_usage_calls(&db.path, &db.cid, DESKTOP).ok()?;
    let min_at = calls.iter().map(|c| c.at_ms).min().unwrap_or(i64::MAX);
    let max_at = calls.iter().map(|c| c.at_ms).max().unwrap_or(i64::MIN);
    let records = calls.len();
    let val = Arc::new(DesktopDbCalls { calls, min_at, max_at });
    desktop_cache().insert(db.path.clone(), sig, Arc::clone(&val), records);
    Some(val)
}

/// Collects deduplicated desktop calls falling inside `[from_ms, to_ms)` from live memory + SQLite DBs.
fn collect_desktop_calls_in_range(dbs: &[DesktopDb], from_ms: i64, to_ms: i64, live_calls: &[Call]) -> Vec<Call> {
    let mut merged = Vec::new();
    let mut seen = DedupSet::default();

    for c in live_calls {
        if c.at_ms >= from_ms && c.at_ms < to_ms && seen.insert(c) {
            merged.push(c.clone());
        }
    }

    for db in dbs {
        // A DB whose metadata cannot be read cannot be opened either.
        let Some(sig) = db.sig else { continue };
        let mtime = sig.mtime_ms;
        if from_ms > 0 && mtime > 0 && mtime < from_ms - 60_000 {
            continue;
        }
        let Some(recs) = desktop_db_calls(db, sig) else { continue };
        if recs.max_at < from_ms || recs.min_at >= to_ms {
            continue;
        }
        for r in &recs.calls {
            if r.at_ms >= from_ms && r.at_ms < to_ms && seen.insert(r) {
                merged.push(r.clone());
            }
        }
    }

    merged
}

/// Per-request cache of deduplicated desktop + CLI calls per calendar day, preventing
/// redundant work across quota groups, past cycles, transition days, and miss checks.
/// The desktop DB listing is taken once per request; parsed DBs come from `DESKTOP_DB_CACHE`.
#[derive(Default)]
struct DayCallCache {
    days: HashMap<String, Vec<Call>>,
    desktop_dbs: Option<Vec<DesktopDb>>,
}

impl DayCallCache {
    fn get_or_load<'a>(
        &'a mut self,
        app_data: &Path,
        cli_dir: &Path,
        plugin_data: &Path,
        cli: &mut CliSnapshot,
        day: &LocalDay,
        live_calls: &[Call],
    ) -> &'a [Call] {
        self.days.entry(day.key()).or_insert_with(|| {
            let dbs = self.desktop_dbs.get_or_insert_with(|| list_desktop_dbs(app_data));
            collect_all_day_calls(dbs, cli_dir, plugin_data, cli, day, live_calls)
        })
    }
}

/// Every call of `day` on disk: the desktop databases', then the CLI's, each call once. Lists and
/// reads files, so callers must not hold the ENGINE lock; combine with the live engine's calls
/// through [`with_live_calls`].
pub(crate) fn disk_day_calls(
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliSnapshot,
    day: &LocalDay,
) -> Vec<Call> {
    collect_all_day_calls(&list_desktop_dbs(app_data), cli_dir, plugin_data, cli, day, &[])
}

/// Every desktop and CLI call of `day`: `live_calls` (the live engine's, possibly not on disk yet),
/// then the desktop calls of `disk` ([`disk_day_calls`]) not already among them, then its CLI calls.
/// The same calls, in the same order, as reading the day with the live calls in hand.
pub(crate) fn with_live_calls(live_calls: &[Call], disk: &[Call], day: &LocalDay) -> Vec<Call> {
    let mut seen = DedupSet::default();
    let mut out: Vec<Call> = live_calls.iter().filter(|c| day.contains(c.at_ms) && seen.insert(c)).cloned().collect();
    let (desktop, cli): (Vec<&Call>, Vec<&Call>) = disk.iter().partition(|c| c.source != CLI);
    out.extend(desktop.into_iter().filter(|c| seen.insert(c)).cloned());
    out.extend(cli.into_iter().cloned());
    out
}

/// A day of history priced, desktop and `agy CLI` apart.
#[derive(Default)]
pub(crate) struct PricedDay {
    pub desktop: DayCost,
    pub cli: DayCost,
}

impl PricedDay {
    /// Both sources by label.
    pub fn merged(self) -> DayCost {
        let mut out = self.desktop;
        for (label, (usd, agg)) in self.cli {
            let e = out.entry(label).or_default();
            e.0 += usd;
            e.1.merge_from(&agg);
        }
        out
    }
}

/// Prices one day of history, as both the status bar's "today" and the Usage tab's day rows show it.
/// On a price-cut day, pass the day's calls ([`day_calls`]) as `exact`: each is priced at its own
/// time and whatever the aggregates hold beyond them at noon. Otherwise everything is priced at noon.
pub(crate) fn price_recorded_day(
    rec: &DayRecord,
    day: &LocalDay,
    pricer: &Pricer,
    exact: Option<&[Call]>,
) -> PricedDay {
    let exact = exact.filter(|_| pricer.is_cut_day(day));
    let (cli_exact, desk_exact): (Vec<Call>, Vec<Call>) =
        exact.into_iter().flatten().cloned().partition(|c| c.source == CLI);
    PricedDay {
        desktop: pricer.price_day(&rec.raw, exact.map(|_| desk_exact.as_slice()), day),
        cli: pricer.price_day(&rec.cli_raw, exact.map(|_| cli_exact.as_slice()), day),
    }
}

/// Collects all desktop + CLI calls for a single local day.
fn collect_all_day_calls(
    desktop_dbs: &[DesktopDb],
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliSnapshot,
    day: &LocalDay,
    live_calls: &[Call],
) -> Vec<Call> {
    let mut out = collect_desktop_calls_in_range(desktop_dbs, day.start_ms(), day.end_ms(), live_calls);
    out.extend(cli.calls_for_day(cli_dir, plugin_data, &day.key()));
    out
}

type AnalyzedCall = (Call, Option<String>, f64, Option<MissInfo>);

/// Runs per-conversation chronological `miss_scan` for `day`, seeding each active conversation
/// with its previous-day calls so cross-midnight first cache misses are accurately detected,
/// while returning only `day`'s calls and miss totals.
#[allow(clippy::too_many_arguments)]
fn analyze_day_calls_with_miss(
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliSnapshot,
    day_cache: &mut DayCallCache,
    day: &LocalDay,
    live_calls: &[Call],
    pricer: &Pricer,
) -> (Vec<AnalyzedCall>, MissSummary) {
    let target_calls = day_cache.get_or_load(app_data, cli_dir, plugin_data, cli, day, live_calls).to_vec();
    if target_calls.is_empty() {
        return (Vec::new(), MissSummary::default());
    }
    let active_convs: HashSet<(&'static str, String)> =
        target_calls.iter().map(|c| (c.source, c.conv_id.clone())).collect();

    let mut by_conv: HashMap<(&'static str, String), Vec<Call>> = HashMap::new();
    let prev_calls = day_cache.get_or_load(app_data, cli_dir, plugin_data, cli, &day.prev(), live_calls);
    for c in prev_calls {
        let key = (c.source, c.conv_id.clone());
        if active_convs.contains(&key) {
            by_conv.entry(key).or_default().push(c.clone());
        }
    }
    for c in target_calls {
        by_conv.entry((c.source, c.conv_id.clone())).or_default().push(c);
    }

    let mut analyzed = Vec::new();
    let mut summary = MissSummary::default();

    for ((src, cid), mut list) in by_conv {
        list.sort_by_key(|c| (c.at_ms, c.step().unwrap_or(0)));
        let markers = if src == DESKTOP { scan_step_markers(app_data, &cid) } else { None };
        let misses = miss_scan(&list, pricer, markers.as_ref());
        let priced: Vec<(Option<String>, f64)> = list
            .iter()
            .map(|c| (pricer.rule(&c.model).map(|r| r.short_name.clone()), pricer.call_cost(c).total()))
            .collect();
        for ((c, (short_opt, usd)), miss_opt) in list.into_iter().zip(priced).zip(misses) {
            if !day.contains(c.at_ms) {
                continue;
            }
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
/// Returns `exact = true` only when NO non-empty boundary day had to fall back to a whole-day summary.
#[allow(clippy::too_many_arguments)]
fn cycle_spend(
    h: &UsageHistory,
    app_data: &Path,
    cli_dir: &Path,
    plugin_data: &Path,
    cli: &mut CliSnapshot,
    day_cache: &mut DayCallCache,
    live_calls: &[Call],
    bucket_id: &str,
    from_ms: i64,
    to_ms: i64,
    pricer: &Pricer,
) -> (f64, u64, bool) {
    if to_ms <= from_ms {
        return (0.0, 0, false);
    }
    let kind = QuotaKind::of_bucket_id(bucket_id);
    let in_group = |c: &&Call| kind.counts(&c.model);
    let start_day = LocalDay::of(from_ms);
    let end_day = LocalDay::of(to_ms.saturating_sub(1).max(from_ms));
    let mut usd = 0.0;
    let mut tokens = 0u64;
    let mut all_boundaries_exact = true;
    let mut any_exact_boundary = false;

    // 1. Boundary day(s): exact call-level filtering over [from_ms, to_ms).
    let mut boundary_days = vec![start_day];
    if end_day != start_day {
        boundary_days.push(end_day);
    }
    for bd in &boundary_days {
        let day_calls = day_cache.get_or_load(app_data, cli_dir, plugin_data, cli, bd, live_calls);
        if !day_calls.is_empty() {
            any_exact_boundary = true;
            for c in day_calls.iter().filter(|c| c.at_ms >= from_ms && c.at_ms < to_ms).filter(in_group) {
                usd += pricer.call_cost(c).total();
                tokens += c.tokens.total();
            }
        } else if let Some(drec) = h.days.get(&bd.key()) {
            let aggs: Vec<_> = group_aggregates(drec, bucket_id).collect();
            if aggs.iter().any(|(_, a)| a.calls > 0) {
                // Boundary day had usage in persisted summary but no raw call details: whole-day fallback.
                all_boundaries_exact = false;
                let (u, t) = day_sum(&pricer.price_day(aggs, None, bd));
                usd += u;
                tokens += t;
            }
        }
    }

    // 2. Full calendar days strictly between `start_day` and `end_day`.
    for (dk, drec) in h.days.range(start_day.key()..=end_day.key()) {
        let Some(day) = LocalDay::parse(dk) else { continue };
        if day == start_day || day == end_day {
            continue;
        }
        let exact: Option<Vec<Call>> = pricer.is_cut_day(&day).then(|| {
            let day_calls = day_cache.get_or_load(app_data, cli_dir, plugin_data, cli, &day, live_calls);
            day_calls.iter().filter(in_group).cloned().collect()
        });
        let (u, t) = day_sum(&pricer.price_day(group_aggregates(drec, bucket_id), exact.as_deref(), &day));
        usd += u;
        tokens += t;
    }

    (usd, tokens, all_boundaries_exact && (any_exact_boundary || usd > 0.0))
}

/// A day's desktop and CLI aggregates for models in quota group `bucket_id`.
fn group_aggregates<'a>(drec: &'a DayRecord, bucket_id: &'a str) -> impl Iterator<Item = (&'a String, &'a ModelAgg)> {
    drec.raw
        .iter()
        .chain(drec.cli_raw.iter())
        .filter(move |(key, _)| QuotaKind::of_bucket_id(bucket_id).counts(&ModelId::from_bucket_key(key)))
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
    cli: &mut CliSnapshot,
    live_calls: &[Call],
    pricer: &Pricer,
    now_ms: i64,
) -> Value {
    let mut day_cache = DayCallCache::default();
    let is_all = span_req.eq_ignore_ascii_case("all");
    let today = LocalDay::today();
    let acct_key = h.cur_acct.as_deref();

    let span_days: usize = if is_all {
        let earliest = h
            .days
            .iter()
            .find(|(_, d)| !d.raw.is_empty() || !d.cli_raw.is_empty())
            .and_then(|(k, _)| LocalDay::parse(k));
        match earliest {
            Some(first) => ((today.date() - first.date()).num_days().max(0) as usize + 1).clamp(7, HISTORY_KEEP_DAYS),
            None => 7,
        }
    } else if span_req.parse::<usize>().unwrap_or(7) >= 20 {
        30
    } else {
        7
    };

    // The last `span_days` days, oldest first, ending today.
    let span: Vec<LocalDay> = (0..span_days.max(1)).rev().map(|n| today.back(n)).collect();

    // Index early-reset events by calendar day for the active account.
    let acct_resets: Vec<&ResetEvent> =
        acct_key.map(|ak| h.resets.iter().filter(|r| r.acct_key == ak).collect()).unwrap_or_default();
    let mut resets_by_day: HashMap<String, Vec<Value>> = HashMap::new();
    for r in &acct_resets {
        resets_by_day.entry(LocalDay::of(r.at_ms).key()).or_default().push(json!(r));
    }

    let mut model_totals: HashMap<String, (f64, u64, u64)> = HashMap::new();
    let mut days_out: Vec<Value> = Vec::with_capacity(span.len());
    let mut total_usd = 0.0;
    let mut total_tokens = 0u64;
    let mut total_calls = 0u64;
    let mut active_days = 0usize;
    let mut has_cli = false;

    for day in &span {
        let dk = &day.key();
        let rec = h.days.get(dk);

        let mut by_model = DayCost::new();
        let mut day_usd = 0.0;
        let mut totals = ModelAgg::default();
        let mut day_cli_calls = 0u64;

        if let Some(r) = rec {
            let persisted_cli_calls = conv_calls(&r.cli_raw);
            // On price-cut days, the calls still on disk are priced at their own time.
            let exact: Option<Vec<Call>> = pricer
                .is_cut_day(day)
                .then(|| day_cache.get_or_load(app_data, cli_dir, plugin_data, cli, day, live_calls).to_vec());
            let exact_cli_calls = exact.iter().flatten().filter(|c| c.source == CLI).count() as u64;
            by_model = price_recorded_day(r, day, pricer, exact.as_deref()).merged();
            for (usd, agg) in by_model.values() {
                day_usd += usd;
                totals.merge_from(agg);
            }
            day_cli_calls = exact_cli_calls.max(persisted_cli_calls);
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

        let day_tokens = totals.total_tokens();
        if totals.calls > 0 {
            active_days += 1;
        }
        total_usd += day_usd;
        total_tokens += day_tokens;
        total_calls += totals.calls;

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
                    json!({
                        "name": name,
                        "usd": r6(usd),
                        "calls": agg.calls,
                        "in": agg.input,
                        "cr": agg.cache_read,
                        "cw": agg.cache_write,
                        "out": agg.output,
                        "tokens": agg.total_tokens(),
                        "hit": agg.tokens().cache_hit(),
                    })
                })
                .collect()
        };

        let day_resets = resets_by_day.get(dk).cloned().unwrap_or_default();
        days_out.push(json!({
            "date": dk,
            "isToday": *day == today,
            "bf": rec.is_some_and(|r| r.bf),
            "convs": rec.map(|r| r.convs + r.cli_convs).unwrap_or(0),
            "cliCalls": day_cli_calls,
            "usd": r6(day_usd),
            "calls": totals.calls,
            "in": totals.input,
            "cr": totals.cache_read,
            "cw": totals.cache_write,
            "out": totals.output,
            "tokens": day_tokens,
            "hit": totals.tokens().cache_hit(),
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
        .map(|ak| {
            weekly_forecasts(
                h,
                samples,
                ak,
                app_data,
                cli_dir,
                plugin_data,
                cli,
                &mut day_cache,
                live_calls,
                pricer,
                now_ms,
            )
        })
        .unwrap_or_default();

    // Today's cache-miss summary across desktop + CLI calls (including cross-midnight baseline).
    let (_, miss_today) =
        analyze_day_calls_with_miss(app_data, cli_dir, plugin_data, cli, &mut day_cache, &today, live_calls, pricer);

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
        "priceSource": pricer.source(),
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
    cli: &mut CliSnapshot,
    day_cache: &mut DayCallCache,
    live_calls: &[Call],
    pricer: &Pricer,
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
            cycle_spend(h, app_data, cli_dir, plugin_data, cli, day_cache, live_calls, bid, start_ms, now_ms, pricer);
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
                    day_cache,
                    live_calls,
                    bid,
                    c.start_ms,
                    c.end_ms,
                    pricer,
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
    cli: &mut CliSnapshot,
    date: &str,
    live_calls: &[Call],
    pricer: &Pricer,
    costly_threshold: f64,
    limit: usize,
) -> Value {
    let mut day_cache = DayCallCache::default();
    let cli_titles = read_cli_titles(cli_dir);
    let (analyzed, miss_summary) = match LocalDay::parse(date) {
        Some(day) => {
            analyze_day_calls_with_miss(app_data, cli_dir, plugin_data, cli, &mut day_cache, &day, live_calls, pricer)
        }
        None => (Vec::new(), MissSummary::default()),
    };

    let mut titles: HashMap<(&'static str, String), String> = HashMap::new();
    let mut tot_usd = 0.0;
    let mut tot = Tokens::default();
    let mut cli_count = 0usize;
    let mut desktop_count = 0usize;
    let total_calls = analyzed.len();

    #[derive(Default)]
    struct ConvAgg {
        title: String,
        source: &'static str,
        calls: usize,
        usd: f64,
        tokens: Tokens,
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
        tot += &c.tokens;
        if c.source == CLI {
            cli_count += 1;
        } else {
            desktop_count += 1;
        }

        let title = titles
            .entry((c.source, c.conv_id.clone()))
            .or_insert_with(|| {
                if c.source == CLI {
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
            .or_else(|| (!c.model.display.is_empty()).then_some(c.model.display.clone()))
            .unwrap_or_else(|| c.model.raw.clone());
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
        ce.tokens += &c.tokens;
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

        let costly = costly_threshold > 0.0 && usd >= costly_threshold;

        rows.push(json!({
            "at": c.at_ms,
            "convId": c.conv_id,
            "convTitle": title,
            "source": c.source,
            "model": model_label,
            "usd": r6(usd),
            "costly": costly,
            "in": c.tokens.input,
            "cr": c.tokens.cache_read,
            "cw": c.tokens.cache_write,
            "out": c.tokens.output,
            "hit": c.tokens.cache_hit(),
            "ttftMs": c.ttft_ms,
            "streamMs": c.stream_ms,
            "tps": c.tps(),
            "miss": miss_opt,
        }));
    }

    let mut by_conv: Vec<Value> = {
        let mut v: Vec<_> = conv_map.into_iter().collect();
        v.sort_by(|a, b| b.1.usd.partial_cmp(&a.1.usd).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .map(|((_, cid), ca)| {
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
                    "in": ca.tokens.input,
                    "cr": ca.tokens.cache_read,
                    "cw": ca.tokens.cache_write,
                    "out": ca.tokens.output,
                    "tokens": ca.tokens.total(),
                    "hit": ca.tokens.cache_hit(),
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
            "in": tot.input,
            "cr": tot.cache_read,
            "cw": tot.cache_write,
            "out": tot.output,
            "tokens": tot.total(),
            "hit": tot.cache_hit(),
        },
        "byConv": by_conv,
        "calls": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::{ModelId, Tokens};
    use crate::config::ModelRule;

    #[test]
    fn recorded_day_prices_desktop_and_cli_apart_with_exact_calls_on_a_cut_day() {
        const CUT: i64 = 1_800_000_000_000;
        // A zone where the cut lands at 10:00: before noon, so the remainder costs the new price.
        let day = LocalDay::of_in(CUT, &chrono::FixedOffset::east_opt(2 * 3600).unwrap());
        let tier = |until_ms, input_per_m| crate::config::PriceTier {
            until_ms,
            input_per_m,
            cache_read_per_m: 0.0,
            cache_write_per_m: 0.0,
            output_per_m: 0.0,
        };
        let rule = ModelRule {
            pattern: "carbon".into(),
            short_name: "Carbon".into(),
            tiers: vec![tier(Some(CUT), 2.0), tier(None, 4.0)],
            ..Default::default()
        };
        let pricer = Pricer::from_rules(vec![rule], CUT);
        assert!(pricer.is_cut_day(&day));

        let model = ModelId::new("gemini-4-carbon", "Gemini 4 Carbon");
        let agg = |calls, input| ModelAgg { calls, input, ..Default::default() };
        let mut rec = DayRecord::default();
        rec.raw.insert(model.bucket_key(), agg(2, 300_000));
        rec.cli_raw.insert(model.bucket_key(), agg(1, 100_000));
        let call = |source, input, at_ms| Call {
            source,
            model: model.clone(),
            tokens: Tokens { input, ..Default::default() },
            at_ms,
            ..Default::default()
        };
        // Before the cut: one desktop call still on disk, and the CLI's only call.
        let exact = [call(DESKTOP, 100_000, CUT - 3_600_000), call(CLI, 100_000, CUT - 3_600_000)];

        let priced = price_recorded_day(&rec, &day, &pricer, Some(&exact));
        // Desktop: 100k at $2/M, the other 200k (no longer on disk) at noon, $4/M.
        assert!((day_sum(&priced.desktop).0 - (0.2 + 0.8)).abs() < 1e-9);
        assert!((day_sum(&priced.cli).0 - 0.2).abs() < 1e-9);
        let merged = priced.merged();
        assert_eq!(merged["Carbon"].1.calls, 3);
        assert!((merged["Carbon"].0 - 1.2).abs() < 1e-9);

        // Without the day's calls, everything is priced at noon.
        let at_noon = price_recorded_day(&rec, &day, &pricer, None).merged();
        assert!((at_noon["Carbon"].0 - 1.6).abs() < 1e-9);
    }

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
    fn live_calls_come_first_and_their_disk_copies_are_dropped() {
        let day = LocalDay::of_in(1_800_000_000_000, &chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let at = day.start_ms() + 3_600_000;
        let call = |conv: &str, source: &'static str, id: &str, at_ms: i64| Call {
            conv_id: conv.into(),
            source,
            model: ModelId::new("gemini-4-neon", "Gemini 4 Neon"),
            tokens: Tokens { input: 10, output: 5, ..Default::default() },
            at_ms,
            response_id: Some(id.into()),
            ..Default::default()
        };
        // Live: one call of the day, one from the day before (dropped).
        let live = [call("a", DESKTOP, "r1", at), call("a", DESKTOP, "r0", day.start_ms() - 1)];
        // Disk: the live call's database copy (dated a few seconds apart), another desktop call, a CLI call.
        let disk = [call("a", DESKTOP, "r1", at + 4_000), call("b", DESKTOP, "r2", at), call("c", CLI, "r3", at)];
        let ids = |calls: &[Call]| calls.iter().map(|c| c.response_id.clone().unwrap()).collect::<Vec<_>>();
        let merged = with_live_calls(&live, &disk, &day);
        assert_eq!(ids(&merged), ["r1", "r2", "r3"]);
        assert_eq!(merged[0].at_ms, at, "the live copy is kept");
        assert_eq!(ids(&with_live_calls(&[], &disk, &day)), ["r1", "r2", "r3"]);
    }

    #[test]
    fn usage_calls_keep_dated_rows_with_tokens() {
        use crate::convdb::fixture::{Gen, Step, write_db};
        let dir = std::env::temp_dir().join(format!("agy-pb-db-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cid = "00000000-0000-4000-8000-000000000001";
        let db_path = dir.join(format!("{cid}.db"));
        let t1 = 1_795_500_000u64;
        write_db(
            &db_path,
            &[Step { idx: 2, at_s: t1, exec: "exec-a" }],
            &[
                Gen {
                    input: 1000,
                    output: 50,
                    cache_read: 9000,
                    thinking_output: 40,
                    response_output: 30,
                    response_id: "req-1",
                    raw: "gemini-4-neon",
                    display: "Gemini 4 Neon",
                    steps: vec![2],
                    exec: "exec-a",
                    ttft: Some((1, 250_000_000)),
                    stream: Some((8, 500_000_000)),
                    ..Default::default()
                },
                // No tokens, and no time: neither counts toward usage.
                Gen { no_usage: true, steps: vec![2], ..Default::default() },
                Gen { input: 5, steps: vec![7], ..Default::default() },
            ],
            None,
        );

        let calls = read_usage_calls(&db_path, cid, crate::call::CLI).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].at_ms, (t1 as i64) * 1000);
        assert_eq!(calls[0].step(), Some(2));
        assert_eq!(calls[0].tokens.output, 70);
        assert_eq!(calls[0].response_id.as_deref(), Some("req-1"));
        assert_eq!(calls[0].source, "cli");
        assert!((calls[0].ttft_ms.unwrap() - 1250.0).abs() < 1.0);
        assert!((calls[0].stream_ms.unwrap() - 8500.0).abs() < 1.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn transition_day_preserves_persisted_totals_when_detail_calls_are_partial() {
        let root = std::env::temp_dir().join(format!("agy-trans-day-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let day = LocalDay::today();
        let today = day.key();
        let d_start = day.start_ms();
        let cut_ms = d_start + 16 * 3_600_000;
        let rules = vec![ModelRule {
            pattern: "carbon".into(),
            display_name: "Gemini 4 Carbon".into(),
            short_name: "Carbon".into(),
            context_limit: 1_000_000,
            input_per_m: 2.0,
            cache_read_per_m: 0.1,
            cache_write_per_m: 0.0,
            output_per_m: 10.0,
            online_key: "gemini-4-carbon".into(),
            tiers: vec![
                crate::config::PriceTier {
                    until_ms: Some(cut_ms),
                    input_per_m: 2.0,
                    cache_read_per_m: 0.1,
                    cache_write_per_m: 0.0,
                    output_per_m: 10.0,
                },
                crate::config::PriceTier {
                    until_ms: None,
                    input_per_m: 4.0,
                    cache_read_per_m: 0.2,
                    cache_write_per_m: 0.0,
                    output_per_m: 20.0,
                },
            ],
        }];

        // Persisted history has 2 calls totaling 100k input tokens; only 1 post-cut live call (50k) remains.
        let mut h = UsageHistory { v: 2, live_from: today.clone(), writable: true, ..Default::default() };
        let mut day_rec = DayRecord::default();
        day_rec.raw.insert(
            "gemini-4-carbon".into(),
            ModelAgg { calls: 2, input: 100_000, cache_read: 0, cache_write: 0, output: 0 },
        );
        h.days.insert(today.clone(), day_rec);

        let live = vec![Call {
            conv_id: "00000000-0000-4000-8000-000000000001".into(),
            source: "desktop",
            at_ms: cut_ms + 3_600_000, // post-cut: $4/M → $0.20 for 50k tokens
            steps: vec![1],
            model: ModelId::new("gemini-4-carbon", "Gemini 4 Carbon"),
            tokens: Tokens { input: 50_000, cache_read: 0, cache_write: 0, output: 0 },
            response_id: Some("req-post".into()),
            ..Default::default()
        }];

        let mut cli = CliSnapshot::default();
        let view = build_usage_view(
            &h,
            &QuotaSamples::default(),
            "7",
            &root,
            &root,
            &root,
            &mut cli,
            &live,
            &Pricer::from_rules(rules, cut_ms + 7_200_000),
            cut_ms + 7_200_000,
        );
        assert_eq!(view["today"]["calls"], 2, "must preserve persisted call count");
        assert_eq!(view["today"]["tokens"], 100_000, "must preserve persisted tokens");
        // Surviving 50k priced at post-cut $4/M ($0.20) + remaining 50k at noon pre-cut $2/M ($0.10) = $0.30.
        let usd = view["today"]["usd"].as_f64().unwrap();
        assert!((usd - 0.30).abs() < 1e-4, "expected $0.30, got {usd}");
        let _ = fs::remove_dir_all(&root);
    }

    /// On a price-cut day, persisted rows and exact calls must get the same label, or the
    /// remainder subtraction never meets and the same call is counted twice. Here the raw name
    /// alone would match "orchestrator" (Carbon) while raw+display matches "hydrogen" (ADR 0001).
    #[test]
    fn transition_day_counts_call_once_when_raw_and_display_match_different_rules() {
        let root = std::env::temp_dir().join(format!("agy-trans-label-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let day = LocalDay::today();
        let today = day.key();
        let d_start = day.start_ms();
        let cut_ms = d_start + 16 * 3_600_000;
        let rule = |pattern: &str, short: &str| ModelRule {
            pattern: pattern.into(),
            display_name: format!("Gemini 4 {short}"),
            short_name: short.into(),
            context_limit: 1_000_000,
            input_per_m: 2.0,
            cache_read_per_m: 0.1,
            cache_write_per_m: 0.0,
            output_per_m: 10.0,
            online_key: String::new(),
            tiers: vec![
                crate::config::PriceTier {
                    until_ms: Some(cut_ms),
                    input_per_m: 2.0,
                    cache_read_per_m: 0.1,
                    cache_write_per_m: 0.0,
                    output_per_m: 10.0,
                },
                crate::config::PriceTier {
                    until_ms: None,
                    input_per_m: 4.0,
                    cache_read_per_m: 0.2,
                    cache_write_per_m: 0.0,
                    output_per_m: 20.0,
                },
            ],
        };
        // Same order as the built-in rules: "hydrogen" is checked before the generic "orchestrator".
        let rules = vec![rule("hydrogen", "Hydrogen"), rule("orchestrator", "Carbon")];

        let model = ModelId::new("orchestrator-exec", "Gemini 4 Hydrogen (Orchestrator)");
        // One call, persisted under its model identity...
        let mut h = UsageHistory { v: 2, live_from: today.clone(), writable: true, ..Default::default() };
        let mut day_rec = DayRecord::default();
        day_rec
            .raw
            .insert(model.bucket_key(), ModelAgg { calls: 1, input: 50_000, cache_read: 0, cache_write: 0, output: 0 });
        h.days.insert(today.clone(), day_rec);

        // ...and the same call still available as an exact record.
        let live = vec![Call {
            conv_id: "00000000-0000-4000-8000-000000000002".into(),
            source: "desktop",
            at_ms: cut_ms + 3_600_000,
            steps: vec![1],
            model,
            tokens: Tokens { input: 50_000, ..Default::default() },
            response_id: Some("resp-1".into()),
            ..Default::default()
        }];

        let mut cli = CliSnapshot::default();
        let view = build_usage_view(
            &h,
            &QuotaSamples::default(),
            "7",
            &root,
            &root,
            &root,
            &mut cli,
            &live,
            &Pricer::from_rules(rules, cut_ms + 7_200_000),
            cut_ms + 7_200_000,
        );
        let _ = fs::remove_dir_all(&root);
        assert_eq!(view["today"]["calls"], 1, "one call must be counted once");
        assert_eq!(view["today"]["tokens"], 50_000, "one call's tokens must be counted once");
    }

    #[test]
    fn cross_midnight_first_call_detects_cache_miss_from_previous_day_baseline() {
        let root = std::env::temp_dir().join(format!("agy-midnight-miss-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let rules = crate::config::default_model_rules();
        let day2 = "2026-11-25";
        let midnight = LocalDay::parse(day2).unwrap().start_ms();
        let conv_id = "00000000-0000-4000-8000-0000000000aa";

        let live = vec![
            Call {
                conv_id: conv_id.into(),
                source: "desktop",
                at_ms: midnight - 300_000, // 23:55 yesterday
                steps: vec![1],
                model: ModelId::new("gemini-4-neon", "Gemini 4 Neon"),
                tokens: Tokens { input: 50_000, cache_read: 0, cache_write: 0, output: 100 },
                ttft_ms: Some(1000.0),
                stream_ms: Some(2000.0),
                response_id: Some("req-prev".into()),
                ..Default::default()
            },
            Call {
                conv_id: conv_id.into(),
                source: "desktop",
                at_ms: midnight + 300_000, // 00:05 today (10 min later, 0 cache read)
                steps: vec![2],
                model: ModelId::new("gemini-4-neon", "Gemini 4 Neon"),
                tokens: Tokens { input: 50_000, cache_read: 0, cache_write: 0, output: 100 },
                ttft_ms: Some(1000.0),
                stream_ms: Some(2000.0),
                response_id: Some("req-today".into()),
                ..Default::default()
            },
        ];

        let mut cli = CliSnapshot::default();
        let out = build_day_calls_view(
            &root,
            &root,
            &root,
            &mut cli,
            day2,
            &live,
            &Pricer::from_rules(rules, midnight + 600_000),
            0.50,
            600,
        );
        assert_eq!(out["totalCalls"], 1, "only day2 calls should be returned");
        assert_eq!(out["missSummary"]["count"], 1, "cross-midnight first call must be counted as cache miss");
        assert!(out["calls"][0]["miss"].is_object());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn db_cache_evicts_least_recently_used_by_record_weight() {
        let sig = |mtime_ms| DbSig { mtime_ms, len: 1 };
        let (a, b, c, big) =
            (PathBuf::from("/x/a.db"), PathBuf::from("/x/b.db"), PathBuf::from("/y/c.db"), PathBuf::from("/x/big.db"));
        let mut cache: DbCache<u32, 10> = DbCache::default();
        cache.insert(a.clone(), sig(1), Arc::new(1), 4);
        cache.insert(b.clone(), sig(1), Arc::new(2), 3);
        assert_eq!(cache.weight(), 9);
        assert_eq!(cache.get(&a, sig(1)).as_deref(), Some(&1));
        assert!(cache.get(&a, sig(2)).is_none(), "a changed signature must miss");

        // `b` is least recently used, so it goes when `c` needs room.
        cache.insert(c.clone(), sig(1), Arc::new(3), 2);
        assert!(cache.contains(&a) && cache.contains(&c) && !cache.contains(&b));
        assert_eq!(cache.weight(), 8);

        // Re-inserting replaces the old entry's weight; an oversized DB is not cached.
        cache.insert(a.clone(), sig(2), Arc::new(4), 0);
        assert_eq!(cache.weight(), 4);
        cache.insert(big.clone(), sig(1), Arc::new(5), 10);
        assert!(!cache.contains(&big));

        cache.retain(|p| p.parent() != Some(Path::new("/x")));
        assert!(!cache.contains(&a) && cache.contains(&c));
        assert_eq!(cache.weight(), 3);
    }

    #[test]
    fn desktop_db_cache_reuses_calls_until_signature_changes() {
        use crate::convdb::fixture::{Gen, Step, write_db};
        let root = std::env::temp_dir().join(format!("agy-desk-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("conversations")).unwrap();
        let cid = "00000000-0000-4000-8000-0000000000cc";
        let db_path = root.join("conversations").join(format!("{cid}.db"));
        let today = LocalDay::today();
        let at_s = (crate::live::now_ms() / 1000) as u64;
        let row = Gen {
            input: 1000,
            output: 20,
            response_id: "req-cache",
            raw: "gemini-4-neon",
            steps: vec![1],
            ..Default::default()
        };
        write_db(&db_path, &[Step { idx: 1, at_s, exec: "" }], &[row], None);

        let day_calls = || {
            let mut cli = CliSnapshot::default();
            let mut cache = DayCallCache::default();
            cache.get_or_load(&root, &root, &root, &mut cli, &today, &[]).to_vec()
        };
        assert_eq!(day_calls().len(), 1);
        assert!(desktop_cache().contains(&db_path));

        // Same mtime and length: the cached calls are served without opening the (now garbage) file.
        let meta = fs::metadata(&db_path).unwrap();
        fs::write(&db_path, vec![0u8; meta.len() as usize]).unwrap();
        fs::File::options().write(true).open(&db_path).unwrap().set_modified(meta.modified().unwrap()).unwrap();
        let calls = day_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].response_id.as_deref(), Some("req-cache"));

        // A length change invalidates the entry; the garbage file then yields no calls.
        fs::write(&db_path, vec![0u8; meta.len() as usize + 1]).unwrap();
        assert!(day_calls().is_empty());

        // Deleted DBs are dropped from the cache on the next listing.
        fs::remove_file(&db_path).unwrap();
        assert!(day_calls().is_empty());
        assert!(!desktop_cache().contains(&db_path));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn trim_samples_applies_age_limit_to_every_account() {
        let pt = |at_ms| QuotaSamplePoint { at_ms, bucket_id: "gemini-weekly".into(), used: 0.1, reset_ms: 0 };
        let mut s = QuotaSamples { v: 1, by_acct: HashMap::new() };
        s.by_acct.insert("cur".into(), vec![pt(10), pt(100)]);
        s.by_acct.insert("other".into(), vec![pt(20), pt(200)]);
        s.by_acct.insert("gone".into(), vec![pt(30)]);
        trim_samples(&mut s, 50);
        assert_eq!(s.by_acct["cur"].iter().map(|p| p.at_ms).collect::<Vec<_>>(), vec![100]);
        assert_eq!(s.by_acct["other"].iter().map(|p| p.at_ms).collect::<Vec<_>>(), vec![200]);
        assert!(!s.by_acct.contains_key("gone"), "accounts with only expired samples are dropped");
    }

    #[test]
    fn cycle_spend_marks_not_exact_when_start_day_falls_back_to_day_summary() {
        let root = std::env::temp_dir().join(format!("agy-cap-exact-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let rules = crate::config::default_model_rules();
        let start_day = "2026-11-24";
        let end_day = "2026-11-25";
        let from_ms = LocalDay::parse(start_day).unwrap().start_ms() + 10 * 3_600_000;
        let to_ms = LocalDay::parse(end_day).unwrap().start_ms() + 12 * 3_600_000;

        // Start day only exists in persisted summary (no raw calls), while end day has live calls.
        let mut h = UsageHistory { v: 2, live_from: start_day.into(), writable: true, ..Default::default() };
        let mut s_rec = DayRecord::default();
        s_rec.raw.insert(
            "gemini-4-neon".into(),
            ModelAgg { calls: 1, input: 100_000, cache_read: 0, cache_write: 0, output: 0 },
        );
        h.days.insert(start_day.into(), s_rec);

        let live = vec![Call {
            conv_id: "00000000-0000-4000-8000-0000000000bb".into(),
            source: "desktop",
            at_ms: to_ms - 60_000,
            steps: vec![1],
            model: ModelId::new("gemini-4-neon", "Gemini 4 Neon"),
            tokens: Tokens { input: 100_000, cache_read: 0, cache_write: 0, output: 0 },
            response_id: Some("req-end".into()),
            ..Default::default()
        }];

        let mut cli = CliSnapshot::default();
        let mut cache = DayCallCache::default();
        let (usd, _tok, exact) = cycle_spend(
            &h,
            &root,
            &root,
            &root,
            &mut cli,
            &mut cache,
            &live,
            "gemini-weekly",
            from_ms,
            to_ms,
            &Pricer::from_rules(rules, to_ms),
        );
        assert!(usd > 0.0);
        assert!(!exact, "capExact must be false when start_day fell back to whole-day summary");
        let _ = fs::remove_dir_all(&root);
    }
}
