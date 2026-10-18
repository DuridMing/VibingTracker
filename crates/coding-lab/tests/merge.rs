//! merge 的行為測試（方案 C，docs/plan/PLAN.md「順序問題與解法」、#632）。

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::{arrivals, read_jsonl, sample_sessions};

use coding_lab::event::{Danger, DangerLevel, Event};
use coding_lab::hook::HookInput;
use coding_lab::merge::{
    AgentMetaRead, CARD_KEEP_MS, DangerRules, FALLBACK_AFTER_MS, FALLBACK_WAIT_MS, Input, Merger,
    SESSION_EVICT_AFTER_MS, SESSION_IDLE_EVICT_MS,
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
        tail: 0,
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
        tail: 0,
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

/// uuid 去重存成數字（E-M5）：標準格式的 uuid 只差一個字也不能當成同一行；不是標準格式的照樣去重。
#[test]
fn uuid_dedup_tells_close_uuids_apart_and_keeps_other_formats() {
    let mut m = merger();
    let a = "0f2d4c1e-7b9a-4e3f-9c8d-1a2b3c4d5e6f";
    let b = "0f2d4c1e-7b9a-4e3f-9c8d-1a2b3c4d5e60";
    assert_eq!(m.handle(line(1, thinking(a, "m1"), 1000)).len(), 1);
    assert_eq!(m.handle(line(2, thinking(b, "m1"), 1001)).len(), 1);
    assert!(m.handle(line(1, thinking(a, "m1"), 1002)).is_empty());
    let upper = a.to_uppercase();
    assert_eq!(
        m.handle(line(3, thinking(&upper, "m1"), 1003)).len(),
        1,
        "大小寫不同就是不同的字串，和以前一樣"
    );
    assert!(m.handle(line(3, thinking(&upper, "m1"), 1004)).is_empty());
}

/// 完成的卡片（已進串流、已結束、結果已送）留 [`CARD_KEEP_MS`] 給重複的 hook（K-3），之後釋放；
/// 沒完成的不動（K-12：長開的 session 不再一直累積卡片）。
#[test]
fn finished_cards_are_released_after_a_while() {
    let mut m = merger();
    m.handle(pre("t1", "Bash", json!({ "command": "ls" }), 1_000));
    m.handle(post("t1", 1_100));
    m.handle(line(1, tool_use("u1", "m1", "t1"), 1_200));
    m.handle(line(2, tool_result("u2", "t1"), 1_300));
    m.handle(line(3, tool_use("u3", "m2", "t2"), 1_400));
    assert_eq!(m.card_count(S), 2);
    m.tick(2_000);
    assert!(
        m.handle(post("t1", 2_100)).is_empty(),
        "保留期間重複的 hook 照樣不處理"
    );
    m.tick(2_000 + CARD_KEEP_MS / 2);
    assert_eq!(m.card_count(S), 2, "還在保留期間");
    m.tick(2_000 + CARD_KEEP_MS);
    assert_eq!(m.card_count(S), 1, "t1 釋放，還在跑的 t2 留著");
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

/// #710：session 開到一半才裝 plugin，永遠收不到 SessionStart。第一個 main agent 的 hook 帶的 cwd 就當路徑基準，
/// 並補一個 `session.started`（source late），store、檔案樹、專案回放才歸得到專案。
#[test]
fn late_session_takes_its_base_from_the_first_main_hook() {
    let mut m = merger();
    let a = m.handle(Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "UserPromptSubmit", "prompt": "修 bug", "prompt_id": "p1",
            "cwd": "/p", "transcript_path": "/t.jsonl",
        })),
        ts_seen: 1,
    });
    assert_eq!(types(&a)[..2], ["session.started", "prompt.submitted"]);
    assert_eq!(a[0].payload["cwd"], "/p");
    assert_eq!(a[0].payload["source"], "late");
    assert_eq!(a[0].payload["transcriptPath"], "/t.jsonl");
    assert_eq!(a[0].payload["model"], Value::Null);
    // 之後的 file.* 相對這個基準
    let b = m.handle(pre("t1", "Read", json!({"file_path": "/p/src/a.rs"}), 2));
    let read = b.iter().find(|e| e.kind == "file.read").unwrap();
    assert_eq!(read.payload["path"], "src/a.rs");
    // 第二個 main hook 不再補 session.started
    let c = m.handle(Input::Hook {
        hook: hook(json!({ "session_id": S, "hook_event_name": "UserPromptSubmit", "prompt": "再來", "cwd": "/p" })),
        ts_seen: 3,
    });
    assert!(!types(&c).contains(&"session.started"), "{:?}", types(&c));
    // 之後真的 SessionStart（resume）到了：照 #672 不換基準
    let d = m.handle(Input::Hook {
        hook: hook(json!({ "session_id": S, "hook_event_name": "SessionStart", "source": "resume", "cwd": "/q" })),
        ts_seen: 4,
    });
    assert_eq!(d[0].payload["source"], "resume");
    let e = m.handle(pre("t2", "Read", json!({"file_path": "/p/src/b.rs"}), 5));
    let read = e.iter().find(|e| e.kind == "file.read").unwrap();
    assert_eq!(read.payload["path"], "src/b.rs");
}

/// #710：subagent 與背景 agent 的 hook 不當基準（它們的 cwd 可能已經在 worktree 裡），等 main 的 hook。
#[test]
fn late_session_ignores_subagent_hooks_for_its_base() {
    let mut m = merger();
    let a = m.handle(agent_hook(
        "SubagentStart",
        "a1",
        Some("Explore"),
        json!({"cwd": "/wt", "transcript_path": "/t.jsonl"}),
    ));
    assert!(!types(&a).contains(&"session.started"), "{:?}", types(&a));
    let b = m.handle(agent_hook("PreToolUse", "bg1", Some(""), json!({"cwd": "/wt", "tool_name": "Read", "tool_input": {"file_path": "/wt/x"}, "tool_use_id": "u9"})));
    assert!(!types(&b).contains(&"session.started"), "{:?}", types(&b));
    let c = m.handle(Input::Hook {
        hook: hook(
            json!({ "session_id": S, "hook_event_name": "PreToolUse", "tool_name": "Read",
            "tool_input": {"file_path": "/p/a.rs"}, "tool_use_id": "t1", "cwd": "/p" }),
        ),
        ts_seen: 2,
    });
    assert_eq!(types(&c)[0], "session.started");
    assert_eq!(c[0].payload["cwd"], "/p");
    let read = c.iter().find(|e| e.kind == "file.read").unwrap();
    assert_eq!(read.payload["path"], "a.rs");
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
        tail: 0,
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
        tail: 0,
        session: S.into(),
        file: Some("agent-a1".into()),
        ts_seen: 1100,
    });
    assert_eq!(types(&a), ["transcript.recovered"]);
    assert_eq!(a[0].payload["file"], "agent-a1");
    let b = m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1200));
    assert_eq!(types(&b), ["tool.called", "tool.placed"], "main 還在降級");

    let c = m.handle(Input::TailReset {
        tail: 0,
        session: S.into(),
        file: None,
        ts_seen: 1300,
    });
    assert_eq!(types(&c), ["transcript.recovered"]);
    assert_eq!(c[0].payload["file"], "main");
    let d = m.handle(pre("t2", "Bash", json!({"command": "pwd"}), 1400));
    assert_eq!(types(&d), ["tool.called"], "重開 tail 後回到暫存");

    let e = m.handle(Input::TailReset {
        tail: 0,
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
        tail: 0,
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

/// 實測（2026-10-17，`claude -p` 一則回覆平行呼叫三次 Agent）：每個呼叫的 PreToolUse 之後約 20 ms 就是
/// 它自己的 SubagentStart，meta.json 再晚 40–60 ms；有候選時一律等 meta（Skill 的 fork 也會有 SubagentStart）。
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
        evs.extend(m.handle(agent_meta(agent, id, t + 70)));
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

/// Agent 工具的 PostToolUse：背景 agent 啟動時（`async_launched`）、前景 agent 結束時都帶 `tool_response.agentId`。
fn agent_post(id: &str, agent: &str, ts: u64) -> Input {
    Input::Hook {
        hook: hook(
            json!({"session_id": S, "hook_event_name": "PostToolUse", "tool_name": "Agent",
                          "tool_use_id": id, "tool_response": {"agentId": agent, "status": "async_launched"}}),
        ),
        ts_seen: ts,
    }
}

fn agent_meta(agent: &str, tool: &str, ts: u64) -> Input {
    meta_read(agent, AgentMetaRead::ToolUse(tool.into()), ts)
}

fn meta_read(agent: &str, meta: AgentMetaRead, ts: u64) -> Input {
    Input::AgentMeta {
        session: S.into(),
        agent_id: agent.into(),
        meta,
        ts_seen: ts,
    }
}

/// 背景 agent：PostToolUse（帶 agentId）和 SubagentStart 同一毫秒到、先後都有（samples dba6a637、cc94acff）。
/// 兩種順序都要配對到對的呼叫；Post 先到時不能把呼叫從待配對清單拿掉。
#[test]
fn background_agent_pairs_whether_post_or_start_comes_first() {
    let mut m = merger();
    let mut evs = m.handle(agent_call("t1", "py", 1_000));
    evs.extend(m.handle(agent_post("t1", "a1", 1_025)));
    evs.extend(m.handle(subagent_start("a1", 1_025)));
    evs.extend(m.handle(agent_call("t2", "md", 2_000)));
    evs.extend(m.handle(subagent_start("a2", 2_022)));
    evs.extend(m.handle(agent_post("t2", "a2", 2_022)));
    assert_eq!(
        spawned(&evs),
        [
            ("a1".into(), json!("py"), json!("t1")),
            ("a2".into(), json!("md"), json!("t2")),
        ]
    );
}

/// 兩個背景 agent，第二個呼叫在第一個啟動前就到（本機自我追蹤實錄的 hook 順序，2026-10-17）：
/// Pre T1 → Pre T2 → Post T1(A1) → Start A1 → Post T2(A2) → Start A2。
#[test]
fn background_agents_pair_by_agent_id_not_by_order() {
    let mut m = merger();
    let mut evs = Vec::new();
    for i in [
        agent_call("t1", "first", 179_256),
        agent_call("t2", "second", 189_856),
        agent_post("t1", "a1", 191_226),
        subagent_start("a1", 191_232),
        agent_post("t2", "a2", 191_238),
        subagent_start("a2", 191_238),
    ] {
        evs.extend(m.handle(i));
    }
    assert_eq!(
        spawned(&evs),
        [
            ("a1".into(), json!("first"), json!("t1")),
            ("a2".into(), json!("second"), json!("t2")),
        ]
    );
}

/// 前景平行派出：PreToolUse 全部先到，再來三個 SubagentStart。有好幾個候選時等 meta.json 的 toolUseId
/// （serve 在 SubagentStart 後 40–60 ms 讀到）再配對；meta 到達的順序和呼叫順序無關。
#[test]
fn parallel_foreground_agents_wait_for_meta() {
    let mut m = merger();
    let mut evs = Vec::new();
    for (i, (id, desc)) in [("ta", "alpha"), ("tb", "beta"), ("tc", "gamma")]
        .iter()
        .enumerate()
    {
        evs.extend(m.handle(agent_call(id, desc, 1_000 + i as u64)));
    }
    for (i, a) in ["x", "y", "z"].iter().enumerate() {
        evs.extend(m.handle(subagent_start(a, 1_100 + i as u64)));
    }
    evs.extend(m.handle(agent_meta("z", "tc", 1_150)));
    evs.extend(m.handle(agent_meta("x", "ta", 1_160)));
    evs.extend(m.handle(agent_meta("y", "tb", 1_170)));
    let mut got = spawned(&evs);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        [
            ("x".into(), json!("alpha"), json!("ta")),
            ("y".into(), json!("beta"), json!("tb")),
            ("z".into(), json!("gamma"), json!("tc")),
        ]
    );
}

/// 等不到 meta.json（舊版 Claude Code、讀檔失敗）：過了等待上限就依呼叫順序配對，不會一直不發 agent.spawned。
#[test]
fn ambiguous_start_falls_back_to_call_order_after_a_while() {
    let mut m = merger();
    let mut evs = Vec::new();
    for (i, (id, desc)) in [("ta", "alpha"), ("tb", "beta"), ("tc", "gamma")]
        .iter()
        .enumerate()
    {
        evs.extend(m.handle(agent_call(id, desc, 1_000 + i as u64)));
    }
    for (i, a) in ["x", "y", "z"].iter().enumerate() {
        evs.extend(m.handle(subagent_start(a, 1_100 + i as u64)));
    }
    evs.extend(m.tick(1_100 + 5_000));
    assert_eq!(
        spawned(&evs),
        [
            ("x".into(), json!("alpha"), json!("ta")),
            ("y".into(), json!("beta"), json!("tb")),
            ("z".into(), json!("gamma"), json!("tc")),
        ]
    );
}

fn subagent_typed(agent: &str, ty: &str, ts: u64) -> Input {
    Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SubagentStart",
                          "agent_id": agent, "agent_type": ty})),
        ts_seen: ts,
    }
}

fn untyped_call(id: &str, ts: u64) -> Input {
    pre(id, "Agent", json!({"description": id, "prompt": id}), ts)
}

/// Skill 開的 fork 也有 SubagentStart，但不是 Agent 呼叫開的（meta.json 沒有 toolUseId）：
/// 不能因為「只有一個候選」就配走真正 agent 的呼叫（9f 驗證，本機 12／51 個 meta.json 沒有 toolUseId）。
#[test]
fn skill_fork_does_not_steal_the_agent_call() {
    let mut m = merger();
    let mut evs = m.handle(untyped_call("ta", 1_000));
    evs.extend(m.handle(subagent_typed("fork", "general-purpose", 1_050)));
    evs.extend(m.handle(subagent_typed("a", "general-purpose", 1_060)));
    evs.extend(m.handle(meta_read("fork", AgentMetaRead::NoToolUse, 1_100)));
    evs.extend(m.handle(agent_meta("a", "ta", 1_110)));
    assert_eq!(
        spawned(&evs),
        [
            ("fork".into(), Value::Null, Value::Null),
            ("a".into(), json!("ta"), json!("ta")),
        ]
    );
}

/// 同一個 SubagentStart 收到兩次（plugin 和 settings 都掛了，samples 每個 hook 都兩份）：只發一次 spawn，
/// 第二筆不能留在等待清單、到期時搶走別的呼叫。
#[test]
fn duplicate_start_is_handled_once() {
    let mut m = merger();
    let mut evs = m.handle(agent_call("ta", "a", 1_000));
    evs.extend(m.handle(agent_call("tb", "b", 1_001)));
    evs.extend(m.handle(subagent_start("x", 1_100)));
    evs.extend(m.handle(subagent_start("x", 1_100)));
    evs.extend(m.handle(agent_meta("x", "ta", 1_150)));
    evs.extend(m.tick(3_000));
    evs.extend(m.handle(subagent_start("x", 3_050)));
    evs.extend(m.handle(subagent_start("y", 3_100)));
    evs.extend(m.handle(agent_meta("y", "tb", 3_150)));
    assert_eq!(
        spawned(&evs),
        [
            ("x".into(), json!("a"), json!("ta")),
            ("y".into(), json!("b"), json!("tb")),
        ]
    );
}

/// meta.json 讀不到（serve 送 Unreadable）：不必等滿上限，直接依呼叫順序配對。
#[test]
fn unreadable_meta_pairs_by_call_order_right_away() {
    let mut m = merger();
    let mut evs = m.handle(agent_call("ta", "a", 1_000));
    evs.extend(m.handle(agent_call("tb", "b", 1_001)));
    evs.extend(m.handle(subagent_start("x", 1_100)));
    evs.extend(m.handle(meta_read("x", AgentMetaRead::Unreadable, 2_100)));
    assert_eq!(spawned(&evs), [("x".into(), json!("a"), json!("ta"))]);
}

/// 延後發的 agent.spawned：等待期間它的工具事件已經開了 agents 列、而且 SubagentStop 已經到了。
/// spawn 要在 agent.finished 之前發出，live 的列合併、不歸零，最後是 done（9f 驗證）。
#[test]
fn delayed_spawn_keeps_the_agent_row_and_ends_done() {
    let mut m = merger();
    let mut evs = Vec::new();
    evs.extend(m.handle(agent_call("ta", "a", 1_000)));
    evs.extend(m.handle(agent_call("tb", "b", 1_001)));
    // 真實的 SubagentStart 帶契約沒用到的欄位（scratchpad_dir 等），會進 Event 的 raw
    evs.extend(m.handle(Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SubagentStart",
                          "agent_id": "x", "agent_type": "Explore", "scratchpad_dir": "/s"})),
        ts_seen: 1_100,
    }));
    evs.extend(m.handle(sub_pre("tx1", "x", 1_200)));
    evs.extend(m.handle(Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SubagentStop",
                          "agent_id": "x", "agent_type": "Explore"})),
        ts_seen: 1_300,
    }));
    evs.extend(m.tick(3_000));
    let order: Vec<&str> = evs
        .iter()
        .filter(|e| e.kind == "agent.spawned" || e.kind == "agent.finished")
        .map(|e| e.kind.as_str())
        .collect();
    assert_eq!(order, ["agent.spawned", "agent.finished"]);
    let spawn = evs.iter().find(|e| e.kind == "agent.spawned").unwrap();
    assert!(
        spawn.raw.is_some(),
        "延後發的 spawn 也帶 SubagentStart 的 raw"
    );
    let mut l = coding_lab::live::Live::new();
    for e in &evs {
        l.apply(e);
    }
    let snap = l.snapshot(&coding_lab::live::Channel::All, 0, 3_000);
    let row = snap["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == format!("{S}/x"))
        .unwrap()
        .clone();
    assert_eq!(
        (row["status"].as_str(), row["toolCount"].as_u64()),
        (Some("done"), Some(1)),
        "{row}"
    );
}

/// 背景：兩個候選，Start 比 Post 先到（9f 驗證的順序）。
#[test]
fn background_start_before_post_with_two_candidates() {
    let mut m = merger();
    let mut evs = Vec::new();
    for i in [
        agent_call("t1", "one", 1),
        agent_call("t2", "two", 2),
        subagent_start("a1", 10),
        agent_post("t1", "a1", 10),
        subagent_start("a2", 20),
        agent_post("t2", "a2", 20),
    ] {
        evs.extend(m.handle(i));
    }
    assert_eq!(
        spawned(&evs),
        [
            ("a1".into(), json!("one"), json!("t1")),
            ("a2".into(), json!("two"), json!("t2")),
        ]
    );
}

/// 等配對期間收到 SessionEnd（9f 整體驗證）：spawn 要在 session.ended 之前發；live 清掉這個 session 的
/// agents 列之後，不能又插回一列永遠是 run 的 agent。
#[test]
fn session_end_pairs_waiting_starts_first() {
    let mut m = merger();
    let mut evs = m.handle(agent_call("ta", "a", 1_000));
    evs.extend(m.handle(subagent_start("x", 1_020)));
    evs.extend(m.handle(Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SessionEnd", "reason": "other"})),
        ts_seen: 1_030,
    }));
    evs.extend(m.tick(3_000));
    let order: Vec<&str> = evs
        .iter()
        .map(|e| e.kind.as_str())
        .filter(|k| *k == "agent.spawned" || *k == "session.ended")
        .collect();
    assert_eq!(order, ["agent.spawned", "session.ended"]);
    let mut l = coding_lab::live::Live::new();
    for e in &evs {
        l.apply(e);
    }
    let snap = l.snapshot(&coding_lab::live::Channel::All, 0, 3_000);
    assert!(
        snap["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] != "run"),
        "{}",
        snap["agents"]
    );
}

/// 等配對期間收到 task-notification（背景 agent 被 TaskStop）：一樣先配對再發 agent.finished。
#[test]
fn task_notification_pairs_a_waiting_start_first() {
    let mut m = merger();
    let mut evs = m.handle(agent_call("ta", "a", 1_000));
    evs.extend(m.handle(agent_call("tb", "b", 1_001)));
    evs.extend(m.handle(subagent_start("x", 1_020)));
    evs.extend(m.handle(Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "UserPromptSubmit",
            "prompt": "<task-notification>\n<task-id>x</task-id>\n<status>killed</status>\n</task-notification>"})),
        ts_seen: 1_100,
    }));
    evs.extend(m.tick(3_000));
    let order: Vec<&str> = evs
        .iter()
        .map(|e| e.kind.as_str())
        .filter(|k| k.starts_with("agent."))
        .collect();
    assert_eq!(order, ["agent.spawned", "agent.finished"]);
}

/// meta 說 ToolUse(ta)，但 ta 已經被另一個讀不到 meta 的 agent 依順序配走：parent 仍然是確定的 ta，不能丟掉。
#[test]
fn known_parent_survives_a_call_taken_by_order() {
    let mut m = merger();
    let mut evs = m.handle(agent_call("ta", "a", 1_000));
    evs.extend(m.handle(agent_call("tb", "b", 1_001)));
    evs.extend(m.handle(subagent_start("x", 1_020)));
    evs.extend(m.handle(subagent_start("y", 1_021)));
    evs.extend(m.handle(meta_read("x", AgentMetaRead::Unreadable, 1_100)));
    evs.extend(m.handle(agent_meta("y", "ta", 1_110)));
    let got = spawned(&evs);
    assert_eq!(got[1].0, "y");
    assert_eq!(got[1].2, json!("ta"), "{got:?}");
}

/// #684 R20：TailReset 早於舊 tail 停下；舊 tail 最後一次讀到壞行送來的 Degraded（編號較舊）不能讓降級留著。
#[test]
fn degraded_from_a_replaced_tail_is_ignored() {
    let mut m = merger();
    m.handle(Input::TailReset {
        tail: 2,
        session: S.into(),
        file: Some("main".into()),
        ts_seen: 1000,
    });
    let stale = m.handle(Input::Degraded {
        tail: 1,
        session: S.into(),
        file: "main".into(),
        error: "invalid JSON".into(),
        ts_seen: 1100,
    });
    assert!(stale.is_empty(), "舊 tail 的降級不算：{stale:?}");
    let a = m.handle(pre("t1", "Bash", json!({"command": "ls"}), 1200));
    assert_eq!(types(&a), ["tool.called"], "沒有降級，照常暫存");

    let live = m.handle(Input::Degraded {
        tail: 2,
        session: S.into(),
        file: "main".into(),
        error: "invalid JSON".into(),
        ts_seen: 1300,
    });
    assert_eq!(
        types(&live)[0],
        "transcript.degraded",
        "目前的 tail 照常降級"
    );

    // 整個 session 的 tail 停掉之後，比停掉時的編號舊的都不算
    m.handle(Input::TailReset {
        tail: 9,
        session: S.into(),
        file: None,
        ts_seen: 1400,
    });
    let stale = m.handle(Input::Degraded {
        tail: 2,
        session: S.into(),
        file: "agent-a1".into(),
        error: "invalid JSON".into(),
        ts_seen: 1500,
    });
    assert!(stale.is_empty(), "{stale:?}");
}

/// #684 R33：Windows 的 transcript 路徑存進 Event 時一律是 `/`（不變式 11）。
#[test]
fn transcript_paths_are_stored_with_forward_slashes() {
    let mut m = merger();
    let a = m.handle(Input::Hook {
        hook: hook(json!({"session_id": S, "hook_event_name": "SessionStart", "source": "startup",
                          "cwd": "C:\\p", "transcript_path": "C:\\Users\\u\\.claude\\projects\\p\\s1.jsonl"})),
        ts_seen: 1000,
    });
    assert_eq!(
        a[0].payload["transcriptPath"],
        "C:/Users/u/.claude/projects/p/s1.jsonl"
    );
    m.handle(Input::Hook {
        hook: hook(
            json!({"session_id": S, "hook_event_name": "SubagentStart", "agent_id": "a1",
                          "agent_type": "Explore"}),
        ),
        ts_seen: 1100,
    });
    let b = m.handle(Input::Hook {
        hook: hook(
            json!({"session_id": S, "hook_event_name": "SubagentStop", "agent_id": "a1",
                          "agent_type": "Explore",
                          "agent_transcript_path": "C:\\Users\\u\\s1\\subagents\\agent-a1.jsonl"}),
        ),
        ts_seen: 1200,
    });
    let finished = b
        .iter()
        .find(|e| e.type_name() == "agent.finished")
        .unwrap();
    assert_eq!(
        finished.payload["transcriptPath"],
        "C:/Users/u/s1/subagents/agent-a1.jsonl"
    );
}

fn post_read(id: &str, output: &str, ts: u64) -> Input {
    Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PostToolUse", "tool_name": "Read",
            "tool_use_id": id, "tool_response": output,
        })),
        ts_seen: ts,
    }
}

/// #654：讀 `.env` 的 tool.result 標 sensitive（本機照存，遠端送出時剝除）；一般檔案不標。
#[test]
fn reading_secrets_marks_the_result_sensitive() {
    let mut m = merger();
    m.handle(pre("t1", "Read", json!({"file_path": "/p/.env"}), 1000));
    let a = m.handle(post_read("t1", "TOKEN=abc", 1100));
    let r = a.iter().find(|e| e.type_name() == "tool.result").unwrap();
    assert_eq!(r.payload["sensitive"], true);
    assert_eq!(r.payload["output"], "TOKEN=abc", "本機照存");

    m.handle(pre(
        "t2",
        "Read",
        json!({"file_path": "/p/src/main.rs"}),
        1200,
    ));
    let b = m.handle(post_read("t2", "fn main() {}", 1300));
    let r = b.iter().find(|e| e.type_name() == "tool.result").unwrap();
    assert!(
        r.payload.get("sensitive").is_none(),
        "沒命中時不輸出這個欄位"
    );
}

/// C8（#761）：敏感的 Write／Edit 在 tool.called、file.write、file.edit 也標 sensitive（hunk 的前後文是從實際檔案
/// 讀的，會帶出 .env 原有的秘密）；判斷和 tool.result 一樣；沒命中時不輸出這個欄位。
#[test]
fn writing_secrets_marks_the_call_and_the_file_event_sensitive() {
    let mut m = merger();
    let w = m.handle(pre(
        "t1",
        "Write",
        json!({"file_path": "/p/.env", "content": "TOKEN=abc\n"}),
        1000,
    ));
    let e = m.handle(pre(
        "t2",
        "Edit",
        json!({"file_path": "/p/.env", "old_string": "abc", "new_string": "xyz"}),
        1100,
    ));
    let n = m.handle(pre(
        "t3",
        "Write",
        json!({"file_path": "/p/src/a.rs", "content": "fn a() {}\n"}),
        1200,
    ));
    let of = |evs: &[Event], t: &str| {
        evs.iter()
            .find(|e| e.type_name() == t)
            .unwrap()
            .payload
            .clone()
    };
    assert_eq!(of(&w, "tool.called")["sensitive"], true);
    assert_eq!(of(&w, "file.write")["sensitive"], true);
    assert_eq!(of(&e, "tool.called")["sensitive"], true);
    assert_eq!(of(&e, "file.edit")["sensitive"], true);
    assert!(of(&n, "tool.called").get("sensitive").is_none());
    assert!(of(&n, "file.write").get("sensitive").is_none());
    // 本機照存
    assert_eq!(of(&w, "file.write")["content"], "TOKEN=abc\n");
}

/// C8：讀敏感檔的指令型工具，tool.called 也標（輸入裡的指令本身通常不是秘密，但和 tool.result 一致）。
#[test]
fn reading_secrets_with_a_command_marks_the_call_sensitive() {
    let mut m = merger();
    let a = m.handle(pre("t1", "Bash", json!({"command": "cat .env"}), 1000));
    let c = a.iter().find(|e| e.type_name() == "tool.called").unwrap();
    assert_eq!(c.payload["sensitive"], true);
}

/// #654（R22）：tool.called 的 input 裡過長的字串截到 8 KB，標 inputTruncated；短的不動。
#[test]
fn long_tool_input_strings_are_truncated() {
    let mut m = merger();
    let big = "a".repeat(20 * 1024);
    let a = m.handle(pre(
        "t1",
        "Write",
        json!({"file_path": "/p/a.txt", "content": big}),
        1000,
    ));
    let c = a.iter().find(|e| e.type_name() == "tool.called").unwrap();
    assert!(c.payload["input"]["content"].as_str().unwrap().len() <= 8 * 1024);
    assert_eq!(c.payload["input"]["file_path"], "/p/a.txt");
    assert_eq!(c.payload["inputTruncated"], true);

    let b = m.handle(pre("t2", "Bash", json!({"command": "ls"}), 1100));
    let c = b.iter().find(|e| e.type_name() == "tool.called").unwrap();
    assert!(c.payload.get("inputTruncated").is_none());
}

/// #654：output 截在 8 KB 時不把多位元組字切壞。
#[test]
fn output_truncation_keeps_utf8_intact() {
    let mut m = merger();
    m.handle(pre("t1", "Read", json!({"file_path": "/p/a.md"}), 1000));
    // 8 KB 的位置落在三位元組字的中間
    let text = format!("{}{}", "a".repeat(8 * 1024 - 1), "中".repeat(10));
    let a = m.handle(post_read("t1", &text, 1100));
    let r = a.iter().find(|e| e.type_name() == "tool.result").unwrap();
    let out = r.payload["output"].as_str().unwrap();
    assert!(out.len() <= 8 * 1024);
    assert!(out.ends_with('a'), "不留下半個字");
    assert_eq!(r.payload["truncated"], true);
}

/// K-8：PreToolUse 到了、transcript 的 tool_use 行還沒到時 collector 重啟。重啟前的暫存卡要種回 merge：
/// tool_use 行到了照常放進串流（不再發第二筆 tool.called，KPI 不重複），等不到就照常降級，不留幽靈卡。
#[test]
fn samples_restart_between_pre_tool_use_and_its_line() {
    let mut tried = 0;
    for dir in sample_sessions() {
        let Some(first) = common::sample_inputs(&dir) else {
            continue;
        };
        let sid = first.session.clone();
        // 重啟點：PreToolUse 剛處理完（它的 tool_use 行一定還沒到）；每個樣本最多 5 個
        let points: Vec<usize> = first
            .inputs
            .iter()
            .enumerate()
            .filter(|(_, (_, i))| {
                matches!(i, Input::Hook { hook, .. } if hook.event_name() == "PreToolUse")
            })
            .map(|(n, _)| n + 1)
            .take(5)
            .collect();
        for cut in points {
            let mut it = common::sample_inputs(&dir).unwrap().inputs.into_iter();
            let mut a = Merger::new(Box::new(common::NoDanger));
            let (mut before, mut last_ts) = (Vec::new(), 0);
            for (ts, input) in it.by_ref().take(cut) {
                before.extend(a.tick(ts));
                before.extend(a.handle(input));
                last_ts = ts;
            }
            // 重啟：和 serve 一樣，從 store 種回 seq／pos／路徑基準與最近的 Event
            let mut b = Merger::new(Box::new(common::NoDanger));
            let seq = before.iter().map(|e| e.seq).max().unwrap_or(0);
            let pos = before.iter().filter_map(|e| e.pos).max().unwrap_or(0);
            let cwd = before
                .iter()
                .find(|e| e.type_name() == "session.started")
                .and_then(|e| e.payload["cwd"].as_str())
                .unwrap_or_default()
                .to_string();
            b.resume(&sid, seq, pos, &cwd);
            b.resume_staged(&before, last_ts);
            let (mut after, mut end) = (Vec::new(), last_ts);
            for (ts, input) in it {
                after.extend(b.tick(ts));
                after.extend(b.handle(input));
                end = ts;
            }
            after.extend(b.tick(end + 60_000));
            let all: Vec<&Event> = before.iter().chain(&after).collect();
            let of = |t: &str, id: &str| {
                all.iter()
                    .filter(|e| e.type_name() == t && e.payload["toolUseId"] == id)
                    .count()
            };
            let called: Vec<&Event> = all
                .iter()
                .copied()
                .filter(|e| e.type_name() == "tool.called")
                .collect();
            for c in &called {
                let id = c.payload["toolUseId"].as_str().unwrap();
                assert_eq!(
                    of("tool.called", id),
                    1,
                    "{} 在第 {cut} 個輸入後重啟：{id} 發了兩次 tool.called",
                    dir.display()
                );
                if c.payload["placement"] == "staged" {
                    assert_eq!(
                        of("tool.placed", id),
                        1,
                        "{} 在第 {cut} 個輸入後重啟：暫存卡 {id} 沒有（或不只一次）放進串流",
                        dir.display()
                    );
                }
            }
            tried += 1;
        }
    }
    if tried == 0 {
        eprintln!("略過：沒有 samples");
    }
}

/// K-8：種回的暫存卡等不到 tool_use 行時，從重啟起算 FALLBACK_AFTER_MS 後照常降級，只放一次；
/// 已經放進串流的不種回；重啟後再收到同一個 PreToolUse 也不重發 tool.called。
#[test]
fn resumed_staged_card_falls_back_after_the_usual_wait() {
    let mut a = merger();
    let mut before = a.handle(pre("t1", "Bash", json!({"command": "ls"}), 1000));
    before.extend(a.handle(pre("t2", "Bash", json!({"command": "pwd"}), 1001)));
    before.extend(a.tick(1001 + FALLBACK_AFTER_MS)); // t1、t2 都降級了
    before.extend(a.handle(pre("t3", "Bash", json!({"command": "id"}), 20_000)));
    let placed = |evs: &[Event], id: &str| {
        evs.iter()
            .filter(|e| e.type_name() == "tool.placed" && e.payload["toolUseId"] == id)
            .count()
    };
    assert_eq!(placed(&before, "t1"), 1, "前提");
    assert_eq!(placed(&before, "t3"), 0, "前提");

    let mut b = merger();
    let seq = before.iter().map(|e| e.seq).max().unwrap();
    let pos = before.iter().filter_map(|e| e.pos).max().unwrap();
    b.resume(S, seq, pos, "/p");
    b.resume_staged(&before, 30_000);
    let mut after = b.handle(pre("t3", "Bash", json!({"command": "id"}), 30_001));
    assert!(
        after.is_empty(),
        "同一個 PreToolUse 不重發：{:?}",
        types(&after)
    );
    after.extend(b.tick(30_000 + FALLBACK_AFTER_MS - 1));
    assert_eq!(placed(&after, "t3"), 0, "從重啟起算還沒滿");
    after.extend(b.tick(30_000 + FALLBACK_AFTER_MS));
    after.extend(b.tick(30_000 + 3 * FALLBACK_AFTER_MS));
    assert_eq!(placed(&after, "t3"), 1);
    assert_eq!(
        placed(&after, "t1") + placed(&after, "t2"),
        0,
        "已放進串流的不種回"
    );
    let p = after
        .iter()
        .find(|e| e.type_name() == "tool.placed")
        .unwrap();
    assert_eq!(p.payload["placement"], "fallback");
    assert_eq!(p.pos, Some(pos + 1), "接著重啟前的位置");
}

/// C7（#744）：file.write 的預覽除了 200 行，也不超過 8 KB；一行 1.5 MB 的 bundle 不再整份進 JSONL、live 與 snapshot。
#[test]
fn write_preview_is_bounded_in_bytes_too() {
    let mut m = merger();
    let big = "x".repeat(1_500_000);
    let w = m.handle(pre(
        "w1",
        "Write",
        json!({"file_path": "/p/bundle.min.js", "content": big}),
        1000,
    ));
    let fw = w.iter().find(|e| e.type_name() == "file.write").unwrap();
    assert!(fw.payload["content"].as_str().unwrap().len() <= 8 * 1024);
    assert_eq!(fw.payload["truncated"], true);
    assert_eq!(fw.payload["lines"], 1, "全文行數照算");
    // 多位元組字元不切壞
    let wide = "中".repeat(10_000);
    let w = m.handle(pre(
        "w2",
        "Write",
        json!({"file_path": "/p/zh.txt", "content": wide}),
        1100,
    ));
    let fw = w.iter().find(|e| e.type_name() == "file.write").unwrap();
    let c = fw.payload["content"].as_str().unwrap();
    assert!(c.len() <= 8 * 1024 && c.chars().all(|ch| ch == '中'));
    // 小檔照舊
    let w = m.handle(pre(
        "w3",
        "Write",
        json!({"file_path": "/p/a.rs", "content": "fn a() {}\n"}),
        1200,
    ));
    let fw = w.iter().find(|e| e.type_name() == "file.write").unwrap();
    assert_eq!(fw.payload["content"], "fn a() {}\n");
    assert_eq!(fw.payload["truncated"], false);
}

fn post_with(id: &str, tool: &str, response: Value, ts: u64) -> Input {
    Input::Hook {
        hook: hook(json!({
            "session_id": S, "hook_event_name": "PostToolUse", "tool_name": tool,
            "tool_use_id": id, "tool_response": response,
        })),
        ts_seen: ts,
    }
}

/// E-M1：大的 tool_response（Edit 帶整個原檔、Read 帶全文）邊序列化邊截；結果和「整份序列化再截 8 KB」一樣。
#[test]
fn large_tool_response_is_cut_to_the_same_8_kb() {
    let mut m = merger();
    let resp =
        json!({"filePath": "/p/a.rs", "originalFile": "中".repeat(400_000), "structuredPatch": []});
    m.handle(pre(
        "e1",
        "Edit",
        json!({"file_path": "/p/a.rs", "old_string": "a", "new_string": "b"}),
        1000,
    ));
    let a = m.handle(post_with("e1", "Edit", resp.clone(), 1100));
    let r = a.iter().find(|e| e.type_name() == "tool.result").unwrap();
    let full = resp.to_string();
    let (want, _) = coding_lab::text::truncate_bytes(&full, 8 * 1024);
    assert_eq!(r.payload["output"], want);
    assert_eq!(r.payload["truncated"], true);

    // Bash：stdout 很長時 stderr 放不進去；短時兩者用換行接起來
    m.handle(pre("b1", "Bash", json!({"command": "x"}), 1200));
    let out = "o".repeat(9000);
    let a = m.handle(post_with(
        "b1",
        "Bash",
        json!({"stdout": out, "stderr": "boom"}),
        1300,
    ));
    let r = a.iter().find(|e| e.type_name() == "tool.result").unwrap();
    assert_eq!(r.payload["output"], "o".repeat(8 * 1024));
    assert_eq!(r.payload["truncated"], true);
    m.handle(pre("b2", "Bash", json!({"command": "x"}), 1400));
    let a = m.handle(post_with(
        "b2",
        "Bash",
        json!({"stdout": "out", "stderr": "err"}),
        1500,
    ));
    let r = a.iter().find(|e| e.type_name() == "tool.result").unwrap();
    assert_eq!(r.payload["output"], "out\nerr");
    assert_eq!(r.payload["truncated"], false);
}
