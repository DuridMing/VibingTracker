//! live：WebSocket 要推的即時狀態（contracts.md §6、#636）。
//!
//! 純邏輯、不碰 I/O：吃 merge 產生的 Event（[`Live::apply`]），維護彙總列（sessions、agents、attention、KPI）、
//! 暫存區、串流與改動紀錄；呼叫端每 50 ms [`Live::flush`] 一次，打包成一個 [`Frame`]。
//! KPI 的「今天」依各連線的時區：事件依 UTC 每 15 分鐘一格累計（時區偏移都是 15 分鐘的倍數），
//! 任何時區的今天＝從當地午夜起的格子加總，新連線也算得出今天已累積的數字，跨午夜自然歸零。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::derive::relative_path;
use crate::event::{DangerLevel, Event, Payload};

pub const STREAM_PER_SESSION: usize = 3000;
pub const ALL_STREAM_PER_SESSION: usize = 200;
pub const CHANGES_KEPT: usize = 60;
pub const FRAMES_KEPT: usize = 2000;
const SLOT_MS: u64 = 15 * 60 * 1000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const SLOTS_KEPT_MS: u64 = 50 * 60 * 60 * 1000;
const TEXT_LIMIT: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    All,
    Session(String),
}

impl Channel {
    pub fn parse(s: &str) -> Option<Channel> {
        match s {
            "*" => Some(Channel::All),
            _ => s
                .strip_prefix("session:")
                .filter(|id| !id.is_empty())
                .map(|id| Channel::Session(id.into())),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Channel::All => "*".into(),
            Channel::Session(id) => format!("session:{id}"),
        }
    }

    fn wants(&self, session: &str) -> bool {
        match self {
            Channel::All => true,
            Channel::Session(id) => id == session,
        }
    }
}

/// 一個 50 ms 批次。各連線依自己的頻道與時區取用（[`Frame::to_json`]）。
#[derive(Debug)]
pub struct Frame {
    pub no: u64,
    events: Vec<Event>,
    sessions: Vec<(String, Value)>,
    agents: Vec<(String, Value)>,
    attention: Vec<(String, Value)>,
    kpi: BTreeMap<i32, Value>,
}

impl Frame {
    pub fn events_for(&self, ch: &Channel) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| ch.wants(&e.session))
            .cloned()
            .collect()
    }

    pub fn rows_for(&self, ch: &Channel) -> Value {
        let mut rows = Map::new();
        for (kind, list) in [
            ("sessions", &self.sessions),
            ("agents", &self.agents),
            ("attention", &self.attention),
        ] {
            let picked: Vec<Value> = list
                .iter()
                .filter(|(s, _)| ch.wants(s))
                .map(|(_, v)| v.clone())
                .collect();
            if !picked.is_empty() {
                rows.insert(kind.into(), Value::Array(picked));
            }
        }
        Value::Object(rows)
    }

    pub fn kpi_for(&self, offset_min: i32) -> Option<Value> {
        self.kpi.get(&offset_min).cloned()
    }

    /// 這個連線該收到的 frame（`*` 才帶 kpi）；沒有內容就回 None。
    pub fn to_json(&self, ch: &Channel, offset_min: i32) -> Option<Value> {
        let events = self.events_for(ch);
        let mut rows = self.rows_for(ch);
        if *ch == Channel::All
            && let Some(k) = self.kpi_for(offset_min)
            && let Some(obj) = rows.as_object_mut()
        {
            obj.insert("kpi".into(), k);
        }
        if events.is_empty() && rows.as_object().is_none_or(|o| o.is_empty()) {
            return None;
        }
        Some(
            json!({ "t": "frame", "frame": self.no, "ch": ch.name(), "events": events, "rows": rows }),
        )
    }
}

#[derive(Clone, Default)]
struct Counts {
    sessions: HashSet<String>,
    agents: HashSet<String>,
    edits: u64,
    writes: u64,
    commands: u64,
    failures: u64,
    dangers: u64,
    commits: u64,
    thinking: u64,
    cost: f64,
}

#[derive(Clone, Default)]
struct SessionLive {
    host: String,
    cwd: String,
    branch: Option<String>,
    model: Option<String>,
    first_prompt: Option<String>,
    last_prompt: Option<String>,
    started_at: u64,
    ended_at: Option<u64>,
    status: String,
    status_since: u64,
    commits: u64,
    dangers: u64,
    failures: u64,
    cost: f64,
    thinking: u64,
    last_pos: Option<u64>,
}

#[derive(Clone)]
struct AgentLive {
    session: String,
    agent_id: String,
    agent_type: Option<String>,
    name: String,
    task: Option<String>,
    status: &'static str,
    current: Option<String>,
    tool_count: u64,
    parent: Option<String>,
    spawned_at: Option<u64>,
    finished_at: Option<u64>,
}

#[derive(Clone)]
struct AttentionLive {
    session: String,
    kind: String,
    reference: String,
    reason: String,
    raised_at: u64,
}

#[derive(Default)]
pub struct Live {
    sessions: BTreeMap<String, SessionLive>,
    agents: BTreeMap<String, AgentLive>,
    attention: BTreeMap<String, AttentionLive>,
    staged: BTreeMap<(String, String), Vec<Event>>,
    streams: HashMap<String, VecDeque<Event>>,
    changes: VecDeque<Event>,
    slots: BTreeMap<u64, Counts>,
    /// 有連線用的時區，以及上次送出的 kpi
    offsets: BTreeMap<i32, Option<Value>>,
    /// 執行中的背景 subagent（每個 session 只顯示一列「背景」）
    bg_running: HashMap<String, HashSet<String>>,
    pending: Vec<Event>,
    dirty_sessions: BTreeSet<String>,
    dirty_agents: BTreeMap<String, bool>,
    dirty_attention: BTreeMap<String, bool>,
    frame_no: u64,
    history: VecDeque<Arc<Frame>>,
}

impl Live {
    pub fn new() -> Live {
        Live::default()
    }

    pub fn frame_no(&self) -> u64 {
        self.frame_no
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// 有連線使用這個時區（`hello.utcOffsetMin`），之後的 frame 會帶它的 kpi。
    pub fn register_offset(&mut self, offset_min: i32) {
        self.offsets.entry(offset_min).or_insert(None);
    }

    pub fn apply(&mut self, e: &Event) {
        self.pending.push(e.clone());
        let sid = e.session.clone();
        let ts = e.ts_seen;
        let payload = e.payload_typed().ok().flatten();
        let counts = self.slots.entry(ts / SLOT_MS).or_default();
        counts.sessions.insert(sid.clone());

        if !self.sessions.contains_key(&sid) {
            self.sessions.insert(
                sid.clone(),
                SessionLive {
                    host: e.host.clone(),
                    started_at: ts,
                    status: "run".into(),
                    status_since: ts,
                    ..SessionLive::default()
                },
            );
        }
        let ended = self
            .sessions
            .get(&sid)
            .is_some_and(|s| s.ended_at.is_some())
            && !matches!(payload, Some(Payload::SessionStarted(_)));
        if !ended {
            self.ensure_agent(&sid, "main", ts);
        }
        if let Some(p) = e.pos {
            let stream = self.streams.entry(sid.clone()).or_default();
            stream.push_back(e.clone());
            while stream.len() > STREAM_PER_SESSION {
                stream.pop_front();
            }
            if let Some(s) = self.sessions.get_mut(&sid) {
                s.last_pos = Some(p);
            }
        }
        self.dirty_sessions.insert(sid.clone());

        let Some(payload) = payload else { return };
        let counts = self.slots.entry(ts / SLOT_MS).or_default();
        match &payload {
            Payload::FileEdit(_) => counts.edits += 1,
            Payload::FileWrite(w) if w.is_new => counts.writes += 1,
            Payload::ToolCalled(c) => {
                if c.tool == "Bash" {
                    counts.commands += 1;
                }
                if c.danger
                    .as_ref()
                    .is_some_and(|d| d.level == DangerLevel::Danger)
                {
                    counts.dangers += 1;
                }
            }
            Payload::ToolUpdated(u) => {
                if u.status == crate::event::ToolStatus::Failed {
                    counts.failures += 1;
                }
                if u.danger
                    .as_ref()
                    .is_some_and(|d| d.level == DangerLevel::Danger)
                {
                    counts.dangers += 1;
                }
            }
            Payload::GitOp(g) if g.op == "commit" && g.ok => counts.commits += 1,
            Payload::AgentSpawned(a) if a.agent_type.is_some() => {
                counts.agents.insert(format!("{sid}/{}", a.agent_id));
            }
            Payload::Usage(u) => {
                counts.thinking += u.thinking;
                counts.cost += u.cost_usd;
            }
            _ => {}
        }

        let Some(s) = self.sessions.get_mut(&sid) else {
            return;
        };
        match payload {
            Payload::SessionStarted(p) => {
                s.cwd = p.cwd.replace('\\', "/");
                s.model = p.model.as_deref().map(short_model).or(s.model.take());
                s.branch = p.branch.or(s.branch.take());
                s.ended_at = None;
                s.status = "run".into();
                s.status_since = ts;
            }
            Payload::SessionEnded(_) => {
                s.ended_at = Some(ts);
                s.status = "ended".into();
                s.status_since = ts;
                let ids: Vec<String> = self
                    .agents
                    .iter()
                    .filter(|(_, a)| a.session == sid)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    self.agents.remove(&id);
                    self.dirty_agents.insert(id, true);
                }
                self.bg_running.remove(&sid);
            }
            Payload::SessionStatus(p) => {
                s.status = serde_json::to_value(p.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                s.status_since = p.since;
            }
            Payload::PromptSubmitted(p) => {
                let text = clip(&p.text);
                s.first_prompt.get_or_insert_with(|| text.clone());
                s.last_prompt = Some(text);
            }
            Payload::Usage(u) => {
                s.cost += u.cost_usd;
                s.thinking += u.thinking;
            }
            Payload::ModelChanged(m) if e.agent == "main" => s.model = Some(short_model(&m.to)),
            Payload::GitOp(g) => {
                if g.op == "commit" && g.ok {
                    s.commits += 1;
                }
                if g.ok
                    && matches!(g.op.as_str(), "switch" | "checkout" | "worktree")
                    && g.branch.is_some()
                {
                    s.branch = g.branch;
                }
            }
            Payload::ToolResult(r) if !r.ok => s.failures += 1,
            Payload::AttentionRaised(a) => {
                let kind = enum_str(&a.kind);
                if kind == "danger" {
                    s.dangers += 1;
                }
                let id = format!("{sid}/{kind}/{}", a.reference);
                self.attention.insert(
                    id.clone(),
                    AttentionLive {
                        session: sid.clone(),
                        kind,
                        reference: a.reference,
                        reason: a.reason.unwrap_or_default(),
                        raised_at: ts,
                    },
                );
                self.dirty_attention.insert(id, false);
            }
            Payload::AttentionCleared(a) => {
                let id = format!("{sid}/{}/{}", enum_str(&a.kind), a.reference);
                if self.attention.remove(&id).is_some() {
                    self.dirty_attention.insert(id, true);
                }
            }
            Payload::ToolCalled(c) => {
                let cwd = s.cwd.clone();
                if e.pos.is_none() {
                    self.staged
                        .insert((sid.clone(), c.tool_use_id.clone()), vec![e.clone()]);
                }
                let id = self.ensure_agent(&sid, &e.agent, ts);
                if let Some(a) = self.agents.get_mut(&id) {
                    a.current = Some(describe(&c.tool, &c.input, &cwd));
                    a.tool_count += 1;
                    let danger = c
                        .danger
                        .as_ref()
                        .is_some_and(|d| d.level == DangerLevel::Danger);
                    a.status = if danger { "danger" } else { "run" };
                    self.dirty_agents.insert(id, false);
                }
            }
            Payload::ToolUpdated(u) => self.stage_update(&sid, &u.tool_use_id, e),
            Payload::ToolResult(r) => self.stage_update(&sid, &r.tool_use_id, e),
            Payload::ToolPlaced(p) => {
                self.staged.remove(&(sid.clone(), p.tool_use_id));
            }
            Payload::AgentSpawned(a) => match a.agent_type {
                None => {
                    self.bg_running
                        .entry(sid.clone())
                        .or_default()
                        .insert(a.agent_id);
                    self.ensure_agent(&sid, "bg", ts);
                }
                Some(t) => {
                    let id = format!("{sid}/{}", a.agent_id);
                    self.agents.insert(
                        id.clone(),
                        AgentLive {
                            session: sid.clone(),
                            agent_id: a.agent_id.clone(),
                            name: a.name.clone().unwrap_or_else(|| t.clone()),
                            agent_type: Some(t),
                            task: a.task.as_deref().map(clip),
                            status: "run",
                            current: None,
                            tool_count: 0,
                            parent: a.parent_tool_use_id,
                            spawned_at: Some(ts),
                            finished_at: None,
                        },
                    );
                    self.dirty_agents.insert(id, false);
                }
            },
            Payload::AgentFinished(a) => {
                let bg = self
                    .bg_running
                    .get_mut(&sid)
                    .is_some_and(|set| set.remove(&a.agent_id));
                if bg {
                    if self.bg_running.get(&sid).is_none_or(|set| set.is_empty()) {
                        let id = format!("{sid}/bg");
                        if self.agents.remove(&id).is_some() {
                            self.dirty_agents.insert(id, true);
                        }
                    }
                } else {
                    let id = format!("{sid}/{}", a.agent_id);
                    if let Some(row) = self.agents.get_mut(&id) {
                        row.status = "done";
                        row.current = None;
                        row.finished_at = Some(ts);
                        self.dirty_agents.insert(id, false);
                    }
                }
            }
            Payload::TurnStopped(_) => {
                let id = format!("{sid}/main");
                if let Some(a) = self.agents.get_mut(&id) {
                    a.status = "idle";
                    a.current = None;
                    self.dirty_agents.insert(id, false);
                }
            }
            Payload::FileEdit(_) | Payload::FileWrite(_) => {
                self.changes.push_back(e.clone());
                while self.changes.len() > CHANGES_KEPT {
                    self.changes.pop_front();
                }
            }
            _ => {}
        }
    }

    /// 注意佇列「已確認」：移除該列（store 另外記 `acked_at`）。
    pub fn ack(&mut self, session: &str, kind: &str, reference: &str) -> bool {
        let id = format!("{session}/{kind}/{reference}");
        let removed = self.attention.remove(&id).is_some();
        if removed {
            self.dirty_attention.insert(id, true);
        }
        removed
    }

    /// 打包這段時間的變化；沒有任何變化回 None。
    pub fn flush(&mut self, now: u64) -> Option<Arc<Frame>> {
        let mut kpi = BTreeMap::new();
        let offsets: Vec<i32> = self.offsets.keys().copied().collect();
        for off in offsets {
            let current = self.kpi(off, now);
            let changed = match self.offsets.get(&off).cloned().flatten() {
                None => current.clone(),
                Some(prev) => {
                    let mut diff = Map::new();
                    if let (Some(c), Some(p)) = (current.as_object(), prev.as_object()) {
                        for (k, v) in c {
                            if p.get(k) != Some(v) {
                                diff.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    Value::Object(diff)
                }
            };
            if changed.as_object().is_some_and(|o| !o.is_empty()) {
                kpi.insert(off, changed);
            }
            self.offsets.insert(off, Some(current));
        }
        let oldest = now.saturating_sub(SLOTS_KEPT_MS) / SLOT_MS;
        self.slots.retain(|slot, _| *slot >= oldest);

        // 同一個 toolUseId 的 tool.updated 只留最後一個
        let pending = std::mem::take(&mut self.pending);
        let mut seen = HashSet::new();
        let mut events: Vec<Event> = pending
            .into_iter()
            .rev()
            .filter(|e| {
                if e.kind != "tool.updated" {
                    return true;
                }
                let id = e.payload["toolUseId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                seen.insert((e.session.clone(), id))
            })
            .collect();
        events.reverse();

        let sessions: Vec<(String, Value)> = std::mem::take(&mut self.dirty_sessions)
            .into_iter()
            .filter_map(|id| self.session_row(&id).map(|row| (id, row)))
            .collect();
        let agents: Vec<(String, Value)> = std::mem::take(&mut self.dirty_agents)
            .into_iter()
            .map(|(id, deleted)| {
                let session = id.split('/').next().unwrap_or_default().to_string();
                let row = match (deleted, self.agents.get(&id)) {
                    (false, Some(a)) => agent_row(&id, a),
                    _ => json!({ "id": id, "deleted": true }),
                };
                (session, row)
            })
            .collect();
        let attention: Vec<(String, Value)> = std::mem::take(&mut self.dirty_attention)
            .into_iter()
            .map(|(id, deleted)| {
                let session = id.split('/').next().unwrap_or_default().to_string();
                let row = match (deleted, self.attention.get(&id)) {
                    (false, Some(a)) => attention_row(&id, a),
                    _ => json!({ "id": id, "deleted": true }),
                };
                (session, row)
            })
            .collect();

        if events.is_empty()
            && sessions.is_empty()
            && agents.is_empty()
            && attention.is_empty()
            && kpi.is_empty()
        {
            return None;
        }
        self.frame_no += 1;
        let frame = Arc::new(Frame {
            no: self.frame_no,
            events,
            sessions,
            agents,
            attention,
            kpi,
        });
        self.history.push_back(frame.clone());
        while self.history.len() > FRAMES_KEPT {
            self.history.pop_front();
        }
        Some(frame)
    }

    /// 編號 `no` 之後的 frame（斷線接續）；太舊已經丟掉、或編號不合理時回 None，改送 snapshot。
    pub fn frames_since(&self, no: u64) -> Option<Vec<Arc<Frame>>> {
        if no > self.frame_no {
            return None;
        }
        if no == self.frame_no {
            return Some(Vec::new());
        }
        let first = self.history.front()?.no;
        if first > no + 1 {
            return None;
        }
        Some(self.history.iter().filter(|f| f.no > no).cloned().collect())
    }

    pub fn snapshot(&self, ch: &Channel, offset_min: i32, now: u64) -> Value {
        let sessions: Vec<Value> = self
            .sessions
            .keys()
            .filter(|id| ch.wants(id))
            .filter_map(|id| self.session_row(id))
            .collect();
        let agents: Vec<Value> = self
            .agents
            .iter()
            .filter(|(_, a)| ch.wants(&a.session))
            .map(|(id, a)| agent_row(id, a))
            .collect();
        let mut attention: Vec<(&String, &AttentionLive)> = self
            .attention
            .iter()
            .filter(|(_, a)| ch.wants(&a.session))
            .collect();
        attention.sort_by_key(|(id, a)| (a.raised_at, (*id).clone()));
        let attention: Vec<Value> = attention
            .into_iter()
            .map(|(id, a)| attention_row(id, a))
            .collect();
        let mut staged: Vec<&Vec<Event>> = self
            .staged
            .iter()
            .filter(|((s, _), _)| ch.wants(s))
            .map(|(_, v)| v)
            .collect();
        staged.sort_by_key(|v| v.first().map(|e| (e.ts_seen, e.session.clone(), e.seq)));
        let staged: Vec<&Event> = staged.into_iter().flatten().collect();

        let per_session = if *ch == Channel::All {
            ALL_STREAM_PER_SESSION
        } else {
            STREAM_PER_SESSION
        };
        let mut stream: Vec<&Event> = self
            .streams
            .iter()
            .filter(|(s, _)| ch.wants(s))
            .flat_map(|(_, q)| q.iter().skip(q.len().saturating_sub(per_session)))
            .collect();
        stream.sort_by(|a, b| (a.ts_seen, &a.session, a.pos).cmp(&(b.ts_seen, &b.session, b.pos)));

        let mut snap = json!({
            "t": "snapshot", "ch": ch.name(), "frame": self.frame_no,
            "sessions": sessions, "agents": agents, "attention": attention,
            "staged": staged, "stream": stream,
        });
        if *ch == Channel::All {
            snap["kpi"] = self.kpi(offset_min, now);
            snap["changes"] = json!(self.changes.iter().collect::<Vec<_>>());
        }
        snap
    }

    fn ensure_agent(&mut self, sid: &str, agent: &str, ts: u64) -> String {
        // 事件的 agent 欄位：main、subagent 的 id，或 bg
        let id = format!("{sid}/{agent}");
        if !self.agents.contains_key(&id) {
            let (name, agent_type) = match agent {
                "main" => ("main".to_string(), None),
                "bg" => ("背景".to_string(), None),
                other => (other.to_string(), None),
            };
            self.agents.insert(
                id.clone(),
                AgentLive {
                    session: sid.to_string(),
                    agent_id: agent.to_string(),
                    agent_type,
                    name,
                    task: None,
                    status: "run",
                    current: None,
                    tool_count: 0,
                    parent: None,
                    spawned_at: Some(ts),
                    finished_at: None,
                },
            );
            self.dirty_agents.insert(id.clone(), false);
        }
        id
    }

    fn stage_update(&mut self, sid: &str, tool_use_id: &str, e: &Event) {
        if let Some(list) = self
            .staged
            .get_mut(&(sid.to_string(), tool_use_id.to_string()))
        {
            list.retain(|x| x.kind != e.kind);
            list.push(e.clone());
        }
    }

    fn kpi(&self, offset_min: i32, now: u64) -> Value {
        let offset_ms = i64::from(offset_min) * 60_000;
        let local = now as i64 + offset_ms;
        let start = (local - local.rem_euclid(DAY_MS) - offset_ms).max(0) as u64;
        let mut total = Counts::default();
        for c in self.slots.range(start / SLOT_MS..).map(|(_, c)| c) {
            total.sessions.extend(c.sessions.iter().cloned());
            total.agents.extend(c.agents.iter().cloned());
            total.edits += c.edits;
            total.writes += c.writes;
            total.commands += c.commands;
            total.failures += c.failures;
            total.dangers += c.dangers;
            total.commits += c.commits;
            total.thinking += c.thinking;
            total.cost += c.cost;
        }
        json!({
            "live": self.sessions.values().filter(|s| s.status == "run").count(),
            "attention": self.attention.len(),
            "sessions": total.sessions.len(),
            "agents": total.agents.len(),
            "edits": total.edits,
            "writes": total.writes,
            "commands": total.commands,
            "failures": total.failures,
            "dangers": total.dangers,
            "commits": total.commits,
            "thinkingTokens": total.thinking,
            "costUsd": total.cost,
        })
    }

    fn session_row(&self, id: &str) -> Option<Value> {
        let s = self.sessions.get(id)?;
        let agents_live = self
            .agents
            .values()
            .filter(|a| a.session == id && a.agent_id != "main" && a.agent_id != "bg")
            .filter(|a| a.status != "done" && a.status != "fail")
            .count();
        Some(json!({
            "id": id, "host": s.host, "cwd": s.cwd, "branch": s.branch, "model": s.model,
            "firstPrompt": s.first_prompt, "lastPrompt": s.last_prompt,
            "startedAt": s.started_at, "endedAt": s.ended_at,
            "status": s.status, "statusSince": s.status_since, "doing": Value::Null,
            "commits": s.commits, "dangers": s.dangers, "failures": s.failures,
            "agentsLive": agents_live, "costUsd": s.cost, "thinkingTokens": s.thinking, "lastPos": s.last_pos,
        }))
    }
}

fn agent_row(id: &str, a: &AgentLive) -> Value {
    json!({
        "id": id, "session": a.session, "agentId": a.agent_id, "agentType": a.agent_type, "name": a.name,
        "task": a.task, "status": a.status, "current": a.current, "toolCount": a.tool_count,
        "parentToolUseId": a.parent, "spawnedAt": a.spawned_at, "finishedAt": a.finished_at,
    })
}

fn attention_row(id: &str, a: &AttentionLive) -> Value {
    json!({
        "id": id, "session": a.session, "kind": a.kind, "ref": a.reference,
        "reason": a.reason, "raisedAt": a.raised_at,
    })
}

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn clip(s: &str) -> String {
    s.chars().take(TEXT_LIMIT).collect()
}

/// `claude-opus-5-5` → `opus 5.5`
fn short_model(id: &str) -> String {
    let base = id.split('[').next().unwrap_or(id);
    let rest = base.strip_prefix("claude-").unwrap_or(base);
    let mut parts = rest.split('-');
    let family = parts.next().unwrap_or(rest);
    let version: Vec<&str> = parts
        .take_while(|p| p.chars().all(|c| c.is_ascii_digit()) && p.len() <= 2)
        .collect();
    if version.is_empty() {
        family.to_string()
    } else {
        format!("{family} {}", version.join("."))
    }
}

/// Agents 面板的「目前動作」。
fn describe(tool: &str, input: &Value, cwd: &str) -> String {
    let path = || {
        input["file_path"]
            .as_str()
            .map(|p| relative_path(cwd, p))
            .unwrap_or_default()
    };
    match tool {
        "Bash" => {
            let cmd = input["command"].as_str().unwrap_or_default();
            let first = cmd.lines().next().unwrap_or_default();
            format!("$ {}", first.chars().take(80).collect::<String>())
        }
        "Read" => format!("read {}", path()),
        "Edit" => format!("edit {}", path()),
        "Write" => format!("write {}", path()),
        other => other.to_string(),
    }
}
