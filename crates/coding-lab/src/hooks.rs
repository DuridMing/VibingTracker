//! hooks 入口（#630，PLAN「collector 設計」的 hooks 模組）。
//!
//! 只綁 127.0.0.1，`POST /hook` 收 Claude Code 的 http hook（以及 `ensure` 轉送的 SessionStart）。
//! 回應一律是空的 200：不帶任何內容才不會影響 Claude Code（零 token、不攔截）；任何錯誤都吞掉寫 log。
//! hook 用 `try_send` 交給 merge **之後**才回應，merge 忙時直接丟棄並計數，絕不讓 Claude Code 等。

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::hook::HookInput;
use crate::merge::Input;

pub const HOOK_PATH: &str = "/hook";

struct HookState {
    tx: mpsc::Sender<Input>,
    /// merge 的 channel 滿了而丟掉的 hook 數（之後給 /healthz）
    dropped: AtomicU64,
}

/// 開始收 hook；回傳實際綁定的位址（測試用 port 0）。非 loopback 位址一律拒絕。
pub async fn serve_hooks(
    addr: SocketAddr,
    tx: mpsc::Sender<Input>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("hook 入口只能綁 127.0.0.1，收到 {addr}"),
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let app = router(tx);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("coding-lab hooks：{e}");
        }
    });
    Ok((local, task))
}

pub fn router(tx: mpsc::Sender<Input>) -> Router {
    let state = Arc::new(HookState {
        tx,
        dropped: AtomicU64::new(0),
    });
    Router::new()
        .route(HOOK_PATH, post(handle))
        .with_state(state)
}

async fn handle(State(state): State<Arc<HookState>>, body: Bytes) -> StatusCode {
    let ts_seen = crate::now_ms();
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
                        eprintln!("coding-lab hooks：讀檔探針失敗：{e}");
                        return StatusCode::OK;
                    }
                };
            }
            if state.tx.try_send(Input::Hook { hook, ts_seen }).is_err() {
                let n = state.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                eprintln!("coding-lab hooks：merge 忙碌，丟棄 hook（累計 {n}）");
            }
        }
        Err(e) => eprintln!("coding-lab hooks：忽略無法解析的 hook：{e}"),
    }
    StatusCode::OK
}
