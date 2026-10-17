//! web 入口（#636，contracts.md §6–§7）：WebSocket、`/healthz`、HTTP API（[`crate::api`]）、前端靜態檔。
//!
//! 每 50 ms 從 [`Live`] 取一個 frame，用 broadcast 發給各連線，各連線依自己訂的頻道與時區過濾。
//! 連線落後超過 broadcast 容量時直接重送 snapshot（snapshot 就是最新狀態，什麼都不會漏）。
//!
//! 安全：P6 的金鑰驗證完成前只綁 loopback；連 localhost 也檢查 `Origin`，否則使用者瀏覽器裡任何網頁
//! 都能連 `ws://127.0.0.1:4801` 讀走 transcript。沒帶 `Origin` 的程式（wscat、curl）放行。

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::event::Event;
use crate::live::{Channel, Frame, Live};
use crate::store::{ProjectKey, SessionRow, Store};

pub const PROTOCOL: u64 = 1;
pub const FLUSH_EVERY: Duration = Duration::from_millis(50);
pub(crate) const MORE_LIMIT: u64 = 500;
/// hello 帶的 `resume` 只套用在這段時間內收到的 subscribe：前端重連時是 hello 後立刻重送所有訂閱，
/// 之後才新開的頻道客戶端沒有它的狀態，要給 snapshot（server 分不出兩者，只能看時間）。
pub const RESUME_WINDOW: Duration = Duration::from_secs(2);
const COMPANION_LOOKBACK: u64 = 1000;

pub struct WebState {
    live: Mutex<Live>,
    store: Option<Arc<Mutex<Store>>>,
    web_root: Option<PathBuf>,
    frames: broadcast::Sender<Arc<Frame>>,
    /// collector 主迴圈回報的健康資訊（tail 數、排程延遲…），併進 /healthz
    health: Mutex<Value>,
    resume_window_ms: AtomicU64,
    /// 這次啟動的識別（`welcome.instance`）：frame 編號每次啟動從頭算，resume 要同一個 instance 才接得上
    instance: String,
}

/// 同一毫秒建立兩個 [`WebState`]（測試）也要不同
static INSTANCES: AtomicU64 = AtomicU64::new(0);

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
            resume_window_ms: AtomicU64::new(RESUME_WINDOW.as_millis() as u64),
            instance: format!(
                "{}-{}",
                crate::now_ms(),
                INSTANCES.fetch_add(1, Ordering::Relaxed)
            ),
        })
    }

    /// hello 的 `resume` 對之後多久內的 subscribe 有效，見 [`RESUME_WINDOW`]（測試用）。
    pub fn set_resume_window(&self, window: Duration) {
        self.resume_window_ms
            .store(window.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn set_health(&self, extra: Value) {
        *self.health.lock().unwrap_or_else(|e| e.into_inner()) = extra;
    }

    pub(crate) fn store(&self) -> Option<&Arc<Mutex<Store>>> {
        self.store.as_ref()
    }

    pub(crate) fn live(&self) -> MutexGuard<'_, Live> {
        // 有執行緒 panic 也繼續用（狀態只是顯示用的彙總）
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// live 第一次看到的 session（collector 重啟後）先用 store 的列種起來（#676）。
    /// 呼叫端先把 `events` 放進 store 的緩衝才給這裡，store 的列還不含它們，不會重複計算。
    pub fn apply(&self, events: &[Event]) {
        let mut unseen: Vec<(&str, u64)> = Vec::new();
        {
            let live = self.live();
            for e in events {
                if !live.has_session(&e.session) && unseen.iter().all(|(s, _)| *s != e.session) {
                    unseen.push((&e.session, e.ts_seen));
                }
            }
        }
        let rows = self.stored_sessions(unseen.iter().map(|(s, _)| *s));
        let mut live = self.live();
        for (r, (_, ts)) in rows.iter().zip(&unseen) {
            if let Some(r) = r {
                live.seed(r, *ts);
            }
        }
        for e in events {
            live.apply(e);
        }
    }

    /// 啟動時重放 store 最近的 Event 重建 live，再把累計值等對齊 store 的列：重放區間可能沒涵蓋
    /// session 的開頭，而 store 的列已經含這些 Event，不能邊種邊重放（會算兩次）。
    pub fn rebuild(&self, events: &[Event]) {
        let mut ids: Vec<&str> = Vec::new();
        for e in events {
            if !ids.contains(&e.session.as_str()) {
                ids.push(&e.session);
            }
        }
        let rows = self.stored_sessions(ids.iter().copied());
        let mut live = self.live();
        for e in events {
            live.apply(e);
        }
        for r in rows.iter().flatten() {
            live.seed(r, r.status_since);
        }
    }

    /// 依序讀 store 的 `sessions` 列（沒有 store、沒有這列或讀不到都是 None）；不同時拿 live 的鎖。
    fn stored_sessions<'a>(&self, ids: impl Iterator<Item = &'a str>) -> Vec<Option<SessionRow>> {
        let Some(store) = &self.store else {
            return ids.map(|_| None).collect();
        };
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        ids.map(|id| store.session(id).ok().flatten()).collect()
    }

    /// merge 定期重算的 session 分數（見 [`Live::set_scores`]）。
    pub fn set_scores(&self, scores: &[(String, i64)]) {
        self.live().set_scores(scores);
    }

    /// 重啟重建後依 store 的佇列去掉已確認的注意列（見 [`Live::keep_attention`]）。
    pub fn keep_attention(&self, open: &std::collections::HashSet<String>) {
        self.live().keep_attention(open);
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
    let api = Router::new()
        .route("/api/sessions", get(crate::api::sessions))
        .route("/api/sessions/{id}/events", get(crate::api::events))
        .route("/api/files", get(crate::api::files))
        .route("/api/projects/events", get(crate::api::project_events))
        .route("/api/branches", get(crate::api::branches))
        .route("/api/branch-diff", get(crate::api::branch_diff))
        .route("/api/graph", get(crate::api::graph))
        .route("/api/commit", get(crate::api::commit))
        .route("/api/commit-diff", get(crate::api::commit_diff))
        .route_layer(middleware::from_fn(require_same_origin));
    let app = Router::new()
        .merge(api)
        .route("/ws", get(ws_handler))
        .route("/healthz", get(healthz))
        .fallback(get(static_file))
        .layer(middleware::from_fn(loopback_host))
        .with_state(state);
    let task = tokio::spawn(async move {
        // 連線來源位址：遠端連線送出前剝除敏感輸出（#654）
        let app = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, app).await {
            crate::logging::log(&format!("web：{e}"));
        }
    });
    Ok((local, task))
}

/// 只接受 loopback 名稱的 `Host`。DNS rebinding 時攻擊者的網域解析到 127.0.0.1，瀏覽器送出的
/// `Origin` 與 `Host` 都是攻擊者網域、彼此相同，[`same_origin`] 擋不住；P6 開放遠端時改由金鑰驗證把關。
async fn loopback_host(req: Request, next: Next) -> Response {
    if host_is_loopback(req.headers()) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "Host 不是本機").into_response()
    }
}

fn host_is_loopback(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `/api/*`：同 `/ws`，使用者瀏覽器裡別的網頁不能讀走 session 資料。
async fn require_same_origin(req: Request, next: Next) -> Response {
    if same_origin(req.headers()) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "Origin 與 Host 不符").into_response()
    }
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

/// 連線是不是從別台來的。P6 之前 web 只綁 loopback，SSH 通道進來的也是本機位址，所以目前一律是本機；
/// P6 開放非 loopback 綁定後，遠端連線送出前剝除敏感輸出（不變式 6）。
pub fn is_remote(peer: SocketAddr) -> bool {
    !peer.ip().is_loopback()
}

async fn ws_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "Origin 與 Host 不符").into_response();
    }
    let remote = is_remote(peer);
    ws.on_upgrade(move |socket| connection(socket, state, remote))
}

struct Conn {
    /// 訂閱的頻道，以及這個頻道已經送到的 frame 編號
    subs: BTreeMap<Channel, u64>,
    offset: i32,
    /// hello 帶的 frame 編號與收到 hello 的時間
    resume: Option<(u64, Instant)>,
    /// 從別台連進來（[`is_remote`]）
    remote: bool,
}

async fn send(socket: &mut WebSocket, remote: bool, v: &Value) -> bool {
    let text = if remote {
        let mut v = v.clone();
        crate::sensitive::redact_remote(&mut v);
        v.to_string()
    } else {
        v.to_string()
    };
    socket.send(Message::Text(text.into())).await.is_ok()
}

async fn connection(mut socket: WebSocket, state: Arc<WebState>, remote: bool) {
    let mut rx = state.frames.subscribe();
    let mut conn = Conn {
        subs: BTreeMap::new(),
        offset: 0,
        resume: None,
        remote,
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
                            && !send(&mut socket, conn.remote, &j).await
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
    send(socket, conn.remote, &snap).await
}

/// 處理一則 client 訊息；回傳 false 表示連線該結束。
async fn on_message(socket: &mut WebSocket, state: &WebState, conn: &mut Conn, text: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return send(
            socket,
            conn.remote,
            &json!({ "t": "error", "message": "不是 JSON" }),
        )
        .await;
    };
    match msg["op"].as_str().unwrap_or_default() {
        "hello" => {
            conn.offset = msg["utcOffsetMin"]
                .as_i64()
                .unwrap_or(0)
                .clamp(-14 * 60, 14 * 60) as i32;
            // 只接受同一個 instance 的 `{instance, frame}`；舊格式（只有數字）分不出是不是重啟前的編號
            conn.resume = (msg["resume"]["instance"].as_str() == Some(state.instance.as_str()))
                .then(|| msg["resume"]["frame"].as_u64())
                .flatten()
                .map(|n| (n, Instant::now()));
            state.live().register_offset(conn.offset);
            let welcome = json!({
                "t": "welcome", "protocol": PROTOCOL, "instance": state.instance, "host": "local",
                "version": env!("CARGO_PKG_VERSION"), "remote": conn.remote, "thinking": "full", "scope": "local",
            });
            send(socket, conn.remote, &welcome).await
        }
        "subscribe" => {
            let Some(ch) = msg["channel"].as_str().and_then(Channel::parse) else {
                return send(
                    socket,
                    conn.remote,
                    &json!({ "t": "error", "message": "不認得的頻道" }),
                )
                .await;
            };
            // 帶 resume 而且接得上：補送漏掉的 frame；否則送 snapshot
            let window = Duration::from_millis(state.resume_window_ms.load(Ordering::Relaxed));
            let missed = conn
                .resume
                .filter(|(_, at)| at.elapsed() <= window)
                .and_then(|(n, _)| {
                    let live = state.live();
                    live.frames_since(n).map(|frames| (frames, live.frame_no()))
                });
            match missed {
                Some((frames, now_no)) => {
                    conn.subs.insert(ch.clone(), now_no);
                    for f in frames {
                        if let Some(j) = f.to_json(&ch, conn.offset)
                            && !send(socket, conn.remote, &j).await
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
                    crate::logging::log(&format!("web：ack 寫入失敗：{e}"));
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
                        crate::logging::log(&format!("web：載入更多失敗：{e}"));
                        Vec::new()
                    })
                }
                None => Vec::new(),
            };
            let reply =
                json!({ "t": "more", "session": session, "beforePos": before, "events": events });
            send(socket, conn.remote, &reply).await
        }
        other => {
            send(
                socket,
                conn.remote,
                &json!({ "t": "error", "message": format!("不認得的 op：{other}") }),
            )
            .await
        }
    }
}

/// 「載入更多」：窗口內的帶 pos Event，加上卡片的伴隨 Event 與窗口範圍內的狀態行（contracts.md §6）。
/// 伴隨 Event 從窗口第一個 seq 往前、最後一個 seq 往後各 [`COMPANION_LOOKBACK`] 個找：平行工具的結果
/// 常在下一張卡片進串流之後才到；伴隨 Event 依 toolUseId 過濾，不會混進窗口外的卡片。
/// 狀態行仍以下一個帶 pos 的 Event 的時間為上限。
pub(crate) fn more_events(
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
    let pool = store.events_by_seq(session, from, last.seq + COMPANION_LOOKBACK)?;
    Ok(crate::live::assemble_stream(
        window,
        pool,
        next.map(|(_, ts)| ts),
    ))
}

/// 全專案回放（#702）的一頁：先依跨 session 順序鍵挑出 `limit` 個串流位置，再把每個 session 的那一段
/// 照「載入更多」組回來（卡片的呼叫、狀態、結果一起帶）。回傳 (Event, 下一頁的游標)；沒有更舊的時游標是 None。
pub(crate) fn project_events(
    store: &Store,
    root: &str,
    from: u64,
    to: u64,
    before: Option<&ProjectKey>,
    limit: usize,
) -> Result<(Vec<Event>, Option<ProjectKey>), crate::store::StoreError> {
    let window = store.project_window(root, from, to, before, limit)?;
    // 同一個 session 在這一頁的 pos 範圍；pos 在 session 內連續，用範圍取不受 ts_seen 偶爾倒退影響
    let mut spans: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for k in &window {
        let e = spans.entry(&k.session).or_insert((k.pos, k.pos));
        e.0 = e.0.min(k.pos);
        e.1 = e.1.max(k.pos);
    }
    let mut out = Vec::new();
    for (session, (lo, hi)) in spans {
        out.extend(more_events(
            store,
            session,
            Some(hi + 1),
            (hi - lo + 1) as usize,
        )?);
    }
    let next = (window.len() == limit)
        .then(|| window.last().cloned())
        .flatten();
    Ok((out, next))
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
