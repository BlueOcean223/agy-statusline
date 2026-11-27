//! Reads conversation databases (`<id>.db`, read-only SQLite) from desktop Antigravity and `agy CLI`.
//!
//! The live engine reads conversations the language_server has not loaded from here. Asking the
//! language_server about such a conversation (e.g. `GetCascadeTrajectoryGeneratorMetadata`) makes
//! it load the whole trajectory into memory and keep it there until Antigravity restarts, which
//! adds tens of MB per conversation and grows without bound with many subagents. Usage history and
//! the `agy CLI` scan read the same databases for past days.
//!
//! Field numbers were mapped against the language_server's JSON for the same rows:
//! `gen_metadata.data` = GeneratorMetadata { 1: chatModel, 2: stepIndices, 4: executionId },
//! chatModel { 4: usage, 9: chatStartMetadata { 2: checkpointIndex, 10: contextWindowMetadata
//! { 1: estimatedTokensUsed, 4: maxContextTokens } }, 11: timeToFirstToken, 12: streamingDuration,
//! 19: responseModel, 21: modelDisplayName }, usage { 2: input, 3: output, 4: cacheWrite,
//! 5: cacheRead, 9: thinkingOutput, 10: responseOutput, 11: responseId }.
//! `steps.metadata` = { 1: createdAt { 1: seconds }, 12: executionId }.
//! `trajectory_metadata_blob.data` = { 1: workspace { 1: folderUri, 3: repository { 1: computedName },
//! 4: branchName }, 2: createdTime, 6: rootConversationId, 8: subagentSpec { 1: typeName, 2: role } }.

use crate::call::{Call, ModelId, Tokens};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Changes whenever the database or its write-ahead log is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DbSig {
    pub mtime_ms: i64,
    pub len: u64,
}

pub fn db_path(app_data: &Path, id: &str) -> PathBuf {
    app_data.join("conversations").join(format!("{id}.db"))
}

pub fn signature(app_data: &Path, id: &str) -> Option<DbSig> {
    let db = db_path(app_data, id);
    let main = fs::metadata(&db).ok()?;
    let mut sig = DbSig { mtime_ms: mtime_ms(&main), len: main.len() };
    if let Ok(wal) = fs::metadata(db.with_extension("db-wal")) {
        sig.mtime_ms = sig.mtime_ms.max(mtime_ms(&wal));
        sig.len += wal.len();
    }
    Some(sig)
}

/// A file's modification time in Unix ms, 0 when unknown.
pub(crate) fn mtime_ms(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `<id>.db` names with a UUID stem; returns the stem.
pub fn is_uuid_db(name: &str) -> Option<&str> {
    name.strip_suffix(".db").filter(|stem| crate::transcript::is_uuid(stem))
}

/// What a conversation database says about the conversation itself.
#[derive(Clone, Debug, Default)]
pub struct ConvMeta {
    pub step_count: u64,
    pub created_ms: i64,
    /// The database's last write ([`DbSig::mtime_ms`]).
    pub modified_ms: i64,
    /// Root conversation, for subagents.
    pub root: Option<String>,
    pub sub_type: Option<String>,
    pub sub_role: Option<String>,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub workspace_path: Option<String>,
}

/// All calls of desktop conversation `id`, plus what the database says about the conversation.
/// The calls are undated (`at_ms == 0`): the live engine dates them from the transcript, as it does
/// the language_server's records.
pub fn read(app_data: &Path, id: &str, sig: DbSig) -> Result<(Vec<Call>, ConvMeta), String> {
    let conn = open(&db_path(app_data, id)).map_err(|e| format!("{id}.db: {e}"))?;
    let calls =
        calls_from(&conn, id, crate::call::DESKTOP, None, OnStepError::Fail).map_err(|e| format!("{id}.db: {e}"))?;
    let step_count = conn.query_row("SELECT COUNT(*) FROM steps", [], |r| r.get::<_, i64>(0)).unwrap_or(0).max(0);
    let meta: Option<Vec<u8>> =
        conn.query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get(0)).ok().flatten();
    let mut out = meta.as_deref().map(|m| meta_from_pb(id, m)).unwrap_or_default();
    out.step_count = step_count as u64;
    out.modified_ms = sig.mtime_ms;
    Ok((calls, out))
}

/// One call per generator row, in language_server order (row `idx`), so index `i` here is record
/// `i` there. Rows without usage are kept ([`Call::has_usage`] is false); calls whose steps carry
/// no time have `at_ms == 0`. If SQLite fails partway through, the rows read before are returned.
pub fn read_calls(path: &Path, conv_id: &str, source: &'static str) -> Result<Vec<Call>, String> {
    let conn = open(path).map_err(|e| e.to_string())?;
    let times = StepTimes::read(&conn);
    calls_from(&conn, conv_id, source, Some(&times), OnStepError::KeepRead).map_err(|e| e.to_string())
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
    Ok(conn)
}

/// What [`calls_from`] does when SQLite fails partway through the table.
#[derive(Clone, Copy, PartialEq)]
enum OnStepError {
    /// Fail the read: the live engine must not take a partial table for the whole conversation.
    Fail,
    /// Keep the rows read so far: history and the CLI scan count what they can.
    KeepRead,
}

/// The calls in `conn`, dated from `times` (left at 0 without). A row whose value is not a blob
/// reads as a call without usage, so row `i` stays call `i` and the other rows still count.
fn calls_from(
    conn: &Connection,
    conv_id: &str,
    source: &'static str,
    times: Option<&StepTimes>,
    on_step_error: OnStepError,
) -> rusqlite::Result<Vec<Call>> {
    // Fall back to storage order for a schema without `idx` (only the live engine needs the order).
    let mut stmt = conn
        .prepare("SELECT data FROM gen_metadata ORDER BY idx")
        .or_else(|_| conn.prepare("SELECT data FROM gen_metadata"))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(e) if on_step_error == OnStepError::Fail => return Err(e),
            Err(_) => break,
        };
        let data = row.get::<_, Option<Vec<u8>>>(0).ok().flatten().unwrap_or_default();
        let mut c = call_from_pb(&data);
        c.conv_id = conv_id.to_string();
        c.source = source;
        if let Some(times) = times {
            let exec = pb_bytes(&data, 4).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("");
            c.at_ms = times.of(&c.steps, exec);
        }
        out.push(c);
    }
    Ok(out)
}

/// Creation time of each step, in whole seconds (as ms).
#[derive(Default)]
struct StepTimes {
    by_step: HashMap<u64, i64>,
    /// First step time per execution, for generator rows whose own steps carry no time.
    by_exec: HashMap<String, i64>,
}

impl StepTimes {
    fn read(conn: &Connection) -> Self {
        let mut out = Self::default();
        let Ok(mut stmt) = conn.prepare("SELECT idx, metadata FROM steps") else { return out };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))) else {
            return out;
        };
        for (idx, meta) in rows.flatten() {
            let Some(meta) = meta else { continue };
            let sec = pb_bytes(&meta, 1).and_then(|ts| pb_u64(ts, 1)).unwrap_or(0);
            if sec == 0 {
                continue;
            }
            let ms = (sec as i64) * 1000;
            if idx >= 0 {
                out.by_step.insert(idx as u64, ms);
            }
            if let Some(ex) = pb_bytes(&meta, 12).and_then(|b| std::str::from_utf8(b).ok()) {
                out.by_exec.entry(ex.to_string()).or_insert(ms);
            }
        }
        out
    }

    fn of(&self, steps: &[u64], exec: &str) -> i64 {
        steps.iter().find_map(|s| self.by_step.get(s).copied()).or_else(|| self.by_exec.get(exec).copied()).unwrap_or(0)
    }
}

fn str_field(buf: &[u8], field: u64) -> Option<String> {
    pb_bytes(buf, field).and_then(|b| std::str::from_utf8(b).ok()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// The record the language_server would return as JSON for this row, without conversation,
/// source or time.
fn call_from_pb(data: &[u8]) -> Call {
    let cm = pb_bytes(data, 1).unwrap_or_default();
    let usage = pb_bytes(cm, 4).unwrap_or_default();
    let u = |f| pb_u64(usage, f).unwrap_or(0);
    let start = pb_bytes(cm, 9).unwrap_or_default();
    let ctx = pb_bytes(start, 10).unwrap_or_default();
    let steps = match pb_get(data, 2) {
        Some(PbVal::Bytes(b)) => pb_varints(b),
        Some(PbVal::Varint(v)) => vec![v],
        None => Vec::new(),
    };
    Call {
        model: ModelId::new(&str_field(cm, 19).unwrap_or_default(), &str_field(cm, 21).unwrap_or_default()),
        tokens: Tokens {
            input: u(2),
            cache_read: u(5),
            cache_write: u(4),
            // Some records only fill the thinking/response split.
            output: u(3).max(u(9).saturating_add(u(10))),
        },
        ctx_used: pb_u64(ctx, 1).unwrap_or(0),
        ctx_max: pb_u64(ctx, 4).unwrap_or(0),
        // int32 -1 is stored as a 64-bit two's-complement varint.
        checkpoint: pb_u64(start, 2).map(|v| v as i64).unwrap_or(-1),
        ttft_ms: pb_bytes(cm, 11).and_then(pb_duration_ms),
        stream_ms: pb_bytes(cm, 12).and_then(pb_duration_ms),
        steps,
        response_id: str_field(usage, 11),
        ..Default::default()
    }
}

fn meta_from_pb(id: &str, meta: &[u8]) -> ConvMeta {
    let ws = pb_bytes(meta, 1).unwrap_or_default();
    let folder = str_field(ws, 1);
    let spec = pb_bytes(meta, 8);
    let created = pb_bytes(meta, 2).map(|ts| {
        let sec = pb_u64(ts, 1).unwrap_or(0) as i64;
        let nanos = pb_u64(ts, 2).unwrap_or(0) as i64;
        sec * 1000 + nanos / 1_000_000
    });
    ConvMeta {
        created_ms: created.unwrap_or(0),
        root: str_field(meta, 6).filter(|r| r != id),
        sub_type: spec.and_then(|s| str_field(s, 1)),
        sub_role: spec.and_then(|s| str_field(s, 2)),
        workspace: pb_bytes(ws, 3)
            .and_then(|r| str_field(r, 1))
            .or_else(|| folder.as_deref().and_then(|u| u.rsplit('/').next()).map(str::to_string)),
        branch: str_field(ws, 4),
        workspace_path: folder.as_deref().and_then(crate::git::uri_to_path),
        ..Default::default()
    }
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

/// Builds conversation databases for tests by field name, so tests don't spell field numbers.
#[cfg(test)]
pub(crate) mod fixture {
    use rusqlite::Connection;
    use std::path::Path;

    /// A protobuf message under construction.
    #[derive(Default)]
    pub(crate) struct Msg(Vec<u8>);

    impl Msg {
        fn varint(mut v: u64, out: &mut Vec<u8>) {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        pub(crate) fn u64(mut self, field: u64, v: u64) -> Self {
            Self::varint(field << 3, &mut self.0);
            Self::varint(v, &mut self.0);
            self
        }
        pub(crate) fn bytes(mut self, field: u64, b: &[u8]) -> Self {
            Self::varint((field << 3) | 2, &mut self.0);
            Self::varint(b.len() as u64, &mut self.0);
            self.0.extend_from_slice(b);
            self
        }
        pub(crate) fn msg(self, field: u64, m: Msg) -> Self {
            self.bytes(field, &m.0)
        }
        pub(crate) fn encode(self) -> Vec<u8> {
            self.0
        }
    }

    /// One `gen_metadata` row. Zero fields are left out of the encoding.
    #[derive(Default)]
    pub(crate) struct Gen {
        pub input: u64,
        pub output: u64,
        pub cache_read: u64,
        pub cache_write: u64,
        pub thinking_output: u64,
        pub response_output: u64,
        pub response_id: &'static str,
        pub raw: &'static str,
        pub display: &'static str,
        pub steps: Vec<u64>,
        pub exec: &'static str,
        /// `(seconds, nanos)`.
        pub ttft: Option<(u64, u64)>,
        pub stream: Option<(u64, u64)>,
        pub ctx_used: u64,
        pub ctx_max: u64,
        pub checkpoint: Option<i64>,
        /// Leave the usage message out entirely.
        pub no_usage: bool,
    }

    impl Gen {
        pub(crate) fn encode(&self) -> Vec<u8> {
            let opt = |m: Msg, f: u64, v: u64| if v > 0 { m.u64(f, v) } else { m };
            let text = |m: Msg, f: u64, s: &str| if s.is_empty() { m } else { m.bytes(f, s.as_bytes()) };
            let dur = |(s, n): (u64, u64)| Msg::default().u64(1, s).u64(2, n);

            let mut cm = Msg::default();
            if !self.no_usage {
                let mut u = Msg::default();
                u = opt(u, 2, self.input);
                u = opt(u, 3, self.output);
                u = opt(u, 4, self.cache_write);
                u = opt(u, 5, self.cache_read);
                u = opt(u, 9, self.thinking_output);
                u = opt(u, 10, self.response_output);
                u = text(u, 11, self.response_id);
                cm = cm.msg(4, u);
            }
            let mut ctx = Msg::default();
            ctx = opt(ctx, 1, self.ctx_used);
            ctx = opt(ctx, 4, self.ctx_max);
            let mut start = Msg::default();
            if let Some(cp) = self.checkpoint {
                start = start.u64(2, cp as u64);
            }
            cm = cm.msg(9, start.msg(10, ctx));
            if let Some(t) = self.ttft {
                cm = cm.msg(11, dur(t));
            }
            if let Some(t) = self.stream {
                cm = cm.msg(12, dur(t));
            }
            cm = text(cm, 19, self.raw);
            cm = text(cm, 21, self.display);

            let mut packed = Msg::default();
            for &s in &self.steps {
                Msg::varint(s, &mut packed.0);
            }
            let row = Msg::default().msg(1, cm).msg(2, packed);
            text(row, 4, self.exec).encode()
        }
    }

    /// One `steps` row.
    pub(crate) struct Step {
        pub idx: i64,
        pub at_s: u64,
        pub exec: &'static str,
    }

    /// Writes a conversation database with these rows (and optional trajectory metadata).
    pub(crate) fn write_db(path: &Path, steps: &[Step], gens: &[Gen], meta: Option<Msg>) {
        let conn = Connection::open(path).unwrap();
        conn.execute("CREATE TABLE steps (idx INTEGER, metadata BLOB)", []).unwrap();
        conn.execute("CREATE TABLE gen_metadata (idx INTEGER, data BLOB)", []).unwrap();
        conn.execute("CREATE TABLE trajectory_metadata_blob (data BLOB)", []).unwrap();
        for s in steps {
            let mut m = Msg::default().msg(1, Msg::default().u64(1, s.at_s));
            if !s.exec.is_empty() {
                m = m.bytes(12, s.exec.as_bytes());
            }
            conn.execute("INSERT INTO steps VALUES (?1, ?2)", rusqlite::params![s.idx, m.encode()]).unwrap();
        }
        for (i, g) in gens.iter().enumerate() {
            conn.execute("INSERT INTO gen_metadata VALUES (?1, ?2)", rusqlite::params![i as i64, g.encode()]).unwrap();
        }
        if let Some(m) = meta {
            conn.execute("INSERT INTO trajectory_metadata_blob VALUES (?1)", [m.encode()]).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Gen, Msg, Step, write_db};
    use super::*;

    fn temp_db(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agy-convdb-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("00000000-0000-4000-8000-000000000001.db");
        (dir, path)
    }

    #[test]
    fn decodes_generator_record_like_the_language_server_json() {
        let g = Gen {
            input: 2550,
            output: 228,
            cache_read: 138_001,
            thinking_output: 41,
            response_output: 187,
            response_id: "resp-1",
            raw: "gemini-4-neon",
            display: "Gemini 4 Neon (XHigh)",
            steps: vec![101],
            ttft: Some((8, 55_828_166)),
            ctx_used: 137_214,
            ctx_max: 512_000,
            checkpoint: Some(-1),
            ..Default::default()
        };
        let c = call_from_pb(&g.encode());
        assert_eq!(c.tokens, Tokens { input: 2550, output: 228, cache_read: 138_001, cache_write: 0 });
        assert_eq!((c.ctx_used, c.ctx_max, c.checkpoint), (137_214, 512_000, -1));
        assert_eq!(c.steps, vec![101]);
        assert_eq!(c.model, ModelId::new("gemini-4-neon", "Gemini 4 Neon (XHigh)"));
        assert_eq!(c.response_id.as_deref(), Some("resp-1"));
        assert!(c.has_usage());
        assert!((c.ttft_ms.unwrap() - 8055.828166).abs() < 1e-6);
        assert_eq!(c.at_ms, 0);
    }

    #[test]
    fn output_falls_back_to_thinking_plus_response() {
        let c =
            call_from_pb(&Gen { output: 50, thinking_output: 40, response_output: 30, ..Default::default() }.encode());
        assert_eq!(c.tokens.output, 70);
    }

    #[test]
    fn record_without_usage_has_no_usage() {
        let c = call_from_pb(&Gen { display: "Gemini", no_usage: true, ..Default::default() }.encode());
        assert!(!c.has_usage());
        assert_eq!(c.checkpoint, -1);
    }

    #[test]
    fn read_calls_keeps_every_row_in_order_and_dates_from_steps() {
        let (dir, path) = temp_db("read");
        let t = 1_795_500_000u64;
        write_db(
            &path,
            &[Step { idx: 2, at_s: t, exec: "exec-a" }, Step { idx: 5, at_s: t + 60, exec: "exec-b" }],
            &[
                Gen { input: 1000, steps: vec![2], exec: "exec-a", response_id: "r1", ..Default::default() },
                Gen { no_usage: true, steps: vec![3], ..Default::default() },
                // Its own step has no time: dated by the first step of its execution.
                Gen { input: 10, steps: vec![9], exec: "exec-b", ..Default::default() },
                Gen { input: 20, steps: vec![99], ..Default::default() },
            ],
            None,
        );
        let calls = read_calls(&path, "conv", crate::call::CLI).unwrap();
        assert_eq!(calls.len(), 4, "rows line up with the language_server's records");
        assert_eq!(
            calls.iter().map(|c| c.at_ms).collect::<Vec<_>>(),
            vec![t as i64 * 1000, 0, (t as i64 + 60) * 1000, 0]
        );
        assert_eq!(calls.iter().map(Call::has_usage).collect::<Vec<_>>(), vec![true, false, true, true]);
        assert!(calls.iter().all(|c| c.conv_id == "conv" && c.source == crate::call::CLI));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_row_that_is_not_a_blob_reads_as_a_call_without_usage() {
        let (dir, path) = temp_db("bad-row");
        write_db(&path, &[], &[Gen { input: 1000, ..Default::default() }], None);
        let conn = Connection::open(&path).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES (1, 'oops')", []).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES (2, ?1)", [Gen { input: 20, ..Default::default() }.encode()])
            .unwrap();
        drop(conn);
        let calls = read_calls(&path, "conv", crate::call::CLI).unwrap();
        assert_eq!(calls.iter().map(|c| c.tokens.input).collect::<Vec<_>>(), vec![1000, 0, 20]);
        assert!(!calls[1].has_usage());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_table_without_idx_is_read_in_storage_order() {
        let (dir, path) = temp_db("no-idx");
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE steps (idx INTEGER, metadata BLOB)", []).unwrap();
        conn.execute("CREATE TABLE gen_metadata (data BLOB)", []).unwrap();
        for input in [7, 9] {
            conn.execute("INSERT INTO gen_metadata VALUES (?1)", [Gen { input, ..Default::default() }.encode()])
                .unwrap();
        }
        drop(conn);
        let calls = read_calls(&path, "conv", crate::call::CLI).unwrap();
        assert_eq!(calls.iter().map(|c| c.tokens.input).collect::<Vec<_>>(), vec![7, 9]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_meta_from_trajectory_metadata() {
        let ws = Msg::default()
            .bytes(1, b"file:///Users/me/code/repo")
            .msg(3, Msg::default().bytes(1, b"owner/repo"))
            .bytes(4, b"main");
        let meta = Msg::default()
            .msg(1, ws)
            .msg(2, Msg::default().u64(1, 1_795_079_117).u64(2, 798_444_000))
            .bytes(6, b"root-id")
            .msg(8, Msg::default().bytes(1, b"research").bytes(2, b"Pi Session Naming Researcher"))
            .encode();

        let s = meta_from_pb("sub-id", &meta);
        assert_eq!(s.root.as_deref(), Some("root-id"));
        assert_eq!(s.sub_type.as_deref(), Some("research"));
        assert_eq!(s.sub_role.as_deref(), Some("Pi Session Naming Researcher"));
        assert_eq!(s.workspace.as_deref(), Some("owner/repo"));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.workspace_path.as_deref(), Some("/Users/me/code/repo"));
        assert_eq!(s.created_ms, 1_795_079_117_798);

        // A top-level conversation names itself as root.
        assert_eq!(meta_from_pb("sub-id", &Msg::default().bytes(6, b"sub-id").encode()).root, None);
    }
}
