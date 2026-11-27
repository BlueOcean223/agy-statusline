//! Prompt cache-miss quantification and 6-cause attribution (ported from `agy-zh-pack` v2.0.9).
//!
//! Compares each model call in a conversation against the previous large call (>= 20k prompt tokens)
//! for the same pricing model: if >= 20k cacheable tokens were billed at full input / cache-write
//! rates instead of cache-read rates, records the wasted USD and attributes one of 6 causes:
//! `model` | `compact` | `user` | `turn` | `wait` | `other`.

use crate::call::Call;
use crate::pricing::Pricer;
use crate::transcript::StepMarkers;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub const MISS_MIN_TOK: u64 = 20_000;
pub const MISS_IDLE_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissCause {
    Model,
    Compact,
    User,
    Turn,
    Wait,
    Other,
}

impl MissCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Compact => "compact",
            Self::User => "user",
            Self::Turn => "turn",
            Self::Wait => "wait",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissInfo {
    pub tok: u64,
    pub usd: f64,
    pub cause: MissCause,
    pub idle_ms: i64,
    pub from_model: String,
}

/// One call as the scan sees it: priced, in conversation order.
struct Input {
    at_ms: i64,
    ttft_ms: Option<f64>,
    stream_ms: Option<f64>,
    step: Option<u64>,
    /// Matched rule's short name (`None` when no rule prices the model).
    model_name: Option<String>,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    p_in: f64,
    p_cache: f64,
    /// Cache-write USD/1M rate (equals `p_in` for models without separate cache-write billing).
    p_cw: f64,
}

impl Input {
    fn new(c: &Call, pricer: &Pricer) -> Self {
        let priced = pricer.rates(&c.model, c.at_ms);
        let p = priced.map(|(_, p)| p).unwrap_or_default();
        Self {
            at_ms: c.at_ms,
            ttft_ms: c.ttft_ms,
            stream_ms: c.stream_ms,
            step: c.step(),
            model_name: priced.map(|(r, _)| r.short_name.clone()),
            input: c.tokens.input,
            cache_read: c.tokens.cache_read,
            cache_write: c.tokens.cache_write,
            p_in: p.input,
            p_cache: p.cache_read,
            p_cw: p.cache_write,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissCauseBucket {
    pub count: usize,
    pub usd: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissSummary {
    pub count: usize,
    pub usd: f64,
    pub total_usd: f64,
    pub by: BTreeMap<String, MissCauseBucket>,
}

impl MissSummary {
    pub fn add(&mut self, miss: &MissInfo) {
        self.count += 1;
        self.usd += miss.usd;
        let e = self.by.entry(miss.cause.as_str().to_string()).or_default();
        e.count += 1;
        e.usd += miss.usd;
    }

    pub fn round_fields(&mut self) {
        self.usd = (self.usd * 1e6).round() / 1e6;
        self.total_usd = (self.total_usd * 1e6).round() / 1e6;
        for b in self.by.values_mut() {
            b.usd = (b.usd * 1e6).round() / 1e6;
        }
    }
}

fn has_step_between(set: &std::collections::HashSet<u64>, a: Option<u64>, b: Option<u64>) -> bool {
    let (Some(lo), Some(hi)) = (a, b) else { return false };
    set.iter().any(|&k| k > lo && k <= hi)
}

/// Scans chronologically ordered calls of a single conversation and returns per-call `MissInfo`.
/// Pass `markers = None` for CLI conversations where step types are not available (pauses → `wait`).
pub fn miss_scan<'a>(
    calls: impl IntoIterator<Item = &'a Call>,
    pricer: &Pricer,
    markers: Option<&StepMarkers>,
) -> Vec<Option<MissInfo>> {
    let list: Vec<Input> = calls.into_iter().map(|c| Input::new(c, pricer)).collect();
    let mut out = vec![None; list.len()];
    let mut last_by_model: HashMap<String, usize> = HashMap::new();
    let mut last_big: Option<usize> = None;

    for (i, c) in list.iter().enumerate() {
        let Some(ref name) = c.model_name else {
            continue;
        };
        let prompt = c.input + c.cache_read + c.cache_write;
        let (base_idx, mut cause) = match last_by_model.get(name).copied() {
            Some(bi) => (Some(bi), None),
            None => match last_big {
                Some(bi) => (Some(bi), Some(MissCause::Model)),
                None => (None, None),
            },
        };

        if let Some(bi) = base_idx {
            let b = &list[bi];
            let prev_prompt = b.input + b.cache_read + b.cache_write;
            let missed_tok = prev_prompt.min(prompt).saturating_sub(c.cache_read);
            if missed_tok >= MISS_MIN_TOK {
                let prev_dur_ms = (b.ttft_ms.unwrap_or(0.0) + b.stream_ms.unwrap_or(0.0)).round() as i64;
                let idle_ms = (c.at_ms - b.at_ms - prev_dur_ms).max(0);
                let final_cause = cause.take().unwrap_or_else(|| {
                    let has_ckpt = markers.is_some_and(|m| has_step_between(&m.ckpt, b.step, c.step));
                    if has_ckpt || (prompt as f64) < 0.6 * (prev_prompt as f64) {
                        MissCause::Compact
                    } else if idle_ms >= MISS_IDLE_MS {
                        match markers {
                            None => MissCause::Wait,
                            Some(m) if has_step_between(&m.user, b.step, c.step) => MissCause::User,
                            Some(_) => MissCause::Turn,
                        }
                    } else {
                        MissCause::Other
                    }
                });
                let uncached = c.input + c.cache_write;
                let full_rate = if uncached > 0 {
                    (c.input as f64 * c.p_in + c.cache_write as f64 * c.p_cw) / uncached as f64
                } else {
                    c.p_in
                };
                let wasted_usd = (missed_tok as f64 * (full_rate - c.p_cache).max(0.0)) / 1e6;
                out[i] = Some(MissInfo {
                    tok: missed_tok,
                    usd: (wasted_usd * 1e6).round() / 1e6,
                    cause: final_cause,
                    idle_ms,
                    from_model: b.model_name.clone().unwrap_or_default(),
                });
            }
        }

        if prompt >= MISS_MIN_TOK {
            last_by_model.insert(name.clone(), i);
            last_big = Some(i);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::{ModelId, Tokens};
    use std::collections::HashSet;

    fn pricer() -> Pricer {
        let rule = |pattern: &str, short: &str, inp, cache, cw| crate::config::ModelRule {
            pattern: pattern.into(),
            short_name: short.into(),
            input_per_m: inp,
            cache_read_per_m: cache,
            cache_write_per_m: cw,
            ..Default::default()
        };
        Pricer::from_rules(vec![rule("carbon", "Carbon", 4.0, 0.4, 0.0), rule("opus", "Opus 5.5", 5.0, 0.5, 6.25)], 0)
    }

    fn call(model: &str, at_s: i64, t: Tokens, dur_s: Option<f64>, step: Option<u64>) -> Call {
        Call {
            model: ModelId::new(model, ""),
            tokens: t,
            at_ms: at_s * 1000,
            ttft_ms: dur_s.map(|d| d * 500.0),
            stream_ms: dur_s.map(|d| d * 500.0),
            steps: step.into_iter().collect(),
            ..Default::default()
        }
    }

    fn gem(at_s: i64, input: u64, cache_read: u64, dur_s: f64, step: Option<u64>) -> Call {
        call("gemini-4-carbon", at_s, Tokens { input, cache_read, ..Default::default() }, Some(dur_s), step)
    }

    fn cla(at_s: i64, input: u64, cache_read: u64, cache_write: u64, step: Option<u64>) -> Call {
        call("claude-opus-5-5", at_s, Tokens { input, cache_read, cache_write, output: 0 }, None, step)
    }

    fn scan(list: &[Call], markers: Option<&StepMarkers>) -> Vec<Option<MissInfo>> {
        miss_scan(list, &pricer(), markers)
    }

    #[test]
    fn cache_hit_is_not_flagged() {
        let m = StepMarkers::default();
        let r = scan(&[gem(0, 300_000, 0, 0.0, Some(1)), gem(20, 5_000, 300_000, 0.0, Some(2))], Some(&m));
        assert!(r[0].is_none() && r[1].is_none());
    }

    #[test]
    fn idle_over_one_minute_classifies_user_turn_or_wait() {
        let list = [gem(0, 300_000, 0, 0.0, Some(1)), gem(400, 310_000, 0, 0.0, Some(5))];
        let mut mu = StepMarkers::default();
        mu.user.insert(3);
        let u = scan(&list, Some(&mu))[1].clone().unwrap();
        assert_eq!(u.cause, MissCause::User);
        assert_eq!(u.tok, 300_000);
        assert!((u.usd - 1.08).abs() < 1e-4);
        assert_eq!(u.idle_ms, 400_000);
        assert_eq!(u.from_model, "Carbon");

        let empty = StepMarkers::default();
        assert_eq!(scan(&list, Some(&empty))[1].as_ref().unwrap().cause, MissCause::Turn);

        let mut after = StepMarkers::default();
        after.user.insert(7);
        assert_eq!(scan(&list, Some(&after))[1].as_ref().unwrap().cause, MissCause::Turn);

        assert_eq!(scan(&list, None)[1].as_ref().unwrap().cause, MissCause::Wait);
    }

    #[test]
    fn idle_subtracts_previous_call_duration() {
        let r = scan(&[gem(0, 200_000, 0, 50.0, None), gem(90, 210_000, 0, 0.0, None)], None);
        let m = r[1].as_ref().unwrap();
        assert_eq!(m.cause, MissCause::Other);
        assert_eq!(m.idle_ms, 40_000);
    }

    #[test]
    fn compaction_detected_by_checkpoint_step_or_prompt_drop() {
        let m = StepMarkers { user: HashSet::new(), ckpt: HashSet::from([4]) };
        let a = scan(&[gem(0, 400_000, 0, 0.0, Some(1)), gem(30, 380_000, 0, 0.0, Some(9))], Some(&m));
        assert_eq!(a[1].as_ref().unwrap().cause, MissCause::Compact);

        let b = scan(&[gem(0, 400_000, 0, 0.0, None), gem(30, 100_000, 0, 0.0, None)], None);
        assert_eq!(b[1].as_ref().unwrap().cause, MissCause::Compact);
        assert_eq!(b[1].as_ref().unwrap().tok, 100_000);
    }

    #[test]
    fn model_switch_uses_cache_write_rate_and_remembers_previous_model() {
        let m = StepMarkers::default();
        let r = scan(&[gem(0, 300_000, 0, 0.0, Some(1)), cla(10, 0, 0, 300_000, Some(2))], Some(&m));
        let info = r[1].as_ref().unwrap();
        assert_eq!(info.cause, MissCause::Model);
        assert_eq!(info.from_model, "Carbon");
        assert!((info.usd - 300_000.0 * (6.25 - 0.5) / 1e6).abs() < 1e-4);

        let back = scan(
            &[gem(0, 300_000, 0, 0.0, Some(1)), cla(10, 0, 0, 300_000, Some(2)), gem(20, 5_000, 300_000, 0.0, Some(3))],
            Some(&m),
        );
        assert!(back[2].is_none());
    }

    #[test]
    fn small_misses_and_unpriced_calls_are_ignored() {
        let r = scan(&[gem(0, 50_000, 0, 0.0, None), gem(10, 15_000, 40_000, 0.0, None)], None);
        assert!(r[1].is_none());

        let r2 =
            scan(&[gem(0, 300_000, 0, 0.0, None), gem(5, 5_000, 0, 0.0, None), gem(400, 310_000, 0, 0.0, None)], None);
        assert!(r2[1].is_none());
        assert_eq!(r2[2].as_ref().unwrap().tok, 300_000);
        assert_eq!(r2[2].as_ref().unwrap().cause, MissCause::Wait);

        let mut unpriced = gem(10, 300_000, 0, 0.0, None);
        unpriced.model = ModelId::new("mystery", "");
        let r3 = scan(&[gem(0, 300_000, 0, 0.0, None), unpriced, gem(20, 5_000, 300_000, 0.0, None)], None);
        assert!(r3.iter().all(Option::is_none));
    }
}
