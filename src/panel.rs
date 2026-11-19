//! Details for the side panel (Overview / Turns), built from the same engine state as the in-page bar.

use crate::config::Config;
use crate::live::{Call, Engine, call_cost, local_midnight_ms, now_ms};
use crate::transcript::read_annotation;
use serde_json::{Value, json};
use std::collections::HashSet;

const SERIES_LEN: usize = 40;
const RECENT_LEN: usize = 15;

impl Engine {
    /// Root conversation the user looked at most recently (annotation view time, else last modified).
    fn auto_conversation(&self) -> Option<String> {
        self.summaries
            .iter()
            .filter(|(_, s)| s.root.is_none())
            .map(|(id, s)| {
                let viewed = read_annotation(&self.app_data, id).map(|a| a.viewed_ms).unwrap_or(0);
                (viewed.max(s.modified_ms), id)
            })
            .max()
            .map(|(_, id)| id.clone())
    }

    /// Makes sure `id` and its subagents are loaded, for a panel request between engine ticks.
    fn ensure_loaded(&mut self, id: &str) {
        let root = self.root_of(id);
        let mut ids = self.tree(&root);
        ids.push(id.to_string());
        for cid in ids {
            if !self.convs.contains_key(&cid) {
                let _ = self.refresh_conv(&cid);
            }
        }
    }

    pub fn panel(&mut self, requested: Option<&str>, cfg: &Config) -> Value {
        let id = match requested.filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => match self.auto_conversation() {
                Some(id) => id,
                None => return json!({ "id": null }),
            },
        };
        self.ensure_loaded(&id);
        let rules = &cfg.models;
        let root = self.root_of(&id);
        let is_sub = root != id;
        let summary = self.summaries.get(&id).cloned().unwrap_or_default();

        // Calls counted in totals: this conversation, plus its subagents when configured (roots only).
        let mut ids = vec![id.clone()];
        if !is_sub && cfg.include_subagents {
            ids.extend(self.tree(&root).into_iter().filter(|x| *x != id));
        }
        let own: Vec<&Call> = self.convs.get(&id).map(|c| c.calls.iter().filter(|c| c.has_usage).collect()).unwrap_or_default();
        let all: Vec<&Call> = ids
            .iter()
            .filter_map(|cid| self.convs.get(cid))
            .flat_map(|c| c.calls.iter().filter(|c| c.has_usage))
            .collect();

        // Tokens and cost
        let (mut fresh, mut cache, mut out) = (0u64, 0u64, 0u64);
        let (mut c_in, mut c_cache, mut c_out) = (0.0, 0.0, 0.0);
        let mut saved = 0.0;
        for c in &all {
            fresh += c.input;
            cache += c.cache;
            out += c.output;
            let k = call_cost(rules, c);
            c_in += k.input;
            c_cache += k.cache;
            c_out += k.out;
            // What the cached tokens would have cost as fresh input.
            if c.cache > 0 && c.input > 0 && k.input > 0.0 {
                saved += c.cache as f64 * (k.input / c.input as f64) - k.cache;
            }
        }
        let cost = c_in + c_cache + c_out;
        let first_at = own.iter().map(|c| c.at_ms).min().unwrap_or(0);
        let last_at = own.iter().map(|c| c.at_ms).max().unwrap_or(0);
        let wall_sec = ((last_at - first_at).max(0) as f64) / 1000.0;
        let rate_per_hr = (wall_sec > 600.0).then(|| cost / (wall_sec / 3600.0));

        let midnight = local_midnight_ms();
        let (mut today_cost, mut today_tokens) = (0.0, 0u64);
        for conv in self.convs.values() {
            for c in conv.calls.iter().filter(|c| c.has_usage && c.at_ms >= midnight) {
                today_cost += call_cost(rules, c).total();
                today_tokens += c.input + c.cache + c.output;
            }
        }

        // Context
        let last = own.last().copied();
        let ctx_max = if cfg.context_limit_override > 0 { cfg.context_limit_override } else { last.map(|c| c.ctx_max).unwrap_or(0) };
        let ctx_used = last.map(|c| c.ctx_used).unwrap_or(0);
        let ctx_series: Vec<u64> = own.iter().rev().take(SERIES_LEN).rev().map(|c| c.ctx_used).collect();
        let deltas: Vec<i64> = ctx_series.windows(2).map(|w| w[1] as i64 - w[0] as i64).collect();
        let last_delta = deltas.last().copied().unwrap_or(0);
        let recent: Vec<i64> = deltas.iter().rev().take(5).copied().filter(|d| *d > 0).collect();
        let avg_growth = if recent.is_empty() { 0 } else { recent.iter().sum::<i64>() / recent.len() as i64 };
        let ctx_left = ctx_max.saturating_sub(ctx_used);
        let turns_left = (avg_growth > 0 && ctx_max > 0).then(|| ctx_left as i64 / avg_growth);

        // Cache
        let hit = |c: &Call| {
            let d = c.cache + c.input;
            if d == 0 { 0.0 } else { c.cache as f64 / d as f64 }
        };
        let session_hit = if fresh + cache > 0 { cache as f64 / (fresh + cache) as f64 } else { 0.0 };
        let misses = own.iter().skip(1).filter(|c| hit(c) < 0.5).count();
        let warm_left = last.map(|c| (c.at_ms + cfg.cache_ttl_sec as i64 * 1000 - now_ms()) / 1000).filter(|s| *s > 0);

        // Speed
        let (out_sum, stream_sum) = own
            .iter()
            .filter_map(|c| c.stream_ms.filter(|ms| *ms > 50.0).map(|ms| (c.output as f64, ms / 1000.0)))
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
                    let calls: Vec<&Call> =
                        self.convs.get(&sid).map(|c| c.calls.iter().filter(|c| c.has_usage).collect()).unwrap_or_default();
                    json!({
                        "id": sid,
                        "type": s.sub_type,
                        "role": s.sub_role,
                        "model": calls.last().map(|c| c.display.clone()),
                        "tokens": calls.iter().map(|c| c.input + c.cache + c.output).sum::<u64>(),
                        "cost": calls.iter().map(|c| call_cost(rules, c).total()).sum::<f64>(),
                        "running": self.is_running(&sid),
                        "lastAt": calls.last().map(|c| c.at_ms),
                    })
                })
                .collect();
            subs.sort_by_key(|v| (!v["running"].as_bool().unwrap_or(false), std::cmp::Reverse(v["lastAt"].as_i64())));
            subs
        };
        let sub_cost: f64 = subagents.iter().filter_map(|v| v["cost"].as_f64()).sum();

        let turns: Vec<Value> = own
            .iter()
            .map(|c| {
                json!({
                    "step": c.steps.first(),
                    "at": c.at_ms,
                    "ctx": c.ctx_used,
                    "hit": hit(c),
                    "out": c.output,
                    "cost": call_cost(rules, c).total(),
                    "tps": c.tps(),
                    "ttftMs": c.ttft_ms,
                    "label": c.label.clone().unwrap_or_else(|| "…".into()),
                })
            })
            .collect();

        let mut recent_convs: Vec<(i64, String)> = self
            .summaries
            .iter()
            .filter(|(_, s)| s.root.is_none())
            .map(|(cid, s)| (s.modified_ms, cid.clone()))
            .collect();
        recent_convs.sort_by(|a, b| b.cmp(a));
        let mut listed: HashSet<String> = HashSet::new();
        let recent: Vec<Value> = std::iter::once(id.clone())
            .chain(recent_convs.into_iter().map(|(_, cid)| cid))
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
            "recent": recent,
            "bar": self.build_payload(&panes, cfg),
            "ctx": {
                "used": ctx_used, "max": ctx_max, "left": ctx_left, "delta": last_delta,
                "turnsLeft": turns_left, "series": ctx_series,
                "compactions": self.view(&id, cfg).main.compactions,
            },
            "cache": {
                "lastHit": last.map(hit), "sessionHit": session_hit, "misses": misses,
                "savedUsd": saved, "warmLeftSec": warm_left, "ttlSec": cfg.cache_ttl_sec,
            },
            "tokens": {
                "fresh": fresh, "cache": cache, "out": out, "total": fresh + cache + out,
                "costIn": c_in, "costCache": c_cache, "costOut": c_out, "cost": cost,
                "ratePerHr": rate_per_hr, "todayCost": today_cost, "todayTokens": today_tokens,
                "includesSubagents": !is_sub && cfg.include_subagents,
            },
            "speed": {
                "lastTps": last.and_then(Call::tps),
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
        })
    }

    fn title(&self, id: &str) -> String {
        let t = read_annotation(&self.app_data, id).map(|a| a.title).unwrap_or_default();
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
