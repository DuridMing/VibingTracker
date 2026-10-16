//! ws：頻道、frame、snapshot、resume、Origin、ack、more、靜態檔（contracts.md §6–§7、#636）。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use coding_lab::event::Event;
use coding_lab::store::Store;
use coding_lab::ws::{WebState, serve_web};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

static N: AtomicU32 = AtomicU32::new(0);

fn tmp(name: &str) -> std::path::PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ws-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ev(s: &str, seq: u64, kind: &str, pos: Option<u64>, payload: Value) -> Event {
    Event::from_json_line(
        &json!({"v": 1, "session": s, "seq": seq, "type": kind, "agent": "main", "line": null, "pos": pos,
                "ts_model": null, "ts_exec": null, "ts_seen": 1_000 + seq, "host": "local", "payload": payload})
        .to_string(),
    )
    .unwrap()
}

fn text(s: &str, seq: u64, pos: u64) -> Event {
    ev(
        s,
        seq,
        "assistant.text",
        Some(pos),
        json!({"text": "x", "messageId": "m", "model": "claude-opus-5-5"}),
    )
}

async fn start(state: Arc<WebState>) -> SocketAddr {
    serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap()
        .0
}

async fn connect(
    addr: SocketAddr,
    origin: Option<&str>,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("Origin", o.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("5 秒內沒收到")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn hello(ws: &mut Ws, extra: Value) -> Value {
    let mut msg = json!({"op": "hello", "protocol": 1, "utcOffsetMin": 0});
    if let (Some(m), Some(e)) = (msg.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    send(ws, msg).await;
    recv(ws).await
}

#[tokio::test]
async fn welcome_snapshot_then_frames_filtered_by_channel() {
    let state = WebState::new(None, None, 64);
    state.apply(&[text("s1", 1, 1)]);
    state.flush(2_000);
    let addr = start(state.clone()).await;

    let mut all = connect(addr, None).await.unwrap();
    let w = hello(&mut all, json!({})).await;
    assert_eq!(
        (
            w["t"].as_str(),
            w["protocol"].as_u64(),
            w["remote"].as_bool()
        ),
        (Some("welcome"), Some(1), Some(false))
    );
    send(&mut all, json!({"op": "subscribe", "channel": "*"})).await;
    let snap = recv(&mut all).await;
    assert_eq!(
        (snap["t"].as_str(), snap["ch"].as_str()),
        (Some("snapshot"), Some("*"))
    );
    assert_eq!(snap["stream"].as_array().unwrap().len(), 1);
    assert!(snap["kpi"].is_object());

    let mut one = connect(addr, None).await.unwrap();
    hello(&mut one, json!({})).await;
    send(
        &mut one,
        json!({"op": "subscribe", "channel": "session:s2"}),
    )
    .await;
    let snap2 = recv(&mut one).await;
    assert_eq!(snap2["ch"], "session:s2");
    assert!(snap2.get("kpi").is_none());

    state.apply(&[text("s1", 2, 2), text("s2", 1, 1)]);
    state.flush(3_000);
    let f = recv(&mut all).await;
    assert_eq!(f["t"], "frame");
    let sessions: Vec<&str> = f["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["session"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, ["s1", "s2"], "`*` 收到兩個 session");
    let f2 = recv(&mut one).await;
    let only: Vec<&str> = f2["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["session"].as_str().unwrap())
        .collect();
    assert_eq!(only, ["s2"], "session:<id> 只含自己");
}

#[tokio::test]
async fn cross_site_origin_is_rejected() {
    let state = WebState::new(None, None, 8);
    let addr = start(state).await;
    assert!(
        connect(addr, Some("https://evil.example")).await.is_err(),
        "別的網站不能讀本機的 transcript"
    );
    assert!(
        connect(addr, Some(&format!("http://{addr}"))).await.is_ok(),
        "同源可以"
    );
    assert!(
        connect(addr, None).await.is_ok(),
        "沒帶 Origin 的程式（wscat）可以"
    );
}

#[tokio::test]
async fn resume_sends_missed_frames_instead_of_snapshot() {
    let state = WebState::new(None, None, 64);
    let addr = start(state.clone()).await;
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let last = recv(&mut ws).await["frame"].as_u64().unwrap();
    drop(ws);

    state.apply(&[text("s1", 1, 1)]);
    state.flush(2_000);
    state.apply(&[text("s1", 2, 2)]);
    state.flush(3_000);

    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({"resume": last})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let a = recv(&mut ws).await;
    let b = recv(&mut ws).await;
    assert_eq!(
        (a["t"].as_str(), b["t"].as_str()),
        (Some("frame"), Some("frame"))
    );
    assert_eq!(
        b["frame"].as_u64().unwrap(),
        a["frame"].as_u64().unwrap() + 1
    );

    // 接不上（編號不合理）就送 snapshot
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({"resume": 999_999})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    assert_eq!(recv(&mut ws).await["t"], "snapshot");
}

#[tokio::test]
async fn lagging_client_gets_a_fresh_snapshot() {
    let state = WebState::new(None, None, 2);
    let addr = start(state.clone()).await;
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    recv(&mut ws).await;
    for i in 1..=20 {
        state.apply(&[text("s1", i, i)]);
        state.flush(1_000 + i);
    }
    // 落後超過 broadcast 容量：最後會收到一份 snapshot，裡面有全部 20 行
    let mut got_snapshot = None;
    for _ in 0..30 {
        let m = recv(&mut ws).await;
        if m["t"] == "snapshot" {
            got_snapshot = Some(m);
            break;
        }
    }
    let snap = got_snapshot.expect("落後時要重送 snapshot");
    assert_eq!(snap["stream"].as_array().unwrap().len(), 20);
}

#[tokio::test]
async fn ack_removes_attention_and_is_stored() {
    let home = tmp("ack");
    let store = Arc::new(std::sync::Mutex::new(Store::open(&home).unwrap()));
    let raised = ev(
        "s1",
        1,
        "attention.raised",
        None,
        json!({"kind": "danger", "ref": "t1", "reason": "rm"}),
    );
    {
        let mut s = store.lock().unwrap();
        s.append(raised.clone());
        s.flush().unwrap();
    }
    let state = WebState::new(Some(store.clone()), None, 64);
    state.apply(&[raised]);
    state.flush(2_000);
    let addr = start(state.clone()).await;
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    assert_eq!(
        recv(&mut ws).await["attention"].as_array().unwrap().len(),
        1
    );

    send(
        &mut ws,
        json!({"op": "ack", "session": "s1", "kind": "danger", "ref": "t1"}),
    )
    .await;
    // 下一個 frame 帶刪除
    let f = recv(&mut ws).await;
    assert_eq!(
        f["rows"]["attention"][0],
        json!({"id": "s1/danger/t1", "deleted": true})
    );
    assert!(
        store.lock().unwrap().open_attention().unwrap().is_empty(),
        "store 也記已確認"
    );
}

/// 「載入更多」也附卡片的伴隨 Event 與狀態行，並回傳請求的 session、beforePos。
#[tokio::test]
async fn more_includes_companions_and_status_lines() {
    let home = tmp("more-companions");
    let store = Arc::new(std::sync::Mutex::new(Store::open(&home).unwrap()));
    {
        let mut s = store.lock().unwrap();
        s.append(ev(
            "s1",
            1,
            "prompt.submitted",
            None,
            json!({"text": "修 bug", "promptId": "p1"}),
        ));
        s.append(ev(
            "s1",
            2,
            "tool.called",
            None,
            json!({"toolUseId": "t1", "tool": "Bash", "input": {"command": "ls"},
            "status": "running", "placement": "staged", "danger": null}),
        ));
        s.append(ev(
            "s1",
            3,
            "tool.placed",
            Some(1),
            json!({"toolUseId": "t1", "placement": "stream"}),
        ));
        s.append(ev("s1", 4, "tool.result", None, json!({"toolUseId": "t1", "ok": true, "exitCode": 0, "output": "a\n", "truncated": false})));
        s.append(text("s1", 5, 2));
        s.append(ev(
            "s1",
            6,
            "prompt.submitted",
            None,
            json!({"text": "下一個", "promptId": "p2"}),
        ));
        s.append(text("s1", 7, 3));
        s.flush().unwrap();
    }
    let state = WebState::new(Some(store), None, 8);
    let addr = start(state).await;
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({})).await;
    send(
        &mut ws,
        json!({"op": "more", "session": "s1", "beforePos": 3, "limit": 10}),
    )
    .await;
    let m = recv(&mut ws).await;
    assert_eq!(
        (m["session"].as_str(), m["beforePos"].as_u64()),
        (Some("s1"), Some(3))
    );
    let types: Vec<&str> = m["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "prompt.submitted",
            "tool.called",
            "tool.placed",
            "tool.result",
            "assistant.text",
            "prompt.submitted"
        ],
        "伴隨 Event 與窗口內的狀態行；pos 3 之後的不送"
    );
}

#[tokio::test]
async fn more_reads_older_stream_from_store() {
    let home = tmp("more");
    let store = Arc::new(std::sync::Mutex::new(Store::open(&home).unwrap()));
    {
        let mut s = store.lock().unwrap();
        for i in 1..=10 {
            s.append(text("s1", i, i));
        }
        s.flush().unwrap();
    }
    let state = WebState::new(Some(store), None, 8);
    let addr = start(state).await;
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({})).await;
    send(
        &mut ws,
        json!({"op": "more", "session": "s1", "beforePos": 6, "limit": 3}),
    )
    .await;
    let m = recv(&mut ws).await;
    assert_eq!(m["t"], "more");
    let pos: Vec<u64> = m["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["pos"].as_u64())
        .collect();
    assert_eq!(pos, [3, 4, 5]);
}

async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn static_files_and_healthz() {
    let root = tmp("web");
    std::fs::write(root.join("index.html"), "<!doctype html><title>t</title>").unwrap();
    std::fs::create_dir_all(root.join("js")).unwrap();
    std::fs::write(root.join("js/app.js"), "console.log(1)").unwrap();
    std::fs::write(root.parent().unwrap().join("secret.txt"), "no").unwrap();
    let state = WebState::new(None, Some(root), 8);
    let addr = start(state).await;

    let index = http_get(addr, "/").await;
    assert!(
        index.starts_with("HTTP/1.1 200") && index.contains("text/html"),
        "{index}"
    );
    let js = http_get(addr, "/js/app.js").await;
    assert!(
        js.contains("javascript") && js.ends_with("console.log(1)"),
        "{js}"
    );
    assert!(
        http_get(addr, "/../secret.txt")
            .await
            .starts_with("HTTP/1.1 404")
    );
    assert!(
        http_get(addr, "/%2e%2e/secret.txt")
            .await
            .starts_with("HTTP/1.1 404")
    );
    let health = http_get(addr, "/healthz").await;
    assert!(
        health.starts_with("HTTP/1.1 200") && health.contains("\"protocol\":1"),
        "{health}"
    );
}

#[tokio::test]
async fn non_loopback_bind_is_refused_until_keys_exist() {
    let state = WebState::new(None, None, 8);
    assert!(
        serve_web("0.0.0.0:0".parse().unwrap(), state)
            .await
            .is_err(),
        "金鑰驗證（P6）之前不開放遠端"
    );
}

async fn http_get_host(addr: SocketAddr, path: &str, host: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

/// #670-6：DNS rebinding。攻擊者把自己的網域解析到 127.0.0.1，瀏覽器送出的 Origin 和 Host 都是那個網域、
/// 彼此相同，所以只比 Origin == Host 擋不住；Host 必須是 loopback 名稱。
#[tokio::test]
async fn non_loopback_host_is_rejected_even_when_origin_matches() {
    let state = WebState::new(None, None, 8);
    let addr = start(state).await;
    let port = addr.port();
    let evil = format!("evil.example:{port}");

    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("Host", evil.parse().unwrap());
    req.headers_mut()
        .insert("Origin", format!("http://{evil}").parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(req).await.is_err(),
        "rebinding 的網頁不能開 WS"
    );
    assert!(
        http_get_host(addr, "/healthz", &evil)
            .await
            .starts_with("HTTP/1.1 403"),
        "HTTP 也要擋"
    );

    for host in [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ] {
        let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
        req.headers_mut().insert("Host", host.parse().unwrap());
        req.headers_mut()
            .insert("Origin", format!("http://{host}").parse().unwrap());
        assert!(
            tokio_tungstenite::connect_async(req).await.is_ok(),
            "{host} 可以（含 SSH 通道的 localhost）"
        );
        assert!(
            http_get_host(addr, "/healthz", &host)
                .await
                .starts_with("HTTP/1.1 200"),
            "{host}"
        );
    }
}
