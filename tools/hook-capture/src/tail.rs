//! tail transcript，記錄每一行的到達時間；結束時把 transcript 複製一份。
//!
//! 實測 SessionStart 不會送 http hook，所以 tail 在第一次看到某個 session 的 http 事件時才開始；
//! 那之前已經寫進檔案的行標記 `backfill: true`，它們的 `ts_seen_ms` 不是真正的到達時間。

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::splitter::LineSplitter;
use crate::store::append_json;

const POLL: Duration = Duration::from_millis(25);
/// 沒有 SessionEnd（例如程序被砍）時，閒置這麼久就收尾。
const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);

pub struct Tail {
    pub transcript: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Tail {
    /// `snapshot_dir`：結束時要把 transcript（含 subagent 子目錄）複製到哪裡；subagent 的 tail 不需要。
    pub fn start(transcript: PathBuf, arrivals: PathBuf, snapshot_dir: Option<PathBuf>) -> Tail {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let path = transcript.clone();
        let handle = thread::spawn(move || run(&path, &arrivals, snapshot_dir.as_deref(), &flag));
        Tail {
            transcript,
            stop,
            handle: Some(handle),
        }
    }

    /// 讀完最後的新行（並複製 transcript）後才回傳。
    pub fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(path: &Path, arrivals: &Path, snapshot_dir: Option<&Path>, stop: &AtomicBool) {
    let mut offset = 0u64;
    let mut line_no = 0u64;
    let mut splitter = LineSplitter::default();
    let mut last_activity = Instant::now();
    // 第一次讀到的內容是 tail 開始前就存在的
    let mut backfill = true;

    loop {
        let stopping = stop.load(Ordering::SeqCst);
        match read_new(path, &mut offset) {
            Ok(Some(bytes)) if !bytes.is_empty() => {
                let ts = crate::now_ms();
                for line in splitter.push(&bytes) {
                    line_no += 1;
                    log(arrivals, describe(&line, line_no, ts, backfill));
                }
                last_activity = Instant::now();
            }
            Ok(None) => {
                // 檔案變短：被替換或截斷，從頭來
                log(
                    arrivals,
                    json!({ "ts_seen_ms": crate::now_ms(), "note": "truncated" }),
                );
                offset = 0;
                line_no = 0;
                splitter = LineSplitter::default();
            }
            _ => {}
        }
        backfill = false;
        if stopping || last_activity.elapsed() > IDLE_LIMIT {
            break;
        }
        thread::sleep(POLL);
    }
    if let Some(dir) = snapshot_dir {
        snapshot(path, dir);
    }
}

/// 有新資料回 `Some(bytes)`；檔案比上次讀到的位置短回 `None`；還不存在回空。
fn read_new(path: &Path, offset: &mut u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(Some(Vec::new())),
    };
    let len = f.metadata()?.len();
    if len < *offset {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(*offset))?;
    let mut buf = Vec::new();
    f.take(len - *offset).read_to_end(&mut buf)?;
    *offset += buf.len() as u64;
    Ok(Some(buf))
}

/// 只記排序分析要用的欄位，不存內容本身（內容在 SessionEnd 複製的 transcript 裡）。
fn describe(line: &[u8], line_no: u64, ts: u128, backfill: bool) -> Value {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return json!({
            "ts_seen_ms": ts, "line_no": line_no,
        "backfill": backfill,
            "bytes": line.len(), "parse_error": true,
        });
    };
    let content = v["message"]["content"].as_array();
    let blocks: Vec<&str> = content
        .map(|c| c.iter().filter_map(|b| b["type"].as_str()).collect())
        .unwrap_or_default();
    let tool_ids: Vec<&str> = content
        .map(|c| {
            c.iter()
                .filter_map(|b| b["id"].as_str().or(b["tool_use_id"].as_str()))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "ts_seen_ms": ts,
        "line_no": line_no,
        "backfill": backfill,
        "bytes": line.len(),
        "type": v["type"],
        "uuid": v["uuid"],
        "parent_uuid": v["parentUuid"],
        "timestamp": v["timestamp"],
        "session_id": v["sessionId"],
        "is_sidechain": v["isSidechain"],
        "model": v["message"]["model"],
        "blocks": blocks,
        "tool_ids": tool_ids,
    })
}

/// 把 transcript 與同名的 session 子目錄（實測 subagent 在 `<stem>/subagents/agent-<id>.jsonl`）複製進樣本目錄。
pub fn snapshot(transcript: &Path, dir: &Path) {
    let _ = fs::create_dir_all(dir);
    let _ = fs::copy(transcript, dir.join("transcript.jsonl"));
    if let (Some(parent), Some(stem)) = (transcript.parent(), transcript.file_stem()) {
        let side = parent.join(stem);
        if side.is_dir() {
            let _ = copy_dir(&side, &dir.join("transcript-dir"));
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn log(path: &Path, row: Value) {
    if let Err(e) = append_json(path, &row) {
        eprintln!("hook-capture: write {}: {e}", path.display());
    }
}
