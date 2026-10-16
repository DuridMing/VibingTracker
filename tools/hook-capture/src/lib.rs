//! 階段 0 的資料收集工具（#624、#625）。
//!
//! - `serve`：http hook 的接收端，同時在 `SessionStart` 後 tail transcript，記錄每一行的到達時間。
//! - `cmd`：command hook 版，從 stdin 讀 payload；用來和 http 版比對 payload 與延遲。
//! - `settings`：產生掛全部事件的 `.claude/settings.local.json`。
//!
//! 輸出只寫在 `--out`（預設 `~/.cache/coding-lab-capture`），不在 repo 裡；
//! 進 `samples/` 前要另外遮蔽。

pub mod http;
pub mod probe;
pub mod settings;
pub mod splitter;
pub mod store;
pub mod tail;

/// 現在的 epoch 毫秒。
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
