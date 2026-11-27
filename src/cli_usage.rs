//! Read-only scanner for `agy CLI` (`~/.gemini/antigravity-cli/conversations/*.db`) usage,
//! titles (`conversation_summaries.db` + `cache/conversation_metadata.json`), and day-indexed
//! call lookup (`cli-index.json`).
//!
//! `agy CLI` shares the same Antigravity account quota as the desktop app, so including its SQLite
//! call records keeps today's spend, daily history, weekly `cap ≈`, and call breakdowns accurate.

use crate::call::{CLI, Call, DedupSet, ModelAgg};
use crate::config::{ReadFile, read_json_keep, write_atomic};
use crate::convdb::{DbSig, db_path, is_uuid_db, signature};
use crate::usage::{
    DbCache, HISTORY_KEEP_DAYS, UsageHistory, conv_calls, day_start_ms, local_day_str, read_usage_calls, today_str,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

const CLI_BF_V: u32 = 3;
const CLI_CALLS_KEEP_MS: i64 = 32 * 86_400_000;
/// Call records kept for CLI DBs older than `CLI_CALLS_KEEP_MS` that a day lookup had to open
/// (e.g. past weekly cycles); least recently used DBs are evicted beyond this.
const CLI_OLD_MAX_RECORDS: usize = 50_000;

/// One CLI DB's calls with each call's local day computed once at read time, so day lookups
/// and aggregation never format a date per call.
pub(crate) struct CliDbCalls {
    calls: Vec<Call>,
    /// Sorted, unique local days (`YYYY-MM-DD`) that have calls.
    days: Vec<String>,
    /// Per call: index into `days`.
    day_of: Vec<u32>,
    /// Per day: indices into `calls`, in row order.
    by_day: Vec<Vec<u32>>,
}

impl CliDbCalls {
    fn new(calls: Vec<Call>) -> Self {
        let keys: Vec<String> = calls.iter().map(|c| local_day_str(c.at_ms)).collect();
        let mut days = keys.clone();
        days.sort();
        days.dedup();
        let mut by_day = vec![Vec::new(); days.len()];
        let mut day_of = Vec::with_capacity(calls.len());
        for (i, k) in keys.iter().enumerate() {
            let d = days.binary_search(k).unwrap_or_default();
            day_of.push(d as u32);
            by_day[d].push(i as u32);
        }
        Self { calls, days, day_of, by_day }
    }

    /// Calls on `date`, in row order.
    fn on_day<'a>(&'a self, date: &str) -> impl Iterator<Item = &'a Call> + 'a {
        let idx = self.days.binary_search_by(|d| d.as_str().cmp(date)).ok();
        idx.into_iter().flat_map(move |d| self.by_day[d].iter().map(move |&i| &self.calls[i as usize]))
    }
}

#[derive(Clone)]
struct CachedCliDb {
    sig: DbSig,
    db: Arc<CliDbCalls>,
}

type OldCliCache = DbCache<CliDbCalls, CLI_OLD_MAX_RECORDS>;

fn lock_old(old: &Mutex<OldCliCache>) -> MutexGuard<'_, OldCliCache> {
    old.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CliIndexEntry {
    pub m: i64,
    pub d: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CliIndex {
    pub v: u32,
    #[serde(default)]
    pub files: HashMap<String, CliIndexEntry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CliBackfillMeta {
    pub version: u32,
    pub at_ms: i64,
}

/// Result of one background CLI SQLite scan, computed completely outside `USAGE` lock.
pub struct CliScanDelta {
    pub from_ms: i64,
    pub today: String,
    pub now_ms: i64,
    pub full: bool,
    pub partial_errors: bool,
    pub agg: BTreeMap<String, (HashMap<String, ModelAgg>, usize)>,
    pub snapshot: CliSnapshot,
}

/// Lock-free read snapshot of cached CLI DBs and `cli-index.json` for HTTP handlers.
/// Cloning is O(1): the recent-DB map and index are shared `Arc`s, and the cache of older DBs
/// opened by day lookups is shared with the scanner and every other snapshot.
#[derive(Clone, Default)]
pub struct CliSnapshot {
    cache: Arc<HashMap<String, CachedCliDb>>,
    old: Arc<Mutex<OldCliCache>>,
    index: Arc<CliIndex>,
    index_loaded: bool,
}

impl CliSnapshot {
    fn ensure_index_readonly(&mut self, plugin_data: &Path) {
        if self.index_loaded {
            return;
        }
        self.index_loaded = true;
        if let ReadFile::Ok(idx) = read_json_keep::<CliIndex>(&plugin_data.join("cli-index.json"))
            && idx.v == 1
        {
            self.index = Arc::new(idx);
        }
    }

    fn load_old_db(&self, cli_dir: &Path, fname: &str) -> Option<Arc<CliDbCalls>> {
        let cid = is_uuid_db(fname)?;
        let sig = signature(cli_dir, cid)?;
        if sig.mtime_ms <= 0 {
            return None;
        }
        let path = db_path(cli_dir, cid);
        let hit = lock_old(&self.old).get(&path, sig);
        if hit.is_some() {
            return hit;
        }
        let db = Arc::new(CliDbCalls::new(read_usage_calls(&path, cid, CLI).ok()?));
        let records = db.calls.len();
        lock_old(&self.old).insert(path, sig, Arc::clone(&db), records);
        Some(db)
    }

    /// Returns all deduplicated CLI calls occurring on `date` (`YYYY-MM-DD`).
    pub fn calls_for_day(&mut self, cli_dir: &Path, plugin_data: &Path, date: &str) -> Vec<Call> {
        self.ensure_index_readonly(plugin_data);
        let mut dbs: Vec<Arc<CliDbCalls>> = self.cache.values().map(|e| Arc::clone(&e.db)).collect();

        // Consult `cli-index.json` for older DBs (>32 days) that touched `date`.
        for (fname, ent) in &self.index.files {
            if !self.cache.contains_key(fname)
                && ent.d.iter().any(|d| d == date)
                && let Some(db) = self.load_old_db(cli_dir, fname)
            {
                dbs.push(db);
            }
        }

        let mut out = Vec::new();
        let mut seen = DedupSet::default();
        for db in &dbs {
            for c in db.on_day(date) {
                if seen.insert(c) {
                    out.push(c.clone());
                }
            }
        }
        out
    }
}

#[derive(Default)]
pub struct CliScanner {
    /// Recent CLI DBs (modified within `CLI_CALLS_KEEP_MS`) cached in memory: filename -> calls.
    /// Copy-on-write: snapshots share it until a scan changes it.
    cache: Arc<HashMap<String, CachedCliDb>>,
    /// Older DBs opened by snapshot day lookups, shared with every snapshot.
    old: Arc<Mutex<OldCliCache>>,
    index: Arc<CliIndex>,
    index_loaded: bool,
    index_writable: bool,
}

impl CliScanner {
    fn ensure_index(&mut self, plugin_data: &Path) {
        if self.index_loaded {
            return;
        }
        self.index_loaded = true;
        let path = plugin_data.join("cli-index.json");
        match read_json_keep::<CliIndex>(&path) {
            ReadFile::Ok(idx) if idx.v == 1 => {
                self.index = Arc::new(idx);
                self.index_writable = true;
            }
            ReadFile::Ok(_) | ReadFile::Missing | ReadFile::Corrupt { kept: true } => {
                self.index = Arc::new(CliIndex { v: 1, files: HashMap::new() });
                self.index_writable = true;
            }
            ReadFile::Unreadable | ReadFile::Corrupt { kept: false } => {
                self.index = Arc::new(CliIndex { v: 1, files: HashMap::new() });
                self.index_writable = false;
            }
        }
    }

    fn save_index(&self, plugin_data: &Path) {
        if !self.index_writable {
            return;
        }
        let path = plugin_data.join("cli-index.json");
        if let Ok(text) = serde_json::to_string(&*self.index) {
            let _ = write_atomic(&path, &text);
        }
    }

    pub fn snapshot(&self) -> CliSnapshot {
        CliSnapshot {
            cache: Arc::clone(&self.cache),
            old: Arc::clone(&self.old),
            index: Arc::clone(&self.index),
            index_loaded: self.index_loaded,
        }
    }

    /// Scans `~/.gemini/antigravity-cli/conversations/*.db` incrementally WITHOUT holding `USAGE`.
    /// Corrupt/locked DBs keep their last successful cached result (if any) and are retried
    /// individually on subsequent ticks without blocking healthy DBs from updating usage totals.
    pub fn scan_delta(
        &mut self,
        cli_dir: &Path,
        plugin_data: &Path,
        cli_bf: Option<&CliBackfillMeta>,
        now_ms: i64,
    ) -> Option<CliScanDelta> {
        let conv_dir = cli_dir.join("conversations");
        let Ok(rd) = fs::read_dir(&conv_dir) else {
            return None;
        };
        self.ensure_index(plugin_data);

        let mut names: Vec<String> = Vec::new();
        for entry in rd.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if is_uuid_db(&fname).is_some() {
                names.push(fname);
            }
        }
        let present: HashSet<&str> = names.iter().map(String::as_str).collect();
        // Mutate the shared maps only when something changes, so snapshots keep sharing them.
        if self.cache.keys().any(|k| !present.contains(k.as_str())) {
            Arc::make_mut(&mut self.cache).retain(|k, _| present.contains(k.as_str()));
        }
        let mut index_dirty = false;
        if self.index.files.keys().any(|k| !present.contains(k.as_str())) {
            Arc::make_mut(&mut self.index).files.retain(|k, _| present.contains(k.as_str()));
            index_dirty = true;
        }
        lock_old(&self.old).retain(|p| {
            p.parent() != Some(conv_dir.as_path())
                || p.file_name().and_then(|n| n.to_str()).is_some_and(|n| present.contains(n))
        });

        let full = cli_bf.is_none_or(|b| b.version < CLI_BF_V);
        let today = today_str();
        let yest = local_day_str(now_ms - 86_400_000);
        let from_day = if full {
            local_day_str(now_ms - (HISTORY_KEEP_DAYS as i64 - 1) * 86_400_000)
        } else {
            let last_at = cli_bf.map(|b| b.at_ms).filter(|&t| t > 0).unwrap_or(now_ms);
            yest.min(local_day_str(last_at))
        };
        let from_ms = day_start_ms(&from_day).unwrap_or(now_ms - 86_400_000);
        let keep_from = now_ms - CLI_CALLS_KEEP_MS;
        let scan_cut = from_ms.min(keep_from);

        let mut partial_errors = false;
        for fname in &names {
            let Some(cid) = is_uuid_db(fname) else { continue };
            // A DB whose metadata cannot be read cannot be opened either.
            let Some(sig) = signature(cli_dir, cid) else { continue };
            let mt = sig.mtime_ms;
            if mt <= 0 || mt < scan_cut {
                continue;
            }
            if self.cache.get(fname).is_some_and(|e| e.sig == sig) {
                continue;
            }
            // Full re-read on change: rows are updated in place after insertion (usage arrives when
            // streaming ends, step timestamps can land after the generator row), so reading only
            // new `idx` rows would need its own invalidation rules. A 9 MB DB reads in ~20 ms.
            match read_usage_calls(&db_path(cli_dir, cid), cid, CLI) {
                Ok(calls) => {
                    let db = Arc::new(CliDbCalls::new(calls));
                    Arc::make_mut(&mut self.index)
                        .files
                        .insert(fname.clone(), CliIndexEntry { m: mt, d: db.days.clone() });
                    Arc::make_mut(&mut self.cache).insert(fname.clone(), CachedCliDb { sig, db });
                    index_dirty = true;
                }
                Err(_) => {
                    // Retain any previous successful cache for this DB; unreadable/new DBs stay
                    // uncached so the next scan retries only them without blocking readable DBs.
                    if mt >= from_ms {
                        partial_errors = true;
                    }
                }
            }
        }

        if index_dirty {
            self.save_index(plugin_data);
        }

        // Aggregate all readable/cached DBs from `from_ms` through today.
        let mut raw_agg: BTreeMap<String, (HashMap<String, ModelAgg>, HashSet<String>)> = BTreeMap::new();
        let mut seen = DedupSet::default();
        for e in self.cache.values() {
            if e.sig.mtime_ms < from_ms {
                continue;
            }
            let db = &e.db;
            for (c, &d) in db.calls.iter().zip(&db.day_of) {
                if c.at_ms < from_ms {
                    continue;
                }
                if !seen.insert(c) {
                    continue;
                }
                let day = &db.days[d as usize];
                // Look the day up before cloning its key: most calls land on a day already seen.
                if !raw_agg.contains_key(day) {
                    raw_agg.insert(day.clone(), Default::default());
                }
                let Some((raw, convs)) = raw_agg.get_mut(day) else { continue };
                raw.entry(c.model.bucket_key()).or_default().add_tokens(&c.tokens);
                if !convs.contains(&c.conv_id) {
                    convs.insert(c.conv_id.clone());
                }
            }
        }
        let agg = raw_agg.into_iter().map(|(d, (raw, convs))| (d, (raw, convs.len()))).collect();

        // DBs not modified within `CLI_CALLS_KEEP_MS` leave memory once aggregated (the first full
        // scan reads up to 400 days); day lookups reopen them via `cli-index.json` when needed.
        if self.cache.values().any(|e| e.sig.mtime_ms < keep_from) {
            Arc::make_mut(&mut self.cache).retain(|_, e| e.sig.mtime_ms >= keep_from);
        }

        Some(CliScanDelta { from_ms, today, now_ms, full, partial_errors, agg, snapshot: self.snapshot() })
    }
}

/// Applies a completed `CliScanDelta` onto `UsageHistory` under the `USAGE` lock (microseconds).
pub fn apply_cli_delta(h: &mut UsageHistory, mut delta: CliScanDelta) -> bool {
    let mut changed = false;
    let mut t = delta.from_ms + 43_200_000; // Local noon step avoids DST 23h/25h skip bugs.
    loop {
        let d = local_day_str(t);
        if d > delta.today {
            break;
        }
        match delta.agg.remove(&d) {
            Some((cli_raw, next_convs)) => {
                let entry = h.days.entry(d.clone()).or_default();
                let (old_calls, new_calls) = (conv_calls(&entry.cli_raw), conv_calls(&cli_raw));
                // If some DB failed to read this tick, never shrink already-persisted day totals,
                // while still allowing healthy DBs to populate or advance totals.
                let allow = !delta.partial_errors || new_calls >= old_calls;
                if allow && (entry.cli_raw != cli_raw || entry.cli_convs != next_convs) {
                    entry.cli_raw = cli_raw;
                    entry.cli_convs = next_convs;
                    changed = true;
                }
            }
            None if !delta.partial_errors => {
                if let Some(entry) = h.days.get_mut(&d)
                    && (!entry.cli_raw.is_empty() || entry.cli_convs > 0)
                {
                    entry.cli_raw.clear();
                    entry.cli_convs = 0;
                    changed = true;
                }
            }
            None => {}
        }
        t += 86_400_000;
    }

    if delta.full || changed || h.cli_bf.as_ref().is_none_or(|b| delta.now_ms - b.at_ms > 300_000) {
        h.cli_bf = Some(CliBackfillMeta { version: CLI_BF_V, at_ms: delta.now_ms });
        changed = true;
    }

    changed
}

fn clean_preview(s: &str) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = if let Some(rest) = collapsed.strip_prefix('/') {
        match rest.split_once(' ') {
            Some((cmd, after)) if cmd.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => {
                after.trim_start()
            }
            _ => collapsed.as_str(),
        }
    } else {
        collapsed.as_str()
    };
    trimmed.chars().take(60).collect()
}

pub fn read_cli_titles(cli_dir: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();

    // Fallback source first: cache/conversation_metadata.json
    let meta_path = cli_dir.join("cache").join("conversation_metadata.json");
    if let Ok(text) = fs::read_to_string(&meta_path)
        && let Ok(v) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
        && let Some(convs) = v["conversations"].as_object()
    {
        for (id, c) in convs {
            if is_uuid_db(&format!("{id}.db")).is_some()
                && let Some(t) = c["summary"]["Title"].as_str().filter(|s| !s.is_empty())
            {
                map.insert(id.clone(), t.chars().take(80).collect());
            }
        }
    }

    // Primary source: conversation_summaries.db (overrides stale JSON metadata)
    let db_path = cli_dir.join("conversation_summaries.db");
    if db_path.exists()
        && let Ok(conn) =
            Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
    {
        let _ = conn.busy_timeout(std::time::Duration::from_millis(300));
        if let Ok(mut stmt) = conn.prepare("SELECT conversation_id, title, preview FROM conversation_summaries")
            && let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
            })
        {
            for row in rows.flatten() {
                let Some(id) = row.0 else { continue };
                if is_uuid_db(&format!("{id}.db")).is_none() {
                    continue;
                }
                let title = row.1.unwrap_or_default();
                let title = title.trim();
                let chosen = if !title.is_empty() {
                    title.chars().take(80).collect::<String>()
                } else {
                    clean_preview(row.2.as_deref().unwrap_or(""))
                };
                if !chosen.is_empty() {
                    map.insert(id, chosen);
                }
            }
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::call::{ModelId, Tokens};
    use crate::convdb::fixture::{Gen, Step, write_db};

    fn neon(response_id: &'static str, steps: Vec<u64>, exec: &'static str) -> Gen {
        Gen { input: 1000, output: 200, response_id, raw: "gemini-4-neon", steps, exec, ..Default::default() }
    }

    /// Daily-aggregate key of the fixture calls (no display name).
    fn neon_key() -> String {
        crate::call::ModelId::new("gemini-4-neon", "").bucket_key()
    }

    #[test]
    fn cleans_slash_command_from_cli_preview() {
        assert_eq!(clean_preview("/plan   refactor  statusline  module"), "refactor statusline module");
        assert_eq!(clean_preview("plain   question\nwith newline"), "plain question with newline");
    }

    #[test]
    fn one_corrupt_cli_db_does_not_block_healthy_dbs() {
        let root = std::env::temp_dir().join(format!("agy-cli-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cli_dir = root.join("cli");
        let conv_dir = cli_dir.join("conversations");
        let pd = root.join("pd");
        fs::create_dir_all(&conv_dir).unwrap();
        fs::create_dir_all(&pd).unwrap();

        let good_id = "00000000-0000-4000-8000-000000000011";
        let bad_id = "00000000-0000-4000-8000-000000000022";
        fs::write(conv_dir.join(format!("{bad_id}.db")), b"not a sqlite database").unwrap();

        let now_ms = crate::live::now_ms();
        let now_s = (now_ms / 1000) as u64;
        write_db(
            &conv_dir.join(format!("{good_id}.db")),
            &[Step { idx: 1, at_s: now_s, exec: "exec-1" }],
            &[neon("cli-req-1", vec![1], "exec-1")],
            None,
        );

        let mut scanner = CliScanner::default();
        let mut h = UsageHistory { v: 2, live_from: today_str(), writable: true, ..Default::default() };
        let delta = scanner.scan_delta(&cli_dir, &pd, h.cli_bf.as_ref(), now_ms).unwrap();
        assert!(delta.partial_errors);
        assert!(apply_cli_delta(&mut h, delta));

        let today_rec = h.days.get(&today_str()).expect("healthy DB should populate today");
        assert_eq!(today_rec.cli_raw[&neon_key()].calls, 1);
        assert_eq!(today_rec.cli_convs, 1);
        let _ = fs::remove_dir_all(&root);
    }

    fn write_cli_db(path: &Path, at_ms: i64, req: &'static str) {
        write_db(path, &[Step { idx: 1, at_s: (at_ms / 1000) as u64, exec: "" }], &[neon(req, vec![1], "")], None);
    }

    fn call_at(at_ms: i64, key: &str) -> Call {
        Call {
            conv_id: "c".into(),
            source: CLI,
            at_ms,
            model: ModelId::default(),
            tokens: Tokens { input: 1, ..Default::default() },
            response_id: Some(key.into()),
            ..Default::default()
        }
    }

    #[test]
    fn cli_db_calls_buckets_calls_by_local_day_in_row_order() {
        let d1 = day_start_ms("2026-11-20").unwrap() + 3_600_000;
        let d2 = day_start_ms("2026-11-21").unwrap() + 3_600_000;
        let db = CliDbCalls::new(vec![call_at(d2, "a"), call_at(d1, "b"), call_at(d2 + 60_000, "c")]);
        assert_eq!(db.days, vec!["2026-11-20".to_string(), "2026-11-21".to_string()]);
        let keys = |date| db.on_day(date).filter_map(|c| c.response_id.clone()).collect::<Vec<_>>();
        assert_eq!(keys("2026-11-21"), vec!["a", "c"]);
        assert_eq!(keys("2026-11-20"), vec!["b"]);
        assert!(keys("2026-11-22").is_empty());
        assert_eq!(db.day_of, vec![1, 0, 1]);
    }

    #[test]
    fn scan_keeps_only_recent_dbs_in_memory_and_snapshots_share_state() {
        let root = std::env::temp_dir().join(format!("agy-cli-trim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cli_dir = root.join("cli");
        let conv_dir = cli_dir.join("conversations");
        let pd = root.join("pd");
        fs::create_dir_all(&conv_dir).unwrap();
        fs::create_dir_all(&pd).unwrap();

        let now_ms = crate::live::now_ms();
        let old_at = now_ms - 40 * 86_400_000;
        let old_fname = "00000000-0000-4000-8000-000000000031.db";
        let new_fname = "00000000-0000-4000-8000-000000000032.db";
        write_cli_db(&conv_dir.join(old_fname), old_at, "req-old");
        write_cli_db(&conv_dir.join(new_fname), now_ms, "req-new");
        let old_mtime = std::time::UNIX_EPOCH + std::time::Duration::from_millis(old_at as u64);
        fs::File::options().write(true).open(conv_dir.join(old_fname)).unwrap().set_modified(old_mtime).unwrap();

        // The first (full) scan aggregates the 40-day-old DB, then drops it from memory.
        let mut scanner = CliScanner::default();
        let delta = scanner.scan_delta(&cli_dir, &pd, None, now_ms).unwrap();
        let old_day = local_day_str(old_at);
        assert_eq!(delta.agg[&old_day].0[&neon_key()].calls, 1);
        assert!(!scanner.cache.contains_key(old_fname));
        assert!(scanner.cache.contains_key(new_fname));
        assert!(scanner.index.files[old_fname].d.contains(&old_day));

        // Day lookups still find it through `cli-index.json`, and keep it in the shared old-DB cache.
        let mut snap = delta.snapshot.clone();
        let calls = snap.calls_for_day(&cli_dir, &pd, &old_day);
        assert_eq!(calls.iter().filter_map(|c| c.response_id.as_deref()).collect::<Vec<_>>(), vec!["req-old"]);
        assert!(lock_old(&scanner.old).contains(&conv_dir.join(old_fname)));
        assert_eq!(snap.calls_for_day(&cli_dir, &pd, &today_str()).len(), 1);

        // A scan with no DB changes hands out the same shared maps (cheap snapshot clones).
        let bf = CliBackfillMeta { version: CLI_BF_V, at_ms: now_ms };
        let again = scanner.scan_delta(&cli_dir, &pd, Some(&bf), now_ms).unwrap();
        assert!(Arc::ptr_eq(&delta.snapshot.cache, &again.snapshot.cache));
        assert!(Arc::ptr_eq(&delta.snapshot.index, &again.snapshot.index));
        assert!(Arc::ptr_eq(&snap.old, &again.snapshot.old));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_cli_titles_preferring_summaries_db_over_json() {
        let dir = std::env::temp_dir().join(format!("agy-cli-titles-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("cache")).unwrap();

        let id1 = "00000000-0000-4000-8000-000000000001";
        let id2 = "00000000-0000-4000-8000-000000000002";
        fs::write(
            dir.join("cache/conversation_metadata.json"),
            serde_json::json!({
                "conversations": {
                    id1: { "summary": { "Title": "Old JSON Title" } },
                    id2: { "summary": { "Title": "From JSON Only" } }
                }
            })
            .to_string(),
        )
        .unwrap();

        let conn = Connection::open(dir.join("conversation_summaries.db")).unwrap();
        conn.execute("CREATE TABLE conversation_summaries (conversation_id TEXT, title TEXT, preview TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO conversation_summaries VALUES (?1, '', '/plan build usage chart')", [id1]).unwrap();
        drop(conn);

        let titles = read_cli_titles(&dir);
        assert_eq!(titles.get(id1).map(String::as_str), Some("build usage chart"));
        assert_eq!(titles.get(id2).map(String::as_str), Some("From JSON Only"));
        let _ = fs::remove_dir_all(&dir);
    }
}
