//! Lifecycle hook receiver. Hooks run synchronously inside the agent loop, so this only forwards
//! `{event, conversationId}` to the sidecar over UDP (fire-and-forget) and answers immediately.
//! All numbers come from the language_server API in the sidecar.

use crate::config::{find_app_data_dir, find_plugin_data_dir};
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::net::UdpSocket;

fn is_valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn poke_sidecar(event: &str, conv: &str) {
    let plugin_data = find_plugin_data_dir(&find_app_data_dir());
    let Ok(port) = std::fs::read_to_string(crate::live::port_file(&plugin_data)) else { return };
    let Ok(port) = port.trim().parse::<u16>() else { return };
    let Ok(sock) = UdpSocket::bind("127.0.0.1:0") else { return };
    let msg = json!({ "e": event, "c": conv }).to_string();
    let _ = sock.send_to(msg.as_bytes(), ("127.0.0.1", port));
}

pub fn run_hook() {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    let event: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let event_name = event["hookEventName"].as_str().unwrap_or("");
    let conv_id = event["conversationId"].as_str().unwrap_or("");

    if is_valid_id(conv_id) {
        poke_sidecar(event_name, conv_id);
    }

    // Stop expects a decision; every other event expects an empty object.
    let out = if event_name == "Stop" { r#"{"decision":"allow_stop"}"# } else { "{}" };
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
}
