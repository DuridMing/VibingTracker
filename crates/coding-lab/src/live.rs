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
const STATUS_PER_SESSION: usize = 500;

/// 卡片的伴隨 Event：沒有 pos，附在串流裡帶 pos 的卡片 Event 旁邊（contracts.md §6）。
const COMPANIONS: &[&str] = &[
    "tool.called",
    "tool.updated",
    "tool.result",
    "file.read",
    "file.edit",
    "file.write",
    "git.op",
    "agent.spawned",
];
/// 帶 `model` 的行：agents 列的 `model` 跟著它們走（決定紀錄 #14）
const MODEL_LINES: &[&str] = &["assistant.text", "assistant.thinking", "usage"];
/// 串流裡的狀態行（▶ prompt、⏸ 等你、✔ 完成…）
const STATUS_LINES: &[&str] = &["prompt.submitted", "session.status"];

/// 這個 Event 屬於哪張卡片（toolUseId）；agent.spawned 屬於開它的那個工具呼叫。
fn card_key(e: &Event) -> Option<String> {
    let key = if e.kind == "agent.spawned" {
        "parentToolUseId"
    } else {
        "toolUseId"
    };
    e.payload[key].as_str().map(str::to_string)
}

/// 組串流：`window` 是依 pos 排好的帶 pos Event，`pool` 是候選的伴隨 Event 與狀態行。
/// 附上窗口內卡片的伴隨 Event（同卡只留最新的 tool.updated）；狀態行只取窗口範圍內的——
/// 窗口從 pos 1 開始時沒有下限，否則從窗口第一個 Event 的時間起，`until`（不含）為上限。
/// 結果依 (ts_seen, session, pos) 排序；伴隨 Event 沒有 pos，前端依 toolUseId 併進卡片。
pub fn assemble_stream(window: Vec<Event>, pool: Vec<Event>, until: Option<u64>) -> Vec<Event> {
    let lower = match window.first() {
        Some(first) if first.pos == Some(1) => 0,
        Some(first) => first.ts_seen,
        None => 0,
    };
    let cards: HashSet<String> = window
        .iter()
        .filter(|e| e.kind == "tool.placed" || e.kind == "tool.called")
        .filter_map(card_key)
        .collect();
    let mut latest_update: HashMap<String, u64> = HashMap::new();
    for e in pool.iter().filter(|e| e.kind == "tool.updated") {
        if let Some(id) = card_key(e) {
            let seq = latest_update.entry(id).or_insert(e.seq);
            *seq = (*seq).max(e.seq);
        }
    }
    let mut seen = HashSet::new();
    let mut out = window;
    for e in pool {
        if e.pos.is_some() || !seen.insert((e.session.clone(), e.seq)) {
            continue;
        }
        let keep = if STATUS_LINES.contains(&e.kind.as_str()) {
            e.ts_seen >= lower && until.is_none_or(|u| e.ts_seen < u)
        } else if COMPANIONS.contains(&e.kind.as_str()) {
            card_key(&e).is_some_and(|id| {
                cards.contains(&id)
                    && (e.kind != "tool.updated" || latest_update.get(&id) == Some(&e.seq))
            })
        } else {
            false
        };
        if keep {
            out.push(e);
        }
    }
    out.sort_by(|a, b| {
        (a.ts_seen, &a.session, a.pos, a.seq).cmp(&(b.ts_seen, &b.session, b.pos, b.seq))
    });
    out
}

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
    /// 短名 → (costUsd, thinking)
    models: BTreeMap<String, (f64, u64)>,
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
    /// merge 定期重算的 session 分數（契約 §6）
    score: i64,
}

#[derive(Clone)]
struct AgentLive {
    session: String,
    agent_id: String,
    agent_type: Option<String>,
    /// 這個 agent 最近一行 assistant／usage 的模型短名
    model: Option<String>,
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
    /// danger 解除（工具結束）的時間：列留到 ack（契約 §6）
    cleared_at: Option<u64>,
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
    /// 有連線用的時區，以及上次送出的 kpi 與它的「今天」起點（跨午夜就送完整的 kpi）
    offsets: BTreeMap<i32, Option<(u64, Value)>>,
    /// 卡片的伴隨 Event（同卡只留最新的 tool.updated）與每個 session 的狀態行
    card_events: HashMap<(String, String), Vec<Event>>,
    status_lines: HashMap<String, VecDeque<Event>>,
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
        // `<synthetic>` 這類 Claude Code 自己產生的訊息不算模型
        if MODEL_LINES.contains(&e.kind.as_str())
            && let Some(m) = e.payload["model"].as_str().filter(|m| !m.starts_with('<'))
        {
            let m = short_model(m);
            // VSCode 與 resume 的 SessionStart 不送 model：主模型取 main 第一行（#673）
            if e.agent == "main"
                && let Some(s) = self.sessions.get_mut(&sid)
            {
                s.model.get_or_insert_with(|| m.clone());
            }
            if let Some(a) = self.agents.get_mut(&format!("{sid}/{}", e.agent))
                && a.model.as_ref() != Some(&m)
            {
                a.model = Some(m);
                self.dirty_agents
                    .entry(format!("{sid}/{}", e.agent))
                    .or_insert(false);
            }
        }
        if e.pos.is_none()
            && COMPANIONS.contains(&e.kind.as_str())
            && let Some(id) = card_key(e)
        {
            let list = self.card_events.entry((sid.clone(), id)).or_default();
            if e.kind == "tool.updated" {
                list.retain(|x| x.kind != "tool.updated");
            }
            list.push(e.clone());
        }
        if STATUS_LINES.contains(&e.kind.as_str()) {
            let lines = self.status_lines.entry(sid.clone()).or_default();
            lines.push_back(e.clone());
            while lines.len() > STATUS_PER_SESSION {
                lines.pop_front();
            }
        }
        if let Some(p) = e.pos {
            let stream = self.streams.entry(sid.clone()).or_default();
            stream.push_back(e.clone());
            while stream.len() > STREAM_PER_SESSION {
                // 卡片被擠出串流，它的伴隨 Event 也不用留了
                if let Some(old) = stream.pop_front()
                    && let Some(id) = card_key(&old)
                {
                    self.card_events.remove(&(sid.clone(), id));
                }
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
                let m = counts.models.entry(short_model(&u.model)).or_default();
                m.0 += u.cost_usd;
                m.1 += u.thinking;
            }
            _ => {}
        }

        let Some(s) = self.sessions.get_mut(&sid) else {
            return;
        };
        match payload {
            Payload::SessionStarted(p) => {
                // 第一次的 cwd 是路徑基準（#672），之後的 session.started 不換
                if s.cwd.is_empty() {
                    s.cwd = p.cwd.replace('\\', "/");
                }
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
                        cleared_at: None,
                    },
                );
                self.dirty_attention.insert(id, false);
            }
            Payload::AttentionCleared(a) => {
                let id = format!("{sid}/{}/{}", enum_str(&a.kind), a.reference);
                match self.attention.get_mut(&id) {
                    // danger 解除只標時間，留到 ack
                    Some(row) if row.kind == "danger" => {
                        row.cleared_at = Some(ts);
                        self.dirty_attention.insert(id, false);
                    }
                    Some(_) => {
                        self.attention.remove(&id);
                        self.dirty_attention.insert(id, true);
                    }
                    None => {}
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
                            model: None,
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

    /// merge 定期重算的分數（契約 §6 `sessions.score`）；變了才標記該列要送。
    pub fn set_scores(&mut self, scores: &[(String, i64)]) {
        for (sid, v) in scores {
            if let Some(s) = self.sessions.get_mut(sid)
                && s.score != *v
            {
                s.score = *v;
                self.dirty_sessions.insert(sid.clone());
            }
        }
    }

    /// 重啟重建後對齊 store 的佇列：ack 不是 Event，重放會讓已確認的列跑回來。
    pub fn keep_attention(&mut self, open: &std::collections::HashSet<String>) {
        let gone: Vec<String> = self
            .attention
            .keys()
            .filter(|id| !open.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            self.attention.remove(&id);
            self.dirty_attention.insert(id, true);
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
            let day = day_start(off, now);
            let changed = match self.offsets.get(&off).cloned().flatten() {
                Some((prev_day, prev)) if prev_day == day => {
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
                // 第一次，或跨過當地午夜：整份重算、送完整的 kpi
                _ => current.clone(),
            };
            if changed.as_object().is_some_and(|o| !o.is_empty()) {
                kpi.insert(off, changed);
            }
            self.offsets.insert(off, Some((day, current)));
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
        let mut stream: Vec<Event> = Vec::new();
        for (sid, q) in self.streams.iter().filter(|(s, _)| ch.wants(s)) {
            let window: Vec<Event> = q
                .iter()
                .skip(q.len().saturating_sub(per_session))
                .cloned()
                .collect();
            let mut pool: Vec<Event> = self
                .status_lines
                .get(sid)
                .map(|l| l.iter().cloned().collect())
                .unwrap_or_default();
            for e in &window {
                if let Some(list) =
                    card_key(e).and_then(|id| self.card_events.get(&(sid.clone(), id)))
                {
                    pool.extend(list.iter().cloned());
                }
            }
            stream.extend(assemble_stream(window, pool, None));
        }
        // 還沒有任何 pos 的 session：只有狀態行
        for (sid, lines) in self
            .status_lines
            .iter()
            .filter(|(s, _)| ch.wants(s) && !self.streams.contains_key(*s))
        {
            let _ = sid;
            stream.extend(lines.iter().cloned());
        }
        stream.sort_by(|a, b| {
            (a.ts_seen, &a.session, a.pos, a.seq).cmp(&(b.ts_seen, &b.session, b.pos, b.seq))
        });

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
                    model: None,
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
        let start = day_start(offset_min, now);
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
            for (name, (cost, thinking)) in &c.models {
                let m = total.models.entry(name.clone()).or_default();
                m.0 += cost;
                m.1 += thinking;
            }
        }
        let models: Map<String, Value> = total
            .models
            .iter()
            .map(|(name, (cost, thinking))| {
                (
                    name.clone(),
                    json!({ "costUsd": cost, "thinkingTokens": thinking }),
                )
            })
            .collect();
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
            "models": models,
        })
    }

    /// 某 session 的 `sessions` 列（contracts.md §6）；不在 live 狀態裡回 None。
    pub fn session_row(&self, id: &str) -> Option<Value> {
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
            "status": s.status, "statusSince": s.status_since, "doing": Value::Null, "score": s.score,
            "commits": s.commits, "dangers": s.dangers, "failures": s.failures,
            "agentsLive": agents_live, "costUsd": s.cost, "thinkingTokens": s.thinking, "lastPos": s.last_pos,
        }))
    }
}

fn agent_row(id: &str, a: &AgentLive) -> Value {
    json!({
        "id": id, "session": a.session, "agentId": a.agent_id, "agentType": a.agent_type, "model": a.model, "name": a.name,
        "task": a.task, "status": a.status, "current": a.current, "toolCount": a.tool_count,
        "parentToolUseId": a.parent, "spawnedAt": a.spawned_at, "finishedAt": a.finished_at,
    })
}

fn attention_row(id: &str, a: &AttentionLive) -> Value {
    json!({
        "id": id, "session": a.session, "kind": a.kind, "ref": a.reference,
        "reason": a.reason, "raisedAt": a.raised_at, "clearedAt": a.cleared_at,
    })
}

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// `offset_min` 時區的當地今天 00:00（epoch ms）。
fn day_start(offset_min: i32, now: u64) -> u64 {
    let offset_ms = i64::from(offset_min) * 60_000;
    let local = now as i64 + offset_ms;
    (local - local.rem_euclid(DAY_MS) - offset_ms).max(0) as u64
}

/// 截成 sessions 列的文字長度（firstPrompt、lastPrompt）。
pub fn clip(s: &str) -> String {
    s.chars().take(TEXT_LIMIT).collect()
}

/// `claude-opus-5-5` → `opus 5.5`
pub fn short_model(id: &str) -> String {
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
