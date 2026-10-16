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

struct Running {
    id: String,
    tool: String,
    input: Value,
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
    prompt_ref: Option<String>,
    fail_streak: HashMap<String, u32>,
    /// agent → 它目前未解除的 fail 的 ref（ref 照契約是 toolUseId）
    fail_open: HashMap<String, String>,
    idle: bool,
    ended: bool,
    signals: Vec<(Signal, u64)>,
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
            prompt_ref: None,
            fail_streak: HashMap::new(),
            fail_open: HashMap::new(),
            idle: false,
            ended: false,
            signals: Vec::new(),
        }
    }
}

impl Tracker {
    pub fn tool_started(&mut self, id: &str, tool: &str, input: &Value) {
        self.running.push(Running {
            id: id.to_string(),
            tool: tool.to_string(),
            input: input.clone(),
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

    /// 一批工具結束：被自動駁回的權限詢問只有這個訊號（實測 12 ms）。
    pub fn batch_done(&mut self, now: u64) -> Vec<Change> {
        let mut out = self.clear_waits();
        out.extend(self.recompute(now));
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
        self.stop_at = None;
        self.idle = false;
        self.prompt_ref = prompt_id.or_else(|| Some(format!("prompt-{now}")));
        out.extend(self.recompute(now));
        out
    }

    /// main agent 的 Stop（subagent、背景 agent 不會呼叫）。
    pub fn stop(&mut self, now: u64) -> Vec<Change> {
        let mut out = self.clear_waits();
        self.stop_at = Some(now);
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

    pub fn session_end(&mut self, now: u64) -> Vec<Change> {
        let mut out = self.clear_waits();
        self.stop_at = None;
        self.ended = true;
        out.extend(self.recompute(now));
        out
    }

    pub fn signal(&mut self, sig: Signal, at: u64) {
        self.signals.push((sig, at));
    }

    pub fn tick(&mut self, now: u64) -> Vec<Change> {
        let mut out = Vec::new();
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
        if !out.is_empty() {
            out.extend(self.recompute(now));
        }
        out
    }

    /// 目前分數（彙總列定期重算用）。
    pub fn score(&self, now: u64) -> f64 {
        let open: Vec<(AttentionKind, u64)> =
            self.open.iter().map(|o| (o.kind, o.raised_at)).collect();
        score(&self.cfg, &open, &self.signals, now)
    }

    pub fn state(&self) -> SessionState {
        self.state
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
                _ if self.idle => SessionState::Idle,
                _ => SessionState::Run,
            }
        };
        if next == self.state {
            return None;
        }
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
