//! session.status、注意佇列與 session 分數（#645，PLAN「注意佇列」「session 分數」）。
//!
//! 純邏輯、不看時鐘：merge 把每個相關的 hook／transcript 行轉成這裡的方法呼叫並帶入時間，
//! 逾時（wait 的 1 秒、done 的 5 秒靜默期）由 [`Tracker::tick`] 處理。回傳的 [`Change`] 由 merge 轉成 Event。

use std::collections::HashMap;

use serde_json::Value;

use crate::event::{Attention, AttentionKind, SessionState, SessionStatus};

/// PermissionRequest 在這段時間內解除就不進佇列（自動決定的權限詢問，實測 12 ms）。
pub const WAIT_AFTER_MS: u64 = 1_000;
/// main agent 的 Stop 後要靜默這麼久才標「完成待確認」。
pub const DONE_AFTER_MS: u64 = 5_000;
/// 「等你」保底判斷的預設門檻（#706）：main 的最後一行是 assistant 文字，這麼久沒有任何事件就推測在等使用者。
pub const WAIT_GUESS_AFTER_MS: u64 = 30_000;
/// 推測出來的 wait 的 ref 前綴（真的 PermissionRequest 的 ref 是 toolUseId）。
pub const GUESS_REF_PREFIX: &str = "guess-";
/// 同一個 agent 連續幾次 Bash 失敗才進佇列。
pub const FAIL_STREAK: u32 = 2;

/// transcript 裡使用者按 Esc 的兩種行（回覆中；工具執行中或權限詢問時）。實測兩者都沒有任何 hook。
pub fn is_interrupt_text(text: &str) -> bool {
    text.starts_with("[Request interrupted by user")
}

/// 背景工作（run_in_background）完成時叫醒 Claude 的 UserPromptSubmit，不是使用者輸入。
pub fn is_task_notification(prompt: &str) -> bool {
    prompt.trim_start().starts_with("<task-notification>")
}

/// 背景工作的結束通知（`<task-notification>`）的欄位。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskNotification {
    pub task_id: String,
    pub tool_use_id: Option<String>,
    pub status: Option<String>,
}

/// 解析 `<task-notification>`；沒有 `<task-id>` 回 None。
pub fn parse_task_notification(prompt: &str) -> Option<TaskNotification> {
    if !is_task_notification(prompt) {
        return None;
    }
    let tag = |name: &str| -> Option<String> {
        let open = format!("<{name}>");
        let start = prompt.find(&open)? + open.len();
        let end = prompt[start..].find(&format!("</{name}>"))? + start;
        let v = prompt[start..end].trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    Some(TaskNotification {
        task_id: tag("task-id")?,
        tool_use_id: tag("tool-use-id"),
        status: tag("status"),
    })
}

/// 背景工作的結束狀態算不算失敗：`completed`、`killed`（主動停掉）以外的值才算；不知道就不算。
pub fn task_failed(status: Option<&str>) -> bool {
    status.is_some_and(|s| s != "completed" && s != "killed")
}

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// 單 session 最近 24 小時的費用超過基準幾倍算暴增（PLAN「session 分數」）
pub const COST_SPIKE_FACTOR: f64 = 3.0;
/// 基準往回看幾天（不含最近 24 小時）
pub const COST_BASELINE_DAYS: u64 = 7;
/// 有花費的日子少於幾天時不算
pub const COST_MIN_DAYS: usize = 3;

/// 費用暴增的基準（#675，方案 C）：過去 7 天（不含最近 24 小時）每個 (session, UTC 日) 的平均費用。
/// `usage` 是 (session, ts_seen, costUsd)；有花費的 UTC 日不到 3 天回 None。
pub fn cost_baseline(usage: &[(String, u64, f64)], now: u64) -> Option<f64> {
    let end = now.saturating_sub(DAY_MS);
    let start = end.saturating_sub(COST_BASELINE_DAYS * DAY_MS);
    let mut per: HashMap<(&str, u64), f64> = HashMap::new();
    for (session, at, usd) in usage {
        if *at >= start && *at < end && *usd > 0.0 {
            *per.entry((session.as_str(), at / DAY_MS)).or_default() += usd;
        }
    }
    let days: std::collections::HashSet<u64> = per.keys().map(|(_, d)| *d).collect();
    if days.len() < COST_MIN_DAYS {
        return None;
    }
    Some(per.values().sum::<f64>() / per.len() as f64)
}

pub enum Change {
    Raised(Attention),
    Cleared(Attention),
    Status(SessionStatus),
}

struct Open {
    kind: AttentionKind,
    reference: String,
    raised_at: u64,
}

/// 還開著的注意項目：session 移出記憶體或 collector 重啟時留下，回來時種回 [`Tracker`]（#679）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenItem {
    pub kind: AttentionKind,
    pub reference: String,
    /// fail 屬於哪個 agent 的連續失敗（其他種類不用）
    pub agent: String,
    pub raised_at: u64,
}

struct Running {
    id: String,
    tool: String,
    input: Value,
    /// 呼叫這個工具的 agent（main、subagent 的 agent_id）
    agent: String,
}

/// 一個 session 的狀態。
pub struct Tracker {
    cfg: ScoreConfig,
    state: SessionState,
    open: Vec<Open>,
    running: Vec<Running>,
    /// 已收到 PermissionRequest、還沒滿 1 秒的 (tool_use_id, 到達時間)
    pending_wait: Vec<(String, u64)>,
    stop_at: Option<u64>,
    /// 這一輪 prompt 已經收到 Stop：交給「完成待確認」，不再推測「等你」
    stopped: bool,
    /// main 最後一行是 assistant 文字時的 (推測 wait 的 ref, 時間)；之後有任何事件就取消（#706）
    guess_from: Option<(String, u64)>,
    /// 推測門檻，0 表示關閉
    guess_after: u64,
    prompt_ref: Option<String>,
    fail_streak: HashMap<String, u32>,
    /// agent → 它目前未解除的 fail 的 ref（ref 照契約是 toolUseId）
    fail_open: HashMap<String, String>,
    idle: bool,
    ended: bool,
    /// 重啟或移出記憶體後種回：store／live 的 status 可能和這裡算的不同（例如已確認的 done 不會種回），
    /// 下一次重算不論有沒有變都送一次（K-6）
    report_next: bool,
    /// 重啟或移出記憶體後回來：store 的狀態是 done／fail，但那個注意項目已經確認、沒有種回（ack 不是 Event，
    /// 沒重啟時 merge 不知道 ack，那一項一直開著）。在沒重啟也會解除它的事件之前照樣是這個狀態，
    /// 上一輪晚到的行或工具結束不能把它算成 run（#749 長情境）
    seeded: Option<SessionState>,
    signals: Vec<(Signal, u64)>,
    /// 最近 24 小時的 (時間, 費用)，CostSpike 用（#675）
    recent_cost: std::collections::VecDeque<(u64, f64)>,
    /// `recent_cost` 的合計，進出時加減（E-M10）
    recent_total: f64,
    last_spike: Option<u64>,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            cfg: ScoreConfig::default(),
            state: SessionState::Run,
            open: Vec::new(),
            running: Vec::new(),
            pending_wait: Vec::new(),
            stop_at: None,
            stopped: false,
            guess_from: None,
            guess_after: WAIT_GUESS_AFTER_MS,
            prompt_ref: None,
            fail_streak: HashMap::new(),
            fail_open: HashMap::new(),
            idle: false,
            ended: false,
            report_next: false,
            seeded: None,
            signals: Vec::new(),
            recent_cost: Default::default(),
            recent_total: 0.0,
            last_spike: None,
        }
    }
}

impl Tracker {
    pub fn tool_started(&mut self, id: &str, tool: &str, input: &Value, agent: &str) {
        self.running.push(Running {
            id: id.to_string(),
            tool: tool.to_string(),
            input: input.clone(),
            agent: agent.to_string(),
        });
    }

    /// PermissionRequest 沒有 tool_use_id：對回最近一張 tool_name＋tool_input 相同、還在跑的卡片。
    pub fn permission_request(&mut self, tool: &str, input: &Value, now: u64) {
        let taken = |id: &str, s: &Tracker| {
            s.pending_wait.iter().any(|(w, _)| w == id) || s.is_open(AttentionKind::Wait, id)
        };
        let id = self
            .running
            .iter()
            .rev()
            .find(|r| r.tool == tool && r.input == *input && !taken(&r.id, self))
            .map(|r| r.id.clone());
        if let Some(id) = id {
            self.pending_wait.push((id, now));
        }
    }

    pub fn danger(&mut self, id: &str, reason: String, now: u64) -> Vec<Change> {
        let mut out = vec![self.raise(AttentionKind::Danger, id, Some(reason), now)];
        out.extend(self.recompute(now));
        out
    }

    /// 工具結束（hook 的 Post／PostFailure／Denied，或 transcript 的 tool_result）。
    pub fn tool_finished(&mut self, id: &str, now: u64) -> Vec<Change> {
        self.running.retain(|r| r.id != id);
        self.pending_wait.retain(|(w, _)| w != id);
        let mut out = Vec::new();
        for kind in [AttentionKind::Wait, AttentionKind::Danger] {
            out.extend(self.clear(kind, id));
        }
        out.extend(self.recompute(now));
        out
    }

    /// hook 的 Bash 結果。只有 hook 會呼叫，所以 Esc 中斷（只有 transcript）不算失敗。
    pub fn bash_result(&mut self, agent: &str, id: &str, ok: bool, now: u64) -> Vec<Change> {
        let streak = self.fail_streak.entry(agent.to_string()).or_insert(0);
        let mut out = Vec::new();
        if ok {
            *streak = 0;
            self.unseed(SessionState::Fail);
            if let Some(r) = self.fail_open.remove(agent) {
                out.extend(self.clear(AttentionKind::Fail, &r));
            }
        } else {
            *streak += 1;
            if *streak >= FAIL_STREAK && !self.fail_open.contains_key(agent) {
                self.fail_open.insert(agent.to_string(), id.to_string());
                out.push(self.raise(AttentionKind::Fail, id, Some("連續失敗".into()), now));
            }
        }
        out.extend(self.recompute(now));
        out
    }

    /// subagent（含背景 agent）結束：之後不會再有它的 Bash 成功來解除它的連續失敗，跟著結束一起清掉（K-1）。
    pub fn agent_finished(&mut self, agent: &str, now: u64) -> Vec<Change> {
        self.fail_streak.remove(agent);
        self.unseed(SessionState::Fail);
        let mut out = Vec::new();
        if let Some(r) = self.fail_open.remove(agent) {
            out.extend(self.clear(AttentionKind::Fail, &r));
        }
        out.extend(self.recompute(now));
        out
    }

    /// `agent` 的一批工具結束：被自動駁回的權限詢問只有這個訊號（實測 12 ms）。只清這個 agent 的工具
    /// 在等的；不知道是誰的（推測的「等你」、重啟前種回的）算 main 的（K-11）。
    pub fn batch_done(&mut self, agent: &str, now: u64) -> Vec<Change> {
        let owner = |running: &[Running], id: &str| {
            running
                .iter()
                .find(|r| r.id == id)
                .map_or("main", |r| r.agent.as_str())
                == agent
        };
        self.pending_wait
            .retain(|(id, _)| !owner(&self.running, id));
        let refs: Vec<String> = self
            .open
            .iter()
            .filter(|o| o.kind == AttentionKind::Wait && owner(&self.running, &o.reference))
            .map(|o| o.reference.clone())
            .collect();
        let mut out: Vec<Change> = refs
            .iter()
            .filter_map(|r| self.clear(AttentionKind::Wait, r))
            .collect();
        out.extend(self.recompute(now));
        out
    }

    /// 「等你」保底判斷的門檻（毫秒，0 表示關閉）。
    pub fn set_wait_guess_after(&mut self, ms: u64) {
        self.guess_after = ms;
    }

    /// main 的 transcript 出現 assistant 文字（#706）：之後 N 秒沒有任何事件就推測在等使用者。
    /// `message_id` 當推測項目的 ref。
    pub fn assistant_text(&mut self, message_id: &str, now: u64) -> Vec<Change> {
        let out = self.activity(now);
        self.guess_from = Some((format!("{GUESS_REF_PREFIX}{message_id}"), now));
        out
    }

    /// session 有任何事件（hook 或 transcript 的訊息行；Notification 除外）：取消推測、撤回已發的推測。
    pub fn activity(&mut self, now: u64) -> Vec<Change> {
        self.guess_from = None;
        let refs: Vec<String> = self
            .open
            .iter()
            .filter(|o| o.kind == AttentionKind::Wait && o.reference.starts_with(GUESS_REF_PREFIX))
            .map(|o| o.reference.clone())
            .collect();
        let mut out: Vec<Change> = refs
            .iter()
            .filter_map(|r| self.clear(AttentionKind::Wait, r))
            .collect();
        if !out.is_empty() {
            out.extend(self.recompute(now));
        }
        out
    }

    pub fn prompt(&mut self, prompt_id: Option<String>, now: u64) -> Vec<Change> {
        let mut out = self.clear_waits();
        let done: Vec<String> = self
            .open
            .iter()
            .filter(|o| o.kind == AttentionKind::Done)
            .map(|o| o.reference.clone())
            .collect();
        for r in done {
            out.extend(self.clear(AttentionKind::Done, &r));
        }
        self.unseed(SessionState::Done);
        self.stop_at = None;
        self.stopped = false;
        self.idle = false;
        self.prompt_ref = prompt_id.or_else(|| Some(format!("prompt-{now}")));
        out.extend(self.recompute(now));
        out
    }

    /// main 開始新的工具（K-4）：Stop hook 回 block 時 Claude Code 會接著工作、不經 UserPromptSubmit，
    /// 所以這輪還沒完成：靜默期取消、已發的 done 撤回。之後的 Stop（帶 `stop_hook_active`）照常計時。
    pub fn main_tool_started(&mut self, now: u64) -> Vec<Change> {
        let done: Vec<String> = self
            .open
            .iter()
            .filter(|o| o.kind == AttentionKind::Done)
            .map(|o| o.reference.clone())
            .collect();
        if !self.stopped && done.is_empty() && self.seeded != Some(SessionState::Done) {
            return Vec::new();
        }
        self.unseed(SessionState::Done);
        self.stop_at = None;
        self.stopped = false;
        let mut out: Vec<Change> = done
            .iter()
            .filter_map(|r| self.clear(AttentionKind::Done, r))
            .collect();
        out.extend(self.recompute(now));
        out
    }

    /// main agent 的 Stop（subagent、背景 agent 不會呼叫）。
    pub fn stop(&mut self, now: u64) -> Vec<Change> {
        let mut out = self.clear_waits();
        self.stop_at = Some(now);
        self.stopped = true;
        out.extend(self.recompute(now));
        out
    }

    /// 使用者按 Esc（transcript 的 `[Request interrupted by user…]`）：沒有 Stop，進 idle、不進佇列。
    pub fn interrupted(&mut self, now: u64) -> Vec<Change> {
        let mut out = self.clear_waits();
        self.stop_at = None;
        self.idle = true;
        out.extend(self.recompute(now));
        out
    }

    /// SessionStart：resume／compact／clear 接回同一個 session 時，結束、中斷與 Stop 的狀態都不再成立。
    /// 之前還在跑的工具已經跟著舊的程序結束、不會再有 Post：它們的 danger／wait 一起解除，
    /// 也不再算「有工具在跑」（K-2）。
    pub fn started(&mut self, now: u64) -> Vec<Change> {
        self.ended = false;
        self.idle = false;
        self.stop_at = None;
        self.stopped = false;
        self.pending_wait.clear();
        let mut out = Vec::new();
        for r in std::mem::take(&mut self.running) {
            for kind in [AttentionKind::Wait, AttentionKind::Danger] {
                out.extend(self.clear(kind, &r.id));
            }
        }
        out.extend(self.recompute(now));
        out
    }

    /// 種回還開著的注意項目（不發 Event：live 與 store 已經有這些列與狀態）。
    pub fn seed_open(&mut self, item: OpenItem) {
        if item.kind == AttentionKind::Fail {
            self.fail_streak.insert(item.agent.clone(), FAIL_STREAK);
            self.fail_open
                .insert(item.agent.clone(), item.reference.clone());
        }
        if !self.is_open(item.kind, &item.reference) {
            self.open.push(Open {
                kind: item.kind,
                reference: item.reference,
                raised_at: item.raised_at,
            });
        }
        let _ = self.recompute(item.raised_at);
    }

    /// 種回 session 是否已結束、是否中斷（D3：重啟或移出記憶體後回來）。不發 Event；之後第一次重算
    /// 一定送 `session.status`，因為 live 與 store 的狀態不一定和種回的相同（已確認的不種回，K-6）。
    pub fn seed_state(&mut self, ended: bool, idle: bool) {
        self.ended = ended;
        self.idle = idle;
        let _ = self.recompute(0);
        self.report_next = true;
    }

    /// 種回 store 的狀態（只有 done、fail 有意義，見 `seeded`）。在 [`Tracker::seed_state`] 之前呼叫。
    pub fn seed_status(&mut self, status: SessionState) {
        if matches!(status, SessionState::Done | SessionState::Fail) {
            self.seeded = Some(status);
        }
    }

    /// 移出記憶體時交給下一次的 [`Tracker::seed_status`]。
    pub fn seeded_status(&self) -> Option<SessionState> {
        self.seeded
    }

    /// 沒重啟時會解除那一項的事件到了：done 是新的 prompt 或 main 開始新的工具（K-4），fail 是 Bash 成功或 agent 結束。
    fn unseed(&mut self, which: SessionState) {
        if self.seeded == Some(which) {
            self.seeded = None;
        }
    }

    /// 是否中斷中（使用者按 Esc 之後、下一個 prompt 之前）。
    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// 還開著的注意項目，交給下一次 [`Tracker::seed_open`]。
    pub fn open_items(&self) -> Vec<OpenItem> {
        self.open
            .iter()
            .map(|o| OpenItem {
                kind: o.kind,
                reference: o.reference.clone(),
                agent: self
                    .fail_open
                    .iter()
                    .find(|(_, r)| o.kind == AttentionKind::Fail && **r == o.reference)
                    .map_or_else(|| "main".to_string(), |(a, _)| a.clone()),
                raised_at: o.raised_at,
            })
            .collect()
    }

    pub fn session_end(&mut self, now: u64) -> Vec<Change> {
        let mut out = self.clear_waits();
        self.stop_at = None;
        self.ended = true;
        out.extend(self.recompute(now));
        out
    }

    /// 一筆 usage 的費用（#675）：最近 24 小時（滾動）累計超過基準 ×3 時加 CostSpike；24 小時內最多一次。
    pub fn cost(&mut self, at: u64, usd: f64, baseline: Option<f64>) {
        self.seed_cost(at, usd);
        let Some(base) = baseline else { return };
        let total = self.recent_total;
        if total > base * COST_SPIKE_FACTOR && self.last_spike.is_none_or(|t| t + DAY_MS <= at) {
            self.last_spike = Some(at);
            self.signal(Signal::CostSpike, at);
        }
    }

    /// 重啟時從 store 補回最近 24 小時的花費：只進滾動區間，不觸發。
    pub fn seed_cost(&mut self, at: u64, usd: f64) {
        self.recent_cost.push_back((at, usd));
        self.recent_total += usd;
        let from = at.saturating_sub(DAY_MS);
        while self.recent_cost.front().is_some_and(|(t, _)| *t < from) {
            if let Some((_, c)) = self.recent_cost.pop_front() {
                self.recent_total -= c;
            }
        }
        if self.recent_cost.is_empty() {
            self.recent_total = 0.0; // 不讓浮點誤差一直累積
        }
    }

    pub fn signal(&mut self, sig: Signal, at: u64) {
        self.signals.push((sig, at));
    }

    /// 還留著的動態訊號數（測試確認衰減到可忽略的會被清掉）
    pub fn signal_count(&self) -> usize {
        self.signals.len()
    }

    pub fn tick(&mut self, now: u64) -> Vec<Change> {
        let mut out = Vec::new();
        // 超過 10 個半衰期的動態訊號（剩不到 0.1%）清掉，scores() 不必一直走過它們（#686）
        let gone_after = (self.cfg.half_life_min * 10.0 * 60_000.0) as u64;
        self.signals
            .retain(|(_, at)| now.saturating_sub(*at) < gone_after);
        let due: Vec<String> = self
            .pending_wait
            .iter()
            .filter(|(_, at)| at + WAIT_AFTER_MS <= now)
            .map(|(id, _)| id.clone())
            .collect();
        self.pending_wait.retain(|(_, at)| at + WAIT_AFTER_MS > now);
        for id in due {
            out.push(self.raise(AttentionKind::Wait, &id, Some("等你回答".into()), now));
        }
        if self.stop_at.is_some_and(|at| at + DONE_AFTER_MS <= now) {
            self.stop_at = None;
            let r = self
                .prompt_ref
                .clone()
                .unwrap_or_else(|| format!("stop-{now}"));
            if !self.is_open(AttentionKind::Done, &r) {
                out.push(self.raise(AttentionKind::Done, &r, Some("完成待確認".into()), now));
            }
        }
        if let Some(c) = self.guess_due(now) {
            out.push(c);
        }
        if !out.is_empty() {
            out.extend(self.recompute(now));
        }
        out
    }

    /// 推測的「等你」到期（#706）。已經結束、中斷、這輪收過 Stop（交給 done）、已有 wait、
    /// 或還有工具在跑時不推測：保底判斷寧可漏報，也不要在正常的 session 上誤報。
    fn guess_due(&mut self, now: u64) -> Option<Change> {
        let (r, at) = self.guess_from.as_ref()?;
        if self.guess_after == 0 || at + self.guess_after > now {
            return None;
        }
        let r = r.clone();
        self.guess_from = None;
        let busy = self.ended
            || self.idle
            || self.stopped
            || !self.running.is_empty()
            || !self.pending_wait.is_empty()
            || self
                .open
                .iter()
                .any(|o| matches!(o.kind, AttentionKind::Wait | AttentionKind::Done));
        (!busy).then(|| {
            self.raise(
                AttentionKind::Wait,
                &r,
                Some("等你回答（推測）".into()),
                now,
            )
        })
    }

    /// 目前分數（彙總列定期重算用）。
    pub fn score(&self, now: u64) -> f64 {
        let open: Vec<(AttentionKind, u64)> =
            self.open.iter().map(|o| (o.kind, o.raised_at)).collect();
        score(&self.cfg, &open, &self.signals, now)
    }

    fn is_open(&self, kind: AttentionKind, r: &str) -> bool {
        self.open.iter().any(|o| o.kind == kind && o.reference == r)
    }

    fn raise(&mut self, kind: AttentionKind, r: &str, reason: Option<String>, now: u64) -> Change {
        self.open.push(Open {
            kind,
            reference: r.to_string(),
            raised_at: now,
        });
        Change::Raised(Attention {
            kind,
            reference: r.to_string(),
            reason,
        })
    }

    fn clear(&mut self, kind: AttentionKind, r: &str) -> Option<Change> {
        let i = self
            .open
            .iter()
            .position(|o| o.kind == kind && o.reference == r)?;
        self.open.remove(i);
        Some(Change::Cleared(Attention {
            kind,
            reference: r.to_string(),
            reason: None,
        }))
    }

    fn clear_waits(&mut self) -> Vec<Change> {
        self.pending_wait.clear();
        let refs: Vec<String> = self
            .open
            .iter()
            .filter(|o| o.kind == AttentionKind::Wait)
            .map(|o| o.reference.clone())
            .collect();
        refs.iter()
            .filter_map(|r| self.clear(AttentionKind::Wait, r))
            .collect()
    }

    /// status = 未解除項目中優先級最高者；都沒有才是 run／idle；ended 優先。只在改變時回報。
    fn recompute(&mut self, now: u64) -> Option<Change> {
        let rank = |k: AttentionKind| match k {
            AttentionKind::Danger => 4,
            AttentionKind::Fail => 3,
            AttentionKind::Wait => 2,
            AttentionKind::Done => 1,
            AttentionKind::Conflict => 0,
        };
        let top = self
            .open
            .iter()
            .map(|o| o.kind)
            .filter(|k| rank(*k) > 0)
            .max_by_key(|k| rank(*k));
        let next = if self.ended {
            SessionState::Ended
        } else {
            match top {
                Some(AttentionKind::Danger) => SessionState::Danger,
                Some(AttentionKind::Fail) => SessionState::Fail,
                Some(AttentionKind::Wait) => SessionState::Wait,
                Some(AttentionKind::Done) => SessionState::Done,
                _ => match self.seeded {
                    Some(s) => s,
                    None if self.idle => SessionState::Idle,
                    None => SessionState::Run,
                },
            }
        };
        if next == self.state && !self.report_next {
            return None;
        }
        self.report_next = false;
        self.state = next;
        Some(Change::Status(SessionStatus {
            status: next,
            since: now,
            score: self.score(now),
        }))
    }
}

// ── 分數

/// 動態分的訊號（已發生的事件，隨時間衰減）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Conflict,
    SubagentFail,
    CostSpike,
    BigEdit,
    NewFile,
}

/// 待處理分：基礎分、每分鐘加、上限。
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    pub base: f64,
    pub per_min: f64,
    pub cap: f64,
}

/// PLAN「session 分數」的初值；`config.toml` 的 `[score]` 可覆寫。
#[derive(Clone, Debug)]
pub struct ScoreConfig {
    pub half_life_min: f64,
    pub danger: Pending,
    pub fail: Pending,
    pub wait: Pending,
    pub done: Pending,
    pub conflict: f64,
    pub subagent_fail: f64,
    pub cost_spike: f64,
    pub big_edit: f64,
    pub new_file: f64,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        let p = |base, per_min, cap| Pending { base, per_min, cap };
        ScoreConfig {
            half_life_min: 10.0,
            danger: p(100.0, 0.0, 100.0),
            fail: p(60.0, 2.0, 80.0),
            wait: p(50.0, 5.0, 100.0),
            done: p(20.0, 1.0, 40.0),
            conflict: 30.0,
            subagent_fail: 20.0,
            cost_spike: 15.0,
            big_edit: 10.0,
            new_file: 3.0,
        }
    }
}

/// 分數 = 待處理分（不衰減、隨等待時間上升到上限）＋ 動態分（權重 × 0.5^(分鐘 / 半衰期)）。
pub fn score(
    cfg: &ScoreConfig,
    open: &[(AttentionKind, u64)],
    signals: &[(Signal, u64)],
    now: u64,
) -> f64 {
    let minutes = |at: u64| now.saturating_sub(at) as f64 / 60_000.0;
    let pending: f64 = open
        .iter()
        .filter_map(|(kind, at)| {
            let p = match kind {
                AttentionKind::Danger => cfg.danger,
                AttentionKind::Fail => cfg.fail,
                AttentionKind::Wait => cfg.wait,
                AttentionKind::Done => cfg.done,
                AttentionKind::Conflict => return None,
            };
            Some((p.base + p.per_min * minutes(*at)).min(p.cap))
        })
        .fold(0.0, |a, b| a + b);
    let dynamic: f64 = signals
        .iter()
        .map(|(sig, at)| {
            let w = match sig {
                Signal::Conflict => cfg.conflict,
                Signal::SubagentFail => cfg.subagent_fail,
                Signal::CostSpike => cfg.cost_spike,
                Signal::BigEdit => cfg.big_edit,
                Signal::NewFile => cfg.new_file,
            };
            w * 0.5_f64.powf(minutes(*at) / cfg.half_life_min)
        })
        .fold(0.0, |a, b| a + b);
    pending + dynamic
}
