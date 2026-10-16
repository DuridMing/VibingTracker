//! web 入口（#636，contracts.md §6–§7）：WebSocket、`/healthz`、前端靜態檔。
//!
//! 每 50 ms 從 [`Live`] 取一個 frame，用 broadcast 發給各連線，各連線依自己訂的頻道與時區過濾。
//! 連線落後超過 broadcast 容量時直接重送 snapshot（snapshot 就是最新狀態，什麼都不會漏）。
//!
//! 安全：P6 的金鑰驗證完成前只綁 loopback；連 localhost 也檢查 `Origin`，否則使用者瀏覽器裡任何網頁
//! 都能連 `ws://127.0.0.1:4801` 讀走 transcript。沒帶 `Origin` 的程式（wscat、curl）放行。

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::event::Event;
use crate::live::{Channel, Frame, Live};
use crate::store::Store;

pub const PROTOCOL: u64 = 1;
pub const FLUSH_EVERY: Duration = Duration::from_millis(50);
const MORE_LIMIT: u64 = 500;
const COMPANION_LOOKBACK: u64 = 1000;

pub struct WebState {
    live: Mutex<Live>,
    store: Option<Arc<Mutex<Store>>>,
    web_root: Option<PathBuf>,
    frames: broadcast::Sender<Arc<Frame>>,
    /// collector 主迴圈回報的健康資訊（tail 數、排程延遲…），併進 /healthz
    health: Mutex<Value>,
}

impl WebState {
    /// `capacity`：每個連線最多落後幾個 frame，超過就改送 snapshot。
    pub fn new(
        store: Option<Arc<Mutex<Store>>>,
        web_root: Option<PathBuf>,
        capacity: usize,
    ) -> Arc<WebState> {
        let (frames, _) = broadcast::channel(capacity.max(1));
        Arc::new(WebState {
            live: Mutex::new(Live::new()),
            store,
            web_root,
            frames,
            health: Mutex::new(json!({})),
        })
    }

    pub fn set_health(&self, extra: Value) {
        *self.health.lock().unwrap_or_else(|e| e.into_inner()) = extra;
    }

    fn live(&self) -> MutexGuard<'_, Live> {
        // 有執行緒 panic 也繼續用（狀態只是顯示用的彙總）
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn apply(&self, events: &[Event]) {
        let mut live = self.live();
        for e in events {
            live.apply(e);
        }
    }

    /// 打包並廣播一個 frame；沒有變化就什麼都不送。
    pub fn flush(&self, now: u64) {
        let frame = self.live().flush(now);
        if let Some(f) = frame {
            // 沒有任何連線時 send 會失敗，不影響
            let _ = self.frames.send(f);
        }
    }

    /// 每 [`FLUSH_EVERY`] flush 一次。
    pub fn spawn_flusher(self: &Arc<Self>) -> JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(FLUSH_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                state.flush(crate::now_ms());
            }
        })
    }
}

/// 開始提供 web 入口；回傳實際綁定的位址（測試用 port 0）。
pub async fn serve_web(
    addr: SocketAddr,
    state: Arc<WebState>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "web 入口在金鑰驗證（P6）完成前只能綁 127.0.0.1，收到 {addr}；要從別台看請用 SSH 通道"
            ),
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/healthz", get(healthz))
        .fallback(get(static_file))
        .with_state(state);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("coding-lab web：{e}");
        }
    });
    Ok((local, task))
}

/// `Origin` 必須和 `Host` 相同（沒帶 `Origin` 的非瀏覽器程式放行）。
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or("");
    !host.is_empty() && authority.trim_end_matches('/') == host
}

async fn ws_handler(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "Origin 與 Host 不符").into_response();
    }
    ws.on_upgrade(move |socket| connection(socket, state))
}

struct Conn {
    /// 訂閱的頻道，以及這個頻道已經送到的 frame 編號
    subs: BTreeMap<Channel, u64>,
    offset: i32,
    resume: Option<u64>,
}

async fn send(socket: &mut WebSocket, v: &Value) -> bool {
    socket
        .send(Message::Text(v.to_string().into()))
        .await
        .is_ok()
}

async fn connection(mut socket: WebSocket, state: Arc<WebState>) {
    let mut rx = state.frames.subscribe();
    let mut conn = Conn {
        subs: BTreeMap::new(),
        offset: 0,
        resume: None,
    };
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if !on_message(&mut socket, &state, &mut conn, &t).await {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            frame = rx.recv() => match frame {
                Ok(f) => {
                    for (ch, since) in conn.subs.iter_mut() {
                        if f.no <= *since {
                            continue;
                        }
                        *since = f.no;
                        if let Some(j) = f.to_json(ch, conn.offset)
                            && !send(&mut socket, &j).await
                        {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let channels: Vec<Channel> = conn.subs.keys().cloned().collect();
                    for ch in channels {
                        if !send_snapshot(&mut socket, &state, &mut conn, &ch).await {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

async fn send_snapshot(
    socket: &mut WebSocket,
    state: &WebState,
    conn: &mut Conn,
    ch: &Channel,
) -> bool {
    let snap = state.live().snapshot(ch, conn.offset, crate::now_ms());
    conn.subs
        .insert(ch.clone(), snap["frame"].as_u64().unwrap_or(0));
    send(socket, &snap).await
}

/// 處理一則 client 訊息；回傳 false 表示連線該結束。
async fn on_message(socket: &mut WebSocket, state: &WebState, conn: &mut Conn, text: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return send(socket, &json!({ "t": "error", "message": "不是 JSON" })).await;
    };
    match msg["op"].as_str().unwrap_or_default() {
        "hello" => {
            conn.offset = msg["utcOffsetMin"]
                .as_i64()
                .unwrap_or(0)
                .clamp(-14 * 60, 14 * 60) as i32;
            conn.resume = msg["resume"].as_u64();
            state.live().register_offset(conn.offset);
            let welcome = json!({
                "t": "welcome", "protocol": PROTOCOL, "host": "local",
                "version": env!("CARGO_PKG_VERSION"), "remote": false, "thinking": "full", "scope": "local",
            });
            send(socket, &welcome).await
        }
        "subscribe" => {
            let Some(ch) = msg["channel"].as_str().and_then(Channel::parse) else {
                return send(socket, &json!({ "t": "error", "message": "不認得的頻道" })).await;
            };
            // 帶 resume 而且接得上：補送漏掉的 frame；否則送 snapshot
            let missed = conn.resume.and_then(|n| {
                let live = state.live();
                live.frames_since(n).map(|frames| (frames, live.frame_no()))
            });
            match missed {
                Some((frames, now_no)) => {
                    conn.subs.insert(ch.clone(), now_no);
                    for f in frames {
                        if let Some(j) = f.to_json(&ch, conn.offset)
                            && !send(socket, &j).await
                        {
                            return false;
                        }
                    }
                    true
                }
                None => send_snapshot(socket, state, conn, &ch).await,
            }
        }
        "unsubscribe" => {
            if let Some(ch) = msg["channel"].as_str().and_then(Channel::parse) {
                conn.subs.remove(&ch);
            }
            true
        }
        "ack" => {
            let (s, k, r) = (
                msg["session"].as_str().unwrap_or_default(),
                msg["kind"].as_str().unwrap_or_default(),
                msg["ref"].as_str().unwrap_or_default(),
            );
            if let Some(store) = &state.store {
                let mut store = store.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = store.ack(s, k, r, crate::now_ms()) {
                    eprintln!("coding-lab web：ack 寫入失敗：{e}");
                }
            }
            state.live().ack(s, k, r);
            state.flush(crate::now_ms()); // 讓所有連線立刻看到刪除
            true
        }
        "more" => {
            let session = msg["session"].as_str().unwrap_or_default().to_string();
            let before = msg["beforePos"].as_u64();
            let limit = msg["limit"].as_u64().unwrap_or(200).min(MORE_LIMIT) as usize;
            let events = match &state.store {
                Some(store) => {
                    let store = store.lock().unwrap_or_else(|e| e.into_inner());
                    more_events(&store, &session, before, limit).unwrap_or_else(|e| {
                        eprintln!("coding-lab web：載入更多失敗：{e}");
                        Vec::new()
                    })
                }
                None => Vec::new(),
            };
            let reply =
                json!({ "t": "more", "session": session, "beforePos": before, "events": events });
            send(socket, &reply).await
        }
        other => {
            send(
                socket,
                &json!({ "t": "error", "message": format!("不認得的 op：{other}") }),
            )
            .await
        }
    }
}

/// 「載入更多」：窗口內的帶 pos Event，加上卡片的伴隨 Event 與窗口範圍內的狀態行（contracts.md §6）。
/// 伴隨 Event 從窗口第一個 seq 往前 [`COMPANION_LOOKBACK`] 個、往後到下一個帶 pos 的 Event 為止找。
fn more_events(
    store: &Store,
    session: &str,
    before: Option<u64>,
    limit: usize,
) -> Result<Vec<Event>, crate::store::StoreError> {
    let window = store.stream_before(session, before, limit)?;
    let (Some(first), Some(last)) = (window.first(), window.last()) else {
        return Ok(window);
    };
    let next = store.next_pos_after(session, last.pos.unwrap_or(0))?;
    let from = first.seq.saturating_sub(COMPANION_LOOKBACK);
    let to = next.map_or(last.seq + COMPANION_LOOKBACK, |(seq, _)| {
        seq.saturating_sub(1)
    });
    let pool = store.events_by_seq(session, from, to)?;
    Ok(crate::live::assemble_stream(
        window,
        pool,
        next.map(|(_, ts)| ts),
    ))
}

async fn healthz(State(state): State<Arc<WebState>>) -> Response {
    let mut body = {
        let live = state.live();
        json!({
            "version": env!("CARGO_PKG_VERSION"), "protocol": PROTOCOL,
            "sessions": live.session_count(), "frame": live.frame_no(),
        })
    };
    let extra = state
        .health
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let (Some(b), Some(x)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(x.clone());
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn static_file(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "not found").into_response();
    let Some(root) = &state.web_root else {
        return not_found();
    };
    let rel = Path::new(uri.path().trim_start_matches('/'));
    // 只接受一般路徑元件：擋掉 `..`、絕對路徑、Windows 磁碟代號
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return not_found();
    }
    let mut path = root.join(rel);
    if path.is_dir() {
        path = path.join("index.html");
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type(&path))], bytes).into_response(),
        Err(_) => not_found(),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
