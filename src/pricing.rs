//! The Pricer: which model rule a model identity matches, and what a call or a day of usage costs.
//!
//! Every path that shows money (the bar, the side panel, the Usage tab, weekly quota cycles and
//! cache-miss analysis) prices through one [`Pricer`], so they agree on which rule a model gets
//! and on how a price-cut day is priced.

use crate::call::{Call, ModelAgg, ModelId, Tokens};
use crate::config::{ModelRule, OnlinePrices, effective_model_rules};
use crate::day::LocalDay;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

/// Label for usage of models no rule matches; it costs nothing.
pub const OTHER: &str = "Other";

/// USD per 1M tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl Rates {
    /// A `cache_write` of 0 means "not set": cache writes then cost the input rate.
    pub fn new(input: f64, cache_read: f64, cache_write: f64, output: f64) -> Self {
        let cache_write = if cache_write > 0.0 { cache_write } else { input };
        Self { input, cache_read, cache_write, output }
    }
}

/// USD, split by token kind.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Cost {
    #[serde(rename = "in")]
    pub input: f64,
    pub cache: f64,
    pub cw: f64,
    pub out: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.cache + self.cw + self.out
    }
}

impl std::ops::AddAssign<&Cost> for Cost {
    fn add_assign(&mut self, o: &Cost) {
        self.input += o.input;
        self.cache += o.cache;
        self.cw += o.cw;
        self.out += o.out;
    }
}

/// One day's usage by label (the matched rule's short name, or [`OTHER`]): USD and totals.
pub type DayCost = BTreeMap<String, (f64, ModelAgg)>;

/// USD and tokens of a priced day.
pub fn day_sum(day: &DayCost) -> (f64, u64) {
    day.values().fold((0.0, 0), |(u, t), (usd, agg)| (u + usd, t + agg.total_tokens()))
}

pub struct Pricer {
    rules: Vec<ModelRule>,
    now_ms: i64,
    source: String,
    fingerprint: u64,
    /// Index of the rule each model seen so far matched. A request prices every call of the
    /// conversations on screen, made by only a handful of models.
    matched: RefCell<HashMap<ModelId, Option<usize>>>,
}

impl Pricer {
    /// Configured model rules with the online price table (if enabled) laid over them. Calls with
    /// no known time are priced at `now_ms`.
    pub fn new(models: &[ModelRule], online: Option<&OnlinePrices>, now_ms: i64) -> Self {
        let source = match online {
            Some(p) if !p.updated.is_empty() => format!("online ({})", p.updated),
            Some(_) => "online".to_string(),
            None => "bundled".to_string(),
        };
        Self::with_rules(effective_model_rules(models, online), source, now_ms)
    }

    fn with_rules(rules: Vec<ModelRule>, source: String, now_ms: i64) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{rules:?}").hash(&mut h);
        Self { rules, now_ms, source, fingerprint: h.finish(), matched: RefCell::default() }
    }

    #[cfg(test)]
    pub(crate) fn from_rules(rules: Vec<ModelRule>, now_ms: i64) -> Self {
        Self::with_rules(rules, "bundled".to_string(), now_ms)
    }

    /// Where the prices come from: `"bundled"`, `"online"` or `"online (<date>)"`.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Changes whenever any rule or price changes.
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// The first rule whose pattern occurs in `"raw display"`, case-insensitively.
    pub fn rule(&self, model: &ModelId) -> Option<&ModelRule> {
        let cached = self.matched.borrow().get(model).copied();
        let idx = cached.unwrap_or_else(|| {
            let key = model.price_key();
            let idx = self.rules.iter().position(|r| !r.pattern.is_empty() && contains_lowercased(&key, &r.pattern));
            self.matched.borrow_mut().insert(model.clone(), idx);
            idx
        });
        idx.map(|i| &self.rules[i])
    }

    /// The matched rule's short name, or [`OTHER`].
    pub fn label(&self, model: &ModelId) -> String {
        self.rule(model).map(|r| r.short_name.clone()).unwrap_or_else(|| OTHER.to_string())
    }

    /// The matched rule and its rates at `at_ms` (or now, when `at_ms` is unknown).
    pub fn rates(&self, model: &ModelId, at_ms: i64) -> Option<(&ModelRule, Rates)> {
        let r = self.rule(model)?;
        Some((r, r.rates_at(at_ms, self.now_ms)))
    }

    pub fn cost(&self, model: &ModelId, t: &Tokens, at_ms: i64) -> Cost {
        let Some((_, p)) = self.rates(model, at_ms) else { return Cost::default() };
        Cost {
            input: t.input as f64 * p.input / 1e6,
            cache: t.cache_read as f64 * p.cache_read / 1e6,
            cw: t.cache_write as f64 * p.cache_write / 1e6,
            out: t.output as f64 * p.output / 1e6,
        }
    }

    pub fn call_cost(&self, c: &Call) -> Cost {
        self.cost(&c.model, &c.tokens, c.at_ms)
    }

    /// True when some price changes during `day` (after its first instant, up to its end). Such a
    /// day is priced call by call, so pass its calls to [`Pricer::price_day`].
    pub fn is_cut_day(&self, day: &LocalDay) -> bool {
        self.rules
            .iter()
            .flat_map(|r| &r.tiers)
            .filter_map(|t| t.until_ms)
            .any(|u| u > day.start_ms() && u <= day.end_ms())
    }

    /// Prices one day of daily aggregates (keys as written by [`ModelId::bucket_key`]).
    ///
    /// On a price-cut day with `exact` calls, each call is priced at its own time and whatever the
    /// aggregates hold beyond those calls (the remainder, e.g. from deleted databases) at local
    /// noon. Otherwise everything is priced at local noon.
    pub fn price_day<'a>(
        &self,
        aggregates: impl IntoIterator<Item = (&'a String, &'a ModelAgg)>,
        exact: Option<&[Call]>,
        day: &LocalDay,
    ) -> DayCost {
        let noon = day.noon_ms();
        let mut out = DayCost::new();
        let Some(exact) = exact.filter(|_| self.is_cut_day(day)) else {
            for (key, agg) in aggregates {
                let model = ModelId::from_bucket_key(key);
                let e = out.entry(self.label(&model)).or_default();
                e.0 += self.cost(&model, &agg.tokens(), noon).total();
                e.1.merge_from(agg);
            }
            return out;
        };

        for c in exact {
            let e = out.entry(self.label(&c.model)).or_default();
            e.0 += self.call_cost(c).total();
            e.1.add_tokens(&c.tokens);
        }
        let mut persisted: BTreeMap<String, (ModelId, ModelAgg)> = BTreeMap::new();
        for (key, agg) in aggregates {
            let model = ModelId::from_bucket_key(key);
            persisted.entry(self.label(&model)).or_insert_with(|| (model, ModelAgg::default())).1.merge_from(agg);
        }
        for (label, (model, p)) in persisted {
            let e = out.entry(label).or_default();
            let rem = p.tokens().saturating_sub(&e.1.tokens());
            e.0 += self.cost(&model, &rem, noon).total();
            e.1.merge_from(&ModelAgg { calls: p.calls.saturating_sub(e.1.calls), ..ModelAgg::from(rem) });
        }
        out
    }
}

/// `hay` is already lowercase. Same as `hay.contains(&pat.to_lowercase())`, without allocating
/// for ASCII patterns.
fn contains_lowercased(hay: &str, pat: &str) -> bool {
    if !pat.is_ascii() {
        return hay.contains(&pat.to_lowercase());
    }
    let (h, p) = (hay.as_bytes(), pat.as_bytes());
    if p.is_empty() {
        return true;
    }
    p.len() <= h.len() && h.windows(p.len()).any(|w| w.iter().zip(p).all(|(a, b)| *a == b.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PriceTier;
    use crate::day::tests::{date, fall_back, spring_forward, utc_ms};
    use chrono::FixedOffset;

    /// 2027-01-15T08:00:00Z.
    const CUT: i64 = 1_800_000_000_000;
    const HOUR: i64 = 3_600_000;

    /// The day containing [`CUT`] in a zone where the cut lands at 10:00, before noon.
    fn cut_day() -> LocalDay {
        LocalDay::of_in(CUT, &FixedOffset::east_opt(2 * 3600).unwrap())
    }

    fn rule(pattern: &str, short: &str) -> ModelRule {
        rule_cut_at(pattern, short, CUT)
    }

    /// $2/M input until `cut`, $4/M after.
    fn rule_cut_at(pattern: &str, short: &str, cut: i64) -> ModelRule {
        let tier = |until_ms, input_per_m| PriceTier {
            until_ms,
            input_per_m,
            cache_read_per_m: 0.0,
            cache_write_per_m: 0.0,
            output_per_m: 0.0,
        };
        ModelRule {
            pattern: pattern.into(),
            short_name: short.into(),
            tiers: vec![tier(Some(cut), 2.0), tier(None, 4.0)],
            ..Default::default()
        }
    }

    fn pricer() -> Pricer {
        // Built-in order: "hydrogen" is checked before the generic "orchestrator".
        Pricer::from_rules(vec![rule("hydrogen", "Hydrogen"), rule("orchestrator", "Carbon")], CUT)
    }

    fn call(model: &ModelId, input: u64, at_ms: i64) -> Call {
        Call { model: model.clone(), tokens: Tokens { input, ..Default::default() }, at_ms, ..Default::default() }
    }

    #[test]
    fn price_pattern_matching_is_case_insensitive_without_allocating() {
        assert!(contains_lowercased("gemini-4-neon gemini 4 neon (xhigh)", "Gemini 4 Neon"));
        assert!(contains_lowercased("claude-opus", "OPUS"));
        assert!(!contains_lowercased("gemini", "gemini-4"));
        assert!(contains_lowercased("flash", ""));
        assert!(contains_lowercased("模型 ä", "Ä"));
    }

    #[test]
    fn matches_raw_and_display_together() {
        let p = pricer();
        assert_eq!(p.label(&ModelId::new("orchestrator-exec", "Gemini 4 Hydrogen (Orchestrator)")), "Hydrogen");
        assert_eq!(p.label(&ModelId::new("orchestrator-exec", "")), "Carbon");
        assert_eq!(p.label(&ModelId::new("mystery", "Mystery")), OTHER);
        assert_eq!(p.call_cost(&call(&ModelId::new("mystery", ""), 1_000_000, 0)).total(), 0.0);
    }

    #[test]
    fn prices_by_call_time_and_unknown_time_at_now() {
        let p = pricer();
        let m = ModelId::new("hydrogen", "");
        assert_eq!(p.call_cost(&call(&m, 1_000_000, CUT - 1)).total(), 2.0);
        assert_eq!(p.call_cost(&call(&m, 1_000_000, CUT)).total(), 4.0);
        assert_eq!(p.call_cost(&call(&m, 1_000_000, 0)).total(), 4.0, "priced at now");
    }

    #[test]
    fn price_cut_day_prices_calls_exactly_and_the_remainder_at_noon() {
        let p = pricer();
        let day = cut_day();
        assert_eq!(day.start_ms(), CUT - 10 * HOUR);
        assert!(p.is_cut_day(&day));
        let m = ModelId::new("hydrogen", "Gemini 4 Hydrogen");
        let mut agg = ModelAgg::default();
        agg.add_tokens(&Tokens { input: 300_000, ..Default::default() });
        agg.add_tokens(&Tokens { input: 100_000, ..Default::default() });
        let aggs = [(m.bucket_key(), agg)];
        let aggs = aggs.iter().map(|(k, a)| (k, a));

        // One surviving call before the cut ($2/M); the other 300k are the remainder, at noon ($4/M).
        let day_cost = p.price_day(aggs, Some(&[call(&m, 100_000, day.start_ms() + HOUR)]), &day);
        let (usd, totals) = &day_cost["Hydrogen"];
        assert_eq!((totals.calls, totals.input), (2, 400_000));
        assert!((usd - (0.2 + 1.2)).abs() < 1e-9, "got {usd}");
    }

    #[test]
    fn price_cut_day_counts_a_call_once_when_raw_alone_would_match_another_rule() {
        let p = pricer();
        let day = cut_day();
        let m = ModelId::new("orchestrator-exec", "Gemini 4 Hydrogen (Orchestrator)");
        let mut agg = ModelAgg::default();
        agg.add_tokens(&Tokens { input: 50_000, ..Default::default() });
        let aggs = [(m.bucket_key(), agg)];
        let day_cost = p.price_day(aggs.iter().map(|(k, a)| (k, a)), Some(&[call(&m, 50_000, CUT + 1)]), &day);
        assert_eq!(day_cost.keys().collect::<Vec<_>>(), vec!["Hydrogen"]);
        assert_eq!(day_cost["Hydrogen"].1.calls, 1);
    }

    #[test]
    fn ordinary_day_prices_aggregates_at_noon() {
        let p = pricer();
        let day = LocalDay::of_in(CUT + 5 * 24 * HOUR, &FixedOffset::east_opt(2 * 3600).unwrap());
        assert!(!p.is_cut_day(&day));
        let mut agg = ModelAgg::default();
        agg.add_tokens(&Tokens { input: 1_000_000, ..Default::default() });
        let aggs = [("hydrogen".to_string(), agg)]; // a key written before ADR 0001
        let day_cost = p.price_day(aggs.iter().map(|(k, a)| (k, a)), Some(&[]), &day);
        assert_eq!(day_cost["Hydrogen"].0, 4.0);
    }

    fn one_rule_cut_at(cut: i64) -> Pricer {
        Pricer::from_rules(vec![rule_cut_at("hydrogen", "Hydrogen", cut)], cut)
    }

    #[test]
    fn a_cut_in_the_25th_hour_of_a_fall_back_day_is_on_that_day() {
        let tz = fall_back();
        let day = LocalDay::on_in(date("2026-10-25"), &tz);
        // Local 23:30, 24.5 hours after the day began.
        let cut = utc_ms("2026-10-25T22:30:00Z");
        assert_eq!(cut - day.start_ms(), 24 * HOUR + 30 * 60_000);
        assert!(one_rule_cut_at(cut).is_cut_day(&day));
        assert!(!one_rule_cut_at(cut).is_cut_day(&LocalDay::on_in(date("2026-10-26"), &tz)));
    }

    #[test]
    fn a_cut_just_after_a_spring_forward_day_is_on_the_next_day() {
        let tz = spring_forward();
        let day = LocalDay::on_in(date("2026-03-29"), &tz);
        // Local 00:30 on the 30th, 23.5 hours after the 29th began.
        let cut = utc_ms("2026-03-29T22:30:00Z");
        assert!(!one_rule_cut_at(cut).is_cut_day(&day));
        assert!(one_rule_cut_at(cut).is_cut_day(&LocalDay::on_in(date("2026-03-30"), &tz)));
    }

    #[test]
    fn aggregates_are_priced_at_wall_clock_noon_on_a_dst_day() {
        let tz = spring_forward();
        let day = LocalDay::on_in(date("2026-03-29"), &tz);
        // Local 12:30: after noon (11 hours in), but before `start + 12h`.
        let cut = utc_ms("2026-03-29T10:30:00Z");
        let p = one_rule_cut_at(cut);
        let mut agg = ModelAgg::default();
        agg.add_tokens(&Tokens { input: 1_000_000, ..Default::default() });
        let aggs = [("hydrogen".to_string(), agg)];
        let day_cost = p.price_day(aggs.iter().map(|(k, a)| (k, a)), None, &day);
        assert_eq!(day_cost["Hydrogen"].0, 2.0, "priced at local noon, before the cut");
    }
}
