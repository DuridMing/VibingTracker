//! merge：把 hook 與 transcript 合成 Event（方案 C，PLAN「順序問題與解法」）。
//!
//! 純邏輯、沒有 I/O 也不看時鐘：輸入附上到達時間，逾時由呼叫端定期 [`Merger::tick`]。
//! 串流位置 `pos` 只在 transcript 的行到達（或逾時降級）時配給，每個 session 只增不減，
//! 所以已經顯示的東西永遠不會被往後擠。

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Map, Value};

use crate::derive;
use crate::event::{
    AgentFinished, AgentSpawned, AssistantDisplay, AssistantText, AssistantThinking, Attention,
    AttentionKind, CONTRACT_VERSION, CompactFinished, CompactStarted, Danger, Event, FileEdit,
    FileRead, FileWrite, HunkSource, LineRef, ModelChanged, Payload, Placement, PromptSubmitted,
    SessionEnded, SessionStarted, ToolCalled, ToolPlaced, ToolResult, ToolStatus, ToolUpdated,
    TranscriptHealth, TurnStopped, Usage,
};
use crate::hook::HookInput;
use crate::prices::Prices;

/// 暫存區等 transcript 的上限（實測最慢 4.5 秒的兩倍以上）。
pub const FALLBACK_AFTER_MS: u64 = 10_000;
/// 工具輸出存前截斷的上限（contracts.md §4）。
pub const OUTPUT_LIMIT: usize = 8 * 1024;
const PROMPT_LIMIT: usize = 2 * 1024;
const MESSAGE_LIMIT: usize = 500;

/// 危險指令判斷（規則引擎在 #634）。
pub trait DangerRules: Send {
    fn check(&self, tool: &str, input: &Value) -> Option<Danger>;
}

pub enum Input {
    Hook {
        hook: HookInput,
        ts_seen: u64,
    },
    /// transcript 的一行。`file` 是 `"main"` 或 `"agent-<agent_id>"`，`no` 從 1 起算。
    Line {
        session: String,
        file: String,
        no: u64,
        value: Value,
        ts_seen: u64,
    },
    Degraded {
        session: String,
        file: String,
        error: String,
        ts_seen: u64,
    },
    Recovered {
        session: String,
        file: String,
        ts_seen: u64,
    },
}

struct Card {
    agent: String,
    /// 還在暫存區時是 PreToolUse 的到達時間；進串流後為 None。
    staged_at: Option<u64>,
    result_sent: bool,
}

#[derive(Default)]
struct Session {
    seq: u64,
    pos: u64,
    cards: HashMap<String, Card>,
    seen_uuids: HashSet<String>,
    degraded: HashSet<String>,
    /// 已降級進串流、但 transcript 的 tool_use 行還沒到的卡片
    fallback_waiting: HashSet<String>,
    /// 衍生事件用（#633）
    cwd: Option<String>,
    last_model: HashMap<String, String>,
    seen_messages: HashSet<String>,
    pending_agents: Vec<PendingAgent>,
}

/// 已呼叫 Agent／Task 工具、還沒對到 SubagentStart 的 subagent。
struct PendingAgent {
    tool_use_id: String,
    subagent_type: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
}

/// Write 的內容只留前 200 行（contracts.md §4）。
const WRITE_PREVIEW_LINES: usize = 200;

/// 產生一筆 Event 需要的外層資訊。
#[derive(Default)]
struct Meta {
    agent: String,
    line: Option<LineRef>,
    in_stream: bool,
    ts_model: Option<String>,
    ts_exec: Option<u64>,
    ts_seen: u64,
    raw: Option<Map<String, Value>>,
}

pub struct Merger {
    danger: Box<dyn DangerRules>,
    prices: Prices,
    host: String,
    sessions: BTreeMap<String, Session>,
}

impl Merger {
    pub fn new(danger: Box<dyn DangerRules>) -> Merger {
        Merger {
            danger,
            prices: Prices::builtin(),
            host: "local".into(),
            sessions: BTreeMap::new(),
        }
    }

    pub fn handle(&mut self, input: Input) -> Vec<Event> {
        match input {
            Input::Hook { hook, ts_seen } => self.on_hook(&hook, ts_seen),
            Input::Line {
                session,
                file,
                no,
                value,
                ts_seen,
            } => self.on_line(&session, &file, no, &value, ts_seen),
            Input::Degraded {
                session,
                file,
                error,
                ts_seen,
            } => {
                let mut out = Vec::new();
                let s = self.sessions.entry(session.clone()).or_default();
                s.degraded.insert(file.clone());
                let payload = Payload::TranscriptDegraded(TranscriptHealth {
                    file,
                    error: Some(error),
                });
                out.push(self.emit(
                    &session,
                    payload,
                    Meta {
                        agent: "main".into(),
                        ts_seen,
                        ..Meta::default()
                    },
                ));
                out.extend(self.fall_back(&session, |_| true, ts_seen));
                out
            }
            Input::Recovered {
                session,
                file,
                ts_seen,
            } => {
                self.sessions
                    .entry(session.clone())
                    .or_default()
                    .degraded
                    .remove(&file);
                let payload = Payload::TranscriptRecovered(TranscriptHealth { file, error: None });
                vec![self.emit(
                    &session,
                    payload,
                    Meta {
                        agent: "main".into(),
                        ts_seen,
                        ..Meta::default()
                    },
                )]
            }
        }
    }

    /// 把暫存超過 [`FALLBACK_AFTER_MS`] 的卡片放進串流末端。
    pub fn tick(&mut self, now: u64) -> Vec<Event> {
        let sessions: Vec<String> = self.sessions.keys().cloned().collect();
        let mut out = Vec::new();
        for sid in sessions {
            out.extend(self.fall_back(&sid, |staged_at| staged_at + FALLBACK_AFTER_MS <= now, now));
        }
        out
    }

    fn fall_back(&mut self, sid: &str, due: impl Fn(u64) -> bool, ts_seen: u64) -> Vec<Event> {
        let Some(s) = self.sessions.get(sid) else {
            return Vec::new();
        };
        let mut ids: Vec<(u64, String)> = s
            .cards
            .iter()
            .filter_map(|(id, c)| c.staged_at.filter(|t| due(*t)).map(|t| (t, id.clone())))
            .collect();
        ids.sort();
        let mut out = Vec::new();
        for (_, id) in ids {
            out.push(self.place(sid, &id, Placement::Fallback, None, None, ts_seen));
        }
        out
    }

    /// 把暫存區的卡片放進串流（配給 pos）。
    fn place(
        &mut self,
        sid: &str,
        id: &str,
        placement: Placement,
        line: Option<LineRef>,
        ts_model: Option<String>,
        ts_seen: u64,
    ) -> Event {
        let s = self.sessions.entry(sid.to_string()).or_default();
        let agent = match s.cards.get_mut(id) {
            Some(card) => {
                card.staged_at = None;
                card.agent.clone()
            }
            None => "main".into(),
        };
        if placement == Placement::Fallback {
            s.fallback_waiting.insert(id.to_string());
        }
        let payload = Payload::ToolPlaced(ToolPlaced {
            tool_use_id: id.to_string(),
            placement,
        });
        self.emit(
            sid,
            payload,
            Meta {
                agent,
                line,
                in_stream: true,
                ts_model,
                ts_seen,
                ..Meta::default()
            },
        )
    }

    fn emit(&mut self, sid: &str, payload: Payload, meta: Meta) -> Event {
        let s = self.sessions.entry(sid.to_string()).or_default();
        s.seq += 1;
        let pos = meta.in_stream.then(|| {
            s.pos += 1;
            s.pos
        });
        Event {
            v: CONTRACT_VERSION,
            session: sid.to_string(),
            seq: s.seq,
            kind: payload.type_name().to_string(),
            agent: meta.agent,
            line: meta.line,
            pos,
            ts_model: meta.ts_model,
            ts_exec: meta.ts_exec,
            ts_seen: meta.ts_seen,
            host: self.host.clone(),
            // Payload 只含字串鍵的值，轉換不會失敗
            payload: serde_json::to_value(&payload).unwrap_or(Value::Null),
            raw: meta.raw.filter(|r| !r.is_empty()),
            extra: Map::new(),
        }
    }

    // ── hook

    fn on_hook(&mut self, h: &HookInput, ts_seen: u64) -> Vec<Event> {
        let sid = h.session_id().to_string();
        if let Some(cwd) = h.str_field("cwd").filter(|c| !c.is_empty()) {
            self.sessions.entry(sid.clone()).or_default().cwd = Some(cwd.to_string());
        }
        let agent = agent_of_hook(h);
        let raw = Some(h.raw().clone());
        let meta = || Meta {
            agent: agent.clone(),
            ts_seen,
            raw: raw.clone(),
            ..Meta::default()
        };
        let s = |k: &str| h.str_field(k).map(str::to_string);
        let mut out = Vec::new();
        match h.event_name() {
            "SessionStart" => {
                let p = SessionStarted {
                    cwd: s("cwd").unwrap_or_default(),
                    source: s("source").unwrap_or_else(|| "startup".into()),
                    model: s("model"),
                    transcript_path: s("transcript_path"),
                    branch: None,
                };
                out.push(self.emit(&sid, Payload::SessionStarted(p), meta()));
            }
            "UserPromptSubmit" => {
                let p = PromptSubmitted {
                    text: truncate(&s("prompt").unwrap_or_default(), PROMPT_LIMIT).0,
                    prompt_id: s("prompt_id"),
                };
                out.push(self.emit(&sid, Payload::PromptSubmitted(p), meta()));
            }
            "PreToolUse" => out.extend(self.on_pre_tool_use(h, &sid, agent.clone(), ts_seen)),
            "PostToolUse" => {
                let Some(id) = h.tool_use_id().map(str::to_string) else {
                    return out;
                };
                let p = ToolUpdated {
                    tool_use_id: id.clone(),
                    status: ToolStatus::Done,
                    duration_ms: h.field("duration_ms").and_then(Value::as_u64),
                    error: None,
                    interrupted: None,
                    danger: None,
                };
                out.push(self.emit(&sid, Payload::ToolUpdated(p), meta()));
                let output = response_text(h.field("tool_response"));
                out.extend(self.result_once(&sid, &id, true, None, &output, &agent, ts_seen));
                out.extend(self.git_op(h, &sid, &id, h.field("tool_response"), true, meta()));
            }
            "PostToolUseFailure" => {
                let Some(id) = h.tool_use_id().map(str::to_string) else {
                    return out;
                };
                let error = s("error").unwrap_or_default();
                let p = ToolUpdated {
                    tool_use_id: id.clone(),
                    status: ToolStatus::Failed,
                    duration_ms: h.field("duration_ms").and_then(Value::as_u64),
                    error: Some(truncate(&error, MESSAGE_LIMIT).0),
                    interrupted: h.field("is_interrupt").and_then(Value::as_bool),
                    danger: None,
                };
                out.push(self.emit(&sid, Payload::ToolUpdated(p), meta()));
                let code = exit_code(&error);
                out.extend(self.result_once(&sid, &id, false, code, &error, &agent, ts_seen));
                out.extend(self.git_op(h, &sid, &id, None, false, meta()));
            }
            "PermissionDenied" => {
                let Some(id) = h.tool_use_id().map(str::to_string) else {
                    return out;
                };
                let p = ToolUpdated {
                    tool_use_id: id,
                    status: ToolStatus::Failed,
                    duration_ms: None,
                    error: s("reason"),
                    interrupted: None,
                    danger: None,
                };
                out.push(self.emit(&sid, Payload::ToolUpdated(p), meta()));
            }
            "Stop" => {
                let p = TurnStopped {
                    last_message: s("last_assistant_message")
                        .map(|m| truncate(&m, MESSAGE_LIMIT).0),
                };
                out.push(self.emit(&sid, Payload::TurnStopped(p), meta()));
            }
            "SessionEnd" => {
                let p = SessionEnded {
                    reason: s("reason"),
                };
                out.push(self.emit(&sid, Payload::SessionEnded(p), meta()));
            }
            "PreCompact" => {
                let p = CompactStarted {
                    trigger: s("trigger"),
                };
                out.push(self.emit(&sid, Payload::CompactStarted(p), meta()));
            }
            "PostCompact" => {
                let p = CompactFinished {
                    trigger: s("trigger"),
                    summary: s("compact_summary").map(|m| truncate(&m, PROMPT_LIMIT).0),
                };
                out.push(self.emit(&sid, Payload::CompactFinished(p), meta()));
            }
            "MessageDisplay" => {
                let p = AssistantDisplay {
                    display_id: s("message_id").unwrap_or_default(),
                    turn_id: s("turn_id"),
                    index: h.field("index").and_then(Value::as_u64).unwrap_or(0) as u32,
                    is_final: h.field("final").and_then(Value::as_bool).unwrap_or(true),
                    delta: s("delta").unwrap_or_default(),
                };
                out.push(self.emit(&sid, Payload::AssistantDisplay(p), meta()));
            }
            "SubagentStart" => {
                let Some(agent_id) = s("agent_id").filter(|a| !a.is_empty()) else {
                    return out;
                };
                let agent_type = s("agent_type").filter(|t| !t.is_empty());
                // 對到最近一次、subagent_type 相同（或沒指定）的 Agent／Task 工具呼叫
                let pending = self.sessions.get_mut(&sid).and_then(|ss| {
                    let i = ss.pending_agents.iter().rposition(|p| {
                        p.subagent_type.is_none() || p.subagent_type == agent_type
                    })?;
                    Some(ss.pending_agents.remove(i))
                });
                let p = AgentSpawned {
                    agent_id,
                    agent_type,
                    name: pending.as_ref().and_then(|p| p.description.clone()),
                    task: pending
                        .as_ref()
                        .and_then(|p| p.prompt.as_ref().map(|t| truncate(t, MESSAGE_LIMIT).0)),
                    parent_tool_use_id: pending.map(|p| p.tool_use_id),
                };
                let m = Meta {
                    agent: "main".into(),
                    ..meta()
                };
                out.push(self.emit(&sid, Payload::AgentSpawned(p), m));
            }
            "SubagentStop" => {
                let Some(agent_id) = s("agent_id").filter(|a| !a.is_empty()) else {
                    return out;
                };
                let p = AgentFinished {
                    agent_id,
                    last_message: s("last_assistant_message")
                        .map(|m| truncate(&m, MESSAGE_LIMIT).0),
                    transcript_path: s("agent_transcript_path"),
                };
                out.push(self.emit(&sid, Payload::AgentFinished(p), meta()));
            }
            // Notification、PermissionRequest 在 P3（狀態與注意佇列）處理
            _ => {}
        }
        out
    }

    fn on_pre_tool_use(
        &mut self,
        h: &HookInput,
        sid: &str,
        agent: String,
        ts_seen: u64,
    ) -> Vec<Event> {
        let Some(id) = h.tool_use_id().map(str::to_string) else {
            return Vec::new();
        };
        let tool = h.str_field("tool_name").unwrap_or_default().to_string();
        let input = h.field("tool_input").cloned().unwrap_or(Value::Null);
        let danger = self.danger.check(&tool, &input);
        let raw = Some(h.raw().clone());
        let session = self.sessions.entry(sid.to_string()).or_default();
        let degraded = !session.degraded.is_empty();
        let mut out = Vec::new();

        if session.cards.contains_key(&id) {
            // transcript 先到（少見）：卡片已在串流，只更新狀態
            let p = ToolUpdated {
                tool_use_id: id.clone(),
                status: ToolStatus::Running,
                duration_ms: None,
                error: None,
                interrupted: None,
                danger: danger.clone(),
            };
            let meta = Meta {
                agent: agent.clone(),
                ts_exec: Some(ts_seen),
                ts_seen,
                raw,
                ..Meta::default()
            };
            out.push(self.emit(sid, Payload::ToolUpdated(p), meta));
        } else {
            session.cards.insert(
                id.clone(),
                Card {
                    agent: agent.clone(),
                    staged_at: Some(ts_seen),
                    result_sent: false,
                },
            );
            let p = ToolCalled {
                tool_use_id: id.clone(),
                tool,
                input,
                status: ToolStatus::Running,
                placement: Placement::Staged,
                danger: danger.clone(),
            };
            let meta = Meta {
                agent: agent.clone(),
                ts_exec: Some(ts_seen),
                ts_seen,
                raw,
                ..Meta::default()
            };
            out.push(self.emit(sid, Payload::ToolCalled(p), meta));
            if degraded {
                out.push(self.place(sid, &id, Placement::Fallback, None, None, ts_seen));
            }
        }
        out.extend(self.derive_from_pre(h, sid, &id, &agent, ts_seen));
        // 危險不等 transcript：PreToolUse 一到就進注意佇列
        if let Some(d) = danger {
            let p = Attention {
                kind: AttentionKind::Danger,
                reference: id,
                reason: Some(d.why),
            };
            out.push(self.emit(
                sid,
                Payload::AttentionRaised(p),
                Meta {
                    agent,
                    ts_seen,
                    ..Meta::default()
                },
            ));
        }
        out
    }

    /// PreToolUse 衍生的 file.* 事件（不佔串流位置），以及記下 Agent／Task 呼叫供 SubagentStart 對應。
    fn derive_from_pre(
        &mut self,
        h: &HookInput,
        sid: &str,
        id: &str,
        agent: &str,
        ts_seen: u64,
    ) -> Vec<Event> {
        let tool = h.str_field("tool_name").unwrap_or_default();
        let input = h.field("tool_input").cloned().unwrap_or(Value::Null);
        let meta = || Meta {
            agent: agent.to_string(),
            ts_seen,
            ..Meta::default()
        };
        let cwd = self
            .sessions
            .get(sid)
            .and_then(|s| s.cwd.clone())
            .unwrap_or_default();
        let path = || {
            input["file_path"]
                .as_str()
                .map(|p| derive::relative_path(&cwd, p))
        };
        let tool_use_id = id.to_string();
        let payload = match tool {
            "Read" => path().map(|path| Payload::FileRead(FileRead { tool_use_id, path })),
            "Edit" => path().map(|path| {
                let hunk = h.probe().and_then(|p| p.hunk.clone());
                let hunk_source = if hunk.is_some() {
                    HunkSource::Pre
                } else {
                    HunkSource::None
                };
                Payload::FileEdit(FileEdit {
                    tool_use_id,
                    path,
                    hunk,
                    hunk_source,
                })
            }),
            "Write" => path().map(|path| {
                let content = input["content"].as_str().unwrap_or_default();
                let total = content.lines().count();
                let preview: Vec<&str> = content.lines().take(WRITE_PREVIEW_LINES).collect();
                let mut content = preview.join("\n");
                if total <= WRITE_PREVIEW_LINES
                    && input["content"].as_str().is_some_and(|c| c.ends_with('\n'))
                {
                    content.push('\n');
                }
                Payload::FileWrite(FileWrite {
                    tool_use_id,
                    path,
                    is_new: !h.probe().is_some_and(|p| p.exists),
                    content,
                    truncated: total > WRITE_PREVIEW_LINES,
                })
            }),
            "Agent" | "Task" => {
                let s = |k: &str| {
                    input[k]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .map(str::to_string)
                };
                let pending = PendingAgent {
                    tool_use_id,
                    subagent_type: s("subagent_type"),
                    description: s("description"),
                    prompt: s("prompt"),
                };
                self.sessions
                    .entry(sid.to_string())
                    .or_default()
                    .pending_agents
                    .push(pending);
                None
            }
            _ => None,
        };
        payload
            .map(|p| self.emit(sid, p, meta()))
            .into_iter()
            .collect()
    }

    /// Bash 結束時判斷 git 操作（PostToolUse 帶 `tool_response`；失敗時只看指令）。
    fn git_op(
        &mut self,
        h: &HookInput,
        sid: &str,
        id: &str,
        response: Option<&Value>,
        ok: bool,
        meta: Meta,
    ) -> Option<Event> {
        if h.str_field("tool_name") != Some("Bash") {
            return None;
        }
        let command = h.field("tool_input").and_then(|i| i["command"].as_str())?;
        let mut op = derive::git_op_from(command, response, ok)?;
        op.tool_use_id = id.to_string();
        Some(self.emit(sid, Payload::GitOp(op), Meta { raw: None, ..meta }))
    }

    /// tool.result 以先到者為準（hook 或 transcript），每張卡片只送一次。
    #[allow(clippy::too_many_arguments)]
    fn result_once(
        &mut self,
        sid: &str,
        id: &str,
        ok: bool,
        exit_code: Option<i32>,
        output: &str,
        agent: &str,
        ts_seen: u64,
    ) -> Option<Event> {
        let s = self.sessions.entry(sid.to_string()).or_default();
        let card = s.cards.entry(id.to_string()).or_insert(Card {
            agent: agent.to_string(),
            staged_at: None,
            result_sent: false,
        });
        if card.result_sent {
            return None;
        }
        card.result_sent = true;
        let agent = card.agent.clone();
        let (output, truncated) = truncate(output, OUTPUT_LIMIT);
        let p = ToolResult {
            tool_use_id: id.to_string(),
            ok,
            exit_code,
            output,
            truncated,
        };
        Some(self.emit(
            sid,
            Payload::ToolResult(p),
            Meta {
                agent,
                ts_seen,
                ..Meta::default()
            },
        ))
    }

    // ── transcript

    fn on_line(&mut self, sid: &str, file: &str, no: u64, v: &Value, ts_seen: u64) -> Vec<Event> {
        let s = self.sessions.entry(sid.to_string()).or_default();
        if let Some(uuid) = v["uuid"].as_str()
            && !s.seen_uuids.insert(uuid.to_string())
        {
            return Vec::new();
        }
        let agent = file.strip_prefix("agent-").unwrap_or("main").to_string();
        let line = Some(LineRef {
            file: file.to_string(),
            no,
        });
        let ts_model = v["timestamp"].as_str().map(str::to_string);
        let Some(block) = v["message"]["content"].as_array().and_then(|c| c.first()) else {
            return Vec::new(); // prompt（字串 content）、attachment 等非訊息行
        };
        let meta = |in_stream| Meta {
            agent: agent.clone(),
            line: line.clone(),
            in_stream,
            ts_model: ts_model.clone(),
            ts_seen,
            ..Meta::default()
        };
        let message_id = v["message"]["id"].as_str().unwrap_or_default().to_string();
        let model = v["message"]["model"].as_str().map(str::to_string);
        let delayed = !s.fallback_waiting.is_empty();

        // 模型切換（每個 agent 各自追蹤；`<synthetic>` 這類 Claude Code 自己產生的訊息不算）
        let real_model = model.clone().filter(|m| !m.starts_with('<'));
        let mut out = Vec::new();
        let mut changed = None;
        let mut first_line_of_message = false;
        if v["type"] == "assistant"
            && let Some(m) = &real_model
        {
            match s.last_model.insert(agent.clone(), m.clone()) {
                Some(prev) if prev != *m => changed = Some(prev),
                _ => {}
            }
            first_line_of_message =
                !message_id.is_empty() && s.seen_messages.insert(message_id.clone());
        }
        if let (Some(from), Some(to)) = (changed, &real_model) {
            let p = ModelChanged {
                from: Some(from),
                to: to.clone(),
            };
            out.push(self.emit(sid, Payload::ModelChanged(p), meta(true)));
        }
        // 同一個 message.id 拆成多行時每行的 usage 都一樣（實測），只在第一行算一次
        if first_line_of_message
            && let (Some(m), Some(u)) = (&real_model, v["message"]["usage"].as_object())
        {
            let u = Value::Object(u.clone());
            let n = |v: &Value| v.as_u64().unwrap_or(0);
            let cc = &u["cache_creation"];
            let (w5, w1) = if cc.is_object() {
                (
                    n(&cc["ephemeral_5m_input_tokens"]),
                    n(&cc["ephemeral_1h_input_tokens"]),
                )
            } else {
                (n(&u["cache_creation_input_tokens"]), 0)
            };
            let p = Usage {
                message_id: message_id.clone(),
                model: m.clone(),
                input: n(&u["input_tokens"]),
                output: n(&u["output_tokens"]),
                thinking: n(&u["output_tokens_details"]["thinking_tokens"]),
                cache_read: n(&u["cache_read_input_tokens"]),
                cache_write_1h: w1,
                cache_write_5m: w5,
                cost_usd: self.prices.cost(m, &u),
            };
            out.push(self.emit(sid, Payload::Usage(p), meta(false)));
        }
        let s = self.sessions.entry(sid.to_string()).or_default();

        let main = match (v["type"].as_str(), block["type"].as_str()) {
            (Some("assistant"), Some("thinking")) => {
                let signed = block["signature"].as_str().is_some_and(|x| !x.is_empty());
                let p = AssistantThinking {
                    text: block["thinking"].as_str().unwrap_or_default().to_string(),
                    summarized: !signed,
                    message_id,
                    model,
                    delayed,
                };
                vec![self.emit(sid, Payload::AssistantThinking(p), meta(true))]
            }
            (Some("assistant"), Some("text")) => {
                let p = AssistantText {
                    text: block["text"].as_str().unwrap_or_default().to_string(),
                    message_id,
                    model,
                    delayed,
                };
                vec![self.emit(sid, Payload::AssistantText(p), meta(true))]
            }
            (Some("assistant"), Some("tool_use")) => {
                let Some(id) = block["id"].as_str().map(str::to_string) else {
                    return out;
                };
                if s.fallback_waiting.remove(&id) {
                    return out; // 已降級進串流，不搬位置
                }
                match s.cards.get(&id) {
                    Some(card) if card.staged_at.is_some() => {
                        vec![self.place(sid, &id, Placement::Stream, line, ts_model, ts_seen)]
                    }
                    Some(_) => Vec::new(),
                    None => {
                        s.cards.insert(
                            id.clone(),
                            Card {
                                agent: agent.clone(),
                                staged_at: None,
                                result_sent: false,
                            },
                        );
                        let p = ToolCalled {
                            tool_use_id: id,
                            tool: block["name"].as_str().unwrap_or_default().to_string(),
                            input: block["input"].clone(),
                            status: ToolStatus::Pending,
                            placement: Placement::Stream,
                            danger: None,
                        };
                        vec![self.emit(sid, Payload::ToolCalled(p), meta(true))]
                    }
                }
            }
            (Some("user"), Some("tool_result")) => {
                let Some(id) = block["tool_use_id"].as_str() else {
                    return out;
                };
                let ok = !block["is_error"].as_bool().unwrap_or(false);
                let output = content_text(&block["content"]);
                self.result_once(sid, id, ok, None, &output, &agent, ts_seen)
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        };
        out.extend(main);
        out
    }
}

fn agent_of_hook(h: &HookInput) -> String {
    match (h.str_field("agent_id"), h.str_field("agent_type")) {
        (Some(_), Some("")) => "bg".into(), // 背景 subagent 不開 lane（contracts.md §2）
        (Some(id), _) if !id.is_empty() => id.to_string(),
        _ => "main".into(),
    }
}

/// 截到 `max` bytes 以內、不切壞 UTF-8；回傳是否有截斷。
fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// PostToolUse 的 `tool_response`：Bash 取 stdout（＋stderr），其他工具用精簡 JSON。
fn response_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(r) if r.get("stdout").is_some() => {
            let out = r["stdout"].as_str().unwrap_or_default();
            match r["stderr"].as_str().filter(|e| !e.is_empty()) {
                Some(err) => format!("{out}\n{err}"),
                None => out.to_string(),
            }
        }
        Some(r) => r.to_string(),
    }
}

/// transcript tool_result 的 content：字串，或 `[{type: text, text}]`。
fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// PostToolUseFailure 的 error 以「Exit code N」開頭（實測）。
fn exit_code(error: &str) -> Option<i32> {
    error
        .strip_prefix("Exit code ")?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}
