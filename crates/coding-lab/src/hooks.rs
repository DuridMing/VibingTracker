//! hooks 入口（#630，PLAN「collector 設計」的 hooks 模組）。
//!
//! 只綁 127.0.0.1，`POST /hook` 收 Claude Code 的 http hook（以及 `ensure` 轉送的 SessionStart）。
//! 回應一律是空的 200：不帶任何內容才不會影響 Claude Code（零 token、不攔截）；任何錯誤都吞掉寫 log。
//! hook 用 `try_send` 交給 merge **之後**才回應，merge 忙時直接丟棄並計數，絕不讓 Claude Code 等。
//!
//! 閒置結束時（#685）先 [`HookServer::set_closing`]：`ensure` 轉送的 SessionStart 改回 503，ensure 會等埠放掉後
//! 拉起新的 collector 再送一次；接著 [`HookServer::close`] 放掉埠，之後才停 tail、flush。
//!
//! `POST /stop`（#700）：`coding-lab stop` 用它請 collector 走同一條正常結束路徑。背景 collector 沒有 console
//! （Windows 更沒人能按 Ctrl-C），這是三平台都通的停法。和 hook 一樣只綁 loopback、不驗金鑰：能連到這個埠的
//! 程序本來就能殺掉 collector。
//!
//! 來源檢查（#762 S-4、D10）：只綁 loopback 擋不住使用者瀏覽器裡的網頁——它們能對 127.0.0.1 送 no-cors 的 POST
//! （`text/plain`，不觸發 preflight），偽造 session、危險指令或停掉 collector。所以帶 `Origin` 的一律 403
//! （Claude Code 不帶，瀏覽器跨站送的一定帶）；`/hook` 不是 `application/json` 回 415；`/stop` 要帶 [`STOP_HEADER`]。

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::hook::HookInput;
use crate::merge::Input;

pub const HOOK_PATH: &str = "/hook";
/// `coding-lab stop` 打的路徑（#700）
pub const STOP_PATH: &str = "/stop";
/// `ensure` 轉送時帶的標頭（值是 `ensure`）
pub const VIA_HEADER: &str = "x-coding-lab-via";
/// `POST /stop` 要帶的標頭（值是 `1`）：瀏覽器的 no-cors 請求帶不了自訂標頭
pub const STOP_HEADER: &str = "x-coding-lab-stop";
/// 放掉埠時最多等進行中的請求多久
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// hook 的 body 上限（#740 L-2）：Edit 的 PostToolUse 帶整份 originalFile
pub const BODY_MAX: usize = 64 * 1024 * 1024;
/// Edit／Write 讀檔探測最多等多久（#762 W-M5）：逾時就不帶探測結果，hook 照常處理
pub const PROBE_LIMIT: Duration = Duration::from_millis(300);

struct HookState {
    tx: mpsc::Sender<Input>,
    closing: Arc<AtomicBool>,
    /// `POST /stop` 來過（permit 會留著，主迴圈晚一點等也等得到）
    stop: Arc<Notify>,
    /// merge 的 channel 滿了而丟掉的 hook 數（/healthz 的 dropped）
    dropped: Arc<AtomicU64>,
    /// 讀檔探測的時限（毫秒）
    probe_limit_ms: Arc<AtomicU64>,
}

/// 執行中的 hook 入口。
pub struct HookServer {
    pub addr: SocketAddr,
    closing: Arc<AtomicBool>,
    stop: Arc<Notify>,
    probe_limit_ms: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    shutdown: Option<oneshot::Sender<()>>,
    /// 不再等進行中的請求、直接收掉（close 等過 [`CLOSE_GRACE`] 之後）
    force: Option<oneshot::Sender<()>>,
    /// hook 執行緒結束時完成
    done: oneshot::Receiver<()>,
}

impl HookServer {
    /// 決定要結束（或取消結束）：結束中 ensure 轉送的 hook 回 503，其他 hook 照收。
    pub fn set_closing(&self, closing: bool) {
        self.closing.store(closing, Ordering::SeqCst);
    }

    /// 啟動以來因 merge 的佇列滿而丟掉的 hook 數。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 讀檔探測的時限（預設 [`PROBE_LIMIT`]，0 表示不讀檔；測試用）。
    pub fn set_probe_limit(&self, limit: Duration) {
        self.probe_limit_ms
            .store(limit.as_millis() as u64, Ordering::SeqCst);
    }

    /// 收到 `POST /stop` 時完成（#700）；主迴圈拿它放進 select，和閒置結束、Ctrl-C 走同一條路。
    pub fn stop_requested(&self) -> Arc<Notify> {
        self.stop.clone()
    }

    /// 不再接新連線並放掉埠；進行中的請求最多等 [`CLOSE_GRACE`]。丟掉 `HookServer` 不會關閉。
    pub async fn close(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if tokio::time::timeout(CLOSE_GRACE, &mut self.done)
            .await
            .is_err()
        {
            if let Some(tx) = self.force.take() {
                let _ = tx.send(());
            }
            let _ = (&mut self.done).await;
        }
    }
}

/// 開始收 hook；回傳實際綁定的位址（測試用 port 0）。非 loopback 位址一律拒絕。
///
/// hook 入口跑在自己的執行緒與 current_thread runtime 上（#740 L-7、X3）：web 的 handler 卡住主 runtime 的
/// worker（例如在 async 裡等 store 的鎖）時，hook 照樣在 100 ms 內回 200（不變式 4）。它只做 try_send 與
/// 讀檔探測（blocking pool），一條執行緒就夠。
pub async fn serve_hooks(addr: SocketAddr, tx: mpsc::Sender<Input>) -> io::Result<HookServer> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("hook 入口只能綁 127.0.0.1，收到 {addr}"),
        ));
    }
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let closing = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::new(Notify::new());
    let probe_limit_ms = Arc::new(AtomicU64::new(PROBE_LIMIT.as_millis() as u64));
    let dropped = Arc::new(AtomicU64::new(0));
    let app = router_with(
        tx,
        closing.clone(),
        stop_flag.clone(),
        probe_limit_ms.clone(),
        dropped.clone(),
    );
    let (shutdown, stop) = oneshot::channel::<()>();
    let stop = async move {
        if stop.await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let (force, forced) = oneshot::channel::<()>();
    let (done_tx, done) = oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("coding-lab-hooks".into())
        .spawn(move || {
            rt.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(l) => l,
                    Err(e) => {
                        crate::logging::log(&format!("hooks：{e}"));
                        return;
                    }
                };
                let serve = axum::serve(listener, app).with_graceful_shutdown(stop);
                tokio::select! {
                    r = serve => {
                        if let Err(e) = r {
                            crate::logging::log(&format!("hooks：{e}"));
                        }
                    }
                    // 丟掉 HookServer 不會關閉（sender 被丟掉時是 Err，這條不成立）
                    Ok(()) = forced => {}
                }
            });
            let _ = done_tx.send(());
        })?;
    Ok(HookServer {
        addr: local,
        closing,
        stop: stop_flag,
        probe_limit_ms,
        dropped,
        shutdown: Some(shutdown),
        force: Some(force),
        done,
    })
}

fn router_with(
    tx: mpsc::Sender<Input>,
    closing: Arc<AtomicBool>,
    stop: Arc<Notify>,
    probe_limit_ms: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
) -> Router {
    let state = Arc::new(HookState {
        tx,
        closing,
        stop,
        dropped,
        probe_limit_ms,
    });
    Router::new()
        .route(HOOK_PATH, post(handle))
        .route(STOP_PATH, post(handle_stop))
        .with_state(state)
}

/// 瀏覽器送的請求：帶 `Origin`（Claude Code 的 http hook 與 ensure、stop 都不帶）。
fn from_browser(headers: &HeaderMap) -> bool {
    headers.contains_key(axum::http::header::ORIGIN)
}

/// `Content-Type: application/json`（可以帶參數、不分大小寫）。
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

/// `POST /stop`：記下要結束，立刻回 200；實際結束由主迴圈做（它要先放埠、停 tail、flush）。
async fn handle_stop(State(state): State<Arc<HookState>>, headers: HeaderMap) -> StatusCode {
    if from_browser(&headers)
        || headers
            .get(STOP_HEADER)
            .is_none_or(|v| v.as_bytes() != b"1")
    {
        crate::logging::log("拒收 stop：帶 Origin 或沒有 X-Coding-Lab-Stop");
        return StatusCode::FORBIDDEN;
    }
    crate::logging::log("收到 stop");
    state.stop.notify_one();
    StatusCode::OK
}

async fn handle(
    State(state): State<Arc<HookState>>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> StatusCode {
    let ts_seen = crate::now_ms();
    if from_browser(&headers) {
        crate::logging::log("拒收 hook：帶 Origin（瀏覽器送的）");
        return StatusCode::FORBIDDEN;
    }
    if !is_json(&headers) {
        crate::logging::log("拒收 hook：Content-Type 不是 application/json");
        return StatusCode::UNSUPPORTED_MEDIA_TYPE;
    }
    if state.closing.load(Ordering::SeqCst)
        && headers
            .get(VIA_HEADER)
            .is_some_and(|v| v.as_bytes() == b"ensure")
    {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    // body 自己讀、自己限大小（L-2）：axum 的 Bytes 預設 2 MB、超過回 413，Claude Code 會顯示 hook 錯誤。
    // 超過上限的一律 200 加 log（不變式 4）；宣告的長度就超過時不讀
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > BODY_MAX) {
        crate::logging::log(&format!("hooks：body 超過 {} MB，略過", BODY_MAX >> 20));
        return StatusCode::OK;
    }
    let body = match axum::body::to_bytes(body, BODY_MAX).await {
        Ok(b) => b,
        Err(e) => {
            crate::logging::log(&format!(
                "hooks：讀不到 body（或超過 {} MB）：{e}",
                BODY_MAX >> 20
            ));
            return StatusCode::OK;
        }
    };
    match HookInput::from_json(&body) {
        Ok(mut hook) => {
            // Edit／Write 的目標檔要在回應之前讀（Claude Code 等 hook 回應才執行工具，#626）。
            // 讀檔是同步 I/O，放到 blocking 執行緒，不佔住處理其他 hook 的 worker
            let limit = Duration::from_millis(state.probe_limit_ms.load(Ordering::SeqCst));
            // 時限 0 表示不讀檔（和逾時一樣：不帶探測結果、照常處理）
            if let Some(job) = crate::derive::probe_job(&hook).filter(|_| !limit.is_zero()) {
                match tokio::time::timeout(limit, tokio::task::spawn_blocking(move || job.run()))
                    .await
                {
                    Ok(Ok(probe)) => hook.set_probe(probe),
                    Ok(Err(e)) => crate::logging::log(&format!("hooks：讀檔探針失敗：{e}")),
                    Err(_) => crate::logging::log(&format!(
                        "hooks：讀檔探測超過 {} ms，不帶探測結果",
                        limit.as_millis()
                    )),
                }
            }
            if state.tx.try_send(Input::Hook { hook, ts_seen }).is_err() {
                let n = state.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                crate::logging::log(&format!("hooks：merge 忙碌，丟棄 hook（累計 {n}）"));
            }
        }
        Err(e) => crate::logging::log(&format!("hooks：忽略無法解析的 hook：{e}")),
    }
    StatusCode::OK
}
