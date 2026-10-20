//! collector 重啟後的 sessions 列（#676）：live 第一次看到 session 時從 store 種回 cwd、firstPrompt、
//! startedAt 與累計值；啟動時的重放不能把 store 已經算過的累計值再加一次。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use coding_lab::event::Event;
use coding_lab::store::Store;
use coding_lab::ws::{WebState, serve_web};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

static N: AtomicU32 = AtomicU32::new(0);

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "restart-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ev(seq: u64, kind: &str, ts: u64, payload: Value) -> Event {
    Event::from_json_line(
        &json!({"v": 1, "session": "s1", "seq": seq, "type": kind, "agent": "main", "line": null, "pos": null,
                "ts_model": null, "ts_exec": null, "ts_seen": ts, "host": "local", "payload": payload})
        .to_string(),
    )
    .unwrap()
}

fn prompt(seq: u64, ts: u64, t: &str) -> Event {
    ev(
        seq,
        "prompt.submitted",
        ts,
        json!({"text": t, "promptId": format!("p{seq}")}),
    )
}

fn usage(seq: u64, ts: u64, cost: f64, thinking: u64) -> Event {
    ev(
        seq,
        "usage",
        ts,
        json!({"messageId": format!("m{seq}"), "model": "claude-opus-5-5", "input": 0, "output": 0,
               "thinking": thinking, "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": cost}),
    )
}

/// 重啟前的一輪：started（cwd、branch、沒有 model）、第一個 prompt、usage、一次 commit。
fn before_restart() -> Vec<Event> {
    vec![
        ev(
            1,
            "session.started",
            1_000,
            json!({"cwd": "/p", "source": "startup", "model": null, "transcriptPath": "/t", "branch": "dev"}),
        ),
        prompt(2, 1_100, "第一"),
        usage(3, 1_200, 1.5, 10),
        ev(
            4,
            "git.op",
            1_300,
            json!({"op": "commit", "ok": true, "toolUseId": "g", "command": "git commit -m x"}),
        ),
    ]
}

async fn row(state: Arc<WebState>) -> Value {
    let addr: SocketAddr = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap()
        .0;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET /api/sessions HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let body: Value =
        serde_json::from_str(out.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap();
    body["sessions"][0].clone()
}

fn assert_seeded(r: &Value) {
    assert_eq!(r["cwd"], "/p", "{r}");
    assert_eq!(r["firstPrompt"], "第一", "{r}");
    assert_eq!(r["startedAt"], 1_000, "{r}");
    assert_eq!(r["commits"], 1, "{r}");
    assert_eq!(r["model"], "opus 5.5", "{r}");
    assert_eq!(r["branch"], "dev", "{r}");
}

/// 執行中：store 已經有重啟前的資料，live 只看到重啟後的新 Event（還在 store 的緩衝裡、沒 flush）。
#[tokio::test]
async fn live_seeds_a_session_from_store_when_it_first_sees_it() {
    let home = tmp("seed");
    let mut store = Store::open(&home).unwrap();
    for e in before_restart() {
        store.append(e);
    }
    store.flush().unwrap();
    let after = [prompt(5, 5_000, "重啟之後"), usage(6, 5_100, 0.5, 5)];
    for e in &after {
        store.append(e.clone()); // 和 serve 的 sink 一樣：先進 store 緩衝，再給 live
    }
    let state = WebState::new(Some(Arc::new(Mutex::new(store))), None, 8);
    state.apply(&after);
    state.flush(6_000);

    let r = row(state).await;
    assert_seeded(&r);
    assert_eq!(r["lastPrompt"], "重啟之後", "{r}");
    assert_eq!(r["costUsd"], 2.0, "重啟前 1.5 加上重啟後 0.5：{r}");
    assert_eq!(r["thinkingTokens"], 15, "{r}");
}

/// 啟動：重放 store 最近的 Event 重建 live，累計值和 store 一致、不重複計算；
/// 重放區間沒涵蓋到的 session.started 與第一個 prompt 也從 store 補回。
#[tokio::test]
async fn rebuild_matches_store_without_counting_twice() {
    for (name, skip) in [("whole", 0), ("window", 2)] {
        let home = tmp(name);
        let mut store = Store::open(&home).unwrap();
        for e in before_restart() {
            store.append(e);
        }
        store.flush().unwrap();
        let recent: Vec<Event> = before_restart().into_iter().skip(skip).collect();
        let state = WebState::new(Some(Arc::new(Mutex::new(store))), None, 8);
        state.rebuild(&recent);
        state.flush(6_000);

        let r = row(state).await;
        assert_seeded(&r);
        assert_eq!(r["costUsd"], 1.5, "{name}：{r}");
        assert_eq!(r["thinkingTokens"], 10, "{name}：{r}");
    }
}
