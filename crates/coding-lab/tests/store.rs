//! store：JSONL ＋ SQLite 索引（contracts.md §5、#635）。

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use coding_lab::event::Event;
use coding_lab::store::{SCHEMA_VERSION, Store};
use serde_json::{Value, json};

static N: AtomicU32 = AtomicU32::new(0);

/// 每個測試一個獨立的資料目錄，放在這個 worktree 的 target/tmp/（不碰真正的 ~/.coding-lab，也不寫到專案外）。
fn home() -> PathBuf {
    let d = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "coding-lab-store-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ev(session: &str, seq: u64, kind: &str, pos: Option<u64>, ts: u64, payload: Value) -> Event {
    Event::from_json_line(
        &json!({
            "v": 1, "session": session, "seq": seq, "type": kind, "agent": "main",
            "line": null, "pos": pos, "ts_model": null, "ts_exec": null, "ts_seen": ts,
            "host": "local", "payload": payload,
        })
        .to_string(),
    )
    .unwrap()
}

fn started(session: &str, seq: u64, cwd: &str) -> Event {
    ev(
        session,
        seq,
        "session.started",
        None,
        1000,
        json!({
            "cwd": cwd, "source": "startup", "model": "claude-opus-5-5",
            "transcriptPath": "/t.jsonl", "branch": "main",
        }),
    )
}

fn text(session: &str, seq: u64, pos: u64) -> Event {
    ev(
        session,
        seq,
        "assistant.text",
        Some(pos),
        1000 + seq,
        json!({
            "text": format!("第 {pos} 行"), "messageId": "m", "model": "claude-opus-5-5",
        }),
    )
}

#[test]
fn appended_events_are_written_as_jsonl_lines() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    let evs = vec![started("s1", 1, "/p"), text("s1", 2, 1), text("s1", 3, 2)];
    for e in &evs {
        s.append(e.clone());
    }
    s.flush().unwrap();
    let file = std::fs::read_to_string(h.join("events/s1.jsonl")).unwrap();
    let lines: Vec<&str> = file.lines().collect();
    assert_eq!(lines.len(), 3);
    for (line, e) in lines.iter().zip(&evs) {
        assert_eq!(Event::from_json_line(line).unwrap(), *e);
    }
}

#[test]
fn nothing_is_written_before_flush() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    assert!(
        !h.join("events/s1.jsonl").exists()
            || std::fs::read(h.join("events/s1.jsonl")).unwrap().is_empty()
    );
    s.flush().unwrap();
    assert!(!std::fs::read(h.join("events/s1.jsonl")).unwrap().is_empty());
}

#[test]
fn stream_events_are_read_back_by_pos() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    for i in 1..=10 {
        s.append(text("s1", i + 1, i));
    }
    s.flush().unwrap();
    // 「載入更多」：pos 5 之前最後 3 筆，依 pos 由小到大
    let got: Vec<u64> = s
        .stream_before("s1", Some(5), 3)
        .unwrap()
        .iter()
        .filter_map(|e| e.pos)
        .collect();
    assert_eq!(got, [2, 3, 4]);
    let latest: Vec<u64> = s
        .stream_before("s1", None, 2)
        .unwrap()
        .iter()
        .filter_map(|e| e.pos)
        .collect();
    assert_eq!(latest, [9, 10]);
}

#[test]
fn session_row_summarises_events() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(ev(
        "s1",
        2,
        "prompt.submitted",
        None,
        1100,
        json!({"text": "修 bug", "promptId": "p1"}),
    ));
    s.append(ev(
        "s1",
        3,
        "prompt.submitted",
        None,
        1200,
        json!({"text": "第二個", "promptId": "p2"}),
    ));
    for (seq, cost, thinking) in [(4, 0.25, 40), (5, 0.5, 60)] {
        s.append(ev("s1", seq, "usage", None, 1300, json!({
            "messageId": format!("m{seq}"), "model": "claude-opus-5-5", "input": 1, "output": 2,
            "thinking": thinking, "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": cost,
        })));
    }
    s.append(ev("s1", 6, "git.op", None, 1400, json!({
        "toolUseId": "g1", "op": "commit", "branch": "main", "hash": "abc", "message": "x", "ok": true,
    })));
    s.append(ev(
        "s1",
        7,
        "attention.raised",
        None,
        1500,
        json!({"kind": "danger", "ref": "t1", "reason": "rm"}),
    ));
    s.append(ev(
        "s1",
        8,
        "tool.result",
        None,
        1600,
        json!({
            "toolUseId": "t2", "ok": false, "exitCode": 1, "output": "", "truncated": false,
        }),
    ));
    s.append(ev(
        "s1",
        9,
        "session.status",
        None,
        1700,
        json!({"status": "wait", "since": 1700, "score": 2.0}),
    ));
    s.flush().unwrap();

    let r = s.session("s1").unwrap().unwrap();
    assert_eq!(r.cwd, "/p");
    assert_eq!(r.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(r.first_prompt.as_deref(), Some("修 bug"));
    assert!((r.cost_usd - 0.75).abs() < 1e-9);
    assert_eq!(r.thinking_tokens, 100);
    assert_eq!((r.commits, r.dangers, r.failures), (1, 1, 1));
    assert_eq!((r.status.as_str(), r.status_since), ("wait", 1700));
    assert_eq!(r.last_seq, 9);
    assert_eq!(r.ended_at, None);

    s.append(ev(
        "s1",
        10,
        "session.ended",
        None,
        1800,
        json!({"reason": "other"}),
    ));
    s.flush().unwrap();
    let r = s.session("s1").unwrap().unwrap();
    assert_eq!(
        (r.ended_at, r.end_reason.as_deref(), r.status.as_str()),
        (Some(1800), Some("other"), "ended")
    );
}

#[test]
fn events_without_session_started_still_get_a_row() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(text("late", 1, 1));
    s.flush().unwrap();
    let r = s.session("late").unwrap().unwrap();
    assert_eq!(r.started_at, 1001);
}

fn file_ev(session: &str, seq: u64, kind: &str, tool: &str, path: &str, ts: u64) -> Event {
    let payload = match kind {
        "file.read" => json!({"toolUseId": tool, "path": path}),
        "file.edit" => json!({"toolUseId": tool, "path": path, "hunkSource": "pre", "hunk": {
            "start": 3, "oldLines": 1, "newLines": 2, "lines": [" a", "-b", "+c", "+d"] }}),
        _ => {
            json!({"toolUseId": tool, "path": path, "isNew": true, "content": "x\ny\nz\n", "truncated": false})
        }
    };
    ev(session, seq, kind, None, ts, payload)
}

#[test]
fn who_touched_a_file() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(started("s2", 1, "/p"));
    s.append(file_ev("s1", 2, "file.read", "r1", "src/a.rs", 2000));
    s.append(file_ev("s1", 3, "file.read", "r2", "src/a.rs", 2100));
    s.append(file_ev("s2", 2, "file.edit", "e1", "src/a.rs", 2200));
    s.append(file_ev("s2", 3, "file.edit", "e2", "src/b.rs", 2300));
    // 失敗的 Edit 計入 fail
    s.append(file_ev("s1", 4, "file.edit", "e3", "src/a.rs", 2400));
    s.append(ev("s1", 5, "tool.result", None, 2410, json!({
        "toolUseId": "e3", "ok": false, "exitCode": null, "output": "old_string not found", "truncated": false,
    })));
    s.flush().unwrap();

    let mut t = s.touches("/p", "src/a.rs").unwrap();
    t.sort_by(|a, b| (&a.session, &a.kind).cmp(&(&b.session, &b.kind)));
    let got: Vec<(&str, &str, u64)> = t
        .iter()
        .map(|x| (x.session.as_str(), x.kind.as_str(), x.count))
        .collect();
    assert_eq!(
        got,
        [
            ("s1", "edit", 1),
            ("s1", "fail", 1),
            ("s1", "read", 2),
            ("s2", "edit", 1)
        ]
    );
    assert!(
        s.touches("/other", "src/a.rs").unwrap().is_empty(),
        "依專案（cwd）分開"
    );
}

#[test]
fn recent_changes_newest_first_with_line_counts() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(file_ev("s1", 2, "file.edit", "e1", "a.rs", 3000));
    s.append(file_ev("s1", 3, "file.write", "w1", "b.rs", 3100));
    s.append(file_ev("s1", 4, "file.read", "r1", "c.rs", 3200));
    s.flush().unwrap();
    let c = s.recent_changes("/p", 10).unwrap();
    let got: Vec<(&str, &str, Option<u64>, Option<u64>)> = c
        .iter()
        .map(|x| (x.path.as_str(), x.kind.as_str(), x.added, x.removed))
        .collect();
    assert_eq!(
        got,
        [
            ("b.rs", "write", Some(3), Some(0)),
            ("a.rs", "edit", Some(2), Some(1))
        ]
    );
}

#[test]
fn attention_raise_clear_and_ack() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(ev(
        "s1",
        2,
        "attention.raised",
        None,
        4000,
        json!({"kind": "wait", "ref": "t1", "reason": "AskUserQuestion"}),
    ));
    s.append(ev(
        "s1",
        3,
        "attention.raised",
        None,
        4100,
        json!({"kind": "danger", "ref": "t2", "reason": "rm"}),
    ));
    s.flush().unwrap();
    assert_eq!(s.open_attention().unwrap().len(), 2);

    s.append(ev(
        "s1",
        4,
        "attention.cleared",
        None,
        4200,
        json!({"kind": "wait", "ref": "t1", "reason": "answered"}),
    ));
    s.flush().unwrap();
    let open = s.open_attention().unwrap();
    assert_eq!(
        open.iter().map(|a| a.kind.as_str()).collect::<Vec<_>>(),
        ["danger"]
    );

    assert!(s.ack("s1", "danger", "t2", 4300).unwrap());
    assert!(!s.ack("s1", "danger", "nope", 4300).unwrap());
    assert!(s.open_attention().unwrap().is_empty());
}

#[test]
fn schema_is_versioned_and_data_survives_reopen() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        s.append(started("s1", 1, "/p"));
        s.flush().unwrap();
    }
    let s = Store::open(&h).unwrap();
    assert_eq!(s.session("s1").unwrap().unwrap().cwd, "/p");
}

#[test]
fn newer_schema_is_refused() {
    let h = home();
    drop(Store::open(&h).unwrap());
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute(
        "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    drop(db);
    assert!(Store::open(&h).is_err(), "比程式新的 schema 不能亂寫");
}

/// 驗收：中途被砍，JSONL 不留半行，索引與 JSONL 一致。
#[test]
fn crash_leaves_no_half_line_and_index_catches_up() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        s.append(text("s1", 2, 1));
        s.flush().unwrap();
    }
    // 模擬：寫進 JSONL 但還沒進索引就被砍，最後還留下半行
    let path = h.join("events/s1.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{}", text("s1", 3, 2).to_json_line()).unwrap();
    write!(f, "{{\"v\":1,\"session\":\"s1\",\"seq\":4,\"ty").unwrap();
    drop(f);

    let mut s = Store::open(&h).unwrap();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.ends_with('\n'), "半行要被截掉");
    assert_eq!(file.lines().count(), 3);
    assert_eq!(
        s.session("s1").unwrap().unwrap().last_seq,
        3,
        "沒進索引的完整行要補索引"
    );
    let pos: Vec<u64> = s
        .stream_before("s1", None, 10)
        .unwrap()
        .iter()
        .filter_map(|e| e.pos)
        .collect();
    assert_eq!(pos, [1, 2]);

    // 之後的寫入接在正確的位置
    s.append(text("s1", 4, 3));
    s.flush().unwrap();
    let pos: Vec<u64> = s
        .stream_before("s1", None, 10)
        .unwrap()
        .iter()
        .filter_map(|e| e.pos)
        .collect();
    assert_eq!(pos, [1, 2, 3]);
}

/// SQLite 可以從 JSONL 重建（schema 升級失敗時的退路）。
#[test]
fn index_is_rebuilt_from_jsonl() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        s.append(file_ev("s1", 2, "file.edit", "e1", "a.rs", 3000));
        s.append(text("s1", 3, 1));
        s.flush().unwrap();
    }
    std::fs::remove_file(h.join("index.sqlite")).unwrap();
    let s = Store::open(&h).unwrap();
    assert_eq!(s.session("s1").unwrap().unwrap().last_seq, 3);
    assert_eq!(s.recent_changes("/p", 10).unwrap().len(), 1);
    assert_eq!(s.stream_before("s1", None, 10).unwrap().len(), 1);
}

#[test]
fn unsafe_session_ids_do_not_escape_the_events_dir() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("../../evil", 1, "/p"));
    s.flush().unwrap();
    assert!(!h.parent().unwrap().join("evil.jsonl").exists());
    assert_eq!(std::fs::read_dir(h.join("events")).unwrap().count(), 1);
}

/// #670-1：重啟時 merge 要從這裡接著編 seq／pos。
#[test]
fn counters_are_the_highest_seq_and_pos_per_session() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    for e in [
        started("a", 1, "/p"),
        text("a", 2, 1),
        text("a", 3, 2),
        started("b", 1, "/q"),
    ] {
        s.append(e);
    }
    s.flush().unwrap();
    drop(s);
    let c = Store::open(&h).unwrap().counters().unwrap();
    let got = |id: &str| c.get(id).map(|c| (c.seq, c.pos, c.cwd.as_str()));
    assert_eq!(got("a"), Some((3, 2, "/p")));
    assert_eq!(got("b"), Some((1, 0, "/q")), "沒進過串流的 pos 是 0");
}

/// #672：之後的 session.started（compact／clear／resume）不換路徑基準，舊的與新的 file.* 都歸在同一個 cwd。
#[test]
fn later_session_started_keeps_the_first_cwd() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/r"));
    let mut compact = started("s1", 2, "/r/worktrees/x");
    compact.payload["source"] = json!("compact");
    s.append(compact);
    s.append(file_ev(
        "s1",
        3,
        "file.edit",
        "e1",
        "worktrees/x/README.md",
        3000,
    ));
    s.flush().unwrap();
    assert_eq!(s.session("s1").unwrap().unwrap().cwd, "/r");
    assert_eq!(s.touches("/r", "worktrees/x/README.md").unwrap().len(), 1);
    assert_eq!(s.recent_changes("/r", 10).unwrap().len(), 1);
    drop(s);
    let c = Store::open(&h).unwrap().counters().unwrap();
    assert_eq!(c["s1"].cwd, "/r", "重啟後交給 merge 的也是第一次的 cwd");
}
