//! live：彙總列、KPI、暫存區、串流、frame 與 snapshot（contracts.md §6、#636）。

use coding_lab::event::Event;
use coding_lab::live::{Channel, Live};
use serde_json::{Value, json};

const H: u64 = 3_600_000;
/// 2026-10-16 00:00 UTC
const DAY: u64 = 1_792_108_800_000;

struct B {
    seq: std::collections::HashMap<String, u64>,
    pos: std::collections::HashMap<String, u64>,
}

impl B {
    fn new() -> B {
        B {
            seq: Default::default(),
            pos: Default::default(),
        }
    }
    fn ev(
        &mut self,
        s: &str,
        kind: &str,
        agent: &str,
        ts: u64,
        in_stream: bool,
        payload: Value,
    ) -> Event {
        let seq = self.seq.entry(s.into()).or_default();
        *seq += 1;
        let pos = in_stream.then(|| {
            let p = self.pos.entry(s.into()).or_default();
            *p += 1;
            *p
        });
        Event::from_json_line(
            &json!({"v": 1, "session": s, "seq": *seq, "type": kind, "agent": agent, "line": null,
                    "pos": pos, "ts_model": null, "ts_exec": null, "ts_seen": ts, "host": "local",
                    "payload": payload})
            .to_string(),
        )
        .unwrap()
    }
    fn started(&mut self, s: &str, ts: u64) -> Event {
        self.ev(
            s,
            "session.started",
            "main",
            ts,
            false,
            json!({"cwd": "/p", "source": "startup",
            "model": "claude-opus-5-5", "transcriptPath": "/t", "branch": "main"}),
        )
    }
    fn called(
        &mut self,
        s: &str,
        id: &str,
        tool: &str,
        input: Value,
        ts: u64,
        danger: Value,
    ) -> Event {
        self.ev(
            s,
            "tool.called",
            "main",
            ts,
            false,
            json!({"toolUseId": id, "tool": tool, "input": input,
            "status": "running", "placement": "staged", "danger": danger}),
        )
    }
    fn text(&mut self, s: &str, ts: u64) -> Event {
        self.ev(
            s,
            "assistant.text",
            "main",
            ts,
            true,
            json!({"text": "x", "messageId": "m", "model": "claude-opus-5-5"}),
        )
    }
}

fn apply_all(l: &mut Live, evs: &[Event]) {
    for e in evs {
        l.apply(e);
    }
}

fn rows<'a>(v: &'a Value, kind: &str) -> Vec<&'a Value> {
    v[kind]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

// ── sessions 列

#[test]
fn session_row_tracks_prompts_cost_and_end() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(&mut l, &[
        b.started("s1", DAY + H),
        b.ev("s1", "prompt.submitted", "main", DAY + H, false, json!({"text": "第一個", "promptId": "p1"})),
        b.ev("s1", "prompt.submitted", "main", DAY + H, false, json!({"text": "第二個", "promptId": "p2"})),
        b.ev("s1", "usage", "main", DAY + H, false, json!({"messageId": "m", "model": "claude-opus-5-5",
            "input": 1, "output": 1, "thinking": 40, "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": 0.5})),
        b.text("s1", DAY + H),
    ]);
    let f = l.flush(DAY + 2 * H).unwrap();
    let s = &f.rows_for(&Channel::All)["sessions"][0];
    assert_eq!(s["id"], "s1");
    assert_eq!(s["model"], "opus 5.5");
    assert_eq!(
        (s["firstPrompt"].as_str(), s["lastPrompt"].as_str()),
        (Some("第一個"), Some("第二個"))
    );
    assert_eq!(
        (s["costUsd"].as_f64(), s["thinkingTokens"].as_u64()),
        (Some(0.5), Some(40))
    );
    assert_eq!(
        (s["status"].as_str(), s["lastPos"].as_u64()),
        (Some("run"), Some(1))
    );

    l.apply(&b.ev(
        "s1",
        "session.ended",
        "main",
        DAY + 3 * H,
        false,
        json!({"reason": "other"}),
    ));
    let f = l.flush(DAY + 3 * H).unwrap();
    let rows = f.rows_for(&Channel::All);
    assert_eq!(rows["sessions"][0]["status"], "ended");
    assert!(
        rows["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["deleted"] == true),
        "結束時 agents 列刪除"
    );
}

// ── KPI

#[test]
fn kpi_counts_today_in_the_viewers_timezone() {
    let mut b = B::new();
    let mut l = Live::new();
    // 台北 +8：當地 10/16 00:00 = 10/15 16:00 UTC
    let midnight_tpe = DAY - 8 * H;
    apply_all(&mut l, &[
        b.started("old", midnight_tpe - H), // 當地昨天
        b.ev("old", "file.edit", "main", midnight_tpe - H, false, json!({"toolUseId": "e0", "path": "a", "hunk": null, "hunkSource": "none"})),
        b.started("s1", DAY),
        b.ev("s1", "file.edit", "main", DAY, false, json!({"toolUseId": "e1", "path": "a", "hunk": null, "hunkSource": "none"})),
        b.ev("s1", "file.write", "main", DAY, false, json!({"toolUseId": "w1", "path": "n", "isNew": true, "content": "", "truncated": false})),
        b.ev("s1", "file.write", "main", DAY, false, json!({"toolUseId": "w2", "path": "o", "isNew": false, "content": "", "truncated": false})),
        b.called("s1", "b1", "Bash", json!({"command": "rm -rf /"}), DAY, json!({"rule": "rm-rf", "level": "danger", "why": "x"})),
        b.called("s1", "b2", "Bash", json!({"command": "sudo ls"}), DAY, json!({"rule": "sudo", "level": "warn", "why": "x"})),
        b.ev("s1", "tool.updated", "main", DAY, false, json!({"toolUseId": "b2", "status": "failed", "durationMs": 1, "error": "x", "interrupted": false})),
        b.ev("s1", "git.op", "main", DAY, false, json!({"toolUseId": "g", "op": "commit", "branch": "main", "hash": "abc", "message": "m", "ok": true})),
        b.ev("s1", "agent.spawned", "main", DAY, false, json!({"agentId": "a1", "agentType": "Explore", "name": "n", "task": "t", "parentToolUseId": null})),
        b.ev("s1", "agent.spawned", "bg", DAY, false, json!({"agentId": "a2", "agentType": null, "name": null, "task": null, "parentToolUseId": null})),
        b.ev("s1", "usage", "main", DAY, false, json!({"messageId": "m", "model": "claude-opus-5-5", "input": 0, "output": 0,
            "thinking": 7, "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": 1.25})),
        b.ev("s1", "attention.raised", "main", DAY, false, json!({"kind": "danger", "ref": "b1", "reason": "x"})),
    ]);
    l.register_offset(480);
    let snap = l.snapshot(&Channel::All, 480, DAY + H);
    let k = &snap["kpi"];
    assert_eq!(k["sessions"], 1, "昨天的 session 不算");
    assert_eq!(k["edits"], 1);
    assert_eq!(k["writes"], 1, "只算新檔案");
    assert_eq!(k["commands"], 2);
    assert_eq!(k["failures"], 1);
    assert_eq!(k["dangers"], 1, "只算 danger 等級");
    assert_eq!(k["commits"], 1);
    assert_eq!(k["agents"], 1, "bg 不算");
    assert_eq!(k["thinkingTokens"], 7);
    assert_eq!(k["costUsd"], 1.25);
    assert_eq!(
        (k["live"].as_u64(), k["attention"].as_u64()),
        (Some(2), Some(1)),
        "live／attention 是當下狀態"
    );

    // UTC 的「今天」從 10/16 00:00 UTC 起：舊 session 也落在 10/15，不算
    assert_eq!(l.snapshot(&Channel::All, 0, DAY + H)["kpi"]["edits"], 1);
}

#[test]
fn kpi_resets_after_local_midnight_and_only_sends_changes() {
    let mut b = B::new();
    let mut l = Live::new();
    l.register_offset(0);
    apply_all(
        &mut l,
        &[
            b.started("s1", DAY + 23 * H),
            b.ev(
                "s1",
                "file.edit",
                "main",
                DAY + 23 * H,
                false,
                json!({"toolUseId": "e", "path": "a", "hunk": null, "hunkSource": "none"}),
            ),
        ],
    );
    let f = l.flush(DAY + 23 * H).unwrap();
    assert_eq!(f.kpi_for(0).unwrap()["edits"], 1);
    assert!(l.flush(DAY + 23 * H).is_none(), "沒有變化就沒有 frame");
    let f = l.flush(DAY + 25 * H).expect("跨過午夜要送新的 kpi");
    assert_eq!(f.kpi_for(0).unwrap()["edits"], 0);
}

// ── agents 列

#[test]
fn agent_rows_lifecycle() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(&mut l, &[
        b.started("s1", DAY),
        b.called("s1", "t1", "Bash", json!({"command": "cargo test"}), DAY, Value::Null),
        b.ev("s1", "agent.spawned", "main", DAY, false, json!({"agentId": "a1", "agentType": "Explore", "name": "找檔案", "task": "列出 md", "parentToolUseId": "t0"})),
        b.ev("s1", "agent.spawned", "bg", DAY, false, json!({"agentId": "a9", "agentType": null, "name": null, "task": null, "parentToolUseId": null})),
    ]);
    let snap = l.snapshot(&Channel::All, 0, DAY);
    let agents = rows(&snap, "agents");
    let main = agents.iter().find(|a| a["id"] == "s1/main").unwrap();
    assert_eq!(
        (main["current"].as_str(), main["toolCount"].as_u64()),
        (Some("$ cargo test"), Some(1))
    );
    let sub = agents.iter().find(|a| a["id"] == "s1/a1").unwrap();
    assert_eq!(
        (sub["name"].as_str(), sub["status"].as_str()),
        (Some("找檔案"), Some("run"))
    );
    let bg = agents.iter().find(|a| a["id"] == "s1/bg").unwrap();
    assert_eq!(bg["name"], "背景");
    assert_eq!(snap["sessions"][0]["agentsLive"], 1, "不含 main、bg");

    l.flush(DAY);
    l.apply(&b.ev(
        "s1",
        "agent.finished",
        "a1",
        DAY,
        false,
        json!({"agentId": "a1", "lastMessage": "ok", "transcriptPath": null}),
    ));
    l.apply(&b.ev(
        "s1",
        "agent.finished",
        "bg",
        DAY,
        false,
        json!({"agentId": "a9", "lastMessage": null, "transcriptPath": null}),
    ));
    let f = l.flush(DAY).unwrap();
    let agents = f.rows_for(&Channel::All)["agents"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        agents
            .iter()
            .any(|a| a["id"] == "s1/a1" && a["status"] == "done")
    );
    assert!(
        agents
            .iter()
            .any(|a| a["id"] == "s1/bg" && a["deleted"] == true),
        "bg 結束就刪"
    );
}

// ── attention 列

#[test]
fn attention_rows_raise_clear_ack() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(
        &mut l,
        &[
            b.started("s1", DAY),
            b.ev(
                "s1",
                "attention.raised",
                "main",
                DAY,
                false,
                json!({"kind": "wait", "ref": "t1", "reason": "AskUserQuestion"}),
            ),
            b.ev(
                "s1",
                "attention.raised",
                "main",
                DAY,
                false,
                json!({"kind": "danger", "ref": "t2", "reason": "rm"}),
            ),
        ],
    );
    let snap = l.snapshot(&Channel::All, 0, DAY);
    assert_eq!(rows(&snap, "attention").len(), 2);
    let mut ids: Vec<&str> = rows(&snap, "attention")
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        ["s1/danger/t2", "s1/wait/t1"],
        "排序在前端做（契約 §6）"
    );
    l.flush(DAY);

    l.apply(&b.ev(
        "s1",
        "attention.cleared",
        "main",
        DAY,
        false,
        json!({"kind": "wait", "ref": "t1", "reason": "answered"}),
    ));
    assert!(l.ack("s1", "danger", "t2"));
    assert!(!l.ack("s1", "danger", "nope"));
    let f = l.flush(DAY).unwrap();
    let att = f.rows_for(&Channel::All)["attention"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(att.len(), 2);
    assert!(att.iter().all(|a| a["deleted"] == true));
    assert_eq!(
        f.kpi_for(0).map(|k| k["attention"].clone()),
        None,
        "沒註冊的時區不送 kpi"
    );
}

// ── 暫存區、串流、改動紀錄

#[test]
fn staged_cards_move_to_stream_when_placed() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(&mut l, &[
        b.started("s1", DAY),
        b.called("s1", "t1", "Bash", json!({"command": "ls"}), DAY, Value::Null),
        b.ev("s1", "tool.updated", "main", DAY, false, json!({"toolUseId": "t1", "status": "done", "durationMs": 3, "error": null, "interrupted": null})),
    ]);
    let snap = l.snapshot(&Channel::All, 0, DAY);
    let staged: Vec<&str> = rows(&snap, "staged")
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(staged, ["tool.called", "tool.updated"]);

    l.apply(&b.ev(
        "s1",
        "tool.placed",
        "main",
        DAY + 1,
        true,
        json!({"toolUseId": "t1", "placement": "stream"}),
    ));
    let snap = l.snapshot(&Channel::All, 0, DAY + 1);
    assert!(rows(&snap, "staged").is_empty());
    assert_eq!(rows(&snap, "stream")[0]["type"], "tool.placed");
}

#[test]
fn all_channel_stream_is_ordered_by_ts_session_pos_and_capped() {
    let mut b = B::new();
    let mut l = Live::new();
    for i in 0..250 {
        l.apply(&b.text("s2", DAY + i));
        l.apply(&b.text("s1", DAY + i));
    }
    let snap = l.snapshot(&Channel::All, 0, DAY + 300);
    let stream = rows(&snap, "stream");
    assert_eq!(stream.len(), 400, "每個 session 最多 200 個");
    let keys: Vec<(u64, String, u64)> = stream
        .iter()
        .map(|e| {
            (
                e["ts_seen"].as_u64().unwrap(),
                e["session"].as_str().unwrap().to_string(),
                e["pos"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(
        keys, sorted,
        "(ts_seen, session, pos) 排序，同毫秒 s1 在 s2 前"
    );
    assert_eq!(keys[0].2, 51, "保留最後 200 個 pos");

    let one = l.snapshot(&Channel::Session("s1".into()), 0, DAY + 300);
    assert_eq!(rows(&one, "stream").len(), 250, "單一 session 最多 3000 個");
    assert_eq!(rows(&one, "sessions").len(), 1);
}

#[test]
fn changes_keep_the_latest_60() {
    let mut b = B::new();
    let mut l = Live::new();
    for i in 0..70 {
        l.apply(&b.ev(
            "s1",
            "file.edit",
            "main",
            DAY + i,
            false,
            json!({"toolUseId": format!("e{i}"), "path": "a", "hunk": null, "hunkSource": "none"}),
        ));
    }
    let snap = l.snapshot(&Channel::All, 0, DAY + 100);
    let changes = rows(&snap, "changes");
    assert_eq!(changes.len(), 60);
    assert_eq!(changes[0]["payload"]["toolUseId"], "e10", "舊到新");
}

// ── frame

#[test]
fn frame_coalesces_tool_updates_and_filters_by_channel() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(&mut l, &[b.started("s1", DAY), b.started("s2", DAY)]);
    l.flush(DAY);
    apply_all(&mut l, &[
        b.ev("s1", "tool.updated", "main", DAY, false, json!({"toolUseId": "t1", "status": "running", "durationMs": null, "error": null, "interrupted": null})),
        b.ev("s1", "tool.updated", "main", DAY, false, json!({"toolUseId": "t1", "status": "done", "durationMs": 9, "error": null, "interrupted": null})),
        b.text("s2", DAY),
    ]);
    let f = l.flush(DAY).unwrap();
    let all = f.events_for(&Channel::All);
    assert_eq!(all.len(), 2, "同一 toolUseId 的 tool.updated 只留最後一個");
    assert_eq!(all[0].payload["status"], "done");
    let only_s2 = f.events_for(&Channel::Session("s2".into()));
    assert_eq!(only_s2.len(), 1);
    assert_eq!(only_s2[0].session, "s2");
    let rows_s1 = f.rows_for(&Channel::Session("s1".into()));
    assert!(
        rows_s1["sessions"]
            .as_array()
            .is_none_or(|a| a.iter().all(|r| r["id"] == "s1"))
    );
}

#[test]
fn frames_can_be_resumed_within_history() {
    let mut b = B::new();
    let mut l = Live::new();
    l.apply(&b.started("s1", DAY));
    let first = l.flush(DAY).unwrap().no;
    for i in 0..5 {
        l.apply(&b.text("s1", DAY + i));
        l.flush(DAY + i);
    }
    let since = l.frames_since(first).unwrap();
    assert_eq!(since.len(), 5);
    assert!(since.windows(2).all(|w| w[0].no + 1 == w[1].no));
    assert!(l.frames_since(first + 5).unwrap().is_empty(), "已是最新");
    assert!(
        l.frames_since(first + 99).is_none(),
        "未來的編號不合理，回 snapshot"
    );
}
