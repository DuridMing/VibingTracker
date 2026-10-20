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
    let instance = hello(&mut ws, json!({})).await["instance"].clone();
    assert!(instance.is_string(), "welcome 帶 instance");
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let last = recv(&mut ws).await["frame"].as_u64().unwrap();
    drop(ws);

    state.apply(&[text("s1", 1, 1)]);
    state.flush(2_000);
    state.apply(&[text("s1", 2, 2)]);
    state.flush(3_000);

    let mut ws = connect(addr, None).await.unwrap();
    hello(
        &mut ws,
        json!({"resume": {"instance": instance, "frame": last}}),
    )
    .await;
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
    hello(
        &mut ws,
        json!({"resume": {"instance": instance, "frame": 999_999}}),
    )
    .await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    assert_eq!(recv(&mut ws).await["t"], "snapshot");

    // 舊格式（resume 只是數字）分不出是不是同一個 collector，當成接不上
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({"resume": last})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    assert_eq!(recv(&mut ws).await["t"], "snapshot");
}

/// R5：collector 重啟後 frame 編號從頭算；舊頁面帶的 frame 編號剛好在新的範圍內，
/// 也不能當成接得上（會漏掉新 collector 的前段 frame），要送 snapshot。
#[tokio::test]
async fn resume_from_another_collector_instance_gets_snapshot() {
    let old = WebState::new(None, None, 64);
    let old_addr = start(old.clone()).await;
    let mut ws = connect(old_addr, None).await.unwrap();
    let old_instance = hello(&mut ws, json!({})).await["instance"].clone();
    drop(ws);

    let state = WebState::new(None, None, 64); // 重啟
    for i in 1..=80 {
        state.apply(&[text("s1", i, i)]);
        state.flush(2_000 + i);
    }
    let addr = start(state.clone()).await;
    let mut ws = connect(addr, None).await.unwrap();
    let w = hello(
        &mut ws,
        json!({"resume": {"instance": old_instance, "frame": 30}}),
    )
    .await;
    assert_ne!(w["instance"], old_instance, "重啟後 instance 不同");
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let m = recv(&mut ws).await;
    assert_eq!(m["t"], "snapshot", "舊 collector 的 frame 30 被當成接得上");
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

/// #716：`sessions` 是 live 裡所有 session（含 2 小時內結束的），另給 `open`＝還沒結束的，免得看數字以為還有 session 開著。
#[tokio::test]
async fn healthz_counts_open_sessions_separately() {
    let state = WebState::new(None, None, 8);
    state.apply(&[
        text("s1", 1, 1),
        text("s2", 1, 1),
        ev("s2", 2, "session.ended", None, json!({"reason": "exit"})),
    ]);
    state.flush(2_000);
    let addr = start(state).await;
    let health = http_get(addr, "/healthz").await;
    let body: Value = serde_json::from_str(health.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
    assert_eq!(body["sessions"], 2, "{body}");
    assert_eq!(body["open"], 1, "{body}");
}

/// 綁定位址能不能用由 config 把關（tests/config.rs）；serve_web 本身不再拒絕非 loopback（#667）。
#[tokio::test]
async fn non_loopback_bind_is_allowed_by_serve_web() {
    let state = WebState::new(None, None, 8);
    assert!(
        serve_web("0.0.0.0:0".parse().unwrap(), state).await.is_ok(),
        "config 已經檢查過，serve_web 直接綁"
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

/// R7：平行工具 A、B 的結果在 B 進串流之後才到；「載入更多」窗口最後一張卡片 A
/// 也要帶上它的 tool.updated 與 tool.result，不能停在「…」。
#[tokio::test]
async fn more_keeps_late_companions_of_the_last_card() {
    let home = tmp("more-late");
    let store = Arc::new(std::sync::Mutex::new(Store::open(&home).unwrap()));
    let called = |seq, id: &str| {
        ev(
            "s1",
            seq,
            "tool.called",
            None,
            json!({"toolUseId": id, "tool": "Bash", "input": {"command": "ls"},
            "status": "running", "placement": "staged", "danger": null}),
        )
    };
    let placed = |seq, id: &str, pos| {
        ev(
            "s1",
            seq,
            "tool.placed",
            Some(pos),
            json!({"toolUseId": id, "placement": "stream"}),
        )
    };
    let done = |seq, id: &str| {
        ev(
            "s1",
            seq,
            "tool.updated",
            None,
            json!({"toolUseId": id, "status": "done", "durationMs": 1, "error": null, "interrupted": false}),
        )
    };
    let result = |seq, id: &str| {
        ev(
            "s1",
            seq,
            "tool.result",
            None,
            json!({"toolUseId": id, "ok": true, "exitCode": 0, "output": "a\n", "truncated": false}),
        )
    };
    {
        let mut s = store.lock().unwrap();
        for e in [
            called(1, "A"),
            called(2, "B"),
            placed(3, "A", 1),
            placed(4, "B", 2),
            done(5, "A"),
            result(6, "A"),
            done(7, "B"),
            result(8, "B"),
        ] {
            s.append(e);
        }
        s.flush().unwrap();
    }
    let state = WebState::new(Some(store), None, 8);
    let addr = start(state).await;
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({})).await;
    send(
        &mut ws,
        json!({"op": "more", "session": "s1", "beforePos": 2, "limit": 10}),
    )
    .await;
    let m = recv(&mut ws).await;
    let a: Vec<&str> = m["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["payload"]["toolUseId"] == "A")
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        a,
        ["tool.called", "tool.placed", "tool.updated", "tool.result"]
    );
    assert!(
        m["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["payload"]["toolUseId"] != "B"),
        "窗口外的卡片 B 不帶"
    );
}

/// #670-5：hello 的 resume 只給重連時一起送出的那批 subscribe；之後才新開的頻道
/// （例如重連後幾秒才打開的 session tab）客戶端沒有它的狀態，要給 snapshot。
#[tokio::test]
async fn resume_only_covers_the_resubscribe_burst() {
    let state = WebState::new(None, None, 64);
    let addr = start(state.clone()).await;
    let mut ws = connect(addr, None).await.unwrap();
    let instance = hello(&mut ws, json!({})).await["instance"].clone();
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let last = recv(&mut ws).await["frame"].as_u64().unwrap();
    drop(ws);
    state.apply(&[text("s1", 1, 1)]);
    state.flush(2_000);

    state.set_resume_window(Duration::ZERO); // 模擬「過了重連那一刻」
    let mut ws = connect(addr, None).await.unwrap();
    hello(
        &mut ws,
        json!({"resume": {"instance": instance, "frame": last}}),
    )
    .await;
    send(&mut ws, json!({"op": "subscribe", "channel": "session:s1"})).await;
    assert_eq!(recv(&mut ws).await["t"], "snapshot");
}

/// #654 驗收（本機那一半）：本機連線收到的敏感輸出是全文；welcome 說明這是本機連線。
/// 遠端那一半（剝除）由下面的 redact_remote_copy_* 與 remote_sessions_rows_are_redacted_over_http_and_ws 涵蓋。
#[tokio::test]
async fn local_connection_sees_sensitive_output_in_full() {
    let state = WebState::new(None, None, 64);
    state.apply(&[ev(
        "s1",
        1,
        "tool.called",
        Some(1),
        json!({"toolUseId": "t1", "tool": "Read", "input": {"file_path": "/p/.env"},
               "status": "done", "placement": "stream", "danger": null}),
    )]);
    state.apply(&[ev(
        "s1",
        2,
        "tool.result",
        None,
        json!({"toolUseId": "t1", "ok": true, "exitCode": null, "output": "TOKEN=abc",
               "truncated": false, "sensitive": true}),
    )]);
    state.flush(2_000);
    let addr = start(state.clone()).await;
    let mut ws = connect(addr, None).await.unwrap();
    let w = hello(&mut ws, json!({})).await;
    assert_eq!(w["remote"], false);
    send(&mut ws, json!({"op": "subscribe", "channel": "session:s1"})).await;
    let snap = recv(&mut ws).await;
    let text = snap.to_string();
    assert!(text.contains("TOKEN=abc"), "本機看得到全文：{text}");
    assert!(!text.contains("\"redacted\""));
}

#[test]
fn only_non_loopback_peers_are_remote() {
    use coding_lab::ws::is_remote;
    assert!(!is_remote("127.0.0.1:5000".parse().unwrap()));
    assert!(!is_remote("[::1]:5000".parse().unwrap()));
    assert!(is_remote("100.64.0.7:5000".parse().unwrap()));
    assert!(is_remote("192.168.1.20:5000".parse().unwrap()));
}

// ── 遠端剝除：單一的 redact_remote_copy（#734 C8、C2、S-5，E-L10） ───────────────────

const CLK: &str = "clk_ABCDEFGH_0123456789abcdefghijABCDEFGHIJ0123456789abcde";

/// 一個 frame：敏感與不敏感的 tool.called／file.write／file.edit／tool.result、一段 thinking、兩種 doing 來源的 sessions 列。
fn redaction_frame() -> Value {
    let event = |kind: &str, payload: Value| json!({"type": kind, "session": "s1", "seq": 1, "payload": payload});
    json!({
        "t": "frame",
        "events": [
            event("tool.called", json!({"toolUseId": "w1", "tool": "Write", "sensitive": true,
                "input": {"file_path": "/p/.env", "content": "API_KEY=NEWSECRET"}})),
            event("tool.called", json!({"toolUseId": "e1", "tool": "Edit", "sensitive": true,
                "input": {"file_path": "/p/.env", "old_string": "OLDSECRET", "new_string": "NEWSECRET", "replace_all": false}})),
            event("tool.called", json!({"toolUseId": "b1", "tool": "Bash", "sensitive": true,
                "input": {"command": "cat .env", "description": "看 NEWSECRET"}})),
            event("file.write", json!({"toolUseId": "w1", "path": "/p/.env", "content": "API_KEY=NEWSECRET", "sensitive": true})),
            event("file.edit", json!({"toolUseId": "e1", "path": "/p/.env", "sensitive": true,
                "hunk": {"oldStart": 1, "oldLines": 3, "newStart": 1, "newLines": 3, "lines": ["-DB=OLDSECRET", "+DB=NEWSECRET", " FOO=1"]}})),
            event("tool.result", json!({"toolUseId": "b1", "ok": true, "output": "API_KEY=NEWSECRET", "sensitive": true})),
            event("tool.called", json!({"toolUseId": "w2", "tool": "Write", "input": {"file_path": "/p/a.txt", "content": "plain"}})),
            event("file.write", json!({"toolUseId": "w2", "path": "/p/a.txt", "content": "plain"})),
            event("file.edit", json!({"toolUseId": "e2", "path": "/p/a.txt", "hunk": {"lines": ["+plain"]}})),
            event("assistant.thinking", json!({"text": "想一想", "summarized": false})),
            event("prompt.submitted", json!({"text": format!("用 {CLK}")})),
        ],
        "sessions": [
            {"id": "s1", "doing": "想一想", "doingSource": "thinking"},
            {"id": "s2", "doing": "改 a.txt", "doingSource": "text"},
        ],
    })
}

#[test]
fn redact_remote_copy_strips_sensitive_writes_edits_and_results() {
    use coding_lab::config::RemoteThinking;
    let frame = redaction_frame();
    let out = coding_lab::ws::redact_remote_copy(&frame, RemoteThinking::OnDemand);
    let text = out.to_string();
    assert!(
        !text.contains("NEWSECRET") && !text.contains("OLDSECRET"),
        "{text}"
    );
    let evs = &out["events"];
    // tool.called 只留路徑類欄位
    assert_eq!(evs[0]["payload"]["input"], json!({"file_path": "/p/.env"}));
    assert_eq!(evs[1]["payload"]["input"], json!({"file_path": "/p/.env"}));
    assert_eq!(evs[2]["payload"]["input"], json!({"command": "cat .env"}));
    for i in 0..6 {
        assert_eq!(evs[i]["payload"]["redacted"], true, "{}", evs[i]);
    }
    assert_eq!(evs[3]["payload"]["content"], "");
    assert_eq!(evs[4]["payload"]["hunk"]["lines"], json!([]));
    assert_eq!(evs[4]["payload"]["hunk"]["oldLines"], 3, "行號與計數保留");
    assert_eq!(evs[5]["payload"]["output"], "");
    // 不敏感的照送
    for i in 6..9 {
        assert_eq!(evs[i], frame["events"][i], "不敏感的不動");
    }
    // 原值不變（只改送出的副本）
    assert_eq!(frame, redaction_frame());
}

#[test]
fn redact_remote_copy_defers_thinking_and_thinking_doing_unless_show() {
    use coding_lab::config::RemoteThinking;
    let frame = redaction_frame();
    for mode in [RemoteThinking::OnDemand, RemoteThinking::Hide] {
        let out = coding_lab::ws::redact_remote_copy(&frame, mode);
        let t = &out["events"][9]["payload"];
        assert_eq!(t["text"], "");
        assert_eq!(t["deferred"], true);
        assert_eq!(t["chars"], 3);
        assert_eq!(
            out["sessions"][0]["doing"],
            Value::Null,
            "{mode:?}：thinking 來的 doing 不送"
        );
        assert_eq!(out["sessions"][1]["doing"], "改 a.txt");
    }
    let out = coding_lab::ws::redact_remote_copy(&frame, RemoteThinking::Show);
    assert_eq!(out["events"][9]["payload"]["text"], "想一想");
    assert_eq!(out["sessions"][0]["doing"], "想一想");
    // 不論模式，clk_ 金鑰都剝
    assert!(!out.to_string().contains(CLK));
    assert!(
        out["events"][10]["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("clk_ABCDEFGH_…")
    );
}

/// 遠端 /api/sessions 與 WS snapshot：prompt 裡的 clk_ 金鑰剝掉（S-5），hide 時 thinking 來的 doing 不送（C2）。
#[tokio::test]
async fn remote_sessions_rows_are_redacted_over_http_and_ws() {
    use coding_lab::config::RemoteThinking;
    let evs = vec![
        ev(
            "s1",
            1,
            "session.started",
            None,
            json!({"cwd": "/p", "source": "startup", "model": "m", "transcriptPath": "/t", "branch": "main"}),
        ),
        ev(
            "s1",
            2,
            "prompt.submitted",
            None,
            json!({"text": format!("用 {CLK} 登入"), "promptId": "p1"}),
        ),
        ev(
            "s1",
            3,
            "assistant.thinking",
            Some(1),
            json!({"text": "THOUGHT-密碼在第三行。", "summarized": false, "messageId": "m1", "model": "m"}),
        ),
    ];
    let home = tmp("remote-rows");
    let mut store = Store::open(&home).unwrap();
    for e in &evs {
        store.append(e.clone());
    }
    store.flush().unwrap();
    let state = WebState::new(Some(Arc::new(std::sync::Mutex::new(store))), None, 8);
    state.set_remote_thinking(RemoteThinking::Hide);
    state.set_force_remote(true);
    state.apply(&evs);
    state.flush(5_000);
    let addr = start(state).await;
    let body = http_get(addr, "/api/sessions").await;
    assert!(!body.contains(CLK), "遠端 /api/sessions 帶完整金鑰：{body}");
    assert!(
        !body.contains("THOUGHT"),
        "遠端 /api/sessions 的 doing 是 thinking：{body}"
    );
    let mut ws = connect(addr, None).await.unwrap();
    hello(&mut ws, json!({})).await;
    send(&mut ws, json!({"op": "subscribe", "channel": "*"})).await;
    let snap = recv(&mut ws).await.to_string();
    assert!(!snap.contains(CLK) && !snap.contains("THOUGHT"), "{snap}");
}

/// 每個回應都禁止被嵌進 iframe（F-7）：local-open 時任何網站都能把 127.0.0.1:4801 放進 iframe 誘導點擊「全部確認」。
#[tokio::test]
async fn every_response_forbids_framing() {
    let home = tmp("frame");
    std::fs::write(home.join("index.html"), "<html></html>").unwrap();
    let state = WebState::new(None, Some(home.clone()), 8);
    let addr = start(state).await;
    for path in ["/", "/healthz", "/api/sessions", "/auth/login", "/nope"] {
        let raw = http_get_host(addr, path, &addr.to_string()).await;
        let head = raw.split("\r\n\r\n").next().unwrap().to_ascii_lowercase();
        assert!(head.contains("x-frame-options: deny"), "{path}：{head}");
        assert!(
            head.contains("content-security-policy: frame-ancestors 'none'"),
            "{path}：{head}"
        );
    }
}

/// C8 端到端：merge 對敏感路徑的 Write／Edit 標 sensitive（#761），遠端副本剝掉新寫入的秘密，
/// 也剝掉 Edit 的 hunk 前後文從實際檔案帶出的舊秘密（#734）。本機副本照舊。
#[test]
fn sensitive_writes_and_edits_from_merge_never_reach_remote_copies() {
    use coding_lab::config::RemoteThinking;
    use coding_lab::hook::HookInput;
    use coding_lab::merge::{Input, Merger};
    let dir = tmp("c8");
    std::fs::write(
        dir.join(".env"),
        "DB_PASSWORD=OLDSECRET-zzz\nFOO=1\nBAR=2\n",
    )
    .unwrap();
    let cwd = dir.to_string_lossy().replace('\\', "/");
    let hook = |v: Value| {
        let mut h = HookInput::from_json(v.to_string().as_bytes()).unwrap();
        coding_lab::derive::attach_probe(&mut h);
        h
    };
    let mut m = Merger::new(Box::new(coding_lab::rules::RuleSet::builtin()));
    let mut evs = Vec::new();
    for (i, v) in [
        json!({"session_id": "s1", "hook_event_name": "SessionStart", "cwd": cwd, "source": "startup",
               "transcript_path": format!("{cwd}/t.jsonl")}),
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Write", "cwd": cwd,
               "tool_input": {"file_path": format!("{cwd}/new/.env"), "content": "API_KEY=NEWSECRET-abc\n"},
               "tool_use_id": "w1"}),
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": cwd,
               "tool_input": {"file_path": format!("{cwd}/.env"), "old_string": "FOO=1", "new_string": "FOO=2"},
               "tool_use_id": "e1"}),
    ]
    .into_iter()
    .enumerate()
    {
        evs.extend(m.handle(Input::Hook { hook: hook(v), ts_seen: i as u64 + 1 }));
    }
    let kinds: Vec<&str> = evs.iter().map(|e| e.type_name()).collect();
    assert!(
        kinds.contains(&"file.write") && kinds.contains(&"file.edit"),
        "{kinds:?}"
    );
    let local = json!({ "events": evs }).to_string();
    assert!(
        local.contains("NEWSECRET") && local.contains("OLDSECRET"),
        "前提：本機副本有全文"
    );
    let remote =
        coding_lab::ws::redact_remote_copy(&json!({ "events": evs }), RemoteThinking::Show)
            .to_string();
    for secret in ["NEWSECRET", "OLDSECRET"] {
        assert!(!remote.contains(secret), "遠端副本含 {secret}：{remote}");
    }
}
