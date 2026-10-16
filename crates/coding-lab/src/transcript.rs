//! transcript tail（#631，PLAN「collector 設計」的 transcript 模組）。
//!
//! 兩層：[`TailReader`] 是同步、不看時鐘的讀取器——只讀新位移、以 `\n` 切、尾端半行留到下次、
//! 檔案變短或被替換就從頭；[`spawn_tail`] 是 tokio task，用 notify 事件加固定間隔輪詢驅動它
//! （notify 會漏事件或合併事件，輪詢是保底），把每一行轉成 merge 的 [`Input::Line`]。
//! 同一個 uuid 的去重在 merge 做。

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::merge::Input;

/// 從哪裡開始讀。`startup` 的 session 從檔頭；resume／compact 或 collector 中途才看到的 session
/// 從檔尾（只數行號），避免 collector 重啟後把整份 transcript 再寫一次進 store。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    Beginning,
    End,
}

#[derive(Debug)]
pub struct TailLine {
    pub no: u64,
    pub value: Value,
}

#[derive(Debug)]
pub struct TailError {
    pub no: u64,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct Poll {
    pub lines: Vec<TailLine>,
    pub errors: Vec<TailError>,
    /// 檔案被替換或截短，這次從頭讀起（行號重新從 1 算）。
    pub restarted: bool,
}

/// 檔案身分：Unix 用 (dev, inode) 偵測 rename 替換；其他平台只靠「檔案變短」判斷。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId(u64, u64);

fn file_id(meta: &fs::Metadata) -> Option<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileId(meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

pub struct TailReader {
    path: PathBuf,
    offset: u64,
    line_no: u64,
    pending: Vec<u8>,
    id: Option<FileId>,
}

impl TailReader {
    /// 檔案還不存在也可以開；之後出現時從頭讀。
    pub fn open(path: &Path, start: Start) -> io::Result<TailReader> {
        let mut r = TailReader {
            path: path.to_path_buf(),
            offset: 0,
            line_no: 0,
            pending: Vec::new(),
            id: None,
        };
        if let Ok(meta) = fs::metadata(path) {
            r.id = file_id(&meta);
            if start == Start::End {
                let mut bytes = Vec::new();
                File::open(path)?.read_to_end(&mut bytes)?;
                let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                r.line_no = bytes[..complete].iter().filter(|b| **b == b'\n').count() as u64;
                r.offset = complete as u64;
            }
        }
        Ok(r)
    }

    /// 讀新寫入的部分，回傳這次湊成的完整行。
    pub fn poll(&mut self) -> io::Result<Poll> {
        let mut out = Poll::default();
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        let id = file_id(&meta);
        let replaced = matches!((self.id, id), (Some(a), Some(b)) if a != b);
        if replaced || meta.len() < self.offset {
            self.offset = 0;
            self.line_no = 0;
            self.pending.clear();
            out.restarted = true;
        }
        self.id = id;
        if meta.len() == self.offset {
            return Ok(out);
        }

        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        f.take(meta.len() - self.offset).read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        self.pending.extend_from_slice(&buf);

        let mut start = 0;
        for i in 0..self.pending.len() {
            if self.pending[i] != b'\n' {
                continue;
            }
            self.line_no += 1;
            let raw = &self.pending[start..i];
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            if !raw.is_empty() {
                match serde_json::from_slice::<Value>(raw) {
                    Ok(value) => out.lines.push(TailLine {
                        no: self.line_no,
                        value,
                    }),
                    Err(e) => out.errors.push(TailError {
                        no: self.line_no,
                        error: e.to_string(),
                    }),
                }
            }
            start = i + 1;
        }
        self.pending.drain(..start);
        Ok(out)
    }
}

pub struct TailOptions {
    /// 輪詢間隔（正式 500 ms）。
    pub poll_every: Duration,
    /// 關掉時只靠輪詢（測試用，也是 notify 建立失敗時的退路）。
    pub use_notify: bool,
}

impl Default for TailOptions {
    fn default() -> Self {
        TailOptions {
            poll_every: Duration::from_millis(500),
            use_notify: true,
        }
    }
}

pub struct TailHandle {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TailHandle {
    /// 結束前會再讀一次，停止之前寫進檔案的行都會送出。
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
    }
}

/// 為一個 transcript 檔開一個 tail task。`file` 是 `"main"` 或 `"agent-<agent_id>"`。
pub fn spawn_tail(
    path: PathBuf,
    session: String,
    file: String,
    start: Start,
    tx: mpsc::Sender<Input>,
    opts: TailOptions,
) -> TailHandle {
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(path, session, file, start, tx, opts, stop_rx));
    TailHandle {
        stop: Some(stop_tx),
        task,
    }
}

async fn run(
    path: PathBuf,
    session: String,
    file: String,
    start: Start,
    tx: mpsc::Sender<Input>,
    opts: TailOptions,
    mut stop: oneshot::Receiver<()>,
) {
    let mut reader = match TailReader::open(&path, start) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("coding-lab transcript：開不了 {}：{e}", path.display());
            return;
        }
    };
    let wake = Arc::new(Notify::new());
    // watcher 要活到 task 結束；建立失敗（例如 inotify 上限）就只靠輪詢
    let _watcher = if opts.use_notify {
        watch(&path, wake.clone())
    } else {
        None
    };
    let mut out = Sender {
        tx,
        session,
        file,
        degraded: false,
    };

    loop {
        out.send(reader.poll()).await;
        tokio::select! {
            _ = &mut stop => break,
            _ = wake.notified() => {}
            _ = tokio::time::sleep(opts.poll_every) => {}
        }
    }
    out.send(reader.poll()).await;
}

/// 監看 transcript 所在的資料夾（檔案可能還不存在，或被 rename 替換）。
fn watch(path: &Path, wake: Arc<Notify>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let name = path.file_name()?.to_os_string();
    let dir = path.parent()?.to_path_buf();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev
                .paths
                .iter()
                .any(|p| p.file_name() == Some(name.as_os_str()))
        {
            wake.notify_one();
        }
    })
    .ok()?;
    w.watch(&dir, notify::RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

struct Sender {
    tx: mpsc::Sender<Input>,
    session: String,
    file: String,
    degraded: bool,
}

impl Sender {
    /// 依行號把行與解析錯誤送給 merge；第一個壞行送 Degraded，之後第一個好行送 Recovered。
    async fn send(&mut self, poll: io::Result<Poll>) {
        let poll = match poll {
            Ok(p) => p,
            Err(e) => {
                eprintln!("coding-lab transcript：讀取失敗 {}：{e}", self.file);
                return;
            }
        };
        let mut items: Vec<(u64, Result<Value, String>)> = poll
            .lines
            .into_iter()
            .map(|l| (l.no, Ok(l.value)))
            .chain(poll.errors.into_iter().map(|e| (e.no, Err(e.error))))
            .collect();
        items.sort_by_key(|(no, _)| *no);
        for (no, item) in items {
            let ts_seen = crate::now_ms();
            let (session, file) = (self.session.clone(), self.file.clone());
            let input = match item {
                Err(error) if !self.degraded => {
                    self.degraded = true;
                    Input::Degraded {
                        session,
                        file,
                        error: format!("第 {no} 行：{error}"),
                        ts_seen,
                    }
                }
                Err(_) => continue,
                Ok(value) => {
                    if self.degraded {
                        self.degraded = false;
                        let recovered = Input::Recovered {
                            session: session.clone(),
                            file: file.clone(),
                            ts_seen,
                        };
                        if self.tx.send(recovered).await.is_err() {
                            return;
                        }
                    }
                    Input::Line {
                        session,
                        file,
                        no,
                        value,
                        ts_seen,
                    }
                }
            };
            if self.tx.send(input).await.is_err() {
                return; // merge 已經結束
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailAction {
    Start {
        session: String,
        file: String,
        path: PathBuf,
        start: Start,
    },
    Stop {
        session: String,
        file: String,
    },
    StopSession {
        session: String,
    },
}

/// 看 hook 決定要開／關哪些 tail（純邏輯；執行由組裝 collector 的地方負責）。
///
/// - 主 transcript：`SessionStart(startup)` 從檔頭；resume／compact 或沒收到 SessionStart 就先看到
///   其他事件時從檔尾。實測 compact 與 resume 沿用同一個檔，所以已在 tail 的 session 不重開。
/// - subagent：`SubagentStart` 開 `<transcript 去掉 .jsonl>/subagents/agent-<id>.jsonl`，`SubagentStop` 關。
/// - `SessionEnd` 關掉該 session 的全部 tail；之後若 resume 會再開。
#[derive(Default)]
pub struct TailPlanner {
    main: std::collections::HashMap<String, PathBuf>,
    agents: std::collections::HashMap<String, std::collections::HashSet<String>>,
}

impl TailPlanner {
    pub fn on_hook(&mut self, h: &crate::hook::HookInput) -> Vec<TailAction> {
        let sid = h.session_id().to_string();
        let event = h.event_name();
        let mut out = Vec::new();

        if event == "SessionEnd" {
            let had_main = self.main.remove(&sid).is_some();
            let had_agents = self.agents.remove(&sid).is_some_and(|a| !a.is_empty());
            if had_main || had_agents {
                out.push(TailAction::StopSession { session: sid });
            }
            return out;
        }

        let transcript = h
            .str_field("transcript_path")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        if let Some(path) = &transcript {
            match self.main.get(&sid) {
                Some(known) if known == path => {}
                known => {
                    if known.is_some() {
                        out.push(TailAction::Stop {
                            session: sid.clone(),
                            file: "main".into(),
                        });
                    }
                    let fresh = event == "SessionStart" && h.str_field("source") == Some("startup");
                    let start = if fresh { Start::Beginning } else { Start::End };
                    out.push(TailAction::Start {
                        session: sid.clone(),
                        file: "main".into(),
                        path: path.clone(),
                        start,
                    });
                    self.main.insert(sid.clone(), path.clone());
                }
            }
        }

        let agent = h.str_field("agent_id").filter(|a| !a.is_empty());
        match (event, agent, &transcript) {
            ("SubagentStart", Some(id), Some(path)) => {
                let file = format!("agent-{id}");
                if self
                    .agents
                    .entry(sid.clone())
                    .or_default()
                    .insert(file.clone())
                {
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_os_string())
                        .unwrap_or_default();
                    let sub = path
                        .with_file_name(stem)
                        .join("subagents")
                        .join(format!("{file}.jsonl"));
                    out.push(TailAction::Start {
                        session: sid,
                        file,
                        path: sub,
                        start: Start::Beginning,
                    });
                }
            }
            ("SubagentStop", Some(id), _) => {
                let file = format!("agent-{id}");
                if self.agents.get_mut(&sid).is_some_and(|a| a.remove(&file)) {
                    out.push(TailAction::Stop { session: sid, file });
                }
            }
            _ => {}
        }
        out
    }
}
