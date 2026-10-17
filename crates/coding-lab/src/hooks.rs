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

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::hook::HookInput;
use crate::merge::Input;

pub const HOOK_PATH: &str = "/hook";
/// `coding-lab stop` 打的路徑（#700）
pub const STOP_PATH: &str = "/stop";
/// `ensure` 轉送時帶的標頭（值是 `ensure`）
pub const VIA_HEADER: &str = "x-coding-lab-via";
/// 放掉埠時最多等進行中的請求多久
const CLOSE_GRACE: Duration = Duration::from_secs(2);

struct HookState {
    tx: mpsc::Sender<Input>,
    closing: Arc<AtomicBool>,
    /// `POST /stop` 來過（permit 會留著，主迴圈晚一點等也等得到）
    stop: Arc<Notify>,
    /// merge 的 channel 滿了而丟掉的 hook 數（之後給 /healthz）
    dropped: AtomicU64,
}

/// 執行中的 hook 入口。
pub struct HookServer {
    pub addr: SocketAddr,
    closing: Arc<AtomicBool>,
    stop: Arc<Notify>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl HookServer {
    /// 決定要結束（或取消結束）：結束中 ensure 轉送的 hook 回 503，其他 hook 照收。
    pub fn set_closing(&self, closing: bool) {
        self.closing.store(closing, Ordering::SeqCst);
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
        if tokio::time::timeout(CLOSE_GRACE, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

/// 開始收 hook；回傳實際綁定的位址（測試用 port 0）。非 loopback 位址一律拒絕。
pub async fn serve_hooks(addr: SocketAddr, tx: mpsc::Sender<Input>) -> io::Result<HookServer> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("hook 入口只能綁 127.0.0.1，收到 {addr}"),
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let closing = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::new(Notify::new());
    let app = router_with(tx, closing.clone(), stop_flag.clone());
    let (shutdown, stop) = oneshot::channel::<()>();
    let stop = async move {
        if stop.await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(stop)
            .await
        {
            crate::logging::log(&format!("hooks：{e}"));
        }
    });
    Ok(HookServer {
        addr: local,
        closing,
        stop: stop_flag,
        shutdown: Some(shutdown),
        task,
    })
}

pub fn router(tx: mpsc::Sender<Input>) -> Router {
    router_with(
        tx,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Notify::new()),
    )
}

fn router_with(tx: mpsc::Sender<Input>, closing: Arc<AtomicBool>, stop: Arc<Notify>) -> Router {
    let state = Arc::new(HookState {
        tx,
        closing,
        stop,
        dropped: AtomicU64::new(0),
    });
    Router::new()
        .route(HOOK_PATH, post(handle))
        .route(STOP_PATH, post(handle_stop))
        .with_state(state)
}

/// `POST /stop`：記下要結束，立刻回 200；實際結束由主迴圈做（它要先放埠、停 tail、flush）。
async fn handle_stop(State(state): State<Arc<HookState>>) -> StatusCode {
    crate::logging::log("收到 stop");
    state.stop.notify_one();
    StatusCode::OK
}

async fn handle(
    State(state): State<Arc<HookState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let ts_seen = crate::now_ms();
    if state.closing.load(Ordering::SeqCst)
        && headers
            .get(VIA_HEADER)
            .is_some_and(|v| v.as_bytes() == b"ensure")
    {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    match HookInput::from_json(&body) {
        Ok(mut hook) => {
            // Edit／Write 的目標檔要在回應之前讀（Claude Code 等 hook 回應才執行工具，#626）。
            // 讀檔是同步 I/O，放到 blocking 執行緒，不佔住處理其他 hook 的 worker
            if crate::derive::needs_probe(&hook) {
                hook = match tokio::task::spawn_blocking(move || {
                    crate::derive::attach_probe(&mut hook);
                    hook
                })
                .await
                {
                    Ok(h) => h,
                    Err(e) => {
                        crate::logging::log(&format!("hooks：讀檔探針失敗：{e}"));
                        return StatusCode::OK;
                    }
                };
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
