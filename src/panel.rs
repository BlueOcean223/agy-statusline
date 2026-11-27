//! Details for the side panel (Overview / Turns), built from the same engine state as the in-page bar.

use crate::config::{Config, OnlinePrices};
use crate::live::{Engine, LiveCall, now_ms};
use crate::miss::MissSummary;
use crate::pricing::Pricer;
use crate::transcript::read_annotation;
use serde_json::{Value, json};
use std::collections::HashSet;

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

    /// Makes sure `id` and its subagents are loaded, for a panel request between engine ticks.
    /// Conversations the language_server hasn't loaded are read from their databases.
    fn ensure_loaded(&mut self, id: &str) {
        if !self.convs.contains_key(id) {
            let _ = self.refresh_conv(id);
        }
        // Reading `id` may have revealed its root (a subagent opened from the list).
        let root = self.root_of(id);
        for cid in self.tree(&root) {
            if !self.convs.contains_key(&cid) {
                let _ = self.refresh_conv(&cid);
            }
        }
        self.load_subagent_hints(&[id.to_string(), root]);
    }

    pub fn panel(&mut self, requested: Option<&str>, cfg: &Config, online: Option<&OnlinePrices>) -> Value {
        let recent_ids = self.top_level_recent();
        let id = match requested.filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => match recent_ids.first() {
                Some(id) => id.clone(),
                None => return json!({ "id": null }),
            },
        };
        self.ensure_loaded(&id);
        let pricer = Pricer::new(&cfg.models, online, now_ms());
        let root = self.root_of(&id);
        let is_sub = root != id;
        let summary = self.summaries.get(&id).cloned().unwrap_or_default();

        // Calls counted in totals: this conversation, plus its subagents when configured (roots only).
        let mut ids = vec![id.clone()];
        if !is_sub && cfg.include_subagents {
            ids.extend(self.tree(&root).into_iter().filter(|x| *x != id));
        }
        let own: Vec<&LiveCall> =
            self.convs.get(&id).map(|c| c.calls.iter().filter(|c| c.has_usage()).collect()).unwrap_or_default();
        let all: Vec<&LiveCall> = ids
            .iter()
            .filter_map(|cid| self.convs.get(cid))
            .flat_map(|c| c.calls.iter().filter(|c| c.has_usage()))
            .collect();

        // Tokens and cost
        let (mut fresh, mut cache, mut cache_write, mut out) = (0u64, 0u64, 0u64, 0u64);
        let (mut c_in, mut c_cache, mut c_cw, mut c_out) = (0.0, 0.0, 0.0, 0.0);
        let mut saved = 0.0;
        for c in &all {
            fresh += c.tokens.input;
            cache += c.tokens.cache_read;
            cache_write += c.tokens.cache_write;
            out += c.tokens.output;
            let k = pricer.call_cost(c);
            c_in += k.input;
            c_cache += k.cache;
            c_cw += k.cw;
            c_out += k.out;
            // What the cached tokens would have cost as fresh input.
            if c.tokens.cache_read > 0 && c.tokens.input > 0 && k.input > 0.0 {
                saved += c.tokens.cache_read as f64 * (k.input / c.tokens.input as f64) - k.cache;
            }
        }
        let cost = c_in + c_cache + c_cw + c_out;
        let first_at = own.iter().map(|c| c.at_ms).min().unwrap_or(0);
        let last_at = own.iter().map(|c| c.at_ms).max().unwrap_or(0);
        let wall_sec = ((last_at - first_at).max(0) as f64) / 1000.0;
        let rate_per_hr = (wall_sec > 600.0).then(|| cost / (wall_sec / 3600.0));

        let (today_cost, today_tokens, today_cli_cost) = self.today_totals(&pricer);

        // Context
        let last = own.last().copied();
        let ctx_max = if cfg.context_limit_override > 0 {
            cfg.context_limit_override
        } else {
            last.map(|c| c.ctx_max).unwrap_or(0)
        };
        let ctx_used = last.map(|c| c.ctx_used).unwrap_or(0);
        let ctx_series: Vec<u64> = own.iter().rev().take(SERIES_LEN).rev().map(|c| c.ctx_used).collect();
        let deltas: Vec<i64> = ctx_series.windows(2).map(|w| w[1] as i64 - w[0] as i64).collect();
        let last_delta = deltas.last().copied().unwrap_or(0);
        let recent: Vec<i64> = deltas.iter().rev().take(5).copied().filter(|d| *d > 0).collect();
        let avg_growth = if recent.is_empty() { 0 } else { recent.iter().sum::<i64>() / recent.len() as i64 };
        let ctx_left = ctx_max.saturating_sub(ctx_used);
        let turns_left = (avg_growth > 0 && ctx_max > 0).then(|| ctx_left as i64 / avg_growth);

        // Cache & 6-cause miss analysis
        let hit = |c: &LiveCall| {
            let d = c.tokens.cache_read + c.tokens.input;
            if d == 0 { 0.0 } else { c.tokens.cache_read as f64 / d as f64 }
        };
        let session_hit = if fresh + cache > 0 { cache as f64 / (fresh + cache) as f64 } else { 0.0 };
        let own_misses = self.conv_misses(&id, &pricer);
        let mut miss_summary = MissSummary::default();
        for (c, m) in own.iter().zip(own_misses.iter()) {
            miss_summary.total_usd += pricer.call_cost(c).total();
            if let Some(info) = m {
                miss_summary.add(info);
            }
        }
        miss_summary.round_fields();
        let misses = miss_summary.count;
        let warm_left = last.map(|c| (c.at_ms + cfg.cache_ttl_sec as i64 * 1000 - now_ms()) / 1000).filter(|s| *s > 0);

        // Speed
        let (out_sum, stream_sum) = own
            .iter()
            .filter_map(|c| c.stream_ms.filter(|ms| *ms > 50.0).map(|ms| (c.tokens.output as f64, ms / 1000.0)))
            .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        let ttfts: Vec<f64> = own.iter().filter_map(|c| c.ttft_ms).collect();
        let tps_series: Vec<f64> = own.iter().rev().take(SERIES_LEN).rev().filter_map(|c| c.tps()).collect();

        let subagents: Vec<Value> = if is_sub {
            Vec::new()
        } else {
            let mut subs: Vec<Value> = self
                .tree(&root)
                .into_iter()
                .filter(|x| *x != root)
                .map(|sid| {
                    let s = self.summaries.get(&sid).cloned().unwrap_or_default();
                    let calls: Vec<&LiveCall> = self
                        .convs
                        .get(&sid)
                        .map(|c| c.calls.iter().filter(|c| c.has_usage()).collect())
                        .unwrap_or_default();
                    json!({
                        "id": sid,
                        "parent": s.parent,
                        "inLs": self.ls_loaded.contains(&sid),
                        "type": s.sub_type,
                        "role": s.sub_role,
                        "model": calls.last().map(|c| c.model.display.clone()),
                        "tokens": calls.iter().map(|c| c.tokens.input + c.tokens.cache_read + c.tokens.cache_write + c.tokens.output).sum::<u64>(),
                        "cost": calls.iter().map(|c| pricer.call_cost(c).total()).sum::<f64>(),
                        "running": self.is_running(&sid),
                        "lastAt": calls.last().map(|c| c.at_ms),
                    })
                })
                .collect();
            subs.sort_by_key(|v| (!v["running"].as_bool().unwrap_or(false), std::cmp::Reverse(v["lastAt"].as_i64())));
            subs
        };
        let sub_cost: f64 = subagents.iter().filter_map(|v| v["cost"].as_f64()).sum();

        // Only the most recent turns are sent: this is rebuilt and re-rendered on every 1.5s poll,
        // and agent-team conversations can have thousands of calls.
        let turns_total = own.len();
        let turns: Vec<Value> = own
            .iter()
            .enumerate()
            .skip(turns_total.saturating_sub(TURNS_LEN))
            .map(|(idx, c)| {
                let c_usd = pricer.call_cost(c).total();
                let costly = cfg.costly_call_usd > 0.0 && c_usd >= cfg.costly_call_usd;
                let miss = own_misses.get(idx).cloned().flatten();
                json!({
                    "step": c.steps.first(),
                    "at": c.at_ms,
                    "model": c.model.display,
                    "ctx": c.ctx_used,
                    "hit": hit(c),
                    "out": c.tokens.output,
                    "cost": c_usd,
                    "costly": costly,
                    "miss": miss,
                    "tps": c.tps(),
                    "ttftMs": c.ttft_ms,
                    "label": c.label.clone().unwrap_or_else(|| "…".into()),
                })
            })
            .collect();

        let mut listed: HashSet<String> = HashSet::new();
        let recent: Vec<Value> = std::iter::once(id.clone())
            .chain(recent_ids)
            .filter(|cid| listed.insert(cid.clone()))
            .take(RECENT_LEN + 1)
            .map(|cid| json!({ "id": cid, "title": self.title(&cid) }))
            .collect();

        let running = self.is_running(&id);
        let panes: HashSet<String> = [id.clone()].into();
        json!({
            "id": id,
            "root": root,
            "isSub": is_sub,
            "title": self.title(&id),
            "running": running,
            "activity": if running { last.and_then(|c| c.label.clone()).map(|l| format!("Running · {l}")).unwrap_or_else(|| "Running".into()) } else { "Idle".into() },
            "workspace": summary.workspace,
            "branch": summary.branch,
            "account": self.account,
            "recent": recent,
            "bar": self.build_payload(&panes, cfg, online),
            "ctx": {
                "used": ctx_used, "max": ctx_max, "left": ctx_left, "delta": last_delta,
                "turnsLeft": turns_left, "series": ctx_series,
                "compactions": self.view(&id, cfg, &pricer).main.compactions,
            },
            "cache": {
                "lastHit": last.map(hit), "sessionHit": session_hit, "misses": misses,
                "missSummary": miss_summary,
                "savedUsd": saved, "warmLeftSec": warm_left, "ttlSec": cfg.cache_ttl_sec,
            },
            "tokens": {
                "fresh": fresh, "cache": cache, "cacheWrite": cache_write, "out": out,
                "total": fresh + cache + cache_write + out,
                "costIn": c_in, "costCache": c_cache, "costCacheWrite": c_cw, "costOut": c_out, "cost": cost,
                "ratePerHr": rate_per_hr, "todayCost": today_cost, "todayCliCost": today_cli_cost, "todayTokens": today_tokens,
                "includesSubagents": !is_sub && cfg.include_subagents,
            },
            "speed": {
                "lastTps": last.and_then(|c| c.tps()),
                "avgTps": (stream_sum > 0.0).then(|| out_sum / stream_sum),
                "lastTtftMs": last.and_then(|c| c.ttft_ms),
                "avgTtftMs": (!ttfts.is_empty()).then(|| ttfts.iter().sum::<f64>() / ttfts.len() as f64),
                "series": tps_series,
                "wallSec": wall_sec,
            },
            "quota": self.quota_for(last),
            "subagents": subagents,
            "subCost": sub_cost,
            "turns": turns,
            "turnsTotal": turns_total,
        })
    }

    fn title(&self, id: &str) -> String {
        let t = match self.annotations.get(id) {
            Some((_, a)) => a.title.clone(),
            // Created since the last annotation scan.
            None => read_annotation(&self.app_data, id).map(|a| a.title).unwrap_or_default(),
        };
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
