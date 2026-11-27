//! One model call, as read from the language_server or from a conversation database.
//!
//! Both sources describe the same record (see `convdb` for the protobuf field map), so live
//! tracking, usage history and the `agy CLI` scan all share this type. State the live engine keeps
//! about a call (transcript dating, labels) lives in `live::LiveCall`, not here.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Desktop Antigravity.
pub const DESKTOP: &str = "desktop";
/// `agy CLI`.
pub const CLI: &str = "cli";

/// Longest model name kept, so a malformed record can't bloat persisted history.
const MODEL_NAME_MAX_CHARS: usize = 64;

/// Backend model name (`raw`, e.g. `gemini-4-neon`) plus the name shown to the user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
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

    /// Key for daily aggregates: `"raw|display"` (see ADR 0001), or `"unknown"` when both are empty.
    pub fn bucket_key(&self) -> String {
        if self.raw.is_empty() && self.display.is_empty() {
            "unknown".to_string()
        } else {
            format!("{}|{}", self.raw, self.display)
        }
    }

    /// The key this model's daily aggregates had before ADR 0001: the raw name, else the display
    /// name, else `"unknown"`.
    pub fn legacy_bucket_key(&self) -> &str {
        if !self.raw.is_empty() {
            &self.raw
        } else if !self.display.is_empty() {
            &self.display
        } else {
            "unknown"
        }
    }

    /// Reads a daily-aggregate key. Keys written before ADR 0001 hold only the raw name.
    pub fn from_bucket_key(key: &str) -> Self {
        match key.split_once('|') {
            Some((raw, display)) => Self { raw: raw.to_string(), display: display.to_string() },
            None => Self { raw: key.to_string(), display: String::new() },
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

    /// Share of prompt tokens read from cache: `cache_read / (cache_read + input)`, `None` when the
    /// prompt was empty.
    pub fn cache_hit(&self) -> Option<f64> {
        let prompt = self.cache_read + self.input;
        (prompt > 0).then(|| self.cache_read as f64 / prompt as f64)
    }

    pub fn saturating_sub(&self, o: &Tokens) -> Tokens {
        Tokens {
            input: self.input.saturating_sub(o.input),
            cache_read: self.cache_read.saturating_sub(o.cache_read),
            cache_write: self.cache_write.saturating_sub(o.cache_write),
            output: self.output.saturating_sub(o.output),
        }
    }
}

impl std::ops::AddAssign<&Tokens> for Tokens {
    fn add_assign(&mut self, o: &Tokens) {
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
    }
}

impl From<Tokens> for ModelAgg {
    /// Totals with no calls counted.
    fn from(t: Tokens) -> Self {
        Self { calls: 0, input: t.input, cache_read: t.cache_read, cache_write: t.cache_write, output: t.output }
    }
}

/// Calls and tokens summed per model for one day; the unit of usage history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAgg {
    pub calls: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl ModelAgg {
    /// Adds one call.
    pub fn add_tokens(&mut self, t: &Tokens) {
        self.calls += 1;
        self.input += t.input;
        self.cache_read += t.cache_read;
        self.cache_write += t.cache_write;
        self.output += t.output;
    }

    pub fn merge_from(&mut self, other: &ModelAgg) {
        self.calls += other.calls;
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
    }

    pub fn tokens(&self) -> Tokens {
        Tokens { input: self.input, cache_read: self.cache_read, cache_write: self.cache_write, output: self.output }
    }

    pub fn total_tokens(&self) -> u64 {
        self.tokens().total()
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
    fn cache_hit_is_none_without_a_prompt() {
        let t = |input, cache_read, output| Tokens { input, cache_read, output, ..Default::default() };
        assert_eq!(t(0, 0, 50).cache_hit(), None, "output only: nothing to hit");
        assert_eq!(t(0, 30, 0).cache_hit(), Some(1.0));
        assert_eq!(t(10, 30, 0).cache_hit(), Some(0.75));
    }

    #[test]
    fn model_id_clips_long_names_and_falls_back_for_buckets() {
        let long = "x".repeat(80);
        assert_eq!(ModelId::new(&long, "").raw.chars().count(), 64);
        assert_eq!(ModelId::new("gemini-4-neon", "Gemini 4 Neon").bucket_key(), "gemini-4-neon|Gemini 4 Neon");
        assert_eq!(ModelId::new("", "").bucket_key(), "unknown");
        let m = ModelId::new("orchestrator-exec", "Gemini 4 Hydrogen (Orchestrator)");
        assert_eq!(ModelId::from_bucket_key(&m.bucket_key()), m);
        assert_eq!(ModelId::from_bucket_key("gemini-4-neon"), ModelId::new("gemini-4-neon", ""), "pre-ADR key");
        assert_eq!(ModelId::new("gemini-4-neon", "Gemini 4 Neon").price_key(), "gemini-4-neon gemini 4 neon");
    }
}
