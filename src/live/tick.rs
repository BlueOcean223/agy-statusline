//! One engine step, split so the ENGINE lock is never held across language_server RPCs, SQLite
//! reads, directory scans or git subprocesses (the side panel's HTTP handlers wait on that lock):
//!
//! 1. `plan_status` (locked) → `StatusPlan::fetch` (unlocked) → `apply_status` (locked):
//!    trajectory summaries, quota, account, `brain/` scan, annotation titles.
//! 2. `plan_convs` (locked) → `ConvPlan::fetch` (unlocked) → `apply_convs` (locked):
//!    generator records, visible subagents' steps, git status, then eviction.
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
const FAIL_BACKOFF: Duration = Duration::from_secs(60);
/// Transcript lines for new records are usually flushed within seconds; stop retrying after this.
const RESYNC_WINDOW: Duration = Duration::from_secs(600);
const ANNOTATIONS_EVERY: Duration = Duration::from_secs(10);
const EVICT_EVERY: Duration = Duration::from_secs(60);
/// Conversations not needed for today's cost or on screen are dropped after this long unused.
const EVICT_IDLE: Duration = Duration::from_secs(600);
const EVICT_IDLE_MS: i64 = 600_000;

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
    git: Vec<String>,
    /// Kept for `apply_convs`: what the bar/panel shows and what today's cost needs.
    keep: HashSet<String>,
}

pub(super) struct ConvFetched {
    convs: Vec<(ConvReq, Result<ConvData, String>)>,
    subs: Vec<(SubReq, Option<Value>)>,
    git: Vec<(String, Option<GitInfo>)>,
    keep: HashSet<String>,
}

impl ConvPlan {
    pub(super) fn fetch(self) -> ConvFetched {
        let convs = self
            .convs
            .into_iter()
            .map(|req| {
                let res = fetch_conv(&self.ls, &self.app_data, &req);
                (req, res)
            })
            .collect();
        let subs = self
            .subs
            .into_iter()
            .map(|r| {
                let j = self.ls.call("GetCascadeTrajectorySteps", &json!({ "cascadeId": r.id, "stepOffset": r.from }));
                (r, j.ok())
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
        ConvFetched { convs, subs, git, keep: self.keep }
    }
}

impl Engine {
    /// One full step without releasing `self` (the CLI; the sidecar uses the phases directly).
    /// Repeats the conversation phase while it finds more to load (subagents discovered in
    /// transcripts read by the previous round), which the sidecar spreads over its ticks.
    pub fn tick(&mut self, interest: &HashSet<String>) {
        let f = self.plan_status().fetch();
        self.apply_status(f);
        for _ in 0..4 {
            let plan = self.plan_convs(interest);
            let more = plan.convs.iter().any(|r| !self.convs.contains_key(r.id()));
            self.apply_convs(plan.fetch());
            if !more {
                break;
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
    pub(super) fn shown_subs(&self, root: &str) -> Vec<String> {
        let now = now_ms();
        let mut ids: Vec<&String> = self
            .summaries
            .iter()
            .filter(|(sid, s)| *sid != root && s.root.as_deref() == Some(root))
            .filter(|(sid, _)| {
                self.is_running(sid) || self.sub_done.get(*sid).is_some_and(|t| now - t < SUB_DONE_SHOW_MS)
            })
            .map(|(sid, _)| sid)
            .collect();
        ids.sort_by_key(|sid| (!self.is_running(sid), std::cmp::Reverse(self.last_call(sid).map(|c| c.at_ms))));
        ids.into_iter().cloned().collect()
    }

    /// Decides what to (re)load. `interest` holds the conversations on screen (chat panes and the
    /// side panel); everything touched today is also kept current for today's cost.
    pub(super) fn plan_convs(&mut self, interest: &HashSet<String>) -> ConvPlan {
        let midnight = local_midnight_ms();
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
            subs_shown.extend(self.shown_subs(root).into_iter().take(MAX_SUBS));
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

        let mut db_budget = DB_LOADS_PER_TICK;
        let mut convs: Vec<ConvReq> = Vec::new();
        let mut queued: HashSet<String> = HashSet::new();
        for (_, id) in todo {
            let req = self.conv_req(id);
            if matches!(req, ConvReq::Db { .. }) {
                if db_budget == 0 {
                    continue;
                }
                db_budget -= 1;
            }
            queued.insert(id.clone());
            convs.push(req);
        }

        // Historical subagents of conversations on screen, found in their transcripts, so a
        // restart doesn't omit subagent cost. Read from their databases; their summaries then
        // name the root, so deeper levels are found on later ticks.
        let mut seeds: Vec<String> = interest.iter().cloned().collect();
        for root in &roots {
            seeds.extend(self.tree(root));
        }
        let mut hints: Vec<String> = Vec::new();
        for cid in &seeds {
            if let Some(t) = self.convs.get(cid).and_then(|c| c.transcript.as_ref()) {
                hints.extend(t.subagent_hints.iter().filter(|sid| *sid != cid).cloned());
            }
        }
        hints.sort();
        hints.dedup();
        for sid in hints {
            if db_budget == 0 {
                break;
            }
            if self.convs.contains_key(&sid) || queued.contains(&sid) || self.probed_hints.contains(&sid) {
                continue;
            }
            self.probed_hints.insert(sid.clone());
            if !self.disk.contains_key(&sid) && !self.app_data.join("brain").join(&sid).is_dir() {
                continue;
            }
            let req = self.conv_req(&sid);
            if matches!(req, ConvReq::Db { .. }) {
                db_budget -= 1;
            }
            queued.insert(sid);
            convs.push(req);
        }

        // Steps for the subagents the bar shows. The first page starts near the end: only the
        // last SUB_STEPS_KEEP steps are kept, and early pages can carry large tool outputs.
        let mut subs = Vec::new();
        let mut shown: Vec<&String> = subs_shown.iter().collect();
        shown.sort();
        for sid in shown {
            if !self.ls_loaded.contains(sid) {
                continue;
            }
            let running = self.is_running(sid);
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
        ConvPlan { ls: self.ls.clone(), app_data: self.app_data.clone(), convs, subs, git, keep }
    }

    pub(super) fn apply_convs(&mut self, f: ConvFetched) {
        let now = Instant::now();
        for (req, res) in f.convs {
            let id = req.id().to_string();
            match res {
                Ok(data) => match self.apply_conv(&req, data) {
                    Applied::Changed => {
                        self.dirty.remove(&id);
                        self.sync_transcript(&id, true);
                    }
                    Applied::Unchanged => {
                        self.dirty.remove(&id);
                    }
                    Applied::Stale => {}
                },
                Err(e) => {
                    if matches!(req, ConvReq::Db { .. }) {
                        self.failed.insert(id.clone(), now);
                    }
                    self.dirty.remove(&id);
                    self.tick_errors.push(e);
                }
            }
        }
        for (r, j) in f.subs {
            if let Some(j) = j {
                self.apply_sub_steps(&r.id, r.from, r.running, &j);
            }
        }
        for (path, info) in f.git {
            self.git.insert(path, (now, info));
        }

        // Pick up late-flushed transcript.jsonl entries (timestamps/labels) without an LS RPC,
        // only for conversations that changed recently: some records never get a label, and
        // re-opening their transcripts every second forever is costly with many subagents.
        let sync_ids: Vec<String> = self
            .convs
            .iter()
            .filter(|(_, c)| c.changed_at.is_some_and(|t| t.elapsed() < RESYNC_WINDOW))
            .filter(|(_, c)| c.calls.iter().any(|call| call.has_usage() && (!call.dated || call.label.is_none())))
            .map(|(id, _)| id.clone())
            .collect();
        for id in sync_ids {
            self.sync_transcript(&id, false);
        }
        self.last_error = self.tick_errors.first().cloned();

        if self.evict_at.is_none_or(|t| t.elapsed() >= EVICT_EVERY) {
            self.evict(&f.keep);
            self.evict_at = Some(now);
        }
    }

    /// Drops state for conversations that are neither on screen nor touched today and have not
    /// been used for a while; they are re-read (cheaply, from the language_server or their
    /// database) if they come back. Today's cost is unaffected: those conversations are kept, and
    /// earlier days live in the persisted usage history.
    fn evict(&mut self, keep: &HashSet<String>) {
        let now = now_ms();
        let today = today_str();
        if self.evict_day != today {
            // Lets yesterday's probed-but-missing subagent ids be tried again; bounds the set.
            self.probed_hints.clear();
            self.evict_day = today;
        }
        self.convs.retain(|id, c| keep.contains(id) || c.fetched_at.is_some_and(|t| t.elapsed() < EVICT_IDLE));
        let convs = &self.convs;
        let ls_loaded = &self.ls_loaded;
        self.summaries.retain(|id, _| ls_loaded.contains(id) || convs.contains_key(id));
        self.dirty.retain(|id| keep.contains(id));
        self.sub_done.retain(|_, t| now - *t < EVICT_IDLE_MS);
        self.hooks.retain(|_, h| now - h.at_ms < EVICT_IDLE_MS);
        let sub_done = &self.sub_done;
        self.sub_steps.retain(|id, c| {
            keep.contains(id) || sub_done.contains_key(id) || c.fetched_at.is_some_and(|t| t.elapsed() < EVICT_IDLE)
        });
        self.git.retain(|_, (at, _)| at.elapsed() < EVICT_IDLE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut e = Engine::new(LsClient::new("127.0.0.1:9", "", false));
        // Never created: nothing in these tests may read the real Antigravity data.
        e.app_data = std::env::temp_dir().join(format!("agy-engine-test-{}", std::process::id()));
        e
    }

    fn uuid(n: usize) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }

    fn record(input: u64) -> Value {
        json!({ "chatModel": { "responseModel": "m", "usage": { "inputTokens": input } }, "stepIndices": [1] })
    }

    fn ls_page(records: &[Value]) -> Result<ConvData, String> {
        Ok(ConvData::Ls(json!({ "generatorMetadata": records })))
    }

    /// Merges fetched conversations, keeping every conversation so eviction stays out of the way.
    fn apply(e: &mut Engine, convs: Vec<(ConvReq, Result<ConvData, String>)>) {
        let keep = e.convs.keys().cloned().chain(convs.iter().map(|(r, _)| r.id().to_string())).collect();
        e.apply_convs(ConvFetched { convs, subs: Vec::new(), git: Vec::new(), keep });
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
        let now = now_ms();
        for n in 0..30 {
            e.disk.insert(uuid(n), now);
        }
        for n in 100..103 {
            e.ls_loaded.insert(uuid(n));
            e.summaries.insert(uuid(n), Summary { modified_ms: now, ..Default::default() });
        }
        let on_screen = uuid(29);
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
        let mut t = Transcript::new(&e.app_data, &root);
        t.subagent_hints.extend([on_disk.clone(), missing.clone()]);
        e.convs.insert(root.clone(), Conv { loaded_once: true, transcript: Some(t), ..Default::default() });
        e.disk.insert(on_disk.clone(), 1); // an old subagent: not wanted for today's cost

        let interest = HashSet::from([root.clone()]);
        let ids = |p: &ConvPlan| p.convs.iter().map(|r| r.id().to_string()).collect::<Vec<_>>();
        let first = e.plan_convs(&interest);
        assert!(ids(&first).contains(&on_disk));
        assert!(!ids(&first).contains(&missing), "no database or transcript: nothing to read");
        let second = e.plan_convs(&interest);
        assert!(!ids(&second).contains(&on_disk));
    }

    #[test]
    fn eviction_keeps_wanted_conversations_and_drops_idle_ones() {
        let mut e = engine();
        let long_ago = Instant::now().checked_sub(EVICT_IDLE + Duration::from_secs(60)).unwrap();
        let (kept, idle, recent) = (uuid(1), uuid(2), uuid(3));
        for (id, at) in [(&kept, long_ago), (&idle, long_ago), (&recent, Instant::now())] {
            e.convs.insert(id.clone(), Conv { fetched_at: Some(at), ..Default::default() });
            e.summaries.insert(id.clone(), Summary::default());
        }
        e.evict(&HashSet::from([kept.clone()]));
        assert!(e.convs.contains_key(&kept) && e.convs.contains_key(&recent));
        assert!(!e.convs.contains_key(&idle));
        assert!(!e.summaries.contains_key(&idle), "summaries go with their conversation");
    }
}
