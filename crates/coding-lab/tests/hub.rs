//! `coding-lab hub`（#663，PLAN「網路綁定」形態 C）：本機對多台 collector 各開一條 WS 訂 `*`，合流後在本機開 web。
//! hub 只轉發不存資料：彙總列照各台算好的（host 改成上游名稱）、KPI 加總、串流每台內照原順序、跨台依到達。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use coding_lab::config::{Config, Overrides};
use coding_lab::event::Event;
use coding_lab::hub::{self, Hub, Out, Upstream};
use coding_lab::store::Store;
use coding_lab::ws::WebState;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Mutex;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
static N: AtomicU32 = AtomicU32::new(0);

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "hub-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ev(s: &str, seq: u64, kind: &str, pos: Option<u64>, ts: u64, payload: Value) -> Event {
    Event::from_json_line(
        &json!({"v": 1, "session": s, "seq": seq, "type": kind, "agent": "main", "line": null, "pos": pos,
                "ts_model": null, "ts_exec": null, "ts_seen": ts, "host": "local", "payload": payload}).to_string(),
    )
    .unwrap()
}
fn started(s: &str, seq: u64, ts: u64, cwd: &str) -> Event {
    ev(
        s,
        seq,
        "session.started",
        None,
        ts,
        json!({"source": "startup", "cwd": cwd, "transcriptPath": null, "model": null}),
    )
}
fn text(s: &str, seq: u64, pos: u64, ts: u64) -> Event {
    ev(
        s,
        seq,
        "assistant.text",
        Some(pos),
        ts,
        json!({"text": format!("t{pos}"), "messageId": "m", "model": "claude-opus-5-5"}),
    )
}

fn snapshot_of(sessions: &[(&str, &str)], stream: Vec<Event>, kpi: Value) -> Value {
    json!({
        "t": "snapshot", "ch": "*", "frame": 7,
        "sessions": sessions.iter().map(|(id, cwd)| json!({"id": id, "host": "local", "cwd": cwd, "status": "run"})).collect::<Vec<_>>(),
        "agents": [], "attention": [], "changes": [], "staged": [], "stream": stream, "kpi": kpi,
    })
}

// ---------- 純邏輯 ----------

#[test]
fn config_reads_hub_bind_and_upstreams() {
    let file = r#"
        [hub]
        bind = "127.0.0.1:4900"
        [[hub.upstream]]
        name = "build-01"
        url = "http://100.64.0.5:4801"
        key = "clk_aaaaaaaa_xxx"
        [[hub.upstream]]
        name = "tunnel"
        url = "http://127.0.0.1:14801"
    "#;
    let c = Config::resolve(
        &Overrides::default(),
        &|_| None,
        Some(file),
        PathBuf::from("/home/u/.coding-lab"),
    )
    .unwrap();
    assert_eq!(c.hub_bind.to_string(), "127.0.0.1:4900");
    assert_eq!(
        c.hub_upstreams,
        vec![
            Upstream {
                name: "build-01".into(),
                url: "http://100.64.0.5:4801".into(),
                key: Some("clk_aaaaaaaa_xxx".into())
            },
            Upstream {
                name: "tunnel".into(),
                url: "http://127.0.0.1:14801".into(),
                key: None
            },
        ]
    );
    let d = Config::resolve(&Overrides::default(), &|_| None, None, PathBuf::from("/x")).unwrap();
    assert_eq!(d.hub_bind.to_string(), hub::DEFAULT_HUB_BIND);
    assert!(d.hub_upstreams.is_empty());
    let bad = Config::resolve(
        &Overrides::default(),
        &|_| None,
        Some("[hub]\nbind = \"0.0.0.0:4802\""),
        PathBuf::from("/x"),
    );
    assert!(
        bad.unwrap_err().contains("hub"),
        "hub 的 web 只能綁 loopback（免金鑰）"
    );
    let dup = Config::resolve(
        &Overrides::default(),
        &|_| None,
        Some(
            "[[hub.upstream]]\nname=\"a\"\nurl=\"http://x:1\"\n[[hub.upstream]]\nname=\"a\"\nurl=\"http://y:1\"",
        ),
        PathBuf::from("/x"),
    );
    assert!(dup.unwrap_err().contains("重複"), "上游名稱不能重複");
}

#[test]
fn ws_url_comes_from_the_http_url() {
    assert_eq!(
        hub::ws_url("http://100.64.0.5:4801").unwrap(),
        "ws://100.64.0.5:4801/ws"
    );
    assert_eq!(
        hub::ws_url("http://127.0.0.1:14801/").unwrap(),
        "ws://127.0.0.1:14801/ws"
    );
    assert!(
        hub::ws_url("https://x:4801").is_err(),
        "TLS 不內建，只接受 http://"
    );
    assert!(hub::ws_url("x:4801").is_err());
}

#[test]
fn merged_snapshot_rewrites_host_sums_kpi_and_keeps_each_upstreams_order() {
    let mut h = Hub::new(vec!["a".into(), "b".into()]);
    // a 的 session 1 兩行（pos 1、2，ts 100、300）；b 的 session 2 一行（ts 200）：跨台依順序鍵、同台內依 pos
    let sa = snapshot_of(
        &[("s1", "/srv/app")],
        vec![text("s1", 1, 1, 100), text("s1", 2, 2, 300)],
        json!({"live": 1, "costUsd": 0.5, "models": {"opus": {"costUsd": 0.5, "thinkingTokens": 10, "ids": ["claude-opus-5-5"]}}}),
    );
    let sb = snapshot_of(
        &[("s2", "/srv/other")],
        vec![text("s2", 1, 1, 200)],
        json!({"live": 2, "costUsd": 0.25, "models": {"opus": {"costUsd": 0.25, "thinkingTokens": 5, "ids": ["claude-opus-5-5"]}, "sonnet": {"costUsd": 0.0, "thinkingTokens": 0, "ids": []}}}),
    );
    let out_a = h.on_message("a", &sa);
    assert!(
        matches!(out_a.as_slice(), [Out::Resnapshot]),
        "上游（重新）連上送 snapshot：hub 的 `*` 訂閱者要重拿合併後的 snapshot：{out_a:?}"
    );
    h.on_message("b", &sb);
    let snap = h.snapshot_all();
    assert_eq!(snap["t"], "snapshot");
    assert_eq!(snap["ch"], "*");
    let hosts: Vec<(&str, &str)> = snap["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s["host"].as_str().unwrap()))
        .collect();
    assert_eq!(hosts, vec![("s1", "a"), ("s2", "b")]);
    assert_eq!(snap["kpi"]["live"], 3);
    assert!((snap["kpi"]["costUsd"].as_f64().unwrap() - 0.75).abs() < 1e-9);
    assert_eq!(snap["kpi"]["models"]["opus"]["thinkingTokens"], 15);
    assert_eq!(snap["kpi"]["models"]["sonnet"]["thinkingTokens"], 0);
    let order: Vec<(String, u64)> = snap["stream"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["session"].as_str().unwrap().into(),
                e["pos"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![("s1".into(), 1), ("s2".into(), 1), ("s1".into(), 2)]
    );
    assert!(
        snap["stream"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["host"] == "a" || e["host"] == "b")
    );
    assert_eq!(snap["stream"][0]["host"], "a");
    assert_eq!(h.owner_of_session("s2").as_deref(), Some("b"));
    assert_eq!(
        h.owner_of_cwd("/srv/app/worktrees/x").as_deref(),
        Some("a"),
        "cwd 在某台 session 的 cwd 底下就歸那台"
    );
    assert_eq!(h.owner_of_cwd("/nowhere"), None);
}

#[test]
fn frames_are_renumbered_rewritten_and_appended_in_arrival_order() {
    let mut h = Hub::new(vec!["a".into(), "b".into()]);
    h.on_message(
        "a",
        &snapshot_of(&[("s1", "/a")], vec![], json!({"live": 1})),
    );
    h.on_message(
        "b",
        &snapshot_of(&[("s2", "/b")], vec![], json!({"live": 1})),
    );
    let fb = json!({"t": "frame", "frame": 8, "ch": "*", "events": [text("s2", 5, 9, 900)], "rows": {"sessions": [{"id": "s2", "status": "wait"}], "kpi": {"live": 0}}});
    let fa =
        json!({"t": "frame", "frame": 8, "ch": "*", "events": [text("s1", 5, 9, 100)], "rows": {}});
    let out_b = h.on_message("b", &fb);
    let out_a = h.on_message("a", &fa);
    let (Out::All(f1), Out::All(f2)) = (&out_b[0], &out_a[0]) else {
        panic!("{out_b:?} {out_a:?}")
    };
    assert!(
        f1["frame"].as_u64().unwrap() < f2["frame"].as_u64().unwrap(),
        "hub 自己編號、依到達順序"
    );
    assert_eq!(f1["events"][0]["host"], "b");
    assert_eq!(
        f1["rows"]["sessions"][0]["host"], "b",
        "rows 的 sessions 列也要帶 host（前端用它標 [host]）"
    );
    assert_eq!(
        f1["rows"]["kpi"]["live"], 1,
        "kpi 是全部上游加總後的完整值（b 變 0、a 還是 1）"
    );
    assert_eq!(f2["events"][0]["host"], "a");
    assert!(
        f2["rows"].get("kpi").is_none(),
        "這個 frame 沒有 kpi 變化就不送"
    );
    // 之後新來的 `*` 訂閱者拿 snapshot：frame 的 Event 也要在裡面（hub 不存資料，但要接得上）
    let snap = h.snapshot_all();
    let ids: Vec<&str> = snap["stream"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["session"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["s2", "s1"], "依到達順序");
    assert_eq!(
        snap["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "s2")
            .unwrap()["status"],
        "wait",
        "rows 的更新要併進快取"
    );
}

#[test]
fn session_channel_messages_are_forwarded_to_that_sessions_subscribers_only() {
    let mut h = Hub::new(vec!["a".into()]);
    h.on_message("a", &snapshot_of(&[("s1", "/a")], vec![], json!({})));
    let snap = json!({"t": "snapshot", "ch": "session:s1", "frame": 9, "sessions": [{"id": "s1", "host": "local"}], "agents": [], "attention": [], "staged": [], "stream": []});
    let out = h.on_message("a", &snap);
    let [Out::Session(id, v)] = out.as_slice() else {
        panic!("{out:?}")
    };
    assert_eq!(id, "s1");
    assert_eq!(v["sessions"][0]["host"], "a");
    let frame = json!({"t": "frame", "frame": 10, "ch": "session:s1", "events": [], "rows": {"sessions": [{"id": "s1", "status": "done"}]}});
    let out = h.on_message("a", &frame);
    let [Out::Session(id, v)] = out.as_slice() else {
        panic!("{out:?}")
    };
    assert_eq!(id, "s1");
    assert_eq!(v["ch"], "session:s1");
    assert_eq!(v["rows"]["sessions"][0]["host"], "a");
    // more 的回覆：前端用 (session, beforePos) 對應、沒在等的會忽略，所以直接給所有連線
    let more =
        json!({"t": "more", "session": "s1", "beforePos": 3, "events": [text("s1", 1, 1, 1)]});
    let out = h.on_message("a", &more);
    let [Out::Any(v)] = out.as_slice() else {
        panic!("{out:?}")
    };
    assert_eq!(v["events"][0]["host"], "a");
}

#[test]
fn offline_upstream_marks_its_sessions_and_a_view_key_cannot_ack() {
    let mut h = Hub::new(vec!["a".into(), "b".into()]);
    h.on_welcome(
        "a",
        &json!({"t": "welcome", "scope": "view", "thinking": "hide"}),
    );
    h.on_welcome(
        "b",
        &json!({"t": "welcome", "scope": "operate", "thinking": "on_demand"}),
    );
    let first = h.on_message(
        "a",
        &snapshot_of(&[("s1", "/a"), ("s3", "/a")], vec![], json!({"live": 2})),
    );
    assert!(
        matches!(first.as_slice(), [Out::Resnapshot]),
        "第一次連上沒有消失的列，只重送 snapshot：{first:?}"
    );
    h.on_message(
        "b",
        &snapshot_of(&[("s2", "/b")], vec![], json!({"live": 1})),
    );
    assert!(!h.can_ack("a"));
    assert!(h.can_ack("b"));
    let out = h.on_offline("a");
    let [Out::All(f)] = out.as_slice() else {
        panic!("{out:?}")
    };
    let rows = f["rows"]["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|r| r["offline"] == true && r["host"] == "a")
    );
    assert_eq!(f["rows"]["kpi"]["live"], 1, "離線那台的 KPI 不再加總");
    // #724：每台上游的 remote_thinking 隨 sessions 列帶給前端，前端才知道 hide 的上游點不開
    let snap0 = h.snapshot_all();
    let th = |snap: &Value, id: &str| {
        snap["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .map(|s| s["thinking"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(
        th(&snap0, "s1"),
        "hide",
        "a 的 session 帶 a 的 thinking 模式"
    );
    assert_eq!(th(&snap0, "s2"), "on_demand");
    assert_eq!(
        h.online(),
        vec![("a".to_string(), false), ("b".to_string(), true)]
    );
    let snap = h.snapshot_all();
    let s1 = snap["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "s1")
        .unwrap();
    assert_eq!(
        s1["offline"], true,
        "離線期間 snapshot 仍列出它的 session、標離線"
    );
    // 重連後的 snapshot 把 offline 拿掉；不在新 snapshot 裡的 s3 要明確送 deleted（已開著的客戶端才會清掉，#723），
    // 然後才重送合併的 snapshot
    let outs = h.on_message(
        "a",
        &snapshot_of(&[("s1", "/a")], vec![], json!({"live": 1})),
    );
    let [Out::All(gone), Out::Resnapshot] = outs.as_slice() else {
        panic!("重連的 snapshot 要先送消失的列再 Resnapshot：{outs:?}")
    };
    let rows = gone["rows"]["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], "s3");
    assert_eq!(rows[0]["host"], "a");
    assert_eq!(rows[0]["deleted"], true);
    let snap = h.snapshot_all();
    let s1 = snap["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "s1")
        .unwrap();
    assert!(!s1.get("offline").and_then(Value::as_bool).unwrap_or(false));
    assert_eq!(
        snap["sessions"].as_array().unwrap().len(),
        2,
        "s3 已不在新 snapshot 裡就不列"
    );
}

#[test]
fn http_response_parsing_handles_content_length_and_chunked() {
    let r = hub::parse_http_response(b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n{\"error\":\"nope\"}").unwrap();
    assert_eq!(
        (r.status, r.content_type.as_str()),
        (404, "application/json")
    );
    assert_eq!(r.body, b"{\"error\":\"nope\"}");
    let r = hub::parse_http_response(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n").unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"{\"a\":1}");
    assert!(hub::parse_http_response(b"garbage").is_err());
}

#[test]
fn hub_add_appends_an_upstream_block_with_private_permissions_and_rejects_duplicates() {
    let dir = tmp("add");
    let path = dir.join("config.toml");
    std::fs::write(&path, "[web]\nbind = \"127.0.0.1:4801\"\n").unwrap();
    hub::append_upstream(
        &path,
        "build-01",
        "http://100.64.0.5:4801",
        Some("clk_aaaaaaaa_secret"),
    )
    .unwrap();
    hub::append_upstream(&path, "tunnel", "http://127.0.0.1:14801", None).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("[web]\nbind = \"127.0.0.1:4801\"\n"),
        "原有內容不動：{text}"
    );
    let c = Config::resolve(&Overrides::default(), &|_| None, Some(&text), dir.clone()).unwrap();
    assert_eq!(c.web_bind.to_string(), "127.0.0.1:4801");
    assert_eq!(c.hub_upstreams.len(), 2);
    assert_eq!(
        c.hub_upstreams[0].key.as_deref(),
        Some("clk_aaaaaaaa_secret")
    );
    assert_eq!(c.hub_upstreams[1].key, None);
    let err = hub::append_upstream(&path, "tunnel", "http://x:1", None).unwrap_err();
    assert!(err.contains("tunnel"), "{err}");
    assert!(
        hub::append_upstream(&path, "bad name", "http://x:1", None).is_err(),
        "名稱只能是 [A-Za-z0-9._-]"
    );
    assert!(
        hub::append_upstream(&path, "x", "https://x:1", None).is_err(),
        "只接受 http://"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "金鑰在裡面：600"
        );
    }
}

// ---------- 兩台真的 collector web 入口 + hub ----------

/// 一台有 store 的 collector web 入口（/api/sessions 從 store 列）
fn collector(name: &str, events: &[Event]) -> Arc<WebState> {
    let mut s = Store::open(&tmp(name)).unwrap();
    for e in events {
        s.append(e.clone());
    }
    s.flush().unwrap();
    let web = WebState::new(Some(Arc::new(Mutex::new(s))), None, 64);
    web.apply(events);
    web.flush(2000);
    web
}

async fn upstream(web: &Arc<WebState>) -> SocketAddr {
    coding_lab::ws::serve_web("127.0.0.1:0".parse().unwrap(), web.clone())
        .await
        .unwrap()
        .0
}

async fn connect(addr: SocketAddr) -> Ws {
    let req = format!("ws://{addr}/ws").into_client_request().unwrap();
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("等 hub 的訊息逾時")
            .expect("連線結束")
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn recv_until(ws: &mut Ws, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..50 {
        let v = recv(ws).await;
        if pred(&v) {
            return v;
        }
    }
    panic!("等不到符合條件的訊息");
}

async fn http_get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let r = hub::http_get(&format!("http://{addr}"), path, None)
        .await
        .unwrap();
    (
        r.status,
        serde_json::from_slice(&r.body).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn hub_merges_two_collectors_and_follows_frames_from_both() {
    let a = collector(
        "a",
        &[started("s1", 1, 1000, "/srv/app"), text("s1", 2, 1, 1001)],
    );
    let b = collector(
        "b",
        &[started("s2", 1, 1000, "/srv/other"), text("s2", 2, 1, 1002)],
    );
    let (addr_a, addr_b) = (upstream(&a).await, upstream(&b).await);
    let ups = vec![
        Upstream {
            name: "alpha".into(),
            url: format!("http://{addr_a}"),
            key: None,
        },
        Upstream {
            name: "beta".into(),
            url: format!("http://{addr_b}"),
            key: None,
        },
    ];
    let (hub_addr, _task) = hub::serve("127.0.0.1:0".parse().unwrap(), ups, None)
        .await
        .unwrap();

    // healthz 列出上游與連線狀態
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, h) = http_get(hub_addr, "/healthz").await;
        if h["upstreams"]
            .as_array()
            .is_some_and(|u| u.iter().all(|x| x["online"] == true))
        {
            assert_eq!(h["hub"], true);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hub 沒有連上兩台上游：{h}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut ws = connect(hub_addr).await;
    send(
        &mut ws,
        json!({"op": "hello", "protocol": 1, "utcOffsetMin": 480}),
    )
    .await;
    let welcome = recv(&mut ws).await;
    assert_eq!(welcome["t"], "welcome");
    assert_eq!(welcome["host"], "hub");
    assert_eq!(welcome["protocol"], coding_lab::ws::PROTOCOL);
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let snap = recv_until(&mut ws, |v| v["t"] == "snapshot").await;
    let mut hosts: Vec<(String, String)> = snap["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().into(),
                s["host"].as_str().unwrap().into(),
            )
        })
        .collect();
    hosts.sort();
    assert_eq!(
        hosts,
        vec![
            ("s1".to_string(), "alpha".to_string()),
            ("s2".to_string(), "beta".to_string())
        ]
    );
    assert_eq!(snap["kpi"]["live"], 2, "兩台的 KPI 加總");
    assert_eq!(snap["stream"].as_array().unwrap().len(), 2);

    // 之後 a 又來一行：hub 的 frame 帶 host
    a.apply(&[text("s1", 3, 2, 3000)]);
    a.flush(3000);
    let frame = recv_until(&mut ws, |v| {
        v["t"] == "frame" && !v["events"].as_array().unwrap().is_empty()
    })
    .await;
    assert_eq!(frame["events"][0]["host"], "alpha");
    assert_eq!(frame["events"][0]["pos"], 2);

    // 單 session 頻道：轉給擁有它的那台，snapshot 回來 host 也改好
    send(&mut ws, json!({"op": "subscribe", "channel": "session:s2"})).await;
    let s2 = recv_until(&mut ws, |v| v["t"] == "snapshot" && v["ch"] == "session:s2").await;
    assert_eq!(s2["sessions"][0]["host"], "beta");

    // HTTP：/api/sessions 合併兩台；帶 cwd 的查詢轉給擁有那個目錄的那台；不認得的 cwd 404
    let (st, body) = http_get(hub_addr, "/api/sessions?limit=10").await;
    assert_eq!(st, 200, "{body}");
    let mut got: Vec<(String, String)> = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().into(),
                s["host"].as_str().unwrap().into(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("s1".to_string(), "alpha".to_string()),
            ("s2".to_string(), "beta".to_string())
        ]
    );
    let (st, body) = http_get(hub_addr, "/api/sessions?cwd=%2Fsrv%2Fother").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(body["sessions"][0]["host"], "beta");
    let (st, _) = http_get(hub_addr, "/api/files?cwd=%2Fnowhere&base=main").await;
    assert_eq!(st, 404);
    // 帶 session id 的路徑轉給擁有它的那台；回應裡的 Event 也標 host
    let (st, body) = http_get(hub_addr, "/api/sessions/s1/events?limit=5").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["events"][0]["host"], "alpha");
    let (st, _) = http_get(hub_addr, "/api/sessions/unknown/events").await;
    assert_eq!(st, 404);
}

#[tokio::test]
async fn upstream_going_away_marks_its_sessions_offline_and_reconnect_restores_them() {
    // b 跑在自己的 runtime 上：丟掉 runtime 就等於那台斷線（連線全部關掉），之後在同一個埠重開
    let a = WebState::new(None, None, 64);
    a.apply(&[started("s1", 1, 1000, "/a"), text("s1", 2, 1, 1001)]);
    a.flush(2000);
    let addr_a = upstream(&a).await;
    let port_b = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let addr_b: SocketAddr = format!("127.0.0.1:{port_b}").parse().unwrap();
    let start_b = |events: Vec<Event>| {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let b = WebState::new(None, None, 64);
                b.apply(&events);
                b.flush(2000);
                coding_lab::ws::serve_web(addr_b, b).await.unwrap();
                ready_tx.send(()).unwrap();
                let _ = tokio::task::spawn_blocking(move || rx.recv()).await;
            });
            drop(rt);
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        tx
    };
    let stop_b = start_b(vec![started("s2", 1, 1000, "/b"), text("s2", 2, 1, 1002)]);
    let ups = vec![
        Upstream {
            name: "alpha".into(),
            url: format!("http://{addr_a}"),
            key: None,
        },
        Upstream {
            name: "beta".into(),
            url: format!("http://{addr_b}"),
            key: None,
        },
    ];
    let (hub_addr, _task) = hub::serve_with_backoff(
        "127.0.0.1:0".parse().unwrap(),
        ups,
        None,
        Duration::from_millis(100),
    )
    .await
    .unwrap();
    let mut ws = connect(hub_addr).await;
    send(&mut ws, json!({"op": "hello", "protocol": 1})).await;
    recv(&mut ws).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    // 兩台都連上後的 snapshot（上游連上時 hub 會重送 snapshot，等到有兩個 session 的那份）
    recv_until(&mut ws, |v| {
        v["t"] == "snapshot" && v["sessions"].as_array().is_some_and(|s| s.len() == 2)
    })
    .await;

    drop(stop_b); // b 斷線
    let off = recv_until(&mut ws, |v| {
        v["t"] == "frame"
            && v["rows"]["sessions"]
                .as_array()
                .is_some_and(|s| s.iter().any(|r| r["offline"] == true))
    })
    .await;
    assert_eq!(off["rows"]["sessions"][0]["id"], "s2");
    assert_eq!(off["rows"]["sessions"][0]["host"], "beta");
    // a 還是照常
    a.apply(&[text("s1", 3, 2, 3000)]);
    a.flush(3000);
    let f = recv_until(&mut ws, |v| {
        v["t"] == "frame" && !v["events"].as_array().unwrap().is_empty()
    })
    .await;
    assert_eq!(f["events"][0]["host"], "alpha");

    // b 回來（多了一行）：hub 重連後重送 snapshot，s2 不再離線、新行也在
    let _stop_b2 = start_b(vec![
        started("s2", 1, 1000, "/b"),
        text("s2", 2, 1, 1002),
        text("s2", 3, 2, 4000),
    ]);
    let snap = recv_until(&mut ws, |v| {
        v["t"] == "snapshot"
            && v["sessions"].as_array().is_some_and(|s| {
                s.iter().any(|r| {
                    r["id"] == "s2" && r.get("offline").and_then(Value::as_bool) != Some(true)
                })
            })
    })
    .await;
    assert!(
        snap["stream"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["session"] == "s2" && e["pos"] == 2),
        "重連後補齊"
    );
}

// ---------- #718：心跳與讀取逾時、時區變了不重連 ----------

#[test]
fn heartbeat_pings_every_10s_and_declares_dead_after_15s_without_any_message() {
    use coding_lab::hub::{Beat, Heartbeat};
    let mut hb = Heartbeat::new(0);
    assert_eq!(hb.tick(5_000), Beat::Nothing);
    assert_eq!(hb.tick(10_000), Beat::Ping, "10 秒沒送過 Ping 就送");
    assert_eq!(hb.tick(12_000), Beat::Nothing, "剛送過不重送");
    assert_eq!(
        hb.tick(15_000),
        Beat::Dead,
        "15 秒沒有任何訊息（Pong、frame）就當斷線"
    );
    // 任何訊息都重置：14 秒時收到 Pong，16 秒還活著、20 秒才送下一個 Ping、29 秒才死
    let mut hb = Heartbeat::new(0);
    assert_eq!(hb.tick(10_000), Beat::Ping);
    hb.on_activity(14_000);
    assert_eq!(hb.tick(16_000), Beat::Nothing);
    assert_eq!(hb.tick(19_000), Beat::Nothing);
    assert_eq!(hb.tick(24_000), Beat::Ping, "Ping 的間隔從最後一次活動算");
    assert_eq!(hb.tick(28_900), Beat::Nothing);
    assert_eq!(hb.tick(29_000), Beat::Dead);
}

#[tokio::test]
async fn clients_with_different_offsets_do_not_make_the_hub_reconnect_its_upstreams() {
    let a = collector(
        "a-offset",
        &[started("s1", 1, 1000, "/a"), text("s1", 2, 1, 1001)],
    );
    let addr_a = upstream(&a).await;
    let ups = vec![Upstream {
        name: "alpha".into(),
        url: format!("http://{addr_a}"),
        key: None,
    }];
    let (hub_addr, _task) = hub::serve("127.0.0.1:0".parse().unwrap(), ups, None)
        .await
        .unwrap();
    let connects = |h: &Value| h["upstreams"][0]["connects"].as_u64().unwrap_or(0);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, h) = http_get(hub_addr, "/healthz").await;
        if h["upstreams"][0]["online"] == true {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{h}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 瀏覽器（+480）與測試腳本（不帶 offset）輪流連：上游最多重連一次（第一個明確的時區），之後不再變
    let mut browser = connect(hub_addr).await;
    send(
        &mut browser,
        json!({"op": "hello", "protocol": 1, "utcOffsetMin": 480}),
    )
    .await;
    recv(&mut browser).await;
    // 那一次重連要等退避（1 秒）＋重新連上，給它 3 秒安定
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let (_, h) = http_get(hub_addr, "/healthz").await;
    let after_first = connects(&h);
    assert!(after_first <= 2, "第一個明確時區最多重連一次：{h}");
    for offset in [None, Some(0), Some(480), Some(-300), None] {
        let mut c = connect(hub_addr).await;
        let mut hello = json!({"op": "hello", "protocol": 1});
        if let Some(o) = offset {
            hello["utcOffsetMin"] = json!(o);
        }
        send(&mut c, hello).await;
        recv(&mut c).await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let (_, h) = http_get(hub_addr, "/healthz").await;
        assert_eq!(
            connects(&h),
            after_first,
            "之後的客戶端不管帶什麼時區都不重連：{h}"
        );
        assert_eq!(h["upstreams"][0]["online"], true);
    }
}

/// 上游 store 裡有、live 裡沒有的 session（已結束、被移出記憶體）：hub 的 snapshot 快取不認得它，
/// 但 Sessions 分頁列得出來，點進去要能回放與載入更多（#726，#715 第三輪實機找到）
fn store_only_collector(name: &str, live: &[Event], stored_only: &[Event]) -> Arc<WebState> {
    let mut s = Store::open(&tmp(name)).unwrap();
    for e in live.iter().chain(stored_only) {
        s.append(e.clone());
    }
    s.flush().unwrap();
    let web = WebState::new(Some(Arc::new(Mutex::new(s))), None, 64);
    web.apply(live);
    web.flush(2000);
    web
}

#[tokio::test]
async fn hub_forwards_sessions_that_are_only_in_an_upstreams_store() {
    let a = store_only_collector(
        "a-old",
        &[started("live1", 1, 1000, "/a"), text("live1", 2, 1, 1001)],
        &[
            started("old1", 1, 500, "/a"),
            text("old1", 2, 1, 501),
            text("old1", 3, 2, 502),
        ],
    );
    let b = collector(
        "b-old",
        &[started("s2", 1, 1000, "/b"), text("s2", 2, 1, 1001)],
    );
    let (addr_a, addr_b) = (upstream(&a).await, upstream(&b).await);
    let ups = vec![
        Upstream {
            name: "alpha".into(),
            url: format!("http://{addr_a}"),
            key: None,
        },
        Upstream {
            name: "beta".into(),
            url: format!("http://{addr_b}"),
            key: None,
        },
    ];
    let (hub_addr, _task) = hub::serve("127.0.0.1:0".parse().unwrap(), ups, None)
        .await
        .unwrap();
    let mut ws = connect(hub_addr).await;
    send(&mut ws, json!({"op": "hello", "protocol": 1})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let snap = recv_until(&mut ws, |v| {
        v["t"] == "snapshot"
            && v["ch"] == "*"
            && v["sessions"].as_array().is_some_and(|a| a.len() >= 2)
    })
    .await;
    assert!(
        !snap["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == "old1"),
        "前提：old1 不在 live，hub 的快取沒有它"
    );

    // 沒先打 /api/sessions：hub 逐台試，第一個回 200 的就是擁有者
    let (st, body) = http_get(hub_addr, "/api/sessions/old1/events?limit=10").await;
    assert_eq!(st, 200, "快取以外的 session 也要轉發得到：{body}");
    let evs = body["events"].as_array().unwrap();
    assert!(!evs.is_empty(), "{body}");
    assert!(
        evs.iter().all(|e| e["host"] == "alpha"),
        "回應的 Event 帶 host：{body}"
    );

    // WS 的 more 也轉給擁有者（回放與載入更多走這條）
    send(
        &mut ws,
        json!({"op": "more", "session": "old1", "beforePos": 99, "limit": 50}),
    )
    .await;
    let more = recv_until(&mut ws, |v| v["t"] == "more" && v["session"] == "old1").await;
    assert!(
        !more["events"].as_array().unwrap().is_empty(),
        "more 要拿到 old1 的事件，不是空的：{more}"
    );

    // 訂 session 頻道也不再回「不認得」
    send(
        &mut ws,
        json!({"op": "subscribe", "channel": "session:old1"}),
    )
    .await;
    let s = recv_until(&mut ws, |v| {
        (v["t"] == "snapshot" && v["ch"] == "session:old1") || v["t"] == "error"
    })
    .await;
    assert_eq!(s["t"], "snapshot", "{s}");

    // 先打過 /api/sessions 的也記得（另一個 id 走對照表，不必再試）
    let (st, _) = http_get(hub_addr, "/api/sessions?limit=50").await;
    assert_eq!(st, 200);
    let (st, body) = http_get(hub_addr, "/api/sessions/s2/events?limit=10").await;
    assert_eq!(st, 200, "{body}");

    // 哪台都沒有的 id 仍 404
    let (st, _) = http_get(hub_addr, "/api/sessions/nope/events").await;
    assert_eq!(st, 404);
}
