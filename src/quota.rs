//! Weekly & 5h quota net-change accounting with period segmentation, early-reset detection,
//! ended-cycle history, and sample-based replay (ported from `agy-zh-pack`'s `lib/quota.mjs`).
//!
//! Pure state-machine logic with no file I/O so it can be thoroughly unit-tested in isolation.

use crate::call::ModelId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

pub const QUOTA_SEGS: usize = 8;
pub const WEEK_MS: i64 = 7 * 86_400_000;

pub fn r4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

pub fn same_period(r1: i64, r2: i64) -> bool {
    (r1 - r2).abs() < 3_600_000
}

/// A finished period segment observed earlier on the same calendar day.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuotaSegment {
    pub reset_ms: i64,
    pub start: f64,
    pub last: f64,
}

impl QuotaSegment {
    fn delta(&self) -> f64 {
        (self.last - self.start).max(0.0)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DayQuotaRec {
    pub name: String,
    pub window: String,
    /// Used fraction at the start of the active period segment today.
    pub start: f64,
    /// Net delta settled in earlier period segments today (`c0 + sum(segments)`).
    pub carried: f64,
    /// Overflow delta folded from segments beyond `QUOTA_SEGS` or migrated from pre-segment records.
    #[serde(default)]
    pub c0: f64,
    /// Last observed used fraction today.
    pub last: f64,
    /// Max observed used fraction today.
    pub max: f64,
    /// Net used fraction consumed today: `carried + max(0, last - start)`.
    pub used: f64,
    pub reset_ms: i64,
    /// Timestamp (ms) of the last reading recorded in this entry.
    #[serde(default)]
    pub last_at_ms: i64,
    /// Earlier period segments seen today (so oscillating readings resume their own segment).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<QuotaSegment>,
    /// Distinguishes freshly created segmented records from legacy records that only had `carried`.
    #[serde(default)]
    pub segmented: bool,
}

#[derive(Clone, Debug)]
pub struct QuotaBucketSample {
    pub id: String,
    pub name: String,
    pub window: String,
    pub used: f64,
    pub reset_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuotaSamplePoint {
    pub at_ms: i64,
    pub bucket_id: String,
    pub used: f64,
    pub reset_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuotaSamples {
    pub v: u32,
    #[serde(default)]
    pub by_acct: HashMap<String, Vec<QuotaSamplePoint>>,
}

/// Period-change event emitted by `note_quota`.
#[derive(Clone, Debug)]
pub struct PeriodTransition {
    pub was_reset_ms: i64,
    pub from_used: f64,
    pub after_ms: i64,
    pub early: bool,
    pub back: bool,
}

/// Recorded early weekly quota reset (Google reset the bucket >1h ahead of schedule).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetEvent {
    pub acct_key: String,
    pub bucket_id: String,
    pub name: String,
    pub window: String,
    pub at_ms: i64,
    pub after_ms: i64,
    pub from_used: f64,
    pub was_reset_ms: i64,
    #[serde(default)]
    pub flips: u32,
}

/// Record of an ended weekly quota cycle, used to track cap stability over time.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleRecord {
    pub acct_key: String,
    pub bucket_id: String,
    pub name: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub used: f64,
    pub early: bool,
}

fn is_early_reset(old_reset_ms: i64, old_used: f64, at_ms: i64) -> bool {
    old_used > 0.0 && (old_reset_ms - at_ms) > 3_600_000
}

fn seg_sum(q: &DayQuotaRec) -> f64 {
    q.c0 + q.segments.iter().map(QuotaSegment::delta).sum::<f64>()
}

/// Updates one day's quota map for a single account with a new batch of bucket samples.
/// Calls `on_reset(bucket, transition)` whenever a bucket changes period.
pub fn note_quota<F, R>(
    qs: &mut HashMap<String, DayQuotaRec>,
    buckets: &[QuotaBucketSample],
    mut prev_fn: F,
    at_ms: i64,
    mut on_reset: R,
) where
    F: FnMut(&str) -> Option<DayQuotaRec>,
    R: FnMut(&QuotaBucketSample, PeriodTransition),
{
    for b in buckets {
        match qs.get_mut(&b.id) {
            None => {
                let prev = prev_fn(&b.id);
                let start = match prev {
                    Some(p) => {
                        if same_period(p.reset_ms, b.reset_ms) {
                            p.last
                        } else if let Some(back) = p.segments.iter().find(|x| same_period(x.reset_ms, b.reset_ms)) {
                            back.last
                        } else {
                            on_reset(
                                b,
                                PeriodTransition {
                                    was_reset_ms: p.reset_ms,
                                    from_used: p.last,
                                    after_ms: p.last_at_ms,
                                    early: is_early_reset(p.reset_ms, p.last, at_ms),
                                    back: false,
                                },
                            );
                            0.0
                        }
                    }
                    None => b.used,
                };
                let last = r4(b.used);
                let start = r4(start);
                let used = r4((last - start).max(0.0));
                qs.insert(
                    b.id.clone(),
                    DayQuotaRec {
                        name: b.name.clone(),
                        window: b.window.clone(),
                        start,
                        carried: 0.0,
                        c0: 0.0,
                        last,
                        max: last,
                        used,
                        reset_ms: b.reset_ms,
                        last_at_ms: at_ms,
                        segments: Vec::new(),
                        segmented: true,
                    },
                );
            }
            Some(q) => {
                // Migrate legacy records created before period segmentation existed.
                if !q.segmented {
                    q.c0 = r4(q.carried);
                    q.segments.clear();
                    q.segmented = true;
                }
                if !same_period(q.reset_ms, b.reset_ms) {
                    let cur = QuotaSegment { reset_ms: q.reset_ms, start: q.start, last: q.last };
                    if let Some(idx) = q.segments.iter().position(|x| same_period(x.reset_ms, b.reset_ms)) {
                        // Reading flipped back to an earlier segment from today: resume it instead of double-counting.
                        let back = q.segments.remove(idx);
                        q.segments.push(cur.clone());
                        q.start = back.start;
                        on_reset(
                            b,
                            PeriodTransition {
                                was_reset_ms: cur.reset_ms,
                                from_used: cur.last,
                                after_ms: q.last_at_ms,
                                early: false,
                                back: true,
                            },
                        );
                    } else {
                        q.segments.push(cur.clone());
                        q.start = 0.0;
                        on_reset(
                            b,
                            PeriodTransition {
                                was_reset_ms: cur.reset_ms,
                                from_used: cur.last,
                                after_ms: q.last_at_ms,
                                early: is_early_reset(cur.reset_ms, cur.last, at_ms),
                                back: false,
                            },
                        );
                    }
                    while q.segments.len() > QUOTA_SEGS {
                        let oldest = q.segments.remove(0);
                        q.c0 = r4(q.c0 + oldest.delta());
                    }
                }
                q.last = r4(b.used);
                q.carried = r4(seg_sum(q));
                q.used = r4(q.carried + (q.last - q.start).max(0.0));
                q.max = r4(q.max.max(b.used));
                q.reset_ms = b.reset_ms;
                q.last_at_ms = at_ms;
                if !b.name.is_empty() {
                    q.name = b.name.clone();
                }
                if !b.window.is_empty() {
                    q.window = b.window.clone();
                }
            }
        }
    }
}

/// Records weekly early-reset events, deduplicating oscillating flips into a single entry.
pub fn note_reset_event(
    list: &mut Vec<ResetEvent>,
    acct_key: &str,
    b: &QuotaBucketSample,
    at_ms: i64,
    ev: &PeriodTransition,
) -> bool {
    if b.window != "weekly" {
        return false;
    }
    if let Some(old) = list
        .iter_mut()
        .find(|e| e.acct_key == acct_key && e.bucket_id == b.id && same_period(e.was_reset_ms, ev.was_reset_ms))
    {
        if ev.back {
            old.from_used = r4(old.from_used.max(ev.from_used));
            old.flips += 1;
            return true;
        }
        return false;
    }
    if ev.back || !ev.early {
        return false;
    }
    list.push(ResetEvent {
        acct_key: acct_key.to_string(),
        bucket_id: b.id.clone(),
        name: b.name.clone(),
        window: b.window.clone(),
        at_ms,
        after_ms: ev.after_ms,
        from_used: r4(ev.from_used),
        was_reset_ms: ev.was_reset_ms,
        flips: 0,
    });
    true
}

/// Records ended weekly cycles (both natural expiration and early resets) for cap history.
pub fn note_cycle_end(
    list: &mut Vec<CycleRecord>,
    acct_key: &str,
    b: &QuotaBucketSample,
    at_ms: i64,
    ev: &PeriodTransition,
) -> bool {
    if b.window != "weekly" || ev.was_reset_ms <= 0 {
        return false;
    }
    if let Some(old) = list
        .iter_mut()
        .find(|e| e.acct_key == acct_key && e.bucket_id == b.id && same_period(e.start_ms + WEEK_MS, ev.was_reset_ms))
    {
        if ev.back {
            old.used = r4(old.used.max(ev.from_used));
            return true;
        }
        return false;
    }
    if ev.back || ev.from_used <= 0.0 {
        return false;
    }
    list.push(CycleRecord {
        acct_key: acct_key.to_string(),
        bucket_id: b.id.clone(),
        name: b.name.clone(),
        start_ms: ev.was_reset_ms - WEEK_MS,
        end_ms: if ev.early { at_ms } else { ev.was_reset_ms.min(at_ms) },
        used: r4(ev.from_used),
        early: ev.early,
    });
    true
}

/// Replays fine-grained weekly samples through the segmented algorithm for days whose samples
/// cover the start of the day (first sample no later than day start + 1h), repairing inflated
/// legacy deltas and backfilling early-reset / ended-cycle records.
pub fn replay_from_samples<D, S, E>(
    days_qa: &mut BTreeMap<String, HashMap<String, HashMap<String, DayQuotaRec>>>,
    samples: &QuotaSamples,
    day_of: D,
    day_start: S,
    mut on_event: E,
) -> Vec<String>
where
    D: Fn(i64) -> String,
    S: Fn(&str) -> i64,
    E: FnMut(&str, &QuotaBucketSample, i64, &PeriodTransition),
{
    let mut changes = Vec::new();
    for (acct_key, list) in &samples.by_acct {
        if list.is_empty() {
            continue;
        }
        let first_at = list.iter().map(|p| p.at_ms).min().unwrap_or(i64::MAX);
        let mut by_day: BTreeMap<String, BTreeMap<i64, Vec<&QuotaSamplePoint>>> = BTreeMap::new();
        for p in list {
            by_day.entry(day_of(p.at_ms)).or_default().entry(p.at_ms).or_default().push(p);
        }

        for (day, by_ts) in by_day {
            if first_at > day_start(&day) + 3_600_000 {
                continue;
            }
            if !days_qa.get(&day).and_then(|m| m.get(acct_key)).is_some() {
                continue;
            }
            // Look up previous day's records for this account before mutating `day`.
            let prev_lookup: HashMap<String, DayQuotaRec> = {
                let mut map = HashMap::new();
                for (_, dm) in days_qa.range(..day.clone()).rev().take(14) {
                    if let Some(qm) = dm.get(acct_key) {
                        for (bid, rec) in qm {
                            map.entry(bid.clone()).or_insert_with(|| rec.clone());
                        }
                    }
                }
                map
            };
            let existing_names: HashMap<String, String> = days_qa
                .get(&day)
                .and_then(|m| m.get(acct_key))
                .map(|qm| qm.iter().map(|(k, v)| (k.clone(), v.name.clone())).collect())
                .unwrap_or_default();

            let mut qs: HashMap<String, DayQuotaRec> = HashMap::new();
            for (at_ms, pts) in by_ts {
                let buckets: Vec<QuotaBucketSample> = pts
                    .into_iter()
                    .map(|p| QuotaBucketSample {
                        id: p.bucket_id.clone(),
                        name: existing_names.get(&p.bucket_id).cloned().unwrap_or_default(),
                        window: "weekly".to_string(),
                        used: p.used,
                        reset_ms: p.reset_ms,
                    })
                    .collect();
                note_quota(
                    &mut qs,
                    &buckets,
                    |bid| prev_lookup.get(bid).cloned(),
                    at_ms,
                    |b, ev| {
                        on_event(acct_key, b, at_ms, &ev);
                    },
                );
            }

            if let Some(rec_map) = days_qa.get_mut(&day).and_then(|m| m.get_mut(acct_key)) {
                for (bid, mut q) in qs {
                    let old_u = rec_map.get(&bid).map(|o| o.used);
                    if let Some(old) = rec_map.get(&bid) {
                        q.max = r4(q.max.max(old.max));
                        if !old.name.is_empty() {
                            q.name = old.name.clone();
                        }
                    }
                    if old_u.is_none_or(|ou| (ou - q.used).abs() >= 1e-4) {
                        let prev_str = old_u.map(|v| format!("{v:.4}")).unwrap_or_else(|| "-".into());
                        changes.push(format!("{day} {bid} {prev_str} -> {:.4}", q.used));
                    }
                    rec_map.insert(bid, q);
                }
            }
        }
    }
    changes
}

/// Which quota a model's calls count against: Gemini models share one, Claude and GPT models the
/// third-party one. The status bar's quota windows, the Usage tab's cycle spend and the quota
/// sample ids all classify through this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaKind {
    Gemini,
    ThirdParty,
    Other,
}

impl QuotaKind {
    /// A model is third-party when its raw or display name says so; anything else is Gemini.
    pub fn of_model(model: &ModelId) -> Self {
        let key = model.price_key();
        if ["claude", "opus", "sonnet", "haiku", "gpt"].iter().any(|k| key.contains(k)) {
            Self::ThirdParty
        } else {
            Self::Gemini
        }
    }

    /// A quota group, from its display name.
    pub fn of_group(display_name: &str) -> Self {
        let low = display_name.to_lowercase();
        if ["claude", "gpt", "3p", "third"].iter().any(|k| low.contains(k)) {
            Self::ThirdParty
        } else if low.contains("gemini") {
            Self::Gemini
        } else {
            Self::Other
        }
    }

    /// A quota bucket id as written by [`QuotaKind::bucket_id`].
    pub fn of_bucket_id(id: &str) -> Self {
        if id.starts_with("3p-") {
            Self::ThirdParty
        } else if id.starts_with("gemini-") {
            Self::Gemini
        } else {
            Self::Other
        }
    }

    /// `"<kind>-<window>"`, e.g. `"3p-weekly"`: how quota samples and history name a bucket.
    pub fn bucket_id(self, window: &str) -> String {
        let prefix = match self {
            Self::Gemini => "gemini",
            Self::ThirdParty => "3p",
            Self::Other => "other",
        };
        format!("{prefix}-{window}")
    }

    /// Whether `model`'s calls count against buckets of this kind (every model counts against
    /// an unrecognised group).
    pub fn counts(self, model: &ModelId) -> bool {
        self == Self::Other || Self::of_model(model) == self
    }
}

/// The quota group (from the language_server's `groups`) that `model`'s calls count against: the
/// group whose description names the model's family, else the first group of its kind, else the
/// first group. A missing model counts as Gemini.
pub fn group_of<'a>(model: Option<&ModelId>, groups: &'a [Value]) -> Option<&'a Value> {
    // "Gemini 4 Neon (XHigh)" -> "Gemini 4 Neon", as the group descriptions list models.
    let family = model.map(|m| m.display.split(" (").next().unwrap_or(&m.display).trim()).unwrap_or("");
    let kind = model.map_or(QuotaKind::Gemini, QuotaKind::of_model);
    groups
        .iter()
        .find(|g| !family.is_empty() && g["description"].as_str().unwrap_or("").contains(family))
        .or_else(|| groups.iter().find(|g| QuotaKind::of_group(g["displayName"].as_str().unwrap_or("")) == kind))
        .or_else(|| groups.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups() -> Vec<Value> {
        serde_json::json!([
            { "displayName": "Gemini Models", "description": "Gemini 4 Neon, Gemini 4 Hydrogen" },
            { "displayName": "Claude & GPT Models", "description": "Claude Opus 5.5, GPT-6" },
        ])
        .as_array()
        .unwrap()
        .clone()
    }

    #[test]
    fn group_of_prefers_the_description_then_the_kind_then_the_first_group() {
        let gs = groups();
        let name = |m: Option<ModelId>| group_of(m.as_ref(), &gs).map(|g| g["displayName"].as_str().unwrap());
        assert_eq!(name(Some(ModelId::new("x", "Claude Opus 5.5 (Thinking)"))), Some("Claude & GPT Models"));
        // Not named in any description: classified by its raw name, as the Usage tab's cycle spend does.
        let opus = ModelId::new("opus-next", "Next");
        assert_eq!(QuotaKind::of_model(&opus), QuotaKind::ThirdParty);
        assert_eq!(name(Some(opus)), Some("Claude & GPT Models"));
        assert_eq!(name(Some(ModelId::new("neon-2", "Neon 2"))), Some("Gemini Models"));
        assert_eq!(name(None), Some("Gemini Models"));
        assert!(group_of(None, &[]).is_none());
    }

    #[test]
    fn bucket_ids_round_trip_through_the_kind() {
        for (group, id) in
            [("Claude & GPT Models", "3p-weekly"), ("Gemini Models", "gemini-weekly"), ("Misc", "other-weekly")]
        {
            let kind = QuotaKind::of_group(group);
            assert_eq!(kind.bucket_id("weekly"), id);
            assert_eq!(QuotaKind::of_bucket_id(id), kind);
        }
        let opus = ModelId::new("claude-opus-5-5", "Claude Opus 5.5");
        assert!(
            QuotaKind::ThirdParty.counts(&opus) && !QuotaKind::Gemini.counts(&opus) && QuotaKind::Other.counts(&opus)
        );
    }
    use chrono::DateTime;

    fn ts(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(&format!("{s}Z")).unwrap().timestamp_millis()
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn oscillating_early_reset_counts_both_segments_once() {
        let mut qs = HashMap::new();
        let mut evs = Vec::new();
        let prev = DayQuotaRec {
            reset_ms: ts("2026-11-29T09:41:21"),
            last: 0.0721,
            start: 0.0423,
            used: 0.0298,
            max: 0.0799,
            window: "weekly".into(),
            name: "G".into(),
            segmented: true,
            ..Default::default()
        };
        let mut step = |at: i64, used: f64, reset: i64| {
            note_quota(
                &mut qs,
                &[QuotaBucketSample {
                    id: "gemini-weekly".into(),
                    name: "G".into(),
                    window: "weekly".into(),
                    used,
                    reset_ms: reset,
                }],
                |_| Some(prev.clone()),
                at,
                |b, e| {
                    note_reset_event(&mut evs, "k", b, at, &e);
                },
            );
        };
        step(ts("2026-11-24T00:12:14"), 0.0721, ts("2026-11-29T09:41:21"));
        step(ts("2026-11-24T04:05:39"), 0.0778, ts("2026-11-29T09:41:21"));
        step(ts("2026-11-24T04:08:30"), 0.0, ts("2026-12-01T04:05:33"));
        step(ts("2026-11-24T04:14:12"), 0.0849, ts("2026-11-29T09:41:21"));
        step(ts("2026-11-24T04:19:54"), 0.0, ts("2026-12-01T04:16:57"));
        let mut t = ts("2026-11-24T04:23:00");
        while t < ts("2026-11-24T09:31:00") {
            step(t, 0.0, t + WEEK_MS);
            t += 171_000;
        }
        step(ts("2026-11-24T09:34:00"), 0.005, ts("2026-12-01T09:31:25"));
        step(ts("2026-11-24T11:42:28"), 0.0271, ts("2026-12-01T09:31:25"));
        step(ts("2026-11-24T11:45:19"), 0.0141, ts("2026-12-01T09:31:25"));
        step(ts("2026-11-24T17:06:04"), 0.0607, ts("2026-12-01T09:31:25"));

        let q = &qs["gemini-weekly"];
        assert!(near(q.used, 0.0735), "used = {}, expected 0.0735", q.used);
        assert_eq!(evs.len(), 1);
        assert!(near(evs[0].from_used, 0.0849));
        assert_eq!(evs[0].flips, 1);
        assert_eq!(evs[0].at_ms, ts("2026-11-24T04:08:30"));
        assert_eq!(evs[0].after_ms, ts("2026-11-24T04:05:39"));
        assert_eq!(evs[0].was_reset_ms, ts("2026-11-29T09:41:21"));
    }

    #[test]
    fn natural_expiry_and_unused_buckets_are_not_early_resets() {
        let mut qs = HashMap::new();
        let mut evs = Vec::new();
        let mut cyc = Vec::new();
        let step = |qs: &mut HashMap<String, DayQuotaRec>,
                    evs: &mut Vec<ResetEvent>,
                    cyc: &mut Vec<CycleRecord>,
                    id: &str,
                    at: &str,
                    used: f64,
                    reset: &str| {
            let at_ms = ts(at);
            note_quota(
                qs,
                &[QuotaBucketSample {
                    id: id.into(),
                    name: "G".into(),
                    window: "weekly".into(),
                    used,
                    reset_ms: ts(reset),
                }],
                |_| None,
                at_ms,
                |b, e| {
                    note_reset_event(evs, "k", b, at_ms, &e);
                    note_cycle_end(cyc, "k", b, at_ms, &e);
                },
            );
        };
        step(&mut qs, &mut evs, &mut cyc, "gemini-weekly", "2026-11-29T09:30:00", 0.40, "2026-11-29T09:41:21");
        step(&mut qs, &mut evs, &mut cyc, "gemini-weekly", "2026-11-29T09:43:00", 0.0, "2026-12-06T09:43:00");
        step(&mut qs, &mut evs, &mut cyc, "gemini-weekly", "2026-11-29T10:00:00", 0.01, "2026-12-06T09:58:00");
        assert!(evs.is_empty());
        assert_eq!(cyc.len(), 1);
        assert!(!cyc[0].early);
        assert_eq!(cyc[0].end_ms, ts("2026-11-29T09:41:21"));
        assert!(near(qs["gemini-weekly"].used, 0.01));

        // Unused bucket drifting reset time is neither early reset nor ended cycle.
        step(&mut qs, &mut evs, &mut cyc, "3p-weekly", "2026-11-24T08:00:00", 0.0, "2026-12-01T08:00:00");
        step(&mut qs, &mut evs, &mut cyc, "3p-weekly", "2026-11-24T13:00:00", 0.0, "2026-12-01T13:00:00");
        assert!(evs.is_empty());
        assert_eq!(cyc.len(), 1);
    }

    #[test]
    fn cross_day_early_reset_and_5h_segment_cap() {
        let mut qs = HashMap::new();
        let mut evs = Vec::new();
        let prev = DayQuotaRec {
            reset_ms: ts("2026-11-29T00:54:11"),
            last: 0.1754,
            last_at_ms: ts("2026-11-23T23:58:00"),
            window: "weekly".into(),
            name: "G".into(),
            segmented: true,
            ..Default::default()
        };
        let at_ms = ts("2026-11-24T09:00:00");
        note_quota(
            &mut qs,
            &[QuotaBucketSample {
                id: "gemini-weekly".into(),
                name: "G".into(),
                window: "weekly".into(),
                used: 0.0,
                reset_ms: ts("2026-12-01T08:55:45"),
            }],
            |_| Some(prev.clone()),
            at_ms,
            |b, e| {
                note_reset_event(&mut evs, "k", b, at_ms, &e);
            },
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].after_ms, prev.last_at_ms);
        assert!(near(evs[0].from_used, 0.1754));
        assert_eq!(qs["gemini-weekly"].start, 0.0);

        // 5h bucket rolling over 12 times caps segments at QUOTA_SEGS without losing total delta.
        let mut qs5 = HashMap::new();
        let mut expected = 0.0;
        for i in 0..12 {
            let r = ts("2026-11-24T05:00:00") + i * 5 * 3_600_000;
            note_quota(
                &mut qs5,
                &[QuotaBucketSample {
                    id: "gemini-5h".into(),
                    name: "G".into(),
                    window: "5h".into(),
                    used: 0.1,
                    reset_ms: r,
                }],
                |_| None,
                r - 4 * 3_600_000,
                |_, _| {},
            );
            note_quota(
                &mut qs5,
                &[QuotaBucketSample {
                    id: "gemini-5h".into(),
                    name: "G".into(),
                    window: "5h".into(),
                    used: 0.3,
                    reset_ms: r,
                }],
                |_| None,
                r - 3_600_000,
                |_, _| {},
            );
            expected += if i == 0 { 0.2 } else { 0.3 };
        }
        assert!(qs5["gemini-5h"].segments.len() <= QUOTA_SEGS);
        assert!((qs5["gemini-5h"].used - expected).abs() < 1e-3);
    }

    #[test]
    fn migrates_legacy_unsegmented_record_and_replays_samples_idempotently() {
        let mut qs = HashMap::new();
        qs.insert(
            "gemini-weekly".into(),
            DayQuotaRec {
                name: "G".into(),
                window: "weekly".into(),
                last: 0.0607,
                max: 0.0922,
                reset_ms: ts("2026-12-01T09:31:25"),
                start: 0.0,
                carried: 0.0906,
                used: 0.1513,
                segmented: false,
                ..Default::default()
            },
        );
        note_quota(
            &mut qs,
            &[QuotaBucketSample {
                id: "gemini-weekly".into(),
                name: "G".into(),
                window: "weekly".into(),
                used: 0.0610,
                reset_ms: ts("2026-12-01T09:31:25"),
            }],
            |_| None,
            ts("2026-11-24T17:10:00"),
            |_, _| {},
        );
        assert!(near(qs["gemini-weekly"].used, 0.1516));
        assert!(near(qs["gemini-weekly"].c0, 0.0906));

        // Sample replay fixes inflated day record and is idempotent on second run.
        let r1 = ts("2026-11-29T09:00:00");
        let r2 = ts("2026-12-01T08:00:00");
        let mut days_qa: BTreeMap<String, HashMap<String, HashMap<String, DayQuotaRec>>> = BTreeMap::new();
        let mut acct_map = HashMap::new();
        let mut bmap = HashMap::new();
        bmap.insert(
            "gemini-weekly".into(),
            DayQuotaRec {
                name: "G".into(),
                window: "weekly".into(),
                last: 0.02,
                max: 0.15,
                reset_ms: r2,
                start: 0.10,
                used: 0.12,
                ..Default::default()
            },
        );
        acct_map.insert("k".into(), bmap);
        days_qa.insert("2026-11-24".into(), acct_map);

        let mut by_acct = HashMap::new();
        by_acct.insert(
            "k".into(),
            vec![
                QuotaSamplePoint {
                    at_ms: ts("2026-11-24T00:05:00"),
                    bucket_id: "gemini-weekly".into(),
                    used: 0.10,
                    reset_ms: r1,
                },
                QuotaSamplePoint {
                    at_ms: ts("2026-11-24T06:00:00"),
                    bucket_id: "gemini-weekly".into(),
                    used: 0.15,
                    reset_ms: r1,
                },
                QuotaSamplePoint {
                    at_ms: ts("2026-11-24T08:00:00"),
                    bucket_id: "gemini-weekly".into(),
                    used: 0.0,
                    reset_ms: r2,
                },
                QuotaSamplePoint {
                    at_ms: ts("2026-11-24T10:00:00"),
                    bucket_id: "gemini-weekly".into(),
                    used: 0.02,
                    reset_ms: r2,
                },
            ],
        );
        let samples = QuotaSamples { v: 1, by_acct };
        let mut evs = Vec::new();
        let ch1 = replay_from_samples(
            &mut days_qa,
            &samples,
            |_| "2026-11-24".into(),
            |_| ts("2026-11-24T00:00:00"),
            |k, b, at, ev| {
                note_reset_event(&mut evs, k, b, at, ev);
            },
        );
        assert_eq!(ch1.len(), 1);
        assert!(near(days_qa["2026-11-24"]["k"]["gemini-weekly"].used, 0.07));
        assert_eq!(evs.len(), 1);
        assert!(near(evs[0].from_used, 0.15));

        let ch2 = replay_from_samples(
            &mut days_qa,
            &samples,
            |_| "2026-11-24".into(),
            |_| ts("2026-11-24T00:00:00"),
            |k, b, at, ev| {
                note_reset_event(&mut evs, k, b, at, ev);
            },
        );
        assert!(ch2.is_empty());
        assert_eq!(evs.len(), 1);
    }
}
