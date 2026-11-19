use crate::config::{find_app_data_dir, find_plugin_data_dir, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InferenceSample {
    pub ended_at_ms: i64,
    pub duration_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeState {
    pub v: u32,
    pub conversation_id: String,
    pub updated_at_ms: i64,
    pub model_name: Option<String>,
    pub cwd: Option<String>,
    pub workspace_paths: Vec<String>,
    pub state: String,
    pub req_start_ms: Option<i64>,
    pub req_pending: bool,
    pub last_inference_ms: Option<i64>,
    pub last_inference_ended_ms: Option<i64>,
    pub inference_history: Vec<InferenceSample>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_valid_id(id: &str) -> bool {
    id.len() >= 8
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub fn run_hook() {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    let event: Value = serde_json::from_str(&input).unwrap_or(Value::Null);

    let event_name = event
        .get("hookEventName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let conv_id = event
        .get("conversationId")
        .and_then(Value::as_str)
        .unwrap_or("");

    if is_valid_id(conv_id) {
        let app_data = find_app_data_dir();
        let plugin_data = find_plugin_data_dir(&app_data);
        let bridge_dir = plugin_data.join("bridge");
        let conv_path = bridge_dir.join(format!("{conv_id}.json"));
        let now = now_ms();

        let mut st: BridgeState = fs::read_to_string(&conv_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        st.v = 1;
        st.conversation_id = conv_id.to_string();
        st.updated_at_ms = now;

        if let Some(m) = event.get("modelName").and_then(Value::as_str) {
            let m = m.trim();
            if !m.is_empty() && m != "None" && m != "auto" {
                st.model_name = Some(m.to_string());
            } else if st.model_name.is_none() && !m.is_empty() {
                st.model_name = Some(m.to_string());
            }
        }

        if let Some(cwd) = event.get("cwd").and_then(Value::as_str)
            && !cwd.trim().is_empty()
        {
            st.cwd = Some(cwd.trim().to_string());
        }

        if let Some(arr) = event.get("workspacePaths").and_then(Value::as_array) {
            let paths: Vec<String> = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if !paths.is_empty() {
                st.workspace_paths = paths;
            }
        }

        let finish_inference = |st: &mut BridgeState| {
            if st.req_pending
                && let Some(start) = st.req_start_ms
                && now > start
            {
                let dur = now - start;
                if dur >= 80 {
                    st.last_inference_ms = Some(dur);
                    st.last_inference_ended_ms = Some(now);
                    st.inference_history.push(InferenceSample {
                        ended_at_ms: now,
                        duration_ms: dur,
                    });
                    if st.inference_history.len() > 30 {
                        let drain = st.inference_history.len() - 30;
                        st.inference_history.drain(0..drain);
                    }
                }
            }
            st.req_pending = false;
        };

        match event_name {
            "SessionStart" => {
                st.state = "idle".into();
            }
            "PreInvocation" => {
                st.state = "thinking".into();
                st.req_start_ms = Some(now);
                st.req_pending = true;
            }
            "PostInvocation" => {
                finish_inference(&mut st);
                st.state = "working".into();
            }
            "PostToolUse" => {
                st.state = "working".into();
            }
            "Stop" => {
                finish_inference(&mut st);
                let idle = event
                    .get("fullyIdle")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                st.state = if idle { "idle".into() } else { "working".into() };
            }
            _ => {}
        }

        if let Ok(json) = serde_json::to_string(&st) {
            let _ = write_atomic(&conv_path, &json);
        }
    }

    let out = if event_name == "Stop" {
        r#"{"decision":"allow_stop"}"#
    } else {
        "{}"
    };
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
}
