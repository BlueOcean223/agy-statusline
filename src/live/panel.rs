//! Details for the side panel (Overview / Turns), built from the same engine state as the in-page bar.

use super::view::{BarPayload, Priced, ViewReq};
use super::{Clock, Engine};
use crate::call::Tokens;
use crate::config::{Config, OnlinePrices};
use crate::miss::{MissInfo, MissSummary};
use crate::pricing::Cost;
use crate::usage::AccountInfo;
use serde::Serialize;
use std::collections::HashSet;
use std::rc::Rc;

const SERIES_LEN: usize = 40;
const RECENT_LEN: usize = 15;
const TURNS_LEN: usize = 300;

impl Engine {
    /// Top-level conversations, most recently viewed or modified first. Includes conversations the
    /// language_server hasn't loaded yet (found through their annotation files).
    fn top_level_recent(&self) -> Vec<String> {
        // Annotation titles/view times come from the engine's cache (refreshed every 10s off-lock);
        // this runs on every 1.5s panel poll.
        let mut ids: HashSet<String> = self.annotations.keys().cloned().collect();
        ids.extend(self.summaries.iter().filter(|(_, s)| s.root.is_none()).map(|(id, _)| id.clone()));
        let mut scored: Vec<(i64, String)> = ids
            .into_iter()
            .filter(|id| self.summaries.get(id).is_none_or(|s| s.root.is_none()))
            .map(|id| {
                let viewed = self.annotations.get(&id).map(|(_, a)| a.viewed_ms).unwrap_or(0);
                let modified = self.summaries.get(&id).map(|s| s.modified_ms).unwrap_or(0);
                let disk = self.disk.get(&id).copied().unwrap_or(0);
                (viewed.max(modified).max(disk), id)
            })
            .collect();
        scored.sort_by(|a, b| b.cmp(a));
        scored.into_iter().map(|(_, id)| id).collect()
    }

    /// The conversation the side panel shows: `requested`, or else the most recent top-level one.
    pub fn panel_target(&self, requested: Option<&str>) -> Option<String> {
        match requested.filter(|s| !s.is_empty()) {
            Some(id) => Some(id.to_string()),
            None => self.top_level_recent().into_iter().next(),
        }
    }

    /// The side panel for `id` from what is loaded; load it first (`load_for_panel`, or
    /// `live::panel_json` in the sidecar).
    pub fn panel(&self, id: &str, cfg: &Config, online: Option<&OnlinePrices>, clock: Clock) -> PanelView {
        let req = ViewReq::new(self, cfg, online, clock);
        let now = clock.now;
        let recent_ids = self.top_level_recent();
        let id = id.to_string();
        let root = self.root_of(&id);
        let is_sub = root != id;
        let summary = self.summaries.get(&id).cloned().unwrap_or_default();

        let bar = req.bar(&HashSet::from([id.clone()]));
        // What the bar shows of this conversation; the panel adds the details.
        let view = &bar.views[&id];
        let own = req.calls(&id);
        // Calls counted in totals, the same as the bar's `view.cost_conv`.
        let convs: Vec<Rc<[Priced]>> = req.counted_ids(&id).iter().map(|cid| req.calls(cid)).collect();

        // Tokens and cost
        let (mut tokens, mut cost) = (Tokens::default(), Cost::default());
        let mut saved = 0.0;
        for &Priced { call: c, cost: k } in convs.iter().flat_map(|c| c.iter()) {
            tokens += &c.tokens;
            cost += &k;
            // What the cached tokens would have cost as fresh input.
            if c.tokens.cache_read > 0 && c.tokens.input > 0 && k.input > 0.0 {
                saved += c.tokens.cache_read as f64 * (k.input / c.tokens.input as f64) - k.cache;
            }
        }
        let first_at = own.iter().map(|p| p.call.at_ms).min().unwrap_or(0);
        let last_at = own.iter().map(|p| p.call.at_ms).max().unwrap_or(0);
        let wall_sec = ((last_at - first_at).max(0) as f64) / 1000.0;
        let rate_per_hr = (wall_sec > 600.0).then(|| view.cost_conv / (wall_sec / 3600.0));

        // Context
        let last = own.last().map(|p| p.call);
        let ctx_max = view.main.ctx_max;
        let ctx_used = last.map(|c| c.ctx_used).unwrap_or(0);
        let ctx_series: Vec<u64> = own.iter().rev().take(SERIES_LEN).rev().map(|p| p.call.ctx_used).collect();
        let deltas: Vec<i64> = ctx_series.windows(2).map(|w| w[1] as i64 - w[0] as i64).collect();
        let last_delta = deltas.last().copied().unwrap_or(0);
        let recent: Vec<i64> = deltas.iter().rev().take(5).copied().filter(|d| *d > 0).collect();
        let avg_growth = if recent.is_empty() { 0 } else { recent.iter().sum::<i64>() / recent.len() as i64 };
        let ctx_left = ctx_max.saturating_sub(ctx_used);
        let turns_left = (avg_growth > 0 && ctx_max > 0).then(|| ctx_left as i64 / avg_growth);

        // Cache & 6-cause miss analysis
        let session_hit = tokens.cache_hit();
        let own_misses = req.misses(&id);
        let mut miss_summary = MissSummary::default();
        for (p, m) in own.iter().zip(own_misses.iter()) {
            miss_summary.total_usd += p.cost.total();
            if let Some(info) = m {
                miss_summary.add(info);
            }
        }
        miss_summary.round_fields();
        let warm_left = last.map(|c| (c.at_ms + cfg.cache_ttl_sec as i64 * 1000 - now) / 1000).filter(|s| *s > 0);

        // Speed
        let (out_sum, stream_sum) = own
            .iter()
            .filter_map(|p| {
                p.call.stream_ms.filter(|ms| *ms > 50.0).map(|ms| (p.call.tokens.output as f64, ms / 1000.0))
            })
            .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        let ttfts: Vec<f64> = own.iter().filter_map(|p| p.call.ttft_ms).collect();
        let tps_series: Vec<f64> = own.iter().rev().take(SERIES_LEN).rev().filter_map(|p| p.call.tps()).collect();

        let subagents: Vec<SubagentRow> = if is_sub {
            Vec::new()
        } else {
            let mut subs: Vec<SubagentRow> = self
                .tree(&root)
                .into_iter()
                .filter(|x| *x != root)
                .map(|sid| {
                    let s = self.summaries.get(&sid).cloned().unwrap_or_default();
                    let calls = req.calls(&sid);
                    SubagentRow {
                        parent: s.parent,
                        in_ls: self.ls_loaded.contains(&sid),
                        sub_type: s.sub_type,
                        role: s.sub_role,
                        model: calls.last().map(|p| p.call.model.display.clone()),
                        tokens: calls.iter().map(|p| p.call.tokens.total()).sum(),
                        cost: calls.iter().map(|p| p.cost.total()).sum(),
                        running: self.is_running(&sid, now),
                        last_at: calls.last().map(|p| p.call.at_ms),
                        id: sid,
                    }
                })
                .collect();
            subs.sort_by_key(|s| (!s.running, std::cmp::Reverse(s.last_at)));
            subs
        };
        let sub_cost: f64 = subagents.iter().map(|s| s.cost).sum();

        // Only the most recent turns are sent: this is rebuilt and re-rendered on every 1.5s poll,
        // and agent-team conversations can have thousands of calls.
        let turns_total = own.len();
        let turns: Vec<Turn> = own
            .iter()
            .enumerate()
            .skip(turns_total.saturating_sub(TURNS_LEN))
            .map(|(idx, &Priced { call: c, cost })| {
                let c_usd = cost.total();
                Turn {
                    step: c.step(),
                    at: c.at_ms,
                    model: c.model.display.clone(),
                    ctx: c.ctx_used,
                    hit: c.tokens.cache_hit(),
                    out: c.tokens.output,
                    cost: c_usd,
                    costly: cfg.costly_call_usd > 0.0 && c_usd >= cfg.costly_call_usd,
                    miss: own_misses.get(idx).cloned().flatten(),
                    tps: c.tps(),
                    ttft_ms: c.ttft_ms,
                    label: c.label.clone().unwrap_or_else(|| "…".into()),
                }
            })
            .collect();

        let mut listed: HashSet<String> = HashSet::new();
        let recent: Vec<Recent> = std::iter::once(id.clone())
            .chain(recent_ids)
            .filter(|cid| listed.insert(cid.clone()))
            .take(RECENT_LEN + 1)
            .map(|cid| Recent { title: self.title(&cid), id: cid })
            .collect();

        let running = view.main.running;
        PanelView {
            title: self.title(&id),
            activity: if running {
                last.and_then(|c| c.label.clone()).map(|l| format!("Running · {l}")).unwrap_or_else(|| "Running".into())
            } else {
                "Idle".into()
            },
            workspace: summary.workspace,
            branch: summary.branch,
            account: self.account.clone(),
            recent,
            bar,
            ctx: PanelCtx { left: ctx_left, delta: last_delta, turns_left, series: ctx_series },
            cache: PanelCache {
                session_hit,
                misses: miss_summary.count,
                miss_summary,
                saved_usd: saved,
                warm_left_sec: warm_left,
                ttl_sec: cfg.cache_ttl_sec,
            },
            tokens: PanelTokens {
                fresh: tokens.input,
                cache: tokens.cache_read,
                cache_write: tokens.cache_write,
                out: tokens.output,
                total: tokens.total(),
                cost_in: cost.input,
                cost_cache: cost.cache,
                cost_cache_write: cost.cw,
                cost_out: cost.out,
                rate_per_hr,
                today_tokens: req.today().tokens,
            },
            speed: PanelSpeed {
                avg_tps: (stream_sum > 0.0).then(|| out_sum / stream_sum),
                avg_ttft_ms: (!ttfts.is_empty()).then(|| ttfts.iter().sum::<f64>() / ttfts.len() as f64),
                series: tps_series,
                wall_sec,
            },
            subagents,
            sub_cost,
            turns,
            turns_total,
            id,
            root,
            is_sub,
        }
    }

    fn title(&self, id: &str) -> String {
        // From the engine's annotation scan (every 10s, off-lock): a conversation created since
        // then shows its fallback name until the next scan.
        let t = self.annotations.get(id).map(|(_, a)| a.title.clone()).unwrap_or_default();
        if !t.is_empty() {
            return t;
        }
        match self.summaries.get(id) {
            Some(s) if s.sub_type.is_some() => {
                format!("{} · {}", s.sub_type.clone().unwrap_or_default(), s.sub_role.clone().unwrap_or_default())
            }
            _ => id.chars().take(8).collect(),
        }
    }
}

/// The side panel (`web/app.js`). What it shows of the conversation that the status bar shows
/// too (context used, last call, cost, quota, running) it reads from `bar.views[id]`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelView {
    pub id: String,
    pub root: String,
    pub is_sub: bool,
    pub title: String,
    pub activity: String,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub account: Option<AccountInfo>,
    /// This conversation, then the most recent top-level ones.
    pub recent: Vec<Recent>,
    /// The status bar for this conversation alone.
    pub bar: BarPayload,
    pub ctx: PanelCtx,
    pub cache: PanelCache,
    pub tokens: PanelTokens,
    pub speed: PanelSpeed,
    /// Every subagent of the tree (roots only), running first.
    pub subagents: Vec<SubagentRow>,
    pub sub_cost: f64,
    /// The last `TURNS_LEN` calls with usage.
    pub turns: Vec<Turn>,
    pub turns_total: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Recent {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelCtx {
    pub left: u64,
    /// Context growth over the last call.
    pub delta: i64,
    /// Calls left at the recent growth rate.
    pub turns_left: Option<i64>,
    pub series: Vec<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelCache {
    /// `None` when no call had a prompt to hit.
    pub session_hit: Option<f64>,
    pub misses: usize,
    pub miss_summary: MissSummary,
    /// What the cached tokens would have cost as fresh input, less what they cost.
    pub saved_usd: f64,
    pub warm_left_sec: Option<i64>,
    pub ttl_sec: u64,
}

/// Totals over the conversation, with its subagents when `bar.views[id].includesSubagents`, and
/// today's tokens across all conversations.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelTokens {
    pub fresh: u64,
    pub cache: u64,
    pub cache_write: u64,
    pub out: u64,
    pub total: u64,
    pub cost_in: f64,
    pub cost_cache: f64,
    pub cost_cache_write: f64,
    pub cost_out: f64,
    pub rate_per_hr: Option<f64>,
    pub today_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelSpeed {
    pub avg_tps: Option<f64>,
    pub avg_ttft_ms: Option<f64>,
    pub series: Vec<f64>,
    pub wall_sec: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentRow {
    pub id: String,
    pub parent: Option<String>,
    pub in_ls: bool,
    #[serde(rename = "type")]
    pub sub_type: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub tokens: u64,
    pub cost: f64,
    pub running: bool,
    pub last_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub step: Option<u64>,
    pub at: i64,
    pub model: String,
    pub ctx: u64,
    /// `None` when the call's prompt was empty.
    pub hit: Option<f64>,
    pub out: u64,
    pub cost: f64,
    pub costly: bool,
    pub miss: Option<MissInfo>,
    pub tps: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::test_support::*;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    /// The panel with what the side panel reads from `bar.views[id]` and `bar` put where the panel
    /// itself carried it before, so one snapshot covers what it shows. Mirrors how `web/app.js`
    /// reads them (`barView` and `renderOverview`): change both together.
    fn as_shown(p: &PanelView) -> Value {
        let mut out = serde_json::to_value(p).unwrap();
        let bar = out["bar"].clone();
        let view = &bar["views"][&p.id];
        let m = &view["main"];
        let moved = [
            ("ctx", "used", json!(m["ctxUsed"].as_u64().unwrap_or(0))),
            ("ctx", "max", m["ctxMax"].clone()),
            ("ctx", "compactions", m["compactions"].clone()),
            ("cache", "lastHit", m["cacheHit"].clone()),
            ("speed", "lastTps", m["tps"].clone()),
            ("speed", "lastTtftMs", m["ttftMs"].clone()),
            ("tokens", "cost", view["costConv"].clone()),
            ("tokens", "includesSubagents", view["includesSubagents"].clone()),
            ("tokens", "todayCost", bar["today"].clone()),
            ("tokens", "todayCliCost", bar["todayCli"].clone()),
            ("", "quota", view["quota"].clone()),
            ("", "running", m["running"].clone()),
        ];
        for (section, key, value) in moved {
            let obj = if section.is_empty() { &mut out } else { &mut out[section] };
            let obj = obj.as_object_mut().unwrap();
            assert!(!obj.contains_key(key), "{section}.{key} is in the panel and the bar");
            obj.insert(key.to_string(), value);
        }
        out
    }

    fn snapshot_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/live/snapshots").join(format!("{name}.json"))
    }

    /// Finds where `actual` differs from `expected`. Numbers may differ by rounding, since a total
    /// summed in another order can change in its last bits.
    fn diff(path: &str, expected: &Value, actual: &Value) -> Option<String> {
        match (expected, actual) {
            (Value::Object(e), Value::Object(a)) => {
                let mut keys: Vec<&String> = e.keys().chain(a.keys()).collect();
                keys.sort();
                keys.dedup();
                keys.into_iter().find_map(|k| match (e.get(k), a.get(k)) {
                    (Some(ev), Some(av)) => diff(&format!("{path}.{k}"), ev, av),
                    (Some(ev), None) => Some(format!("{path}.{k}: expected {ev}, missing")),
                    (_, av) => Some(format!("{path}.{k}: unexpected {}", av.unwrap_or(&Value::Null))),
                })
            }
            (Value::Array(e), Value::Array(a)) if e.len() != a.len() => {
                Some(format!("{path}: expected {} items, got {}", e.len(), a.len()))
            }
            (Value::Array(e), Value::Array(a)) => {
                e.iter().zip(a).enumerate().find_map(|(i, (ev, av))| diff(&format!("{path}[{i}]"), ev, av))
            }
            (Value::Number(e), Value::Number(a)) => {
                let (e, a) = (e.as_f64().unwrap_or(f64::NAN), a.as_f64().unwrap_or(f64::NAN));
                ((e - a).abs() > 1e-9 * e.abs().max(a.abs()).max(1e-3))
                    .then(|| format!("{path}: expected {e}, got {a}"))
            }
            _ => (expected != actual).then(|| format!("{path}: expected {expected}, got {actual}")),
        }
    }

    /// Rounds floats to 12 significant digits, so a rewritten snapshot doesn't change in its last
    /// bits with the order totals were summed in.
    fn rounded(v: &Value) -> Value {
        match v {
            Value::Number(n) if n.is_f64() => {
                let f = n.as_f64().unwrap_or(0.0);
                let r = format!("{f:.11e}").parse::<f64>().unwrap_or(f);
                serde_json::Number::from_f64(r).map_or(Value::Null, Value::Number)
            }
            Value::Array(a) => Value::Array(a.iter().map(rounded).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), rounded(v))).collect()),
            _ => v.clone(),
        }
    }

    /// Compares `actual` with the stored snapshot `name`; `UPDATE_SNAPSHOTS=1` rewrites it instead.
    fn assert_snapshot(name: &str, actual: &Value) {
        let path = snapshot_path(name);
        if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_string_pretty(&rounded(actual)).unwrap() + "\n").unwrap();
            return;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_SNAPSHOTS=1 to create it)", path.display()));
        let expected: Value = serde_json::from_str(&text).unwrap();
        if let Some(d) = diff("$", &expected, actual) {
            panic!("{name} snapshot differs at {d}");
        }
    }

    #[test]
    fn panel_of_a_root_conversation_matches_its_snapshot() {
        let (e, ids) = view_fixture();
        assert_snapshot("panel_root", &as_shown(&e.panel(&ids.root, &view_config(), None, clock())));
    }

    #[test]
    fn panel_of_a_root_without_subagent_totals_matches_its_snapshot() {
        let (e, ids) = view_fixture();
        let cfg = Config { include_subagents: false, ..view_config() };
        assert_snapshot("panel_root_own", &as_shown(&e.panel(&ids.root, &cfg, None, clock())));
    }

    #[test]
    fn panel_of_a_subagent_matches_its_snapshot() {
        let (e, ids) = view_fixture();
        assert_snapshot("panel_subagent", &as_shown(&e.panel(&ids.running, &view_config(), None, clock())));
    }

    #[test]
    fn panel_of_an_idle_subagent_matches_its_snapshot() {
        let (e, ids) = view_fixture();
        assert_snapshot("panel_idle_subagent", &as_shown(&e.panel(&ids.old, &view_config(), None, clock())));
    }

    #[test]
    fn bar_payload_matches_its_snapshot() {
        let (e, ids) = view_fixture();
        let panes = HashSet::from([ids.root.clone(), ids.running.clone(), ids.other.clone()]);
        let bar = e.build_payload(&panes, &view_config(), None, clock());
        assert_snapshot("bar", &serde_json::to_value(bar).unwrap());
    }

    #[test]
    fn snapshot_diff_tolerates_rounding_but_not_changes() {
        let e = json!({ "a": [1.0, { "b": 0.1 }], "c": "x" });
        assert_eq!(diff("$", &e, &json!({ "a": [1.0, { "b": 0.1 + 1e-15 }], "c": "x" })), None);
        assert!(diff("$", &e, &json!({ "a": [1.0, { "b": 0.2 }], "c": "x" })).unwrap().starts_with("$.a[1].b"));
        assert!(diff("$", &e, &json!({ "a": [1.0], "c": "x" })).is_some());
        assert!(diff("$", &e, &json!({ "a": [1.0, { "b": 0.1 }] })).unwrap().starts_with("$.c"));
        assert!(diff("$", &json!({ "n": null }), &json!({ "n": 0 })).is_some());
        assert!(diff("$", &e, &json!({ "a": [1.0, { "b": 0.1 }], "c": "x", "d": 1 })).unwrap().starts_with("$.d"));
        assert_eq!(rounded(&json!([0.8598499999999999, 3, null])), json!([0.85985, 3, null]));
    }
}
