//! Read-only scanner for `agy CLI` (`~/.gemini/antigravity-cli/conversations/*.db`) usage,
//! titles (`conversation_summaries.db` + `cache/conversation_metadata.json`), and day-indexed
//! call lookup (`cli-index.json`).
//!
//! `agy CLI` shares the same Antigravity account quota as the desktop app, so including its SQLite
//! call records keeps today's spend, daily history, weekly `cap ≈`, and call breakdowns accurate.

use crate::config::{ReadFile, read_json_keep, write_atomic};
use crate::usage::{
    CallRecord, HISTORY_KEEP_DAYS, ModelAgg, UsageHistory, day_start_ms, is_uuid_db, local_day_str, read_conv_db,
    today_str,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

const CLI_BF_V: u32 = 3;
const CLI_CALLS_KEEP_MS: i64 = 32 * 86_400_000;
const CLI_OLD_MAX: usize = 100;

/// Returns the latest modification timestamp (ms) across `<db>` and `<db>-wal`.
/// In SQLite WAL mode, recent writes only bump `-wal`'s mtime until a checkpoint runs.
pub fn db_mtime_ms(path: &Path) -> i64 {
    let mut best = file_mtime_ms(path);
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    best = best.max(file_mtime_ms(Path::new(&wal)));
    best
}

fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
#[derive(Clone, Default)]
pub struct CliSnapshot {
    cache: HashMap<String, (i64, Arc<Vec<CallRecord>>)>,
    old_cache: HashMap<String, (i64, Arc<Vec<CallRecord>>)>,
    old_order: VecDeque<String>,
    index: CliIndex,
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
            self.index = idx;
        }
    }

    fn load_old_db(&mut self, conv_dir: &Path, fname: &str) -> Option<Arc<Vec<CallRecord>>> {
        let cid = is_uuid_db(fname)?;
        let path = conv_dir.join(fname);
        let mt = db_mtime_ms(&path);
        if mt <= 0 {
            return None;
        }
        let fresh = self.old_cache.get(fname).is_some_and(|(old_mt, _)| *old_mt == mt);
        if !fresh {
            let calls = Arc::new(read_conv_db(&path, cid, "cli").ok()?);
            self.old_cache.insert(fname.to_string(), (mt, calls));
            self.old_order.retain(|k| k != fname);
            self.old_order.push_back(fname.to_string());
            while self.old_order.len() > CLI_OLD_MAX {
                if let Some(evict) = self.old_order.pop_front() {
                    self.old_cache.remove(&evict);
                }
            }
        }
        self.old_cache.get(fname).map(|(_, v)| Arc::clone(v))
    }

    /// Returns all deduplicated CLI calls occurring on `date` (`YYYY-MM-DD`).
    pub fn calls_for_day(&mut self, cli_dir: &Path, plugin_data: &Path, date: &str) -> Vec<CallRecord> {
        self.ensure_index_readonly(plugin_data);
        let conv_dir = cli_dir.join("conversations");
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for (_, calls) in self.cache.values() {
            for c in calls.iter() {
                if local_day_str(c.at_ms) == date && seen.insert(c.dedup_key.clone()) {
                    out.push(c.clone());
                }
            }
        }

        // Consult `cli-index.json` for older DBs (>32 days) that touched `date`.
        let extra_files: Vec<String> = self
            .index
            .files
            .iter()
            .filter(|(fname, ent)| !self.cache.contains_key(*fname) && ent.d.iter().any(|d| d == date))
            .map(|(fname, _)| fname.clone())
            .collect();
        for fname in extra_files {
            if let Some(calls) = self.load_old_db(&conv_dir, &fname) {
                for c in calls.iter() {
                    if local_day_str(c.at_ms) == date && seen.insert(c.dedup_key.clone()) {
                        out.push(c.clone());
                    }
                }
            }
        }

        out
    }
}

#[derive(Default)]
pub struct CliScanner {
    /// Recent (<= 32 days modified) CLI DBs cached in memory: filename -> (mtime_ms, calls).
    cache: HashMap<String, (i64, Arc<Vec<CallRecord>>)>,
    index: CliIndex,
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
                self.index = idx;
                self.index_writable = true;
            }
            ReadFile::Ok(_) | ReadFile::Missing | ReadFile::Corrupt { kept: true } => {
                self.index = CliIndex { v: 1, files: HashMap::new() };
                self.index_writable = true;
            }
            ReadFile::Unreadable | ReadFile::Corrupt { kept: false } => {
                self.index = CliIndex { v: 1, files: HashMap::new() };
                self.index_writable = false;
            }
        }
    }

    fn save_index(&self, plugin_data: &Path) {
        if !self.index_writable {
            return;
        }
        let path = plugin_data.join("cli-index.json");
        if let Ok(text) = serde_json::to_string(&self.index) {
            let _ = write_atomic(&path, &text);
        }
    }

    pub fn snapshot(&self) -> CliSnapshot {
        CliSnapshot {
            cache: self.cache.clone(),
            old_cache: HashMap::new(),
            old_order: VecDeque::new(),
            index: self.index.clone(),
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
        self.cache.retain(|k, _| present.contains(k.as_str()));
        let mut index_dirty = false;
        self.index.files.retain(|k, _| {
            let keep = present.contains(k.as_str());
            if !keep {
                index_dirty = true;
            }
            keep
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
            let path = conv_dir.join(fname);
            let mt = db_mtime_ms(&path);
            if mt <= 0 || mt < scan_cut {
                continue;
            }
            if self.cache.get(fname).is_some_and(|(old_mt, _)| *old_mt == mt) {
                continue;
            }
            match read_conv_db(&path, cid, "cli") {
                Ok(calls) => {
                    let mut days: Vec<String> =
                        calls.iter().map(|c| local_day_str(c.at_ms)).collect::<HashSet<_>>().into_iter().collect();
                    days.sort();
                    self.index.files.insert(fname.clone(), CliIndexEntry { m: mt, d: days });
                    self.cache.insert(fname.clone(), (mt, Arc::new(calls)));
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
        let mut seen: HashSet<String> = HashSet::new();
        for (mt, calls) in self.cache.values() {
            if *mt < from_ms {
                continue;
            }
            for c in calls.iter() {
                if c.at_ms < from_ms {
                    continue;
                }
                if !seen.insert(c.dedup_key.clone()) {
                    continue;
                }
                let day = local_day_str(c.at_ms);
                let model_key = if !c.model_raw.is_empty() {
                    c.model_raw.clone()
                } else if !c.model_display.is_empty() {
                    c.model_display.clone()
                } else {
                    "unknown".to_string()
                };
                let (raw, convs) = raw_agg.entry(day).or_default();
                raw.entry(model_key).or_default().add(c.input, c.cache_read, c.cache_write, c.output);
                convs.insert(c.conv_id.clone());
            }
        }
        let agg = raw_agg.into_iter().map(|(d, (raw, convs))| (d, (raw, convs.len()))).collect();

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
                let old_calls: u64 = entry.cli_raw.values().map(|a| a.calls).sum();
                let new_calls: u64 = cli_raw.values().map(|a| a.calls).sum();
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
        let conn = Connection::open(conv_dir.join(format!("{good_id}.db"))).unwrap();
        conn.execute("CREATE TABLE steps (idx INTEGER, metadata BLOB)", []).unwrap();
        conn.execute("CREATE TABLE gen_metadata (data BLOB)", []).unwrap();
        let meta = [fb(1, &fv(1, now_s)), fb(12, b"exec-1")].concat();
        conn.execute("INSERT INTO steps VALUES (1, ?1)", [&meta]).unwrap();
        let usage = [fv(2, 1000), fv(3, 200), fb(11, b"cli-req-1")].concat();
        let cm = [fb(4, &usage), fb(19, b"gemini-4-neon")].concat();
        let row = [fb(1, &cm), fb(2, &varint(1)), fb(4, b"exec-1")].concat();
        conn.execute("INSERT INTO gen_metadata VALUES (?1)", [&row]).unwrap();
        drop(conn);

        let mut scanner = CliScanner::default();
        let mut h = UsageHistory { v: 2, live_from: today_str(), writable: true, ..Default::default() };
        let delta = scanner.scan_delta(&cli_dir, &pd, h.cli_bf.as_ref(), now_ms).unwrap();
        assert!(delta.partial_errors);
        assert!(apply_cli_delta(&mut h, delta));

        let today_rec = h.days.get(&today_str()).expect("healthy DB should populate today");
        assert_eq!(today_rec.cli_raw["gemini-4-neon"].calls, 1);
        assert_eq!(today_rec.cli_convs, 1);
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
