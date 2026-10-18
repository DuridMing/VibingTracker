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

/// danger 解除（工具結束）後仍在佇列，ack 才離開；重開也讀得回來（契約 §5 `attention_open`）。
#[test]
fn cleared_danger_stays_open_until_ack() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        for (seq, kind, r) in [(2, "danger", "t1"), (3, "fail", "t2")] {
            s.append(ev(
                "s1",
                seq,
                "attention.raised",
                None,
                4000,
                json!({"kind": kind, "ref": r, "reason": "x"}),
            ));
        }
        for (seq, kind, r) in [(4, "danger", "t1"), (5, "fail", "t2")] {
            s.append(ev(
                "s1",
                seq,
                "attention.cleared",
                None,
                4100,
                json!({"kind": kind, "ref": r, "reason": "done"}),
            ));
        }
        s.flush().unwrap();
    }
    let mut s = Store::open(&h).unwrap();
    let open = s.open_attention().unwrap();
    assert_eq!(
        open.iter()
            .map(|a| (a.kind.as_str(), a.reference.as_str()))
            .collect::<Vec<_>>(),
        [("danger", "t1")],
        "解除的 danger 留到 ack；fail 解除就離開"
    );
    assert!(s.ack("s1", "danger", "t1", 4200).unwrap());
    assert!(s.open_attention().unwrap().is_empty());
}

/// v1 的資料庫開啟時升級：v2 換掉 attention_open 索引，資料不動（不變式 9）。
#[test]
fn v1_database_is_migrated_to_v2() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        s.flush().unwrap();
    }
    // 把它改回 v1 的樣子：舊索引條件與版本號
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute_batch(
        "DROP INDEX attention_open;
         CREATE INDEX attention_open ON attention (raised_at) WHERE cleared_at IS NULL AND acked_at IS NULL;
         UPDATE meta SET value = '1' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(db);

    let s = Store::open(&h).unwrap();
    assert_eq!(
        s.schema_version().unwrap(),
        SCHEMA_VERSION,
        "v1 一路升到最新"
    );
    assert_eq!(s.session("s1").unwrap().unwrap().cwd, "/p", "資料不動");
    drop(s);
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    let sql: String = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'attention_open'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("kind = 'danger'"), "索引換成 v2：{sql}");
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

/// #673：SessionStart 沒有 model 時，sessions.model 取 main 第一筆 usage 的模型；model.changed 換掉；重建後一樣。
#[test]
fn session_model_comes_from_the_first_main_usage() {
    let h = home();
    let usage = |seq: u64, agent: &str, model: &str| {
        let mut e = ev(
            "s1",
            seq,
            "usage",
            None,
            1000 + seq,
            json!({"messageId": format!("m{seq}"), "model": model, "input": 0, "output": 0, "thinking": 0,
                   "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": 0.0}),
        );
        e.agent = agent.into();
        e
    };
    let model = |s: &Store| s.session("s1").unwrap().unwrap().model;
    {
        let mut s = Store::open(&h).unwrap();
        let mut start = started("s1", 1, "/p");
        start.payload["model"] = Value::Null;
        s.append(start);
        s.append(usage(2, "a1", "claude-haiku-5-5"));
        s.flush().unwrap();
        assert_eq!(model(&s), None, "subagent 不算");
        s.append(usage(3, "main", "claude-opus-5-5"));
        s.append(usage(4, "main", "claude-sonnet-5-5"));
        s.flush().unwrap();
        assert_eq!(model(&s).as_deref(), Some("claude-opus-5-5"));
        s.append(ev(
            "s1",
            5,
            "model.changed",
            Some(1),
            1005,
            json!({"from": "claude-opus-5-5", "to": "claude-sonnet-5-5"}),
        ));
        s.flush().unwrap();
        assert_eq!(model(&s).as_deref(), Some("claude-sonnet-5-5"));
    }
    std::fs::remove_file(h.join("index.sqlite")).unwrap();
    let s = Store::open(&h).unwrap();
    assert_eq!(
        model(&s).as_deref(),
        Some("claude-sonnet-5-5"),
        "從 JSONL 重建"
    );
}

/// #675：費用暴增的基準要讀最近的 usage（session、ts_seen、costUsd）。
#[test]
fn usage_since_reads_cost_rows() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    let usage = |seq, ts, cost| {
        ev(
            "s1",
            seq,
            "usage",
            None,
            ts,
            json!({"messageId": format!("m{seq}"), "model": "claude-opus-5-5",
            "input": 0, "output": 0, "thinking": 0, "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": cost}),
        )
    };
    s.append(usage(2, 1000, 0.5));
    s.append(usage(3, 5000, 1.25));
    s.flush().unwrap();
    assert_eq!(
        s.usage_since(2000).unwrap(),
        [("s1".to_string(), 5000, 1.25)],
        "只取 since 之後的"
    );
}

// ── 寫入失敗與重複索引（#683）

/// flush 寫 JSONL 失敗時，這批 Event 留著下次再寫，不會遺失。
#[test]
#[cfg(unix)]
fn failed_flush_keeps_events_for_the_next_one() {
    use std::os::unix::fs::PermissionsExt;
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(text("s1", 2, 1));
    let events = h.join("events");
    std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o500)).unwrap();
    let first = s.flush();
    std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(first.is_err(), "前提：events/ 唯讀時寫不進去");
    s.flush().unwrap();
    assert_eq!(s.recent_events(0).unwrap().len(), 2, "失敗的那批要補寫");
    let pos: Vec<u64> = s
        .stream_before("s1", None, 10)
        .unwrap()
        .iter()
        .filter_map(|e| e.pos)
        .collect();
    assert_eq!(pos, [1], "offset 指得到正確的行");
    assert_eq!(
        std::fs::read_to_string(events.join("s1.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

/// 同一個 (session, seq) 再索引一次時不重複累加。
#[test]
fn duplicate_seq_is_not_counted_twice() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    let usage = json!({ "messageId": "m", "model": "claude-opus-5-5", "input": 1, "output": 1,
                        "thinking": 5, "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0,
                        "costUsd": 1.0 });
    s.append(ev("s1", 1, "usage", None, 1, usage.clone()));
    s.append(ev("s1", 1, "usage", None, 1, usage));
    s.flush().unwrap();
    let row = s.session("s1").unwrap().unwrap();
    assert_eq!(row.cost_usd, 1.0);
    assert_eq!(row.thinking_tokens, 5);
}

/// 重開時只讀索引最後一行之後的部分，不把整個 JSONL 讀進記憶體。
#[test]
fn reopen_reads_only_the_unindexed_tail() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        for seq in 2..50 {
            s.append(text("s1", seq, seq - 1));
        }
        s.flush().unwrap();
    }
    let size = std::fs::metadata(h.join("events/s1.jsonl")).unwrap().len();
    let s = Store::open(&h).unwrap();
    assert!(s.recover_bytes() > 0, "至少讀最後一行確認");
    assert!(
        s.recover_bytes() < size / 10,
        "recover 讀了 {} bytes，檔案 {size} bytes",
        s.recover_bytes()
    );
    assert_eq!(s.session("s1").unwrap().unwrap().last_seq, 49);
}

// ── 被拒或失敗的改動、Write 行數（#682）

fn failed(session: &str, seq: u64, tool: &str, ts: u64) -> Event {
    ev(
        session,
        seq,
        "tool.updated",
        None,
        ts,
        json!({
            "toolUseId": tool, "status": "failed", "durationMs": null, "error": "denied", "interrupted": null,
        }),
    )
}

/// 卡片以 failed 結束：那筆改動視為沒發生——changes 刪掉、file_touch 的 edit／write 減一並記 fail（contracts §4）。
#[test]
fn failed_edit_and_write_are_not_changes() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(file_ev("s1", 2, "file.edit", "e1", "a.rs", 3000));
    s.append(failed("s1", 3, "e1", 3010));
    s.append(file_ev("s1", 4, "file.write", "w1", "b.rs", 3100));
    s.append(failed("s1", 5, "w1", 3110));
    s.append(file_ev("s1", 6, "file.edit", "e2", "a.rs", 3200));
    s.flush().unwrap();
    let paths: Vec<(String, String)> = s
        .recent_changes("/p", 10)
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.tool_use_id))
        .collect();
    assert_eq!(paths, [("a.rs".to_string(), "e2".to_string())]);
    let kinds = |p: &str| {
        let mut t: Vec<(String, u64)> = s
            .touches("/p", p)
            .unwrap()
            .into_iter()
            .map(|t| (t.kind, t.count))
            .collect();
        t.sort();
        t
    };
    assert_eq!(
        kinds("a.rs"),
        [("edit".to_string(), 1), ("fail".to_string(), 1)]
    );
    assert_eq!(kinds("b.rs"), [("fail".to_string(), 1)]);
}

/// file.write 帶全文行數 `lines` 時，改動紀錄用它而不是 200 行的預覽（contracts §4）。
#[test]
fn write_line_count_uses_full_length() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    let preview: String = (0..200).map(|i| format!("l{i}\n")).collect();
    s.append(ev("s1", 2, "file.write", None, 3000, json!({
        "toolUseId": "w1", "path": "big.txt", "isNew": true, "content": preview, "truncated": true, "lines": 5000,
    })));
    s.flush().unwrap();
    assert_eq!(s.recent_changes("/p", 10).unwrap()[0].added, Some(5000));
}

/// /compact 的 session.started 不能把 done 改成 run：狀態由 merge 的 `session.status` 決定（不變式 8，K-5）。
#[test]
fn only_session_status_changes_the_status() {
    let mut s = Store::open(&home()).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(ev(
        "s1",
        2,
        "session.status",
        None,
        1500,
        json!({"status": "done", "since": 1500, "score": 1.0}),
    ));
    let mut compact = started("s1", 3, "/p");
    compact.payload["source"] = json!("compact");
    compact.ts_seen = 2000;
    s.append(compact);
    s.flush().unwrap();
    let row = s.session("s1").unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.status_since), ("done", 1500));
}

/// 結束之後又回到其他狀態（被補 abandoned 的 session 繼續使用，#679）：sessions 列的結束時間與原因清掉，
/// 否則下次重啟又被種回 ended。
#[test]
fn status_after_end_clears_the_end() {
    let mut s = Store::open(&home()).unwrap();
    s.append(ev(
        "s1",
        1,
        "session.ended",
        None,
        100,
        json!({"reason": "abandoned"}),
    ));
    s.append(ev(
        "s1",
        2,
        "session.status",
        None,
        200,
        json!({"status": "run", "since": 200, "score": 0.0}),
    ));
    s.flush().unwrap();
    let row = s.session("s1").unwrap().unwrap();
    assert_eq!(
        (row.ended_at, row.end_reason, row.status.as_str()),
        (None, None, "run")
    );
}

fn rows_of(h: &std::path::Path, session: &str) -> Vec<(String, i64)> {
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    ["sessions", "events", "file_touch", "changes", "attention"]
        .iter()
        .map(|t| {
            let col = if *t == "sessions" { "id" } else { "session" };
            let n: i64 = db
                .query_row(
                    &format!("SELECT COUNT(*) FROM {t} WHERE {col} = ?1"),
                    [session],
                    |r| r.get(0),
                )
                .unwrap();
            (t.to_string(), n)
        })
        .collect()
}

/// #655：最後活動早於界線的 session 整個清掉（JSONL 與所有索引列），剛好在界線上的留著；重開不會被 recover 補回來。
#[test]
fn prune_removes_sessions_whose_last_activity_is_before_the_cutoff() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("old", 1, "/p"));
        s.append(ev(
            "old",
            2,
            "assistant.text",
            Some(1),
            5000,
            json!({"text": "a", "messageId": "m"}),
        ));
        s.append(started("edge", 1, "/p"));
        s.append(ev(
            "edge",
            2,
            "assistant.text",
            Some(1),
            5001,
            json!({"text": "b", "messageId": "m"}),
        ));
        s.flush().unwrap();
    }
    {
        // 其他表也放一列，確認一起清掉
        let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
        for sid in ["old", "edge"] {
            db.execute(
                "INSERT INTO file_touch VALUES ('/p', 'a.rs', ?1, 'edit', 1, 5000)",
                [sid],
            )
            .unwrap();
            db.execute(
                "INSERT INTO changes VALUES (?1, 99, 't', '/p', 'a.rs', 'edit', 'main', 5000, 1, 0)",
                [sid],
            )
            .unwrap();
            db.execute(
                "INSERT INTO attention VALUES (?1, 'danger', 't', NULL, 5000, NULL, NULL)",
                [sid],
            )
            .unwrap();
        }
    }
    let mut s = Store::open(&h).unwrap();
    let report = s.prune(5001).unwrap();
    assert_eq!(
        report.sessions,
        ["old"],
        "5000 早於 5001，5001 剛好在界線上不清"
    );
    assert!(report.bytes > 0);
    assert!(!h.join("events/old.jsonl").exists());
    assert!(h.join("events/edge.jsonl").exists());
    assert!(
        rows_of(&h, "old").iter().all(|(_, n)| *n == 0),
        "{:?}",
        rows_of(&h, "old")
    );
    assert!(
        rows_of(&h, "edge").iter().all(|(_, n)| *n == 1 || *n == 2),
        "{:?}",
        rows_of(&h, "edge")
    );
    // 留下來的 session 照常寫入
    s.append(text("edge", 3, 2));
    s.flush().unwrap();
    drop(s);

    let s = Store::open(&h).unwrap();
    assert!(s.session("old").unwrap().is_none(), "重開不會被補回來");
    assert_eq!(s.session("edge").unwrap().unwrap().last_seq, 3);
}

/// 驗收：舊版（v1）資料目錄升級後照常查詢：session 列、事件、注意佇列都在。
#[test]
fn old_data_directory_is_queryable_after_upgrade() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        s.append(text("s1", 2, 1));
        s.append(ev(
            "s1",
            3,
            "attention.raised",
            None,
            1003,
            json!({"kind": "danger", "ref": "t1", "reason": "rm -rf"}),
        ));
        s.flush().unwrap();
    }
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute_batch(
        "DROP INDEX attention_open;
         CREATE INDEX attention_open ON attention (raised_at) WHERE cleared_at IS NULL AND acked_at IS NULL;
         UPDATE meta SET value = '1' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(db);

    let s = Store::open(&h).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(s.sessions(Some("/p"), 10).unwrap().len(), 1);
    assert_eq!(s.events_by_seq("s1", 1, 3).unwrap().len(), 3);
    assert_eq!(s.open_attention().unwrap().len(), 1);
}

fn at_line(session: &str, seq: u64, file: &str, no: u64) -> Event {
    Event::from_json_line(
        &json!({
            "v": 1, "session": session, "seq": seq, "type": "assistant.text", "agent": "main",
            "line": {"file": file, "no": no}, "pos": seq, "ts_model": null, "ts_exec": null,
            "ts_seen": 1000 + seq, "host": "local",
            "payload": {"text": "t", "messageId": "m", "model": "claude-opus-5-5"},
        })
        .to_string(),
    )
    .unwrap()
}

fn started_at(session: &str, path: &str) -> Event {
    ev(
        session,
        1,
        "session.started",
        None,
        1000,
        json!({"cwd": "/p", "source": "startup", "transcriptPath": path}),
    )
}

/// #684 D4：tails 記每個 transcript 檔已產生 Event 的最大行號，重開後還在。
#[test]
fn tails_record_the_last_line_of_each_transcript_file() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started_at("s1", "/t/s1.jsonl"));
        s.append(at_line("s1", 2, "main", 3));
        s.append(at_line("s1", 3, "agent-a1", 2));
        s.append(at_line("s1", 4, "main", 5));
        s.flush().unwrap();
    }
    let s = Store::open(&h).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    let mut rows: Vec<_> = s
        .tail_lines()
        .unwrap()
        .into_iter()
        .map(|t| (t.session, t.file, t.path, t.line_no))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            ("s1".into(), "agent-a1".into(), None, 2),
            ("s1".into(), "main".into(), Some("/t/s1.jsonl".into()), 5),
        ]
    );
}

/// #684 D4：v2 的資料庫升到 v3 時掃一次 JSONL 回填 tails；prune 一起清掉。
#[test]
fn v2_database_is_backfilled_with_tails_and_prune_clears_them() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started_at("s1", "/t/s1.jsonl"));
        s.append(at_line("s1", 2, "main", 4));
        s.flush().unwrap();
    }
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute_batch(
        "DROP TABLE IF EXISTS tails; UPDATE meta SET value = '2' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(db);

    let mut s = Store::open(&h).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    let rows: Vec<_> = s
        .tail_lines()
        .unwrap()
        .into_iter()
        .map(|t| (t.file, t.line_no))
        .collect();
    assert_eq!(rows, [("main".to_string(), 4)], "升級時從 JSONL 回填");

    s.prune(u64::MAX).unwrap();
    assert!(s.tail_lines().unwrap().is_empty(), "prune 一起清掉 tails");
}

/// #684：index.sqlite 整個刪掉重建時，tails 也從 JSONL 重建。
#[test]
fn tails_are_rebuilt_with_the_index() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started_at("s1", "/t/s1.jsonl"));
        s.append(at_line("s1", 2, "main", 6));
        s.flush().unwrap();
    }
    for f in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
        let _ = std::fs::remove_file(h.join(f));
    }
    let s = Store::open(&h).unwrap();
    let rows: Vec<_> = s
        .tail_lines()
        .unwrap()
        .into_iter()
        .map(|t| (t.file, t.path, t.line_no))
        .collect();
    assert_eq!(
        rows,
        [("main".to_string(), Some("/t/s1.jsonl".to_string()), 6)]
    );
}

/// #686：工具卡片碰的檔（索引改動用）在 session 結束時清掉，不隨時間一直長大。
#[test]
fn tool_paths_are_forgotten_when_the_session_ends() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.append(ev(
        "s1",
        2,
        "file.read",
        None,
        1002,
        json!({"toolUseId": "t1", "path": "a.rs"}),
    ));
    s.flush().unwrap();
    assert_eq!(s.cached_tool_paths(), 1);
    s.append(ev(
        "s1",
        3,
        "session.ended",
        None,
        1003,
        json!({"reason": "exit"}),
    ));
    s.flush().unwrap();
    assert_eq!(s.cached_tool_paths(), 0);
}

/// v3 升 v4：加上 events 依 ts_seen 的索引，全專案回放不用掃全表（不變式 9）。
#[test]
fn v3_database_gets_the_ts_index() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        s.append(text("s1", 2, 1));
        s.flush().unwrap();
    }
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute_batch(
        "DROP INDEX IF EXISTS events_by_ts;
         UPDATE meta SET value = '3' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(db);

    let s = Store::open(&h).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(
        s.timeline("/p", 0, 0, u64::MAX, 10).unwrap().0.len(),
        2,
        "升級前的資料查得到"
    );
    drop(s);
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    let n: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'events_by_ts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "v4 有 events_by_ts 索引");
}

// ── #713：tails 的「不知道」（NULL，從檔尾）和「從頭、還沒讀」（0，重啟從第 1 行）分開

fn started_from(session: &str, path: &str, source: &str) -> Event {
    ev(
        session,
        1,
        "session.started",
        None,
        1000,
        json!({"cwd": "/p", "source": source, "transcriptPath": path}),
    )
}

fn raw_tail_line_no(h: &std::path::Path, session: &str) -> Option<Option<i64>> {
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.query_row(
        "SELECT line_no FROM tails WHERE session = ?1 AND file = 'main'",
        [session],
        |r| r.get::<_, Option<i64>>(0),
    )
    .ok()
}

/// startup／clear／late 的 session.started 是從檔頭讀的檔：還沒有行產生 Event 時記 0，重啟從第 1 行讀；
/// resume／compact 接的是從檔尾 tail 的檔：記 NULL，重啟照舊從檔尾，不整份重讀。
#[test]
fn tails_of_a_fresh_file_start_at_zero_and_of_a_resumed_file_are_unknown() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started_from("fresh", "/t/fresh.jsonl", "startup"));
    s.append(started_from("cleared", "/t/cleared.jsonl", "clear"));
    s.append(started_from("late", "/t/late.jsonl", "late"));
    s.append(started_from("resumed", "/t/resumed.jsonl", "resume"));
    s.append(started_from("compacted", "/t/compacted.jsonl", "compact"));
    s.flush().unwrap();
    let mut rows: Vec<_> = s
        .tail_lines()
        .unwrap()
        .into_iter()
        .map(|t| (t.session, t.line_no))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            ("cleared".to_string(), 0),
            ("fresh".to_string(), 0),
            ("late".to_string(), 0)
        ],
        "從頭讀的檔記 0 並交給 TailPlanner；resume／compact 的不交（NULL）"
    );
    assert_eq!(raw_tail_line_no(&h, "resumed"), Some(None), "NULL＝不知道");
    assert_eq!(raw_tail_line_no(&h, "compacted"), Some(None));

    // 之後讀到的行照常更新；resume 的檔讀到行之後也會接著讀
    s.append(at_line("resumed", 2, "main", 7));
    s.flush().unwrap();
    assert_eq!(raw_tail_line_no(&h, "resumed"), Some(Some(7)));
}

/// v4 → v5：既有的 0 全改成 NULL（舊資料維持「從檔尾」，不把 #684 之前從檔尾 tail 的 session 整份重讀）；
/// 升級後新的 startup 列才記 0。
#[test]
fn v4_database_turns_zero_tails_into_unknown() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started_at("s1", "/t/s1.jsonl"));
        s.append(started_at("s2", "/t/s2.jsonl"));
        s.append(at_line("s2", 2, "main", 4));
        s.flush().unwrap();
    }
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute_batch(
        "UPDATE tails SET line_no = 0 WHERE session = 's1';
         UPDATE meta SET value = '4' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(db);

    let mut s = Store::open(&h).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(raw_tail_line_no(&h, "s1"), Some(None), "升級把 0 改成 NULL");
    assert_eq!(raw_tail_line_no(&h, "s2"), Some(Some(4)), "有行號的不動");
    // 重啟交給 TailPlanner 的只有 s2：s1 照舊從檔尾讀，不會整份重讀、不會重複產生 Event
    let resumed: Vec<_> = s
        .tail_lines()
        .unwrap()
        .into_iter()
        .map(|t| (t.session, t.line_no))
        .collect();
    assert_eq!(resumed, [("s2".to_string(), 4)], "NULL 的列不拿來接續");
    s.append(started_at("s3", "/t/s3.jsonl"));
    s.flush().unwrap();
    assert_eq!(
        raw_tail_line_no(&h, "s3"),
        Some(Some(0)),
        "升級後新的 startup 列記 0"
    );
}

/// tails 記的是「tail 讀到哪一行」，不只是「哪一行產生了 Event」：沒產生 Event（或 Event 不帶 line）的行
/// 也算讀過，重啟不重讀（例如 Esc 的 `[Request interrupted by user]` 行只改狀態，重讀會讓 session 又變 idle）。
/// 位置和 Event 在同一次 flush 寫入：當掉時兩者一起留下或一起沒有。
#[test]
fn tails_advance_to_the_last_line_read_even_without_events() {
    let h = home();
    let mut s = Store::open(&h).unwrap();
    s.append(started_at("s1", "/t/s1.jsonl"));
    s.flush().unwrap();
    s.saw_line("s1", "main", 3);
    assert_eq!(raw_tail_line_no(&h, "s1"), Some(Some(0)), "flush 前還沒寫");
    s.flush().unwrap();
    assert_eq!(raw_tail_line_no(&h, "s1"), Some(Some(3)));
    s.saw_line("s1", "agent-a1", 2);
    s.flush().unwrap();
    let mut rows: Vec<_> = s
        .tail_lines()
        .unwrap()
        .into_iter()
        .map(|t| (t.file, t.line_no))
        .collect();
    rows.sort();
    assert_eq!(rows, [("agent-a1".to_string(), 2), ("main".to_string(), 3)]);
}

// ── 起不來的情況（#738 L-10、L-14）：壞掉的一個檔或做到一半的建表不能讓 collector 永遠起不來

/// L-14：新建資料庫的 v0→v1 原本沒有交易，建表做到一半當掉後，下次再建就撞上已經在的表。
#[test]
fn half_created_v1_schema_still_opens() {
    let h = home();
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE sessions (id TEXT PRIMARY KEY, host TEXT NOT NULL DEFAULT 'local', cwd TEXT NOT NULL,
           transcript_path TEXT, model TEXT, branch TEXT, first_prompt TEXT, started_at INTEGER NOT NULL,
           ended_at INTEGER, end_reason TEXT, status TEXT NOT NULL DEFAULT 'run', status_since INTEGER NOT NULL,
           commits INTEGER NOT NULL DEFAULT 0, dangers INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0,
           cost_usd REAL NOT NULL DEFAULT 0, thinking_tokens INTEGER NOT NULL DEFAULT 0, last_seq INTEGER NOT NULL DEFAULT 0,
           summary TEXT);",
    )
    .unwrap();
    drop(db);
    let mut s = Store::open(&h).expect("做到一半的建表要能補完");
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    s.append(started("s1", 1, "/p"));
    s.flush().unwrap();
    assert_eq!(s.session("s1").unwrap().unwrap().cwd, "/p");
}

/// L-10：JSONL 第一行斷在多位元組字元中間（寫到一半當掉、或檔案被截壞），不能讓整個 store 開不了；
/// 後面完整的行照樣進索引。
#[test]
fn a_broken_first_line_does_not_stop_the_store() {
    let h = home();
    std::fs::create_dir_all(h.join("events")).unwrap();
    let good = serde_json::to_string(&started("s1", 2, "/p")).unwrap();
    let mut bytes = "{\"v\":1,\"session\":\"s1\",\"seq\":1,\"type\":\"prompt.submitted\",\"payload\":{\"text\":\"中"
        .as_bytes()
        .to_vec();
    bytes.truncate(bytes.len() - 1); // 「中」只留兩個 byte
    bytes.push(b'\n');
    bytes.extend_from_slice(good.as_bytes());
    bytes.push(b'\n');
    std::fs::write(h.join("events/s1.jsonl"), &bytes).unwrap();
    let s = Store::open(&h).expect("壞掉的首行不能擋住啟動");
    assert_eq!(
        s.session("s1").unwrap().unwrap().cwd,
        "/p",
        "後面完整的行照樣索引"
    );
}

/// L-10：只有一個沒寫完的半行（合法 UTF-8、沒有換行）時要截掉，不然新的寫入會黏在它後面。
#[test]
fn a_lone_partial_line_is_cut_before_new_writes() {
    let h = home();
    std::fs::create_dir_all(h.join("events")).unwrap();
    let path = h.join("events/s1.jsonl");
    std::fs::write(&path, "{\"v\":1,\"session\":\"s1\",\"seq\":1,\"ty").unwrap();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("s1", 1, "/p"));
        s.flush().unwrap();
    }
    let text = std::fs::read_to_string(&path).unwrap();
    for l in text.lines() {
        assert!(
            Event::from_json_line(l).is_ok(),
            "每一行都要是完整的 Event：{l}"
        );
    }
    let s = Store::open(&h).unwrap();
    assert_eq!(s.session("s1").unwrap().unwrap().cwd, "/p");
}

/// W-L8：一個 JSONL 讀不到（例如被防毒鎖住）時跳過它，其他 session 照常。
#[cfg(unix)]
#[test]
fn an_unreadable_jsonl_is_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started("a1", 1, "/a"));
        s.append(started("b1", 1, "/b"));
        s.flush().unwrap();
    }
    for f in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
        let _ = std::fs::remove_file(h.join(f));
    }
    let locked = h.join("events/a1.jsonl");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let opened = Store::open(&h);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    let s = opened.expect("一個檔讀不到不能讓 store 開不了");
    assert_eq!(s.session("b1").unwrap().unwrap().cwd, "/b");
}

/// L-14：v2 升級時的 tails 回填遇到非 UTF-8 的行不能失敗（失敗的話版本已經升上去、之後永遠不再回填）；
/// 那一行略過，其他行照樣回填。
#[test]
fn tails_backfill_skips_lines_that_are_not_utf8() {
    let h = home();
    {
        let mut s = Store::open(&h).unwrap();
        s.append(started_at("s1", "/t/s1.jsonl"));
        s.append(at_line("s1", 2, "main", 4));
        s.flush().unwrap();
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(h.join("events/s1.jsonl"))
        .unwrap();
    f.write_all(b"\xff\xfe not json\n").unwrap();
    drop(f);
    let db = rusqlite::Connection::open(h.join("index.sqlite")).unwrap();
    db.execute_batch(
        "DROP TABLE IF EXISTS tails; UPDATE meta SET value = '2' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(db);
    let s = Store::open(&h).expect("回填遇到非 UTF-8 不能失敗");
    let rows: Vec<_> = s
        .tail_lines()
        .unwrap()
        .into_iter()
        .map(|t| (t.file, t.line_no))
        .collect();
    assert_eq!(rows, [("main".to_string(), 4)]);
}

/// S-6（#762、contracts §5）：資料目錄 0700、底下的檔 0600（Claude Code 自己的 transcript 也是 0600）。
/// 原本照預設的 umask（0755／0644），同一台機器的其他使用者讀得到 transcript 的內容。
#[cfg(unix)]
#[test]
fn data_dir_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let h = home().join(".coding-lab");
    let mut s = Store::open(&h).unwrap();
    s.append(started("s1", 1, "/p"));
    s.flush().unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&h), 0o700, "資料目錄");
    assert_eq!(mode(&h.join("events")), 0o700);
    assert_eq!(mode(&h.join("events/s1.jsonl")), 0o600);
    assert_eq!(mode(&h.join("index.sqlite")), 0o600);
    // 舊版建的目錄也收緊
    drop(s);
    std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _s = Store::open(&h).unwrap();
    assert_eq!(mode(&h), 0o700, "重新開啟時收緊舊版的權限");
}
