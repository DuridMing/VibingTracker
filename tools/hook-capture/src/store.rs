use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// hook 是從哪條路進來的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Http,
    Command,
}

impl Via {
    fn as_str(self) -> &'static str {
        match self {
            Via::Http => "http",
            Via::Command => "command",
        }
    }
}

/// 把一筆 hook payload 附上到達時間，寫進 `<out>/<session_id>/hooks.jsonl`。
/// 回傳 payload 裡的 session_id（沒有就是 `unknown`）。
pub fn record_hook(out: &Path, via: Via, body: &[u8]) -> std::io::Result<String> {
    record_hook_at(out, via, body, crate::now_ms())
}

pub fn record_hook_at(
    out: &Path,
    via: Via,
    body: &[u8],
    ts_seen_ms: u128,
) -> std::io::Result<String> {
    record_hook_with(out, via, body, ts_seen_ms, serde_json::Map::new())
}

/// 同 `record_hook_at`，另外把 `extra` 的欄位併進這一筆（例如 Edit 探針結果）。
pub fn record_hook_with(
    out: &Path,
    via: Via,
    body: &[u8],
    ts_seen_ms: u128,
    extra: serde_json::Map<String, Value>,
) -> std::io::Result<String> {
    let payload = parse_payload(body);
    let session_id = payload
        .as_ref()
        .and_then(|p| p["session_id"].as_str())
        .map(sanitize)
        .unwrap_or_else(|| "unknown".to_string());
    let event = payload
        .as_ref()
        .and_then(|p| p["hook_event_name"].as_str())
        .unwrap_or("unknown")
        .to_string();
    let row = match payload {
        Some(p) => {
            json!({ "ts_seen_ms": ts_seen_ms, "via": via.as_str(), "event": event, "payload": p })
        }
        None => json!({
            "ts_seen_ms": ts_seen_ms, "via": via.as_str(), "event": event,
            "raw": String::from_utf8_lossy(body),
        }),
    };
    let mut row = row;
    if let Some(obj) = row.as_object_mut() {
        obj.extend(extra);
    }
    append_json(&session_dir(out, &session_id).join("hooks.jsonl"), &row)?;
    Ok(session_id)
}

/// 一行 JSON 一次 `write_all`（O_APPEND），讓 serve 與多個 `cmd` 程序同時寫也不會交錯。
pub fn append_json(path: &Path, row: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_vec(row)?;
    line.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)
}

pub fn session_dir(out: &Path, session_id: &str) -> PathBuf {
    out.join(sanitize(session_id))
}

/// 只留下 session id 可能用到的字元，避免 payload 決定寫入路徑。
pub fn sanitize(id: &str) -> String {
    if id.is_empty() {
        return "unknown".to_string();
    }
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn parse_payload(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}
