//! resume／compact 接回（#651）：用 samples 的真實 hook 序列推 [`TailPlanner`]，照每行寫入 transcript 的
//! 時間模擬 tail 讀檔，確認每行只讀一次、session 開著時寫的行都讀到；再把結果交給 merge 與 live，
//! 確認串流只 append，而且 resume／compact 在串流裡各有一行狀態。沒有 samples/ 時略過。

mod common;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use common::{arrivals, read_jsonl, sample_sessions, sessions_in};

use coding_lab::event::{Danger, Event};
use coding_lab::hook::HookInput;
use coding_lab::live::{Channel, Live};
use coding_lab::merge::{DangerRules, Input, Merger};
use coding_lab::transcript::{Start, TailAction, TailPlanner};
use serde_json::Value;

struct NoDanger;
impl DangerRules for NoDanger {
    fn check(&self, _tool: &str, _input: &Value) -> Option<Danger> {
        None
    }
}

/// merge 會讀的行（contracts.md §3「讀的行」）；其他 type 漏讀不影響畫面。
fn is_read(v: &Value) -> bool {
    match v["type"].as_str() {
        Some("assistant") | Some("user") => true,
        Some("system") => v["subtype"] == "compact_boundary",
        _ => false,
    }
}

/// `2026-10-16T00:48:20.533Z` → epoch ms（samples 的 transcript 都是 UTC、毫秒三位）。
fn rfc3339_ms(s: &str) -> Option<u64> {
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss, ms) = (num(11, 13)?, num(14, 16)?, num(17, 19)?, num(20, 23)?);
    // days from civil（Howard Hinnant）
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((((days * 24 + hh) * 60 + mm) * 60 + ss) as u64 * 1000 + ms as u64)
}

struct Sample {
    name: String,
    /// (到達時間, hook)：SessionStart 只有 command hook（`ensure` 轉送），其他取 http 那一份
    hooks: Vec<(u64, Value)>,
    lines: Vec<Value>,
    /// 每行寫進檔案的時間：有 transcript-arrivals 就用它（缺的行沿用前一行），否則用行的 timestamp（取前綴最大值，timestamp 會倒退）
    written_at: Vec<u64>,
}

fn load(dir: &Path) -> Option<Sample> {
    let hooks: Vec<(u64, Value)> = read_jsonl(&dir.join("hooks.jsonl"))
        .into_iter()
        .filter(|h| {
            let start = h["payload"]["hook_event_name"] == "SessionStart";
            h["via"] == if start { "command" } else { "http" }
        })
        .filter_map(|h| Some((h["ts_seen_ms"].as_u64()?, h["payload"].clone())))
        .collect();
    let lifecycle = hooks.iter().any(|(_, h)| {
        h["hook_event_name"] == "SessionStart"
            && matches!(h["source"].as_str(), Some("resume" | "compact"))
    });
    let lines = read_jsonl(&dir.join("transcript.jsonl"));
    if !lifecycle || lines.is_empty() {
        return None;
    }
    let arrived = arrivals(&dir.join("transcript-arrivals.jsonl"));
    let mut written_at = vec![u64::MAX; lines.len()];
    if arrived.is_empty() {
        let mut t = 0;
        for (i, v) in lines.iter().enumerate() {
            if let Some(ts) = v["timestamp"].as_str().and_then(rfc3339_ms) {
                t = t.max(ts);
            }
            written_at[i] = t;
        }
    } else {
        written_at = written_from_arrivals(lines.len(), &arrived);
    }
    Some(Sample {
        name: dir.file_name()?.to_string_lossy().to_string(),
        hooks,
        lines,
        written_at,
    })
}

/// 依 hook-capture 的到達紀錄算每行寫進檔案的時間。沒有紀錄的行（hook-capture 提早停止記錄）當成和前一行同時寫入；
/// 行號從 1 起算，0 是壞資料。
fn written_from_arrivals(len: usize, arrived: &HashMap<u64, u64>) -> Vec<u64> {
    // 第 1 行就沒有紀錄時，用行號最小的有效行（≥ 1）的時間
    let mut t = arrived
        .iter()
        .filter(|(no, _)| **no >= 1)
        .min_by_key(|(no, _)| **no)
        .map_or(0, |(_, ts)| *ts);
    (1..=len as u64)
        .map(|no| {
            if let Some(&ts) = arrived.get(&no) {
                t = ts;
            }
            t
        })
        .collect()
}

/// 模擬 collector 的主 transcript tail：Start(Beginning) 從第 1 行、Start(End) 從當下檔尾，
/// 開著的時候每寫一行就讀到，Stop 前讀完已寫的行（`stop_drains_lines_written_before_stop`）。
/// 回傳依處理順序排好的 merge 輸入，以及被讀到的行號（依讀取順序）。
fn simulate(s: &Sample) -> (Vec<(u64, Input)>, Vec<u64>) {
    let sid = s.hooks[0].1["session_id"].as_str().unwrap().to_string();
    // (時間, 0=hook／1=寫一行, 索引)：同一毫秒 hook 先處理（實測 hook 永遠先到）
    let mut timeline: Vec<(u64, u8, usize)> = s
        .hooks
        .iter()
        .enumerate()
        .map(|(i, (ts, _))| (*ts, 0, i))
        .collect();
    timeline.extend(
        s.written_at
            .iter()
            .enumerate()
            .filter(|(_, t)| **t != u64::MAX)
            .map(|(i, t)| (*t, 1, i)),
    );
    timeline.sort();

    let mut planner = TailPlanner::default();
    let mut written = 0u64; // 檔案目前有幾行
    let mut next: Option<u64> = None; // tail 開著時，下一個要讀的行號
    let mut read = Vec::new();
    let mut inputs = Vec::new();
    let drain = |next: &mut Option<u64>,
                 written: u64,
                 ts: u64,
                 read: &mut Vec<u64>,
                 inputs: &mut Vec<(u64, Input)>| {
        if let Some(n) = next {
            while *n <= written {
                read.push(*n);
                inputs.push((
                    ts,
                    Input::Line {
                        session: sid.clone(),
                        file: "main".into(),
                        no: *n,
                        value: s.lines[*n as usize - 1].clone(),
                        ts_seen: ts,
                    },
                ));
                *n += 1;
            }
        }
    };
    for (ts, kind, i) in timeline {
        if kind == 1 {
            written = written.max(i as u64 + 1);
            drain(&mut next, written, ts, &mut read, &mut inputs);
            continue;
        }
        let hook = HookInput::from_json(s.hooks[i].1.to_string().as_bytes()).unwrap();
        for action in planner.on_hook(&hook) {
            match action {
                TailAction::Start { file, start, .. } if file == "main" => {
                    next = Some(match start {
                        Start::Beginning => 1,
                        Start::End => written + 1,
                    });
                    drain(&mut next, written, ts, &mut read, &mut inputs);
                }
                TailAction::Stop { file, .. } if file == "main" => {
                    drain(&mut next, written, ts, &mut read, &mut inputs);
                    next = None;
                }
                TailAction::StopSession { .. } => {
                    drain(&mut next, written, ts, &mut read, &mut inputs);
                    next = None;
                }
                _ => {}
            }
        }
        inputs.push((ts, Input::Hook { hook, ts_seen: ts }));
    }
    (inputs, read)
}

/// 檢查 `dirs` 裡每個有 resume／compact 的樣本；回傳檢查了幾個。
fn check_samples(dirs: &[std::path::PathBuf]) -> usize {
    let mut checked = 0;
    for dir in dirs {
        let Some(s) = load(dir) else { continue };
        checked += 1;
        let name = &s.name;
        let (inputs, read) = simulate(&s);

        // 1. tail：每行最多讀一次；沒讀到的只能是 merge 本來就略過的行（session 結束到 resume 之間寫的 meta 行）
        let mut seen = HashSet::new();
        for no in &read {
            assert!(seen.insert(*no), "{name}: 第 {no} 行讀了兩次");
        }
        let missed: Vec<(u64, String)> = (1..=s.lines.len() as u64)
            .filter(|no| !seen.contains(no))
            .filter(|no| is_read(&s.lines[*no as usize - 1]))
            .map(|no| (no, s.lines[no as usize - 1]["type"].to_string()))
            .collect();
        assert!(missed.is_empty(), "{name}: 漏讀 {missed:?}");

        // 2. merge：串流只 append；每次 SessionStart 都有 session.started，來源依序對得上
        let mut m = Merger::new(Box::new(NoDanger));
        let mut events: Vec<Event> = Vec::new();
        for (ts, input) in inputs {
            events.extend(m.tick(ts));
            events.extend(m.handle(input));
        }
        let pos: Vec<u64> = events.iter().filter_map(|e| e.pos()).collect();
        assert!(
            pos.windows(2).all(|w| w[0] < w[1]),
            "{name}: pos 不是遞增 {pos:?}"
        );
        let mut lines_with_pos = BTreeSet::new();
        for e in events.iter().filter(|e| e.pos().is_some()) {
            if let Some(l) = &e.line
                && e.type_name() != "model.changed"
            {
                assert!(
                    lines_with_pos.insert((l.file.clone(), l.no, e.type_name().to_string())),
                    "{name}: {}:{} 的 {} 進串流兩次",
                    l.file,
                    l.no,
                    e.type_name()
                );
            }
        }
        let sources = |evs: &[Value]| -> Vec<String> {
            evs.iter()
                .filter(|e| e["type"] == "session.started")
                .map(|e| e["payload"]["source"].as_str().unwrap_or("").to_string())
                .collect()
        };
        let expected: Vec<String> = s
            .hooks
            .iter()
            .filter(|(_, h)| h["hook_event_name"] == "SessionStart")
            .map(|(_, h)| h["source"].as_str().unwrap_or("").to_string())
            .collect();
        let as_json: Vec<Value> = events
            .iter()
            .map(|e| serde_json::from_str(&e.to_json_line()).unwrap())
            .collect();
        assert_eq!(sources(&as_json), expected, "{name}");

        // 3. live：resume／compact 在 session 的串流裡各有一行狀態（contracts.md §3、§6）
        let mut live = Live::new();
        for e in &events {
            live.apply(e);
        }
        let now = events.last().map_or(0, |e| e.ts_seen);
        let snap = live.snapshot(&Channel::Session(events[0].session.clone()), 0, now);
        let in_stream: Vec<String> = sources(snap["stream"].as_array().unwrap())
            .into_iter()
            .filter(|s| s != "startup")
            .collect();
        let want: Vec<String> = expected.into_iter().filter(|s| s != "startup").collect();
        assert_eq!(in_stream, want, "{name}: 串流裡的 resume／compact 狀態行");
        eprintln!(
            "{name}：讀 {} 行（共 {}）、{} 個 Event、串流狀態行 {in_stream:?}",
            read.len(),
            s.lines.len(),
            events.len()
        );
    }
    checked
}

#[test]
fn samples_compact_and_resume_neither_repeat_nor_drop_lines() {
    let dirs = sample_sessions();
    if dirs.is_empty() {
        eprintln!("略過：沒有 samples/sessions");
        return;
    }
    if check_samples(&dirs) == 0 {
        // samples 不進 git、每台機器不同：沒有 resume／compact 的樣本就略過，不讓 check.sh 紅燈（#688）
        eprintln!("略過：samples 裡沒有 resume／compact 的 session");
    }
}

/// 把一個樣本複製到 target/tmp，給測試改動用。
fn copy_sample(name: &str, to: &Path) -> Option<std::path::PathBuf> {
    let from = sample_sessions().into_iter().find(|d| d.ends_with(name))?;
    let dir = to.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    for entry in std::fs::read_dir(&from).ok()?.flatten() {
        if entry.path().is_file() {
            std::fs::copy(entry.path(), dir.join(entry.file_name())).ok()?;
        }
    }
    Some(dir)
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lifecycle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// hook-capture 漏記檔尾幾行的到達時間（提早停止記錄，#688）：那些行當成和前一行同時寫入，不算 collector 漏讀。
#[test]
fn missing_arrival_is_not_reported_as_a_dropped_line() {
    let root = tmp("gap");
    let Some(dir) = copy_sample("a41c94d2", &root) else {
        eprintln!("略過：沒有樣本 a41c94d2");
        return;
    };
    let lines = read_jsonl(&dir.join("transcript.jsonl"));
    let gap = lines
        .iter()
        .rposition(|v| v["type"] == "assistant")
        .map(|i| i as u64 + 1)
        .unwrap();
    let path = dir.join("transcript-arrivals.jsonl");
    let kept: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| {
            serde_json::from_str::<Value>(l)
                .is_ok_and(|a| a["line_no"].as_u64().is_some_and(|n| n < gap))
        })
        .map(str::to_string)
        .collect();
    std::fs::write(&path, kept.join("\n") + "\n").unwrap();
    assert_eq!(check_samples(&sessions_in(&root)), 1);
}

/// arrivals 有 line_no 0 這種壞資料時不 panic（#688）。
#[test]
fn zero_line_number_in_arrivals_does_not_panic() {
    let root = tmp("zero");
    let Some(dir) = copy_sample("a41c94d2", &root) else {
        eprintln!("略過：沒有樣本 a41c94d2");
        return;
    };
    let path = dir.join("transcript-arrivals.jsonl");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"line_no\":0,\"ts_seen_ms\":1}\n");
    std::fs::write(&path, text).unwrap();
    assert_eq!(check_samples(&sessions_in(&root)), 1);
}

/// 有 samples 但沒有 resume／compact 的樣本時略過，不失敗（#688）。
#[test]
fn samples_without_lifecycle_are_skipped() {
    let root = tmp("none");
    let Some(_) = copy_sample("377fa132", &root) else {
        eprintln!("略過：沒有樣本 377fa132");
        return;
    };
    assert_eq!(check_samples(&sessions_in(&root)), 0);
}

/// 第 1 行沒有到達紀錄時，用第一筆有效行（line_no ≥ 1）的時間，不吃 line_no 0 這種壞資料的時間戳；
/// 否則那一行會被當成在所有 hook 之前寫入（#688）。
#[test]
fn first_line_without_arrival_takes_the_first_valid_time() {
    let arrived = HashMap::from([(0, 1), (2, 500), (3, 600)]);
    assert_eq!(written_from_arrivals(3, &arrived), [500, 500, 600]);
}
