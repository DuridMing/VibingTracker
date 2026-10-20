use serde_json::{Value, json};

/// Claude Code 文件列出的全部 hook 事件（code.claude.com/docs/en/hooks，2026-10 版）。
/// 階段 0 全部都收，再決定 collector 要訂哪些。
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "Setup",
    "UserPromptSubmit",
    "UserPromptExpansion",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "Notification",
    "MessageDisplay",
    "SubagentStart",
    "SubagentStop",
    "TaskCreated",
    "TaskCompleted",
    "Stop",
    "StopFailure",
    "TeammateIdle",
    "InstructionsLoaded",
    "ConfigChange",
    "CwdChanged",
    "DirectoryAdded",
    "FileChanged",
    "WorktreeCreate",
    "WorktreeRemove",
    "PreCompact",
    "PostCompact",
    "PreModelSwitch",
    "PostModelSwitch",
    "Elicitation",
    "ElicitationResult",
    "SessionEnd",
];

/// 產生同時掛 http 與 command 兩種 hook 的設定（不加 matcher＝全部符合）。
pub fn settings_json(port: u16, bin: &str) -> Value {
    let hook = json!([{
        "hooks": [
            { "type": "http", "url": format!("http://127.0.0.1:{port}/hook"), "timeout": 5 },
            { "type": "command", "command": format!("{bin} cmd"), "timeout": 5 },
        ]
    }]);
    let hooks: serde_json::Map<String, Value> = EVENTS
        .iter()
        .map(|ev| ((*ev).to_string(), hook.clone()))
        .collect();
    json!({ "hooks": hooks })
}
