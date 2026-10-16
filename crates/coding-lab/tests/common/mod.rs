//! 回放測試共用的樣本載入（#688）：samples/ 不進 git，沒有時各測試自己略過。

#![allow(dead_code)] // 每個測試檔只用到其中幾個

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `samples/sessions/` 底下的每個樣本目錄（排序後）；沒有 samples 時是空的。
pub fn sample_sessions() -> Vec<PathBuf> {
    sessions_in(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/sessions"))
}

/// `root` 底下的每個樣本目錄（排序後）。
pub fn sessions_in(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// JSONL 的每一行（讀不到檔或解析不了的行略過）。
pub fn read_jsonl(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// hook-capture 的 `*-arrivals.jsonl`：行號 → 最早的到達時間（resume 重開 tail 時同一行會再記一次）。
pub fn arrivals(p: &Path) -> HashMap<u64, u64> {
    let mut out: HashMap<u64, u64> = HashMap::new();
    for a in read_jsonl(p) {
        if let (Some(no), Some(ts)) = (a["line_no"].as_u64(), a["ts_seen_ms"].as_u64()) {
            let t = out.entry(no).or_insert(ts);
            *t = (*t).min(ts);
        }
    }
    out
}
