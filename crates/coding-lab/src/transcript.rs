//! transcript tail（#631，PLAN「collector 設計」的 transcript 模組）。
//!
//! 兩層：[`TailReader`] 是同步、不看時鐘的讀取器——只讀新位移、以 `\n` 切、尾端半行留到下次、
//! 檔案變短或被替換就從頭；[`spawn_tail`] 是 tokio task，用 notify 事件加固定間隔輪詢驅動它
//! （notify 會漏事件或合併事件，輪詢是保底），把每一行轉成 merge 的 [`Input::Line`]。
//! 同一個 uuid 的去重在 merge 做。

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::merge::{AgentMetaRead, Input};

/// 超過這個長度的行（Read 大檔、Write 大檔）只留必要欄位：字串截短到 [`BIG_LINE_STRING_CAP`]（#684 R21）
pub const BIG_LINE: usize = 256 * 1024;
pub const BIG_LINE_STRING_CAP: usize = 64 * 1024;
/// 被截短的字串後面接這個標記，不假裝是完整內容
pub const CUT_MARK: &str = "…（原文過長，已截斷）";

/// 從哪裡開始讀。`startup` 的 session 從檔頭；resume／compact 或 collector 中途才看到的 session
/// 從檔尾（只數行號），避免 collector 重啟後把整份 transcript 再寫一次進 store。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    Beginning,
    End,
    /// 接著第 n 行之後讀（collector 重啟後依 store 記的行號，#684）；檔案不到 n 行就從頭讀
    After(u64),
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
    /// `pending` 裡已經找過換行的長度（大行分很多次寫入時不從頭再找，#684 R28）
    searched: usize,
    id: Option<FileId>,
    /// 累計掃描過的 bytes（找換行用；測試確認大行分段寫入時不重複掃描）
    scanned: u64,
}

impl TailReader {
    /// 檔案還不存在也可以開；之後出現時從頭讀。
    pub fn open(path: &Path, start: Start) -> io::Result<TailReader> {
        let mut r = TailReader {
            path: path.to_path_buf(),
            offset: 0,
            line_no: 0,
            pending: Vec::new(),
            searched: 0,
            id: None,
            scanned: 0,
        };
        if let Ok(meta) = fs::metadata(path) {
            r.id = file_id(&meta);
            let stop_at = match start {
                Start::Beginning => None,
                Start::End => Some(u64::MAX),
                Start::After(n) => Some(n),
            };
            if let Some(stop_at) = stop_at.filter(|n| *n > 0) {
                // 分段數換行：transcript 可能上百 MB（Read 大檔整個進一行），不整個讀進記憶體
                let mut f = File::open(path)?;
                let mut buf = vec![0u8; 64 * 1024];
                let mut read = 0u64;
                'scan: loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    for (i, b) in buf[..n].iter().enumerate() {
                        if *b == b'\n' {
                            r.line_no += 1;
                            r.offset = read + i as u64 + 1;
                            if r.line_no == stop_at {
                                break 'scan;
                            }
                        }
                    }
                    read += n as u64;
                }
                // 記錄的行號比檔案還長：檔案被替換或截短，從頭讀
                if matches!(start, Start::After(n) if r.line_no < n) {
                    r.offset = 0;
                    r.line_no = 0;
                }
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
            self.searched = 0;
            out.restarted = true;
        }
        self.id = id;
        if meta.len() == self.offset {
            return Ok(out);
        }

        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(self.offset))?;
        // 直接接在半行後面讀，大行不多一份拷貝（E-S9）
        let want = meta.len() - self.offset;
        self.pending.reserve(usize::try_from(want).unwrap_or(0));
        let got = f.take(want).read_to_end(&mut self.pending)?;
        self.offset += got as u64;

        let mut start = 0;
        self.scanned += (self.pending.len() - self.searched) as u64;
        for i in self.searched..self.pending.len() {
            if self.pending[i] != b'\n' {
                continue;
            }
            self.line_no += 1;
            let raw = &self.pending[start..i];
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            if !raw.is_empty() {
                match parse_line(raw) {
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
        // 剩下的是還沒寫完的半行，裡面沒有換行
        self.searched = self.pending.len();
        Ok(out)
    }

    pub fn scanned_bytes(&self) -> u64 {
        self.scanned
    }

    /// 檔案裡還沒讀的 bytes（檔案不見或變短時是 0）。
    pub fn backlog(&self) -> u64 {
        fs::metadata(&self.path).map_or(0, |m| m.len().saturating_sub(self.offset))
    }
}

/// 讀一次。新增超過 [`BIG_LINE`] 時（Read 大檔整個進一行：實測 release 2 MB 約 20 ms、100 MB 約 1 秒）
/// 在 blocking 執行緒讀與解析，不佔住 runtime 的 worker（hook 要 100 ms 內回，E-S9）。
/// blocking 執行緒沒回來（runtime 正在關）時回 None，tail 就此結束。
async fn poll_reader(mut reader: TailReader) -> Option<(TailReader, io::Result<Poll>)> {
    if reader.backlog() <= BIG_LINE as u64 {
        let polled = reader.poll();
        return Some((reader, polled));
    }
    tokio::task::spawn_blocking(move || {
        let polled = reader.poll();
        (reader, polled)
    })
    .await
    .ok()
}

/// 一般的行整個解析；大行用 [`Capped`] 邊解析邊截短長字串，記憶體不隨行長倍數成長（#684 R21）。
fn parse_line(raw: &[u8]) -> Result<Value, serde_json::Error> {
    if raw.len() <= BIG_LINE {
        return serde_json::from_slice(raw);
    }
    let mut de = serde_json::Deserializer::from_slice(raw);
    let v = serde::de::DeserializeSeed::deserialize(Capped(BIG_LINE_STRING_CAP), &mut de)?;
    de.end()?;
    Ok(v)
}

/// 解析成 `Value`，但超過上限的字串只留前面（UTF-8 邊界）並接上 [`CUT_MARK`]。
#[derive(Clone, Copy)]
struct Capped(usize);

impl<'de> serde::de::DeserializeSeed<'de> for Capped {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for Capped {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        if v.len() <= self.0 {
            return Ok(Value::String(v.to_string()));
        }
        let mut end = self.0;
        while !v.is_char_boundary(end) {
            end -= 1;
        }
        Ok(Value::String(format!("{}{CUT_MARK}", &v[..end])))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(self)? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = map.next_key::<String>()? {
            let v = map.next_value_seed(self)?;
            out.insert(k, v);
        }
        Ok(Value::Object(out))
    }
}

pub struct TailOptions {
    /// 輪詢間隔（正式 500 ms）。
    pub poll_every: Duration,
    /// 關掉時只靠輪詢（測試用，也是 notify 建立失敗時的退路）。
    pub use_notify: bool,
    /// 這個 tail 的編號（serve 遞增發號）；Degraded／Recovered 帶著它，merge 用來忽略已被取代的 tail（#684 R20）
    pub tail: u64,
    /// 共用的 notify watcher（E-S6）；None 時這個 tail 自己開一個
    pub watcher: Option<Arc<SharedWatcher>>,
}

impl Default for TailOptions {
    fn default() -> Self {
        TailOptions {
            poll_every: Duration::from_millis(500),
            use_notify: true,
            tail: 0,
            watcher: None,
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

/// 停掉所有 tail，期間持續把 `rx` 收到的輸入交給 `on_input`。
pub async fn stop_all(
    tails: impl IntoIterator<Item = TailHandle>,
    rx: &mut mpsc::Receiver<Input>,
    mut on_input: impl FnMut(Input),
) {
    // tail 結束前最後一次讀檔用 `send().await`，佇列滿時要靠這裡繼續收才停得下來
    let mut stopping = tokio::task::JoinSet::new();
    for t in tails {
        stopping.spawn(t.stop());
    }
    loop {
        tokio::select! {
            done = stopping.join_next() => {
                if done.is_none() {
                    break;
                }
            }
            Some(input) = rx.recv() => on_input(input),
        }
    }
    while let Ok(input) = rx.try_recv() {
        on_input(input);
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
    // 從檔尾開始要掃過整個檔案數行號，放到 blocking 執行緒，不卡住 runtime
    let opened = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || TailReader::open(&path, start)).await
    };
    let mut reader = match opened {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            crate::logging::log(&format!("transcript：開不了 {}：{e}", path.display()));
            return;
        }
        Err(e) => {
            crate::logging::log(&format!("transcript：開 {} 失敗：{e}", path.display()));
            return;
        }
    };
    let wake = Arc::new(Notify::new());
    // 登記要活到 task 結束（結束時 unwatch）；失敗（例如 inotify 上限）就只靠輪詢
    let shared = match (opts.use_notify, opts.watcher) {
        (false, _) => None,
        (true, Some(w)) => Some(w),
        (true, None) => Some(Arc::new(SharedWatcher::default())),
    };
    let _watch = shared.and_then(|w| w.register(&path, wake.clone()));
    let mut out = Sender {
        tx,
        session,
        file,
        tail: opts.tail,
        degraded: false,
    };

    loop {
        let Some((r, polled)) = poll_reader(reader).await else {
            return;
        };
        reader = r;
        out.send(polled).await;
        tokio::select! {
            _ = &mut stop => break,
            _ = wake.notified() => {}
            _ = tokio::time::sleep(opts.poll_every) => {}
        }
    }
    if let Some((_, polled)) = poll_reader(reader).await {
        out.send(polled).await;
    }
}

/// 整個 collector 共用的 notify watcher（E-S6）。每個 tail 各開一個時，每個就是一條 OS 執行緒加一個
/// inotify instance（Linux 預設上限 128，用完只剩輪詢），而且主 transcript 都在同一個專案資料夾：
/// 任何一個檔寫入，所有 tail 一起醒。這裡只開一個，資料夾依引用計數 watch／unwatch，事件依檔名分給等它的 tail。
#[derive(Default)]
pub struct SharedWatcher {
    /// 檔名 → 等它的 tail（登記編號、資料夾、叫醒誰）。notify 的 callback 只拿這把鎖
    routes: Arc<Mutex<HashMap<OsString, Vec<Route>>>>,
    /// watcher 與每個資料夾的引用數。呼叫 watch／unwatch 時不能拿著 `routes`：inotify 的 watch 要等事件執行緒回應，
    /// 事件執行緒若正在 callback 裡等 `routes` 就會互相卡住
    dirs: Mutex<WatchedDirs>,
    next: AtomicU64,
}

struct Route {
    id: u64,
    dir: PathBuf,
    wake: Arc<Notify>,
}

#[derive(Default)]
struct WatchedDirs {
    watcher: Option<notify::RecommendedWatcher>,
    count: HashMap<PathBuf, usize>,
}

/// [`SharedWatcher::register`] 的登記；drop 時取消，資料夾沒人用了就 unwatch。跟著 tail task 一起結束。
pub struct WatchGuard {
    owner: Arc<SharedWatcher>,
    dir: PathBuf,
    name: OsString,
    id: u64,
}

impl SharedWatcher {
    /// 監看 `path` 所在的資料夾（檔案可能還不存在，或被 rename 替換），這個檔有事件時叫醒 `wake`。
    /// 失敗（watcher 開不起來、資料夾不存在）時回 None，tail 只靠輪詢。
    pub fn register(self: &Arc<Self>, path: &Path, wake: Arc<Notify>) -> Option<WatchGuard> {
        use notify::Watcher;
        let name = path.file_name()?.to_os_string();
        let dir = path.parent()?.to_path_buf();
        {
            let mut d = self.dirs.lock().unwrap_or_else(|e| e.into_inner());
            let WatchedDirs { watcher, count } = &mut *d;
            if watcher.is_none() {
                let routes = self.routes.clone();
                *watcher =
                    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                        let Ok(ev) = res else { return };
                        let routes = routes.lock().unwrap_or_else(|e| e.into_inner());
                        for p in &ev.paths {
                            let Some(waiting) = p.file_name().and_then(|n| routes.get(n)) else {
                                continue;
                            };
                            // 同名的檔在別的資料夾不叫醒；事件路徑的寫法和登記時不同（macOS 的 /private/var 之類）
                            // 對不上任何一個時，退回同名的全部叫醒（多讀一次無害）
                            let exact: Vec<&Route> = waiting
                                .iter()
                                .filter(|r| p.parent() == Some(r.dir.as_path()))
                                .collect();
                            let wake = if exact.is_empty() {
                                waiting.iter().collect()
                            } else {
                                exact
                            };
                            for r in wake {
                                r.wake.notify_one();
                            }
                        }
                    })
                    .ok();
            }
            let w = watcher.as_mut()?;
            let n = count.get(&dir).copied().unwrap_or(0);
            if n == 0 && w.watch(&dir, notify::RecursiveMode::NonRecursive).is_err() {
                return None;
            }
            count.insert(dir.clone(), n + 1);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.clone())
            .or_default()
            .push(Route {
                id,
                dir: dir.clone(),
                wake,
            });
        Some(WatchGuard {
            owner: self.clone(),
            dir,
            name,
            id,
        })
    }

    /// 目前 watch 中的資料夾數。
    pub fn watched_dirs(&self) -> usize {
        self.dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .count
            .len()
    }

    /// 目前登記中的 tail 數。
    pub fn watched_files(&self) -> usize {
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(Vec::len)
            .sum()
    }
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        use notify::Watcher;
        {
            let mut routes = self.owner.routes.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = routes.get_mut(&self.name) {
                v.retain(|r| r.id != self.id);
                if v.is_empty() {
                    routes.remove(&self.name);
                }
            }
        }
        let mut d = self.owner.dirs.lock().unwrap_or_else(|e| e.into_inner());
        let WatchedDirs { watcher, count } = &mut *d;
        if let Some(n) = count.get_mut(&self.dir) {
            *n -= 1;
            if *n == 0 {
                count.remove(&self.dir);
                // 資料夾已被刪掉時 watch 早就自己沒了，unwatch 的錯誤不理
                if let Some(w) = watcher {
                    let _ = w.unwatch(&self.dir);
                }
            }
        }
    }
}

struct Sender {
    tx: mpsc::Sender<Input>,
    session: String,
    file: String,
    tail: u64,
    degraded: bool,
}

impl Sender {
    /// 依行號把行與解析錯誤送給 merge；第一個壞行送 Degraded，之後第一個好行送 Recovered。
    async fn send(&mut self, poll: io::Result<Poll>) {
        let poll = match poll {
            Ok(p) => p,
            Err(e) => {
                crate::logging::log(&format!("transcript：讀取失敗 {}：{e}", self.file));
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
                        tail: self.tail,
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
                            tail: self.tail,
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

/// subagent 的檔案：`<transcript 去掉 .jsonl>/subagents/agent-<id>.<ext>`（transcript 是 `jsonl`、meta 是 `meta.json`）。
pub fn subagent_file(transcript: &Path, agent_id: &str, ext: &str) -> PathBuf {
    let stem = transcript
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    transcript
        .with_file_name(stem)
        .join("subagents")
        .join(format!("agent-{agent_id}.{ext}"))
}

/// 讀 subagent 的 `meta.json`（#680）。SubagentStart 送達後 40–60 ms 才寫好（實測），所以最多試 `tries` 次，
/// 兩次之間呼叫 `wait`（serve 傳 sleep；測試傳不等待的版本，測試不 sleep）。
/// 檔案在但沒有 `toolUseId`（Skill 的 fork）回 NoToolUse；試完都讀不到回 Unreadable。
pub async fn read_agent_meta<W, F>(path: PathBuf, tries: u32, mut wait: W) -> AgentMetaRead
where
    W: FnMut() -> F,
    F: std::future::Future<Output = ()>,
{
    for i in 0..tries {
        if i > 0 {
            wait().await;
        }
        // 檔案很小（< 1 KB），直接讀；寫到一半解析失敗就下次再讀
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        return match v["toolUseId"].as_str().filter(|s| !s.is_empty()) {
            Some(id) => AgentMetaRead::ToolUse(id.to_string()),
            None => AgentMetaRead::NoToolUse,
        };
    }
    AgentMetaRead::Unreadable
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

impl TailAction {
    /// 執行這個動作時要交給 merge 的 [`Input::TailReset`]：被停掉或重開的 tail 不會再送 Recovered。
    /// `tail` 是新開的 tail 的編號（停掉時是比現有的都大的編號），比它舊的 tail 送來的降級訊號 merge 不再理會。
    pub fn reset(&self, ts_seen: u64, tail: u64) -> Input {
        let (session, file) = match self {
            TailAction::Start { session, file, .. } | TailAction::Stop { session, file } => {
                (session, Some(file.clone()))
            }
            TailAction::StopSession { session } => (session, None),
        };
        Input::TailReset {
            session: session.clone(),
            file,
            tail,
            ts_seen,
        }
    }
}

/// 看 hook 決定要開／關哪些 tail（純邏輯；執行由組裝 collector 的地方負責）。
///
/// - 主 transcript：`SessionStart(startup／clear)` 從檔頭；resume／compact 或沒收到 SessionStart 就先看到
///   其他事件時從檔尾。實測 compact 與 resume 沿用同一個檔，所以已在 tail 的 session 不重開。
/// - subagent：`SubagentStart` 開 `<transcript 去掉 .jsonl>/subagents/agent-<id>.jsonl`，`SubagentStop` 關。
///   collector 重啟時還在跑的 subagent 沒有 SubagentStart：第一個帶 `agent_id`（有 `agent_type`，不是背景 agent）
///   的 hook 補開它的 tail（#684 N1）。
/// - `SessionEnd` 關掉該 session 的全部 tail；之後若 resume 會再開。
/// - 讀過的檔（[`TailPlanner::seed`] 交入的 store 記錄、或這一輪 [`TailPlanner::saw_line`] 看到的）再開時
///   從記錄的行之後接著讀（#684 R11）；startup／clear 是新檔，一律從檔頭。
#[derive(Default)]
pub struct TailPlanner {
    main: std::collections::HashMap<String, PathBuf>,
    agents: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// 這一輪結束過的 subagent：晚到的 hook 不再重開
    stopped: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// session → file → (記錄時的路徑（`/` 分隔，不知道時 None）, 讀到的行號)。分兩層：每行都要找，
    /// 用 `&str` 就找得到，不必先組 key（E-S10）
    read:
        std::collections::HashMap<String, std::collections::HashMap<String, (Option<String>, u64)>>,
    /// session → file → 現在 tail 的路徑（`/` 分隔），開 tail 時算好，每行不再重組（E-S10）
    paths: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// 已結束的 session（SessionEnd，或 merge 判定 abandoned）：下一個 SessionStart 之前不再開 tail（C12、L-8）
    ended: std::collections::HashSet<String>,
}

fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// subagent 的 transcript：`<主檔去掉 .jsonl>/subagents/<file>.jsonl`
fn subagent_path(main: &Path, file: &str) -> PathBuf {
    subagent_file(main, file.strip_prefix("agent-").unwrap_or(file), "jsonl")
}

impl TailPlanner {
    /// collector 重啟時交入 store 記的行號（`tails` 表）；`path` 是記錄時的檔案路徑（`/` 分隔，不知道時 None）。
    pub fn seed(&mut self, session: &str, file: &str, path: Option<&str>, line_no: u64) {
        self.read.entry(session.to_string()).or_default().insert(
            file.to_string(),
            (path.map(|p| p.replace('\\', "/")), line_no),
        );
    }

    /// tail 讀到第 `no` 行（serve 每收到一行就告訴 planner），之後重開同一個檔時從這裡接著讀。
    /// 每行都會呼叫：已有記錄時只更新行號（路徑變了才換），不配置字串（E-S10）。
    pub fn saw_line(&mut self, session: &str, file: &str, no: u64) {
        let path = self.paths.get(session).and_then(|m| m.get(file));
        if let Some(entry) = self.read.get_mut(session).and_then(|m| m.get_mut(file)) {
            entry.1 = no;
            if entry.0.as_ref() != path {
                entry.0 = path.cloned();
            }
            return;
        }
        // 不是經過這裡開的 tail（測試直接餵行）：照主檔推出路徑
        let path = path.cloned().or_else(|| {
            self.main.get(session).map(|m| {
                if file == "main" {
                    slash(m)
                } else {
                    slash(&subagent_path(m, file))
                }
            })
        });
        self.read
            .entry(session.to_string())
            .or_default()
            .insert(file.to_string(), (path, no));
    }

    /// 重開 `file` 時從哪裡讀：有這個檔的記錄（路徑相同或記錄沒有路徑）就接著讀，否則用 `default`。
    fn resume_from(&self, session: &str, file: &str, path: &Path, default: Start) -> Start {
        match self.read.get(session).and_then(|m| m.get(file)) {
            Some((recorded, n)) if recorded.as_deref().is_none_or(|r| r == slash(path)) => {
                Start::After(*n)
            }
            _ => default,
        }
    }

    /// merge 判定這個 session 結束了（沒有 SessionEnd、被丟下的 abandoned，L-8）：停掉它的 tail。
    /// 不停的話 tail 與它的 watcher 會一直留著，常駐服務（不閒置結束）下只增不減。
    pub fn on_session_ended(&mut self, session: &str) -> Vec<TailAction> {
        self.ended.insert(session.to_string());
        self.paths.remove(session);
        let had_main = self.main.remove(session).is_some();
        let had_agents = self.agents.remove(session).is_some_and(|a| !a.is_empty());
        self.stopped.remove(session);
        if had_main || had_agents {
            vec![TailAction::StopSession {
                session: session.to_string(),
            }]
        } else {
            Vec::new()
        }
    }

    /// 這個 session 結束了、還沒有新的 SessionStart。
    pub fn is_ended(&self, session: &str) -> bool {
        self.ended.contains(session)
    }

    pub fn on_hook(&mut self, h: &crate::hook::HookInput) -> Vec<TailAction> {
        let out = self.plan_hook(h);
        // 開 tail 時把路徑記下，之後每行的 saw_line 直接用（E-S10）
        for a in &out {
            if let TailAction::Start {
                session,
                file,
                path,
                ..
            } = a
            {
                self.paths
                    .entry(session.clone())
                    .or_default()
                    .insert(file.clone(), slash(path));
            }
        }
        out
    }

    fn plan_hook(&mut self, h: &crate::hook::HookInput) -> Vec<TailAction> {
        let sid = h.session_id().to_string();
        let event = h.event_name();
        let mut out = Vec::new();

        if event == "SessionEnd" {
            return self.on_session_ended(&sid);
        }
        // 結束之後、下一個 SessionStart 之前的 hook（/clear 後舊 id 晚到的背景 agent 結果）不開 tail（C12）：
        // 沒有東西會再停掉它，tail 與 watcher 會一直留著。
        // main 的 UserPromptSubmit／PreToolUse 例外：merge 依同一條規則把 session 重新打開（#679，閒置被補
        // abandoned 後終端機又用起來，不會有 SessionStart），tail 要跟著回來
        let main_activity = matches!(event, "UserPromptSubmit" | "PreToolUse")
            && h.str_field("agent_id").is_none_or(str::is_empty);
        if event == "SessionStart" || main_activity {
            self.ended.remove(&sid);
        } else if self.ended.contains(&sid) {
            return out;
        }

        // UNC 路徑不碰：Windows 會去連那台主機（送出 NTLM 憑證），離線時還會卡住（#762 W-M5）。
        // 不 tail 的 session 照收 hook，10 秒後依不變式 2 降級進串流
        let transcript = h
            .str_field("transcript_path")
            .filter(|p| !p.is_empty() && !crate::derive::is_unc(p))
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
                    // startup 與 clear 都是新的 session、新的檔（`/clear` 換 session id，#688 實測），從檔頭讀
                    let fresh = event == "SessionStart"
                        && matches!(h.str_field("source"), Some("startup" | "clear"));
                    let start = if fresh {
                        Start::Beginning
                    } else {
                        self.resume_from(&sid, "main", path, Start::End)
                    };
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
                    out.push(TailAction::Start {
                        session: sid,
                        path: subagent_path(path, &file),
                        file,
                        start: Start::Beginning,
                    });
                }
            }
            ("SubagentStop", Some(id), _) => {
                let file = format!("agent-{id}");
                self.stopped
                    .entry(sid.clone())
                    .or_default()
                    .insert(file.clone());
                if self.agents.get_mut(&sid).is_some_and(|a| a.remove(&file)) {
                    out.push(TailAction::Stop { session: sid, file });
                }
            }
            // 重啟前就在跑的 subagent（N1）：背景 agent 的 agent_type 是空字串或沒有，不開 lane 也不 tail
            (_, Some(id), Some(path))
                if h.str_field("agent_type").is_some_and(|t| !t.is_empty()) =>
            {
                let file = format!("agent-{id}");
                let known = self.agents.get(&sid).is_some_and(|a| a.contains(&file))
                    || self.stopped.get(&sid).is_some_and(|a| a.contains(&file));
                if !known {
                    self.agents
                        .entry(sid.clone())
                        .or_default()
                        .insert(file.clone());
                    let sub = subagent_path(path, &file);
                    let start = self.resume_from(&sid, &file, &sub, Start::Beginning);
                    out.push(TailAction::Start {
                        session: sid,
                        file,
                        path: sub,
                        start,
                    });
                }
            }
            _ => {}
        }
        out
    }
}
