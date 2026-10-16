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
    let k = f.kpi_for(0).unwrap();
    assert_eq!(k["edits"], 0);
    for key in ["live", "attention", "sessions", "costUsd", "models"] {
        assert!(
            k.get(key).is_some(),
            "跨午夜整份重算、送完整的 kpi：缺 {key}：{k}"
        );
    }
}

fn usage(b: &mut B, s: &str, ts: u64, model: &str, thinking: u64, cost: f64) -> Event {
    b.ev(
        s,
        "usage",
        "main",
        ts,
        false,
        json!({"messageId": "m", "model": model, "input": 0, "output": 0, "thinking": thinking,
               "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": cost}),
    )
}

/// 決定紀錄 #12：費用與思考 tokens 依模型分列，各鍵加總等於總數；不認得的模型 costUsd 0 照樣列。
#[test]
fn kpi_models_split_cost_and_thinking_by_model() {
    let mut b = B::new();
    let mut l = Live::new();
    l.register_offset(0);
    let evs = [
        b.started("s1", DAY + H),
        usage(&mut b, "s1", DAY + H, "claude-opus-5-5", 7, 1.25),
        usage(&mut b, "s1", DAY + H, "claude-opus-5-5[1m]", 1, 0.25),
        usage(&mut b, "s1", DAY + H, "claude-sonnet-5-5", 3, 0.5),
        usage(&mut b, "s1", DAY + H, "claude-foo-9", 2, 0.0),
        usage(&mut b, "old", DAY - H, "claude-haiku-5-5", 100, 9.0), // 昨天
    ];
    apply_all(&mut l, &evs);
    let k = l.snapshot(&Channel::All, 0, DAY + 2 * H)["kpi"].clone();
    assert_eq!(
        k["models"],
        json!({
            "opus 5.5": {"costUsd": 1.5, "thinkingTokens": 8, "ids": ["claude-opus-5-5", "claude-opus-5-5[1m]"]},
            "sonnet 5.5": {"costUsd": 0.5, "thinkingTokens": 3, "ids": ["claude-sonnet-5-5"]},
            "foo 9": {"costUsd": 0.0, "thinkingTokens": 2, "ids": ["claude-foo-9"]},
        }),
        "{k}"
    );
    let m = k["models"].as_object().unwrap();
    let cost: f64 = m.values().map(|v| v["costUsd"].as_f64().unwrap()).sum();
    let think: u64 = m
        .values()
        .map(|v| v["thinkingTokens"].as_u64().unwrap())
        .sum();
    assert_eq!(
        (cost, think),
        (
            k["costUsd"].as_f64().unwrap(),
            k["thinkingTokens"].as_u64().unwrap()
        )
    );

    // 有一個模型變了就送整個 models（取代，不合併）
    l.flush(DAY + 2 * H);
    l.apply(&usage(
        &mut b,
        "s1",
        DAY + 2 * H,
        "claude-sonnet-5-5",
        1,
        0.25,
    ));
    let f = l.flush(DAY + 2 * H).unwrap();
    let models = f.kpi_for(0).unwrap()["models"].clone();
    assert_eq!(models.as_object().unwrap().len(), 3, "{models}");
    assert_eq!(
        models["sonnet 5.5"],
        json!({"costUsd": 0.75, "thinkingTokens": 4, "ids": ["claude-sonnet-5-5"]})
    );
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

/// 分數由 merge 每 10 秒交來；變了才送該 session 列（契約 §6）。
#[test]
fn session_score_is_sent_only_when_changed() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(&mut l, &[b.started("s1", DAY), b.started("s2", DAY)]);
    l.flush(DAY);

    l.set_scores(&[("s1".to_string(), 56), ("s2".to_string(), 0)]);
    let f = l.flush(DAY + 1).expect("s1 分數變了要送");
    let sent = f.rows_for(&Channel::All)["sessions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(sent.len(), 1, "s2 沒變，不送");
    assert_eq!(
        (sent[0]["id"].as_str(), sent[0]["score"].as_i64()),
        (Some("s1"), Some(56))
    );
    assert_eq!(l.session_row("s1").unwrap()["score"], 56);

    l.set_scores(&[("s1".to_string(), 56), ("s2".to_string(), 0)]);
    assert!(l.flush(DAY + 2).is_none(), "都沒變就沒有 frame");

    l.set_scores(&[("nope".to_string(), 9)]);
    assert!(l.flush(DAY + 3).is_none(), "不認得的 session 略過");
}

/// `doing`：只看這一輪、只看 main；跟著 sessions 列送（#648，契約 §6）。
#[test]
fn session_doing_follows_the_round_and_ignores_subagents() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(&mut l, &[b.started("s1", DAY)]);
    assert_eq!(l.session_row("s1").unwrap()["doing"], Value::Null);
    l.flush(DAY);

    let doing = |l: &Live| l.session_row("s1").unwrap()["doing"].clone();
    l.apply(&b.ev(
        "s1",
        "prompt.submitted",
        "main",
        DAY,
        true,
        json!({"text": "修好 sub", "promptId": "p1"}),
    ));
    assert_eq!(doing(&l), "修好 sub");
    l.apply(&b.called(
        "s1",
        "t1",
        "Bash",
        json!({"command": "cargo test"}),
        DAY,
        Value::Null,
    ));
    assert_eq!(doing(&l), "$ cargo test");
    l.flush(DAY + 1);

    l.apply(&b.ev(
        "s1",
        "assistant.text",
        "a1",
        DAY,
        true,
        json!({"text": "subagent 的話。", "messageId": "m0", "model": null}),
    ));
    assert_eq!(doing(&l), "$ cargo test", "subagent 的文字不算");
    l.flush(DAY + 2);

    l.apply(&b.ev(
        "s1",
        "assistant.text",
        "main",
        DAY,
        true,
        json!({"text": "測試通過。接下來改 sub。", "messageId": "m1", "model": null}),
    ));
    assert_eq!(doing(&l), "測試通過。");
    let f = l.flush(DAY + 3).unwrap();
    assert_eq!(
        f.rows_for(&Channel::All)["sessions"][0]["doing"],
        "測試通過。"
    );
}

/// agent.finished 帶通知的 status：非 completed／killed 的列標 fail（#677）。
#[test]
fn finished_agent_row_reflects_notification_status() {
    let mut b = B::new();
    let mut l = Live::new();
    let spawn = |b: &mut B, id: &str| {
        b.ev("s1", "agent.spawned", "main", DAY, false,
            json!({"agentId": id, "agentType": "general-purpose", "name": id, "task": null, "parentToolUseId": null}))
    };
    let fin =
        |b: &mut B, id: &str, status: &str| {
            b.ev("s1", "agent.finished", "main", DAY + 1, false,
            json!({"agentId": id, "lastMessage": null, "transcriptPath": null, "status": status}))
        };
    apply_all(
        &mut l,
        &[
            b.started("s1", DAY),
            spawn(&mut b, "a1"),
            spawn(&mut b, "a2"),
            spawn(&mut b, "a3"),
            fin(&mut b, "a1", "killed"),
            fin(&mut b, "a2", "failed"),
            fin(&mut b, "a3", "completed"),
        ],
    );
    let snap = l.snapshot(&Channel::All, 0, DAY + 1);
    let status = |id: &str| {
        rows(&snap, "agents")
            .iter()
            .find(|a| a["agentId"] == id)
            .map(|a| a["status"].clone())
    };
    assert_eq!(status("a1"), Some(json!("done")), "killed 不算失敗");
    assert_eq!(status("a2"), Some(json!("fail")));
    assert_eq!(status("a3"), Some(json!("done")));
    assert_eq!(l.session_row("s1").unwrap()["agentsLive"], 0);
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

/// danger 解除（工具結束）後列留到 ack，帶 `clearedAt`；其他種類解除就刪（契約 §6）。
#[test]
fn danger_row_stays_until_ack_after_clear() {
    let mut b = B::new();
    let mut l = Live::new();
    let raise = |b: &mut B, kind: &str, r: &str| {
        b.ev(
            "s1",
            "attention.raised",
            "main",
            DAY,
            false,
            json!({"kind": kind, "ref": r, "reason": "x"}),
        )
    };
    let clear = |b: &mut B, kind: &str, r: &str| {
        b.ev(
            "s1",
            "attention.cleared",
            "main",
            DAY + 5,
            false,
            json!({"kind": kind, "ref": r, "reason": "done"}),
        )
    };
    apply_all(
        &mut l,
        &[
            b.started("s1", DAY),
            raise(&mut b, "danger", "t1"),
            raise(&mut b, "fail", "t2"),
        ],
    );
    l.register_offset(0);
    let snap = l.snapshot(&Channel::All, 0, DAY);
    assert!(
        rows(&snap, "attention")
            .iter()
            .all(|a| a["clearedAt"].is_null())
    );
    l.flush(DAY);

    apply_all(
        &mut l,
        &[clear(&mut b, "danger", "t1"), clear(&mut b, "fail", "t2")],
    );
    let f = l.flush(DAY + 5).unwrap();
    let mut att: Vec<Value> = f.rows_for(&Channel::All)["attention"]
        .as_array()
        .unwrap()
        .clone();
    att.sort_by_key(|a| a["id"].as_str().unwrap().to_string());
    assert_eq!(att.len(), 2);
    assert_eq!(att[0]["id"], "s1/danger/t1");
    assert_eq!(att[0]["clearedAt"], DAY + 5, "danger 解除後更新列，不刪");
    assert!(att[0].get("deleted").is_none());
    assert_eq!(att[1]["id"], "s1/fail/t2");
    assert_eq!(att[1]["deleted"], true, "其他種類解除就刪");

    let snap = l.snapshot(&Channel::All, 0, DAY + 5);
    assert_eq!(
        rows(&snap, "attention").len(),
        1,
        "snapshot 也留著解除的 danger"
    );
    assert_eq!(snap["kpi"]["attention"], 1, "KPI 與佇列列數一致");

    assert!(l.ack("s1", "danger", "t1"));
    let f = l.flush(DAY + 6).unwrap();
    assert_eq!(f.rows_for(&Channel::All)["attention"][0]["deleted"], true);
    assert!(rows(&l.snapshot(&Channel::All, 0, DAY + 6), "attention").is_empty());
}

/// 重啟重建：ack 不是 Event，重放後要依 store 的佇列去掉已確認的列（含解除後留著的 danger）。
#[test]
fn rebuilt_attention_drops_rows_acked_before_restart() {
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
                json!({"kind": "danger", "ref": "t1", "reason": "x"}),
            ),
            b.ev(
                "s1",
                "attention.cleared",
                "main",
                DAY,
                false,
                json!({"kind": "danger", "ref": "t1", "reason": "done"}),
            ),
            b.ev(
                "s1",
                "attention.raised",
                "main",
                DAY,
                false,
                json!({"kind": "danger", "ref": "t2", "reason": "x"}),
            ),
            b.ev(
                "s1",
                "attention.raised",
                "main",
                DAY,
                false,
                json!({"kind": "done", "ref": "p1", "reason": "x"}),
            ),
        ],
    );
    // store 說還在佇列的只有 t2（t1、p1 重啟前已經 ack）
    l.keep_attention(&["s1/danger/t2".to_string()].into_iter().collect());
    let snap = l.snapshot(&Channel::All, 0, DAY);
    let ids: Vec<&str> = rows(&snap, "attention")
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["s1/danger/t2"]);
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
    let types: Vec<&str> = rows(&snap, "stream")
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"tool.placed"), "{types:?}");
    assert!(
        types.contains(&"tool.called"),
        "放進串流的卡片附上它的 tool.called：{types:?}"
    );
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

// ── 串流的伴隨 Event 與狀態行（與 #639 前端協調，contracts.md §6）

#[test]
fn stream_carries_card_companions_and_status_lines() {
    let mut b = B::new();
    let mut l = Live::new();
    let t = |v: &str| json!({"toolUseId": "t1", "status": v, "durationMs": null, "error": null, "interrupted": null});
    apply_all(&mut l, &[
        b.started("s1", DAY),
        b.ev("s1", "prompt.submitted", "main", DAY + 1, false, json!({"text": "修 bug", "promptId": "p1"})),
        b.called("s1", "t1", "Edit", json!({"file_path": "/p/a.rs"}), DAY + 2, Value::Null),
        b.ev("s1", "tool.updated", "main", DAY + 3, false, t("running")),
        b.ev("s1", "file.edit", "main", DAY + 3, false, json!({"toolUseId": "t1", "path": "a.rs", "hunk": null, "hunkSource": "none"})),
        b.ev("s1", "tool.updated", "main", DAY + 4, false, t("done")),
        b.ev("s1", "tool.result", "main", DAY + 5, false, json!({"toolUseId": "t1", "ok": true, "exitCode": null, "output": "", "truncated": false})),
        b.ev("s1", "tool.placed", "main", DAY + 6, true, json!({"toolUseId": "t1", "placement": "stream"})),
        b.ev("s1", "session.status", "main", DAY + 7, false, json!({"status": "wait", "since": DAY + 7, "score": 1.0})),
        b.ev("s1", "agent.spawned", "main", DAY + 8, false, json!({"agentId": "a1", "agentType": "Explore", "name": "n", "task": null, "parentToolUseId": "t1"})),
    ]);
    for ch in [Channel::All, Channel::Session("s1".into())] {
        let snap = l.snapshot(&ch, 0, DAY + 10);
        let stream = rows(&snap, "stream");
        let types: Vec<&str> = stream.iter().map(|e| e["type"].as_str().unwrap()).collect();
        for want in [
            "prompt.submitted",
            "tool.called",
            "tool.result",
            "file.edit",
            "tool.placed",
            "session.status",
            "agent.spawned",
        ] {
            assert!(types.contains(&want), "{ch:?} 缺 {want}：{types:?}");
        }
        let updates: Vec<&&Value> = stream
            .iter()
            .filter(|e| e["type"] == "tool.updated")
            .collect();
        assert_eq!(updates.len(), 1, "只附最新一筆 tool.updated");
        assert_eq!(updates[0]["payload"]["status"], "done");
        // 伴隨 Event 跟著卡片（依 seq），狀態行排在 ts_seen 不晚於它的卡片之後（contracts §6）
        let at = |t: &str| types.iter().position(|x| *x == t).unwrap();
        assert!(at("prompt.submitted") < at("tool.called"));
        assert!(at("tool.called") < at("tool.placed"));
        assert!(at("agent.spawned") < at("session.status"), "{types:?}");
    }
}

#[test]
fn status_lines_outside_the_window_are_dropped() {
    let mut b = B::new();
    let mut l = Live::new();
    l.apply(&b.ev(
        "s1",
        "prompt.submitted",
        "main",
        DAY,
        false,
        json!({"text": "很早的 prompt", "promptId": "p0"}),
    ));
    for i in 0..250 {
        l.apply(&b.text("s1", DAY + 10 + i));
    }
    l.apply(&b.ev(
        "s1",
        "prompt.submitted",
        "main",
        DAY + 1000,
        false,
        json!({"text": "新的 prompt", "promptId": "p1"}),
    ));
    let snap = l.snapshot(&Channel::All, 0, DAY + 2000);
    let prompts: Vec<&str> = rows(&snap, "stream")
        .iter()
        .filter(|e| e["type"] == "prompt.submitted")
        .map(|e| e["payload"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        prompts,
        ["新的 prompt"],
        "窗口（最後 200 個 pos）之前的狀態行不送"
    );
    // 單一 session 頻道的窗口是 3000，從 pos 1 開始，全部都送
    let one = l.snapshot(&Channel::Session("s1".into()), 0, DAY + 2000);
    assert_eq!(
        rows(&one, "stream")
            .iter()
            .filter(|e| e["type"] == "prompt.submitted")
            .count(),
        2
    );
}

/// subagent 結束也是狀態行（#641）：重新整理（snapshot）後仍在窗口範圍內就送，窗口之前的不送。
#[test]
fn agent_finished_is_a_status_line_within_the_window() {
    let mut b = B::new();
    let mut l = Live::new();
    let fin = |b: &mut B, id: &str, ts: u64| {
        b.ev(
            "s1",
            "agent.finished",
            "main",
            ts,
            false,
            json!({"agentId": id, "lastMessage": null, "transcriptPath": null}),
        )
    };
    l.apply(&fin(&mut b, "early", DAY));
    for i in 0..250 {
        l.apply(&b.text("s1", DAY + 10 + i));
    }
    l.apply(&fin(&mut b, "late", DAY + 1000));
    let ids = |snap: &Value| -> Vec<String> {
        rows(snap, "stream")
            .iter()
            .filter(|e| e["type"] == "agent.finished")
            .map(|e| e["payload"]["agentId"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(&l.snapshot(&Channel::All, 0, DAY + 2000)),
        ["late"],
        "窗口之前的不送"
    );
    assert_eq!(
        ids(&l.snapshot(&Channel::Session("s1".into()), 0, DAY + 2000)),
        ["early", "late"],
        "session 頻道從 pos 1 開始，全部都送"
    );
}

/// 「載入更多」走同一個 assemble_stream：窗口範圍內的 agent.finished 要帶上。
#[test]
fn more_carries_agent_finished_in_range() {
    let mut b = B::new();
    let window: Vec<Event> = (0..3).map(|i| b.text("s1", DAY + i * 10)).collect();
    let pool = vec![b.ev(
        "s1",
        "agent.finished",
        "main",
        DAY + 15,
        false,
        json!({"agentId": "a1", "lastMessage": null, "transcriptPath": null}),
    )];
    let out = coding_lab::live::assemble_stream(window, pool, Some(DAY + 100));
    assert!(out.iter().any(|e| e.kind == "agent.finished"));
}

/// #672：之後的 session.started（compact 等）不換 sessions 列的 cwd（路徑基準）。
#[test]
fn later_session_started_keeps_the_first_cwd() {
    let mut b = B::new();
    let mut live = Live::new();
    live.apply(&b.started("s1", DAY));
    live.apply(&b.ev(
        "s1",
        "session.started",
        "main",
        DAY + 1,
        false,
        json!({"cwd": "/p/worktrees/x", "source": "compact",
               "model": "claude-opus-5-5", "transcriptPath": "/t", "branch": "main"}),
    ));
    assert_eq!(live.session_row("s1").unwrap()["cwd"], "/p");
}

/// 決定紀錄 #14：agents 列帶各 agent 實際用的模型短名；subagent 可以和 main 不同，中途切換就更新。
#[test]
fn agent_rows_carry_their_own_model() {
    let mut b = B::new();
    let mut l = Live::new();
    let line = |b: &mut B, agent: &str, kind: &str, model: &str| {
        b.ev(
            "s1",
            kind,
            agent,
            DAY,
            false,
            json!({"text": "x", "messageId": "m", "model": model}),
        )
    };
    apply_all(&mut l, &[
        b.started("s1", DAY),
        b.ev("s1", "agent.spawned", "main", DAY, false, json!({"agentId": "a1", "agentType": "Explore", "name": "找檔案", "task": "t", "parentToolUseId": "t0"})),
    ]);
    let model_of = |l: &Live, id: &str| {
        let snap = l.snapshot(&Channel::All, 0, DAY);
        rows(&snap, "agents")
            .iter()
            .find(|a| a["id"] == id)
            .map(|a| a["model"].clone())
            .unwrap()
    };
    assert!(model_of(&l, "s1/main").is_null(), "還沒看到 assistant 行");
    assert!(model_of(&l, "s1/a1").is_null());

    apply_all(
        &mut l,
        &[
            line(&mut b, "main", "assistant.text", "claude-opus-5-5"),
            line(&mut b, "a1", "assistant.thinking", "claude-haiku-5-5"),
        ],
    );
    assert_eq!(model_of(&l, "s1/main"), "opus 5.5");
    assert_eq!(model_of(&l, "s1/a1"), "haiku 5.5", "subagent 用自己的模型");

    l.flush(DAY);
    l.apply(&line(
        &mut b,
        "main",
        "assistant.text",
        "claude-sonnet-5-5[1m]",
    ));
    let f = l.flush(DAY).unwrap();
    let agents = f.rows_for(&Channel::All)["agents"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        agents
            .iter()
            .any(|a| a["id"] == "s1/main" && a["model"] == "sonnet 5.5"),
        "中途切換要送更新：{agents:?}"
    );
    l.apply(&line(
        &mut b,
        "main",
        "assistant.text",
        "claude-sonnet-5-5[1m]",
    ));
    let f = l.flush(DAY);
    assert!(
        f.is_none_or(|f| f.rows_for(&Channel::All).get("agents").is_none()),
        "模型沒變不送 agents 列（列帶完整 id，比的是完整 id，#643）"
    );
}

/// #673：VSCode 與 resume 的 SessionStart 不送 model，session 主模型取 main 第一行帶真模型的 assistant 行。
#[test]
fn session_model_falls_back_to_the_first_main_assistant_line() {
    let mut b = B::new();
    let mut l = Live::new();
    let line = |b: &mut B, agent: &str, model: &str| {
        b.ev(
            "s1",
            "assistant.text",
            agent,
            DAY,
            true,
            json!({"text": "x", "messageId": "m", "model": model}),
        )
    };
    let model_of = |l: &Live| l.session_row("s1").unwrap()["model"].clone();
    l.apply(&b.ev(
        "s1",
        "session.started",
        "main",
        DAY,
        false,
        json!({"cwd": "/p", "source": "startup", "model": null, "transcriptPath": "/t", "branch": null}),
    ));
    assert!(model_of(&l).is_null());

    l.apply(&line(&mut b, "a1", "claude-haiku-5-5"));
    assert!(model_of(&l).is_null(), "subagent 的行不算 session 主模型");
    l.apply(&line(&mut b, "main", "<synthetic>"));
    assert!(model_of(&l).is_null(), "<synthetic> 不是模型");

    l.apply(&line(&mut b, "main", "claude-opus-5-5"));
    assert_eq!(model_of(&l), "opus 5.5");
    l.apply(&line(&mut b, "main", "claude-sonnet-5-5"));
    assert_eq!(
        model_of(&l),
        "opus 5.5",
        "之後的行不換，切換走 model.changed"
    );

    l.apply(&b.ev(
        "s1",
        "model.changed",
        "main",
        DAY,
        true,
        json!({"from": "claude-opus-5-5", "to": "claude-sonnet-5-5"}),
    ));
    assert_eq!(model_of(&l), "sonnet 5.5");
}

/// #673：SessionStart 有送 model 時沿用，不被 transcript 第一行換掉；agents 列也不收 `<synthetic>`。
#[test]
fn session_start_model_wins_and_synthetic_is_ignored() {
    let mut b = B::new();
    let mut l = Live::new();
    l.apply(&b.started("s1", DAY));
    for model in ["<synthetic>", "claude-sonnet-5-5"] {
        l.apply(&b.ev(
            "s1",
            "assistant.text",
            "main",
            DAY,
            true,
            json!({"text": "x", "messageId": "m", "model": model}),
        ));
        if model == "<synthetic>" {
            let snap = l.snapshot(&Channel::All, 0, DAY);
            let main = rows(&snap, "agents")
                .into_iter()
                .find(|a| a["id"] == "s1/main")
                .unwrap()
                .clone();
            assert!(main["model"].is_null(), "{main}");
        }
    }
    assert_eq!(l.session_row("s1").unwrap()["model"], "opus 5.5");
}

/// #643：短名顯示、完整 id 放 tooltip。sessions 與 agents 列帶 `modelId`，跟著 `model` 一起換。
#[test]
fn rows_carry_the_full_model_id_next_to_the_short_name() {
    let mut b = B::new();
    let mut l = Live::new();
    let line = |b: &mut B, agent: &str, model: &str| {
        b.ev(
            "s1",
            "assistant.text",
            agent,
            DAY,
            false,
            json!({"text": "x", "messageId": "m", "model": model}),
        )
    };
    apply_all(&mut l, &[
        b.started("s1", DAY),
        b.ev("s1", "agent.spawned", "main", DAY, false, json!({"agentId": "a1", "agentType": "Explore", "name": "找檔案", "task": "t", "parentToolUseId": "t0"})),
        line(&mut b, "main", "claude-opus-5-5"),
        line(&mut b, "a1", "claude-haiku-5-5"),
    ]);
    let ids = |l: &Live| {
        let snap = l.snapshot(&Channel::All, 0, DAY);
        let agent = |id: &str| {
            rows(&snap, "agents")
                .iter()
                .find(|a| a["id"] == id)
                .map(|a| (a["model"].clone(), a["modelId"].clone()))
                .unwrap()
        };
        let s = &rows(&snap, "sessions")[0];
        (
            (s["model"].clone(), s["modelId"].clone()),
            agent("s1/main"),
            agent("s1/a1"),
        )
    };
    assert_eq!(
        ids(&l),
        (
            (json!("opus 5.5"), json!("claude-opus-5-5")),
            (json!("opus 5.5"), json!("claude-opus-5-5")),
            (json!("haiku 5.5"), json!("claude-haiku-5-5")),
        )
    );

    l.apply(&b.ev(
        "s1",
        "model.changed",
        "main",
        DAY,
        true,
        json!({"from": "claude-opus-5-5", "to": "claude-sonnet-5-5[1m]"}),
    ));
    l.apply(&line(&mut b, "main", "claude-sonnet-5-5[1m]"));
    assert_eq!(
        ids(&l).0,
        (json!("sonnet 5.5"), json!("claude-sonnet-5-5[1m]")),
        "切換模型後兩個一起換"
    );
}

/// `/clear` 是新 session 的第一行（#688 實測），不是「接回」：串流裡只有 resume／compact 標狀態行。
#[test]
fn clear_is_not_a_resume_status_line() {
    let mut b = B::new();
    let mut l = Live::new();
    let start = |b: &mut B, s: &str, source: &str, ts: u64| {
        b.ev(s, "session.started", "main", ts, false, json!({"cwd": "/p", "source": source, "model": null, "transcriptPath": "/t", "branch": null}))
    };
    let evs = [
        start(&mut b, "s1", "resume", DAY),
        start(&mut b, "s2", "clear", DAY + 1),
    ];
    apply_all(&mut l, &evs);
    let snap = l.snapshot(&Channel::All, 0, DAY + 2);
    let sources: Vec<&str> = rows(&snap, "stream")
        .iter()
        .filter(|e| e["type"] == "session.started")
        .map(|e| e["payload"]["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, ["resume"]);
}

// ── 同 session 內只依 pos（不變式 1、R14、#681）

fn pos_of(stream: &[&Value], s: &str) -> Vec<u64> {
    stream
        .iter()
        .filter(|e| e["session"] == s)
        .filter_map(|e| e["pos"].as_u64())
        .collect()
}

/// ts_seen 在同 session 內可能倒退（main 與 subagent 兩個 tail 搶 channel、逾時降級用當下時間）。
#[test]
fn assemble_stream_keeps_pos_order_when_ts_seen_goes_back() {
    let mut b = B::new();
    let p5 = b.text("s1", DAY + 1_001);
    let p6 = b.text("s1", DAY + 1_000);
    let out = coding_lab::live::assemble_stream(vec![p5, p6], vec![], None);
    let pos: Vec<u64> = out.iter().filter_map(|e| e.pos).collect();
    assert_eq!(pos, [1, 2]);
}

#[test]
fn snapshot_merges_sessions_without_reordering_within_one() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(
        &mut l,
        &[
            b.text("s1", DAY + 10),
            b.text("s2", DAY + 11),
            b.text("s1", DAY + 5), // 倒退
            b.ev(
                "s1",
                "prompt.submitted",
                "main",
                DAY + 12,
                false,
                json!({"text": "下一個", "promptId": "p2"}),
            ),
            b.text("s2", DAY + 13),
            b.text("s1", DAY + 14),
        ],
    );
    for ch in [Channel::All, Channel::Session("s1".into())] {
        let snap = l.snapshot(&ch, 0, DAY + 100);
        let stream = rows(&snap, "stream");
        assert_eq!(pos_of(&stream, "s1"), [1, 2, 3], "{ch:?}");
        let s1: Vec<&str> = stream
            .iter()
            .filter(|e| e["session"] == "s1")
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            s1,
            [
                "assistant.text",
                "assistant.text",
                "prompt.submitted",
                "assistant.text"
            ],
            "狀態行排在 ts_seen 不晚於它的最後一個 pos 之後"
        );
    }
    let all = l.snapshot(&Channel::All, 0, DAY + 100);
    let order: Vec<(String, u64)> = rows(&all, "stream")
        .iter()
        .filter_map(|e| Some((e["session"].as_str()?.to_string(), e["pos"].as_u64()?)))
        .collect();
    // s1 的 pos 2（ts 5）倒退：開頭換成它之後比 s2 的 pos 1（ts 11）早，先出
    let want: Vec<(String, u64)> = [("s1", 1), ("s1", 2), ("s2", 1), ("s2", 2), ("s1", 3)]
        .iter()
        .map(|(s, p)| (s.to_string(), *p))
        .collect();
    assert_eq!(
        order, want,
        "跨 session 依各列開頭的 (ts_seen, session, pos) 合併"
    );
}

// ── tool.updated 的 danger 不能被之後的 tool.updated 蓋掉（R15）

#[test]
fn later_tool_update_keeps_the_danger_from_an_earlier_one() {
    let mut b = B::new();
    let mut l = Live::new();
    let danger = json!({"rule": "rm-rf", "level": "danger", "why": "刪除"});
    let up = |b: &mut B, ts, status: &str, danger: Option<&Value>| {
        let mut p = json!({"toolUseId": "t1", "status": status, "durationMs": null, "error": null, "interrupted": null});
        if let Some(d) = danger {
            p["danger"] = d.clone();
        }
        b.ev("s1", "tool.updated", "main", ts, false, p)
    };
    // transcript 先到：tool.called 直接帶 pos、沒有 danger；PreToolUse 後到才判斷出危險
    let called = b.ev(
        "s1",
        "tool.called",
        "main",
        DAY,
        true,
        json!({"toolUseId": "t1", "tool": "Bash", "input": {"command": "rm -rf x"},
               "status": "pending", "placement": "stream", "danger": null}),
    );
    apply_all(
        &mut l,
        &[called, up(&mut b, DAY + 1, "running", Some(&danger))],
    );
    l.flush(DAY + 1);
    // 同一個 frame 裡先後兩筆：只送最後一筆，danger 要留著
    apply_all(
        &mut l,
        &[
            up(&mut b, DAY + 2, "running", None),
            up(&mut b, DAY + 3, "done", None),
        ],
    );
    let f = l.flush(DAY + 3).unwrap();
    let ups = f.events_for(&Channel::All);
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].payload["status"], "done");
    assert_eq!(
        ups[0].payload["danger"], danger,
        "frame 只留最後一筆時 danger 不見"
    );

    let snap = l.snapshot(&Channel::All, 0, DAY + 10);
    let u: Vec<&Value> = rows(&snap, "stream")
        .into_iter()
        .filter(|e| e["type"] == "tool.updated")
        .collect();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0]["payload"]["danger"], danger, "snapshot 後危險標示不見");

    // 「載入更多」從 store 組：pool 裡有兩筆 tool.updated，只留最新的那筆也要帶 danger
    let mut b = B::new();
    let called = b.ev(
        "s1",
        "tool.called",
        "main",
        DAY,
        true,
        json!({"toolUseId": "t1", "tool": "Bash", "input": {"command": "rm -rf x"},
               "status": "pending", "placement": "stream", "danger": null}),
    );
    let pool = vec![
        up(&mut b, DAY + 1, "running", Some(&danger)),
        up(&mut b, DAY + 2, "done", None),
    ];
    let out = coding_lab::live::assemble_stream(vec![called], pool, None);
    let u: Vec<&Event> = out.iter().filter(|e| e.kind == "tool.updated").collect();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].payload["status"], "done");
    assert_eq!(u[0].payload["danger"], danger);
}

// ── 改動紀錄與分支（#682）

/// 被拒的 Edit 不留在 snapshot 的改動紀錄（contracts §4「被拒或失敗的改動」）。
#[test]
fn failed_edit_leaves_the_changes_list() {
    let mut b = B::new();
    let mut l = Live::new();
    let edit = |b: &mut B, id: &str, ts: u64| {
        b.ev(
            "s1",
            "file.edit",
            "main",
            ts,
            false,
            json!({"toolUseId": id, "path": "a", "hunk": null, "hunkSource": "none"}),
        )
    };
    let evs = [
        b.started("s1", DAY),
        edit(&mut b, "e1", DAY + 1),
        b.ev("s1", "tool.updated", "main", DAY + 2, false, json!({"toolUseId": "e1", "status": "failed", "durationMs": null, "error": "denied", "interrupted": null})),
        edit(&mut b, "e2", DAY + 3),
    ];
    apply_all(&mut l, &evs);
    let snap = l.snapshot(&Channel::All, 0, DAY + 10);
    let ids: Vec<&str> = rows(&snap, "changes")
        .iter()
        .map(|e| e["payload"]["toolUseId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["e2"]);
}

/// `git worktree add -b X` 是在另一個 worktree 建分支，目前 session 的分支不變；checkout／switch -b 照改。
#[test]
fn worktree_add_does_not_change_the_session_branch() {
    let mut b = B::new();
    let mut l = Live::new();
    let op = |b: &mut B, op: &str, branch: &str, ts: u64| {
        b.ev("s1", "git.op", "main", ts, false, json!({"toolUseId": "t", "op": op, "branch": branch, "hash": null, "message": null, "ok": true}))
    };
    let evs = [
        b.started("s1", DAY),
        op(&mut b, "worktree", "feature/x", DAY + 1),
    ];
    apply_all(&mut l, &evs);
    assert_eq!(l.session_row("s1").unwrap()["branch"], "main");
    let e = op(&mut b, "switch", "feature/y", DAY + 2);
    l.apply(&e);
    assert_eq!(l.session_row("s1").unwrap()["branch"], "feature/y");
}

/// 被拒或失敗的 Edit／Write 視為沒有發生（contracts §4、#682）：頂部 KPI 的改動與新檔案也要扣回，
/// 而且只扣一次；成功的照算。
#[test]
fn kpi_does_not_count_denied_or_failed_changes() {
    let mut b = B::new();
    let mut l = Live::new();
    let failed = |b: &mut B, id: &str, ts| {
        b.ev("s1", "tool.updated", "main", ts, false,
             json!({"toolUseId": id, "status": "failed", "durationMs": null, "error": "denied", "interrupted": null}))
    };
    apply_all(&mut l, &[
        b.ev("s1", "file.edit", "main", DAY + 1, false,
             json!({"toolUseId": "e1", "path": "a.rs", "hunk": null, "hunkSource": "none"})),
        b.ev("s1", "file.edit", "main", DAY + 2, false,
             json!({"toolUseId": "e2", "path": "a.rs", "hunk": null, "hunkSource": "none"})),
        b.ev("s1", "file.write", "main", DAY + 3, false,
             json!({"toolUseId": "w1", "path": "n.rs", "content": "x", "isNew": true, "truncated": false, "lines": 1})),
    ]);
    let before = l.snapshot(&Channel::All, 0, DAY + 3)["kpi"].clone();
    assert_eq!(
        (before["edits"].as_u64(), before["writes"].as_u64()),
        (Some(2), Some(1)),
        "前提"
    );
    // e2 被拒、w1 失敗；e2 再收到一次 failed 也不能扣兩次
    apply_all(
        &mut l,
        &[
            failed(&mut b, "e2", DAY + 4),
            failed(&mut b, "w1", DAY + 5),
            failed(&mut b, "e2", DAY + 6),
        ],
    );
    let k = l.snapshot(&Channel::All, 0, DAY + 10)["kpi"].clone();
    assert_eq!(
        (k["edits"].as_u64(), k["writes"].as_u64()),
        (Some(1), Some(0))
    );
}

/// R15 的 danger 接手（#681）只是為了顯示：KPI 的 dangers 一張卡片只算一次，
/// 接手來的 danger 不再計入。
#[test]
fn carried_danger_is_not_counted_again_in_kpi() {
    let mut b = B::new();
    let mut l = Live::new();
    let danger = json!({"rule": "rm-rf", "level": "danger", "why": "x"});
    let called = b.ev(
        "s1",
        "tool.called",
        "main",
        DAY,
        true,
        json!({"toolUseId": "t1", "tool": "Bash", "input": {"command": "rm -rf x"},
               "status": "pending", "placement": "stream", "danger": null}),
    );
    let up = |b: &mut B, ts, status: &str, danger: Option<&Value>| {
        let mut p = json!({"toolUseId": "t1", "status": status, "durationMs": null, "error": null, "interrupted": null});
        if let Some(d) = danger {
            p["danger"] = d.clone();
        }
        b.ev("s1", "tool.updated", "main", ts, false, p)
    };
    let first = up(&mut b, DAY + 1, "running", Some(&danger));
    apply_all(&mut l, &[called, first]);
    assert_eq!(l.snapshot(&Channel::All, 0, DAY + 1)["kpi"]["dangers"], 1);
    let later = [
        up(&mut b, DAY + 2, "done", None),
        up(&mut b, DAY + 3, "failed", None),
    ];
    apply_all(&mut l, &later);
    assert_eq!(l.snapshot(&Channel::All, 0, DAY + 3)["kpi"]["dangers"], 1);
}

/// 同一毫秒的狀態行與帶 pos 的 Event：snapshot 的順序要和即時 append（依 seq）一樣。
#[test]
fn status_line_tie_follows_seq_like_live_append() {
    let mut b = B::new();
    let line = b.ev(
        "s1",
        "prompt.submitted",
        "main",
        DAY + 5,
        false,
        json!({"text": "hi", "promptId": null}),
    );
    let t = b.text("s1", DAY + 5);
    let out = coding_lab::live::assemble_stream(vec![t], vec![line], None);
    let seqs: Vec<u64> = out.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [1, 2], "即時 append 是 prompt（seq 1）在前");
}

/// 重啟後 live 第一次看到的 session 用 store 的列種（#676）：狀態與結束時間也要照 store，
/// 和 merge 種回的一致（D3，#679）。
#[test]
fn seeded_row_keeps_the_stored_status_and_end() {
    use coding_lab::store::SessionRow;
    let row = SessionRow {
        id: "s1".into(),
        host: "local".into(),
        cwd: "/p".into(),
        transcript_path: None,
        model: None,
        branch: None,
        first_prompt: None,
        started_at: DAY - 3 * H,
        ended_at: Some(DAY - 2 * H),
        end_reason: Some("abandoned".into()),
        status: "ended".into(),
        status_since: DAY - 2 * H,
        commits: 0,
        dangers: 0,
        failures: 0,
        cost_usd: 0.0,
        thinking_tokens: 0,
        last_seq: 40,
    };
    let mut l = Live::new();
    l.seed(&row, DAY);
    let r = l.session_row("s1").unwrap();
    assert_eq!(
        (r["status"].clone(), r["endedAt"].clone()),
        (json!("ended"), json!(DAY - 2 * H))
    );
    // 之後又回到 run：結束不再成立
    let mut b = B::new();
    l.apply(&b.ev(
        "s1",
        "session.status",
        "main",
        DAY + 1,
        false,
        json!({"status": "run", "since": DAY + 1, "score": 0.0}),
    ));
    let r = l.session_row("s1").unwrap();
    assert_eq!(
        (r["status"].clone(), r["endedAt"].clone()),
        (json!("run"), Value::Null)
    );
}

/// #652：transcript 解析失敗時 sessions 列帶 `degraded`（哪些檔降級），恢復後清掉；前端依它顯示標語。
#[test]
fn session_row_lists_degraded_transcript_files_until_recovered() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(
        &mut l,
        &[
            b.started("s1", DAY + H),
            b.ev(
                "s1",
                "transcript.degraded",
                "main",
                DAY + H,
                false,
                json!({"file": "main", "error": "第 3 行：invalid JSON"}),
            ),
            b.ev(
                "s1",
                "transcript.degraded",
                "main",
                DAY + H,
                false,
                json!({"file": "agent-a1", "error": "第 1 行：invalid JSON"}),
            ),
        ],
    );
    let degraded = |l: &Live| l.session_row("s1").unwrap()["degraded"].clone();
    assert_eq!(degraded(&l), json!(["agent-a1", "main"]));
    l.apply(&b.ev(
        "s1",
        "transcript.recovered",
        "main",
        DAY + H,
        false,
        json!({"file": "main"}),
    ));
    assert_eq!(degraded(&l), json!(["agent-a1"]));
    l.apply(&b.ev(
        "s1",
        "transcript.recovered",
        "main",
        DAY + H,
        false,
        json!({"file": "agent-a1"}),
    ));
    assert_eq!(degraded(&l), json!([]));
}

/// #686（R24）：結束超過 2 小時的 session 從 live 移除（列、串流、狀態行），snapshot 不再含它；
/// 之後看它改由 /api/sessions 與回放。
#[test]
fn ended_sessions_leave_live_after_two_hours() {
    let mut b = B::new();
    let mut l = Live::new();
    apply_all(
        &mut l,
        &[
            b.started("s1", DAY),
            b.text("s1", DAY + 1),
            b.ev(
                "s1",
                "session.ended",
                "main",
                DAY + H,
                false,
                json!({"reason": "exit"}),
            ),
            b.started("s2", DAY),
        ],
    );
    l.flush(DAY + 3 * H - 1);
    assert_eq!(l.session_count(), 2, "結束 2 小時內還在");
    l.flush(DAY + 3 * H + 1);
    assert_eq!(l.session_count(), 1);
    let snap = l.snapshot(&Channel::All, 0, DAY + 3 * H + 2);
    let text = snap.to_string();
    assert!(!text.contains("\"s1\""), "snapshot 不含 s1：{text}");
    assert!(text.contains("\"s2\""));
}

/// #686（R23）：沒有新 Event 時 flush 不重算 KPI（只在有新 Event、跨時間格或跨午夜時算）。
#[test]
fn flush_without_new_events_does_not_recompute_kpi() {
    let mut b = B::new();
    let mut l = Live::new();
    l.register_offset(0);
    l.apply(&b.started("s1", DAY + H));
    l.flush(DAY + H + 10);
    let n = l.kpi_computations();
    assert!(n >= 1);
    l.flush(DAY + H + 60);
    l.flush(DAY + H + 110);
    assert_eq!(l.kpi_computations(), n, "沒有新 Event、同一格：不重算");
    l.apply(&b.text("s1", DAY + H + 120));
    l.flush(DAY + H + 160);
    assert_eq!(l.kpi_computations(), n + 1, "有新 Event 才重算");
}

/// #687（R32）：重啟後用 store 補 live 時，live 從串流看到的模型保留（照 seed 的說明），沒有才用 store 的。
#[test]
fn seed_keeps_the_model_live_already_knows() {
    use coding_lab::store::SessionRow;
    let row = |id: &str| SessionRow {
        id: id.into(),
        host: "local".into(),
        cwd: "/p".into(),
        transcript_path: None,
        model: Some("claude-sonnet-5-5".into()),
        branch: None,
        first_prompt: None,
        started_at: DAY,
        ended_at: None,
        end_reason: None,
        status: "run".into(),
        status_since: DAY,
        commits: 0,
        dangers: 0,
        failures: 0,
        cost_usd: 0.0,
        thinking_tokens: 0,
        last_seq: 0,
    };
    let mut b = B::new();
    let mut l = Live::new();
    l.apply(&b.started("s1", DAY));
    l.apply(&b.text("s1", DAY + 1)); // main 第一行 assistant：opus
    l.seed(&row("s1"), DAY);
    assert_eq!(l.session_row("s1").unwrap()["modelId"], "claude-opus-5-5");
    l.seed(&row("s2"), DAY);
    assert_eq!(
        l.session_row("s2").unwrap()["modelId"],
        "claude-sonnet-5-5",
        "live 沒有就用 store 的"
    );
}
