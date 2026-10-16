//! store：Event 寫進 JSONL（一 session 一檔），索引寫進 SQLite（contracts.md §5）。
//!
//! 寫入順序固定是「先 JSONL、後索引」，所以當機時索引只會落後、不會超前；
//! [`Store::open`] 會截掉 JSONL 尾端沒寫完的半行，並把還沒進索引的完整行補進去。
//! 索引可以整個刪掉，下次開啟時從 JSONL 重建。

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::event::{Event, Payload};

pub const SCHEMA_VERSION: u32 = 1;

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
CREATE INDEX attention_open ON attention (raised_at) WHERE cleared_at IS NULL AND acked_at IS NULL;
";

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
}

impl Store {
    /// 開啟（或建立）資料目錄：`<home>/events/`、`<home>/index.sqlite`。
    pub fn open(home: &Path) -> Result<Store, StoreError> {
        let events_dir = home.join("events");
        fs::create_dir_all(&events_dir)?;
        let db = Connection::open(home.join("index.sqlite"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&db)?;

        let mut store = Store {
            events_dir,
            db,
            pending: Vec::new(),
            file_len: HashMap::new(),
            cwd_of: HashMap::new(),
            path_of_tool: HashMap::new(),
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
        store.recover()?;
        Ok(store)
    }

    /// 先放進緩衝；呼叫端定期 [`Store::flush`]（store task 每 100 ms 一次）。
    pub fn append(&mut self, e: Event) {
        self.pending.push(e);
    }

    pub fn flush(&mut self) -> Result<(), StoreError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);

        // 1. 依 session 分組寫 JSONL（每檔一次 write_all），記下每行的 offset
        let mut batches: Vec<Batch> = Vec::new();
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
                    batches.len() - 1
                }
            };
            let batch = &mut batches[b];
            offset_of[i] = batch.base + batch.buf.len() as u64;
            batch.buf.extend_from_slice(e.to_json_line().as_bytes());
            batch.buf.push(b'\n');
        }
        for b in batches {
            let path = self.events_dir.join(&b.name);
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?
                .write_all(&b.buf)?;
            self.file_len.insert(b.name, b.base + b.buf.len() as u64);
        }

        // 2. 再寫索引（同一個交易）
        let tx = self.db.transaction()?;
        for (e, off) in pending.iter().zip(offset_of) {
            index_event(&tx, &mut self.cwd_of, &mut self.path_of_tool, e, off)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 截掉半行、補上 JSONL 有但索引沒有的行、刪掉索引超前 JSONL 的列。
    fn recover(&mut self) -> Result<(), StoreError> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.events_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        files.sort();
        for path in files {
            let mut bytes = Vec::new();
            File::open(&path)?.read_to_end(&mut bytes)?;
            let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            if complete < bytes.len() {
                OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(complete as u64)?;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            self.file_len.insert(name, complete as u64);

            let mut start = 0usize;
            let mut lines: Vec<(Event, u64)> = Vec::new();
            for (i, b) in bytes[..complete].iter().enumerate() {
                if *b == b'\n' {
                    let text = String::from_utf8_lossy(&bytes[start..i]);
                    if let Ok(e) = Event::from_json_line(&text) {
                        lines.push((e, start as u64));
                    }
                    start = i + 1;
                }
            }
            let Some(session) = lines.first().map(|(e, _)| e.session.clone()) else {
                continue;
            };
            let tx = self.db.transaction()?;
            tx.execute(
                "DELETE FROM events WHERE session = ?1 AND offset >= ?2",
                params![session, complete as i64],
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

    /// Sessions tab：最新開始的在前；`cwd` 為 None 時列出全部專案。
    pub fn sessions(&self, cwd: Option<&str>, limit: usize) -> Result<Vec<SessionRow>, StoreError> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE (?1 IS NULL OR cwd = ?1)
             ORDER BY started_at DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![cwd, limit as i64], session_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
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

    /// 注意佇列：還沒自動解除、也還沒被使用者確認的。
    pub fn open_attention(&self) -> Result<Vec<AttentionRow>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, kind, ref, reason, raised_at FROM attention
             WHERE cleared_at IS NULL AND acked_at IS NULL ORDER BY raised_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AttentionRow {
                session: r.get(0)?,
                kind: r.get(1)?,
                reference: r.get(2)?,
                reason: r.get(3)?,
                raised_at: r.get::<_, i64>(4)? as u64,
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

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(read_version(&self.db)?.unwrap_or(0))
    }
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

/// 依版本逐步升級；v0（空資料庫）直接建 v1。
fn migrate(db: &Connection) -> Result<(), StoreError> {
    match read_version(db)? {
        Some(v) if v > SCHEMA_VERSION => Err(StoreError::FutureSchema(v)),
        Some(_) => Ok(()),
        None => {
            db.execute_batch(SCHEMA_V1)?;
            db.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )?;
            Ok(())
        }
    }
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
    tx.execute(
        "INSERT OR IGNORE INTO events (session, seq, type, pos, ts_seen, offset) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![sid, e.seq as i64, e.kind, e.pos.map(|p| p as i64), ts, offset as i64],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO sessions (id, host, cwd, started_at, status_since) VALUES (?1, ?2, '', ?3, ?3)",
        params![sid, e.host, ts],
    )?;
    tx.execute(
        "UPDATE sessions SET last_seq = MAX(last_seq, ?2) WHERE id = ?1",
        params![sid, e.seq as i64],
    )?;

    // 不認得的 type 或格式不符的 payload 只建索引、不做彙總
    let Ok(Some(payload)) = e.payload_typed() else {
        return Ok(());
    };
    let cwd = cwd_of.get(sid).cloned().unwrap_or_default();
    let touch = |kind: &str, path: &str| {
        tx.execute(
            "INSERT INTO file_touch (cwd, path, session, kind, count, last_ts) VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT (cwd, path, session, kind) DO UPDATE SET count = count + 1, last_ts = excluded.last_ts",
            params![cwd, path, sid, kind, ts],
        )
    };
    let change = |tool: &str, path: &str, kind: &str, added: Option<u64>, removed: Option<u64>| {
        tx.execute(
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
            cwd_of.insert(sid.to_string(), p.cwd.clone());
            tx.execute(
                "UPDATE sessions SET cwd = ?2, transcript_path = COALESCE(?3, transcript_path),
                   model = COALESCE(?4, model), branch = COALESCE(?5, branch),
                   ended_at = NULL, end_reason = NULL, status = 'run', status_since = ?6 WHERE id = ?1",
                params![sid, p.cwd, p.transcript_path, p.model, p.branch, ts],
            )?;
        }
        Payload::SessionEnded(p) => {
            tx.execute(
                "UPDATE sessions SET ended_at = ?2, end_reason = ?3, status = 'ended', status_since = ?2 WHERE id = ?1",
                params![sid, ts, p.reason],
            )?;
        }
        Payload::SessionStatus(p) => {
            let status = serde_json::to_value(p.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            tx.execute(
                "UPDATE sessions SET status = ?2, status_since = ?3 WHERE id = ?1",
                params![sid, status, p.since as i64],
            )?;
        }
        Payload::PromptSubmitted(p) => {
            tx.execute(
                "UPDATE sessions SET first_prompt = COALESCE(first_prompt, ?2) WHERE id = ?1",
                params![sid, p.text],
            )?;
        }
        Payload::Usage(p) => {
            tx.execute(
                "UPDATE sessions SET cost_usd = cost_usd + ?2, thinking_tokens = thinking_tokens + ?3 WHERE id = ?1",
                params![sid, p.cost_usd, p.thinking as i64],
            )?;
        }
        Payload::GitOp(p) if p.op == "commit" && p.ok => {
            tx.execute(
                "UPDATE sessions SET commits = commits + 1 WHERE id = ?1",
                params![sid],
            )?;
        }
        Payload::AttentionRaised(p) => {
            let kind = serde_json::to_value(p.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            if p.kind == crate::event::AttentionKind::Danger {
                tx.execute(
                    "UPDATE sessions SET dangers = dangers + 1 WHERE id = ?1",
                    params![sid],
                )?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO attention (session, kind, ref, reason, raised_at, cleared_at, acked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
                params![sid, kind, p.reference, p.reason, ts],
            )?;
        }
        Payload::AttentionCleared(p) => {
            let kind = serde_json::to_value(p.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            tx.execute(
                "UPDATE attention SET cleared_at = ?4 WHERE session = ?1 AND kind = ?2 AND ref = ?3 AND cleared_at IS NULL",
                params![sid, kind, p.reference, ts],
            )?;
        }
        Payload::ToolResult(p) if !p.ok => {
            tx.execute(
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
                Some(p.content.lines().count() as u64),
                Some(0),
            )?;
            path_of_tool.insert((sid.to_string(), p.tool_use_id), p.path);
        }
        _ => {}
    }
    Ok(())
}
