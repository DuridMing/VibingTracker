//! merge 的行為測試（方案 C，docs/plan/PLAN.md「順序問題與解法」、#632）。

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::{arrivals, read_jsonl, sample_sessions};

use coding_lab::event::{Danger, DangerLevel, Event};
use coding_lab::hook::HookInput;
use coding_lab::merge::{
    DangerRules, FALLBACK_AFTER_MS, FALLBACK_WAIT_MS, Input, Merger, SESSION_EVICT_AFTER_MS,
    SESSION_IDLE_EVICT_MS,
};
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
    assert_eq!(types(&a), ["tool.updated", "tool.result"]);
    assert_eq!(a[0].payload["status"], "done");
    let b = m.handle(post("t1", 1250));
    assert_eq!(
        types(&b),
        ["tool.updated"],
        "duration 由 hook 補上，tool.result 不重送"
    );
}

/// 執行中按 Esc：沒有任何 hook，只有 transcript 的 tool_result（樣本 5d21db12）。
#[test]
fn esc_during_tool_ends_the_card_from_transcript() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "sleep 60"}), 1000));
    m.handle(line(1, tool_use("u1", "m1", "t1"), 1100));
    let mut r = tool_result("u2", "t1");
    r["message"]["content"][0]["is_error"] = json!(true);
    r["toolUseResult"] = json!("User rejected tool use");
    let a = m.handle(line(2, r, 14_000));
    assert_eq!(types(&a), ["tool.updated", "tool.result"]);
    assert_eq!(a[0].payload["status"], "failed");
    assert_eq!(a[0].payload["interrupted"], true);
    assert_eq!(a[1].payload["ok"], false);
}

/// 一般情況 hook 先給結束狀態，transcript 的 tool_result 不再送一次。
#[test]
fn transcript_result_after_hook_does_not_resend_status() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    m.handle(line(1, tool_use("u1", "m1", "t1"), 1100));
    m.handle(post("t1", 1150));
    let a = m.handle(line(2, tool_result("u2", "t1"), 1200));
    assert!(a.is_empty(), "{a:?}");
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
    // 危險進佇列，session.status 跟著變成 danger（#645）
    assert_eq!(
        types(&a),
        ["tool.called", "attention.raised", "session.status"]
    );
    assert_eq!(a[0].payload["danger"]["rule"], "rm-rf");
    assert_eq!(a[1].payload["kind"], "danger");
    assert_eq!(a[1].payload["ref"], "t1");
}

#[test]
fn danger_on_transcript_first_card_goes_into_update() {
    let mut m = Merger::new(Box::new(RmRf));
    m.handle(line(1, tool_use("u1", "m1", "t1"), 1000));
    let a = m.handle(pre("t1", "Bash", json!({"command": "rm -rf build"}), 1050));
    assert_eq!(
        types(&a),
        ["tool.updated", "attention.raised", "session.status"]
    );
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

fn agent_hook(event: &str, agent_id: &str, agent_type: Option<&str>, extra: Value) -> Input {
    let mut v = json!({ "session_id": S, "hook_event_name": event, "agent_id": agent_id });
    if let Some(t) = agent_type {
        v["agent_type"] = json!(t);
    }
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    Input::Hook {
        hook: hook(v),
        ts_seen: 0,
    }
}

fn at(input: Input, ts: u64) -> Input {
    match input {
        Input::Hook { hook, .. } => Input::Hook { hook, ts_seen: ts },
        other => other,
    }
}

fn bg_pre(id: &str, command: &str) -> Value {
    json!({ "tool_name": "Bash", "tool_input": { "command": command }, "tool_use_id": id })
}

/// 「下一步建議」的背景 agent（樣本 64c1a1a7）：PreToolUse 沒有 agent_type、也沒有 SubagentStart，
/// 工具實際上不會執行（PostToolBatch 回 "No tools needed for suggestion"）。
#[test]
fn suggestion_agent_tool_never_becomes_a_card() {
    let mut m = Merger::new(Box::new(RmRf));
    let a = m.handle(at(
        agent_hook("PreToolUse", "bg1", None, bg_pre("t1", "rm -rf x")),
        1000,
    ));
    assert!(a.is_empty(), "不建卡片、不判危險：{a:?}");
    assert!(m.tick(1000 + FALLBACK_AFTER_MS).is_empty(), "不降級進串流");
    let batch = json!({ "tool_calls": [{ "tool_use_id": "t1", "tool_name": "Bash",
                                         "tool_response": "No tools needed for suggestion" }] });
    assert!(
        m.handle(at(agent_hook("PostToolBatch", "bg1", None, batch), 1300))
            .is_empty()
    );
    let stop = m.handle(at(
        agent_hook("SubagentStop", "bg1", Some(""), json!({})),
        2000,
    ));
    assert_eq!(types(&stop), ["agent.finished"]);
    assert_eq!(stop[0].agent, "bg");
    assert!(m.tick(1000 + 10 * FALLBACK_AFTER_MS).is_empty());
}

/// 萬一背景 agent 的工具真的執行了（收到 PostToolUse），才補建卡片、判危險，直接放進串流末端。
#[test]
fn background_tool_that_really_ran_is_shown_late() {
    let mut m = Merger::new(Box::new(RmRf));
    assert!(
        m.handle(at(
            agent_hook("PreToolUse", "bg1", None, bg_pre("t1", "rm -rf x")),
            1000
        ))
        .is_empty()
    );
    let post = json!({ "tool_name": "Bash", "tool_use_id": "t1", "duration_ms": 5,
                       "tool_response": { "stdout": "", "stderr": "", "interrupted": false } });
    let a = m.handle(at(agent_hook("PostToolUse", "bg1", None, post), 1200));
    assert_eq!(
        types(&a)[..5],
        [
            "tool.called",
            "attention.raised",
            "session.status",
            "tool.placed",
            "tool.updated"
        ],
        "{a:?}"
    );
    assert_eq!(a[0].agent, "bg");
    assert_eq!(a[0].ts_exec, Some(1000));
    assert_eq!(a[3].payload["placement"], "fallback");
    assert!(m.tick(1000 + FALLBACK_AFTER_MS).is_empty(), "只放一次");

    // 失敗也算執行過
    m.handle(at(
        agent_hook("PreToolUse", "bg1", None, bg_pre("t2", "ls")),
        2000,
    ));
    let fail = json!({ "tool_name": "Bash", "tool_use_id": "t2", "error": "Exit code 1" });
    let b = m.handle(at(
        agent_hook("PostToolUseFailure", "bg1", None, fail),
        2100,
    ));
    assert_eq!(
        types(&b)[..3],
        ["tool.called", "tool.placed", "tool.updated"]
    );
}

/// 沒等到執行結果的背景工具 60 秒後丟掉；之後才到的 PostToolUse 也不補半張卡片。
#[test]
fn held_background_tool_is_forgotten_after_a_minute() {
    let mut m = merger();
    m.handle(at(
        agent_hook("PreToolUse", "bg1", None, bg_pre("t1", "ls")),
        1000,
    ));
    assert!(m.tick(1000 + 60_000).is_empty());
    let post = json!({ "tool_name": "Bash", "tool_use_id": "t1",
                       "tool_response": { "stdout": "", "stderr": "", "interrupted": false } });
    let a = m.handle(at(agent_hook("PostToolUse", "bg1", None, post), 62_000));
    assert!(a.is_empty(), "{a:?}");
}

/// 有 SubagentStart 的 agent 是真的 subagent：就算之後的工具事件沒有 agent_type 也照常建卡片。
#[test]
fn started_subagent_without_agent_type_still_gets_cards() {
    let mut m = merger();
    m.handle(at(
        agent_hook("SubagentStart", "a1", Some("Explore"), json!({})),
        900,
    ));
    let a = m.handle(at(
        agent_hook("PreToolUse", "a1", None, bg_pre("t1", "ls")),
        1000,
    ));
    assert_eq!(types(&a), ["tool.called"]);
    assert_eq!(a[0].agent, "a1");
    assert_eq!(a[0].payload["placement"], "staged");
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
    assert_eq!(types(&c), ["session.ended", "session.status"]);
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
    let hooks = read_jsonl(&dir.join("hooks.jsonl"));
    if read_jsonl(&dir.join("transcript-arrivals.jsonl")).is_empty() {
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
    for (file, transcript, arrival_file) in files {
        // session 中途重開（resume）時同一個檔會被 tail 兩輪，第二輪把舊行重讀一次（backfill）；
        // 同一行號取最早的到達時間
        let arrival_ts = arrivals(&arrival_file);
        for (i, t) in read_jsonl(&transcript).into_iter().enumerate() {
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

        // 實測資料不該觸發降級
        assert!(
            !r.events
                .iter()
                .any(|e| e.payload["placement"] == "fallback"),
            "{name}: 不該有降級卡片"
        );

        // 有結果的卡片一定有結束狀態（按 Esc 中斷時沒有任何 hook，只有 transcript 的 tool_result）
        let mut last_status: HashMap<String, String> = HashMap::new();
        for e in r.events.iter().filter(|e| e.type_name() == "tool.updated") {
            last_status.insert(
                e.payload["toolUseId"].as_str().unwrap().to_string(),
                e.payload["status"].as_str().unwrap().to_string(),
            );
        }
        for e in r.events.iter().filter(|e| e.type_name() == "tool.result") {
            let id = e.payload["toolUseId"].as_str().unwrap();
            let st = last_status.get(id).map(String::as_str);
            assert!(
                matches!(st, Some("done" | "failed")),
                "{name}: {id} 有 tool.result 但狀態停在 {st:?}"
            );
        }

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
    }
    let dirs = sample_sessions();
    if dirs.is_empty() {
        eprintln!("略過：沒有 samples/sessions");
    } else {
        assert!(replayed > 0, "有 samples/ 卻沒有任何 session 可回放");
    }
}

/// #670-1：collector 重啟後，續跑的 session 接著 store 裡最後的 seq／pos 編號，不從 1 重來。
#[test]
fn resumed_session_continues_seq_and_pos() {
    let mut m = merger();
    m.resume(S, 500, 200, "");
    let a = m.handle(line(1, thinking("u1", "m1"), 1000));
    assert_eq!((a[0].seq, a[0].pos()), (501, Some(201)));
    let b = m.handle(Input::Line {
        session: "s2".into(),
        file: "main".into(),
        no: 1,
        value: thinking("v1", "n1"),
        ts_seen: 1001,
    });
    assert_eq!(
        (b[0].seq, b[0].pos()),
        (1, Some(1)),
        "沒記錄的 session 照常從 1 開始"
    );
}

fn degraded(file: &str, ts: u64) -> Input {
    Input::Degraded {
        session: S.into(),
        file: file.into(),
        error: "invalid JSON".into(),
        ts_seen: ts,
    }
}

fn sub_pre(id: &str, agent: &str, ts: u64) -> Input {
    Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PreToolUse", "tool_name": "Glob",
            "tool_input": {}, "tool_use_id": id, "agent_id": agent, "agent_type": "Explore",
        })),
        ts_seen: ts,
    }
}

/// #670-2：subagent 檔降級只影響那個 subagent 的卡片，main 照常等 transcript。
#[test]
fn degraded_subagent_file_does_not_push_main_into_fallback() {
    let mut m = merger();
    m.handle(Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SubagentStart",
                          "agent_id": "a1", "agent_type": "Explore"})),
        ts_seen: 900,
    });
    m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    m.handle(sub_pre("t2", "a1", 1010));
    let a = m.handle(degraded("agent-a1", 1100));
    let placed: Vec<&Value> = a
        .iter()
        .filter(|e| e.kind == "tool.placed")
        .map(|e| &e.payload["toolUseId"])
        .collect();
    assert_eq!(placed, [&json!("t2")], "只有 a1 的暫存卡片降級");

    let b = m.handle(pre("t3", "Bash", json!({"command": "pwd"}), 1200));
    assert_eq!(types(&b), ["tool.called"], "main 的新卡片照常暫存");
    let c = m.handle(sub_pre("t4", "a1", 1210));
    assert_eq!(
        types(&c),
        ["tool.called", "tool.placed"],
        "a1 的新卡片直接進串流"
    );
}

/// #670-2：降級的 tail 被停掉或重開後不會再送 Recovered（新 tail 從健康狀態開始），
/// merge 要自己清掉降級並告訴前端，否則之後的卡片永遠走降級。
#[test]
fn degraded_is_cleared_when_the_tail_is_stopped_or_restarted() {
    let mut m = merger();
    m.handle(degraded("agent-a1", 1000));
    m.handle(degraded("main", 1000));
    let a = m.handle(Input::TailReset {
        session: S.into(),
        file: Some("agent-a1".into()),
        ts_seen: 1100,
    });
    assert_eq!(types(&a), ["transcript.recovered"]);
    assert_eq!(a[0].payload["file"], "agent-a1");
    let b = m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1200));
    assert_eq!(types(&b), ["tool.called", "tool.placed"], "main 還在降級");

    let c = m.handle(Input::TailReset {
        session: S.into(),
        file: None,
        ts_seen: 1300,
    });
    assert_eq!(types(&c), ["transcript.recovered"]);
    assert_eq!(c[0].payload["file"], "main");
    let d = m.handle(pre("t2", "Bash", json!({"command": "pwd"}), 1400));
    assert_eq!(types(&d), ["tool.called"], "重開 tail 後回到暫存");

    let e = m.handle(Input::TailReset {
        session: S.into(),
        file: Some("main".into()),
        ts_seen: 1500,
    });
    assert!(e.is_empty(), "沒降級時不送事件");
}

/// #670-3：背景 agent 的工具沒有 transcript 行，降級進串流後不能讓之後的文字一直標延遲。
#[test]
fn background_tool_fallback_does_not_mark_later_text_delayed() {
    let mut m = merger();
    m.handle(at(
        agent_hook("PreToolUse", "bg1", None, bg_pre("t1", "ls")),
        1000,
    ));
    let post = json!({ "tool_name": "Bash", "tool_use_id": "t1", "duration_ms": 5,
                       "tool_response": { "stdout": "", "stderr": "", "interrupted": false } });
    m.handle(at(agent_hook("PostToolUse", "bg1", None, post), 1200));
    let a = m.handle(line(1, text("u1", "m1"), 1300));
    assert!(a[0].payload.get("delayed").is_none(), "{:?}", a[0].payload);
}

/// #670-3：subagent 的卡片在等 transcript，不影響 main 的文字。
#[test]
fn waiting_subagent_card_does_not_mark_main_text_delayed() {
    let mut m = merger();
    m.handle(Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SubagentStart",
                          "agent_id": "a1", "agent_type": "Explore"})),
        ts_seen: 900,
    });
    m.handle(sub_pre("t1", "a1", 1000));
    m.tick(1000 + FALLBACK_AFTER_MS);
    let a = m.handle(line(1, text("u1", "m1"), 12_000));
    assert!(a[0].payload.get("delayed").is_none(), "{:?}", a[0].payload);
}

/// #670-3：降級卡片的 tool_use 行一直沒來（那行解析失敗被跳過、或 tail 重開從檔尾讀），
/// 過了 FALLBACK_WAIT_MS 或 tail 重開後就不再等，之後的文字不標延遲。
#[test]
fn fallback_wait_expires_or_is_reset_with_the_tail() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    m.tick(1000 + FALLBACK_AFTER_MS);
    let a = m.handle(line(1, text("u1", "m1"), 12_000));
    assert_eq!(a[0].payload["delayed"], true, "還在等的時候照常標延遲");
    m.tick(1000 + FALLBACK_AFTER_MS + FALLBACK_WAIT_MS);
    let b = m.handle(line(2, text("u2", "m2"), 80_000));
    assert!(b[0].payload.get("delayed").is_none(), "{:?}", b[0].payload);

    m.handle(pre("t2", "Bash", json!({"command": "pwd"}), 90_000));
    m.tick(90_000 + FALLBACK_AFTER_MS);
    m.handle(Input::TailReset {
        session: S.into(),
        file: Some("main".into()),
        ts_seen: 100_001,
    });
    let c = m.handle(line(3, text("u3", "m3"), 100_002));
    assert!(c[0].payload.get("delayed").is_none(), "{:?}", c[0].payload);
}

/// #670-4：PreToolUse 遺失（佇列滿被丟、或在 collector 重啟前）時，PostToolUse 先建了卡片；
/// transcript 的 tool_use 行到時卡片仍要進串流。
#[test]
fn card_known_only_from_post_tool_use_still_enters_the_stream() {
    let mut m = merger();
    let a = m.handle(post("t1", 1000));
    assert!(types(&a).contains(&"tool.updated"), "{:?}", types(&a));
    let b = m.handle(line(1, tool_use("u1", "m1", "t1"), 1100));
    assert_eq!(types(&b), ["tool.called"], "{b:?}");
    assert_eq!(b[0].payload["placement"], "stream");
    assert_eq!(b[0].pos(), Some(1));
    let c = m.handle(line(2, tool_use("u2", "m1", "t1"), 1200));
    assert!(c.is_empty(), "已進串流的卡片不再送");
}

fn hook_at(event: &str, ts: u64) -> Input {
    Input::Hook {
        hook: hook(
            json!({"session_id": S, "hook_event_name": event, "source": "resume",
                          "prompt": "hi", "cwd": "/p"}),
        ),
        ts_seen: ts,
    }
}

/// #670-12：結束的 session 過了寬限期就從記憶體移除（常駐模式下不能一直長、tick 也不能掃全部歷史），
/// 之後 resume 回來 seq／pos 照樣接續。
#[test]
fn ended_sessions_are_evicted_after_a_grace_period_and_keep_their_numbering() {
    let mut m = merger();
    m.handle(hook_at("UserPromptSubmit", 1000));
    let end = m.handle(hook_at("SessionEnd", 2000));
    let last_seq = end.last().unwrap().seq;
    assert_eq!(m.session_count(), 1);
    m.tick(2000 + SESSION_EVICT_AFTER_MS - 1);
    assert_eq!(m.session_count(), 1, "寬限期內留著（晚到的 transcript 行）");
    m.tick(2000 + SESSION_EVICT_AFTER_MS);
    assert_eq!(m.session_count(), 0);

    let back = m.handle(hook_at("SessionStart", 9_000_000));
    assert_eq!(back[0].seq, last_seq + 1, "移除後 resume 回來接著編號");
}

/// #670-12：resume 回來的 session 不會被當成已結束移除；完全沒動靜的 session 放久了也移除。
#[test]
fn resumed_sessions_stay_and_abandoned_ones_are_evicted() {
    let mut m = merger();
    m.handle(hook_at("SessionEnd", 1000));
    m.handle(hook_at("SessionStart", 2000));
    m.tick(1000 + SESSION_EVICT_AFTER_MS);
    assert_eq!(m.session_count(), 1);
    m.tick(2000 + SESSION_IDLE_EVICT_MS - 1);
    assert_eq!(m.session_count(), 1);
    m.tick(2000 + SESSION_IDLE_EVICT_MS);
    assert_eq!(
        m.session_count(),
        0,
        "沒有 SessionEnd（終端機被關掉）也不能永遠留著"
    );
}

// ── subagent 與 Agent 呼叫的配對（#680）

fn agent_call(id: &str, desc: &str, ts: u64) -> Input {
    pre(
        id,
        "Agent",
        json!({"subagent_type": "Explore", "description": desc, "prompt": desc}),
        ts,
    )
}

fn subagent_start(agent: &str, ts: u64) -> Input {
    Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SubagentStart",
                          "agent_id": agent, "agent_type": "Explore"})),
        ts_seen: ts,
    }
}

fn spawned(evs: &[Event]) -> Vec<(String, Value, Value)> {
    evs.iter()
        .filter(|e| e.kind == "agent.spawned")
        .map(|e| {
            (
                e.payload["agentId"].as_str().unwrap().to_string(),
                e.payload["name"].clone(),
                e.payload["parentToolUseId"].clone(),
            )
        })
        .collect()
}

/// 實測（2026-10-17，一則回覆平行呼叫三次 Agent，兩次都一樣）：每個呼叫的 PreToolUse 之後約 20 ms 就是
/// 它自己的 SubagentStart，下一個 PreToolUse 再隔約 800 ms；配對時待配對的只有最近一個呼叫。
#[test]
fn parallel_agent_calls_pair_in_the_measured_hook_order() {
    let mut m = merger();
    let mut evs = Vec::new();
    for (i, (id, desc, agent)) in [
        ("ta", "alpha", "x"),
        ("tb", "beta", "y"),
        ("tc", "gamma", "z"),
    ]
    .iter()
    .enumerate()
    {
        let t = 1_000 + i as u64 * 800;
        evs.extend(m.handle(agent_call(id, desc, t)));
        evs.extend(m.handle(subagent_start(agent, t + 20)));
    }
    assert_eq!(
        spawned(&evs),
        [
            ("x".into(), json!("alpha"), json!("ta")),
            ("y".into(), json!("beta"), json!("tb")),
            ("z".into(), json!("gamma"), json!("tc")),
        ]
    );
}

/// 失敗、沒有啟動 subagent 的 Agent 呼叫（例如不存在的 subagent_type）不能留在待配對清單裡被之後的 subagent 配到。
#[test]
fn failed_agent_call_is_not_paired_later() {
    let mut m = merger();
    m.handle(pre(
        "t1",
        "Agent",
        json!({"description": "lost", "prompt": "lost"}),
        1_000,
    ));
    m.handle(Input::Hook {
        hook: hook(
            json!({"session_id": S, "hook_event_name": "PostToolUseFailure",
                          "tool_name": "Agent", "tool_use_id": "t1",
                          "error": "Agent type 'nope' not found", "is_interrupt": false}),
        ),
        ts_seen: 1_100,
    });
    let evs = m.handle(subagent_start("x", 5_000));
    assert_eq!(spawned(&evs), [("x".into(), Value::Null, Value::Null)]);
}
