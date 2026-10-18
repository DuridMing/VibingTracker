//! web 入口（#636，contracts.md §6–§7）：WebSocket、`/healthz`、HTTP API（[`crate::api`]）、前端靜態檔。
//!
//! 每 50 ms 從 [`Live`] 取一個 frame，用 broadcast 發給各連線，各連線依自己訂的頻道與時區過濾。
//! 連線落後超過 broadcast 容量時直接重送 snapshot（snapshot 就是最新狀態，什麼都不會漏）。
//!
//! 安全（[`gate`]）：本機免金鑰的連線要 `Host` 是 loopback 名稱、`/api` 與 `/ws` 的 `Origin` 要等於 `Host`
//! （否則使用者瀏覽器裡任何網頁都能連 `ws://127.0.0.1:4801` 讀走 transcript；沒帶 `Origin` 的 wscat、curl 放行）。
//! 遠端或 `auth = "always"` 的連線每個請求都要金鑰（[`crate::auth`]）：Bearer 或登入 cookie。

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::auth::{self, Auth, Denied, Identity};
use crate::config::{AuthMode, RemoteThinking};
use crate::event::Event;
use crate::live::{Channel, Frame, Live};
use crate::store::{SessionRow, Store};

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
    auth: Auth,
    /// 遠端連線的 thinking 怎麼送（#668）；本機連線永遠完整
    remote_thinking: Mutex<RemoteThinking>,
    /// 測試用：把 loopback 連線當成遠端（真的非 loopback 來源在單機測不到）
    force_remote: std::sync::atomic::AtomicBool,
    /// 進行中的回放請求數與上限（#742 B-N1）
    replays: std::sync::atomic::AtomicUsize,
    max_replays: std::sync::atomic::AtomicUsize,
}

/// 同一個 collector 同時最多幾個回放請求（#742 B-N1）：回放要讀整段 JSONL、重放、佔記憶體，
/// 連續拖動滑桿或遠端送大範圍不能把 collector 拖垮。
pub const MAX_REPLAYS: usize = 2;

/// 一個回放名額；drop 時還回去。
pub(crate) struct ReplaySlot(Arc<WebState>);

impl Drop for ReplaySlot {
    fn drop(&mut self) {
        self.0.replays.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 同一毫秒建立兩個 [`WebState`]（測試）也要不同
static INSTANCES: AtomicU64 = AtomicU64::new(0);

impl WebState {
    /// `capacity`：每個連線最多落後幾個 frame，超過就改送 snapshot。沒有金鑰檔：只信本機，遠端一律拒絕。
    pub fn new(
        store: Option<Arc<Mutex<Store>>>,
        web_root: Option<PathBuf>,
        capacity: usize,
    ) -> Arc<WebState> {
        WebState::build(store, web_root, capacity, Auth::local_only())
    }

    /// 金鑰在 `<home>/keys.json`；`mode` 決定 localhost 要不要金鑰（#667）。
    pub fn new_with_auth(
        store: Option<Arc<Mutex<Store>>>,
        web_root: Option<PathBuf>,
        capacity: usize,
        home: PathBuf,
        mode: AuthMode,
    ) -> Arc<WebState> {
        WebState::build(store, web_root, capacity, Auth::new(home, mode))
    }

    fn build(
        store: Option<Arc<Mutex<Store>>>,
        web_root: Option<PathBuf>,
        capacity: usize,
        auth: Auth,
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
            auth,
            remote_thinking: Mutex::new(RemoteThinking::OnDemand),
            force_remote: std::sync::atomic::AtomicBool::new(false),
            replays: std::sync::atomic::AtomicUsize::new(0),
            max_replays: std::sync::atomic::AtomicUsize::new(MAX_REPLAYS),
        })
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// `[web].remote_thinking`（#668）。
    pub fn set_remote_thinking(&self, mode: RemoteThinking) {
        *self
            .remote_thinking
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mode;
    }

    pub fn remote_thinking(&self) -> RemoteThinking {
        *self
            .remote_thinking
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 同時進行的回放請求上限（預設 [`MAX_REPLAYS`]）；測試用。
    pub fn set_max_replays(&self, n: usize) {
        self.max_replays.store(n, Ordering::Relaxed);
    }

    /// 佔一個回放名額；滿了回 None（呼叫端回 429）。名額在 [`ReplaySlot`] drop 時還回去。
    pub(crate) fn replay_slot(self: &Arc<Self>) -> Option<ReplaySlot> {
        let max = self.max_replays.load(Ordering::Relaxed);
        let mut n = self.replays.load(Ordering::Acquire);
        loop {
            if n >= max {
                return None;
            }
            match self
                .replays
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(ReplaySlot(self.clone())),
                Err(now) => n = now,
            }
        }
    }

    /// 測試用：所有連線都當成遠端（剝除敏感輸出與 thinking）。
    pub fn set_force_remote(&self, on: bool) {
        self.force_remote.store(on, Ordering::Relaxed);
    }

    /// 這條連線算不算遠端：來源不是 loopback（[`is_remote`]），或測試強制。
    pub fn remote_peer(&self, peer: SocketAddr) -> bool {
        self.force_remote.load(Ordering::Relaxed) || is_remote(peer)
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

/// 開始提供 web 入口；回傳實際綁定的位址（測試用 port 0）。綁定位址能不能用由 [`crate::config`] 把關。
pub async fn serve_web(
    addr: SocketAddr,
    state: Arc<WebState>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let api = Router::new()
        .route("/api/sessions", get(crate::api::sessions))
        .route("/api/sessions/{id}/events", get(crate::api::events))
        .route("/api/files", get(crate::api::files))
        .route("/api/projects/replay", get(crate::api::project_replay))
        .route("/api/branches", get(crate::api::branches))
        .route("/api/branch-diff", get(crate::api::branch_diff))
        .route("/api/graph", get(crate::api::graph))
        .route("/api/commit", get(crate::api::commit))
        .route("/api/commit-diff", get(crate::api::commit_diff))
        .route(
            "/api/sessions/{id}/thinking/{seq}",
            get(crate::api::thinking),
        )
        .route_layer(middleware::from_fn(require_same_origin));
    let app = Router::new()
        .merge(api)
        .route("/ws", get(ws_handler))
        .route("/healthz", get(healthz))
        .route("/auth/login", get(auth_login_page).post(auth_login))
        .route("/auth/once/{code}", get(auth_once))
        .route("/auth/logout", post(auth_logout))
        .fallback(get(static_file))
        .layer(middleware::from_fn_with_state(state.clone(), gate))
        .layer(middleware::map_response(no_framing))
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

/// 每個請求的門口：決定這條連線是「本機免金鑰」還是要驗金鑰，並把 [`Identity`] 放進 request extensions。
///
/// - `/healthz` 與 `/auth/*` 不驗（登入頁本身要能開）。
/// - 本機免金鑰（loopback 來源、沒有轉發標頭，且 `auth = "local-open"`）：`Host` 必須是 loopback 名稱——DNS rebinding 時攻擊者的
///   網域解析到 127.0.0.1，瀏覽器送出的 `Origin` 與 `Host` 都是攻擊者網域、彼此相同，只比 Origin 擋不住。
///   `/api` 與 `/ws` 另外要 `Origin == Host`。
/// - 要金鑰的連線：Bearer 或 cookie（cookie 要配 `Origin`）。沒有或不對：API 與 WS 回 401／403，網頁導去登入頁。
///   cookie 是 SameSite=Strict 且綁 Host，rebinding 的網域拿不到它，所以這裡不限制 `Host`（tailnet 的 MagicDNS 名稱要能用）。
async fn gate(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let open = path == "/healthz" || path.starts_with("/auth/");
    let guarded = path.starts_with("/api/") || path == "/ws";
    // 反向代理轉來的：來源與 Host 都可能是 loopback（nginx 預設改寫 Host），只有轉發標頭看得出來（S-1）
    let proxied = proxied(req.headers());
    req.extensions_mut()
        .insert(Remote(proxied || state.remote_peer(peer)));
    if !proxied && !state.auth.needs_auth(peer) {
        if !host_is_loopback(req.headers()) {
            return (StatusCode::FORBIDDEN, "Host 不是本機").into_response();
        }
        if guarded && !same_origin(req.headers()) {
            return (StatusCode::FORBIDDEN, "Origin 與 Host 不符").into_response();
        }
        req.extensions_mut().insert(Identity::Local);
        return next.run(req).await;
    }
    if open {
        return next.run(req).await;
    }
    match state.auth.identify(req.headers(), crate::now_ms()) {
        Ok(id) => {
            req.extensions_mut().insert(id);
            next.run(req).await
        }
        Err(Denied::OriginMismatch) => {
            (StatusCode::FORBIDDEN, "Origin 與 Host 不符").into_response()
        }
        Err(denied) => {
            if let Denied::BadCredentials(why) = &denied {
                crate::logging::log(&format!("web：{peer} 的憑證被拒：{why}"));
            }
            if guarded {
                (
                    StatusCode::UNAUTHORIZED,
                    "需要金鑰：登入或 Authorization: Bearer clk_…",
                )
                    .into_response()
            } else {
                Redirect::to("/auth/login").into_response()
            }
        }
    }
}

const LOGIN_PAGE: &str = r#"<!doctype html>
<html lang="zh-Hant"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>coding-lab 登入</title>
<style>
body{font:15px/1.5 system-ui,sans-serif;background:#f4f4f3;color:#222;display:flex;min-height:100vh;margin:0;align-items:center;justify-content:center}
main{background:#fff;border:1px solid #ddd;border-radius:8px;padding:24px 28px;max-width:440px;width:calc(100% - 32px)}
h1{font-size:18px;margin:0 0 12px}input{width:100%;box-sizing:border-box;font:inherit;padding:8px 10px;border:1px solid #bbb;border-radius:6px}
button{margin-top:12px;font:inherit;padding:8px 16px;border:0;border-radius:6px;background:#2a6;color:#fff}
p{margin:8px 0}code{background:#eee;padding:1px 4px;border-radius:3px}.err{color:#b00}.hint{color:#555;font-size:13px}
@media (prefers-color-scheme:dark){body{background:#1b1b1b;color:#ddd}main{background:#242424;border-color:#333}input{background:#1b1b1b;color:#ddd;border-color:#444}code{background:#333}}
</style></head><body><main>
<h1>coding-lab</h1>
<p>這個 collector 需要金鑰才能看。貼上在伺服器上用 <code>coding-lab key create --name &lt;名稱&gt;</code> 建立的金鑰（<code>clk_…</code>）：</p>
<!--ERROR-->
<form method="post" action="/auth/login"><input name="key" type="password" autocomplete="off" placeholder="clk_xxxxxxxx_…" autofocus required><button type="submit">登入</button></form>
<p class="hint">登入後 30 天內不必再貼；撤銷金鑰（<code>coding-lab key revoke &lt;id&gt;</code>）會立即登出。<!--NOKEYS--></p>
</main></body></html>"#;

fn login_html(state: &WebState, error: Option<&str>) -> Html<String> {
    let mut page = LOGIN_PAGE.replace(
        "<!--ERROR-->",
        &error
            .map(|e| format!("<p class=\"err\">{e}</p>"))
            .unwrap_or_default(),
    );
    if !state.auth.has_keys() {
        page = page.replace(
            "<!--NOKEYS-->",
            "<br>這台還沒有任何金鑰：請先在伺服器上執行 <code>coding-lab key create --name laptop</code>。",
        );
    }
    Html(page)
}

async fn auth_login_page(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
    let error = uri
        .query()
        .is_some_and(|q| q.split('&').any(|kv| kv == "error=1"))
        .then_some("金鑰不對、已撤銷或已到期");
    login_html(&state, error).into_response()
}

fn cookie_response(location: &str, cookie: &str, headers: &HeaderMap) -> Response {
    let set = auth::set_cookie_header(
        cookie,
        auth::COOKIE_TTL_MS / 1000,
        auth::forwarded_https(headers),
    );
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, location.to_string()),
            (header::SET_COOKIE, set),
        ],
    )
        .into_response()
}

/// `POST /auth/login`（表單 `key=clk_…`）：成功發 cookie 導回首頁；失敗 401（每個 IP 每分鐘最多 10 次）。
async fn auth_login(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let now = crate::now_ms();
    if !state.auth.login_allowed(peer.ip(), now) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            login_html(&state, Some("登入失敗太多次，一分鐘後再試")),
        )
            .into_response();
    }
    let key = auth::form_value(&String::from_utf8_lossy(&body), "key").unwrap_or_default();
    match state.auth.login(key.trim(), now) {
        Ok((cookie, id, scope)) => {
            crate::logging::log(&format!(
                "web：金鑰 {id}（{}）從 {peer} 登入",
                scope.as_str()
            ));
            cookie_response("/", &cookie, &headers)
        }
        Err(why) => {
            state.auth.login_failed(peer.ip(), now);
            crate::logging::log(&format!("web：{peer} 登入失敗：{why}"));
            (
                StatusCode::UNAUTHORIZED,
                login_html(&state, Some("金鑰不對、已撤銷或已到期")),
            )
                .into_response()
        }
    }
}

/// `GET /auth/once/<code>`：一次性登入連結（`coding-lab key login-link`），5 分鐘內、只能用一次。
async fn auth_once(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::extract::Path(code): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    let now = crate::now_ms();
    if !state.auth.login_allowed(peer.ip(), now) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "登入失敗太多次，一分鐘後再試",
        )
            .into_response();
    }
    match state.auth.login_once(&code, now) {
        Ok((cookie, id)) => {
            crate::logging::log(&format!("web：金鑰 {id} 從 {peer} 以一次性連結登入"));
            cookie_response("/", &cookie, &headers)
        }
        Err(why) => {
            state.auth.login_failed(peer.ip(), now);
            crate::logging::log(&format!("web：{peer} 的一次性連結被拒：{why}"));
            (
                StatusCode::NOT_FOUND,
                "連結已失效或用過；在伺服器上重新執行 coding-lab key login-link",
            )
                .into_response()
        }
    }
}

async fn auth_logout(headers: HeaderMap) -> Response {
    let set = auth::set_cookie_header("", 0, auth::forwarded_https(&headers));
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/auth/login".to_string()),
            (header::SET_COOKIE, set),
        ],
    )
        .into_response()
}

/// gate 判定的「這條連線算不算遠端」：來源不是 loopback、測試強制，或經過反向代理（[`proxied`]）。
/// handler 一律讀這個，不再各自看來源位址——代理轉來的連線來源是 127.0.0.1，只看位址會當成本機（S-1）。
#[derive(Clone, Copy, Debug)]
pub struct Remote(pub bool);

/// 帶任何一個轉發標頭就是反向代理轉來的。代理一定會加其中之一（nginx 的 `proxy_set_header`、Caddy 預設、
/// RFC 7239 `Forwarded`）；Claude Code 與瀏覽器直連都不會帶。標頭本身不可信，所以只拿來「升級成遠端」，不拿來放行。
pub fn proxied(headers: &HeaderMap) -> bool {
    [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
    ]
    .iter()
    .any(|h| headers.contains_key(*h))
}

/// 每個回應（含 gate 拒絕的）都禁止被嵌進 iframe（F-7）：local-open 時任何網站都能把這頁放進 iframe、
/// 誘導使用者點「全部確認」清掉危險警示。hub 的 web 入口也用。
pub(crate) async fn no_framing(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("frame-ancestors 'none'"),
    );
    res
}

/// `Host` 是 loopback 名稱（hub 的 web 入口也用，#663）。
pub(crate) fn host_is_loopback(headers: &HeaderMap) -> bool {
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
    auth::same_origin(headers)
}

pub fn thinking_str(mode: RemoteThinking) -> &'static str {
    match mode {
        RemoteThinking::OnDemand => "on_demand",
        RemoteThinking::Hide => "hide",
        RemoteThinking::Show => "show",
    }
}

/// 連線是不是從別台來的。SSH 通道進來的也是本機位址，等同信任；遠端連線送出前剝除敏感輸出（不變式 6）。
pub fn is_remote(peer: SocketAddr) -> bool {
    !peer.ip().is_loopback()
}

async fn ws_handler(
    State(state): State<Arc<WebState>>,
    Extension(Remote(remote)): Extension<Remote>,
    Extension(identity): Extension<Identity>,
    ws: WebSocketUpgrade,
) -> Response {
    // Origin 與金鑰都在 gate 驗過了，遠端與否也是 gate 判定的
    ws.on_upgrade(move |socket| connection(socket, state, remote, identity))
}

struct Conn {
    /// 訂閱的頻道，以及這個頻道已經送到的 frame 編號
    subs: BTreeMap<Channel, u64>,
    offset: i32,
    /// hello 帶的 frame 編號與收到 hello 的時間
    resume: Option<(u64, Instant)>,
    /// 遠端連線（gate 判定的 [`Remote`]）
    remote: bool,
    /// 本機或哪一把金鑰（決定 `welcome.scope` 與能不能 ack）
    identity: Identity,
    /// 連線建立時的 `remote_thinking`（只對 remote 有意義）
    thinking: RemoteThinking,
}

/// 送一則訊息；遠端連線先剝除敏感輸出（#654）與 thinking 全文（#668，依 `remote_thinking`）。
async fn send(socket: &mut WebSocket, conn: &Conn, v: &Value) -> bool {
    let text = if conn.remote {
        redact_remote_copy(v, conn.thinking).to_string()
    } else {
        v.to_string()
    };
    socket.send(Message::Text(text.into())).await.is_ok()
}

impl WebState {
    /// HTTP API 回應送給遠端前的剝除：敏感輸出（#654）＋依 `remote_thinking` 的 thinking（#668）。
    /// WS 的 [`send`] 做同一件事；兩邊共用這個判斷，REST 不能繞過 WS 的佔位。`remote` 是 gate 判定的 [`Remote`]。
    pub fn redact_for(&self, remote: bool, body: &mut Value) {
        if remote {
            *body = redact_remote_copy(body, self.remote_thinking());
        }
    }
}

/// 送給遠端連線的副本（不變式 6、7，contracts §4）：WS、每個 API、回放、hub 轉送都走這一個，一次走訪產出：
/// - 命中敏感規則（`payload.sensitive`）的 Event 剝內容並加 `redacted: true`：`tool.result.output` 換 `""`；
///   `tool.called.input` 只留路徑類欄位；`file.write.content` 換 `""`；`file.edit.hunk.lines` 換 `[]`（行號與計數保留，C8）。
/// - `thinking` 不是 `Show` 時：`assistant.thinking` 的 `text` 換 `""`、加 `deferred` 與 `chars`（#668）；
///   sessions 列裡來源是 thinking 的 `doing` 換 null（C2）。
/// - 所有字串裡的 `clk_<id>_<secret>` 只留 `clk_<id>_…`（#720）。
///
/// frame、snapshot、載入更多、API 回應的形狀不同，所以整個走一遍；原值不動（JSONL、store、本機連線照舊）。
pub fn redact_remote_copy(v: &Value, thinking: RemoteThinking) -> Value {
    copy_for_remote(v, thinking != RemoteThinking::Show)
}

/// `tool.called.input` 剝除後留下的欄位：畫面要顯示是哪個檔或哪個指令。
const KEEP_INPUT: [&str; 4] = ["file_path", "notebook_path", "path", "command"];

fn copy_for_remote(v: &Value, defer: bool) -> Value {
    match v {
        Value::String(s) if s.contains("clk_") => {
            let mut masked = Value::String(s.clone());
            crate::sensitive::redact_keys(&mut masked);
            masked
        }
        Value::Object(map) => {
            let mut out: serde_json::Map<String, Value> = map
                .iter()
                .map(|(k, child)| (k.clone(), copy_for_remote(child, defer)))
                .collect();
            redact_event(map, &mut out, defer);
            if defer && map.get("doingSource").and_then(Value::as_str) == Some("thinking") {
                out.insert("doing".into(), Value::Null);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|child| copy_for_remote(child, defer))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `out` 是 `orig` 已複製（字串已遮金鑰）的副本；是 Event 時依種類剝內容。
fn redact_event(
    orig: &serde_json::Map<String, Value>,
    out: &mut serde_json::Map<String, Value>,
    defer: bool,
) {
    let Some(kind) = orig.get("type").and_then(Value::as_str) else {
        return;
    };
    let Some(Value::Object(p)) = out.get_mut("payload") else {
        return;
    };
    if kind == "assistant.thinking" {
        if defer {
            let chars = orig["payload"]["text"]
                .as_str()
                .map_or(0, |t| t.chars().count());
            p.insert("text".into(), Value::String(String::new()));
            p.insert("deferred".into(), Value::Bool(true));
            p.insert("chars".into(), json!(chars));
        }
        return;
    }
    if p.get("sensitive").and_then(Value::as_bool) != Some(true) {
        return;
    }
    match kind {
        "tool.result" => {
            p.insert("output".into(), Value::String(String::new()));
        }
        "tool.called" => {
            let kept = match p.get("input") {
                Some(Value::Object(input)) => input
                    .iter()
                    .filter(|(k, _)| KEEP_INPUT.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                _ => serde_json::Map::new(),
            };
            p.insert("input".into(), Value::Object(kept));
        }
        "file.write" => {
            p.insert("content".into(), Value::String(String::new()));
        }
        "file.edit" => {
            if let Some(Value::Object(hunk)) = p.get_mut("hunk") {
                hunk.insert("lines".into(), Value::Array(Vec::new()));
            }
        }
        _ => return,
    }
    p.insert("redacted".into(), Value::Bool(true));
}

async fn connection(mut socket: WebSocket, state: Arc<WebState>, remote: bool, identity: Identity) {
    let mut rx = state.frames.subscribe();
    let mut conn = Conn {
        subs: BTreeMap::new(),
        offset: 0,
        resume: None,
        remote,
        identity,
        thinking: state.remote_thinking(),
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
                    let channels: Vec<Channel> = conn.subs.keys().cloned().collect();
                    for ch in channels {
                        if conn.subs.get(&ch).is_some_and(|since| f.no <= *since) {
                            continue;
                        }
                        conn.subs.insert(ch.clone(), f.no);
                        if let Some(j) = f.to_json(&ch, conn.offset)
                            && !send(&mut socket, &conn, &j).await
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
    send(socket, conn, &snap).await
}

/// 處理一則 client 訊息；回傳 false 表示連線該結束。
async fn on_message(socket: &mut WebSocket, state: &WebState, conn: &mut Conn, text: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return send(
            socket,
            conn,
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
                "version": env!("CARGO_PKG_VERSION"), "remote": conn.remote,
                "thinking": if conn.remote { thinking_str(conn.thinking) } else { "full" },
                "scope": conn.identity.scope_str(),
            });
            send(socket, conn, &welcome).await
        }
        "subscribe" => {
            let Some(ch) = msg["channel"].as_str().and_then(Channel::parse) else {
                return send(
                    socket,
                    conn,
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
                            && !send(socket, conn, &j).await
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
            if !conn.identity.can_operate() {
                return send(
                    socket,
                    conn,
                    &json!({ "t": "error", "message": "這把金鑰只能看（view）；「已確認」需要 operate 金鑰" }),
                )
                .await;
            }
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
            send(socket, conn, &reply).await
        }
        other => {
            send(
                socket,
                conn,
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

async fn healthz(State(state): State<Arc<WebState>>) -> Response {
    let mut body = {
        let live = state.live();
        json!({
            "version": env!("CARGO_PKG_VERSION"), "protocol": PROTOCOL,
            "sessions": live.session_count(), "open": live.open_session_count(),
            "frame": live.frame_no(),
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
    static_response(state.web_root.as_deref(), &uri).await
}

/// 前端靜態檔（hub 的 web 入口也用同一份 `web/`，#663）。
pub(crate) async fn static_response(root: Option<&Path>, uri: &Uri) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "not found").into_response();
    let Some(root) = root else {
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
