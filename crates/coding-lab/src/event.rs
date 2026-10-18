//! Event：collector 產生的每一筆事實（contracts.md §4）。
//!
//! 儲存與傳輸用 [`Event`]：外層欄位型別化，`payload` 保留原始 JSON，讀進來再寫出去不會遺失
//! 任何欄位（包括新版才有的 type 與欄位）。要用內容時呼叫 [`Event::payload_typed`] 轉成
//! [`Payload`]；兩者的欄位名稱必須與 contracts.md 一致（tests/contract.rs 檢查）。

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 目前的契約版本（contracts.md §1）。
pub const CONTRACT_VERSION: u32 = 1;

/// 指令型工具（D12）：`tool_input.command` 是一條 shell 指令的工具。敏感判斷、危險規則、git 操作、
/// 連續失敗、KPI 的指令數都認這個集合，不寫死 `"Bash"`。cmd 不是工具，只在指令字串裡的 `cmd /c …` 比對。
/// 危險規則的 `tools` 寫 [`COMMAND_TOOLS_ALIAS`] 等於列出這裡的每一個。
pub const COMMAND_TOOLS: [&str; 2] = ["Bash", "PowerShell"];

/// 危險規則 `tools` 裡代表「所有指令型工具」的寫法。
pub const COMMAND_TOOLS_ALIAS: &str = "@command";

pub fn is_command_tool(tool: &str) -> bool {
    COMMAND_TOOLS.contains(&tool)
}

#[derive(Debug)]
pub enum Error {
    Json(serde_json::Error),
    /// Event 的 `v` 比這個版本新：語意可能變了，不猜。
    UnsupportedVersion(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Json(e) => write!(f, "JSON 格式錯誤：{e}"),
            Error::UnsupportedVersion(v) => {
                write!(f, "Event 版本 {v} 比支援的 {CONTRACT_VERSION} 新")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

/// transcript 裡的位置：`file` 是 `"main"` 或 `"agent-<agent_id>"`，`no` 從 1 起算。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRef {
    pub file: String,
    pub no: u64,
}

/// 一筆 Event（contracts.md §4 的外層欄位）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub session: String,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub agent: String,
    // 以下四個一律輸出，沒有值就是 null
    pub line: Option<LineRef>,
    pub pos: Option<u64>,
    pub ts_model: Option<String>,
    pub ts_exec: Option<u64>,
    pub ts_seen: u64,
    pub host: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Map<String, Value>>,
    /// 新版加的外層欄位，原樣保留。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Event {
    pub fn from_json_line(line: &str) -> Result<Event, Error> {
        let ev: Event = serde_json::from_str(line)?;
        if ev.v > CONTRACT_VERSION {
            return Err(Error::UnsupportedVersion(ev.v));
        }
        Ok(ev)
    }

    pub fn to_json_line(&self) -> String {
        // Event 只含字串鍵的 JSON 值，序列化不會失敗
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn type_name(&self) -> &str {
        &self.kind
    }

    pub fn pos(&self) -> Option<u64> {
        self.pos
    }

    /// 依 `type` 把 payload 轉成型別化的結構；不認得的 type 回 `Ok(None)`。
    pub fn payload_typed(&self) -> Result<Option<Payload>, Error> {
        Ok(Payload::parse(&self.kind, &self.payload)?)
    }
}

macro_rules! payloads {
    ($($name:literal => $variant:ident($ty:ident)),* $(,)?) => {
        /// 契約 v1 定義的全部 Event type。
        pub const EVENT_TYPES: &[&str] = &[$($name),*];

        /// 型別化的 payload，一個 Event type 對應一個 variant。
        #[derive(Clone, Debug, PartialEq, Serialize)]
        #[serde(untagged)]
        pub enum Payload {
            $($variant($ty)),*
        }

        impl Payload {
            pub fn parse(kind: &str, v: &Value) -> Result<Option<Payload>, serde_json::Error> {
                Ok(Some(match kind {
                    // `&Value` 本身就能反序列化，不必先 clone 整棵 payload（E-M6）
                    $($name => Payload::$variant(<$ty as serde::Deserialize>::deserialize(v)?),)*
                    _ => return Ok(None),
                }))
            }

            pub fn type_name(&self) -> &'static str {
                match self {
                    $(Payload::$variant(_) => $name),*
                }
            }
        }
    };
}

payloads! {
    "session.started" => SessionStarted(SessionStarted),
    "session.ended" => SessionEnded(SessionEnded),
    "session.status" => SessionStatus(SessionStatus),
    "prompt.submitted" => PromptSubmitted(PromptSubmitted),
    "turn.stopped" => TurnStopped(TurnStopped),
    "assistant.thinking" => AssistantThinking(AssistantThinking),
    "assistant.text" => AssistantText(AssistantText),
    "assistant.display" => AssistantDisplay(AssistantDisplay),
    "tool.called" => ToolCalled(ToolCalled),
    "tool.placed" => ToolPlaced(ToolPlaced),
    "tool.updated" => ToolUpdated(ToolUpdated),
    "tool.result" => ToolResult(ToolResult),
    "file.read" => FileRead(FileRead),
    "file.edit" => FileEdit(FileEdit),
    "file.write" => FileWrite(FileWrite),
    "git.op" => GitOp(GitOp),
    "agent.spawned" => AgentSpawned(AgentSpawned),
    "agent.finished" => AgentFinished(AgentFinished),
    "model.changed" => ModelChanged(ModelChanged),
    "usage" => Usage(Usage),
    "compact.started" => CompactStarted(CompactStarted),
    "compact.finished" => CompactFinished(CompactFinished),
    "attention.raised" => AttentionRaised(Attention),
    "attention.cleared" => AttentionCleared(Attention),
    "transcript.degraded" => TranscriptDegraded(TranscriptHealth),
    "transcript.recovered" => TranscriptRecovered(TranscriptHealth),
}

// ── 列舉值（一律小寫字串）

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Run,
    /// 使用者按 Esc 中斷（沒有 hook、沒有 Stop），不進佇列（#645）
    Idle,
    Wait,
    Done,
    Fail,
    Danger,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Pending,
    Running,
    Done,
    Failed,
}

/// 卡片在哪裡（方案 C）：staged 只在 Agents／NOW，stream 依 transcript 進串流，fallback 由 hook 逾時放入。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    Staged,
    Stream,
    Fallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DangerLevel {
    Warn,
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HunkSource {
    Pre,
    Git,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttentionKind {
    Danger,
    Fail,
    Wait,
    Done,
    Conflict,
}

// ── payload

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStarted {
    pub cwd: String,
    pub source: String,
    pub model: Option<String>,
    pub transcript_path: Option<String>,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEnded {
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub status: SessionState,
    pub since: u64,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSubmitted {
    pub text: String,
    pub prompt_id: Option<String>,
    /// `"task"`：背景工作完成叫醒 Claude 的 `<task-notification>`，不是使用者輸入；使用者輸入時不輸出
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStopped {
    pub last_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantThinking {
    pub text: String,
    /// 沒有 `signature` 的 thinking block 視為摘要。
    pub summarized: bool,
    pub message_id: String,
    pub model: Option<String>,
    /// 降級卡片之後才補上的行（PLAN 方案 C 規則 5）；false 時不輸出。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delayed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantText {
    pub text: String,
    pub message_id: String,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delayed: bool,
}

/// MessageDisplay hook 的顯示片段；`display_id` 與 transcript 的 `message.id` 不同套。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantDisplay {
    pub display_id: String,
    pub turn_id: Option<String>,
    pub index: u32,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub delta: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Danger {
    pub rule: String,
    pub level: DangerLevel,
    pub why: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCalled {
    pub tool_use_id: String,
    pub tool: String,
    pub input: Value,
    /// input 裡有字串被截到 8 KB（#654）；false 時不輸出
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub input_truncated: bool,
    pub status: ToolStatus,
    pub placement: Placement,
    pub danger: Option<Danger>,
    /// 命中敏感規則（同 `tool.result`）：對遠端連線剝除對應欄位（C8、#761）；false 時不輸出
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPlaced {
    pub tool_use_id: String,
    pub placement: Placement,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdated {
    pub tool_use_id: String,
    pub status: ToolStatus,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub interrupted: Option<bool>,
    /// transcript 先建卡、PreToolUse 後到時才判斷出的危險；沒有時不輸出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger: Option<Danger>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub tool_use_id: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub truncated: bool,
    /// 命中敏感規則（讀 `.env` 等）：對遠端連線剝除（#654）；false 時不輸出
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRead {
    pub tool_use_id: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    pub start: u32,
    pub old_lines: u32,
    pub new_lines: u32,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEdit {
    pub tool_use_id: String,
    pub path: String,
    pub hunk: Option<Hunk>,
    pub hunk_source: HunkSource,
    /// hunk.lines 合計超過 8 KB，放不下的行丟掉、最後一行截在字元邊界（C7、#744）；false 時不輸出
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// 命中敏感規則（同 `tool.result`）：對遠端連線剝除對應欄位（C8、#761）；false 時不輸出
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
    /// `replace_all` 的 Edit：`hunk` 為 null，畫面標「多處取代」，不畫第一處（D7、#745）；false 時不輸出
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace_all: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileWrite {
    pub tool_use_id: String,
    pub path: String,
    pub is_new: bool,
    pub content: String,
    pub truncated: bool,
    /// 全文行數（`content` 只留前 200 行）；舊的 Event 沒有（#682）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
    /// 命中敏感規則（同 `tool.result`）：對遠端連線剝除對應欄位（C8、#761）；false 時不輸出
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitOp {
    pub tool_use_id: String,
    /// worktree | branch | commit | push | reset | stash；用字串以免新的操作讀不進來。
    pub op: String,
    pub branch: Option<String>,
    pub hash: Option<String>,
    pub message: Option<String>,
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpawned {
    pub agent_id: String,
    pub agent_type: Option<String>,
    pub name: Option<String>,
    pub task: Option<String>,
    pub parent_tool_use_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentFinished {
    pub agent_id: String,
    pub last_message: Option<String>,
    pub transcript_path: Option<String>,
    /// 由 `<task-notification>` 結束時的 `<status>`（completed、killed…）；SubagentStop 結束時不輸出（#677）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChanged {
    pub from: Option<String>,
    pub to: String,
}

/// 每個 `message.id` 只算一次（同一則訊息拆成多行時每行的 usage 相同）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub message_id: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub thinking: u64,
    pub cache_read: u64,
    #[serde(rename = "cacheWrite1h")]
    pub cache_write_1h: u64,
    #[serde(rename = "cacheWrite5m")]
    pub cache_write_5m: u64,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactStarted {
    pub trigger: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactFinished {
    pub trigger: Option<String>,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attention {
    pub kind: AttentionKind,
    #[serde(rename = "ref")]
    pub reference: String,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptHealth {
    pub file: String,
    pub error: Option<String>,
}
