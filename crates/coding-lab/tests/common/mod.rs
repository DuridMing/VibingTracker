//! 回放測試共用的樣本載入（#688）：samples/ 不進 git，沒有時各測試自己略過。

#![allow(dead_code)] // 每個測試檔只用到其中幾個

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `samples/sessions/` 底下的每個樣本目錄（排序後）；沒有 samples 時是空的。
pub fn sample_sessions() -> Vec<PathBuf> {
    sessions_in(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/sessions"))
}

/// `root` 底下的每個樣本目錄（排序後）。
pub fn sessions_in(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// JSONL 的每一行（讀不到檔或解析不了的行略過）。
pub fn read_jsonl(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// hook-capture 的 `*-arrivals.jsonl`：行號 → 最早的到達時間（resume 重開 tail 時同一行會再記一次）。
pub fn arrivals(p: &Path) -> HashMap<u64, u64> {
    let mut out: HashMap<u64, u64> = HashMap::new();
    for a in read_jsonl(p) {
        if let (Some(no), Some(ts)) = (a["line_no"].as_u64(), a["ts_seen_ms"].as_u64()) {
            let t = out.entry(no).or_insert(ts);
            *t = (*t).min(ts);
        }
    }
    out
}

pub struct Merged {
    pub session: String,
    pub events: Vec<coding_lab::event::Event>,
    /// (transcript 檔名, 行號) → message.id（只有 assistant 行）
    pub message_of_line: HashMap<(String, u64), String>,
}

/// 把一個樣本目錄餵過 merge（照到達時間；同一毫秒 hook 先）。
/// 沒有到達時間（`transcript-arrivals.jsonl`）的樣本回 None。
pub fn merged_events(dir: &Path) -> Option<Merged> {
    use coding_lab::event::Danger;
    use coding_lab::hook::HookInput;
    use coding_lab::merge::{DangerRules, Input, Merger};

    struct NoDanger;
    impl DangerRules for NoDanger {
        fn check(&self, _tool: &str, _input: &Value) -> Option<Danger> {
            None
        }
    }

    let hooks = read_jsonl(&dir.join("hooks.jsonl"));
    if read_jsonl(&dir.join("transcript-arrivals.jsonl")).is_empty() {
        return None;
    }
    let session = hooks.first()?["payload"]["session_id"]
        .as_str()?
        .to_string();
    let mut files = vec![(
        "main".to_string(),
        dir.join("transcript.jsonl"),
        dir.join("transcript-arrivals.jsonl"),
    )];
    for entry in std::fs::read_dir(dir.join("subagents"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry
            .file_name()
            .to_string_lossy()
            .trim_end_matches(".jsonl")
            .to_string();
        let Some(id) = name.strip_prefix("agent-") else {
            continue;
        };
        let arr = dir.join(format!("subagent-{id}-arrivals.jsonl"));
        if arr.exists() {
            files.push((name.clone(), entry.path(), arr));
        }
    }
    let mut inputs: Vec<(u64, u8, Input)> = Vec::new();
    for h in hooks.iter().filter(|h| h["via"] == "http") {
        let ts = h["ts_seen_ms"].as_u64()?;
        let hook = HookInput::from_json(h["payload"].to_string().as_bytes()).ok()?;
        inputs.push((ts, 0, Input::Hook { hook, ts_seen: ts }));
    }
    let mut message_of_line = HashMap::new();
    for (file, transcript, arrival_file) in files {
        let arrival_ts = arrivals(&arrival_file);
        for (i, value) in read_jsonl(&transcript).into_iter().enumerate() {
            let no = i as u64 + 1;
            let Some(&ts) = arrival_ts.get(&no) else {
                continue;
            };
            if let Some(id) = value["message"]["id"].as_str() {
                message_of_line.insert((file.clone(), no), id.to_string());
            }
            let session = session.clone();
            let file = file.clone();
            inputs.push((
                ts,
                1,
                Input::Line {
                    session,
                    file,
                    no,
                    value,
                    ts_seen: ts,
                },
            ));
        }
    }
    inputs.sort_by_key(|(ts, kind, _)| (*ts, *kind));
    let mut m = Merger::new(Box::new(NoDanger));
    let mut events = Vec::new();
    for (ts, _, input) in inputs {
        events.extend(m.tick(ts));
        events.extend(m.handle(input));
    }
    Some(Merged {
        session,
        events,
        message_of_line,
    })
}
