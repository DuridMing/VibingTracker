//! merge 的行為測試（方案 C，docs/plan/PLAN.md「順序問題與解法」、#632）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use coding_lab::event::{Danger, DangerLevel, Event};
use coding_lab::hook::HookInput;
use coding_lab::merge::{DangerRules, FALLBACK_AFTER_MS, Input, Merger};
use serde_json::{Value, json};

const S: &str = "s1";

fn hook(v: Value) -> HookInput {
    HookInput::from_json(v.to_string().as_bytes()).unwrap()
}

fn pre(id: &str, tool: &str, input: Value, ts: u64) -> Input {
    Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PreToolUse", "tool_name": tool,
            "tool_input": input, "tool_use_id": id,
        })),
        ts_seen: ts,
    }
}

fn post(id: &str, ts: u64) -> Input {
    Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_use_id": id, "duration_ms": 30,
            "tool_response": { "stdout": "ok\n", "stderr": "", "interrupted": false },
        })),
        ts_seen: ts,
    }
}

fn line(no: u64, v: Value, ts: u64) -> Input {
    Input::Line {
        session: S.into(),
        file: "main".into(),
        no,
        value: v,
        ts_seen: ts,
    }
}

fn thinking(uuid: &str, msg: &str) -> Value {
    json!({ "type": "assistant", "uuid": uuid, "timestamp": "2026-10-16T00:00:00Z",
            "message": { "id": msg, "model": "claude-opus-5-5",
                         "content": [{ "type": "thinking", "thinking": "想一下", "signature": "x" }] } })
}

fn text(uuid: &str, msg: &str) -> Value {
    json!({ "type": "assistant", "uuid": uuid, "timestamp": "2026-10-16T00:00:00Z",
            "message": { "id": msg, "model": "claude-opus-5-5",
                         "content": [{ "type": "text", "text": "我先讀檔" }] } })
}

fn tool_use(uuid: &str, msg: &str, id: &str) -> Value {
    json!({ "type": "assistant", "uuid": uuid, "timestamp": "2026-10-16T00:00:00Z",
            "message": { "id": msg, "model": "claude-opus-5-5",
                         "content": [{ "type": "tool_use", "id": id, "name": "Bash",
                                       "input": { "command": "ls" } }] } })
}

fn tool_result(uuid: &str, id: &str) -> Value {
    json!({ "type": "user", "uuid": uuid, "timestamp": "2026-10-16T00:00:01Z",
            "message": { "content": [{ "type": "tool_result", "tool_use_id": id, "content": "ok\n" }] } })
}

fn types(evs: &[Event]) -> Vec<&str> {
    evs.iter().map(|e| e.type_name()).collect()
}

struct NoDanger;
impl DangerRules for NoDanger {
    fn check(&self, _tool: &str, _input: &Value) -> Option<Danger> {
        None
    }
}

/// 只把 `rm -rf` 當危險的測試規則。
struct RmRf;
impl DangerRules for RmRf {
    fn check(&self, _tool: &str, input: &Value) -> Option<Danger> {
        input["command"]
            .as_str()
            .filter(|c| c.contains("rm -rf"))
            .map(|_| Danger {
                rule: "rm-rf".into(),
                level: DangerLevel::Danger,
                why: "遞迴強制刪除".into(),
            })
    }
}

fn merger() -> Merger {
    Merger::new(Box::new(NoDanger))
}

#[test]
fn hook_first_is_staged_then_placed_by_transcript_in_line_order() {
    let mut m = merger();
    let a = m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    assert_eq!(types(&a), ["tool.called"]);
    assert_eq!(a[0].pos(), None, "hook 先到只進暫存區");
    assert_eq!(a[0].payload["placement"], "staged");
    assert_eq!(a[0].ts_exec, Some(1000));

    let b = m.handle(line(1, thinking("u1", "m1"), 1100));
    let c = m.handle(line(2, text("u2", "m1"), 1100));
    let d = m.handle(line(3, tool_use("u3", "m1", "t1"), 1110));
    assert_eq!(types(&b), ["assistant.thinking"]);
    assert_eq!(types(&c), ["assistant.text"]);
    assert_eq!(types(&d), ["tool.placed"]);
    assert_eq!(
        [b[0].pos(), c[0].pos(), d[0].pos()],
        [Some(1), Some(2), Some(3)]
    );
    assert_eq!(d[0].payload["placement"], "stream");
    assert_eq!(d[0].line.as_ref().map(|l| l.no), Some(3));
}

#[test]
fn transcript_first_creates_pending_card_in_stream() {
    let mut m = merger();
    let a = m.handle(line(1, tool_use("u1", "m1", "t1"), 1000));
    assert_eq!(types(&a), ["tool.called"]);
    assert_eq!(a[0].payload["status"], "pending");
    assert_eq!(a[0].payload["placement"], "stream");
    assert_eq!(a[0].pos(), Some(1));

    let b = m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1050));
    assert_eq!(types(&b), ["tool.updated"]);
    assert_eq!(b[0].payload["status"], "running");
    assert_eq!(b[0].pos(), None, "狀態更新不佔串流位置");
}

#[test]
fn card_done_while_staged_enters_stream_once() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    let a = m.handle(post("t1", 1030));
    assert_eq!(types(&a), ["tool.updated", "tool.result"]);
    assert_eq!(a[0].payload["status"], "done");
    assert_eq!(a[1].payload["output"], "ok\n");

    let b = m.handle(line(1, tool_use("u1", "m1", "t1"), 1200));
    assert_eq!(types(&b), ["tool.placed"]);
    let c = m.handle(line(2, tool_result("u2", "t1"), 1300));
    assert!(
        c.is_empty(),
        "tool.result 先到者為準，transcript 不重送：{c:?}"
    );
}

#[test]
fn transcript_result_first_then_hook_does_not_duplicate() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    m.handle(line(1, tool_use("u1", "m1", "t1"), 1100));
    let a = m.handle(line(2, tool_result("u2", "t1"), 1200));
    assert_eq!(types(&a), ["tool.result"]);
    let b = m.handle(post("t1", 1250));
    assert_eq!(types(&b), ["tool.updated"]);
}

#[test]
fn staged_card_falls_back_after_timeout_and_never_moves() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    assert!(m.tick(1000 + FALLBACK_AFTER_MS - 1).is_empty());
    let a = m.tick(1000 + FALLBACK_AFTER_MS);
    assert_eq!(types(&a), ["tool.placed"]);
    assert_eq!(a[0].payload["placement"], "fallback");
    assert_eq!(a[0].pos(), Some(1));
    assert!(
        m.tick(1000 + 3 * FALLBACK_AFTER_MS).is_empty(),
        "只降級一次"
    );

    // 晚到的思考照常 append 並標延遲；tool_use 行不再搬位置
    let b = m.handle(line(1, thinking("u1", "m1"), 20_000));
    assert_eq!(b[0].pos(), Some(2));
    assert_eq!(b[0].payload["delayed"], true);
    let c = m.handle(line(2, tool_use("u2", "m1", "t1"), 20_010));
    assert!(c.is_empty(), "{c:?}");
    // 之後正常的思考不再標延遲
    let d = m.handle(line(3, thinking("u3", "m2"), 20_100));
    assert!(d[0].payload.get("delayed").is_none());
}

#[test]
fn degraded_transcript_falls_back_immediately() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    let a = m.handle(Input::Degraded {
        session: S.into(),
        file: "main".into(),
        error: "invalid JSON".into(),
        ts_seen: 1100,
    });
    assert_eq!(types(&a), ["transcript.degraded", "tool.placed"]);
    assert_eq!(a[1].payload["placement"], "fallback");

    let b = m.handle(pre("t2", "Bash", json!({"command": "pwd"}), 1200));
    assert_eq!(
        types(&b),
        ["tool.called", "tool.placed"],
        "降級中的新卡片直接進串流"
    );

    let c = m.handle(Input::Recovered {
        session: S.into(),
        file: "main".into(),
        ts_seen: 1300,
    });
    assert_eq!(types(&c), ["transcript.recovered"]);
    let d = m.handle(pre("t3", "Bash", json!({"command": "id"}), 1400));
    assert_eq!(types(&d), ["tool.called"], "恢復後回到暫存");
}

/// 自動模式分類器拒絕：PermissionDenied 先到，transcript 的 tool_use 行 4.5 秒後才寫。
#[test]
fn classifier_denial_then_late_tool_use_line() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "echo x"}), 1000));
    let a = m.handle(Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PermissionDenied", "tool_name": "Bash",
            "tool_use_id": "t1", "reason": "[Safety Bypass Flag]",
        })),
        ts_seen: 5425,
    });
    assert_eq!(types(&a), ["tool.updated"]);
    assert_eq!(a[0].payload["status"], "failed");
    assert_eq!(a[0].payload["error"], "[Safety Bypass Flag]");
    let b = m.handle(line(1, tool_use("u1", "m1", "t1"), 5545));
    assert_eq!(types(&b), ["tool.placed"]);
    assert_eq!(b[0].payload["placement"], "stream", "4.5 秒內仍在門檻內");
}

#[test]
fn danger_is_raised_when_pre_tool_use_arrives() {
    let mut m = Merger::new(Box::new(RmRf));
    let a = m.handle(pre("t1", "Bash", json!({"command": "rm -rf build"}), 1000));
    assert_eq!(types(&a), ["tool.called", "attention.raised"]);
    assert_eq!(a[0].payload["danger"]["rule"], "rm-rf");
    assert_eq!(a[1].payload["kind"], "danger");
    assert_eq!(a[1].payload["ref"], "t1");
}

#[test]
fn danger_on_transcript_first_card_goes_into_update() {
    let mut m = Merger::new(Box::new(RmRf));
    m.handle(line(1, tool_use("u1", "m1", "t1"), 1000));
    let a = m.handle(pre("t1", "Bash", json!({"command": "rm -rf build"}), 1050));
    assert_eq!(types(&a), ["tool.updated", "attention.raised"]);
    assert_eq!(a[0].payload["danger"]["rule"], "rm-rf");
}

#[test]
fn failure_hook_parses_exit_code() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "nope"}), 1000));
    let a = m.handle(Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PostToolUseFailure", "tool_name": "Bash",
            "tool_use_id": "t1", "duration_ms": 12, "is_interrupt": false,
            "error": "Exit code 127\nbash: nope: command not found",
        })),
        ts_seen: 1020,
    });
    assert_eq!(types(&a), ["tool.updated", "tool.result"]);
    assert_eq!(a[0].payload["status"], "failed");
    assert_eq!(a[1].payload["ok"], false);
    assert_eq!(a[1].payload["exitCode"], 127);
}

#[test]
fn duplicate_transcript_lines_are_ignored() {
    let mut m = merger();
    assert_eq!(m.handle(line(1, thinking("u1", "m1"), 1000)).len(), 1);
    assert!(m.handle(line(1, thinking("u1", "m1"), 1001)).is_empty());
}

#[test]
fn non_message_lines_are_ignored() {
    let mut m = merger();
    for (no, t) in [
        "attachment",
        "queue-operation",
        "ai-title",
        "file-history-snapshot",
    ]
    .iter()
    .enumerate()
    {
        let v = json!({ "type": t, "uuid": format!("x{no}") });
        assert!(m.handle(line(no as u64 + 1, v, 1000)).is_empty(), "{t}");
    }
    let prompt = json!({ "type": "user", "uuid": "p", "message": { "content": "hi" } });
    assert!(
        m.handle(line(9, prompt, 1000)).is_empty(),
        "prompt 由 hook 產生"
    );
}

#[test]
fn sessions_have_independent_seq_and_pos() {
    let mut m = merger();
    let a = m.handle(line(1, thinking("u1", "m1"), 1000));
    let b = m.handle(Input::Line {
        session: "s2".into(),
        file: "main".into(),
        no: 1,
        value: thinking("v1", "n1"),
        ts_seen: 1001,
    });
    assert_eq!((a[0].seq, a[0].pos()), (1, Some(1)));
    assert_eq!((b[0].seq, b[0].pos()), (1, Some(1)));
    assert_eq!(b[0].session, "s2");
}

#[test]
fn subagent_lines_carry_agent_id() {
    let mut m = merger();
    let a = m.handle(Input::Line {
        session: S.into(),
        file: "agent-a123".into(),
        no: 1,
        value: thinking("u1", "m1"),
        ts_seen: 1000,
    });
    assert_eq!(a[0].agent, "a123");
    let b = m.handle(Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PreToolUse", "tool_name": "Glob",
            "tool_input": {}, "tool_use_id": "t9", "agent_id": "a123", "agent_type": "Explore",
        })),
        ts_seen: 1001,
    });
    assert_eq!(b[0].agent, "a123");
}

#[test]
fn session_boundaries_and_prompt() {
    let mut m = merger();
    let a = m.handle(Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "SessionStart", "source": "startup",
            "cwd": "/p", "model": "claude-opus-5-5", "transcript_path": "/t.jsonl",
            "context_tokens": 100,
        })),
        ts_seen: 1,
    });
    assert_eq!(types(&a), ["session.started"]);
    assert_eq!(a[0].payload["model"], "claude-opus-5-5");
    assert_eq!(a[0].raw.as_ref().unwrap()["context_tokens"], 100);
    let b = m.handle(Input::Hook {
        hook: hook(json!({ "session_id": S, "hook_event_name": "UserPromptSubmit", "prompt": "修 bug", "prompt_id": "p1" })),
        ts_seen: 2,
    });
    assert_eq!(types(&b), ["prompt.submitted"]);
    let c = m.handle(Input::Hook {
        hook: hook(json!({ "session_id": S, "hook_event_name": "SessionEnd", "reason": "other" })),
        ts_seen: 3,
    });
    assert_eq!(types(&c), ["session.ended"]);
    assert_eq!([a[0].seq, b[0].seq, c[0].seq], [1, 2, 3]);
}

#[test]
fn every_event_matches_the_contract() {
    let mut m = Merger::new(Box::new(RmRf));
    let mut all = Vec::new();
    all.extend(m.handle(pre("t1", "Bash", json!({"command": "rm -rf x"}), 1000)));
    all.extend(m.handle(line(1, thinking("u1", "m1"), 1100)));
    all.extend(m.handle(line(2, text("u2", "m1"), 1100)));
    all.extend(m.handle(line(3, tool_use("u3", "m1", "t1"), 1100)));
    all.extend(m.handle(post("t1", 1200)));
    for ev in all {
        let back = Event::from_json_line(&ev.to_json_line()).unwrap();
        assert!(
            back.payload_typed().unwrap().is_some(),
            "{}",
            ev.to_json_line()
        );
    }
}

// ── samples 回放（沒有 samples/ 時略過）

struct Replay {
    events: Vec<Event>,
    /// transcript 行號 → message.id（只有 assistant 行）
    message_of_line: HashMap<(String, u64), String>,
}

fn replay(dir: &Path) -> Option<Replay> {
    let read = |p: PathBuf| -> Vec<Value> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let hooks = read(dir.join("hooks.jsonl"));
    if read(dir.join("transcript-arrivals.jsonl")).is_empty() {
        return None;
    }
    let session = hooks.first()?["payload"]["session_id"]
        .as_str()?
        .to_string();

    // 主檔與每個 subagent 檔：(file 名稱, transcript 行, 行號 → 到達時間)
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
    for h in &hooks {
        if h["via"] != "http" {
            continue;
        }
        let ts = h["ts_seen_ms"].as_u64()?;
        let body = h["payload"].to_string();
        inputs.push((
            ts,
            0,
            Input::Hook {
                hook: HookInput::from_json(body.as_bytes()).ok()?,
                ts_seen: ts,
            },
        ));
    }
    let mut message_of_line = HashMap::new();
    for (file, transcript, arrivals) in files {
        // session 中途重開（resume）時同一個檔會被 tail 兩輪，第二輪把舊行重讀一次（backfill）；
        // 同一行號取最早的到達時間
        let mut arrival_ts: HashMap<u64, u64> = HashMap::new();
        for a in read(arrivals) {
            if let (Some(no), Some(ts)) = (a["line_no"].as_u64(), a["ts_seen_ms"].as_u64()) {
                let t = arrival_ts.entry(no).or_insert(ts);
                *t = (*t).min(ts);
            }
        }
        for (i, t) in read(transcript).into_iter().enumerate() {
            let no = i as u64 + 1;
            let Some(&ts) = arrival_ts.get(&no) else {
                continue;
            };
            if let Some(id) = t["message"]["id"].as_str() {
                message_of_line.insert((file.clone(), no), id.to_string());
            }
            let input = Input::Line {
                session: session.clone(),
                file: file.clone(),
                no,
                value: t,
                ts_seen: ts,
            };
            inputs.push((ts, 1, input));
        }
    }
    // 同一毫秒內 hook 先處理（實測 hook 永遠先到）；transcript 行保持行號順序
    inputs.sort_by_key(|(ts, kind, _)| (*ts, *kind));
    let mut m = merger();
    let mut events = Vec::new();
    for (ts, _, input) in inputs {
        events.extend(m.tick(ts));
        events.extend(m.handle(input));
    }
    Some(Replay {
        events,
        message_of_line,
    })
}

fn sample_sessions() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/sessions");
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs
}

#[test]
fn samples_replay_never_inserts_and_keeps_thinking_before_action() {
    let mut replayed = 0;
    for dir in sample_sessions() {
        let Some(r) = replay(&dir) else { continue };
        replayed += 1;
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let count = |t: &str| r.events.iter().filter(|e| e.type_name() == t).count();
        eprintln!(
            "回放 {name}：{} 個 Event，thinking {}、text {}、tool.called {}、tool.placed {}",
            r.events.len(),
            count("assistant.thinking"),
            count("assistant.text"),
            count("tool.called"),
            count("tool.placed"),
        );

        // 串流位置只增不減（沒有往前插入）
        let positions: Vec<u64> = r.events.iter().filter_map(|e| e.pos()).collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "{name}: pos 不是遞增 {positions:?}"
        );

        // 每張卡片的串流位置在同一則訊息的 thinking／text 之後
        let mut pos_of_message: HashMap<String, Vec<u64>> = HashMap::new();
        for e in &r.events {
            if matches!(e.type_name(), "assistant.thinking" | "assistant.text") {
                let line = e.line.as_ref().unwrap();
                let msg = &r.message_of_line[&(line.file.clone(), line.no)];
                pos_of_message
                    .entry(msg.clone())
                    .or_default()
                    .push(e.pos().unwrap());
            }
        }
        for e in r.events.iter().filter(|e| e.type_name() == "tool.placed") {
            let line = e.line.as_ref().unwrap();
            if let Some(msg) = r.message_of_line.get(&(line.file.clone(), line.no)) {
                for p in pos_of_message.get(msg).into_iter().flatten() {
                    assert!(*p < e.pos().unwrap(), "{name}: 思考排在動作之後 {msg}");
                }
            }
        }
        // 實測資料不該觸發降級
        assert!(
            !r.events
                .iter()
                .any(|e| e.payload["placement"] == "fallback"),
            "{name}: 不該有降級卡片"
        );
    }
    let dirs = sample_sessions();
    if dirs.is_empty() {
        eprintln!("略過：沒有 samples/sessions");
    } else {
        assert!(replayed > 0, "有 samples/ 卻沒有任何 session 可回放");
    }
}
