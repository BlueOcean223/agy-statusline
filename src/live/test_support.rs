//! Helpers shared by the engine's tests.

use super::*;
use crate::config::ModelRule;

pub(super) fn engine() -> Engine {
    let mut e = Engine::new(LsClient::new("127.0.0.1:9", "", false));
    // Never created: nothing in these tests may read the real Antigravity data.
    e.app_data = std::env::temp_dir().join(format!("agy-engine-test-{}", std::process::id()));
    e
}

pub(super) fn uuid(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

pub(super) fn record(input: u64) -> Value {
    json!({ "chatModel": { "responseModel": "m", "usage": { "inputTokens": input } }, "stepIndices": [1] })
}

pub(super) fn ls_page(records: &[Value]) -> Result<ConvData, String> {
    Ok(ConvData::Ls(json!({ "generatorMetadata": records })))
}

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;

/// 2026-11-27T12:00:00Z.
pub(super) const NOW: i64 = 1_795_780_800_000;
/// A fixed clock for view tests, with the day starting 12 hours before [`NOW`] whatever the
/// machine's time zone.
pub(super) const CLOCK: Clock = Clock { now: NOW, midnight: NOW - 12 * HOUR };

/// Pricing rules for view tests, fixed here so changes to the shipped defaults don't move them.
pub(super) fn view_config() -> Config {
    let rule = |pattern: &str, name: &str, ctx: u64, rates: [f64; 4]| ModelRule {
        pattern: pattern.into(),
        display_name: name.into(),
        short_name: name.into(),
        context_limit: ctx,
        input_per_m: rates[0],
        cache_read_per_m: rates[1],
        cache_write_per_m: rates[2],
        output_per_m: rates[3],
        ..Default::default()
    };
    Config {
        costly_call_usd: 0.20,
        models: vec![
            rule("claude-opus", "Claude Opus", 200_000, [5.0, 0.5, 6.25, 25.0]),
            rule("gemini-4-neon", "Gemini 4 Neon", 1_000_000, [2.0, 0.2, 0.0, 12.0]),
        ],
        ..Config::default()
    }
}

/// One model call with usage, `ago` ms before [`NOW`].
pub(super) struct At {
    pub ago: i64,
    pub step: u64,
    /// Backend model name and the name shown to the user.
    pub model: (&'static str, &'static str),
    /// Input, cache read, cache write, output.
    pub tokens: [u64; 4],
    /// Context window used and its size.
    pub ctx: (u64, u64),
    /// Checkpoint index; a new one counts as a compaction. -1 for none.
    pub checkpoint: i64,
    /// Time to first token and streaming time.
    pub timing: Option<(f64, f64)>,
    pub label: Option<&'static str>,
}

const OPUS: (&str, &str) = ("claude-opus-5-5", "Claude Opus 5.5");
const NEON: (&str, &str) = ("gemini-4-neon", "Gemini 4 Neon (High)");

fn live_call(conv: &str, a: &At) -> LiveCall {
    let [input, cache_read, cache_write, output] = a.tokens;
    LiveCall {
        call: Call {
            conv_id: conv.to_string(),
            source: crate::call::DESKTOP,
            model: ModelId::new(a.model.0, a.model.1),
            tokens: Tokens { input, cache_read, cache_write, output },
            at_ms: NOW - a.ago,
            steps: vec![a.step],
            ttft_ms: a.timing.map(|t| t.0),
            stream_ms: a.timing.map(|t| t.1),
            ctx_used: a.ctx.0,
            ctx_max: a.ctx.1,
            checkpoint: a.checkpoint,
            response_id: Some(format!("resp-{conv}-{}", a.step)),
        },
        dated: true,
        live_seen: false,
        label: a.label.map(str::to_string),
    }
}

fn at(ago: i64, step: u64, model: (&'static str, &'static str), tokens: [u64; 4], ctx: (u64, u64)) -> At {
    At { ago, step, model, tokens, ctx, checkpoint: -1, timing: None, label: None }
}

/// Ids in [`view_fixture`]: a root conversation, a running subagent waiting on the user, a subagent
/// that just finished, an older finished subagent, and a top-level conversation known only from its
/// annotation. Three more running subagents push the bar's subagent rows past `MAX_SUBS`.
pub(super) struct ViewIds {
    pub root: String,
    pub running: String,
    pub done: String,
    pub old: String,
    pub other: String,
}

/// An engine holding one conversation tree with priced calls across two days, quota, hooks,
/// subagent steps, git status, annotations and an error.
///
/// Not covered: today's persisted and `agy CLI` usage (the global `USAGE` store is empty in tests),
/// online prices, price-cut days, and miss causes that need transcript markers.
pub(super) fn view_fixture() -> (Engine, ViewIds) {
    let mut e = engine();
    let ids = ViewIds { root: uuid(1), running: uuid(2), done: uuid(3), old: uuid(4), other: uuid(5) };
    let ws = "/work/agy";

    let root_calls = [
        At {
            timing: Some((900.0, 4_000.0)),
            label: Some("Read the sources"),
            ..at(27 * HOUR, 1, OPUS, [20_000, 0, 20_000, 500], (40_000, 200_000))
        },
        // A day later, nothing cached: a miss after a long wait.
        At {
            timing: Some((1_500.0, 6_000.0)),
            label: Some("Edit panel.rs"),
            ..at(80 * MIN, 3, OPUS, [1_000, 0, 39_000, 800], (150_000, 200_000))
        },
        At {
            checkpoint: 0,
            timing: Some((700.0, 5_000.0)),
            label: Some("Run the tests"),
            ..at(40 * MIN, 5, OPUS, [500, 38_000, 2_000, 1_200], (160_000, 200_000))
        },
        // Compacted, then a switch to another model: a miss caused by the model change.
        At {
            checkpoint: 1,
            timing: Some((1_200.0, 3_000.0)),
            label: Some("Answer"),
            ..at(3 * MIN, 7, NEON, [30_000, 0, 0, 400], (30_000, 1_000_000))
        },
    ];
    let mut calls: Vec<LiveCall> = root_calls.iter().map(|a| live_call(&ids.root, a)).collect();
    // An aborted record with no tokens, which every total skips.
    calls.insert(
        2,
        LiveCall {
            call: Call { conv_id: ids.root.clone(), at_ms: NOW - 60 * MIN, steps: vec![4], ..Default::default() },
            ..Default::default()
        },
    );
    e.convs.insert(ids.root.clone(), Conv { calls, ..Default::default() });
    e.summaries.insert(
        ids.root.clone(),
        Summary {
            status: "CASCADE_RUN_STATUS_IDLE".into(),
            step_count: 8,
            created_ms: NOW - 28 * HOUR,
            modified_ms: NOW - 4 * MIN,
            workspace: Some("agy".into()),
            branch: Some("main".into()),
            workspace_path: Some(ws.into()),
            ..Default::default()
        },
    );
    // A hook fired a few seconds ago: running before the next status poll says so.
    e.hooks.insert(ids.root.clone(), HookState { event: "PostInvocation".into(), at_ms: NOW - 3_000 });

    let sub = |status: &str, sub_type: &str, role: &str, modified: i64| Summary {
        status: status.into(),
        step_count: 2,
        created_ms: NOW - modified - 5 * MIN,
        modified_ms: NOW - modified,
        parent: Some(ids.root.clone()),
        root: Some(ids.root.clone()),
        sub_type: Some(sub_type.into()),
        sub_role: Some(role.into()),
        workspace_path: Some(ws.into()),
        ..Default::default()
    };
    e.summaries.insert(ids.running.clone(), sub("CASCADE_RUN_STATUS_RUNNING", "explorer", "Find callers", MIN));
    e.summaries.insert(ids.done.clone(), sub("CASCADE_RUN_STATUS_IDLE", "reviewer", "Check tests", 2 * MIN));
    e.summaries.insert(ids.old.clone(), sub("CASCADE_RUN_STATUS_IDLE", "explorer", "Map modules", 100 * MIN));
    e.ls_loaded.extend([ids.root.clone(), ids.running.clone(), ids.done.clone()]);
    e.sub_done.insert(ids.done.clone(), NOW - 4_000);

    let sub_calls = [
        (
            &ids.running,
            vec![
                At {
                    timing: Some((600.0, 2_000.0)),
                    label: Some("Search"),
                    ..at(20 * MIN, 1, OPUS, [8_000, 0, 8_000, 300], (16_000, 200_000))
                },
                // Over the costly-call threshold.
                At {
                    timing: Some((500.0, 40_000.0)),
                    label: Some("Read callers"),
                    ..at(10 * MIN, 3, OPUS, [400, 16_000, 1_000, 8_000], (17_400, 200_000))
                },
            ],
        ),
        (
            &ids.done,
            vec![At {
                timing: Some((800.0, 4_000.0)),
                ..at(50 * MIN, 1, NEON, [12_000, 0, 0, 900], (12_000, 1_000_000))
            }],
        ),
        (&ids.old, vec![at(110 * MIN, 1, OPUS, [5_000, 0, 5_000, 200], (10_000, 200_000))]),
    ];
    for (id, list) in sub_calls {
        let calls = list.iter().map(|a| live_call(id, a)).collect();
        e.convs.insert(id.clone(), Conv { calls, ..Default::default() });
    }
    for n in 0..3 {
        let id = uuid(6 + n);
        let role = format!("Worker {n}");
        e.summaries.insert(id.clone(), sub("CASCADE_RUN_STATUS_RUNNING", "worker", &role, 5 * MIN));
        let call = at((15 + 10 * n as i64) * MIN, 1, NEON, [2_000 + n as u64, 0, 0, 100], (2_000, 1_000_000));
        e.convs.insert(id.clone(), Conv { calls: vec![live_call(&id, &call)], ..Default::default() });
    }
    let step =
        |status: &str, text: &str, t0: i64, t1: i64| SubStep { status: status.into(), text: text.into(), t0, t1 };
    e.sub_steps.insert(
        ids.running.clone(),
        SubStepCache {
            total: 3,
            first_at: NOW - 21 * MIN,
            steps: BTreeMap::from([
                (0, step("DONE", "Search for callers", NOW - 21 * MIN, NOW - 15 * MIN)),
                (1, step("ERROR", "Read live.rs", NOW - 15 * MIN, NOW - 9 * MIN)),
                (2, step("WAITING", "Approve a command", NOW - 9 * MIN, 0)),
            ]),
            ..Default::default()
        },
    );

    e.quota = json!({ "groups": [
        { "displayName": "Gemini Models", "description": "Gemini 4 Neon, Gemini 4 Flash", "buckets": [
            { "window": "weekly", "remainingFraction": 0.3, "resetTime": "2026-11-30T12:00:00Z" },
            { "window": "5h", "remainingFraction": 0.55, "resetTime": "2026-11-27T14:00:00Z" },
        ]},
        { "displayName": "Claude and GPT Models", "description": "Claude Opus 5.5, GPT-6", "buckets": [
            { "window": "5h", "remainingFraction": 0.9, "resetTime": "2026-11-27T15:30:00Z" },
        ]},
    ]});
    e.account = Some(AccountInfo {
        key: "1a2b3c4d".into(),
        masked: "ab***12@example.com".into(),
        tier: "Google AI Ultra".into(),
    });
    e.git.insert(
        ws.into(),
        (
            Instant::now(),
            Some(GitInfo { branch: "main".into(), dirty: true, ahead: 2, behind: 0, added: 40, deleted: 12 }),
        ),
    );
    let note = |title: &str, viewed: i64| (0, Annotation { title: title.into(), viewed_ms: viewed });
    e.annotations.insert(ids.root.clone(), note("Refactor the panel", NOW - 2 * MIN));
    e.annotations.insert(ids.other.clone(), note("Release notes", NOW - 2 * HOUR));
    e.disk.insert(ids.other.clone(), NOW - 2 * HOUR);
    e.last_error = Some("language_server: connection refused".into());
    (e, ids)
}
