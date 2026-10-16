//! store：Event 寫進 JSONL（一 session 一檔），索引寫進 SQLite（contracts.md §5）。
//!
//! 寫入順序固定是「先 JSONL、後索引」，所以當機時索引只會落後、不會超前；
//! [`Store::open`] 會截掉 JSONL 尾端沒寫完的半行，並把還沒進索引的完整行補進去。
//! 索引可以整個刪掉，下次開啟時從 JSONL 重建。

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::event::{Event, Payload};

pub const SCHEMA_VERSION: u32 = 3;
/// 資料保留多久：最後活動早於這麼多天前的 session 由 [`Store::prune`] 清掉
pub const RETENTION_DAYS: u64 = 30;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Sql(rusqlite::Error),
    /// 索引的 schema 比程式新：不寫，避免弄壞新版的資料。
    FutureSchema(u32),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "store I/O：{e}"),
            StoreError::Sql(e) => write!(f, "store SQLite：{e}"),
            StoreError::FutureSchema(v) => {
                write!(f, "index.sqlite 的 schema {v} 比支援的 {SCHEMA_VERSION} 新")
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub host: String,
    pub cwd: String,
    pub transcript_path: Option<String>,
    pub model: Option<String>,
    pub branch: Option<String>,
    pub first_prompt: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub end_reason: Option<String>,
    pub status: String,
    pub status_since: u64,
    pub commits: u64,
    pub dangers: u64,
    pub failures: u64,
    pub cost_usd: f64,
    pub thinking_tokens: u64,
    pub last_seq: u64,
}

/// `tails` 表的一列（schema v3，#684）：某個 transcript 檔已產生 Event 的最大行號。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailRow {
    pub session: String,
    pub file: String,
    pub path: Option<String>,
    pub line_no: u64,
}

/// [`Store::prune`] 清掉了什麼。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub sessions: Vec<String>,
    /// 刪掉的 JSONL 大小
    pub bytes: u64,
}

/// collector 重啟時交給 merge 接續的狀態，見 [`Store::counters`]。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counters {
    pub seq: u64,
    pub pos: u64,
    /// 路徑基準（session 第一次 `session.started` 的 cwd）；不知道時是空字串
    pub cwd: String,
    /// sessions 列的結束時間與狀態、最後一筆 Event 的 ts_seen（重啟時種回 merge，#679 D3）
    pub ended_at: Option<u64>,
    pub status: String,
    pub last_seen: u64,
}

/// session 最後一個 prompt 的文字與串流最後一個 `pos`（見 [`Store::last_prompt_and_pos`]）。
pub type LastSeen = (Option<String>, Option<u64>);

#[derive(Clone, Debug, PartialEq)]
pub struct Touch {
    pub session: String,
    pub kind: String,
    pub count: u64,
    pub last_ts: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub session: String,
    pub seq: u64,
    pub tool_use_id: String,
    pub path: String,
    pub kind: String,
    pub agent: String,
    pub ts: u64,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttentionRow {
    pub session: String,
    pub kind: String,
    pub reference: String,
    pub reason: Option<String>,
    pub raised_at: u64,
    /// danger 解除後仍留在佇列（等 ack），這時有值
    pub cleared_at: Option<u64>,
}

const SCHEMA_V1: &str = "
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  host TEXT NOT NULL DEFAULT 'local',
  cwd TEXT NOT NULL,
  transcript_path TEXT,
  model TEXT,
  branch TEXT,
  first_prompt TEXT,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  end_reason TEXT,
  status TEXT NOT NULL DEFAULT 'run',
  status_since INTEGER NOT NULL,
  commits INTEGER NOT NULL DEFAULT 0,
  dangers INTEGER NOT NULL DEFAULT 0,
  failures INTEGER NOT NULL DEFAULT 0,
  cost_usd REAL NOT NULL DEFAULT 0,
  thinking_tokens INTEGER NOT NULL DEFAULT 0,
  last_seq INTEGER NOT NULL DEFAULT 0,
  summary TEXT
);
CREATE INDEX sessions_by_cwd ON sessions (cwd, started_at);

CREATE TABLE events (
  session TEXT NOT NULL,
  seq INTEGER NOT NULL,
  type TEXT NOT NULL,
  pos INTEGER,
  ts_seen INTEGER NOT NULL,
  offset INTEGER NOT NULL,
  PRIMARY KEY (session, seq)
) WITHOUT ROWID;
CREATE INDEX events_by_pos ON events (session, pos) WHERE pos IS NOT NULL;

CREATE TABLE file_touch (
  cwd TEXT NOT NULL,
  path TEXT NOT NULL,
  session TEXT NOT NULL,
  kind TEXT NOT NULL,
  count INTEGER NOT NULL,
  last_ts INTEGER NOT NULL,
  PRIMARY KEY (cwd, path, session, kind)
) WITHOUT ROWID;

CREATE TABLE changes (
  session TEXT NOT NULL,
  seq INTEGER NOT NULL,
  tool_use_id TEXT NOT NULL,
  cwd TEXT NOT NULL,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  agent TEXT NOT NULL,
  ts INTEGER NOT NULL,
  added INTEGER,
  removed INTEGER,
  PRIMARY KEY (session, seq)
) WITHOUT ROWID;
CREATE INDEX changes_by_ts ON changes (cwd, ts DESC);

CREATE TABLE attention (
  session TEXT NOT NULL,
  kind TEXT NOT NULL,
  ref TEXT NOT NULL,
  reason TEXT,
  raised_at INTEGER NOT NULL,
  cleared_at INTEGER,
  acked_at INTEGER,
  PRIMARY KEY (session, kind, ref)
) WITHOUT ROWID;
";

/// v2：佇列是「未確認，且未解除或是 danger」（danger 解除後留到 ack）。
const ATTENTION_OPEN_V2: &str = "
CREATE INDEX attention_open ON attention (raised_at)
  WHERE acked_at IS NULL AND (cleared_at IS NULL OR kind = 'danger');
";

/// v3：每個 transcript 檔已產生 Event 的最大行號，collector 重啟後接著讀（#684）。
const TAILS_V3: &str = "
CREATE TABLE IF NOT EXISTS tails (
  session TEXT NOT NULL,
  file TEXT NOT NULL,
  path TEXT,
  line_no INTEGER NOT NULL,
  PRIMARY KEY (session, file)
) WITHOUT ROWID;
";

/// Event 帶 `line` 時更新那個檔的行號（同一個檔的行依序進來，直接覆寫；檔案被替換後行號變小也跟著變小）。
const UPSERT_TAIL_LINE: &str = "INSERT INTO tails (session, file, line_no) VALUES (?1, ?2, ?3)
  ON CONFLICT (session, file) DO UPDATE SET line_no = excluded.line_no";
/// session.started 記主檔路徑；換了檔就把行號歸零（舊檔的行號對新檔沒有意義）。
const UPSERT_TAIL_PATH: &str = "INSERT INTO tails (session, file, path, line_no) VALUES (?1, 'main', ?2, 0)
  ON CONFLICT (session, file) DO UPDATE SET
    line_no = CASE WHEN tails.path IS excluded.path THEN tails.line_no ELSE 0 END, path = excluded.path";

/// 一次 flush 裡同一個 session 檔要追加的內容。
struct Batch {
    name: String,
    base: u64,
    buf: Vec<u8>,
}

pub struct Store {
    events_dir: PathBuf,
    db: Connection,
    pending: Vec<Event>,
    /// 每個 session 檔目前的長度＝下一行的 offset
    file_len: HashMap<String, u64>,
    /// 索引時需要的狀態：session 的 cwd、工具卡片碰的是哪個檔
    cwd_of: HashMap<String, String>,
    path_of_tool: HashMap<(String, String), String>,
    /// 已寫進 JSONL、還沒進索引的 (Event, offset)：索引寫入失敗時留到下次 flush
    unindexed: Vec<(Event, u64)>,
    /// 開啟時 recover 讀了多少 bytes
    recover_bytes: u64,
}

impl Store {
    /// 開啟（或建立）資料目錄：`<home>/events/`、`<home>/index.sqlite`。
    pub fn open(home: &Path) -> Result<Store, StoreError> {
        let events_dir = home.join("events");
        fs::create_dir_all(&events_dir)?;
        let db = Connection::open(home.join("index.sqlite"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        let backfill_tails = migrate(&db)?;

        let mut store = Store {
            events_dir,
            db,
            pending: Vec::new(),
            file_len: HashMap::new(),
            cwd_of: HashMap::new(),
            path_of_tool: HashMap::new(),
            unindexed: Vec::new(),
            recover_bytes: 0,
        };
        {
            let mut stmt = store.db.prepare("SELECT id, cwd FROM sessions")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, cwd) = row?;
                store.cwd_of.insert(id, cwd);
            }
        }
        if backfill_tails {
            store.backfill_tails()?;
        }
        store.recover()?;
        Ok(store)
    }

    /// v2 → v3 升級時掃一次全部 JSONL，回填每個 transcript 檔讀到的行號（只在升級那次做）。
    fn backfill_tails(&mut self) -> Result<(), StoreError> {
        let mut last: BTreeMap<(String, String), u64> = BTreeMap::new();
        let mut paths: BTreeMap<String, String> = BTreeMap::new();
        for entry in fs::read_dir(&self.events_dir)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            for line in BufReader::new(File::open(&path)?).lines() {
                let Ok(e) = Event::from_json_line(&line?) else {
                    continue;
                };
                if let Some(l) = &e.line {
                    last.insert((e.session.clone(), l.file.clone()), l.no);
                }
                if e.kind == "session.started"
                    && let Some(p) = e.payload["transcriptPath"].as_str()
                {
                    paths.insert(e.session.clone(), p.to_string());
                }
            }
        }
        let tx = self.db.transaction()?;
        for (sid, p) in &paths {
            tx.execute(UPSERT_TAIL_PATH, params![sid, p])?;
        }
        for ((sid, file), no) in &last {
            tx.execute(UPSERT_TAIL_LINE, params![sid, file, *no as i64])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 先放進緩衝；呼叫端定期 [`Store::flush`]（store task 每 100 ms 一次）。
    pub fn append(&mut self, e: Event) {
        self.pending.push(e);
    }

    /// 寫入失敗時不丟資料（#683）：JSONL 寫不進去的 Event 放回緩衝、寫到一半的檔截回原長度；
    /// 已寫進 JSONL 但索引交易失敗的留在 `unindexed`，下次 flush 再索引。
    pub fn flush(&mut self) -> Result<(), StoreError> {
        if self.pending.is_empty() && self.unindexed.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);

        // 1. 依 session 分組寫 JSONL（每檔一次 write_all），記下每行的 offset
        let mut batches: Vec<Batch> = Vec::new();
        let mut members: Vec<Vec<usize>> = Vec::new();
        let mut offset_of = vec![0u64; pending.len()];
        for (i, e) in pending.iter().enumerate() {
            let name = file_name(&e.session);
            let b = match batches.iter().position(|b| b.name == name) {
                Some(b) => b,
                None => {
                    let base = match self.file_len.get(&name) {
                        Some(len) => *len,
                        None => fs::metadata(self.events_dir.join(&name))
                            .map(|m| m.len())
                            .unwrap_or(0),
                    };
                    batches.push(Batch {
                        name,
                        base,
                        buf: Vec::new(),
                    });
                    members.push(Vec::new());
                    batches.len() - 1
                }
            };
            let batch = &mut batches[b];
            offset_of[i] = batch.base + batch.buf.len() as u64;
            batch.buf.extend_from_slice(e.to_json_line().as_bytes());
            batch.buf.push(b'\n');
            members[b].push(i);
        }
        let mut written = vec![false; pending.len()];
        let mut failure = None;
        for (b, idx) in batches.into_iter().zip(&members) {
            if failure.is_some() {
                break;
            }
            match self.write_batch(&b) {
                Ok(()) => {
                    self.file_len.insert(b.name, b.base + b.buf.len() as u64);
                    for &i in idx {
                        written[i] = true;
                    }
                }
                Err(e) => failure = Some(e),
            }
        }
        for (i, e) in pending.into_iter().enumerate() {
            if written[i] {
                self.unindexed.push((e, offset_of[i]));
            } else {
                self.pending.push(e);
            }
        }

        // 2. 再寫索引（同一個交易）；失敗時交易回滾，整批留到下次
        let tx = self.db.transaction()?;
        for (e, off) in &self.unindexed {
            index_event(&tx, &mut self.cwd_of, &mut self.path_of_tool, e, *off)?;
        }
        tx.commit()?;
        self.unindexed.clear();
        match failure {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    /// 追加一批到 session 檔；寫到一半失敗就截回原長度，不留半行。
    fn write_batch(&mut self, b: &Batch) -> io::Result<()> {
        let path = self.events_dir.join(&b.name);
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        if let Err(e) = f.write_all(&b.buf) {
            self.file_len.remove(&b.name);
            if let Err(t) = OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(b.base))
            {
                crate::logging::log(&format!("store：截回 {} 失敗：{t}", b.name));
            }
            return Err(e);
        }
        Ok(())
    }

    /// 截掉半行、補上 JSONL 有但索引沒有的行、刪掉索引超前 JSONL 的列。
    /// 只讀索引最後一行（含）之後的部分（#683）：索引完整時每個檔只讀一行。
    fn recover(&mut self) -> Result<(), StoreError> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.events_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        files.sort();
        for path in files {
            let len = fs::metadata(&path)?.len();
            let Some(session) = first_session(&path)? else {
                continue;
            };
            let indexed: Option<i64> = self.db.query_row(
                "SELECT MAX(offset) FROM events WHERE session = ?1",
                params![session],
                |r| r.get(0),
            )?;
            // 索引超前 JSONL（檔案被截短）時從頭讀
            let from = indexed.map(|o| o as u64).filter(|o| *o < len).unwrap_or(0);
            let mut file = File::open(&path)?;
            file.seek(SeekFrom::Start(from))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            self.recover_bytes += bytes.len() as u64;
            let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            let end = from + complete as u64;
            if end < len {
                OpenOptions::new().write(true).open(&path)?.set_len(end)?;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            self.file_len.insert(name, end);

            let mut start = 0usize;
            let mut lines: Vec<(Event, u64)> = Vec::new();
            for (i, b) in bytes[..complete].iter().enumerate() {
                if *b == b'\n' {
                    let text = String::from_utf8_lossy(&bytes[start..i]);
                    if let Ok(e) = Event::from_json_line(&text) {
                        lines.push((e, from + start as u64));
                    }
                    start = i + 1;
                }
            }
            let tx = self.db.transaction()?;
            tx.execute(
                "DELETE FROM events WHERE session = ?1 AND offset >= ?2",
                params![session, end as i64],
            )?;
            let indexed: Option<i64> = tx.query_row(
                "SELECT MAX(offset) FROM events WHERE session = ?1",
                params![session],
                |r| r.get(0),
            )?;
            for (e, off) in &lines {
                if indexed.is_none_or(|max| (*off as i64) > max) {
                    index_event(&tx, &mut self.cwd_of, &mut self.path_of_tool, e, *off)?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }

    /// 串流的「載入更多」：`before` 之前（不含）最後 `limit` 個有 `pos` 的 Event，依 pos 由小到大。
    pub fn stream_before(
        &self,
        session: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        let before = before.map_or(i64::MAX, |b| b as i64);
        let mut stmt = self.db.prepare(
            "SELECT offset FROM events WHERE session = ?1 AND pos IS NOT NULL AND pos < ?2
             ORDER BY pos DESC LIMIT ?3",
        )?;
        let offsets: Vec<i64> = stmt
            .query_map(params![session, before, limit as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut out = Vec::with_capacity(offsets.len());
        if offsets.is_empty() {
            return Ok(out);
        }
        let mut file = BufReader::new(File::open(self.events_dir.join(file_name(session)))?);
        for off in offsets.into_iter().rev() {
            file.seek(SeekFrom::Start(off as u64))?;
            let mut line = String::new();
            file.read_line(&mut line)?;
            if let Ok(e) = Event::from_json_line(line.trim_end()) {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// 某 session 在 seq 範圍（含兩端）內的所有 Event，依 seq 排序。
    pub fn events_by_seq(
        &self,
        session: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<Event>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT offset FROM events WHERE session = ?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq",
        )?;
        let offsets: Vec<i64> = stmt
            .query_map(params![session, from as i64, to as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        self.read_at(session, offsets)
    }

    /// `pos` 之後的第一個帶 pos 的 Event：(seq, ts_seen)。
    pub fn next_pos_after(
        &self,
        session: &str,
        pos: u64,
    ) -> Result<Option<(u64, u64)>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT seq, ts_seen FROM events WHERE session = ?1 AND pos > ?2 ORDER BY pos LIMIT 1",
                params![session, pos as i64],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?)
    }

    /// 每個 session 已用到的最大 seq、pos（沒進過串流的是 0）與路徑基準 cwd。
    pub fn counters(&self) -> Result<HashMap<String, Counters>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT e.session, MAX(e.seq), COALESCE(MAX(e.pos), 0), COALESCE(s.cwd, ''),
                    s.ended_at, COALESCE(s.status, ''), MAX(e.ts_seen)
             FROM events e LEFT JOIN sessions s ON s.id = e.session GROUP BY e.session",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Counters {
                    seq: r.get::<_, i64>(1)? as u64,
                    pos: r.get::<_, i64>(2)? as u64,
                    cwd: r.get(3)?,
                    ended_at: r.get::<_, Option<i64>>(4)?.map(|t| t as u64),
                    status: r.get(5)?,
                    last_seen: r.get::<_, i64>(6)? as u64,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `ts_seen` ≥ `since` 的所有 Event，依 (ts_seen, session, seq)：collector 重啟時重建 live 狀態用。
    pub fn recent_events(&self, since: u64) -> Result<Vec<Event>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, offset FROM events WHERE ts_seen >= ?1 ORDER BY ts_seen, session, seq",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![since as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        let mut start = 0;
        while start < rows.len() {
            // 同一個 session 連續的列一次讀
            let session = rows[start].0.clone();
            let end = rows[start..]
                .iter()
                .position(|(s, _)| *s != session)
                .map_or(rows.len(), |n| start + n);
            let offsets: Vec<i64> = rows[start..end].iter().map(|(_, o)| *o).collect();
            out.extend(self.read_at(&session, offsets)?);
            start = end;
        }
        out.sort_by(|a, b| (a.ts_seen, &a.session, a.seq).cmp(&(b.ts_seen, &b.session, b.seq)));
        Ok(out)
    }

    /// `since` 之後的 usage：(session, ts_seen, costUsd)（#675 費用基準）。
    pub fn usage_since(&self, since: u64) -> Result<Vec<(String, u64, f64)>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, offset FROM events WHERE type = 'usage' AND ts_seen >= ?1 ORDER BY session, seq",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![since as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut by_session: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for (session, off) in rows {
            by_session.entry(session).or_default().push(off);
        }
        let mut out = Vec::new();
        for (session, offsets) in by_session {
            for e in self.read_at(&session, offsets)? {
                out.push((
                    session.clone(),
                    e.ts_seen,
                    e.payload["costUsd"].as_f64().unwrap_or(0.0),
                ));
            }
        }
        Ok(out)
    }

    fn read_at(&self, session: &str, offsets: Vec<i64>) -> Result<Vec<Event>, StoreError> {
        let mut out = Vec::with_capacity(offsets.len());
        if offsets.is_empty() {
            return Ok(out);
        }
        let mut file = BufReader::new(File::open(self.events_dir.join(file_name(session)))?);
        for off in offsets {
            file.seek(SeekFrom::Start(off as u64))?;
            let mut line = String::new();
            file.read_line(&mut line)?;
            if let Ok(e) = Event::from_json_line(line.trim_end()) {
                out.push(e);
            }
        }
        Ok(out)
    }

    pub fn session(&self, id: &str) -> Result<Option<SessionRow>, StoreError> {
        Ok(self
            .db
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                params![id],
                session_row,
            )
            .optional()?)
    }

    /// Sessions tab：最新開始的在前；`cwd` 為 None 時列出全部專案，否則是 `cwd` 本身與底下（worktree）的。
    pub fn sessions(&self, cwd: Option<&str>, limit: usize) -> Result<Vec<SessionRow>, StoreError> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE ?1 IS NULL OR REPLACE(cwd, '\\', '/') = ?1
               OR substr(REPLACE(cwd, '\\', '/'), 1, length(?1) + 1) = ?1 || '/'
             ORDER BY started_at DESC, id LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![cwd, limit as i64], session_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 所有 session 的 `cwd`（`/` 分隔、不重複）。
    pub fn cwds(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT REPLACE(cwd, '\\', '/') FROM sessions WHERE cwd != ''")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 串流最後一個 `pos`。
    pub fn last_pos(&self, session: &str) -> Result<Option<u64>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT MAX(pos) FROM events WHERE session = ?1",
                params![session],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .map(|v| v as u64))
    }

    /// 這些 session 各自最後一個 `prompt.submitted` 的文字與串流最後一個 `pos`，兩句查詢批次取（#683，Sessions tab）。
    pub fn last_prompt_and_pos(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, LastSeen>, StoreError> {
        let mut out: HashMap<String, LastSeen> = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let list = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
        // SQLite 的 MAX() 聚合時，同列的其他欄位取自最大值那一列
        let mut stmt = self.db.prepare(
            "SELECT session, offset, MAX(seq) FROM events
             WHERE type = 'prompt.submitted' AND session IN (SELECT value FROM json_each(?1))
             GROUP BY session",
        )?;
        let prompts: Vec<(String, i64)> = stmt
            .query_map(params![list], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (session, off) in prompts {
            let text = self
                .read_at(&session, vec![off])?
                .pop()
                .and_then(|e| e.payload["text"].as_str().map(str::to_string));
            out.entry(session).or_default().0 = text;
        }
        let mut stmt = self.db.prepare(
            "SELECT session, MAX(pos) FROM events
             WHERE pos IS NOT NULL AND session IN (SELECT value FROM json_each(?1))
             GROUP BY session",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![list], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (session, pos) in rows {
            out.entry(session).or_default().1 = Some(pos as u64);
        }
        Ok(out)
    }

    /// 某 session 這些 type 的 Event，依 seq 排序。
    pub fn events_of_types(&self, session: &str, types: &[&str]) -> Result<Vec<Event>, StoreError> {
        let mut out = Vec::new();
        for t in types {
            let mut stmt = self
                .db
                .prepare("SELECT seq, offset FROM events WHERE session = ?1 AND type = ?2")?;
            let rows: Vec<(i64, i64)> = stmt
                .query_map(params![session, t], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            out.extend(rows);
        }
        out.sort();
        self.read_at(session, out.into_iter().map(|(_, o)| o).collect())
    }

    /// 某 session 最後一個這種 type 的 Event。
    pub fn last_of_type(&self, session: &str, kind: &str) -> Result<Option<Event>, StoreError> {
        let off: Option<i64> = self
            .db
            .query_row(
                "SELECT offset FROM events WHERE session = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
                params![session, kind],
                |r| r.get(0),
            )
            .optional()?;
        Ok(self.read_at(session, off.into_iter().collect())?.pop())
    }

    /// 「誰碰過這檔」。
    pub fn touches(&self, cwd: &str, path: &str) -> Result<Vec<Touch>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, kind, count, last_ts FROM file_touch WHERE cwd = ?1 AND path = ?2",
        )?;
        let rows = stmt.query_map(params![cwd, path], |r| {
            Ok(Touch {
                session: r.get(0)?,
                kind: r.get(1)?,
                count: r.get::<_, i64>(2)? as u64,
                last_ts: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 改動紀錄：最新在前。
    pub fn recent_changes(&self, cwd: &str, limit: usize) -> Result<Vec<Change>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, seq, tool_use_id, path, kind, agent, ts, added, removed FROM changes
             WHERE cwd = ?1 ORDER BY ts DESC, seq DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cwd, limit as i64], |r| {
            Ok(Change {
                session: r.get(0)?,
                seq: r.get::<_, i64>(1)? as u64,
                tool_use_id: r.get(2)?,
                path: r.get(3)?,
                kind: r.get(4)?,
                agent: r.get(5)?,
                ts: r.get::<_, i64>(6)? as u64,
                added: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                removed: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 注意佇列：還沒被使用者確認，且還沒自動解除或是 danger（danger 解除後留到 ack）。
    pub fn open_attention(&self) -> Result<Vec<AttentionRow>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, kind, ref, reason, raised_at, cleared_at FROM attention
             WHERE acked_at IS NULL AND (cleared_at IS NULL OR kind = 'danger') ORDER BY raised_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AttentionRow {
                session: r.get(0)?,
                kind: r.get(1)?,
                reference: r.get(2)?,
                reason: r.get(3)?,
                raised_at: r.get::<_, i64>(4)? as u64,
                cleared_at: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 使用者按「已確認」；回傳是否真的有一筆被確認。
    pub fn ack(
        &mut self,
        session: &str,
        kind: &str,
        reference: &str,
        ts: u64,
    ) -> Result<bool, StoreError> {
        let n = self.db.execute(
            "UPDATE attention SET acked_at = ?4 WHERE session = ?1 AND kind = ?2 AND ref = ?3 AND acked_at IS NULL",
            params![session, kind, reference, ts as i64],
        )?;
        Ok(n > 0)
    }

    /// 上次開啟時 recover 讀了多少 bytes 的 JSONL（測試與 healthz 用）。
    /// 清掉最後活動早於 `before`（epoch ms）的 session：先刪索引列，再刪 JSONL（#655）。
    /// 中途當掉時 JSONL 還在、索引沒了，下次開啟會被 recover 補回，再清一次即可。
    pub fn prune(&mut self, before: u64) -> Result<PruneReport, StoreError> {
        let old: Vec<String> = {
            let mut stmt = self.db.prepare(
                "SELECT s.id FROM sessions s
                 LEFT JOIN (SELECT session, MAX(ts_seen) AS t FROM events GROUP BY session) e
                   ON e.session = s.id
                 WHERE MAX(COALESCE(e.t, 0), s.started_at, COALESCE(s.ended_at, 0), s.status_since) < ?1
                 ORDER BY s.id",
            )?;
            stmt.query_map(params![before.min(i64::MAX as u64) as i64], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let mut report = PruneReport::default();
        if old.is_empty() {
            return Ok(report);
        }
        let tx = self.db.transaction()?;
        for sid in &old {
            tx.execute("DELETE FROM sessions WHERE id = ?1", params![sid])?;
            for table in ["events", "file_touch", "changes", "attention", "tails"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE session = ?1"),
                    params![sid],
                )?;
            }
        }
        tx.commit()?;
        for sid in old {
            let name = file_name(&sid);
            let path = self.events_dir.join(&name);
            if let Ok(meta) = fs::metadata(&path) {
                report.bytes += meta.len();
            }
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            self.file_len.remove(&name);
            self.cwd_of.remove(&sid);
            self.path_of_tool.retain(|(s, _), _| *s != sid);
            report.sessions.push(sid);
        }
        Ok(report)
    }

    /// collector 重啟時交給 TailPlanner：每個 transcript 檔讀到第幾行。
    /// 行號 0 的列（只記了路徑、還沒有任何行產生 Event）不回傳：那種 session 照舊從檔尾讀，
    /// 不把 #684 之前從檔尾開始 tail 的舊 session 整份重讀。
    pub fn tail_lines(&self) -> Result<Vec<TailRow>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, file, path, line_no FROM tails WHERE line_no > 0 ORDER BY session, file",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TailRow {
                session: r.get(0)?,
                file: r.get(1)?,
                path: r.get(2)?,
                line_no: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 記著的「工具卡片碰的檔」數（測試確認 session 結束後清掉）
    pub fn cached_tool_paths(&self) -> usize {
        self.path_of_tool.len()
    }

    pub fn recover_bytes(&self) -> u64 {
        self.recover_bytes
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(read_version(&self.db)?.unwrap_or(0))
    }
}

/// JSONL 第一行的 session（檔名經過清理，不一定等於 session id）。
fn first_session(path: &Path) -> Result<Option<String>, StoreError> {
    let mut line = String::new();
    BufReader::new(File::open(path)?).read_line(&mut line)?;
    Ok(Event::from_json_line(line.trim_end())
        .ok()
        .map(|e| e.session))
}

/// session id 只留安全字元當檔名，payload 不能決定寫入路徑。
fn file_name(session: &str) -> String {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{}.jsonl",
        if safe.is_empty() {
            "unknown".into()
        } else {
            safe
        }
    )
}

fn read_version(db: &Connection) -> rusqlite::Result<Option<u32>> {
    db.execute(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )?;
    let v: Option<String> = db
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(|s| s.parse().ok()))
}

/// 依版本逐步升級；v0（空資料庫）先建 v1 的表，再一路升上來。
/// 回傳要不要回填 tails：舊版資料庫升到 v3 時要；新建的不用（JSONL 由 recover 從頭索引，順便填好）。
fn migrate(db: &Connection) -> Result<bool, StoreError> {
    let from = read_version(db)?;
    let mut v = match from {
        Some(v) if v > SCHEMA_VERSION => return Err(StoreError::FutureSchema(v)),
        Some(v) => v,
        None => {
            db.execute_batch(SCHEMA_V1)?;
            db.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
                [],
            )?;
            1
        }
    };
    while v < SCHEMA_VERSION {
        let tx = db.unchecked_transaction()?;
        if v == 1 {
            tx.execute_batch(&format!(
                "DROP INDEX IF EXISTS attention_open;{ATTENTION_OPEN_V2}"
            ))?;
        }
        if v == 2 {
            tx.execute_batch(TAILS_V3)?;
        }
        v += 1;
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            params![v.to_string()],
        )?;
        tx.commit()?;
    }
    Ok(from.is_some_and(|v| v < 3))
}

const SESSION_COLUMNS: &str = "id, host, cwd, transcript_path, model, branch, first_prompt, started_at, ended_at,
    end_reason, status, status_since, commits, dangers, failures, cost_usd, thinking_tokens, last_seq";

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let u = |i: usize| r.get::<_, i64>(i).map(|v| v as u64);
    Ok(SessionRow {
        id: r.get(0)?,
        host: r.get(1)?,
        cwd: r.get(2)?,
        transcript_path: r.get(3)?,
        model: r.get(4)?,
        branch: r.get(5)?,
        first_prompt: r.get(6)?,
        started_at: u(7)?,
        ended_at: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        end_reason: r.get(9)?,
        status: r.get(10)?,
        status_since: u(11)?,
        commits: u(12)?,
        dangers: u(13)?,
        failures: u(14)?,
        cost_usd: r.get(15)?,
        thinking_tokens: u(16)?,
        last_seq: u(17)?,
    })
}

fn index_event(
    tx: &Transaction<'_>,
    cwd_of: &mut HashMap<String, String>,
    path_of_tool: &mut HashMap<(String, String), String>,
    e: &Event,
    offset: u64,
) -> rusqlite::Result<()> {
    let sid = e.session.as_str();
    let ts = e.ts_seen as i64;
    let inserted = exec(
        tx,
        "INSERT OR IGNORE INTO events (session, seq, type, pos, ts_seen, offset) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            sid,
            e.seq as i64,
            e.kind,
            e.pos.map(|p| p as i64),
            ts,
            offset as i64
        ],
    )?;
    // 同一個 (session, seq) 已經索引過：彙總也已經算過，不再累加（#683）
    if inserted == 0 {
        return Ok(());
    }
    if let Some(l) = &e.line {
        exec(tx, UPSERT_TAIL_LINE, params![sid, l.file, l.no as i64])?;
    }
    exec(
        tx,
        "INSERT OR IGNORE INTO sessions (id, host, cwd, started_at, status_since) VALUES (?1, ?2, '', ?3, ?3)",
        params![sid, e.host, ts],
    )?;
    exec(
        tx,
        "UPDATE sessions SET last_seq = MAX(last_seq, ?2) WHERE id = ?1",
        params![sid, e.seq as i64],
    )?;

    // 不認得的 type 或格式不符的 payload 只建索引、不做彙總
    let Ok(Some(payload)) = e.payload_typed() else {
        return Ok(());
    };
    let cwd = cwd_of.get(sid).cloned().unwrap_or_default();
    let touch = |kind: &str, path: &str| {
        exec(tx,
            "INSERT INTO file_touch (cwd, path, session, kind, count, last_ts) VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT (cwd, path, session, kind) DO UPDATE SET count = count + 1, last_ts = excluded.last_ts",
            params![cwd, path, sid, kind, ts],
        )
    };
    let change = |tool: &str, path: &str, kind: &str, added: Option<u64>, removed: Option<u64>| {
        exec(tx,
            "INSERT OR IGNORE INTO changes (session, seq, tool_use_id, cwd, path, kind, agent, ts, added, removed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                sid,
                e.seq as i64,
                tool,
                cwd,
                path,
                kind,
                e.agent,
                ts,
                added.map(|v| v as i64),
                removed.map(|v| v as i64)
            ],
        )
    };

    match payload {
        Payload::SessionStarted(p) => {
            // 路徑基準只取第一次的 cwd（#672）；之後的 compact／clear／resume 不換
            let cwd = cwd_of.entry(sid.to_string()).or_default();
            if cwd.is_empty() {
                *cwd = p.cwd.clone();
            }
            exec(tx,
                "UPDATE sessions SET cwd = CASE WHEN cwd = '' THEN ?2 ELSE cwd END, transcript_path = COALESCE(?3, transcript_path),
                   model = COALESCE(?4, model), branch = COALESCE(?5, branch),
                   ended_at = NULL, end_reason = NULL, status = 'run', status_since = ?6 WHERE id = ?1",
                params![sid, p.cwd, p.transcript_path, p.model, p.branch, ts],
            )?;
            if let Some(path) = &p.transcript_path {
                exec(tx, UPSERT_TAIL_PATH, params![sid, path])?;
            }
        }
        Payload::SessionEnded(p) => {
            // 結束的 session 不會再有工具結束的 Event（#686）
            path_of_tool.retain(|(s, _), _| s != sid);
            exec(
                tx,
                "UPDATE sessions SET ended_at = ?2, end_reason = ?3, status = 'ended', status_since = ?2 WHERE id = ?1",
                params![sid, ts, p.reason],
            )?;
        }
        Payload::SessionStatus(p) => {
            let status = serde_json::to_value(p.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            exec(
                tx,
                // 結束之後又回到其他狀態（被補 abandoned 的 session 繼續使用，#679）：結束不再成立
                "UPDATE sessions SET status = ?2, status_since = ?3,
                   ended_at = CASE WHEN ?2 = 'ended' THEN ended_at END,
                   end_reason = CASE WHEN ?2 = 'ended' THEN end_reason END WHERE id = ?1",
                params![sid, status, p.since as i64],
            )?;
        }
        Payload::PromptSubmitted(p) => {
            exec(
                tx,
                "UPDATE sessions SET first_prompt = COALESCE(first_prompt, ?2) WHERE id = ?1",
                params![sid, p.text],
            )?;
        }
        Payload::Usage(p) => {
            // SessionStart 沒送 model 時（VSCode、resume），主模型取 main 第一筆 usage（#673）
            let model = (e.agent == "main").then_some(p.model.as_str());
            exec(tx,
                "UPDATE sessions SET cost_usd = cost_usd + ?2, thinking_tokens = thinking_tokens + ?3,
                   model = COALESCE(model, ?4) WHERE id = ?1",
                params![sid, p.cost_usd, p.thinking as i64, model],
            )?;
        }
        Payload::ModelChanged(p) if e.agent == "main" => {
            exec(
                tx,
                "UPDATE sessions SET model = ?2 WHERE id = ?1",
                params![sid, p.to],
            )?;
        }
        Payload::GitOp(p) if p.op == "commit" && p.ok => {
            exec(
                tx,
                "UPDATE sessions SET commits = commits + 1 WHERE id = ?1",
                params![sid],
            )?;
        }
        Payload::AttentionRaised(p) => {
            let kind = serde_json::to_value(p.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            if p.kind == crate::event::AttentionKind::Danger {
                exec(
                    tx,
                    "UPDATE sessions SET dangers = dangers + 1 WHERE id = ?1",
                    params![sid],
                )?;
            }
            exec(tx,
                "INSERT OR REPLACE INTO attention (session, kind, ref, reason, raised_at, cleared_at, acked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
                params![sid, kind, p.reference, p.reason, ts],
            )?;
        }
        Payload::AttentionCleared(p) => {
            let kind = serde_json::to_value(p.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            exec(
                tx,
                "UPDATE attention SET cleared_at = ?4 WHERE session = ?1 AND kind = ?2 AND ref = ?3 AND cleared_at IS NULL",
                params![sid, kind, p.reference, ts],
            )?;
        }
        Payload::ToolUpdated(p) if p.status == crate::event::ToolStatus::Failed => {
            // 被拒或失敗的 Edit／Write 視為沒有發生（contracts §4，#682）：刪掉改動紀錄、碰檔計數減一並記 fail（Esc 中斷不記）。
            // 之後同一張卡片的 tool.result（ok=false）不再記第二次 fail
            let rows: Vec<(String, String)> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT path, kind FROM changes WHERE session = ?1 AND tool_use_id = ?2",
                )?;
                stmt.query_map(params![sid, p.tool_use_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            for (path, kind) in &rows {
                exec(
                    tx,
                    "DELETE FROM changes WHERE session = ?1 AND tool_use_id = ?2",
                    params![sid, p.tool_use_id],
                )?;
                exec(
                    tx,
                    "UPDATE file_touch SET count = count - 1 WHERE cwd = ?1 AND path = ?2 AND session = ?3 AND kind = ?4",
                    params![cwd, path, sid, kind],
                )?;
                exec(
                    tx,
                    "DELETE FROM file_touch WHERE cwd = ?1 AND path = ?2 AND session = ?3 AND kind = ?4 AND count <= 0",
                    params![cwd, path, sid, kind],
                )?;
                // Esc 中斷不算失敗（和前端的失敗計數一致）
                if p.interrupted != Some(true) {
                    touch("fail", path)?;
                }
            }
            if !rows.is_empty() {
                path_of_tool.remove(&(sid.to_string(), p.tool_use_id));
            }
        }
        Payload::ToolResult(p) if !p.ok => {
            exec(
                tx,
                "UPDATE sessions SET failures = failures + 1 WHERE id = ?1",
                params![sid],
            )?;
            if let Some(path) = path_of_tool.get(&(sid.to_string(), p.tool_use_id.clone())) {
                touch("fail", path)?;
            }
        }
        Payload::FileRead(p) => {
            touch("read", &p.path)?;
            path_of_tool.insert((sid.to_string(), p.tool_use_id), p.path);
        }
        Payload::FileEdit(p) => {
            touch("edit", &p.path)?;
            let (added, removed) = match &p.hunk {
                Some(h) => (
                    Some(h.lines.iter().filter(|l| l.starts_with('+')).count() as u64),
                    Some(h.lines.iter().filter(|l| l.starts_with('-')).count() as u64),
                ),
                None => (None, None),
            };
            change(&p.tool_use_id, &p.path, "edit", added, removed)?;
            path_of_tool.insert((sid.to_string(), p.tool_use_id), p.path);
        }
        Payload::FileWrite(p) => {
            touch("write", &p.path)?;
            change(
                &p.tool_use_id,
                &p.path,
                "write",
                // 全文行數；舊 Event 沒有 lines 時退回預覽的行數（#682）
                Some(p.lines.unwrap_or(p.content.lines().count() as u64)),
                Some(0),
            )?;
            path_of_tool.insert((sid.to_string(), p.tool_use_id), p.path);
        }
        _ => {}
    }
    Ok(())
}

/// 執行一句 SQL；語句快取在連線上，每筆 Event 不必重新 prepare（#683）。
fn exec(tx: &Transaction<'_>, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<usize> {
    tx.prepare_cached(sql)?.execute(p)
}
