//! Git status of a conversation's workspace for the Git segment.

use serde::Serialize;
use std::process::Command;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct GitInfo {
    pub branch: String,
    pub dirty: bool,
    pub ahead: u64,
    pub behind: u64,
    pub added: u64,
    pub deleted: u64,
}

fn git(path: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        // Read-only status must not take the index lock the user's own git commands need.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn read(path: &str) -> Option<GitInfo> {
    let status = git(path, &["status", "--porcelain=v2", "--branch"])?;
    let mut info = GitInfo::default();
    for line in status.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            info.branch = if head == "(detached)" { "detached".into() } else { head.to_string() };
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            info.dirty = true;
        }
    }
    // Lines changed in tracked files vs HEAD (staged and unstaged). Fails on a repo without commits.
    if info.dirty
        && let Some(numstat) = git(path, &["diff", "HEAD", "--numstat"])
    {
        for line in numstat.lines() {
            let mut it = line.split_whitespace();
            info.added += it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            info.deleted += it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        }
    }
    Some(info)
}

/// `file:///Users/x/my%20repo` → `/Users/x/my repo`.
pub fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_uri() {
        assert_eq!(uri_to_path("file:///Users/a/my%20repo").as_deref(), Some("/Users/a/my repo"));
        assert_eq!(uri_to_path("https://x"), None);
    }

    #[test]
    fn reads_this_repo() {
        let info = read(env!("CARGO_MANIFEST_DIR")).expect("repo");
        assert!(!info.branch.is_empty());
    }
}
