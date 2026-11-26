//! Local files that the language_server API doesn't expose:
//! - `brain/<id>/.system_generated/logs/transcript.jsonl`: per-step `created_at` (to date model calls)
//!   and the tool calls each planner step made (to label a turn). Read incrementally from a byte offset.
//! - `annotations/<id>.pbtxt`: the conversation title and when the user last looked at it.

use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Debug, Default)]
pub struct StepMarkers {
    pub user: HashSet<u64>,
    pub ckpt: HashSet<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct StepInfo {
    pub at_ms: i64,
    /// Set only on planner steps: what the model did with this response.
    pub label: Option<String>,
}

#[derive(Default)]
pub struct Transcript {
    path: PathBuf,
    offset: u64,
    pub steps: HashMap<u64, StepInfo>,
    pub markers: StepMarkers,
    /// Candidate subagent conversation ids seen in this transcript (used only as load hints;
    /// actual parent-child ownership still comes from `GetAllCascadeTrajectories`).
    pub subagent_hints: HashSet<String>,
}

impl Transcript {
    pub fn new(app_data: &Path, id: &str) -> Self {
        Self {
            path: app_data.join("brain").join(id).join(".system_generated/logs/transcript.jsonl"),
            ..Default::default()
        }
    }

    pub fn mtime_ms(&self) -> i64 {
        file_mtime_ms(&self.path)
    }

    /// Reads lines appended since the last call. Returns true if anything new was parsed.
    pub fn sync(&mut self) -> bool {
        let Ok(mut f) = File::open(&self.path) else { return false };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Rewritten from scratch.
            self.offset = 0;
            self.steps.clear();
            self.markers.user.clear();
            self.markers.ckpt.clear();
            self.subagent_hints.clear();
        }
        if len == self.offset || f.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        if f.read_to_end(&mut buf).is_err() {
            return false;
        }
        // Only consume complete lines; a partially written last line is read next time.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return false };
        for line in buf[..end].split(|&b| b == b'\n') {
            let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
            let Some(idx) = v["step_index"].as_u64() else { continue };
            let kind = v["type"].as_str().unwrap_or("");
            if kind == "USER_INPUT" {
                self.markers.user.insert(idx);
            } else if kind == "CHECKPOINT" {
                self.markers.ckpt.insert(idx);
            }
            let at_ms = v["created_at"].as_str().and_then(crate::live::parse_ts).unwrap_or(0);
            let label = (kind == "PLANNER_RESPONSE").then(|| {
                v["tool_calls"]
                    .as_array()
                    .and_then(|tcs| tcs.last())
                    .map(describe_tool_call)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Final reply".into())
            });
            extract_subagent_hints(&v, &mut self.subagent_hints);
            self.steps.insert(idx, StepInfo { at_ms, label });
        }
        self.offset += end as u64 + 1;
        true
    }
}

/// Scans `brain/<id>/.system_generated/logs/transcript.jsonl` for `USER_INPUT` and `CHECKPOINT`
/// step indices (used for historical cache-miss attribution without loading the full transcript).
pub fn scan_step_markers(app_data: &Path, id: &str) -> Option<StepMarkers> {
    if !is_uuid(id) {
        return None;
    }
    let path = app_data.join("brain").join(id).join(".system_generated/logs/transcript.jsonl");
    let f = File::open(&path).ok()?;
    let mut out = StepMarkers::default();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("USER_INPUT") && !line.contains("CHECKPOINT") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(idx) = v["step_index"].as_u64() else { continue };
        match v["type"].as_str() {
            Some("USER_INPUT") => {
                out.user.insert(idx);
            }
            Some("CHECKPOINT") => {
                out.ckpt.insert(idx);
            }
            _ => {}
        }
    }
    Some(out)
}

/// Reads the latest (plus compact history of all) compaction checkpoint continuation messages from
/// `brain/<id>/.system_generated/logs/transcript_full.jsonl` (falling back to `transcript.jsonl`).
pub fn read_checkpoint(app_data: &Path, conv_id: &str) -> Value {
    if !is_uuid(conv_id) {
        return json!({ "conv": conv_id, "err": "无效的对话 ID" });
    }
    let dir = app_data.join("brain").join(conv_id).join(".system_generated").join("logs");
    let mut chosen: Option<PathBuf> = None;
    let mut text = String::new();
    for name in ["transcript_full.jsonl", "transcript.jsonl"] {
        let p = dir.join(name);
        if let Ok(s) = fs::read_to_string(&p) {
            text = s;
            chosen = Some(p);
            break;
        }
    }
    let Some(log_path) = chosen else {
        return json!({ "conv": conv_id, "err": format!("这个对话没有日志文件（{}）", dir.display()) });
    };

    let mut checkpoints: Vec<Value> = Vec::new();
    for line in text.lines() {
        if !line.contains("CHECKPOINT") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v["type"].as_str() == Some("CHECKPOINT")
        {
            checkpoints.push(v);
        }
    }
    if checkpoints.is_empty() {
        return json!({
            "conv": conv_id,
            "err": "这个对话的日志里还没有压缩记录（压缩刚发生时，日志可能晚几秒才写进去）"
        });
    }

    static REQ_LINE: OnceLock<Regex> = OnceLock::new();
    let req_re = REQ_LINE.get_or_init(|| Regex::new(r"(?m)^\d+\.\s").unwrap());

    let parse_entry = |o: &Value| -> (u64, i64, usize, usize, String) {
        let step = o["step_index"].as_u64().unwrap_or(0);
        let at = o["created_at"].as_str().and_then(crate::live::parse_ts).unwrap_or(0);
        let content = o["content"].as_str().unwrap_or("");
        let (before, summary) = match content.find("<summary>") {
            Some(a) => {
                let start = a + "<summary>".len();
                let end = content[start..].find("</summary>").map(|rel| start + rel).unwrap_or(content.len());
                (&content[..a], content[start..end].trim())
            }
            None => (content, ""),
        };
        let requests = req_re.find_iter(before).count();
        let artifacts = content.matches("[ARTIFACT: ").count();
        let clipped: String = summary.chars().take(80_000).collect();
        (step, at, requests, artifacts, clipped)
    };

    let history: Vec<Value> = checkpoints
        .iter()
        .enumerate()
        .map(|(idx, o)| {
            let (step, at, requests, artifacts, _) = parse_entry(o);
            json!({ "index": idx + 1, "step": step, "at": at, "requests": requests, "artifacts": artifacts })
        })
        .collect();
    let (step, at, requests, artifacts, summary) = parse_entry(checkpoints.last().unwrap());

    json!({
        "conv": conv_id,
        "n": checkpoints.len(),
        "step": step,
        "at": at,
        "requests": requests,
        "artifacts": artifacts,
        "summary": summary,
        "log": log_path.to_string_lossy(),
        "history": history,
    })
}

fn extract_subagent_hints(v: &Value, out: &mut HashSet<String>) {
    static CREATED_SUB: OnceLock<Regex> = OnceLock::new();
    static MSG_SENDER: OnceLock<Regex> = OnceLock::new();
    let Some(content) = v["content"].as_str() else { return };
    if v["type"] == "GENERIC"
        && let Some((_, rest)) = content.split_once("Created the following subagent:")
    {
        let re = CREATED_SUB.get_or_init(|| {
            Regex::new(r#""conversationId"\s*:\s*"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})""#)
                .unwrap()
        });
        if let Some(id) = re.captures(rest).and_then(|c| c.get(1)) {
            out.insert(id.as_str().to_string());
        }
    } else if v["type"] == "SYSTEM_MESSAGE" && content.contains("sender=") {
        // Subagent messages use `sender=<uuid> ` (whereas background tasks use `sender=<uuid>/task-N`).
        let re = MSG_SENDER.get_or_init(|| {
            Regex::new(r"sender=([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:\s|$)").unwrap()
        });
        for cap in re.captures_iter(content) {
            if let Some(id) = cap.get(1) {
                out.insert(id.as_str().to_string());
            }
        }
    }
}

fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        serde_json::from_str::<String>(t).unwrap_or_else(|_| t[1..t.len() - 1].to_string())
    } else {
        t.to_string()
    }
}

fn describe_tool_call(tc: &Value) -> String {
    let name = tc["name"].as_str().unwrap_or("");
    if name.is_empty() {
        return String::new();
    }
    if let Some(args) = tc["args"].as_object() {
        for key in ["toolSummary", "Description", "Role"] {
            if let Some(v) = args.get(key).and_then(Value::as_str) {
                let u = unquote(v);
                if !u.is_empty() {
                    return u.chars().take(48).collect();
                }
            }
        }
        for key in ["Path", "TargetFile"] {
            if let Some(v) = args.get(key).and_then(Value::as_str) {
                let u = unquote(v);
                if let Some(fname) = Path::new(&u).file_name().and_then(|f| f.to_str()) {
                    return format!("{name} {fname}");
                }
            }
        }
    }
    name.to_string()
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Conversations on disk with their transcript mtime. After Antigravity restarts, the
/// language_server only lists conversations it has loaded, so this is how older ones are found;
/// querying a conversation's metadata loads it.
pub fn scan_brain(app_data: &Path) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    let Ok(rd) = fs::read_dir(app_data.join("brain")) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !is_uuid(&name) {
            continue;
        }
        let mtime = file_mtime_ms(&e.path().join(".system_generated/logs/transcript.jsonl"));
        out.insert(name, mtime);
    }
    out
}

/// Titles and last-view times of all top-level conversations, re-reading only annotation files
/// whose mtime changed since `prev`.
pub fn scan_annotations(
    app_data: &Path,
    mut prev: HashMap<String, (i64, Annotation)>,
) -> HashMap<String, (i64, Annotation)> {
    let mut out = HashMap::new();
    let Ok(rd) = fs::read_dir(app_data.join("annotations")) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(id) = name.strip_suffix(".pbtxt").filter(|n| is_uuid(n)) else { continue };
        let mtime = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let ann = match prev.remove(id) {
            Some((m, a)) if m == mtime => a,
            _ => read_annotation(app_data, id).unwrap_or_default(),
        };
        out.insert(id.to_string(), (mtime, ann));
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct Annotation {
    pub title: String,
    pub viewed_ms: i64,
}

/// Title and last-viewed time from `annotations/<id>.pbtxt` (protobuf text format).
pub fn read_annotation(app_data: &Path, id: &str) -> Option<Annotation> {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static VIEW: OnceLock<Regex> = OnceLock::new();
    let text = fs::read_to_string(app_data.join("annotations").join(format!("{id}.pbtxt"))).ok()?;
    let title = TITLE
        .get_or_init(|| Regex::new(r#"title:\s*"((?:[^"\\]|\\.)*)""#).unwrap())
        .captures(&text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().replace("\\\"", "\""))
        .unwrap_or_default();
    let viewed_ms = VIEW
        .get_or_init(|| Regex::new(r"last_user_view_time:\s*\{\s*seconds:\s*(\d+)(?:\s+nanos:\s*(\d+))?").unwrap())
        .captures(&text)
        .map(|c| {
            let sec: i64 = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let nanos: i64 = c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            sec * 1000 + nanos / 1_000_000
        })
        .unwrap_or(0);
    Some(Annotation { title, viewed_ms })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_subagent_hints_without_task_ids() {
        let mut hints = HashSet::new();
        let created = json!({
            "type": "GENERIC",
            "content": "Created At: 2026-11-19\nCreated the following subagent:\n{\n  \"conversationId\": \"09209826-31ff-4ac7-85c3-d5de1fad9d1f\"\n}"
        });
        extract_subagent_hints(&created, &mut hints);

        let sub_msg = json!({
            "type": "SYSTEM_MESSAGE",
            "content": "[Message] timestamp=2026-11-19T09:14:32Z sender=1d1ccfdb-56ca-48fb-a613-a38ffe68f9e4 priority=HIGH"
        });
        extract_subagent_hints(&sub_msg, &mut hints);

        let task_msg = json!({
            "type": "SYSTEM_MESSAGE",
            "content": "[Message] timestamp=2026-11-19T09:09:26Z sender=64259309-8ea1-426a-9a74-5b0fce55ad48/task-30 priority=HIGH"
        });
        extract_subagent_hints(&task_msg, &mut hints);

        assert!(hints.contains("09209826-31ff-4ac7-85c3-d5de1fad9d1f"));
        assert!(hints.contains("1d1ccfdb-56ca-48fb-a613-a38ffe68f9e4"));
        assert!(!hints.contains("64259309-8ea1-426a-9a74-5b0fce55ad48"));
    }

    #[test]
    fn reads_latest_checkpoint_and_scans_step_markers() {
        let root = std::env::temp_dir().join(format!("agy-ckpt-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let conv = "00000000-0000-4000-8000-000000000001";
        let log_dir = root.join("brain").join(conv).join(".system_generated/logs");
        fs::create_dir_all(&log_dir).unwrap();

        let mk_ck = |step: u64, reqs: usize, sum: &str| {
            let req_block = (1..=reqs).map(|i| format!("{i}. request {i}")).collect::<Vec<_>>().join("\n");
            json!({
                "step_index": step,
                "type": "CHECKPOINT",
                "created_at": "2026-11-24T12:49:27Z",
                "content": format!("# Resuming from a compaction\n\n# User Requests\n{req_block}\n<summary>\n{sum}\n</summary>\n\n[ARTIFACT: a]\nPath: x\n\n[ARTIFACT: b]\nPath: y\n")
            })
            .to_string()
        };

        let full_text = [
            json!({ "step_index": 0, "type": "USER_INPUT", "content": "hi" }).to_string(),
            mk_ck(212, 2, "old summary"),
            json!({ "step_index": 300, "type": "PLANNER_RESPONSE", "content": "x" }).to_string(),
            mk_ck(702, 3, "1. **Task Overview**\n   - done"),
        ]
        .join("\n")
            + "\n";
        fs::write(log_dir.join("transcript_full.jsonl"), &full_text).unwrap();
        fs::write(log_dir.join("transcript.jsonl"), &full_text).unwrap();

        let out = read_checkpoint(&root, conv);
        assert_eq!(out["n"], 2);
        assert_eq!(out["step"], 702);
        assert_eq!(out["requests"], 3);
        assert_eq!(out["artifacts"], 2);
        assert_eq!(out["summary"], "1. **Task Overview**\n   - done");
        assert!(out["log"].as_str().unwrap().ends_with("transcript_full.jsonl"));
        assert_eq!(out["history"].as_array().unwrap().len(), 2);

        let markers = scan_step_markers(&root, conv).unwrap();
        assert!(markers.user.contains(&0));
        assert!(markers.ckpt.contains(&212));
        assert!(markers.ckpt.contains(&702));

        let missing = read_checkpoint(&root, "00000000-0000-4000-8000-000000000002");
        assert!(missing["err"].as_str().unwrap().contains("没有日志文件"));

        let bad_id = read_checkpoint(&root, "../etc/passwd");
        assert!(bad_id["err"].as_str().unwrap().contains("无效"));

        let _ = fs::remove_dir_all(&root);
    }
}
