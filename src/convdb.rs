//! Reads one conversation straight from `<app_data>/conversations/<id>.db` (read-only SQLite).
//!
//! Used for conversations the language_server has not loaded. Asking the language_server about
//! such a conversation (e.g. `GetCascadeTrajectoryGeneratorMetadata`) makes it load the whole
//! trajectory into memory and keep it there until Antigravity restarts, which adds tens of MB
//! per conversation and grows without bound with many subagents.
//!
//! Field numbers were mapped against the language_server's JSON for the same rows:
//! `gen_metadata.data` = GeneratorMetadata { 1: chatModel, 2: stepIndices }, chatModel
//! { 4: usage, 9: chatStartMetadata { 2: checkpointIndex, 10: contextWindowMetadata
//! { 1: estimatedTokensUsed, 4: maxContextTokens } }, 11: timeToFirstToken, 12: streamingDuration,
//! 19: responseModel, 21: modelDisplayName }, usage { 2: input, 3: output, 4: cacheWrite,
//! 5: cacheRead, 9: thinkingOutput, 10: responseOutput }.
//! `trajectory_metadata_blob.data` = { 1: workspace { 1: folderUri, 3: repository { 1: computedName },
//! 4: branchName }, 2: createdTime, 6: rootConversationId, 8: subagentSpec { 1: typeName, 2: role } }.

use crate::live::{Call, Summary};
use crate::usage::{PbVal, pb_bytes, pb_duration_ms, pb_get, pb_u64, pb_varints};
use rusqlite::{Connection, OpenFlags};
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

fn mtime_ms(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Generator records in language_server order (row `idx`), plus a summary built from the
/// trajectory metadata. Calls are undated (`at_ms == 0`); the transcript dates them later.
pub fn read(app_data: &Path, id: &str, sig: DbSig) -> Result<(Vec<Call>, Summary), String> {
    let path = db_path(app_data, id);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&path, flags).map_err(|e| format!("{id}.db: {e}"))?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));

    let mut calls = Vec::new();
    let mut stmt = conn.prepare("SELECT data FROM gen_metadata ORDER BY idx").map_err(|e| format!("{id}.db: {e}"))?;
    let rows = stmt.query_map([], |r| r.get::<_, Option<Vec<u8>>>(0)).map_err(|e| format!("{id}.db: {e}"))?;
    for row in rows {
        let data = row.map_err(|e| format!("{id}.db: {e}"))?.unwrap_or_default();
        calls.push(call_from_pb(&data));
    }
    drop(stmt);

    let step_count = conn.query_row("SELECT COUNT(*) FROM steps", [], |r| r.get::<_, i64>(0)).unwrap_or(0).max(0);
    let meta: Option<Vec<u8>> =
        conn.query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get(0)).ok().flatten();
    let mut summary = meta.as_deref().map(|m| summary_from_pb(id, m)).unwrap_or_default();
    summary.step_count = step_count as u64;
    summary.modified_ms = sig.mtime_ms;
    Ok((calls, summary))
}

fn str_field(buf: &[u8], field: u64) -> Option<String> {
    pb_bytes(buf, field).and_then(|b| std::str::from_utf8(b).ok()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Mirrors `Call::from_json` for a record the language_server never saw this session.
pub(crate) fn call_from_pb(data: &[u8]) -> Call {
    let cm = pb_bytes(data, 1).unwrap_or_default();
    let usage = pb_bytes(cm, 4);
    let u = |f| usage.and_then(|u| pb_u64(u, f)).unwrap_or(0);
    let (input, output_raw, cache_write, cache) = (u(2), u(3), u(4), u(5));
    let output = output_raw.max(u(9).saturating_add(u(10)));
    let start = pb_bytes(cm, 9).unwrap_or_default();
    let ctx = pb_bytes(start, 10).unwrap_or_default();
    let steps = match pb_get(data, 2) {
        Some(PbVal::Bytes(b)) => pb_varints(b),
        Some(PbVal::Varint(v)) => vec![v],
        None => Vec::new(),
    };
    Call {
        display: str_field(cm, 21).unwrap_or_default(),
        resp_model: str_field(cm, 19).unwrap_or_default(),
        input,
        cache,
        cache_write,
        output,
        ctx_used: pb_u64(ctx, 1).unwrap_or(0),
        ctx_max: pb_u64(ctx, 4).unwrap_or(0),
        // int32 -1 is stored as a 64-bit two's-complement varint.
        checkpoint: pb_u64(start, 2).map(|v| v as i64).unwrap_or(-1),
        ttft_ms: pb_bytes(cm, 11).and_then(pb_duration_ms),
        stream_ms: pb_bytes(cm, 12).and_then(pb_duration_ms),
        steps,
        has_usage: usage.is_some() && (input > 0 || cache > 0 || cache_write > 0 || output > 0),
        at_ms: 0,
        dated: false,
        live_seen: false,
        label: None,
        request_id: None,
    }
}

fn summary_from_pb(id: &str, meta: &[u8]) -> Summary {
    let ws = pb_bytes(meta, 1).unwrap_or_default();
    let folder = str_field(ws, 1);
    let spec = pb_bytes(meta, 8);
    let created = pb_bytes(meta, 2).map(|ts| {
        let sec = pb_u64(ts, 1).unwrap_or(0) as i64;
        let nanos = pb_u64(ts, 2).unwrap_or(0) as i64;
        sec * 1000 + nanos / 1_000_000
    });
    Summary {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn vfield(f: u64, v: u64, out: &mut Vec<u8>) {
        varint(f << 3, out);
        varint(v, out);
    }
    fn bfield(f: u64, b: &[u8], out: &mut Vec<u8>) {
        varint((f << 3) | 2, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }

    #[test]
    fn decodes_generator_record_like_the_language_server_json() {
        let mut usage = Vec::new();
        vfield(2, 2550, &mut usage);
        vfield(3, 228, &mut usage);
        vfield(5, 138_001, &mut usage);
        vfield(9, 41, &mut usage);
        vfield(10, 187, &mut usage);
        let mut ctx = Vec::new();
        vfield(1, 137_214, &mut ctx);
        vfield(4, 512_000, &mut ctx);
        let mut start = Vec::new();
        vfield(2, u64::MAX, &mut start); // checkpointIndex = -1
        bfield(10, &ctx, &mut start);
        let mut ttft = Vec::new();
        vfield(1, 8, &mut ttft);
        vfield(2, 55_828_166, &mut ttft);
        let mut cm = Vec::new();
        bfield(4, &usage, &mut cm);
        bfield(9, &start, &mut cm);
        bfield(11, &ttft, &mut cm);
        bfield(19, b"gemini-4-neon", &mut cm);
        bfield(21, b"Gemini 4 Neon (XHigh)", &mut cm);
        let mut data = Vec::new();
        bfield(1, &cm, &mut data);
        bfield(2, &[101], &mut data);

        let c = call_from_pb(&data);
        assert_eq!((c.input, c.output, c.cache, c.cache_write), (2550, 228, 138_001, 0));
        assert_eq!((c.ctx_used, c.ctx_max, c.checkpoint), (137_214, 512_000, -1));
        assert_eq!(c.steps, vec![101]);
        assert_eq!(c.resp_model, "gemini-4-neon");
        assert_eq!(c.display, "Gemini 4 Neon (XHigh)");
        assert!(c.has_usage);
        assert!((c.ttft_ms.unwrap() - 8055.828166).abs() < 1e-6);
        assert_eq!(c.at_ms, 0);
    }

    #[test]
    fn record_without_usage_has_no_usage() {
        let mut cm = Vec::new();
        bfield(21, b"Gemini", &mut cm);
        let mut data = Vec::new();
        bfield(1, &cm, &mut data);
        let c = call_from_pb(&data);
        assert!(!c.has_usage);
        assert_eq!(c.checkpoint, -1);
    }

    #[test]
    fn subagent_summary_from_trajectory_metadata() {
        let mut repo = Vec::new();
        bfield(1, b"owner/repo", &mut repo);
        let mut ws = Vec::new();
        bfield(1, b"file:///Users/me/code/repo", &mut ws);
        bfield(3, &repo, &mut ws);
        bfield(4, b"main", &mut ws);
        let mut ts = Vec::new();
        vfield(1, 1_795_079_117, &mut ts);
        vfield(2, 798_444_000, &mut ts);
        let mut spec = Vec::new();
        bfield(1, b"research", &mut spec);
        bfield(2, b"Pi Session Naming Researcher", &mut spec);
        let mut meta = Vec::new();
        bfield(1, &ws, &mut meta);
        bfield(2, &ts, &mut meta);
        bfield(6, b"root-id", &mut meta);
        bfield(8, &spec, &mut meta);

        let s = summary_from_pb("sub-id", &meta);
        assert_eq!(s.root.as_deref(), Some("root-id"));
        assert_eq!(s.sub_type.as_deref(), Some("research"));
        assert_eq!(s.sub_role.as_deref(), Some("Pi Session Naming Researcher"));
        assert_eq!(s.workspace.as_deref(), Some("owner/repo"));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.workspace_path.as_deref(), Some("/Users/me/code/repo"));
        assert_eq!(s.created_ms, 1_795_079_117_798);
        assert!(!s.active());

        // A top-level conversation names itself as root.
        let mut top = Vec::new();
        bfield(6, b"sub-id", &mut top);
        assert_eq!(summary_from_pb("sub-id", &top).root, None);
    }
}
