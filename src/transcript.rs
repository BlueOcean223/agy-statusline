//! Local files that the language_server API doesn't expose:
//! - `brain/<id>/.system_generated/logs/transcript.jsonl`: per-step `created_at` (to date model calls)
//!   and the tool calls each planner step made (to label a turn). Read incrementally from a byte offset.
//! - `annotations/<id>.pbtxt`: the conversation title and when the user last looked at it.

use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
            let at_ms = v["created_at"].as_str().and_then(crate::live::parse_ts).unwrap_or(0);
            let label = (v["type"] == "PLANNER_RESPONSE").then(|| {
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

/// Top-level conversations that have an annotation file (subagents don't get one).
pub fn annotated_ids(app_data: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(app_data.join("annotations")) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".pbtxt").map(str::to_string))
        .filter(|n| is_uuid(n))
        .collect()
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
}
