//! The status bar's view of conversations, shared with the side panel. The views built for one
//! request share a [`ViewReq`], which prices each conversation's calls, analyses its cache misses
//! and adds up today's totals once. (Today's totals keep their own per-conversation cache across
//! requests; see `Engine::today_totals`.)

use super::{Clock, Engine, LiveCall, MAX_SUBS, parse_ts};
use crate::config::{BarLook, BarSegments, Config, OnlinePrices};
use crate::git::GitInfo;
use crate::miss::{MissInfo, miss_scan};
use crate::pricing::{Cost, Pricer};
use crate::quota::group_of;
use serde::Serialize;
use serde_json::Value;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

/// A call with usage and its cost.
#[derive(Clone, Copy)]
pub(super) struct Priced<'a> {
    pub call: &'a LiveCall,
    pub cost: Cost,
}

/// Today's cost and tokens across conversations, including the `agy CLI`'s.
#[derive(Clone, Copy, Debug)]
pub(super) struct TodayTotals {
    pub usd: f64,
    pub tokens: u64,
    pub cli_usd: f64,
}

/// What the views built for one request share: the configuration, one clock reading, the prices,
/// and each conversation's priced calls, cache misses and today's totals, each computed once.
pub(super) struct ViewReq<'a> {
    eng: &'a Engine,
    cfg: &'a Config,
    pricer: Pricer,
    clock: Clock,
    calls: RefCell<HashMap<String, Rc<[Priced<'a>]>>>,
    misses: RefCell<HashMap<String, Rc<[Option<MissInfo>]>>>,
    today: OnceCell<TodayTotals>,
}

impl<'a> ViewReq<'a> {
    pub fn new(eng: &'a Engine, cfg: &'a Config, online: Option<&OnlinePrices>, clock: Clock) -> Self {
        Self {
            eng,
            cfg,
            pricer: Pricer::new(&cfg.models, online, clock.now),
            clock,
            calls: RefCell::default(),
            misses: RefCell::default(),
            today: OnceCell::new(),
        }
    }

    /// `id`'s calls with usage, in order, with their costs.
    pub fn calls(&self, id: &str) -> Rc<[Priced<'a>]> {
        if let Some(c) = self.calls.borrow().get(id) {
            return c.clone();
        }
        let eng: &'a Engine = self.eng;
        let priced: Rc<[Priced<'a>]> = eng
            .convs
            .get(id)
            .into_iter()
            .flat_map(|conv| conv.usage_calls())
            .map(|call| Priced { call, cost: self.pricer.call_cost(call) })
            .collect();
        self.calls.borrow_mut().insert(id.to_string(), priced.clone());
        priced
    }

    /// 6-cause cache-miss analysis of `id`'s calls with usage, one entry per call.
    pub fn misses(&self, id: &str) -> Rc<[Option<MissInfo>]> {
        if let Some(m) = self.misses.borrow().get(id) {
            return m.clone();
        }
        let markers = self.eng.convs.get(id).and_then(|conv| conv.transcript.as_ref()).map(|t| &t.markers);
        // One entry per `self.calls(id)`, in the same order.
        let misses: Rc<[Option<MissInfo>]> =
            miss_scan(self.calls(id).iter().map(|p| &p.call.call), &self.pricer, markers).into();
        self.misses.borrow_mut().insert(id.to_string(), misses.clone());
        misses
    }

    /// The conversations whose calls count in `id`'s totals: `id`, then its subagents when it is a
    /// root and subagents are included.
    pub fn counted_ids(&self, id: &str) -> Vec<String> {
        if self.includes_subagents(id) { self.eng.tree(id) } else { vec![id.to_string()] }
    }

    fn includes_subagents(&self, id: &str) -> bool {
        self.cfg.include_subagents && self.eng.root_of(id) == id
    }

    pub fn today(&self) -> TodayTotals {
        *self.today.get_or_init(|| self.eng.today_totals(&self.pricer, &self.clock.today))
    }

    /// The status bar's payload for the conversations in `panes`.
    pub fn bar(&self, panes: &HashSet<String>) -> BarPayload {
        let today = self.today();
        let quota_groups = self.eng.quota["groups"]
            .as_array()
            .map(|gs| {
                gs.iter()
                    .map(|g| QuotaGroup {
                        name: g["displayName"].clone(),
                        models: g["description"].clone(),
                        buckets: quota_views(g, self.clock.now),
                    })
                    .collect()
            })
            .unwrap_or_default();
        BarPayload {
            today: today.usd,
            today_cli: today.cli_usd,
            costly_threshold: self.cfg.costly_call_usd,
            price_source: self.pricer.source().to_string(),
            quota_groups,
            views: panes.iter().map(|id| (id.clone(), self.view(id))).collect(),
            opts: BarOpts {
                enabled: self.cfg.bar_enabled,
                seg: self.cfg.bar_segments.clone(),
                look: self.cfg.bar_look.clone(),
            },
            error: self.eng.last_error.clone(),
        }
    }

    fn agent_line(&self, id: &str) -> AgentLine {
        let (eng, cfg) = (self.eng, self.cfg);
        let calls = self.calls(id);
        let last = calls.last().map(|p| p.call);
        let ctx_max = if cfg.context_limit_override > 0 {
            cfg.context_limit_override
        } else {
            last.map(|c| c.ctx_max).unwrap_or(0)
        };
        let mut by_ck = 0;
        let mut by_drop = 0;
        let mut prev_ck = -1;
        let mut prev_used = 0u64;
        let mut own_cost = 0.0;
        let mut first_call_at = 0i64;
        for &Priced { call: c, cost } in calls.iter() {
            own_cost += cost.total();
            if c.at_ms > 0 && (first_call_at == 0 || c.at_ms < first_call_at) {
                first_call_at = c.at_ms;
            }
            if c.checkpoint > prev_ck {
                if c.checkpoint >= 0 {
                    by_ck += 1;
                }
                prev_ck = c.checkpoint;
            }
            if prev_used > 100_000 && c.ctx_used > 0 && (c.ctx_used as f64) < prev_used as f64 * 0.6 {
                by_drop += 1;
            }
            if c.ctx_used > 0 {
                prev_used = c.ctx_used;
            }
        }
        let compactions = by_ck.max(by_drop);
        let s = eng.summaries.get(id);
        let sc = eng.sub_steps.get(id);

        let started_ms = [s.map(|x| x.created_ms).unwrap_or(0), sc.map(|x| x.first_at).unwrap_or(0), first_call_at]
            .into_iter()
            .filter(|&t| t > 0)
            .min();

        let (waiting, current_step, timeline, ended_ms, step_total) = if let Some(sc) = sc {
            let active_step = sc
                .steps
                .iter()
                .rev()
                .find(|(_, st)| st.status.contains("WAIT") || st.status == "RUNNING" || st.status == "GENERATING")
                .or_else(|| sc.steps.iter().next_back());
            let waiting = active_step.is_some_and(|(_, st)| st.status.contains("WAIT"));
            let cur = active_step.map(|(k, st)| format!("{} (step {})", st.text, k + 1));
            let tl: Vec<String> = sc
                .steps
                .iter()
                .rev()
                .take(5)
                .rev()
                .map(|(k, st)| {
                    let sym = if st.status.contains("WAIT") {
                        "?"
                    } else if st.status == "ERROR" {
                        "✗"
                    } else if st.status == "DONE" {
                        "✓"
                    } else {
                        "●"
                    };
                    format!("{sym} #{} {}", k + 1, st.text)
                })
                .collect();
            let end = sc.steps.values().map(|st| st.t1.max(st.t0)).max().filter(|&t| t > 0);
            (waiting, cur, tl, end, sc.total)
        } else {
            (false, None, Vec::new(), None, s.map(|x| x.step_count as usize).unwrap_or(0))
        };

        let ended_ms = ended_ms.or_else(|| last.map(|c| c.at_ms).filter(|&t| t > 0));

        AgentLine {
            id: id.to_string(),
            sub_type: s.and_then(|s| s.sub_type.clone()),
            sub_role: s.and_then(|s| s.sub_role.clone()),
            running: eng.is_running(id, self.clock.now),
            waiting,
            model: last.map(|c| c.model.display.clone()),
            resp_model: last.map(|c| c.model.raw.clone()),
            ctx_used: last.map(|c| c.ctx_used),
            ctx_max,
            compactions,
            cache_read: last.map(|c| c.tokens.cache_read),
            cache_write: last.map(|c| c.tokens.cache_write),
            cache_hit: last.and_then(|c| c.tokens.cache_hit()),
            ttft_ms: last.and_then(|c| c.ttft_ms),
            tps: last.and_then(|c| c.tps()),
            out_tokens: last.map(|c| c.tokens.output),
            cost: own_cost,
            started_ms,
            ended_ms,
            step_total,
            current_step,
            timeline,
            last_at: last.map(|c| c.at_ms),
            calls: calls.len(),
        }
    }

    fn view(&self, id: &str) -> View {
        let (eng, cfg) = (self.eng, self.cfg);
        let root = eng.root_of(id);
        let is_sub = root != id;
        let main = self.agent_line(id);
        let includes_subagents = self.includes_subagents(id);
        let ids = self.counted_ids(id);
        let mut cost_conv = 0.0;
        let mut cost_conv_today = 0.0;
        let mut multi_day = false;
        for cid in &ids {
            for p in self.calls(cid).iter() {
                let cost = p.cost.total();
                cost_conv += cost;
                if p.call.at_ms >= self.clock.today.start_ms() {
                    cost_conv_today += cost;
                } else if p.call.at_ms > 0 {
                    multi_day = true;
                }
            }
        }
        let last = self.calls(id).last().copied();
        let cost_last = last.map(|p| p.cost);
        let last_costly = cost_last.is_some_and(|k| cfg.costly_call_usd > 0.0 && k.total() >= cfg.costly_call_usd);
        let last_call_key = last.map(|p| format!("{}:{}:{}", id, main.calls, p.call.at_ms));
        let last_miss = self.misses(id).last().cloned().flatten();
        let price = last.and_then(|p| self.pricer.rule(&p.call.model)).map(|r| r.display_name.clone());

        let mut subs = Vec::new();
        if !is_sub {
            let shown = eng.shown_subs(&root, self.clock.now);
            let more = shown.len().saturating_sub(MAX_SUBS);
            subs = shown.iter().take(MAX_SUBS).map(|sid| self.agent_line(sid)).collect();
            if more > 0 {
                subs.push(AgentLine { id: format!("+{more}"), ..Default::default() });
            }
        }

        let summary = eng.summaries.get(id);
        let git =
            summary.and_then(|s| s.workspace_path.as_ref()).and_then(|p| eng.git.get(p)).and_then(|(_, g)| g.clone());
        View {
            git,
            workspace: summary.and_then(|s| s.workspace_path.clone()),
            main,
            is_sub,
            cost_conv,
            cost_conv_today,
            multi_day,
            includes_subagents,
            cost_last,
            last_call_costly: last_costly,
            last_call_key,
            last_call_miss: last_miss,
            price_name: price,
            quota: self.quota_for(last.map(|p| p.call)),
            subs,
        }
    }

    /// The quota windows of the group `call`'s model counts against.
    fn quota_for(&self, call: Option<&LiveCall>) -> Vec<QuotaView> {
        let groups = self.eng.quota["groups"].as_array().map(Vec::as_slice).unwrap_or_default();
        match group_of(call.map(|c| &c.model), groups) {
            Some(group) => quota_views(group, self.clock.now),
            None => Vec::new(),
        }
    }
}

impl Engine {
    /// The status bar's payload for the conversations in `panes`, as of `clock`.
    pub fn build_payload(
        &self,
        panes: &HashSet<String>,
        cfg: &Config,
        online: Option<&OnlinePrices>,
        clock: Clock,
    ) -> BarPayload {
        ViewReq::new(self, cfg, online, clock).bar(panes)
    }
}

/// What the in-page status bar (`web/bar.js`) renders.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BarPayload {
    /// Today's cost across conversations, including the `agy CLI`'s.
    pub today: f64,
    pub today_cli: f64,
    pub costly_threshold: f64,
    pub price_source: String,
    pub quota_groups: Vec<QuotaGroup>,
    /// One per pane, by conversation id.
    pub views: BTreeMap<String, View>,
    pub opts: BarOpts,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct QuotaGroup {
    /// As the language_server reports them (null when missing).
    pub name: Value,
    pub models: Value,
    pub buckets: Vec<QuotaView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BarOpts {
    pub enabled: bool,
    pub seg: BarSegments,
    pub look: BarLook,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLine {
    pub id: String,
    pub sub_type: Option<String>,
    pub sub_role: Option<String>,
    pub running: bool,
    pub waiting: bool,
    pub model: Option<String>,
    pub resp_model: Option<String>,
    pub ctx_used: Option<u64>,
    pub ctx_max: u64,
    pub compactions: usize,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub cache_hit: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub tps: Option<f64>,
    pub out_tokens: Option<u64>,
    pub cost: f64,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub step_total: usize,
    pub current_step: Option<String>,
    pub timeline: Vec<String>,
    pub last_at: Option<i64>,
    pub calls: usize,
}

/// One conversation as the status bar shows it, and the side panel's source for what both show.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub main: AgentLine,
    pub is_sub: bool,
    /// The conversation's cost, with its subagents' when `includes_subagents`.
    pub cost_conv: f64,
    pub cost_conv_today: f64,
    pub multi_day: bool,
    pub includes_subagents: bool,
    pub cost_last: Option<Cost>,
    pub last_call_costly: bool,
    pub last_call_key: Option<String>,
    pub last_call_miss: Option<MissInfo>,
    pub price_name: Option<String>,
    pub quota: Vec<QuotaView>,
    pub subs: Vec<AgentLine>,
    pub git: Option<GitInfo>,
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaView {
    pub window: String,
    pub used: f64,
    pub reset_ms: Option<i64>,
    /// Used fraction minus elapsed fraction of the window; > 0 means burning faster than even pace.
    pub pace: Option<f64>,
    /// Fraction of the window already elapsed.
    pub elapsed: Option<f64>,
}

fn quota_views(group: &Value, now: i64) -> Vec<QuotaView> {
    let mut out: Vec<QuotaView> = group["buckets"]
        .as_array()
        .map(|bs| {
            bs.iter()
                .map(|b| {
                    let window = b["window"].as_str().unwrap_or("").to_string();
                    let remaining = b["remainingFraction"].as_f64().unwrap_or(1.0).clamp(0.0, 1.0);
                    let reset_ms = b["resetTime"].as_str().and_then(parse_ts);
                    let span_ms = match window.as_str() {
                        "5h" => Some(5 * 3_600_000),
                        "weekly" => Some(7 * 86_400_000),
                        _ => None,
                    };
                    let elapsed = match (reset_ms, span_ms) {
                        (Some(r), Some(span)) => Some(1.0 - ((r - now) as f64 / span as f64).clamp(0.0, 1.0)),
                        _ => None,
                    };
                    // Rounded so the pushed payload only changes when the shown numbers can.
                    let elapsed = elapsed.map(|e| (e * 1000.0).round() / 1000.0);
                    let pace = elapsed.map(|e| (((1.0 - remaining) - e) * 1000.0).round() / 1000.0);
                    QuotaView { window, used: 1.0 - remaining, reset_ms, pace, elapsed }
                })
                .collect()
        })
        .unwrap_or_default();
    // 5h first, then weekly.
    out.sort_by_key(|q| if q.window == "5h" { 0 } else { 1 });
    out
}
