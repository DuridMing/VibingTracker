//! 未決 3：PreToolUse 時讀 Edit 的目標檔，會不會讀到已經改過的內容？
//! 在 hook 到達的不同時間點各讀一次，記錄 old_string／new_string 出現幾次。

use serde_json::{Value, json};

/// `payload` 是 Edit 的 Pre／PostToolUse 時回傳探測結果，其他事件回 `None`。
pub fn edit_probe(payload: &Value) -> Option<Value> {
    let event = payload["hook_event_name"].as_str()?;
    if payload["tool_name"].as_str()? != "Edit" || !matches!(event, "PreToolUse" | "PostToolUse") {
        return None;
    }
    let input = &payload["tool_input"];
    let path = input["file_path"].as_str()?;
    let old = input["old_string"].as_str().unwrap_or_default();
    let new = input["new_string"].as_str().unwrap_or_default();
    let ts_ms = crate::now_ms();
    let Ok(text) = std::fs::read_to_string(path) else {
        return Some(json!({ "ts_ms": ts_ms, "exists": false }));
    };
    let count = |s: &str| {
        if s.is_empty() {
            0
        } else {
            text.matches(s).count()
        }
    };
    let old_line = text
        .find(old)
        .filter(|_| !old.is_empty())
        .map(|i| text[..i].matches('\n').count() + 1);
    Some(json!({
        "ts_ms": ts_ms,
        "exists": true,
        "old_count": count(old),
        "new_count": count(new),
        "old_line": old_line,
    }))
}
