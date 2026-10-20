//! 回放測試共用的樣本載入（#688）。
//!
//! samples/（遮蔽後的真實樣本）不進 git。沒有它時，通用的回放與契約測試改用 git 裡的極小 fixture
//! （`tests/fixtures/samples/`：一個專門跑的計算器小專案 session，經 scripts/redact-samples.py 遮蔽，
//! 含 Read／Edit／Write、成功與失敗的 Bash、subagent、resume，#749 T-1／D9），乾淨 clone 與 CI 也會真的跑。
//! 指名特定 session 的測試（例如背景 agent、Esc）只有真的 samples/ 才有，用 [`full_samples`]：
//! 沒有時略過，但設了 `CODING_LAB_REQUIRE_SAMPLES=1` 就直接失敗，本機確認有真的跑到。

#![allow(dead_code)] // 每個測試檔只用到其中幾個

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// 樣本的根目錄：有 samples/ 就用它，否則用 git 裡的 fixture。
pub fn samples_root() -> PathBuf {
    let real = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples");
    if real.join("sessions").is_dir() {
        real
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/samples")
    }
}

/// 只有真的 samples/ 才有的樣本（指名特定 session 的測試用）。沒有時回 None，
/// 但設了 `CODING_LAB_REQUIRE_SAMPLES=1` 就直接失敗（不讓「略過」看起來像「通過」，#749 T-1）。
pub fn full_samples() -> Option<PathBuf> {
    let real = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples");
    if real.join("sessions").is_dir() {
        return Some(real);
    }
    if std::env::var("CODING_LAB_REQUIRE_SAMPLES").as_deref() == Ok("1") {
        panic!("CODING_LAB_REQUIRE_SAMPLES=1 但沒有 samples/（scripts/redact-samples.py 產生）");
    }
    None
}

/// 樣本的每個 session 目錄（排序後）：samples/ 或 fixture。
pub fn sample_sessions() -> Vec<PathBuf> {
    sessions_in(&samples_root().join("sessions"))
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

/// 不判斷危險的規則（回放測試只看管線）。
pub struct NoDanger;
impl coding_lab::merge::DangerRules for NoDanger {
    fn check(&self, _tool: &str, _input: &Value) -> Option<coding_lab::event::Danger> {
        None
    }
}

pub struct SampleInputs {
    pub session: String,
    /// (到達時間, 輸入)：照到達時間排好，同一毫秒 hook 先
    pub inputs: Vec<(u64, coding_lab::merge::Input)>,
    /// (transcript 檔名, 行號) → message.id（只有 assistant 行）
    pub message_of_line: HashMap<(String, u64), String>,
}

/// 一個樣本目錄的 hook 與 transcript 行，照到達時間排成 merge 的輸入。
/// 沒有到達時間（`transcript-arrivals.jsonl`）的樣本回 None。
pub fn sample_inputs(dir: &Path) -> Option<SampleInputs> {
    use coding_lab::hook::HookInput;
    use coding_lab::merge::Input;

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
    Some(SampleInputs {
        session,
        inputs: inputs.into_iter().map(|(ts, _, i)| (ts, i)).collect(),
        message_of_line,
    })
}

/// 把一個樣本目錄餵過 merge（照到達時間；同一毫秒 hook 先）。
/// 沒有到達時間（`transcript-arrivals.jsonl`）的樣本回 None。
pub fn merged_events(dir: &Path) -> Option<Merged> {
    let s = sample_inputs(dir)?;
    let mut m = coding_lab::merge::Merger::new(Box::new(NoDanger));
    let mut events = Vec::new();
    for (ts, input) in s.inputs {
        events.extend(m.tick(ts));
        events.extend(m.handle(input));
    }
    Some(Merged {
        session: s.session,
        events,
        message_of_line: s.message_of_line,
    })
}
