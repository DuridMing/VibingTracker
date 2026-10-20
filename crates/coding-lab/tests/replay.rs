//! 回放歷史 session（#653）：從 JSONL 讀回（`/api/sessions/<id>/events` 一路往前翻）的序列，
//! 要和同一批 Event 即時進來時 session 頻道的 snapshot 完全一樣（同一 session 只看 pos，contracts.md §6）。

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{merged_events, sample_sessions};

use coding_lab::event::Event;
use coding_lab::store::Store;
use coding_lab::ws::{WebState, serve_web};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

static N: AtomicU32 = AtomicU32::new(0);

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "replay-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn get(addr: SocketAddr, path: &str) -> Value {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let body = out.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    serde_json::from_str(body).unwrap_or(Value::Null)
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn ws_recv(ws: &mut Ws) -> Value {
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

/// session 頻道的 snapshot（即時產生的串流）。
async fn live_stream(addr: SocketAddr, session: &str) -> Vec<Value> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    for m in [
        json!({"op": "hello", "protocol": 1, "utcOffsetMin": 0}),
        json!({"op": "subscribe", "channel": format!("session:{session}")}),
    ] {
        ws.send(Message::Text(m.to_string().into())).await.unwrap();
    }
    loop {
        let m = ws_recv(&mut ws).await;
        if m["t"] == "snapshot" {
            return m["stream"].as_array().cloned().unwrap_or_default();
        }
    }
}

/// 從 JSONL 回放：最新的一頁開始，用每頁最小的 pos 往前翻到底，舊的接在前面。
/// 樣本都不大，`limit` 取小一點才會跨很多頁，驗到頁與頁的接縫。
async fn replayed_stream(addr: SocketAddr, session: &str, limit: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut before: Option<u64> = None;
    loop {
        let q = before.map_or(String::new(), |b| format!("&beforePos={b}"));
        let page = get(
            addr,
            &format!("/api/sessions/{session}/events?limit={limit}{q}"),
        )
        .await;
        let events = page["events"].as_array().cloned().unwrap_or_default();
        let Some(min) = events.iter().filter_map(|e| e["pos"].as_u64()).min() else {
            return out;
        };
        out.splice(0..0, events);
        if min <= 1 {
            return out;
        }
        before = Some(min);
    }
}

fn keys(stream: &[Value]) -> Vec<(u64, String)> {
    stream
        .iter()
        .map(|e| {
            (
                e["seq"].as_u64().unwrap(),
                e["type"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn samples_replay_from_jsonl_matches_live_session_snapshot() {
    let mut replayed = 0;
    for dir in sample_sessions() {
        let Some(m) = merged_events(&dir) else {
            continue;
        };
        let (session, events) = (m.session, m.events);
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let home = tmp(&name);
        let mut store = Store::open(&home).unwrap();
        for e in &events {
            store.append(e.clone());
        }
        store.flush().unwrap();
        let state = WebState::new(Some(Arc::new(Mutex::new(store))), None, 8);
        state.apply(&events);
        let addr = serve_web("127.0.0.1:0".parse().unwrap(), state)
            .await
            .unwrap()
            .0;

        let live = live_stream(addr, &session).await;
        let replay = replayed_stream(addr, &session, 5).await;
        eprintln!("{name}：即時 {} 行、回放 {} 行", live.len(), replay.len());
        assert!(!live.is_empty(), "{name}: 即時的串流是空的");
        assert_eq!(keys(&replay), keys(&live), "{name}: 回放與即時的串流不同");
        replayed += 1;
    }
    if sample_sessions().is_empty() {
        eprintln!("略過：沒有 samples/sessions");
    } else {
        assert!(replayed > 0, "有 samples/ 卻沒有任何 session 可回放");
    }
}

/// 回放的串流裡，同一則 assistant 訊息的 thinking／text 一定排在它產生的工具卡片之前（PLAN P5 驗收）。
/// 工具卡片沒有 messageId，用它的 transcript 行查出屬於哪一則訊息。
#[tokio::test]
async fn samples_replay_keeps_thinking_and_text_before_their_tools() {
    let mut checked = 0;
    for dir in sample_sessions() {
        let Some(m) = merged_events(&dir) else {
            continue;
        };
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let home = tmp(&format!("order-{name}"));
        let mut store = Store::open(&home).unwrap();
        for e in &m.events {
            store.append(e.clone());
        }
        store.flush().unwrap();
        let state = WebState::new(Some(Arc::new(Mutex::new(store))), None, 8);
        let addr = serve_web("127.0.0.1:0".parse().unwrap(), state)
            .await
            .unwrap()
            .0;
        let replay: Vec<Event> = replayed_stream(addr, &m.session, 500)
            .await
            .iter()
            .map(|v| Event::from_json_line(&v.to_string()).unwrap())
            .collect();
        let message = |e: &Event| {
            let line = e.line.as_ref()?;
            m.message_of_line
                .get(&(line.file.clone(), line.no))
                .cloned()
        };
        // 每則訊息最後一段 thinking／text 在串流裡的索引
        let mut last_text: HashMap<String, usize> = HashMap::new();
        for (i, e) in replay.iter().enumerate() {
            match e.type_name() {
                "assistant.thinking" | "assistant.text" => {
                    if let Some(msg) = message(e) {
                        last_text.insert(msg, i);
                    }
                }
                "tool.placed" => {
                    let Some(msg) = message(e) else { continue };
                    // 同一則訊息的 thinking／text 若有，一定已經出現過
                    let later = replay[i..].iter().any(|x| {
                        matches!(x.type_name(), "assistant.thinking" | "assistant.text")
                            && message(x).as_deref() == Some(msg.as_str())
                    });
                    assert!(!later, "{name}: {msg} 的 thinking／text 排在工具之後");
                    if last_text.contains_key(&msg) {
                        checked += 1;
                    }
                }
                _ => {}
            }
        }
    }
    if !sample_sessions().is_empty() {
        assert!(
            checked > 0,
            "有 samples/ 卻沒有比對到任何「thinking／text → 工具」"
        );
    }
    eprintln!("比對了 {checked} 張有 thinking／text 的工具卡片");
}
