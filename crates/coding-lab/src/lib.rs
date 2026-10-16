//! coding-lab collector。資料契約見 docs/plan/contracts.md。

pub mod api;
pub mod config;
pub mod derive;
pub mod doing;
pub mod event;
pub mod hook;
pub mod hooks;
pub mod live;
pub mod logging;
pub mod merge;
pub mod prices;
pub mod rules;
pub mod serve;
pub mod status;
pub mod store;
pub mod transcript;
pub mod ws;

/// 現在的 epoch 毫秒（Event 的 `ts_seen`）。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
