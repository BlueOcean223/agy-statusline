//! One engine step, split so the ENGINE lock is never held across language_server RPCs, SQLite or
//! transcript reads, directory scans or git subprocesses (the side panel's HTTP handlers wait on
//! that lock):
//!
//! 1. `plan_status` (locked) → `StatusPlan::fetch` (unlocked) → `apply_status` (locked):
//!    trajectory summaries, quota, account, `brain/` scan, annotation titles.
//! 2. `plan_convs` (locked) → `ConvPlan::fetch` (unlocked) → `apply_convs` (locked):
//!    generator records and the transcripts that date them, visible subagents' steps, git
//!    status, then eviction.
//!
//! The second plan depends on the first apply (new summaries decide what is stale).

use super::*;

/// Hooks can arrive many times a second in agent teams; never refresh summaries faster than this.
const SUMMARY_MIN_GAP: Duration = Duration::from_millis(1500);
/// After a `Stop` hook the quota is refreshed early, but at most this often.
const QUOTA_AFTER_STOP_GAP: Duration = Duration::from_secs(30);
/// Conversations not on screen (needed only for today's cost) are re-read at most this often.
const OFFSCREEN_GAP: Duration = Duration::from_secs(5);
/// Database reads per tick, so a restart or opening a large agent tree doesn't stall one tick.
const DB_LOADS_PER_TICK: usize = 24;
/// Rounds of loading per side-panel request (and per CLI tick); each round can reveal subagents one
/// level deeper (named in transcripts read by the previous round). The engine loop finds the rest.
pub(super) const LOAD_ROUNDS: usize = 4;
const FAIL_BACKOFF: Duration = Duration::from_secs(60);
/// Transcript lines for new records are usually flushed within seconds; stop retrying after this.
const RESYNC_WINDOW: Duration = Duration::from_secs(600);
const ANNOTATIONS_EVERY: Duration = Duration::from_secs(10);
const EVICT_EVERY: Duration = Duration::from_secs(60);
/// Conversations not needed for today's cost or on screen are dropped after this long unused.
const EVICT_IDLE: Duration = Duration::from_secs(600);
const EVICT_IDLE_MS: i64 = EVICT_IDLE.as_millis() as i64;

/// The conversation reads planned for one round, with at most [`DB_LOADS_PER_TICK`] database reads.
struct ReadQueue {
    db_budget: usize,
    queued: HashSet<String>,
    reqs: Vec<ConvReq>,
}

impl ReadQueue {
    fn new() -> Self {
        Self { db_budget: DB_LOADS_PER_TICK, queued: HashSet::new(), reqs: Vec::new() }
    }

    /// Queues `req` unless it is a database read and the budget is spent.
    fn push(&mut self, req: ConvReq) {
        if matches!(req, ConvReq::Db { .. }) {
            if self.db_budget == 0 {
                return;
            }
            self.db_budget -= 1;
        }
        self.queued.insert(req.id().to_string());
        self.reqs.push(req);
    }
}

pub(super) struct StatusPlan {
    ls: LsClient,
    app_data: PathBuf,
    summaries: bool,
    quota: bool,
    user_status: bool,
    disk: bool,
    annotations: Option<HashMap<String, (i64, Annotation)>>,
}

pub(super) struct StatusFetched {
    summaries: Option<Result<Value, String>>,
    quota: Option<Result<Value, String>>,
    user_status: Option<Option<Value>>,
    disk: Option<HashMap<String, i64>>,
    annotations: Option<HashMap<String, (i64, Annotation)>>,
}

impl StatusPlan {
    pub(super) fn fetch(self) -> StatusFetched {
        let ls = &self.ls;
        StatusFetched {
            summaries: self.summaries.then(|| ls.call("GetAllCascadeTrajectories", &json!({}))),
            quota: self.quota.then(|| ls.call("RetrieveUserQuotaSummary", &json!({}))),
            user_status: self.user_status.then(|| ls.call("GetUserStatus", &json!({})).ok()),
            disk: self.disk.then(|| crate::transcript::scan_brain(&self.app_data)),
            annotations: self.annotations.map(|prev| crate::transcript::scan_annotations(&self.app_data, prev)),
        }
    }
}

struct SubReq {
    id: String,
    from: usize,
    running: bool,
}

pub(super) struct ConvPlan {
    ls: LsClient,
    app_data: PathBuf,
    convs: Vec<ConvReq>,
    subs: Vec<SubReq>,
    /// Transcripts to read, each from where its conversation has read up to.
    transcripts: Vec<(String, ReadPos)>,
    git: Vec<String>,
    /// Kept for `apply_convs`: what the bar/panel shows and what today's cost needs. `None` for a
    /// side-panel load, which only adds conversations: it neither evicts nor reports errors.
    keep: Option<HashSet<String>>,
}

pub(super) struct ConvFetched {
    convs: Vec<(ConvReq, Result<ConvData, String>)>,
    subs: Vec<(SubReq, Option<Value>)>,
    transcripts: Vec<(String, TranscriptChunk)>,
    git: Vec<(String, Option<GitInfo>)>,
    keep: Option<HashSet<String>>,
}

impl ConvPlan {
    pub(super) fn fetch(self) -> ConvFetched {
        let convs: Vec<(ConvReq, Result<ConvData, String>)> = self
            .convs
            .into_iter()
            .map(|req| {
                let res = fetch_conv(&self.ls, &self.app_data, &req);
                (req, res)
            })
            .collect();
        // A conversation read for the first time whose records failed is not merged, nor is its
        // transcript: don't read the whole file for nothing.
        let unread: HashSet<&str> = convs.iter().filter(|(_, res)| res.is_err()).map(|(req, _)| req.id()).collect();
        let subs = self
            .subs
            .into_iter()
            .map(|r| {
                let j = self.ls.call("GetCascadeTrajectorySteps", &json!({ "cascadeId": r.id, "stepOffset": r.from }));
                (r, j.ok())
            })
            .collect();
        // After the records, so the transcript has the steps those records refer to.
        let transcripts = self
            .transcripts
            .into_iter()
            .filter(|(id, at)| !(at.offset() == 0 && unread.contains(id.as_str())))
            .map(|(id, at)| {
                let chunk = crate::transcript::read_since(&self.app_data, &id, at);
                (id, chunk)
            })
            .collect();
        let git = self
            .git
            .into_iter()
            .map(|p| {
                let info = crate::git::read(&p);
                (p, info)
            })
            .collect();
        ConvFetched { convs, subs, transcripts, git, keep: self.keep }
    }
}

impl Engine {
    /// One full step without releasing `self` (the CLI; the sidecar uses the phases directly).
    /// Repeats the conversation phase while it finds more to load (subagents discovered in
    /// transcripts read by the previous round), which the sidecar spreads over its ticks.
    pub fn tick(&mut self, interest: &HashSet<String>) {
        let f = self.plan_status().fetch();
        self.apply_status(f);
        for _ in 0..LOAD_ROUNDS {
            let plan = self.plan_convs(interest);
            let more = plan.convs.iter().any(|r| !self.convs.contains_key(r.id()));
            self.apply_convs(plan.fetch());
            if !more {
                break;
            }
        }
    }

    /// Loads what the side panel needs for `id` without releasing `self` (the CLI; the sidecar
    /// releases the ENGINE lock between phases, see `live::panel_json`).
    pub fn load_for_panel(&mut self, id: &str) {
        let mut tried = HashSet::new();
        for _ in 0..LOAD_ROUNDS {
            let Some(plan) = self.plan_load(id, &mut tried) else { break };
            for stale in self.apply_convs(plan.fetch()) {
                tried.remove(&stale);
            }
        }
    }

    pub(super) fn plan_status(&mut self) -> StatusPlan {
        self.tick_errors.clear();
        let busy = self.any_active() || !self.dirty.is_empty();
        let every = Duration::from_secs(if busy { 2 } else { 10 });
        let summaries = self
            .summaries_at
            .is_none_or(|t| t.elapsed() >= every || (self.summaries_poke && t.elapsed() >= SUMMARY_MIN_GAP));
        let quota = self.quota_at.is_none_or(|t| {
            t.elapsed() >= Duration::from_secs(180) || (self.quota_poke && t.elapsed() >= QUOTA_AFTER_STOP_GAP)
        });
        let user_status = self.user_status_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(300));
        let disk = self.disk_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
        let annotations =
            self.annotations_at.is_none_or(|t| t.elapsed() >= ANNOTATIONS_EVERY).then(|| self.annotations.clone());
        StatusPlan {
            ls: self.ls.clone(),
            app_data: self.app_data.clone(),
            summaries,
            quota,
            user_status,
            disk,
            annotations,
        }
    }

    pub(super) fn apply_status(&mut self, f: StatusFetched) {
        let now = Instant::now();
        match f.summaries {
            Some(Ok(j)) => {
                self.apply_summaries(&j);
                self.summaries_at = Some(now);
                self.summaries_poke = false;
            }
            Some(Err(e)) => self.tick_errors.push(e),
            None => {}
        }
        match f.quota {
            Some(Ok(j)) => self.apply_quota(j),
            Some(Err(e)) => self.tick_errors.push(e),
            None => {}
        }
        if let Some(j) = f.user_status {
            self.apply_user_status(j.as_ref());
        }
        if let Some(d) = f.disk {
            self.disk = d;
            self.disk_at = Some(now);
        }
        if let Some(a) = f.annotations {
            self.annotations = a;
            self.annotations_at = Some(now);
        }
    }

    /// Running or just-finished subagents of `root`, in the bar's order (see `view`).
    pub(super) fn shown_subs(&self, root: &str, now: i64) -> Vec<String> {
        let mut ids: Vec<&String> = self
            .summaries
            .iter()
            .filter(|(sid, s)| *sid != root && s.root.as_deref() == Some(root))
            .filter(|(sid, _)| {
                self.is_running(sid, now) || self.sub_done.get(*sid).is_some_and(|t| now - t < SUB_DONE_SHOW_MS)
            })
            .map(|(sid, _)| sid)
            .collect();
        ids.sort_by_key(|sid| (!self.is_running(sid, now), std::cmp::Reverse(self.last_call(sid).map(|c| c.at_ms))));
        ids.into_iter().cloned().collect()
    }

    /// Decides what to (re)load. `interest` holds the conversations on screen (chat panes and the
    /// side panel); everything touched today is also kept current for today's cost.
    pub(super) fn plan_convs(&mut self, interest: &HashSet<String>) -> ConvPlan {
        let clock = Clock::read();
        let (now, midnight) = (clock.now, clock.today.start_ms());
        let mut want: HashSet<String> = HashSet::new();
        for (id, s) in &self.summaries {
            if s.modified_ms >= midnight {
                want.insert(id.clone());
            }
        }
        for (id, mtime) in &self.disk {
            if *mtime >= midnight {
                want.insert(id.clone());
            }
        }
        let mut roots: HashSet<String> = HashSet::new();
        for p in interest {
            want.insert(p.clone());
            roots.insert(self.root_of(p));
        }
        // Only the subagents the bar can show (MAX_SUBS per root) are polled live; the rest of
        // a tree is kept current for its cost at the off-screen pace.
        let mut subs_shown: HashSet<String> = HashSet::new();
        for root in &roots {
            want.insert(root.clone());
            for (id, s) in &self.summaries {
                if s.root.as_deref() == Some(root.as_str()) {
                    want.insert(id.clone());
                }
            }
            subs_shown.extend(self.shown_subs(root, now).into_iter().take(MAX_SUBS));
        }
        let mut visible: HashSet<String> = interest.clone();
        visible.extend(subs_shown.iter().cloned());
        // Hook ids that are not wanted (e.g. other windows' conversations) would otherwise stay
        // dirty forever and keep summaries on the fast 2s cadence.
        self.dirty.retain(|id| want.contains(id));
        self.failed.retain(|_, t| t.elapsed() < FAIL_BACKOFF);

        let mut todo: Vec<(bool, &String)> = want
            .iter()
            .filter(|id| !self.failed.contains_key(*id))
            .filter_map(|id| {
                let conv = self.convs.get(id);
                let loaded = conv.is_some_and(|c| c.loaded_once);
                let steps_changed = match (conv, self.summaries.get(id)) {
                    (Some(c), Some(s)) => c.loaded_steps != s.step_count,
                    _ => false,
                };
                let on_screen = visible.contains(id);
                let running_on_screen = on_screen && self.summaries.get(id).is_some_and(Summary::active);
                if !(!loaded || steps_changed || self.dirty.contains(id) || running_on_screen) {
                    return None;
                }
                if loaded && !on_screen && conv.and_then(|c| c.fetched_at).is_some_and(|t| t.elapsed() < OFFSCREEN_GAP)
                {
                    return None;
                }
                Some((!on_screen, id))
            })
            .collect();
        todo.sort();

        let mut q = ReadQueue::new();
        for (_, id) in todo {
            q.push(self.conv_req(id));
        }

        // Historical subagents of conversations on screen, found in their transcripts, so a
        // restart doesn't omit subagent cost. Read from their databases; their summaries then
        // name the root, so deeper levels are found on later ticks.
        let mut seeds: Vec<String> = interest.iter().cloned().collect();
        for root in &roots {
            seeds.extend(self.tree(root));
        }
        self.plan_hints(&seeds, &mut q);
        let convs = q.reqs;

        // Steps for the subagents the bar shows. The first page starts near the end: only the
        // last SUB_STEPS_KEEP steps are kept, and early pages can carry large tool outputs.
        let mut subs = Vec::new();
        let mut shown: Vec<&String> = subs_shown.iter().collect();
        shown.sort();
        for sid in shown {
            if !self.ls_loaded.contains(sid) {
                continue;
            }
            let running = self.is_running(sid, now);
            let step_count = self.summaries.get(sid).map_or(0, |s| s.step_count as usize);
            let cache = self.sub_steps.entry(sid.clone()).or_default();
            let due = if running {
                cache.fetched_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(3))
            } else {
                !cache.final_fetched && cache.fetched_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(1200))
            };
            if !due {
                continue;
            }
            let from =
                if cache.fetched_at.is_none() { step_count.saturating_sub(SUB_STEPS_KEEP) } else { cache.next_offset };
            cache.fetched_at = Some(Instant::now());
            subs.push(SubReq { id: sid.clone(), from, running });
        }

        // Git status for the workspaces on screen, at most every 5s per workspace.
        let mut git: Vec<String> = interest
            .iter()
            .filter_map(|id| self.summaries.get(id)?.workspace_path.clone())
            .filter(|p| self.git.get(p).is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(5)))
            .collect();
        git.sort();
        git.dedup();

        let mut keep = want;
        keep.extend(visible);
        let transcripts = self.plan_transcripts(&convs, true);
        ConvPlan {
            ls: self.ls.clone(),
            app_data: self.app_data.clone(),
            convs,
            subs,
            transcripts,
            git,
            keep: Some(keep),
        }
    }

    /// Plans reading what the side panel needs for `id` and does not have yet: the conversation,
    /// its root's subagent tree, and subagents named in their transcripts. `tried` holds what
    /// earlier rounds of the same request asked for, so a failing read is tried once per request;
    /// remove from it what [`Engine::apply_convs`] dropped as stale, so it is planned again (e.g. from
    /// the language_server, which loaded it during a database read).
    /// `None` when nothing is missing; keeping loaded conversations current is left to the engine
    /// loop.
    pub(super) fn plan_load(&mut self, id: &str, tried: &mut HashSet<String>) -> Option<ConvPlan> {
        let root = self.root_of(id);
        let mut seeds = vec![id.to_string()];
        seeds.extend(self.tree(&root).into_iter().filter(|cid| cid != id));
        let mut q = ReadQueue::new();
        for cid in &seeds {
            if self.convs.contains_key(cid) || tried.contains(cid) {
                continue;
            }
            let req = self.conv_req(cid);
            // Failed reads are database reads (see `apply_summaries`).
            if matches!(req, ConvReq::Db { .. }) && self.failed.get(cid).is_some_and(|t| t.elapsed() < FAIL_BACKOFF) {
                continue;
            }
            q.push(req);
        }
        q.queued.extend(tried.iter().cloned());
        self.plan_hints(&seeds, &mut q);
        let convs = q.reqs;
        tried.extend(convs.iter().map(|r| r.id().to_string()));
        if convs.is_empty() {
            return None;
        }
        let transcripts = self.plan_transcripts(&convs, false);
        Some(ConvPlan {
            ls: self.ls.clone(),
            app_data: self.app_data.clone(),
            convs,
            subs: Vec::new(),
            transcripts,
            git: Vec::new(),
            keep: None,
        })
    }

    /// The transcripts to read alongside `convs`: theirs, and with `resync` those of recently
    /// changed conversations that still have undated or unlabeled calls. Late-flushed
    /// transcript.jsonl entries are picked up that way without an LS RPC, only for a while: some
    /// records never get a label, and re-reading every transcript every second is costly with
    /// many subagents.
    fn plan_transcripts(&self, convs: &[ConvReq], resync: bool) -> Vec<(String, ReadPos)> {
        let mut ids: Vec<&str> = convs.iter().map(ConvReq::id).collect();
        if resync {
            let mut late: Vec<&str> = self
                .convs
                .iter()
                .filter(|(_, c)| c.changed_at.is_some_and(|t| t.elapsed() < RESYNC_WINDOW))
                .filter(|(_, c)| c.calls.iter().any(|call| call.has_usage() && (!call.dated || call.label.is_none())))
                .map(|(id, _)| id.as_str())
                .filter(|id| !ids.contains(id))
                .collect();
            late.sort();
            ids.extend(late);
        }
        ids.into_iter()
            .map(|id| {
                let at = self.convs.get(id).and_then(|c| c.transcript.as_ref()).map(Transcript::position);
                (id.to_string(), at.unwrap_or_default())
            })
            .collect()
    }

    /// Queues historical subagents named in the transcripts of `seeds`, each probed once (see
    /// `probed_hints`), within the remaining database budget.
    fn plan_hints(&mut self, seeds: &[String], q: &mut ReadQueue) {
        let mut hints: Vec<String> = Vec::new();
        for cid in seeds {
            if let Some(t) = self.convs.get(cid).and_then(|c| c.transcript.as_ref()) {
                hints.extend(t.subagent_hints.iter().filter(|sid| *sid != cid).cloned());
            }
        }
        hints.sort();
        hints.dedup();
        for sid in hints {
            if q.db_budget == 0 {
                break;
            }
            // Not on disk (as of the last `brain/` scan): nothing to read yet, ask again later.
            if self.convs.contains_key(&sid)
                || q.queued.contains(&sid)
                || self.probed_hints.contains(&sid)
                || !self.disk.contains_key(&sid)
            {
                continue;
            }
            self.probed_hints.insert(sid.clone());
            q.push(self.conv_req(&sid));
        }
    }

    /// Merges a fetched plan. Returns the conversations whose read was dropped as stale (something
    /// changed while it was fetched), so a side-panel request can plan them again.
    pub(super) fn apply_convs(&mut self, f: ConvFetched) -> Vec<String> {
        let now = Instant::now();
        let mut changed: HashSet<String> = HashSet::new();
        let mut stale = Vec::new();
        for (req, res) in f.convs {
            let id = req.id().to_string();
            match res {
                Ok(data) => match self.apply_conv(&req, data) {
                    Applied::Changed => {
                        self.dirty.remove(&id);
                        changed.insert(id);
                    }
                    Applied::Unchanged => {
                        self.dirty.remove(&id);
                    }
                    Applied::Stale => stale.push(id),
                },
                Err(e) => {
                    if matches!(req, ConvReq::Db { .. }) {
                        self.failed.insert(id.clone(), now);
                    }
                    self.dirty.remove(&id);
                    if f.keep.is_some() {
                        self.tick_errors.push(e);
                    }
                }
            }
        }
        for (r, j) in f.subs {
            if let Some(j) = j {
                self.apply_sub_steps(&r.id, r.from, r.running, &j);
            }
        }
        for (id, chunk) in f.transcripts {
            let force = changed.remove(&id);
            self.apply_transcript(&id, Some(chunk), force);
        }
        for id in changed {
            self.apply_transcript(&id, None, true);
        }
        for (path, info) in f.git {
            self.git.insert(path, (now, info));
        }
        let Some(keep) = f.keep else { return stale };
        self.last_error = self.tick_errors.first().cloned();

        if self.evict_at.is_none_or(|t| t.elapsed() >= EVICT_EVERY) {
            self.evict(&keep, now);
            self.evict_at = Some(now);
        }
        stale
    }

    /// Drops state for conversations that are neither on screen nor touched today and have not
    /// been used for a while; they are re-read (cheaply, from the language_server or their
    /// database) if they come back. Today's cost is unaffected: those conversations are kept, and
    /// earlier days live in the persisted usage history.
    fn evict(&mut self, keep: &HashSet<String>, at: Instant) {
        let idle = |t: &Instant| at.saturating_duration_since(*t) >= EVICT_IDLE;
        let now = now_ms();
        let today = LocalDay::today().key();
        if self.evict_day != today {
            // Lets yesterday's probed subagent ids be tried again; bounds the set.
            self.probed_hints.clear();
            self.evict_day = today;
        }
        self.convs.retain(|id, c| keep.contains(id) || c.fetched_at.is_some_and(|t| !idle(&t)));
        let convs = &self.convs;
        let ls_loaded = &self.ls_loaded;
        self.summaries.retain(|id, _| ls_loaded.contains(id) || convs.contains_key(id));
        self.dirty.retain(|id| keep.contains(id));
        self.sub_done.retain(|_, t| now - *t < EVICT_IDLE_MS);
        self.hooks.retain(|_, h| now - h.at_ms < EVICT_IDLE_MS);
        let sub_done = &self.sub_done;
        self.sub_steps
            .retain(|id, c| keep.contains(id) || sub_done.contains_key(id) || c.fetched_at.is_some_and(|t| !idle(&t)));
        self.git.retain(|_, (t, _)| !idle(t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::test_support::*;

    /// Merges fetched conversations, keeping every conversation so eviction stays out of the way.
    fn apply(e: &mut Engine, convs: Vec<(ConvReq, Result<ConvData, String>)>) {
        let keep = e.convs.keys().cloned().chain(convs.iter().map(|(r, _)| r.id().to_string())).collect();
        e.apply_convs(ConvFetched {
            convs,
            subs: Vec::new(),
            transcripts: Vec::new(),
            git: Vec::new(),
            keep: Some(keep),
        });
    }

    #[test]
    fn language_server_records_fetched_against_an_older_length_are_dropped() {
        let mut e = engine();
        let a = uuid(1);
        e.ls_loaded.insert(a.clone());
        let first = e.conv_req(&a);
        apply(&mut e, vec![(first, ls_page(&[record(10), record(20)]))]);
        assert_eq!(e.convs[&a].calls.len(), 2);

        // Two fetches planned from the same state (e.g. the panel and the engine loop).
        let (r1, r2) = (e.conv_req(&a), e.conv_req(&a));
        assert!(matches!(r1, ConvReq::Ls { offset: 1, len: 2, .. }));
        apply(&mut e, vec![(r1, ls_page(&[record(20), record(30)]))]);
        let rev = e.convs[&a].rev;
        apply(&mut e, vec![(r2, ls_page(&[record(20), record(99)]))]);
        let c = &e.convs[&a];
        assert_eq!(c.calls.iter().map(|c| c.tokens.input).collect::<Vec<_>>(), vec![10, 20, 30]);
        assert_eq!(c.rev, rev, "the stale page must not touch the conversation");
    }

    #[test]
    fn database_result_is_dropped_once_the_language_server_has_the_conversation() {
        let mut e = engine();
        let a = uuid(1);
        let req = e.conv_req(&a);
        assert!(matches!(req, ConvReq::Db { have: None, .. }));
        // The language_server loads the conversation while its database is being read.
        e.ls_loaded.insert(a.clone());
        let db = ConvData::Db(Some((
            DbSig { mtime_ms: 1, len: 1 },
            vec![LiveCall::default()],
            Box::new(Summary { step_count: 3, ..Default::default() }),
        )));
        apply(&mut e, vec![(req, Ok(db))]);
        assert!(!e.convs.contains_key(&a));

        let plan = e.plan_convs(&HashSet::from([a.clone()]));
        assert!(matches!(plan.convs.as_slice(), [ConvReq::Ls { .. }]));
    }

    #[test]
    fn database_reads_per_tick_are_capped_and_on_screen_conversations_go_first() {
        let mut e = engine();
        // Tomorrow is "today or later" even if local midnight passes while the test runs.
        let now = LocalDay::today().next().start_ms();
        for n in 0..DB_LOADS_PER_TICK + 6 {
            e.disk.insert(uuid(n), now);
        }
        for n in 100..103 {
            e.ls_loaded.insert(uuid(n));
            e.summaries.insert(uuid(n), Summary { modified_ms: now, ..Default::default() });
        }
        let on_screen = uuid(DB_LOADS_PER_TICK + 5); // sorts after every other database read
        let plan = e.plan_convs(&HashSet::from([on_screen.clone()]));
        let db: Vec<&str> = plan.convs.iter().filter(|r| matches!(r, ConvReq::Db { .. })).map(ConvReq::id).collect();
        assert_eq!(db.len(), DB_LOADS_PER_TICK);
        assert_eq!(db[0], on_screen);
        assert_eq!(plan.convs.iter().filter(|r| matches!(r, ConvReq::Ls { .. })).count(), 3, "no cap on RPCs");
    }

    #[test]
    fn transcript_subagent_hints_are_probed_once() {
        let mut e = engine();
        let (root, on_disk, missing) = (uuid(1), uuid(2), uuid(3));
        let mut t = Transcript::default();
        t.subagent_hints.extend([on_disk.clone(), missing.clone()]);
        e.convs.insert(root.clone(), Conv { loaded_once: true, transcript: Some(t), ..Default::default() });
        e.disk.insert(on_disk.clone(), 1); // an old subagent: not wanted for today's cost

        let interest = HashSet::from([root.clone()]);
        let ids = |p: &ConvPlan| p.convs.iter().map(|r| r.id().to_string()).collect::<Vec<_>>();
        let first = e.plan_convs(&interest);
        assert!(ids(&first).contains(&on_disk));
        assert!(!ids(&first).contains(&missing), "not on disk: nothing to read");
        let second = e.plan_convs(&interest);
        assert!(!ids(&second).contains(&on_disk));
        // It appears in the next `brain/` scan.
        e.disk.insert(missing.clone(), 1);
        assert!(ids(&e.plan_convs(&interest)).contains(&missing));
    }

    #[test]
    fn eviction_keeps_wanted_conversations_and_drops_idle_ones() {
        let mut e = engine();
        // Evict "later" rather than fetch "earlier": on Windows an `Instant` cannot predate boot.
        let fetched = Instant::now();
        let later = fetched + EVICT_IDLE + Duration::from_secs(60);
        let (kept, idle, recent) = (uuid(1), uuid(2), uuid(3));
        for (id, at) in [(&kept, fetched), (&idle, fetched), (&recent, later)] {
            e.convs.insert(id.clone(), Conv { fetched_at: Some(at), ..Default::default() });
            e.summaries.insert(id.clone(), Summary::default());
        }
        e.evict(&HashSet::from([kept.clone()]), later);
        assert!(e.convs.contains_key(&kept) && e.convs.contains_key(&recent));
        assert!(!e.convs.contains_key(&idle));
        assert!(!e.summaries.contains_key(&idle), "summaries go with their conversation");
    }

    fn db_conv(root: Option<&str>) -> Result<ConvData, String> {
        let summary = Summary { root: root.map(str::to_string), step_count: 1, ..Default::default() };
        Ok(ConvData::Db(Some((DbSig { mtime_ms: 1, len: 1 }, Vec::new(), Box::new(summary)))))
    }

    fn panel_round(
        e: &mut Engine,
        id: &str,
        tried: &mut HashSet<String>,
        data: impl Fn(&str) -> Result<ConvData, String>,
    ) -> Vec<String> {
        let Some(plan) = e.plan_load(id, tried) else { return Vec::new() };
        let ids: Vec<String> = plan.convs.iter().map(|r| r.id().to_string()).collect();
        let convs = plan.convs.into_iter().map(|r| (r.clone(), data(r.id()))).collect();
        let stale = e.apply_convs(ConvFetched {
            convs,
            subs: Vec::new(),
            transcripts: Vec::new(),
            git: Vec::new(),
            keep: plan.keep,
        });
        for id in stale {
            tried.remove(&id);
        }
        ids
    }

    #[test]
    fn panel_load_follows_a_subagent_to_its_root_then_stops() {
        let mut e = engine();
        let (sub, root, sibling) = (uuid(1), uuid(2), uuid(3));
        // The sibling is known from the language_server's summaries but its records are not loaded.
        e.summaries.insert(sibling.clone(), Summary { root: Some(root.clone()), ..Default::default() });
        let data = |id: &str| db_conv((id != root).then_some(root.as_str()));
        let mut tried = HashSet::new();

        // Round 1: only the subagent is known; its database names the root.
        assert_eq!(panel_round(&mut e, &sub, &mut tried, data), vec![sub.clone()]);
        // Round 2: the root's tree, minus what is loaded.
        let mut second = panel_round(&mut e, &sub, &mut tried, data);
        second.sort();
        assert_eq!(second, vec![root.clone(), sibling.clone()]);
        assert!(e.plan_load(&sub, &mut HashSet::new()).is_none(), "everything loaded: nothing to plan");
    }

    #[test]
    fn panel_load_neither_evicts_nor_reports_errors_and_backs_off_failures() {
        let mut e = engine();
        let (idle, missing) = (uuid(1), uuid(2));
        // Never fetched and not kept: an engine-loop merge would evict it right away.
        e.convs.insert(idle.clone(), Conv::default());
        assert!(e.evict_at.is_none());

        let planned = panel_round(&mut e, &missing, &mut HashSet::new(), |_| Err("no database".to_string()));
        assert_eq!(planned, vec![missing.clone()]);
        assert!(e.convs.contains_key(&idle), "a panel load only adds conversations");
        // The engine loop's next merge reports its own tick's errors; the panel's must not be among them.
        apply(&mut e, Vec::new());
        assert!(e.last_error.is_none(), "panel load errors do not reach the bar");
        assert!(e.plan_load(&missing, &mut HashSet::new()).is_none(), "a failed read waits for FAIL_BACKOFF");

        // Once the language_server has it, it is read from there without waiting.
        e.apply_summaries(&json!({ "trajectorySummaries": { missing.clone(): {} } }));
        let plan = e.plan_load(&missing, &mut HashSet::new()).expect("planned");
        assert!(matches!(plan.convs.as_slice(), [ConvReq::Ls { .. }]));
    }

    #[test]
    fn panel_load_tries_a_failing_read_once_per_request() {
        let mut e = engine();
        let a = uuid(1);
        e.ls_loaded.insert(a.clone());
        let mut tried = HashSet::new();
        let fail = |_: &str| Err("timed out".to_string());
        assert_eq!(panel_round(&mut e, &a, &mut tried, fail), vec![a.clone()]);
        assert!(e.plan_load(&a, &mut tried).is_none(), "the same request does not ask again");
        assert!(e.plan_load(&a, &mut HashSet::new()).is_some(), "the next request does");
    }

    #[test]
    fn panel_load_rereads_from_the_language_server_a_database_read_it_loaded_meanwhile() {
        let mut e = engine();
        let a = uuid(1);
        let mut tried = HashSet::new();
        let plan = e.plan_load(&a, &mut tried).expect("planned");
        assert!(matches!(plan.convs.as_slice(), [ConvReq::Db { .. }]));
        // While the database is read, the engine loop's summaries say the language_server has it.
        e.ls_loaded.insert(a.clone());
        let convs = plan.convs.into_iter().map(|r| (r, db_conv(None))).collect();
        let stale = e.apply_convs(ConvFetched {
            convs,
            subs: Vec::new(),
            transcripts: Vec::new(),
            git: Vec::new(),
            keep: plan.keep,
        });
        assert_eq!(stale, vec![a.clone()]);
        assert!(!e.convs.contains_key(&a));
        for id in stale {
            tried.remove(&id);
        }
        let plan = e.plan_load(&a, &mut tried).expect("planned again in the same request");
        assert!(matches!(plan.convs.as_slice(), [ConvReq::Ls { .. }]));
    }

    #[test]
    fn panel_load_reads_transcript_hints_within_the_database_budget() {
        let mut e = engine();
        let root = uuid(1);
        let hint = uuid(500);
        for n in 0..DB_LOADS_PER_TICK + 6 {
            e.summaries.insert(uuid(100 + n), Summary { root: Some(root.clone()), ..Default::default() });
        }
        let plan = e.plan_load(&root, &mut HashSet::new()).expect("planned");
        assert_eq!(plan.convs.len(), DB_LOADS_PER_TICK, "root and tree members share one budget");

        let mut e = engine();
        let mut t = Transcript::default();
        t.subagent_hints.insert(hint.clone());
        e.convs.insert(root.clone(), Conv { loaded_once: true, transcript: Some(t), ..Default::default() });
        e.disk.insert(hint.clone(), 1);
        let ids: Vec<String> = e
            .plan_load(&root, &mut HashSet::new())
            .expect("planned")
            .convs
            .iter()
            .map(|r| r.id().to_string())
            .collect();
        assert_eq!(ids, vec![hint]);
    }

    #[test]
    fn a_database_read_planned_before_another_merge_is_stale() {
        let mut e = engine();
        let a = uuid(1);
        let (older, newer) = (e.conv_req(&a), e.conv_req(&a));
        let read = |len| {
            Ok(ConvData::Db(Some((
                DbSig { mtime_ms: len as i64, len },
                vec![LiveCall::default(); len as usize],
                Box::default(),
            ))))
        };
        apply(&mut e, vec![(newer, read(3))]);
        apply(&mut e, vec![(older, read(2))]);
        assert_eq!(e.convs[&a].calls.len(), 3, "the read merged second was planned before the first");
    }

    #[test]
    fn records_are_current_to_the_steps_seen_when_planned() {
        let mut e = engine();
        let a = uuid(1);
        e.ls_loaded.insert(a.clone());
        e.summaries.insert(a.clone(), Summary { step_count: 5, ..Default::default() });
        let req = e.conv_req(&a);
        // The engine loop refreshes the summary while this read is in flight.
        e.summaries.get_mut(&a).unwrap().step_count = 7;
        apply(&mut e, vec![(req, ls_page(&[record(10)]))]);
        assert_eq!(e.convs[&a].loaded_steps, 5);
        let plan = e.plan_convs(&HashSet::from([a.clone()]));
        assert!(plan.convs.iter().any(|r| r.id() == a), "steps changed since the read: read again");
    }

    #[test]
    fn a_failed_database_read_does_not_hold_back_the_language_server_read() {
        let mut e = engine();
        let a = uuid(1);
        let interest = HashSet::from([a.clone()]);
        let req = e.conv_req(&a);
        apply(&mut e, vec![(req, Err("no database yet".to_string()))]);
        assert!(e.plan_convs(&interest).convs.is_empty(), "the database read backs off");
        e.apply_summaries(&json!({ "trajectorySummaries": { a.clone(): {} } }));
        assert!(matches!(e.plan_convs(&interest).convs.as_slice(), [ConvReq::Ls { .. }]));
    }

    #[test]
    fn transcripts_are_read_with_their_conversations_and_late_ones_resynced() {
        let mut e = engine();
        let call = |input: u64, dated: bool, label: Option<&str>| LiveCall {
            call: Call { tokens: Tokens { input, ..Default::default() }, ..Default::default() },
            dated,
            label: label.map(str::to_string),
            ..Default::default()
        };
        let now = Some(Instant::now());
        let (fetched, late, labeled, no_usage, old, undated) = (uuid(1), uuid(2), uuid(3), uuid(4), uuid(5), uuid(6));
        // Fetched (not loaded yet) and late at once: read once.
        e.ls_loaded.insert(fetched.clone());
        e.convs
            .insert(fetched.clone(), Conv { changed_at: now, calls: vec![call(1, false, None)], ..Default::default() });
        e.convs.insert(
            late.clone(),
            Conv { loaded_once: true, changed_at: now, calls: vec![call(1, true, None)], ..Default::default() },
        );
        e.convs.insert(
            undated.clone(),
            Conv {
                loaded_once: true,
                changed_at: now,
                calls: vec![call(1, false, Some("Read"))],
                ..Default::default()
            },
        );
        let settled = [(&labeled, call(1, true, Some("Read"))), (&no_usage, call(0, false, None))];
        for (id, c) in settled {
            e.convs
                .insert(id.clone(), Conv { loaded_once: true, changed_at: now, calls: vec![c], ..Default::default() });
        }
        // Changed longer ago than RESYNC_WINDOW (not representable on Windows shortly after boot).
        if let Some(t) = Instant::now().checked_sub(RESYNC_WINDOW + Duration::from_secs(1)) {
            e.convs.insert(
                old.clone(),
                Conv {
                    loaded_once: true,
                    changed_at: Some(t),
                    calls: vec![call(1, false, None)],
                    ..Default::default()
                },
            );
        }
        let plan = e.plan_convs(&HashSet::from([fetched.clone()]));
        let ids: Vec<&str> = plan.transcripts.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec![fetched.as_str(), late.as_str(), undated.as_str()]);

        let mut tried = HashSet::new();
        let panel = e.plan_load(&uuid(9), &mut tried).expect("planned");
        assert_eq!(panel.transcripts.len(), 1, "a panel load reads only what it loads");
    }

    /// A throwaway app-data directory for tests that read real transcript files.
    struct TempData(PathBuf);

    impl TempData {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("agy-engine-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn write(&self, id: &str, steps: &[u64]) {
            let path = crate::transcript::transcript_path(&self.0, id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let text: String = steps
                .iter()
                .map(|n| {
                    format!("{}\n", json!({ "step_index": n, "type": "PLANNER_RESPONSE", "created_at": step_at(*n) }))
                })
                .collect();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for TempData {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn step_at(n: u64) -> String {
        format!("2026-11-27T0{n}:00:00Z")
    }

    fn record_for(step: u64) -> Value {
        json!({ "chatModel": { "responseModel": "m", "usage": { "inputTokens": 1 } }, "stepIndices": [step] })
    }

    /// One engine-loop round for `id` as `plan_convs` builds it: records, then the planned transcript read.
    fn round(e: &mut Engine, id: &str, records: &[Value]) {
        let req = e.conv_req(id);
        let transcripts = e
            .plan_transcripts(std::slice::from_ref(&req), false)
            .into_iter()
            .map(|(id, at)| {
                let chunk = crate::transcript::read_since(&e.app_data, &id, at);
                (id, chunk)
            })
            .collect();
        let keep = Some(HashSet::from([id.to_string()]));
        e.apply_convs(ConvFetched {
            convs: vec![(req, ls_page(records))],
            subs: Vec::new(),
            transcripts,
            git: Vec::new(),
            keep,
        });
    }

    #[test]
    fn a_changed_conversation_is_dated_from_its_transcript() {
        let data = TempData::new("dating");
        let mut e = engine();
        e.app_data = data.0.clone();
        let a = uuid(1);
        e.ls_loaded.insert(a.clone());
        let dated_at = |e: &Engine, i: usize| {
            let c = &e.convs[&a].calls[i];
            (c.dated, c.at_ms, c.label.clone())
        };
        let expect = |n: u64| (true, parse_ts(&step_at(n)).unwrap(), Some("Final reply".to_string()));

        data.write(&a, &[1, 2]);
        round(&mut e, &a, &[record_for(1)]);
        assert_eq!(dated_at(&e, 0), expect(1));

        // A new record for a step already read: nothing new in the transcript, still dated.
        round(&mut e, &a, &[record_for(1), record_for(2)]);
        assert_eq!(dated_at(&e, 1), expect(2));

        // New lines are read from where the last read ended.
        data.write(&a, &[1, 2, 3]);
        round(&mut e, &a, &[record_for(2), record_for(3)]);
        assert_eq!(dated_at(&e, 2), expect(3));
    }

    #[test]
    fn fetch_reads_planned_transcripts_but_not_for_records_that_failed_first() {
        let data = TempData::new("fetch");
        let (a, b) = (uuid(1), uuid(2));
        data.write(&a, &[1]);
        data.write(&b, &[1]);
        let plan = ConvPlan {
            ls: LsClient::new("127.0.0.1:9", "", false),
            app_data: data.0.clone(),
            // No database for `b`: its records fail without any RPC.
            convs: vec![ConvReq::Db { id: b.clone(), have: None }],
            subs: Vec::new(),
            transcripts: vec![(a.clone(), ReadPos::default()), (b.clone(), ReadPos::default())],
            git: Vec::new(),
            keep: None,
        };
        let fetched = plan.fetch();
        let ids: Vec<&str> = fetched.transcripts.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec![a.as_str()]);
        let (_, chunk) = fetched.transcripts.into_iter().next().unwrap();
        let mut t = Transcript::default();
        assert_eq!(t.apply(chunk), Some(true));
        assert!(t.steps.contains_key(&1));
    }
}
