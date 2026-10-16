//! hook 輸入（contracts.md §2）。
//!
//! payload 依欄位拆成兩份：契約用到的（`fields`）與沒用到的（`raw`，原樣進 Event 的 `raw`）。
//! 任何 `hook_event_name` 都能解析，包括沒有訂閱的事件。

use std::fmt;

use serde_json::{Map, Value};

/// contracts.md §2 用到的欄位：共同欄位加上各事件的欄位。不在這裡的都進 `raw`。
pub const CONTRACT_FIELDS: &[&str] = &[
    // 共同
    "session_id",
    "hook_event_name",
    "transcript_path",
    "cwd",
    "prompt_id",
    "permission_mode",
    "effort",
    // SessionStart／SessionEnd
    "source",
    "model",
    "reason",
    // UserPromptSubmit
    "prompt",
    // 工具
    "tool_name",
    "tool_input",
    "tool_use_id",
    "tool_response",
    "duration_ms",
    "error",
    "is_interrupt",
    // subagent
    "agent_id",
    "agent_type",
    "agent_transcript_path",
    "last_assistant_message",
    // Notification
    "notification_type",
    "message",
    // compact
    "trigger",
    "compact_summary",
    // MessageDisplay
    "message_id",
    "turn_id",
    "index",
    "final",
    "delta",
];

#[derive(Debug)]
pub enum Error {
    Json(serde_json::Error),
    NotAnObject,
    /// 缺少 `session_id` 或 `hook_event_name`，無法分流。
    Missing(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Json(e) => write!(f, "hook JSON 格式錯誤：{e}"),
            Error::NotAnObject => write!(f, "hook payload 不是 JSON 物件"),
            Error::Missing(k) => write!(f, "hook payload 缺少 {k}"),
        }
    }
}

impl std::error::Error for Error {}

/// PreToolUse 時（回應 hook 之前）對 Edit／Write 目標檔的探測結果，見 `derive::attach_probe`。
#[derive(Clone, Debug, PartialEq)]
pub struct FileProbe {
    pub exists: bool,
    pub hunk: Option<crate::event::Hunk>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HookInput {
    event: String,
    session_id: String,
    fields: Map<String, Value>,
    raw: Map<String, Value>,
    probe: Option<FileProbe>,
}

impl HookInput {
    pub fn from_json(body: &[u8]) -> Result<HookInput, Error> {
        let v: Value = serde_json::from_slice(body).map_err(Error::Json)?;
        let Value::Object(all) = v else {
            return Err(Error::NotAnObject);
        };
        let (fields, raw): (Map<String, Value>, Map<String, Value>) = all
            .into_iter()
            .partition(|(k, _)| CONTRACT_FIELDS.contains(&k.as_str()));
        let text = |k: &'static str| {
            fields
                .get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or(Error::Missing(k))
        };
        Ok(HookInput {
            event: text("hook_event_name")?,
            session_id: text("session_id")?,
            fields,
            raw,
            probe: None,
        })
    }

    pub fn event_name(&self) -> &str {
        &self.event
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// PreToolUse、PostToolUse、PostToolUseFailure、PermissionDenied 才有；PermissionRequest 沒有。
    pub fn tool_use_id(&self) -> Option<&str> {
        self.str_field("tool_use_id")
    }

    /// 契約用到的字串欄位。
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    /// 契約用到的欄位（原始 JSON 值）。
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    pub fn set_probe(&mut self, probe: FileProbe) {
        self.probe = Some(probe);
    }

    pub fn probe(&self) -> Option<&FileProbe> {
        self.probe.as_ref()
    }

    /// payload 裡契約沒有用到的欄位。
    pub fn raw(&self) -> &Map<String, Value> {
        &self.raw
    }
}
