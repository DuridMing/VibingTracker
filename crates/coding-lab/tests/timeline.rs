//! 全專案時間軸回放（#729）：記錄下來的 Event 重新套進一份獨立的 live 狀態，算出任意時刻 T 的主頁畫面
//! 與 T 之後逐時間點的 frame。時間全部由 Event 的 ts_seen 帶入，不看時鐘。

use std::collections::BTreeMap;

use coding_lab::event::Event;
use coding_lab::timeline::{Ack, replay};
use serde_json::{Value, json};

struct Log {
    seq: BTreeMap<String, u64>,
    pos: BTreeMap<String, u64>,
    events: Vec<Event>,
}

impl Log {
    fn new() -> Log {
        Log {
            seq: BTreeMap::new(),
            pos: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    fn ev(&mut self, s: &str, kind: &str, ts: u64, in_stream: bool, payload: Value) -> &mut Log {
        let seq = self.seq.entry(s.into()).or_insert(0);
        *seq += 1;
        let pos = if in_stream {
            let p = self.pos.entry(s.into()).or_insert(0);
            *p += 1;
            json!(*p)
        } else {
            Value::Null
        };
        let e = Event::from_json_line(
            &json!({"v": 1, "session": s, "seq": *seq, "type": kind, "agent": "main", "line": null,
                    "pos": pos, "ts_model": null, "ts_exec": null, "ts_seen": ts, "host": "local",
                    "payload": payload})
            .to_string(),
        )
        .unwrap();
        self.events.push(e);
        self
    }

    fn start(&mut self, s: &str, ts: u64) -> &mut Log {
        self.ev(
            s,
            "session.started",
            ts,
            false,
            json!({"cwd": "/p", "source": "startup", "model": null, "transcriptPath": null, "branch": null}),
        )
    }

    fn text(&mut self, s: &str, ts: u64, text: &str) -> &mut Log {
        self.ev(
            s,
            "assistant.text",
            ts,
            true,
            json!({"text": text, "messageId": format!("m{ts}"), "model": null}),
        )
    }

    fn status(&mut self, s: &str, ts: u64, status: &str) -> &mut Log {
        self.ev(
            s,
            "session.status",
            ts,
            false,
            json!({"status": status, "since": ts, "score": 0}),
        )
    }

    fn raise(&mut self, s: &str, ts: u64, kind: &str, r: &str) -> &mut Log {
        self.ev(
            s,
            "attention.raised",
            ts,
            false,
            json!({"kind": kind, "ref": r, "reason": "測試"}),
        )
    }

    fn clear(&mut self, s: &str, ts: u64, kind: &str, r: &str) -> &mut Log {
        self.ev(
            s,
            "attention.cleared",
            ts,
            false,
            json!({"kind": kind, "ref": r, "reason": null}),
        )
    }
}

/// s1 從 1000 開始：2000 等你（3000 解除）、4000 完成待確認（6000 被確認）；s2 從 5000 開始。
fn sample() -> (Vec<Event>, Vec<Ack>) {
    let mut l = Log::new();
    l.start("s1", 1000)
        .text("s1", 1500, "一")
        .raise("s1", 2000, "wait", "t1")
        .status("s1", 2000, "wait")
        .clear("s1", 3000, "wait", "t1")
        .status("s1", 3000, "run")
        .raise("s1", 4000, "done", "p1")
        .status("s1", 4000, "done")
        .start("s2", 5000)
        .text("s2", 5500, "二")
        .text("s1", 7000, "三");
    let acks = vec![Ack {
        session: "s1".into(),
        kind: "done".into(),
        reference: "p1".into(),
        at: 6000,
    }];
    (l.events, acks)
}

fn ids(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn status_of(snap: &Value, id: &str) -> Value {
    snap["sessions"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == id))
        .map_or(Value::Null, |r| r["status"].clone())
}

#[test]
fn snapshot_shows_the_project_as_it_was_at_t() {
    let (events, acks) = sample();
    let r = replay(&events, &acks, 2500, 2500, 100, 0, true);
    let snap = &r["snapshot"];
    assert_eq!(snap["t"], "snapshot");
    assert_eq!(ids(snap, "sessions"), ["s1"], "s2 在 5000 才開始");
    assert_eq!(status_of(snap, "s1"), "wait");
    assert_eq!(ids(snap, "attention"), ["s1/wait/t1"]);
    let lines: Vec<&Value> = snap["stream"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["pos"].is_u64())
        .collect();
    assert_eq!(lines.len(), 1, "有 pos 的只有 1500 那一行（另有狀態行）");
    assert!(snap["kpi"].is_object() && snap["changes"].is_array());

    let r = replay(&events, &acks, 3500, 3500, 100, 0, true);
    assert!(ids(&r["snapshot"], "attention").is_empty(), "3000 已解除");
    assert_eq!(status_of(&r["snapshot"], "s1"), "run");
}

#[test]
fn acks_before_t_hide_the_row_and_later_acks_do_not() {
    let (events, acks) = sample();
    let r = replay(&events, &acks, 5500, 5500, 100, 0, true);
    assert_eq!(
        ids(&r["snapshot"], "attention"),
        ["s1/done/p1"],
        "6000 才確認"
    );
    assert_eq!(ids(&r["snapshot"], "sessions"), ["s1", "s2"]);
    let r = replay(&events, &acks, 6500, 6500, 100, 0, true);
    assert!(
        ids(&r["snapshot"], "attention").is_empty(),
        "6000 已確認：{}",
        r["snapshot"]["attention"]
    );
}

/// 前端怎麼套 frame，這裡就怎麼套：依 id upsert、`deleted` 移除。
fn fold(rows: &mut BTreeMap<String, Value>, list: &Value) {
    for r in list.as_array().into_iter().flatten() {
        let id = r["id"].as_str().unwrap_or_default().to_string();
        if r["deleted"] == true {
            rows.remove(&id);
        } else {
            let e = rows.entry(id).or_insert_with(|| json!({}));
            for (k, v) in r.as_object().into_iter().flatten() {
                e[k] = v.clone();
            }
        }
    }
}

#[test]
fn frames_play_forward_to_the_same_state_as_a_snapshot_at_until() {
    let (events, acks) = sample();
    let r = replay(&events, &acks, 1200, 8000, 100, 0, true);
    let frames = r["frames"].as_array().unwrap();
    let ts: Vec<u64> = frames.iter().map(|f| f["ts"].as_u64().unwrap()).collect();
    assert!(!ts.is_empty());
    assert!(ts.windows(2).all(|w| w[0] < w[1]), "依時間遞增：{ts:?}");
    assert!(ts.iter().all(|t| *t > 1200 && *t <= 8000), "{ts:?}");
    assert!(frames.iter().all(|f| f["t"] == "frame"));
    assert_eq!(r["next"], Value::Null, "範圍內都放進來了");

    let mut sessions = BTreeMap::new();
    let mut attention = BTreeMap::new();
    fold(&mut sessions, &r["snapshot"]["sessions"]);
    fold(&mut attention, &r["snapshot"]["attention"]);
    let mut stream = r["snapshot"]["stream"].as_array().unwrap().len();
    for f in frames {
        fold(&mut sessions, &f["rows"]["sessions"]);
        fold(&mut attention, &f["rows"]["attention"]);
        stream += f["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["pos"].is_u64())
            .count();
    }
    let end = replay(&events, &acks, 8000, 8000, 100, 0, true);
    let end_sessions: BTreeMap<String, Value> = end["snapshot"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap().to_string(), r["status"].clone()))
        .collect();
    let played: BTreeMap<String, Value> = sessions
        .iter()
        .map(|(id, r)| (id.clone(), r["status"].clone()))
        .collect();
    assert_eq!(played, end_sessions);
    assert_eq!(
        attention.keys().cloned().collect::<Vec<_>>(),
        ids(&end["snapshot"], "attention")
    );
    assert_eq!(
        stream,
        end["snapshot"]["stream"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["pos"].is_u64())
            .count()
    );
}

#[test]
fn limit_cuts_at_a_time_point_and_next_continues_from_there() {
    let (events, acks) = sample();
    let r = replay(&events, &acks, 1200, 8000, 3, 0, false);
    assert!(
        r.get("snapshot").is_none_or(Value::is_null),
        "不要 snapshot"
    );
    let frames = r["frames"].as_array().unwrap();
    let last = frames.last().unwrap()["ts"].as_u64().unwrap();
    assert_eq!(r["next"], json!(last), "下一段從最後套用的時間點接著要");
    let more = replay(&events, &acks, last, 8000, 100, 0, false);
    let first = more["frames"][0]["ts"].as_u64().unwrap();
    assert!(first > last);
}

#[test]
fn sessions_ended_more_than_two_hours_before_t_are_gone() {
    let mut l = Log::new();
    l.start("old", 1000).ev(
        "old",
        "session.ended",
        2000,
        false,
        json!({"reason": "exit"}),
    );
    l.start("new", 2 * 3_600_000 + 5000);
    let r = replay(&l.events, &[], 2 * 3_600_000 + 6000, 0, 100, 0, true);
    assert_eq!(ids(&r["snapshot"], "sessions"), ["new"]);
}
