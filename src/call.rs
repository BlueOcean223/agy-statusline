//! One model call, as read from the language_server or from a conversation database.
//!
//! Both sources describe the same record (see `convdb` for the protobuf field map), so live
//! tracking, usage history and the `agy CLI` scan all share this type. State the live engine keeps
//! about a call (transcript dating, labels) lives in `live::LiveCall`, not here.

use std::collections::{HashMap, HashSet};

/// Desktop Antigravity.
pub const DESKTOP: &str = "desktop";
/// `agy CLI`.
pub const CLI: &str = "cli";

/// Longest model name kept, so a malformed record can't bloat persisted history.
const MODEL_NAME_MAX_CHARS: usize = 64;

/// Backend model name (`raw`, e.g. `gemini-4-neon`) plus the name shown to the user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelId {
    pub raw: String,
    pub display: String,
}

impl ModelId {
    pub fn new(raw: &str, display: &str) -> Self {
        let clip = |s: &str| s.chars().take(MODEL_NAME_MAX_CHARS).collect();
        Self { raw: clip(raw), display: clip(display) }
    }

    /// Lowercased `"raw display"`, the text model rules are matched against.
    pub fn price_key(&self) -> String {
        format!("{} {}", self.raw, self.display).to_lowercase()
    }

    /// Key for daily aggregates: the raw name, else the display name, else `"unknown"`.
    pub fn bucket_key(&self) -> String {
        if !self.raw.is_empty() {
            self.raw.clone()
        } else if !self.display.is_empty() {
            self.display.clone()
        } else {
            "unknown".to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    pub fn any(&self) -> bool {
        self.total() > 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct Call {
    pub conv_id: String,
    /// [`DESKTOP`] or [`CLI`].
    pub source: &'static str,
    pub model: ModelId,
    pub tokens: Tokens,
    /// 0 when unknown.
    pub at_ms: i64,
    pub steps: Vec<u64>,
    pub ttft_ms: Option<f64>,
    pub stream_ms: Option<f64>,
    pub ctx_used: u64,
    pub ctx_max: u64,
    pub checkpoint: i64,
    /// The provider's response ID; the same value in the language_server and in the database.
    pub response_id: Option<String>,
}

impl Call {
    /// Aborted or placeholder records carry no tokens at all.
    pub fn has_usage(&self) -> bool {
        self.tokens.any()
    }

    pub fn step(&self) -> Option<u64> {
        self.steps.first().copied()
    }

    pub fn tps(&self) -> Option<f64> {
        let ms = self.stream_ms?;
        let out = self.tokens.output;
        (ms > 50.0 && out > 0).then(|| out as f64 / (ms / 1000.0))
    }
}

/// Recognizes the same call read twice: from the language_server and a database, or from two
/// database copies.
///
/// Calls with equal response IDs are the same call. Otherwise a call matches one with the same
/// conversation, second, input and output tokens, unless both carry response IDs (then the IDs
/// already said they differ).
#[derive(Default)]
pub struct DedupSet {
    ids: HashSet<String>,
    /// Soft key -> whether some call with that key had no response ID.
    soft: HashMap<String, bool>,
}

impl DedupSet {
    /// Records `c` and returns true if no earlier call was the same call.
    pub fn insert(&mut self, c: &Call) -> bool {
        let id = c.response_id.as_deref().filter(|s| !s.is_empty());
        if id.is_some_and(|id| self.ids.contains(id)) {
            return false;
        }
        let soft = format!("{}:{}:{}:{}", c.conv_id, c.at_ms.div_euclid(1000), c.tokens.input, c.tokens.output);
        if let Some(&some_without_id) = self.soft.get(&soft)
            && (id.is_none() || some_without_id)
        {
            return false;
        }
        if let Some(id) = id {
            self.ids.insert(id.to_string());
        }
        *self.soft.entry(soft).or_default() |= id.is_none();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: Option<&str>, at_ms: i64, input: u64) -> Call {
        Call {
            conv_id: "c".into(),
            at_ms,
            tokens: Tokens { input, output: 7, ..Default::default() },
            response_id: id.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn same_response_id_is_the_same_call_even_at_another_time() {
        let mut seen = DedupSet::default();
        assert!(seen.insert(&call(Some("r1"), 1_000, 10)));
        assert!(!seen.insert(&call(Some("r1"), 9_000, 10)));
    }

    #[test]
    fn soft_key_matches_when_either_call_lacks_a_response_id() {
        let mut seen = DedupSet::default();
        assert!(seen.insert(&call(Some("r1"), 1_200, 10)));
        assert!(!seen.insert(&call(None, 1_900, 10)), "same second, same tokens");
        assert!(seen.insert(&call(None, 2_000, 10)), "next second is another call");

        let mut seen = DedupSet::default();
        assert!(seen.insert(&call(None, 1_000, 10)));
        assert!(!seen.insert(&call(Some("r2"), 1_000, 10)));
    }

    #[test]
    fn different_response_ids_are_different_calls_despite_equal_soft_keys() {
        let mut seen = DedupSet::default();
        assert!(seen.insert(&call(Some("r1"), 1_000, 10)));
        assert!(seen.insert(&call(Some("r2"), 1_000, 10)));
    }

    #[test]
    fn model_id_clips_long_names_and_falls_back_for_buckets() {
        let long = "x".repeat(80);
        assert_eq!(ModelId::new(&long, "").raw.chars().count(), 64);
        assert_eq!(ModelId::new("", "Gemini 4 Neon").bucket_key(), "Gemini 4 Neon");
        assert_eq!(ModelId::new("", "").bucket_key(), "unknown");
        assert_eq!(ModelId::new("gemini-4-neon", "Gemini 4 Neon").price_key(), "gemini-4-neon gemini 4 neon");
    }
}
