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

#[derive(Default)]
pub struct CliScanner {
    /// Recent (<= 32 days modified) CLI DBs cached in memory: filename -> (mtime_ms, calls).
    cache: HashMap<String, (i64, Vec<CallRecord>)>,
    /// Bounded on-demand FIFO cache for older DBs (> 32 days) consulted via `cli-index.json`.
    old_cache: HashMap<String, (i64, Vec<CallRecord>)>,
    old_order: VecDeque<String>,
    index: CliIndex,
    index_loaded: bool,
    index_writable: bool,
    titles_key: String,
    titles_map: HashMap<String, String>,
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

    /// Scans `~/.gemini/antigravity-cli/conversations/*.db` incrementally, updates `cli-index.json`
    /// and `history.days[day].cli_raw` / `cli_convs`. Returns `true` if `history` changed.
    pub fn scan(&mut self, cli_dir: &Path, plugin_data: &Path, h: &mut UsageHistory, now_ms: i64) -> bool {
        let conv_dir = cli_dir.join("conversations");
        let Ok(rd) = fs::read_dir(&conv_dir) else {
            return false;
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
                self.old_cache.remove(k);
                index_dirty = true;
            }
            keep
        });

        let full = h.cli_bf.as_ref().is_none_or(|b| b.version < CLI_BF_V);
        let today = today_str();
        let yest = local_day_str(now_ms - 86_400_000);
        let from_day = if full {
            local_day_str(now_ms - (HISTORY_KEEP_DAYS as i64 - 1) * 86_400_000)
        } else {
            let last_at = h.cli_bf.as_ref().map(|b| b.at_ms).filter(|&t| t > 0).unwrap_or(now_ms);
            yest.min(local_day_str(last_at))
        };
        let from_ms = day_start_ms(&from_day).unwrap_or(now_ms - 86_400_000);
        let keep_from = now_ms - CLI_CALLS_KEEP_MS;
        let scan_cut = from_ms.min(keep_from);

        let mut missing_unreadable = 0usize;
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
                    self.cache.insert(fname.clone(), (mt, calls));
                    self.old_cache.remove(fname);
                    index_dirty = true;
                }
                Err(_) => {
                    if !self.cache.contains_key(fname) && mt >= from_ms {
                        missing_unreadable += 1;
                    }
                }
            }
        }

        if index_dirty {
            self.save_index(plugin_data);
        }

        if missing_unreadable > 0 {
            // Skip overwriting day aggregates if an expected DB failed to open; retry on next tick.
            return false;
        }

        // Aggregate per day from `from_ms` through today.
        let mut agg: BTreeMap<String, (HashMap<String, ModelAgg>, HashSet<String>)> = BTreeMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (mt, calls) in self.cache.values() {
            if *mt < from_ms {
                continue;
            }
            for c in calls {
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
                let (raw, convs) = agg.entry(day).or_default();
                raw.entry(model_key).or_default().add(c.input, c.cache_read, c.cache_write, c.output);
                convs.insert(c.conv_id.clone());
            }
        }

        let mut changed = false;
        let mut t = from_ms + 43_200_000; // Local noon step avoids DST 23h/25h skip bugs.
        loop {
            let d = local_day_str(t);
            if d > today {
                break;
            }
            match agg.remove(&d) {
                Some((cli_raw, convs)) => {
                    let entry = h.days.entry(d.clone()).or_default();
                    let next_convs = convs.len();
                    if entry.cli_raw != cli_raw || entry.cli_convs != next_convs {
                        entry.cli_raw = cli_raw;
                        entry.cli_convs = next_convs;
                        changed = true;
                    }
                }
                None => {
                    if let Some(entry) = h.days.get_mut(&d)
                        && (!entry.cli_raw.is_empty() || entry.cli_convs > 0)
                    {
                        entry.cli_raw.clear();
                        entry.cli_convs = 0;
                        changed = true;
                    }
                }
            }
            t += 86_400_000;
        }

        if full || changed || h.cli_bf.as_ref().is_none_or(|b| now_ms - b.at_ms > 300_000) {
            h.cli_bf = Some(CliBackfillMeta { version: CLI_BF_V, at_ms: now_ms });
            changed = true;
        }

        changed
    }

    fn load_old_db(&mut self, conv_dir: &Path, fname: &str) -> Option<&Vec<CallRecord>> {
        let cid = is_uuid_db(fname)?;
        let path = conv_dir.join(fname);
        let mt = db_mtime_ms(&path);
        if mt <= 0 {
            return None;
        }
        let fresh = self.old_cache.get(fname).is_some_and(|(old_mt, _)| *old_mt == mt);
        if !fresh {
            let calls = read_conv_db(&path, cid, "cli").ok()?;
            self.old_cache.insert(fname.to_string(), (mt, calls));
            self.old_order.retain(|k| k != fname);
            self.old_order.push_back(fname.to_string());
            while self.old_order.len() > CLI_OLD_MAX {
                if let Some(evict) = self.old_order.pop_front() {
                    self.old_cache.remove(&evict);
                }
            }
        }
        self.old_cache.get(fname).map(|(_, v)| v)
    }

    /// Returns all deduplicated CLI calls occurring on `date` (`YYYY-MM-DD`).
    pub fn calls_for_day(&mut self, cli_dir: &Path, plugin_data: &Path, date: &str) -> Vec<CallRecord> {
        self.ensure_index(plugin_data);
        let conv_dir = cli_dir.join("conversations");
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for (_, calls) in self.cache.values() {
            for c in calls {
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
                for c in calls {
                    if local_day_str(c.at_ms) == date && seen.insert(c.dedup_key.clone()) {
                        out.push(c.clone());
                    }
                }
            }
        }

        out
    }

    /// Resolves CLI conversation titles from `conversation_summaries.db` (preferred) and
    /// `cache/conversation_metadata.json`, cached by file mtimes.
    pub fn titles(&mut self, cli_dir: &Path) -> &HashMap<String, String> {
        let meta_json = cli_dir.join("cache").join("conversation_metadata.json");
        let sum_db = cli_dir.join("conversation_summaries.db");
        let key = format!("{}|{}", file_mtime_ms(&meta_json), db_mtime_ms(&sum_db));
        if key == self.titles_key {
            return &self.titles_map;
        }
        self.titles_key = key;
        self.titles_map = read_cli_titles(cli_dir);
        &self.titles_map
    }
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

    #[test]
    fn cleans_slash_command_from_cli_preview() {
        assert_eq!(clean_preview("/plan   refactor  statusline  module"), "refactor statusline module");
        assert_eq!(clean_preview("plain   question\nwith newline"), "plain question with newline");
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
