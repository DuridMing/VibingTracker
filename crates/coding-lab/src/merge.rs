//! merge：把 hook 與 transcript 合成 Event（方案 C，PLAN「順序問題與解法」）。
//!
//! 純邏輯、沒有 I/O 也不看時鐘：輸入附上到達時間，逾時由呼叫端定期 [`Merger::tick`]。
//! 串流位置 `pos` 只在 transcript 的行到達（或逾時降級）時配給，每個 session 只增不減，
//! 所以已經顯示的東西永遠不會被往後擠。

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Map, Value};

use crate::derive;
use crate::event::{
    AgentFinished, AgentSpawned, AssistantDisplay, AssistantText, AssistantThinking, AttentionKind,
    CONTRACT_VERSION, CompactFinished, CompactStarted, Danger, DangerLevel, Event, FileEdit,
    FileRead, FileWrite, HunkSource, LineRef, ModelChanged, Payload, Placement, PromptSubmitted,
    SessionEnded, SessionStarted, ToolCalled, ToolPlaced, ToolResult, ToolStatus, ToolUpdated,
    TranscriptHealth, TurnStopped, Usage,
};
use crate::hook::HookInput;
use crate::prices::Prices;
use crate::status::{self, Change, OpenItem, Signal, Tracker};

/// 暫存區等 transcript 的上限（實測最慢 4.5 秒的兩倍以上）。
pub const FALLBACK_AFTER_MS: u64 = 10_000;
/// 降級進串流的卡片等 transcript tool_use 行的上限；過了就不再把之後的文字標成延遲。
pub const FALLBACK_WAIT_MS: u64 = 60_000;
/// session 結束後在記憶體留多久（晚到的 transcript 行、tail 關掉前最後一次讀檔）。
pub const SESSION_EVICT_AFTER_MS: u64 = 10 * 60 * 1000;
/// 沒有 SessionEnd（終端機被直接關掉）的 session，多久沒有任何事件就移除。
pub const SESSION_IDLE_EVICT_MS: u64 = 24 * 60 * 60 * 1000;
/// SubagentStart 有好幾個候選呼叫時，等 meta.json／PostToolUse 的 agentId 的上限；過了依呼叫順序配對（#680）。
pub const AGENT_PAIR_WAIT_MS: u64 = 1_500;
/// 還沒配到 subagent 的 Agent／Task 呼叫最多留多久（背景 agent 的呼叫在 PostToolUse 就有 agentId，前景的在 SubagentStart 配走）。
const AGENT_CALL_KEEP_MS: u64 = 60 * 60 * 1000;
/// 背景 agent 的 PreToolUse 等執行結果的上限，過了就丟掉。
pub const BG_HOLD_MS: u64 = 60_000;
/// 工具輸出存前截斷的上限（contracts.md §4）。
pub const OUTPUT_LIMIT: usize = 8 * 1024;
const PROMPT_LIMIT: usize = 2 * 1024;
const MESSAGE_LIMIT: usize = 500;
/// transcript 的 `toolUseResult`：使用者按 Esc 中斷或拒絕（實測，樣本 a41c94d2、5d21db12）。
const USER_REJECTED: &str = "User rejected tool use";

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
        /// 送出的 tail 的編號（[`crate::transcript::TailOptions::tail`]）
        tail: u64,
        ts_seen: u64,
    },
    Recovered {
        session: String,
        file: String,
        tail: u64,
        ts_seen: u64,
    },
    /// serve 在 SubagentStart 後讀 subagent 目錄的 `agent-<id>.meta.json` 的結果（#680）。
    AgentMeta {
        session: String,
        agent_id: String,
        meta: AgentMetaRead,
        ts_seen: u64,
    },
    /// serve 停掉或重開了 tail（`file` 為 None 表示整個 session）。新的 tail 從健康狀態開始、
    /// 舊的不會再送 Recovered，所以由這裡清掉降級。
    TailReset {
        session: String,
        file: Option<String>,
        /// 之後有效的 tail 編號下限
        tail: u64,
        ts_seen: u64,
    },
}

/// `agent-<id>.meta.json` 讀到什麼（#680）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentMetaRead {
    /// 有 `toolUseId`：這個 subagent 由那個 Agent／Task 呼叫開出來
    ToolUse(String),
    /// 檔案在但沒有 `toolUseId`：不是 Agent／Task 開的（例如 Skill 的 fork），沒有 parent
    NoToolUse,
    /// 重試到上限都讀不到：退回呼叫順序
    Unreadable,
}

struct Card {
    agent: String,
    /// 還在暫存區時是 PreToolUse 的到達時間；進串流後為 None。
    staged_at: Option<u64>,
    result_sent: bool,
    /// 已送出結束狀態（done／failed）的 tool.updated
    finished: bool,
    /// 已送出 tool.called。PreToolUse 遺失時 PostToolUse／tool_result 會先建出沒有 tool.called 的卡片
    called: bool,
    /// 命中敏感規則（#654）：tool.result 帶 sensitive，遠端送出時剝除
    sensitive: bool,
}

impl Card {
    /// 還沒進暫存區、什麼都還沒送的卡片（PreToolUse 遺失時由 PostToolUse／tool_result 先建出來）
    fn new(agent: &str) -> Card {
        Card {
            agent: agent.to_string(),
            staged_at: None,
            result_sent: false,
            finished: false,
            called: false,
            sensitive: false,
        }
    }
}

#[derive(Default)]
struct Session {
    seq: u64,
    pos: u64,
    cards: HashMap<String, Card>,
    seen_uuids: HashSet<String>,
    degraded: HashSet<String>,
    /// TailReset 之後有效的 tail 編號下限（每個檔、整個 session）：比它舊的 tail 送來的降級訊號不理（#684 R20）
    tail_floor: HashMap<String, u64>,
    tail_floor_all: u64,
    /// 已降級進串流、但 transcript 的 tool_use 行還沒到的卡片：id → (transcript 檔, 開始等的時間)。
    /// 背景 agent 的卡片沒有 transcript 行，不記。
    fallback_waiting: HashMap<String, (String, u64)>,
    /// file.* 的路徑基準：第一次 SessionStart 的 cwd（#672）；之後的 hook 與 SessionStart 都不改
    cwd: Option<String>,
    last_model: HashMap<String, String>,
    seen_messages: HashSet<String>,
    pending_agents: Vec<PendingAgent>,
    /// 已知的 agent_id → 呼叫的 tool_use_id：PostToolUse 的 `tool_response.agentId`、meta.json 的 `toolUseId`（#680）
    known_agents: HashMap<String, String>,
    /// 還在等確定對應的 SubagentStart
    waiting_starts: Vec<WaitingStart>,
    /// 已發過 agent.spawned 的 agent_id（重複的 SubagentStart 不再處理）
    spawned_agents: HashSet<String>,
    /// 每個 Agent／Task 呼叫的名稱與 prompt：呼叫被別的 agent 依順序配走後，確定的對應晚到時仍補得回來
    call_info: HashMap<String, PendingAgent>,
    /// 有 SubagentStart 的 agent_id（真的 subagent）
    started_agents: HashSet<String>,
    /// 已發過 agent.finished 的 agent_id：SubagentStop 與 task-notification 只結束一次（#677）
    finished_agents: HashSet<String>,
    /// 背景 agent 的 PreToolUse：工具通常不會執行，等到 PostToolUse／Failure 才建卡片
    held_bg: HashMap<String, HeldTool>,
    /// 最後一次收到這個 session 輸入的時間，與 SessionEnd 的時間：決定何時從記憶體移除
    last_seen: u64,
    ended_at: Option<u64>,
    /// session.status 與注意佇列（#645）
    status: Tracker,
}

impl Session {
    /// 這個 tail 已經被 TailReset 取代（編號比下限舊）
    fn replaced(&self, file: &str, tail: u64) -> bool {
        let floor = self
            .tail_floor
            .get(file)
            .copied()
            .unwrap_or(0)
            .max(self.tail_floor_all);
        tail < floor
    }
}

struct HeldTool {
    hook: HookInput,
    agent_id: String,
    ts_seen: u64,
}

/// 已呼叫 Agent／Task 工具、還沒對到 SubagentStart 的 subagent。
#[derive(Clone)]
struct PendingAgent {
    tool_use_id: String,
    subagent_type: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    /// PreToolUse 的到達時間
    at: u64,
}

/// 有候選呼叫、還在等確定對應的 SubagentStart。
struct WaitingStart {
    agent_id: String,
    agent_type: Option<String>,
    since: u64,
    /// SubagentStart 的原始 hook，延後發出的 agent.spawned 照樣帶上
    raw: Option<Map<String, Value>>,
}

/// agent 對應的 transcript 檔（[`Input::Line`] 的 `file`）。
fn file_of(agent: &str) -> String {
    if agent == "main" {
        "main".into()
    } else {
        format!("agent-{agent}")
    }
}

/// Write 的內容只留前 200 行（contracts.md §4）。
const WRITE_PREVIEW_LINES: usize = 200;
/// 單次 Edit 超過這麼多行算「大改動」（session 分數的訊號）。
const BIG_EDIT_LINES: u32 = 200;

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
    /// 上一輪 collector（或 evict）留下的編號與路徑基準，見 [`Merger::resume`]
    resumed: HashMap<String, Resumed>,
    /// 費用暴增的基準（#675）；有花費的日子不到 3 天時是 None
    cost_baseline: Option<f64>,
}

#[derive(Default)]
struct Resumed {
    seq: u64,
    pos: u64,
    cwd: Option<String>,
    /// 還開著的注意項目（#679）
    open: Vec<OpenItem>,
    /// 結束時間與是否中斷（D3）；`last_seen` 有值才會在沒回來時補 abandoned（見 [`Merger::resume_state`]）
    ended_at: Option<u64>,
    idle: bool,
    last_seen: Option<u64>,
}

impl Merger {
    pub fn new(danger: Box<dyn DangerRules>) -> Merger {
        Merger {
            danger,
            prices: Prices::builtin(),
            host: "local".into(),
            sessions: BTreeMap::new(),
            resumed: HashMap::new(),
            cost_baseline: None,
        }
    }

    /// collector 重啟時，把 store 裡各 session 已用到的 seq／pos 交給 merge，續跑的 session 接著編號
    /// （seq 重複會被索引忽略、pos 倒退會破壞只 append 的串流）。到 session 第一次產生事件時才套用。
    /// `cwd` 是 file.* 的路徑基準（store 的 sessions.cwd），空字串表示不知道。
    pub fn resume(&mut self, sid: &str, seq: u64, pos: u64, cwd: &str) {
        let r = self.resumed.entry(sid.to_string()).or_default();
        (r.seq, r.pos) = (seq, pos);
        r.cwd = (!cwd.is_empty()).then(|| cwd.to_string());
    }

    /// collector 重啟時，把 store 裡 session 的結束時間、是否中斷（status idle）與最後一筆 Event 的時間
    /// 交給 merge（D3，#679）：session 回到記憶體時種回 Tracker，下一個 prompt 才會送 run；
    /// 已結束的不會被當成丟棄；沒結束、之後也沒再回來的，沒有輸入滿 [`SESSION_IDLE_EVICT_MS`] 時補 abandoned。
    pub fn resume_state(&mut self, sid: &str, ended_at: Option<u64>, idle: bool, last_seen: u64) {
        let r = self.resumed.entry(sid.to_string()).or_default();
        r.ended_at = ended_at;
        r.idle = idle;
        r.last_seen = Some(last_seen);
    }

    /// collector 重啟時，把 store 裡還開著（未解除、未確認）的注意項目交給 merge，session 回到記憶體時
    /// 種回 Tracker，之後的 prompt、工具結束才解得掉它們。`agent` 是 fail 的連續失敗所屬的 agent。
    pub fn resume_attention(
        &mut self,
        sid: &str,
        kind: AttentionKind,
        reference: &str,
        agent: &str,
        raised_at: u64,
    ) {
        let item = OpenItem {
            kind,
            reference: reference.to_string(),
            agent: agent.to_string(),
            raised_at,
        };
        self.resumed
            .entry(sid.to_string())
            .or_default()
            .open
            .push(item);
    }

    /// 取得（必要時建立）session；新建時套用上一輪留下的編號、路徑基準與注意項目，
    /// 之後 Tracker 一被用到就已經是接續的狀態。
    fn session(&mut self, sid: &str) -> &mut Session {
        let resumed = &mut self.resumed;
        self.sessions.entry(sid.to_string()).or_insert_with(|| {
            let mut s = Session::default();
            if let Some(r) = resumed.remove(sid) {
                (s.seq, s.pos, s.cwd) = (r.seq, r.pos, r.cwd);
                s.ended_at = r.ended_at;
                s.last_seen = r.last_seen.unwrap_or(0);
                for item in r.open {
                    s.status.seed_open(item);
                }
                s.status.seed_state(r.ended_at.is_some(), r.idle);
            }
            s
        })
    }

    /// file.* 的路徑基準；還沒套用的 resume 也算（第一個 file.* 在 emit 之前就要用到）。
    fn path_base(&self, sid: &str) -> Option<&str> {
        self.sessions
            .get(sid)
            .and_then(|s| s.cwd.as_deref())
            .or_else(|| self.resumed.get(sid).and_then(|r| r.cwd.as_deref()))
    }

    /// 記憶體裡的 session 數。
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn handle(&mut self, input: Input) -> Vec<Event> {
        let seen = match &input {
            Input::Hook { hook, ts_seen } => Some((hook.session_id(), *ts_seen)),
            Input::Line {
                session, ts_seen, ..
            }
            | Input::Degraded {
                session, ts_seen, ..
            }
            | Input::Recovered {
                session, ts_seen, ..
            }
            | Input::AgentMeta {
                session, ts_seen, ..
            } => Some((session.as_str(), *ts_seen)),
            Input::TailReset { .. } => None,
        };
        if let Some((sid, ts)) = seen {
            let s = self.session(sid);
            s.last_seen = s.last_seen.max(ts);
        }
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
                tail,
            } => {
                let mut out = Vec::new();
                let s = self.session(&session);
                if s.replaced(&file, tail) {
                    return out;
                }
                s.degraded.insert(file.clone());
                let file_name = file.clone();
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
                out.extend(self.fall_back(&session, |c| file_of(&c.agent) == file_name, ts_seen));
                out
            }
            Input::Recovered {
                session,
                file,
                ts_seen,
                tail,
            } => {
                let s = self.session(&session);
                if s.replaced(&file, tail) {
                    return Vec::new();
                }
                s.degraded.remove(&file);
                vec![self.recovered(&session, file, ts_seen)]
            }
            Input::AgentMeta {
                session,
                agent_id,
                meta,
                ts_seen,
            } => match meta {
                AgentMetaRead::ToolUse(tool) => self.agent_known(&session, agent_id, tool, ts_seen),
                AgentMetaRead::NoToolUse => self.agent_without_call(&session, &agent_id, ts_seen),
                AgentMetaRead::Unreadable => self.agent_by_order(&session, &agent_id, ts_seen),
            },
            Input::TailReset {
                session,
                file,
                ts_seen,
                tail,
            } => {
                // 第一個 hook 的 TailReset 排在 hook 本身之前：session 還沒建立也要記下限
                let s = self.session(&session);
                match &file {
                    Some(f) => {
                        let floor = s.tail_floor.entry(f.clone()).or_default();
                        *floor = (*floor).max(tail);
                    }
                    None => s.tail_floor_all = s.tail_floor_all.max(tail),
                }
                // 重開的 tail 從檔尾讀，降級卡片的 tool_use 行不會再來
                s.fallback_waiting
                    .retain(|_, (f, _)| file.as_ref().is_some_and(|reset| reset != f));
                let mut cleared: Vec<String> = match file {
                    Some(f) => s.degraded.take(&f).into_iter().collect(),
                    None => s.degraded.drain().collect(),
                };
                cleared.sort();
                cleared
                    .into_iter()
                    .map(|f| self.recovered(&session, f, ts_seen))
                    .collect()
            }
        }
    }

    /// 結束超過 [`SESSION_EVICT_AFTER_MS`]、或 [`SESSION_IDLE_EVICT_MS`] 沒有任何輸入的 session 移出記憶體；
    /// seq／pos、路徑基準與還開著的注意項目留在 `resumed`，resume 回來時接著用。
    /// 沒有 SessionEnd 就被丟棄的 session 先結束它（`reason: "abandoned"`），否則 live 與 store 永遠是 run。
    fn evict(&mut self, now: u64) -> Vec<Event> {
        let mut out = Vec::new();
        // 重啟前就沒有 SessionEnd、之後也沒再回到記憶體的 session（只在 resumed 裡）
        // 結束時間記成最後一次活動，不是發現它的這一刻（升級後第一次重啟會一次補很多舊 session）
        let stale: Vec<(String, u64)> = self
            .resumed
            .iter()
            .filter_map(|(id, r)| {
                let t = r.last_seen.filter(|_| r.ended_at.is_none())?;
                (t + SESSION_IDLE_EVICT_MS <= now).then(|| (id.clone(), t))
            })
            .collect();
        for (id, last) in stale {
            out.extend(self.end_session(&id, Some("abandoned".into()), last, None));
        }
        let gone: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.ended_at
                    .is_some_and(|t| t + SESSION_EVICT_AFTER_MS <= now)
                    || s.last_seen + SESSION_IDLE_EVICT_MS <= now
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            let abandoned = self
                .sessions
                .get(&id)
                .filter(|s| s.ended_at.is_none() && s.seq > 0)
                .map(|s| s.last_seen);
            if let Some(last) = abandoned {
                out.extend(self.end_session(&id, Some("abandoned".into()), last, None));
            }
            if let Some(s) = self.sessions.remove(&id)
                && (s.seq > 0 || !s.status.open_items().is_empty())
            {
                let r = Resumed {
                    seq: s.seq,
                    pos: s.pos,
                    open: s.status.open_items(),
                    cwd: s.cwd,
                    ended_at: s.ended_at,
                    idle: s.status.is_idle(),
                    last_seen: None,
                };
                self.resumed.insert(id, r);
            }
        }
        out
    }

    /// 結束 session：session.ended 加上狀態機的變化。`raw` 是 SessionEnd hook 的原始內容（merge 自己結束時沒有）。
    fn end_session(
        &mut self,
        sid: &str,
        reason: Option<String>,
        ts_seen: u64,
        raw: Option<Map<String, Value>>,
    ) -> Vec<Event> {
        // 還在等配對的 subagent 先發出 spawn：session.ended 之後 live 會清掉這個 session 的 agents 列
        let waiting: Vec<String> = self
            .session(sid)
            .waiting_starts
            .iter()
            .map(|w| w.agent_id.clone())
            .collect();
        let mut out: Vec<Event> = waiting
            .iter()
            .flat_map(|a| self.agent_by_order(sid, a, ts_seen))
            .collect();
        self.session(sid).ended_at = Some(ts_seen);
        let meta = Meta {
            agent: "main".into(),
            ts_seen,
            raw,
            ..Meta::default()
        };
        out.push(self.emit(sid, Payload::SessionEnded(SessionEnded { reason }), meta));
        let changes = self.tracker(sid).session_end(ts_seen);
        out.extend(self.apply(sid, changes, "main", ts_seen));
        out
    }

    fn recovered(&mut self, sid: &str, file: String, ts_seen: u64) -> Event {
        let payload = Payload::TranscriptRecovered(TranscriptHealth { file, error: None });
        self.emit(
            sid,
            payload,
            Meta {
                agent: "main".into(),
                ts_seen,
                ..Meta::default()
            },
        )
    }

    /// 換成 `prices.json` 覆寫過的單價表（#678）；serve 啟動時讀一次，已算過的費用不重算。
    pub fn set_prices(&mut self, prices: Prices) {
        self.prices = prices;
    }

    /// 費用暴增的基準（#675）：serve 啟動時與每小時由 store 算好交進來。
    pub fn set_cost_baseline(&mut self, baseline: Option<f64>) {
        self.cost_baseline = baseline;
    }

    /// 重啟時補回 session 最近 24 小時的花費（只進滾動區間，不觸發）。
    pub fn seed_cost(&mut self, sid: &str, at: u64, usd: f64) {
        let s = self.session(sid);
        // 新建的 session last_seen 是 0，不更新的話下一次 tick 就被 evict
        s.last_seen = s.last_seen.max(at);
        s.status.seed_cost(at, usd);
    }

    /// 各 session 目前的分數，四捨五入成整數（契約 §6 `sessions.score`）。
    pub fn scores(&self, now: u64) -> Vec<(String, i64)> {
        self.sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.status.score(now).round() as i64))
            .collect()
    }

    /// 把暫存超過 [`FALLBACK_AFTER_MS`] 的卡片放進串流末端。
    pub fn tick(&mut self, now: u64) -> Vec<Event> {
        let mut out = self.evict(now);
        let sessions: Vec<String> = self.sessions.keys().cloned().collect();
        for sid in sessions {
            let changes = match self.sessions.get_mut(&sid) {
                Some(s) => {
                    s.held_bg.retain(|_, h| h.ts_seen + BG_HOLD_MS > now);
                    s.fallback_waiting
                        .retain(|_, (_, since)| *since + FALLBACK_WAIT_MS > now);
                    s.status.tick(now)
                }
                None => Vec::new(),
            };
            out.extend(self.fall_back(
                &sid,
                |c| c.staged_at.is_some_and(|t| t + FALLBACK_AFTER_MS <= now),
                now,
            ));
            out.extend(self.apply(&sid, changes, "main", now));
            out.extend(self.agents_tick(&sid, now));
        }
        out
    }

    /// 把 `due` 選中的暫存卡片依暫存先後放進串流末端。
    fn fall_back(&mut self, sid: &str, due: impl Fn(&Card) -> bool, ts_seen: u64) -> Vec<Event> {
        let Some(s) = self.sessions.get(sid) else {
            return Vec::new();
        };
        let mut ids: Vec<(u64, String)> = s
            .cards
            .iter()
            .filter_map(|(id, c)| c.staged_at.filter(|_| due(c)).map(|t| (t, id.clone())))
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
        let s = self.session(sid);
        let agent = match s.cards.get_mut(id) {
            Some(card) => {
                card.staged_at = None;
                card.agent.clone()
            }
            None => "main".into(),
        };
        if placement == Placement::Fallback && agent != "bg" {
            s.fallback_waiting
                .insert(id.to_string(), (file_of(&agent), ts_seen));
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
        let s = self.session(sid);
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

    /// 狀態機的變化轉成 Event（不佔串流位置）。
    fn apply(&mut self, sid: &str, changes: Vec<Change>, agent: &str, ts_seen: u64) -> Vec<Event> {
        changes
            .into_iter()
            .map(|c| {
                let payload = match c {
                    Change::Raised(a) => Payload::AttentionRaised(a),
                    Change::Cleared(a) => Payload::AttentionCleared(a),
                    Change::Status(st) => Payload::SessionStatus(st),
                };
                let meta = Meta {
                    agent: agent.to_string(),
                    ts_seen,
                    ..Meta::default()
                };
                self.emit(sid, payload, meta)
            })
            .collect()
    }

    fn tracker(&mut self, sid: &str) -> &mut Tracker {
        &mut self.session(sid).status
    }

    // ── hook

    fn on_hook(&mut self, h: &HookInput, ts_seen: u64) -> Vec<Event> {
        let sid = h.session_id().to_string();
        let agent = self.agent_of_hook(&sid, h);
        let mut out = Vec::new();
        if agent == "bg" {
            let (prefix, done) = self.on_background_tool(h, &sid, ts_seen);
            if done {
                return prefix;
            }
            out = prefix;
        }
        let raw = Some(h.raw().clone());
        let meta = || Meta {
            agent: agent.clone(),
            ts_seen,
            raw: raw.clone(),
            ..Meta::default()
        };
        let s = |k: &str| h.str_field(k).map(str::to_string);
        // 已結束（多半是閒置 24 小時被補 abandoned）的 session 又有新的動作，而且不會有 SessionStart
        // （終端機一直開著）：重新打開，status 回到依注意項目決定（#679）
        // 只認 main：SessionEnd（例如 /clear）之後才到的 subagent hook 不代表使用者繼續使用
        if matches!(h.event_name(), "UserPromptSubmit" | "PreToolUse")
            && agent == "main"
            && self.session(&sid).ended_at.is_some()
        {
            self.session(&sid).ended_at = None;
            let changes = self.tracker(&sid).started(ts_seen);
            out.extend(self.apply(&sid, changes, &agent, ts_seen));
        }
        match h.event_name() {
            "SessionStart" => {
                let cwd = s("cwd").unwrap_or_default().replace('\\', "/");
                let base = (self.path_base(&sid).is_none() && !cwd.is_empty()).then(|| cwd.clone());
                let session = self.session(&sid);
                session.ended_at = None;
                if base.is_some() {
                    session.cwd = base;
                }
                let p = SessionStarted {
                    cwd,
                    source: s("source").unwrap_or_else(|| "startup".into()),
                    model: s("model"),
                    transcript_path: s("transcript_path").map(|p| p.replace('\\', "/")),
                    branch: None,
                };
                out.push(self.emit(&sid, Payload::SessionStarted(p), meta()));
                // resume 回來：結束、中斷與 Stop 不再成立；還開著的注意項目決定新的 status（#679）
                let changes = self.tracker(&sid).started(ts_seen);
                out.extend(self.apply(&sid, changes, &agent, ts_seen));
            }
            "UserPromptSubmit" => {
                let prompt = s("prompt").unwrap_or_default();
                let p = PromptSubmitted {
                    text: truncate(&prompt, PROMPT_LIMIT).0,
                    prompt_id: s("prompt_id"),
                    source: status::is_task_notification(&prompt).then(|| "task".into()),
                };
                out.push(self.emit(&sid, Payload::PromptSubmitted(p), meta()));
                // 被 TaskStop 停掉的背景 agent 沒有 SubagentStop，只有這個通知（#677）
                let notice = status::parse_task_notification(&prompt).filter(|n| {
                    self.sessions
                        .get(&sid)
                        .is_some_and(|ss| ss.started_agents.contains(&n.task_id))
                });
                // 背景 subagent 失敗（#674）：status 不是 completed／killed
                if notice
                    .as_ref()
                    .is_some_and(|n| status::task_failed(n.status.as_deref()))
                {
                    self.tracker(&sid).signal(Signal::SubagentFail, ts_seen);
                }
                // 還在等配對就先配好、發出 spawn：agent.finished 不能比 agent.spawned 早
                if let Some(n) = &notice {
                    out.extend(self.agent_by_order(&sid, &n.task_id, ts_seen));
                }
                if let Some(n) = notice
                    && let Some(ss) = self.sessions.get_mut(&sid)
                    && ss.finished_agents.insert(n.task_id.clone())
                {
                    ss.held_bg.retain(|_, h| h.agent_id != n.task_id);
                    let p = AgentFinished {
                        agent_id: n.task_id,
                        last_message: None,
                        transcript_path: None,
                        status: n.status,
                    };
                    out.push(self.emit(&sid, Payload::AgentFinished(p), meta()));
                }
                let changes = self.tracker(&sid).prompt(s("prompt_id"), ts_seen);
                out.extend(self.apply(&sid, changes, &agent, ts_seen));
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
                self.mark_finished(&sid, &id, &agent);
                out.push(self.emit(&sid, Payload::ToolUpdated(p), meta()));
                let output = response_text(h.field("tool_response"));
                out.extend(self.result_once(&sid, &id, true, None, &output, &agent, ts_seen));
                out.extend(self.git_op(h, &sid, &id, h.field("tool_response"), true, meta()));
                // Agent／Task 的結果帶 agentId（背景 agent 啟動時、前景 agent 結束時）：subagent 對應哪個呼叫（#680）
                if matches!(h.str_field("tool_name"), Some("Agent" | "Task"))
                    && let Some(a) = h
                        .field("tool_response")
                        .and_then(|r| r["agentId"].as_str())
                        .filter(|a| !a.is_empty())
                {
                    out.extend(self.agent_known(&sid, a.to_string(), id.clone(), ts_seen));
                }
                out.extend(self.tool_ended(h, &sid, &id, &agent, Some(true), ts_seen));
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
                self.mark_finished(&sid, &id, &agent);
                out.push(self.emit(&sid, Payload::ToolUpdated(p), meta()));
                let code = exit_code(&error);
                out.extend(self.result_once(&sid, &id, false, code, &error, &agent, ts_seen));
                out.extend(self.git_op(h, &sid, &id, None, false, meta()));
                let interrupt = h.field("is_interrupt").and_then(Value::as_bool) == Some(true);
                let ok = (!interrupt).then_some(false);
                out.extend(self.tool_ended(h, &sid, &id, &agent, ok, ts_seen));
            }
            "PermissionDenied" => {
                let Some(id) = h.tool_use_id().map(str::to_string) else {
                    return out;
                };
                self.mark_finished(&sid, &id, &agent);
                let p = ToolUpdated {
                    tool_use_id: id.clone(),
                    status: ToolStatus::Failed,
                    duration_ms: None,
                    error: s("reason"),
                    interrupted: None,
                    danger: None,
                };
                out.push(self.emit(&sid, Payload::ToolUpdated(p), meta()));
                out.extend(self.tool_ended(h, &sid, &id, &agent, None, ts_seen));
            }
            "Stop" => {
                let p = TurnStopped {
                    last_message: s("last_assistant_message")
                        .map(|m| truncate(&m, MESSAGE_LIMIT).0),
                };
                out.push(self.emit(&sid, Payload::TurnStopped(p), meta()));
                if agent == "main" {
                    let changes = self.tracker(&sid).stop(ts_seen);
                    out.extend(self.apply(&sid, changes, &agent, ts_seen));
                }
            }
            "SessionEnd" => {
                out.extend(self.end_session(&sid, s("reason"), ts_seen, raw.clone()));
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
                self.session(&sid).started_agents.insert(agent_id.clone());
                let agent_type = s("agent_type").filter(|t| !t.is_empty());
                out.extend(self.agent_started(&sid, agent_id, agent_type, ts_seen, raw.clone()));
            }
            "SubagentStop" => {
                let Some(agent_id) = s("agent_id").filter(|a| !a.is_empty()) else {
                    return out;
                };
                // 還在等配對就先配好、發出 spawn：agent.finished 不能比 agent.spawned 早
                out.extend(self.agent_by_order(&sid, &agent_id, ts_seen));
                let ss = self.session(&sid);
                ss.held_bg.retain(|_, h| h.agent_id != agent_id);
                if !ss.finished_agents.insert(agent_id.clone()) {
                    return out;
                }
                let p = AgentFinished {
                    agent_id,
                    last_message: s("last_assistant_message")
                        .map(|m| truncate(&m, MESSAGE_LIMIT).0),
                    transcript_path: s("agent_transcript_path").map(|p| p.replace('\\', "/")),
                    status: None,
                };
                out.push(self.emit(&sid, Payload::AgentFinished(p), meta()));
            }
            "PermissionRequest" => {
                let tool = h.str_field("tool_name").unwrap_or_default().to_string();
                let input = h.field("tool_input").cloned().unwrap_or(Value::Null);
                self.tracker(&sid)
                    .permission_request(&tool, &input, ts_seen);
            }
            "PostToolBatch" => {
                let changes = self.tracker(&sid).batch_done(ts_seen);
                out.extend(self.apply(&sid, changes, &agent, ts_seen));
            }
            // Notification（idle_prompt、permission_prompt）只當補強，P3 先不用（PLAN「注意佇列」）
            _ => {}
        }
        out
    }

    // ── subagent 與 Agent／Task 呼叫的配對（#680）
    //
    // SubagentStart 沒有 tool_use_id，到達順序也不可靠。確定的對應有兩個，都可能晚到：PostToolUse 的
    // `tool_response.agentId`（背景 agent 和 SubagentStart 同一毫秒、先後都有；前景 agent 結束時才到），與
    // meta.json 的 `toolUseId`（SubagentStart 後 40–60 ms 才寫好，serve 讀到送 Input::AgentMeta）。Skill 開的
    // fork 也有 SubagentStart，但不是 Agent／Task 開的（meta.json 沒有 toolUseId），所以只要還有候選呼叫、
    // 又還不知道對應，就一律等：meta 有 toolUseId 就配它、沒有就當沒有 parent、讀不到或等太久才依呼叫順序。
    // agent.spawned 每個 agent 只發一次；它的 SubagentStop 先到時先配對再發。

    fn agent_started(
        &mut self,
        sid: &str,
        agent_id: String,
        agent_type: Option<String>,
        ts_seen: u64,
        raw: Option<Map<String, Value>>,
    ) -> Vec<Event> {
        let s = self.session(sid);
        // 同一個 SubagentStart 收到兩次（plugin 和 settings 都掛了）：已經發過或已經在等就不再處理
        if s.spawned_agents.contains(&agent_id)
            || s.waiting_starts.iter().any(|w| w.agent_id == agent_id)
        {
            return Vec::new();
        }
        if let Some(tool) = s.known_agents.get(&agent_id).cloned() {
            return vec![self.spawn_for(sid, agent_id, agent_type, Some(tool), ts_seen, raw)];
        }
        let ty = agent_type.clone();
        let candidates = s.pending_agents.iter().filter(|p| fits(s, p, &ty)).count();
        if candidates == 0 {
            return vec![self.spawn_for(sid, agent_id, agent_type, None, ts_seen, raw)];
        }
        let ended = s.ended_at.is_some();
        let id = agent_id.clone();
        s.waiting_starts.push(WaitingStart {
            agent_id,
            agent_type,
            since: ts_seen,
            raw,
        });
        // session 已經結束：不再等，否則 agent.spawned 會落在 session.ended 之後
        if ended {
            return self.agent_by_order(sid, &id, ts_seen);
        }
        Vec::new()
    }

    /// 知道了某個 agent 對應哪個呼叫；它的 SubagentStart 還在等的話現在配對。
    fn agent_known(
        &mut self,
        sid: &str,
        agent_id: String,
        tool: String,
        ts_seen: u64,
    ) -> Vec<Event> {
        let s = self.session(sid);
        s.known_agents.insert(agent_id.clone(), tool.clone());
        let Some(w) = take_waiting(s, &agent_id) else {
            return Vec::new();
        };
        // 就算這個呼叫已經被別的 agent 依順序配走，parent 也是確定的
        vec![self.spawn_for(sid, w.agent_id, w.agent_type, Some(tool), ts_seen, w.raw)]
    }

    /// meta.json 在但沒有 toolUseId：不是 Agent／Task 開的（Skill 的 fork），沒有 parent。
    fn agent_without_call(&mut self, sid: &str, agent_id: &str, ts_seen: u64) -> Vec<Event> {
        let Some(w) = take_waiting(self.session(sid), agent_id) else {
            return Vec::new();
        };
        vec![self.spawn_for(sid, w.agent_id, w.agent_type, None, ts_seen, w.raw)]
    }

    /// 等不到確定的對應（meta.json 讀不到、等太久、或 SubagentStop 已經到了）：依呼叫順序配對。
    fn agent_by_order(&mut self, sid: &str, agent_id: &str, ts_seen: u64) -> Vec<Event> {
        let s = self.session(sid);
        let Some(w) = take_waiting(s, agent_id) else {
            return Vec::new();
        };
        let ty = w.agent_type.clone();
        let tool = s
            .pending_agents
            .iter()
            .find(|p| fits(s, p, &ty))
            .map(|p| p.tool_use_id.clone());
        vec![self.spawn_for(sid, w.agent_id, w.agent_type, tool, ts_seen, w.raw)]
    }

    /// 等太久的 SubagentStart 依呼叫順序配對；太舊、一直沒配到的呼叫丟掉。
    fn agents_tick(&mut self, sid: &str, now: u64) -> Vec<Event> {
        let s = self.session(sid);
        s.pending_agents.retain(|p| p.at + AGENT_CALL_KEEP_MS > now);
        s.call_info.retain(|_, p| p.at + AGENT_CALL_KEEP_MS > now);
        let due: Vec<String> = s
            .waiting_starts
            .iter()
            .filter(|w| w.since + AGENT_PAIR_WAIT_MS <= now)
            .map(|w| w.agent_id.clone())
            .collect();
        due.iter()
            .flat_map(|a| self.agent_by_order(sid, a, now))
            .collect()
    }

    /// 發出 agent.spawned：`tool` 是配到的呼叫（沒有就是 None），名稱與 prompt 從那個呼叫取，並把它移出候選。
    fn spawn_for(
        &mut self,
        sid: &str,
        agent_id: String,
        agent_type: Option<String>,
        tool: Option<String>,
        ts_seen: u64,
        raw: Option<Map<String, Value>>,
    ) -> Event {
        let s = self.session(sid);
        s.spawned_agents.insert(agent_id.clone());
        if let Some(t) = &tool {
            s.pending_agents.retain(|p| p.tool_use_id != *t);
        }
        let info = tool.as_ref().and_then(|t| s.call_info.get(t)).cloned();
        let p = AgentSpawned {
            agent_id,
            agent_type,
            name: info.as_ref().and_then(|p| p.description.clone()),
            task: info
                .as_ref()
                .and_then(|p| p.prompt.as_ref().map(|t| truncate(t, MESSAGE_LIMIT).0)),
            parent_tool_use_id: tool,
        };
        let meta = Meta {
            agent: "main".into(),
            ts_seen,
            raw,
            ..Meta::default()
        };
        self.emit(sid, Payload::AgentSpawned(p), meta)
    }

    /// hook 回報工具結束：解除這張卡片的 wait／danger；Bash 的成敗算進該 agent 的連續失敗。
    /// `ok` 為 None 表示不算（PermissionDenied、is_interrupt）。
    fn tool_ended(
        &mut self,
        h: &HookInput,
        sid: &str,
        id: &str,
        agent: &str,
        ok: Option<bool>,
        ts_seen: u64,
    ) -> Vec<Event> {
        // 成功的呼叫不移除：背景 agent 的 PostToolUse 常比 SubagentStart 先到（同一毫秒，#680）
        if ok != Some(true) {
            self.drop_pending_agent(sid, id);
        }
        let t = self.tracker(sid);
        let mut changes = t.tool_finished(id, ts_seen);
        match (ok, h.str_field("tool_name")) {
            (Some(ok), Some("Bash")) => changes.extend(t.bash_result(agent, id, ok, ts_seen)),
            // subagent 失敗（#674）：Agent／Task 工具失敗；Esc 中斷（ok 為 None）不算
            (Some(false), Some("Agent" | "Task")) => t.signal(Signal::SubagentFail, ts_seen),
            _ => {}
        }
        self.apply(sid, changes, agent, ts_seen)
    }

    /// Agent／Task 呼叫失敗、被拒或 Esc 中斷（不會有 subagent）：從待配對清單移除，免得之後的 subagent 配到它（#680）。
    fn drop_pending_agent(&mut self, sid: &str, id: &str) {
        self.session(sid)
            .pending_agents
            .retain(|p| p.tool_use_id != id);
    }

    /// 背景 agent（agent_type 沒有值、也沒有 SubagentStart）的工具事件。實測是「下一步建議」：
    /// 只送 PreToolUse 與 PostToolBatch，工具不會執行、也不寫 transcript。所以 PreToolUse 先靜默記住，
    /// 不建卡片、不判危險；真的收到 PostToolUse／Failure 才補建卡片並直接放進串流末端。
    /// 回傳（要先送出的事件, 是否已處理完）；沒處理完就接著照一般流程處理這個 hook。
    fn on_background_tool(&mut self, h: &HookInput, sid: &str, ts_seen: u64) -> (Vec<Event>, bool) {
        let s = self.session(sid);
        let id = h.tool_use_id().map(str::to_string);
        let known = id.as_ref().is_some_and(|i| s.cards.contains_key(i));
        match h.event_name() {
            "PreToolUse" if !known => {
                let Some(id) = id else {
                    return (Vec::new(), true);
                };
                let agent_id = h.str_field("agent_id").unwrap_or_default().to_string();
                s.held_bg.insert(
                    id,
                    HeldTool {
                        hook: h.clone(),
                        agent_id,
                        ts_seen,
                    },
                );
                (Vec::new(), true)
            }
            "PostToolUse" | "PostToolUseFailure" if !known => {
                let held = id.and_then(|i| s.held_bg.remove(&i));
                let Some(held) = held else {
                    return (Vec::new(), true); // 已丟掉或從沒看過：不補半張卡片
                };
                let mut out = self.on_pre_tool_use(&held.hook, sid, "bg".into(), held.ts_seen);
                let tool_id = held.hook.tool_use_id().unwrap_or_default().to_string();
                let staged = self
                    .sessions
                    .get(sid)
                    .and_then(|s| s.cards.get(&tool_id))
                    .is_some_and(|c| c.staged_at.is_some());
                if staged {
                    out.push(self.place(sid, &tool_id, Placement::Fallback, None, None, ts_seen));
                }
                (out, false)
            }
            "PermissionDenied" if !known => {
                if let Some(i) = id {
                    s.held_bg.remove(&i);
                }
                (Vec::new(), true)
            }
            "PostToolBatch" => {
                for c in h
                    .field("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(i) = c["tool_use_id"].as_str() {
                        s.held_bg.remove(i);
                    }
                }
                (Vec::new(), true)
            }
            _ => (Vec::new(), false),
        }
    }

    /// 背景 agent 記成 `"bg"`、不開 lane（contracts.md §2）：`agent_type` 是空字串，或沒有
    /// `agent_type` 且這個 agent 沒有 SubagentStart（「下一步建議」的 PreToolUse 就沒有這個欄位）。
    fn agent_of_hook(&self, sid: &str, h: &HookInput) -> String {
        let Some(id) = h.str_field("agent_id").filter(|a| !a.is_empty()) else {
            return "main".into();
        };
        let started = self
            .sessions
            .get(sid)
            .is_some_and(|s| s.started_agents.contains(id));
        match h.str_field("agent_type") {
            Some("") => "bg".into(),
            None if !started && h.event_name() != "SubagentStart" => "bg".into(),
            _ => id.to_string(),
        }
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
        let sensitive = crate::sensitive::is_sensitive(&tool, &input);
        let raw = Some(h.raw().clone());
        let session = self.session(sid);
        let degraded = session.degraded.contains(&file_of(&agent));
        session.status.tool_started(&id, &tool, &input);
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
                    staged_at: Some(ts_seen),
                    called: true,
                    sensitive,
                    ..Card::new(&agent)
                },
            );
            let (input, input_truncated) = truncate_input(input);
            let p = ToolCalled {
                tool_use_id: id.clone(),
                tool,
                input,
                status: ToolStatus::Running,
                placement: Placement::Staged,
                danger: danger.clone(),
                input_truncated,
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
        // 危險不等 transcript：PreToolUse 一到就進注意佇列；warn 只在串流標示
        if let Some(d) = danger.filter(|d| d.level == DangerLevel::Danger) {
            let changes = self.tracker(sid).danger(&id, d.why, ts_seen);
            out.extend(self.apply(sid, changes, &agent, ts_seen));
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
        let cwd = self.path_base(sid).unwrap_or_default().to_string();
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
                    lines: Some(total as u64),
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
                    at: ts_seen,
                };
                let s = self.session(sid);
                s.call_info
                    .insert(pending.tool_use_id.clone(), pending.clone());
                s.pending_agents.push(pending);
                None
            }
            _ => None,
        };
        let sig = match &payload {
            Some(Payload::FileWrite(w)) if w.is_new => Some(Signal::NewFile),
            Some(Payload::FileEdit(e))
                if e.hunk
                    .as_ref()
                    .is_some_and(|k| k.old_lines.max(k.new_lines) > BIG_EDIT_LINES) =>
            {
                Some(Signal::BigEdit)
            }
            _ => None,
        };
        if let Some(sig) = sig {
            self.tracker(sid).signal(sig, ts_seen);
        }
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
    /// 記下卡片已有結束狀態；回傳這次是不是第一次。
    fn mark_finished(&mut self, sid: &str, id: &str, agent: &str) -> bool {
        let s = self.session(sid);
        let card = s.cards.entry(id.to_string()).or_insert(Card::new(agent));
        !std::mem::replace(&mut card.finished, true)
    }

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
        let s = self.session(sid);
        let card = s.cards.entry(id.to_string()).or_insert(Card::new(agent));
        if card.result_sent {
            return None;
        }
        card.result_sent = true;
        let agent = card.agent.clone();
        let sensitive = card.sensitive;
        let (output, truncated) = truncate(output, OUTPUT_LIMIT);
        let p = ToolResult {
            tool_use_id: id.to_string(),
            ok,
            exit_code,
            output,
            truncated,
            sensitive,
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
        let s = self.session(sid);
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
        let delayed = s.fallback_waiting.values().any(|(f, _)| f == file);

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
            let (w5, w1) = crate::prices::cache_writes(&u);
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
            let (usd, base) = (p.cost_usd, self.cost_baseline);
            self.tracker(sid).cost(ts_seen, usd, base);
            out.push(self.emit(sid, Payload::Usage(p), meta(false)));
        }
        let s = self.session(sid);

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
                if s.fallback_waiting.remove(&id).is_some() {
                    return out; // 已降級進串流，不搬位置
                }
                match s.cards.get(&id) {
                    Some(card) if card.staged_at.is_some() => {
                        vec![self.place(sid, &id, Placement::Stream, line, ts_model, ts_seen)]
                    }
                    Some(card) if card.called => Vec::new(),
                    _ => {
                        let card = s.cards.entry(id.clone()).or_insert(Card::new(&agent));
                        card.called = true;
                        let tool = block["name"].as_str().unwrap_or_default().to_string();
                        card.sensitive = crate::sensitive::is_sensitive(&tool, &block["input"]);
                        let (input, input_truncated) = truncate_input(block["input"].clone());
                        let p = ToolCalled {
                            tool_use_id: id,
                            tool,
                            input,
                            status: ToolStatus::Pending,
                            placement: Placement::Stream,
                            danger: None,
                            input_truncated,
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
                let mut evs = Vec::new();
                // 執行中按 Esc（或在權限詢問按 Esc）沒有任何 hook，tool_result 是唯一的結束訊號
                if self.mark_finished(sid, id, &agent) {
                    let rejected = v["toolUseResult"].as_str() == Some(USER_REJECTED);
                    let error = (!ok).then(|| {
                        let why = v["toolUseResult"].as_str().unwrap_or(&output);
                        truncate(why, MESSAGE_LIMIT).0
                    });
                    let p = ToolUpdated {
                        tool_use_id: id.to_string(),
                        status: if ok {
                            ToolStatus::Done
                        } else {
                            ToolStatus::Failed
                        },
                        duration_ms: None,
                        error,
                        interrupted: rejected.then_some(true),
                        danger: None,
                    };
                    let agent = self
                        .sessions
                        .get(sid)
                        .and_then(|s| s.cards.get(id))
                        .map_or(agent.clone(), |c| c.agent.clone());
                    let m = Meta {
                        agent,
                        ts_seen,
                        ..Meta::default()
                    };
                    evs.push(self.emit(sid, Payload::ToolUpdated(p), m));
                }
                evs.extend(self.result_once(sid, id, ok, None, &output, &agent, ts_seen));
                // Esc 拒絕或中斷時這是唯一的結束訊號；只解除 wait／danger，不算失敗
                if !ok {
                    self.drop_pending_agent(sid, id);
                }
                let changes = self.tracker(sid).tool_finished(id, ts_seen);
                evs.extend(self.apply(sid, changes, &agent, ts_seen));
                evs
            }
            (Some("user"), Some("text"))
                if block["text"]
                    .as_str()
                    .is_some_and(status::is_interrupt_text) =>
            {
                let changes = self.tracker(sid).interrupted(ts_seen);
                self.apply(sid, changes, "main", ts_seen)
            }
            _ => Vec::new(),
        };
        out.extend(main);
        out
    }
}

/// 這個呼叫可以是這種 subagent 的候選：類型相同（或呼叫沒指定），而且不是已知屬於別的 agent 的呼叫。
fn fits(s: &Session, p: &PendingAgent, agent_type: &Option<String>) -> bool {
    !s.known_agents.values().any(|t| *t == p.tool_use_id)
        && (p.subagent_type.is_none() || p.subagent_type == *agent_type)
}

fn take_waiting(s: &mut Session, agent_id: &str) -> Option<WaitingStart> {
    let i = s
        .waiting_starts
        .iter()
        .position(|w| w.agent_id == agent_id)?;
    Some(s.waiting_starts.remove(i))
}

/// 截到 `max` bytes 以內、不切壞 UTF-8；回傳是否有截斷。
/// tool.called 的 input：每個超過 [`OUTPUT_LIMIT`] 的字串截短（#654 R22，Write 大檔佔 JSONL 很多）；
/// 回傳有沒有截到。Write 的預覽與 Edit 的 hunk 用的是 hook 的原始 input，不受影響。
fn truncate_input(v: Value) -> (Value, bool) {
    match v {
        Value::String(s) if s.len() > OUTPUT_LIMIT => {
            (Value::String(truncate(&s, OUTPUT_LIMIT).0), true)
        }
        Value::Array(items) => {
            let mut cut = false;
            let items = items
                .into_iter()
                .map(|x| {
                    let (x, c) = truncate_input(x);
                    cut |= c;
                    x
                })
                .collect();
            (Value::Array(items), cut)
        }
        Value::Object(map) => {
            let mut cut = false;
            let map = map
                .into_iter()
                .map(|(k, x)| {
                    let (x, c) = truncate_input(x);
                    cut |= c;
                    (k, x)
                })
                .collect();
            (Value::Object(map), cut)
        }
        other => (other, false),
    }
}

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
