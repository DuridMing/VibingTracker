//! 遠端的 thinking（#668，contracts.md §4、§6、§7）：依連線來源與 `[web].remote_thinking` 的剝除矩陣，
//! `GET /api/sessions/<id>/thinking/<seq>` 的權限，JSONL／store 不受影響。
//! 測試用 `set_force_remote(true)` 把 loopback 連線當成遠端（真的非 loopback 來源在單機測不到）。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use coding_lab::config::RemoteThinking;
use coding_lab::event::Event;
use coding_lab::store::Store;
use coding_lab::ws::{WebState, serve_web};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

static N: AtomicU32 = AtomicU32::new(0);
const THOUGHT: &str = "先看 README，再決定要不要改 config。\n第二行。";

fn tmp(name: &str) -> std::path::PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "thinking-{name}-{}-{}",
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

fn events() -> Vec<Event> {
    vec![
        ev(
            "s1",
            1,
            "assistant.thinking",
            Some(1),
            json!({"text": THOUGHT, "summarized": false, "messageId": "m1", "model": "claude-opus-5-5"}),
        ),
        ev(
            "s1",
            2,
            "assistant.text",
            Some(2),
            json!({"text": "好，我來看。", "messageId": "m1", "model": "claude-opus-5-5"}),
        ),
    ]
}

struct Srv {
    addr: SocketAddr,
    store: Arc<Mutex<Store>>,
}

async fn start(mode: RemoteThinking, force_remote: bool) -> Srv {
    let home = tmp("srv");
    let mut store = Store::open(&home).unwrap();
    for e in events() {
        store.append(e);
    }
    store.flush().unwrap();
    let store = Arc::new(Mutex::new(store));
    let state = WebState::new(Some(store.clone()), None, 8);
    state.set_remote_thinking(mode);
    state.set_force_remote(force_remote);
    state.apply(&events());
    state.flush(5_000);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    Srv { addr, store }
}

async fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let code: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}

async fn ws(addr: SocketAddr) -> Ws {
    tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap()
        .0
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    match tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str(&t).unwrap(),
        other => panic!("沒等到訊息：{other:?}"),
    }
}

/// 連上、hello、訂 `*`：回 (welcome, snapshot)。
async fn open(addr: SocketAddr) -> (Ws, Value, Value) {
    let mut w = ws(addr).await;
    send(&mut w, json!({"op": "hello", "protocol": 1})).await;
    let welcome = recv(&mut w).await;
    assert_eq!(welcome["t"], "welcome");
    send(&mut w, json!({"op": "subscribe", "channel": "*"})).await;
    let snap = recv(&mut w).await;
    assert_eq!(snap["t"], "snapshot", "{snap}");
    (w, welcome, snap)
}

fn thinking_rows(v: &Value) -> Vec<&Value> {
    fn walk<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
        match v {
            Value::Object(m) => {
                if m.get("type").and_then(Value::as_str) == Some("assistant.thinking") {
                    out.push(v);
                    return;
                }
                m.values().for_each(|c| walk(c, out));
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

fn assert_deferred(rows: &[&Value], what: &str) {
    assert!(!rows.is_empty(), "{what}：要有 thinking 列");
    for r in rows {
        assert_eq!(r["payload"]["text"], "", "{what}：text 要是空的：{r}");
        assert_eq!(r["payload"]["deferred"], true, "{what}：{r}");
        assert_eq!(
            r["payload"]["chars"].as_u64(),
            Some(THOUGHT.chars().count() as u64),
            "{what}：佔位要帶字數：{r}"
        );
    }
}

fn assert_full(rows: &[&Value], what: &str) {
    assert!(!rows.is_empty(), "{what}：要有 thinking 列");
    for r in rows {
        assert_eq!(r["payload"]["text"], THOUGHT, "{what}：{r}");
        assert!(r["payload"].get("deferred").is_none(), "{what}：{r}");
    }
}

// ── 剝除矩陣：來源 × 設定 ─────────────────────────────────────────────────

#[tokio::test]
async fn remote_on_demand_sends_placeholders_everywhere_and_api_returns_full_text() {
    let s = start(RemoteThinking::OnDemand, true).await;
    let (mut w, welcome, snap) = open(s.addr).await;
    assert_eq!(welcome["remote"], true);
    assert_eq!(welcome["thinking"], "on_demand");
    assert_deferred(&thinking_rows(&snap), "snapshot");

    // 載入更多
    send(
        &mut w,
        json!({"op": "more", "session": "s1", "beforePos": 3, "limit": 10}),
    )
    .await;
    let more = recv(&mut w).await;
    assert_eq!(more["t"], "more");
    assert_deferred(&thinking_rows(&more), "more");

    // 即時 frame
    let state_ev = ev(
        "s1",
        3,
        "assistant.thinking",
        Some(3),
        json!({"text": THOUGHT, "summarized": true, "messageId": "m2", "model": "claude-opus-5-5"}),
    );
    // 直接餵到同一個 WebState：透過第二個連線拿不到 state，所以用 API 旁路——改用 more 的結果已涵蓋；frame 由下面的
    // 連線重訂驗證
    drop(state_ev);

    // 點開才載入：API 回全文
    let (code, body) = get(s.addr, "/api/sessions/s1/thinking/1").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["text"], THOUGHT);
    assert_eq!(body["summarized"], false);
    assert_eq!(body["session"], "s1");
    assert_eq!(body["seq"], 1);
    assert_eq!(body["messageId"], "m1");

    // 不是 thinking 的 seq → 404
    let (code, _) = get(s.addr, "/api/sessions/s1/thinking/2").await;
    assert_eq!(code, 404);
    let (code, _) = get(s.addr, "/api/sessions/s1/thinking/99").await;
    assert_eq!(code, 404);
}

#[tokio::test]
async fn remote_hide_sends_placeholders_and_api_is_forbidden() {
    let s = start(RemoteThinking::Hide, true).await;
    let (_w, welcome, snap) = open(s.addr).await;
    assert_eq!(welcome["thinking"], "hide");
    assert_deferred(&thinking_rows(&snap), "snapshot");
    let (code, body) = get(s.addr, "/api/sessions/s1/thinking/1").await;
    assert_eq!(code, 403, "{body}");
    assert!(body["text"].is_null());
}

#[tokio::test]
async fn remote_show_sends_full_text() {
    let s = start(RemoteThinking::Show, true).await;
    let (_w, welcome, snap) = open(s.addr).await;
    assert_eq!(welcome["thinking"], "show");
    assert_full(&thinking_rows(&snap), "snapshot");
    let (code, body) = get(s.addr, "/api/sessions/s1/thinking/1").await;
    assert_eq!(code, 200);
    assert_eq!(body["text"], THOUGHT);
}

#[tokio::test]
async fn local_connections_always_get_full_text_whatever_the_setting() {
    for mode in [
        RemoteThinking::OnDemand,
        RemoteThinking::Hide,
        RemoteThinking::Show,
    ] {
        let s = start(mode, false).await;
        let (_w, welcome, snap) = open(s.addr).await;
        assert_eq!(welcome["remote"], false);
        assert_eq!(welcome["thinking"], "full", "本機永遠完整：{mode:?}");
        assert_full(&thinking_rows(&snap), &format!("本機 {mode:?}"));
        let (code, body) = get(s.addr, "/api/sessions/s1/thinking/1").await;
        assert_eq!(code, 200, "本機 {mode:?}：{body}");
        assert_eq!(body["text"], THOUGHT);
    }
}

#[tokio::test]
async fn live_frames_to_remote_are_deferred_too() {
    let home = tmp("frame");
    let store = Arc::new(Mutex::new(Store::open(&home).unwrap()));
    let state = WebState::new(Some(store), None, 8);
    state.set_remote_thinking(RemoteThinking::OnDemand);
    state.set_force_remote(true);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state.clone())
        .await
        .unwrap();
    let (mut w, _, _) = open(addr).await;
    state.apply(&events());
    state.flush(9_000);
    let frame = recv(&mut w).await;
    assert_eq!(frame["t"], "frame", "{frame}");
    assert_deferred(&thinking_rows(&frame), "frame");
}

#[tokio::test]
async fn store_and_jsonl_keep_the_full_text() {
    let s = start(RemoteThinking::Hide, true).await;
    let (_w, _, snap) = open(s.addr).await;
    assert_deferred(&thinking_rows(&snap), "snapshot");
    let rows = s.store.lock().unwrap().events_by_seq("s1", 1, 1).unwrap();
    assert_eq!(rows.len(), 1);
    let j = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(j["payload"]["text"], THOUGHT, "剝除只改送出的副本");
}

/// 敏感輸出的剝除（#654）與 thinking 的剝除同一條路，兩個都要發生。
#[tokio::test]
async fn sensitive_redaction_still_applies_alongside_thinking() {
    let home = tmp("both");
    let mut store = Store::open(&home).unwrap();
    let evs = vec![
        ev(
            "s1",
            1,
            "assistant.thinking",
            Some(1),
            json!({"text": THOUGHT, "summarized": false, "messageId": "m1", "model": "claude-opus-5-5"}),
        ),
        ev(
            "s1",
            2,
            "tool.called",
            Some(2),
            json!({"toolUseId": "t1", "tool": "Read", "input": {"file_path": "/p/.env"}, "sensitive": true, "danger": null}),
        ),
        ev(
            "s1",
            3,
            "tool.result",
            None,
            json!({"toolUseId": "t1", "output": "SECRET=1", "truncated": false, "sensitive": true}),
        ),
    ];
    for e in &evs {
        store.append(e.clone());
    }
    store.flush().unwrap();
    let state = WebState::new(Some(Arc::new(Mutex::new(store))), None, 8);
    state.set_remote_thinking(RemoteThinking::OnDemand);
    state.set_force_remote(true);
    state.apply(&evs);
    state.flush(5_000);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    let (_w, _, snap) = open(addr).await;
    assert_deferred(&thinking_rows(&snap), "snapshot");
    let text = snap.to_string();
    assert!(!text.contains("SECRET=1"), "敏感輸出要剝除：{text}");
    assert!(text.contains("\"redacted\":true"), "{text}");
}

/// REST 的回放／載入更多（`/api/sessions/<id>/events`、`/api/projects/events`）對遠端也要剝除 thinking，
/// 不然遠端用 API 就能繞過 WS 的佔位（#715 實機驗收找到）。
#[tokio::test]
async fn rest_events_to_remote_are_deferred_like_ws() {
    for (mode, deferred) in [
        (RemoteThinking::OnDemand, true),
        (RemoteThinking::Hide, true),
        (RemoteThinking::Show, false),
    ] {
        let s = start(mode, true).await;
        let (code, body) = get(s.addr, "/api/sessions/s1/events?limit=10").await;
        assert_eq!(code, 200, "{mode:?}：{body}");
        let rows = thinking_rows(&body);
        if deferred {
            assert_deferred(&rows, &format!("REST events {mode:?}"));
        } else {
            assert_full(&rows, &format!("REST events {mode:?}"));
        }
    }
    // 本機不剝除
    let s = start(RemoteThinking::Hide, false).await;
    let (_, body) = get(s.addr, "/api/sessions/s1/events?limit=10").await;
    assert_full(&thinking_rows(&body), "REST events 本機");
}

/// #720：遠端的 WS、REST events、thinking API 都不能露出 clk_ 金鑰的 secret；本機照常。
#[tokio::test]
async fn remote_copies_mask_clk_keys_but_local_keeps_them() {
    let key = "clk_HWYrRkbl_ff4bBW3FbFRndyOYvtsLRB8ryusCZcbbE2dGf1co9j8";
    let secret = "ff4bBW3FbFRndyOYvtsLRB8ryusCZcbbE2dGf1co9j8";
    let evs = vec![
        ev(
            "s1",
            1,
            "assistant.thinking",
            Some(1),
            json!({"text": format!("要用 {key} 登入"), "summarized": false, "messageId": "m1", "model": "claude-opus-5-5"}),
        ),
        ev(
            "s1",
            2,
            "assistant.text",
            Some(2),
            json!({"text": format!("金鑰：{key}"), "messageId": "m1", "model": "claude-opus-5-5"}),
        ),
        ev(
            "s1",
            3,
            "tool.called",
            Some(3),
            json!({"toolUseId": "t1", "tool": "Bash", "input": {"command": format!("curl -H 'Authorization: Bearer {key}' x")}, "sensitive": false, "danger": null}),
        ),
    ];
    for remote in [true, false] {
        let home = tmp("keys");
        let mut store = Store::open(&home).unwrap();
        for e in &evs {
            store.append(e.clone());
        }
        store.flush().unwrap();
        let state = WebState::new(Some(Arc::new(Mutex::new(store))), None, 8);
        state.set_remote_thinking(RemoteThinking::Show);
        state.set_force_remote(remote);
        state.apply(&evs);
        state.flush(5_000);
        let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
            .await
            .unwrap();
        let (_w, _, snap) = open(addr).await;
        let (_, rest) = get(addr, "/api/sessions/s1/events?limit=10").await;
        let (_, think) = get(addr, "/api/sessions/s1/thinking/1").await;
        for (what, body) in [
            ("snapshot", &snap),
            ("REST events", &rest),
            ("thinking API", &think),
        ] {
            let text = body.to_string();
            if remote {
                assert!(!text.contains(secret), "遠端 {what} 不能有 secret：{text}");
                assert!(
                    text.contains("clk_HWYrRkbl_…"),
                    "遠端 {what} 要留 id：{text}"
                );
            } else {
                assert!(text.contains(key), "本機 {what} 完整：{text}");
            }
        }
    }
}
