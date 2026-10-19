//! store：Event 寫進 JSONL（一 session 一檔），索引寫進 SQLite（contracts.md §5）。
//!
//! 寫入順序固定是「先 JSONL、後索引」，所以當機時索引只會落後、不會超前；
//! [`Store::open`] 會截掉 JSONL 尾端沒寫完的半行，並把還沒進索引的完整行補進去。
//! 索引可以整個刪掉，下次開啟時從 JSONL 重建。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::event::{Event, Payload};

pub const SCHEMA_VERSION: u32 = 6;
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
/// 0 表示這個檔從頭讀、還沒有行產生 Event（重啟從第 1 行讀，#713）；表裡 `line_no` 是 NULL（不知道，從檔尾）的列不回傳。
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
    /// JSONL 刪不掉的 (session, 原因)：索引照留，下次再清（L-17）
    pub failed: Vec<(String, String)>,
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
CREATE TABLE IF NOT EXISTS sessions (
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
CREATE INDEX IF NOT EXISTS sessions_by_cwd ON sessions (cwd, started_at);

CREATE TABLE IF NOT EXISTS events (
  session TEXT NOT NULL,
  seq INTEGER NOT NULL,
  type TEXT NOT NULL,
  pos INTEGER,
  ts_seen INTEGER NOT NULL,
  offset INTEGER NOT NULL,
  PRIMARY KEY (session, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS events_by_pos ON events (session, pos) WHERE pos IS NOT NULL;

CREATE TABLE IF NOT EXISTS file_touch (
  cwd TEXT NOT NULL,
  path TEXT NOT NULL,
  session TEXT NOT NULL,
  kind TEXT NOT NULL,
  count INTEGER NOT NULL,
  last_ts INTEGER NOT NULL,
  PRIMARY KEY (cwd, path, session, kind)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS changes (
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
CREATE INDEX IF NOT EXISTS changes_by_ts ON changes (cwd, ts DESC);

CREATE TABLE IF NOT EXISTS attention (
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

/// v4：全專案回放（#702）依跨 session 順序鍵 (ts_seen, session, pos) 分頁，只索引進串流的 Event。
const EVENTS_BY_TS_V4: &str = "
CREATE INDEX IF NOT EXISTS events_by_ts ON events (ts_seen, session, pos) WHERE pos IS NOT NULL;
";

/// v6：拿掉 v4 的 events_by_ts（`/api/projects/events` 已移除，#742 D5：每筆帶 pos 的 Event 少一次 B-tree 插入）；
/// 加每小時費用基準用的 usage 索引（#755 E-S5）、依種類找某個 session 的 Event（#756 E-H3）、
/// prune 依 session 刪 file_touch（主鍵從 cwd 開始，沒有它要全表掃，#750 L-17）。資料不動。
const INDEXES_V6: &str = "
DROP INDEX IF EXISTS events_by_ts;
CREATE INDEX IF NOT EXISTS events_usage_ts ON events (ts_seen) WHERE type = 'usage';
CREATE INDEX IF NOT EXISTS events_by_type ON events (session, type, seq);
CREATE INDEX IF NOT EXISTS file_touch_by_session ON file_touch (session);
";

/// v5（#713）：`tails.line_no` 可為 NULL，把「不知道（從檔尾）」和「從頭讀、還沒有行產生 Event」（0）分開。
/// 既有的 0 全改成 NULL：舊資料維持從檔尾，不把 #684 之前從檔尾 tail 的 session 整份重讀。
const TAILS_V5: &str = "
CREATE TABLE tails_v5 (
  session TEXT NOT NULL,
  file TEXT NOT NULL,
  path TEXT,
  line_no INTEGER,
  PRIMARY KEY (session, file)
) WITHOUT ROWID;
INSERT INTO tails_v5 (session, file, path, line_no)
  SELECT session, file, path, NULLIF(line_no, 0) FROM tails;
DROP TABLE tails;
ALTER TABLE tails_v5 RENAME TO tails;
";

/// Event 帶 `line` 時更新那個檔的行號（同一個檔的行依序進來，直接覆寫；檔案被替換後行號變小也跟著變小）。
const UPSERT_TAIL_LINE: &str = "INSERT INTO tails (session, file, line_no) VALUES (?1, ?2, ?3)
  ON CONFLICT (session, file) DO UPDATE SET line_no = excluded.line_no";
/// session.started 記主檔路徑與起始行號（?3：從頭讀的檔是 0，從檔尾接的是 NULL，見 [`initial_tail_line`]）；
/// 換了檔就用新的起始行號（舊檔的行號對新檔沒有意義），同一個檔則保留已讀到的行號。
const UPSERT_TAIL_PATH: &str = "INSERT INTO tails (session, file, path, line_no) VALUES (?1, 'main', ?2, ?3)
  ON CONFLICT (session, file) DO UPDATE SET
    line_no = CASE WHEN tails.path IS excluded.path THEN tails.line_no ELSE excluded.line_no END, path = excluded.path";

/// session.started 時 `tails.line_no` 的起始值（#713）：startup／clear 的檔從頭 tail，記 0，
/// collector 重啟時從第 1 行讀，當掉前還沒 flush 的行補得回來；resume／compact／late 接的是從檔尾 tail 的檔，
/// 記 NULL（不知道讀到哪），重啟照舊從檔尾。late（沒收到 SessionStart，#710）的 tail 也是從檔尾開始：
/// 記 0 的話，讀到新行之前重啟會把整份歷史重讀進串流與費用（#741 D4）。
fn initial_tail_line(source: &str) -> Option<i64> {
    matches!(source, "startup" | "clear").then_some(0)
}

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
    /// sessions 表裡已有列的 session：每筆 Event 不必再 `INSERT OR IGNORE`（E-S8）。只在交易 commit 之後加入
    known_sessions: HashSet<String>,
    /// 已寫進 JSONL、還沒進索引的 (Event, offset)：索引寫入失敗時留到下次 flush
    unindexed: Vec<(Event, u64)>,
    /// tail 讀到的行（(session, file) → 行號），下次 flush 和 Event 同一個交易寫進 `tails`（#713）
    read_lines: BTreeMap<String, BTreeMap<String, u64>>,
    /// 開啟時 recover 讀了多少 bytes
    recover_bytes: u64,
    /// 寫不進去時 `pending` 最多留多少 bytes（序列化後），超過的丟最舊的（W-M10）
    pending_limit: u64,
    /// 因為超過 `pending_limit` 丟掉的 Event 數（啟動以來）
    dropped: u64,
    /// 開啟時補回的 `pending-*.jsonl`：寫進去之後才刪
    replayed: Vec<PathBuf>,
}

/// `pending` 的上限（W-M10）：磁碟滿、檔案被鎖很久時記憶體不會無限長
pub const PENDING_LIMIT_BYTES: u64 = 50 * 1024 * 1024;
/// 結束時還寫不進去的 hook Event 存在這種檔（`<home>/pending-<epoch ms>.jsonl`），下次開 store 補回
pub const PENDING_DUMP_PREFIX: &str = "pending-";

// ── 權限（S-6、contracts §5）：資料目錄與底下的檔只給目前使用者（Claude Code 自己的 transcript 也是 0600）

/// 建（或收緊）資料目錄：unix 0700；Windows 第一次建立時停用 ACL 繼承、只授權目前使用者，底下的檔沿用它。
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let existed = dir.exists();
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    if !existed {
        restrict_dir_acl(dir);
    }
    let _ = existed;
    Ok(())
}

/// `icacls <dir> /inheritance:r /grant:r <使用者>:(OI)(CI)F`：第一次建立時，與 serve 啟動時資料目錄在 %USERPROFILE%
/// 以外（[`tighten_existing_home`]）。繼承來的 ACE 拿掉後，底下沿用繼承的檔跟著只剩自己。失敗只寫 log。
#[cfg(windows)]
fn restrict_dir_acl(dir: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(user) = std::env::var("USERNAME") else {
        return;
    };
    let ok = std::process::Command::new("icacls")
        .arg(dir)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:(OI)(CI)F"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        crate::logging::log(&format!("store：收緊 {} 的 ACL 失敗", dir.display()));
    }
}

/// 資料目錄的 ACL 要不要收緊（Windows；#762 B-S3）：剛建立的一律要；已經存在的只有不在 %USERPROFILE% 底下時要
/// （那裡預設只有自己能讀；`C:\` 根目錄、別的磁碟會繼承 Users:(RX)、Authenticated Users:(M)）。不知道 profile 時也收緊。
/// 純函式，Linux 也測得到；比對照 `paths::within`（Windows 路徑不分大小寫）。
pub fn acl_restrict_needed(home: &Path, user_profile: Option<&Path>, created_now: bool) -> bool {
    created_now
        || !user_profile
            .is_some_and(|p| crate::paths::within(&p.to_string_lossy(), &home.to_string_lossy()))
}

/// serve 啟動時對已經存在的資料目錄再檢查一次（#762 B-S3）：使用者先手動建好、或舊版的子命令用一般的
/// create_dir_all 建出來的目錄，`create_private_dir` 不會再碰。只有 serve 起來時多跑一次 icacls，ensure 與 hook 不受影響。
pub fn tighten_existing_home(home: &Path) {
    #[cfg(windows)]
    if acl_restrict_needed(home, crate::config::user_home().as_deref(), false) {
        restrict_dir_acl(home);
    }
    let _ = home;
}

/// 以追加模式開檔；新建的檔是 0600（unix）。
pub fn open_private_append(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// 整個寫入一個檔（collector.json、collector.failed）：0600（unix），已存在的也收緊。
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut o = OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    restrict_file(path)?;
    f.write_all(data)
}

/// 別人建的檔（SQLite 的 index.sqlite）收緊成 0600；Windows 沿用資料目錄的 ACL。
pub fn restrict_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

/// [`Store::last_prompt_and_pos_reads`] 讀出來的 prompt 填進文字欄位。
pub fn fill_last_prompts(out: &mut HashMap<String, LastSeen>, prompts: Vec<Event>) {
    for e in prompts {
        let text = e.payload["text"].as_str().map(str::to_string);
        out.entry(e.session).or_default().0 = text;
    }
}

/// 開 store 之前先檢查會讓它起不來的情況（L-1）：index.sqlite 打不開、schema 比支援的新。唯讀、不拿 store 鎖，
/// 上一個 collector 還在寫的時候也可以查；serve 在綁 hook 埠之前呼叫，失敗的話一個 hook 都還沒收。
pub fn preflight(home: &Path) -> Result<(), StoreError> {
    let path = home.join("index.sqlite");
    if !path.exists() {
        return Ok(());
    }
    let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_meta: bool = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
        [],
        |r| r.get(0),
    )?;
    if !has_meta {
        return Ok(());
    }
    let v: Option<String> = db
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match v.and_then(|s| s.parse::<u32>().ok()) {
        Some(v) if v > SCHEMA_VERSION => Err(StoreError::FutureSchema(v)),
        _ => Ok(()),
    }
}

/// 鎖內查好的「要讀 JSONL 的哪些行」：依 session 分組的 offset，順序就是結果的順序（X1）。
#[derive(Clone, Debug, Default)]
pub struct Reads {
    groups: Vec<(String, Vec<i64>)>,
}

impl Reads {
    fn one(session: &str, offsets: Vec<i64>) -> Reads {
        Reads {
            groups: vec![(session.to_string(), offsets)],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|(_, o)| o.is_empty())
    }
}

/// 不拿 store 鎖讀 JSONL（X1、#740）：JSONL 只會 append，索引裡的 offset 一定指向已經寫完的行
/// （先寫 JSONL、後寫索引），所以 web 端在鎖內查好 [`Reads`]、放掉鎖再讀，主迴圈的寫入不必等它。
#[derive(Clone, Debug)]
pub struct EventsReader {
    events_dir: PathBuf,
}

impl EventsReader {
    pub fn read(&self, reads: &Reads) -> Result<Vec<Event>, StoreError> {
        let mut out = Vec::new();
        for (session, offsets) in &reads.groups {
            self.read_session(session, offsets, &mut out)?;
        }
        Ok(out)
    }

    /// 一個 session 檔裡依序讀這些 offset 的行（E-S3）：offset 多半是接續或往後的，不 seek 就不會丟掉
    /// BufReader 的緩衝（`seek` 一定會清空它，原本每行都重讀 8 KB）；行的 String 重複使用。
    fn read_session(
        &self,
        session: &str,
        offsets: &[i64],
        out: &mut Vec<Event>,
    ) -> Result<(), StoreError> {
        if offsets.is_empty() {
            return Ok(());
        }
        let file = File::open(self.events_dir.join(file_name(session)))?;
        let mut file = BufReader::with_capacity(64 * 1024, file);
        let mut at: u64 = 0;
        let mut line = String::new();
        for &off in offsets {
            let off = off.max(0) as u64;
            if off != at {
                // 往前、或往後在緩衝內：seek_relative 不清空緩衝
                file.seek_relative(off as i64 - at as i64)?;
            }
            line.clear();
            let n = file.read_line(&mut line)?;
            at = off + n as u64;
            if let Ok(e) = Event::from_json_line(line.trim_end()) {
                out.push(e);
            }
        }
        Ok(())
    }
}

impl Store {
    /// 鎖外讀 JSONL 用的讀取器（X1）。
    pub fn reader(&self) -> EventsReader {
        EventsReader {
            events_dir: self.events_dir.clone(),
        }
    }

    /// [`Store::stream_before`] 的 offset（鎖內）；交給 [`EventsReader::read`]。
    pub fn stream_before_reads(
        &self,
        session: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Reads, StoreError> {
        let before = before.map_or(i64::MAX, |b| b as i64);
        let mut stmt = self.db.prepare_cached(
            "SELECT offset FROM events WHERE session = ?1 AND pos IS NOT NULL AND pos < ?2
             ORDER BY pos DESC LIMIT ?3",
        )?;
        let mut offsets: Vec<i64> = stmt
            .query_map(params![session, before, limit as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        offsets.reverse();
        Ok(Reads::one(session, offsets))
    }

    /// 開啟（或建立）資料目錄：`<home>/events/`、`<home>/index.sqlite`。
    pub fn open(home: &Path) -> Result<Store, StoreError> {
        let events_dir = home.join("events");
        create_private_dir(home)?;
        create_private_dir(&events_dir)?;
        let db = Connection::open(home.join("index.sqlite"))?;
        // -wal、-shm 由 SQLite 照資料庫檔的權限建立
        restrict_file(&home.join("index.sqlite"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        // index_event 一筆可能用到二十幾句不同的 SQL，預設的 16 個快取不夠（E-S8）
        db.set_prepared_statement_cache_capacity(32);
        let backfill_tails = migrate(&db)?;

        let mut store = Store {
            events_dir,
            db,
            pending: Vec::new(),
            file_len: HashMap::new(),
            cwd_of: HashMap::new(),
            path_of_tool: HashMap::new(),
            known_sessions: HashSet::new(),
            unindexed: Vec::new(),
            read_lines: BTreeMap::new(),
            recover_bytes: 0,
            pending_limit: PENDING_LIMIT_BYTES,
            dropped: 0,
            replayed: Vec::new(),
        };
        {
            let mut stmt = store.db.prepare("SELECT id, cwd FROM sessions")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, cwd) = row?;
                store.known_sessions.insert(id.clone());
                store.cwd_of.insert(id, cwd);
            }
        }
        if backfill_tails {
            store.backfill_tails()?;
        }
        store.recover()?;
        store.replay_pending_dumps(home);
        Ok(store)
    }

    /// 上次結束時沒寫進去的 hook Event（[`Store::dump_pending`]）補回去（W-M10）。在 recover 之後、
    /// serve 讀計數之前做，seq 才接得上。補不進去就留著檔，之後哪次 flush 成功再刪。
    fn replay_pending_dumps(&mut self, home: &Path) {
        let Ok(dir) = fs::read_dir(home) else {
            return;
        };
        let mut files: Vec<PathBuf> = dir
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with(PENDING_DUMP_PREFIX) && n.ends_with(".jsonl")
                })
            })
            .collect();
        files.sort();
        for f in files {
            match fs::read_to_string(&f) {
                Ok(text) => {
                    self.pending
                        .extend(text.lines().filter_map(|l| Event::from_json_line(l).ok()));
                    self.replayed.push(f);
                }
                Err(e) => crate::logging::log(&format!("store：讀不到 {}：{e}", f.display())),
            }
        }
        if self.replayed.is_empty() {
            return;
        }
        let n = self.pending.len();
        match self.flush() {
            Ok(()) => {
                crate::logging::log(&format!("store：補回上次結束時沒寫進去的 {n} 筆 hook 事件"))
            }
            Err(e) => crate::logging::log(&format!(
                "store：上次結束時沒寫進去的 {n} 筆 hook 事件還是寫不進去（{e}），檔案留著，之後再試"
            )),
        }
    }

    /// `pending` 的上限（bytes，測試用）。
    pub fn set_pending_limit(&mut self, bytes: u64) {
        self.pending_limit = bytes;
    }

    /// 還沒寫進 JSONL 的 Event 數。
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 啟動以來因 `pending` 超過上限丟掉的 Event 數。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 結束時 flush 還失敗：把 hook 來的 Event（`line` 是 null）寫進 `<home>/pending-<時間>.jsonl`，下次開 store
    /// 補回（W-M10）。transcript 來的不寫：寫不進 JSONL 時它們的行號不 commit（C15），重啟時 tail 會重讀，
    /// 寫進去會變兩份。回傳（存下的, 略過的 transcript Event）；寫不了檔回錯誤，呼叫端寫 log 說丟了幾筆。
    pub fn dump_pending(&mut self, home: &Path) -> io::Result<(usize, usize)> {
        let (hook, transcript): (Vec<&Event>, Vec<&Event>) =
            self.pending.iter().partition(|e| e.line.is_none());
        let counts = (hook.len(), transcript.len());
        if !hook.is_empty() {
            let mut buf = String::new();
            for e in hook {
                buf.push_str(&e.to_json_line());
                buf.push('\n');
            }
            let path = home.join(format!("{PENDING_DUMP_PREFIX}{}.jsonl", crate::now_ms()));
            write_private(&path, buf.as_bytes())?;
        }
        // 補回來又沒寫進去的那些也在這次的檔裡了，舊檔不刪的話下次會補兩次
        for f in std::mem::take(&mut self.replayed) {
            let _ = fs::remove_file(f);
        }
        self.pending.clear();
        Ok(counts)
    }

    /// v2 → v3 升級時掃一次全部 JSONL，回填每個 transcript 檔讀到的行號（只在升級那次做）。
    fn backfill_tails(&mut self) -> Result<(), StoreError> {
        let mut last: BTreeMap<(String, String), u64> = BTreeMap::new();
        let mut paths: BTreeMap<String, (String, Option<i64>)> = BTreeMap::new();
        for entry in fs::read_dir(&self.events_dir)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            // 讀不到的檔、不是 UTF-8 的行略過，不讓回填失敗（L-14）
            let file = match File::open(&path) {
                Ok(f) => f,
                Err(e) => {
                    crate::logging::log(&format!(
                        "store：回填 tails 時讀不到 {}：{e}",
                        path.display()
                    ));
                    continue;
                }
            };
            for line in BufReader::new(file).split(b'\n') {
                let Ok(line) = line else { break };
                let Ok(e) = Event::from_json_line(String::from_utf8_lossy(&line).trim_end()) else {
                    continue;
                };
                if let Some(l) = &e.line {
                    last.insert((e.session.clone(), l.file.clone()), l.no);
                }
                if e.kind == "session.started"
                    && let Some(p) = e.payload["transcriptPath"].as_str()
                {
                    let source = e.payload["source"].as_str().unwrap_or_default();
                    paths.insert(
                        e.session.clone(),
                        (p.to_string(), initial_tail_line(source)),
                    );
                }
            }
        }
        let tx = self.db.transaction()?;
        for (sid, (p, start)) in &paths {
            tx.execute(UPSERT_TAIL_PATH, params![sid, p, start])?;
        }
        for ((sid, file), no) in &last {
            exec(&tx, UPSERT_TAIL_LINE, params![sid, file, *no as i64])?;
        }
        tx.execute("DELETE FROM meta WHERE key = 'backfill_tails'", [])?;
        tx.commit()?;
        Ok(())
    }

    /// 先放進緩衝；呼叫端定期 [`Store::flush`]（store task 每 100 ms 一次）。
    pub fn append(&mut self, e: Event) {
        self.pending.push(e);
    }

    /// tail 讀到第 `no` 行（不論這行有沒有產生 Event）：下次 flush 寫進 `tails`，重啟從它之後接著讀（#713）。
    /// 和同一批 Event 同一個交易寫入，當掉時位置不會跑在 Event 前面。
    /// 每行都會呼叫：已有的 (session, file) 只更新行號，不配置字串（E-S10）。
    pub fn saw_line(&mut self, session: &str, file: &str, no: u64) {
        if let Some(n) = self
            .read_lines
            .get_mut(session)
            .and_then(|m| m.get_mut(file))
        {
            *n = no;
            return;
        }
        self.read_lines
            .entry(session.to_string())
            .or_default()
            .insert(file.to_string(), no);
    }

    /// 寫入失敗時不丟資料（#683）：JSONL 寫不進去的 Event 放回緩衝、寫到一半的檔截回原長度；
    /// 已寫進 JSONL 但索引交易失敗的留在 `unindexed`，下次 flush 再索引。
    pub fn flush(&mut self) -> Result<(), StoreError> {
        if self.pending.is_empty() && self.unindexed.is_empty() && self.read_lines.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);

        // 1. 依 session 分組寫 JSONL（每檔一次 write_all），記下每行的 offset
        let mut batches: Vec<Batch> = Vec::new();
        let mut members: Vec<Vec<usize>> = Vec::new();
        let mut offset_of = vec![0u64; pending.len()];
        let mut size_of = vec![0u64; pending.len()];
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
            let line = e.to_json_line();
            size_of[i] = line.len() as u64 + 1;
            batch.buf.extend_from_slice(line.as_bytes());
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
        let mut back = Vec::new();
        for (i, e) in pending.into_iter().enumerate() {
            if written[i] {
                self.unindexed.push((e, offset_of[i]));
            } else {
                back.push((e, size_of[i]));
            }
        }
        // 放回 pending 的超過上限：丟最舊的、計數（W-M10）
        let mut total: u64 = back.iter().map(|(_, n)| n).sum();
        let mut skip = 0;
        while total > self.pending_limit && skip < back.len() {
            total -= back[skip].1;
            skip += 1;
        }
        if skip > 0 {
            self.dropped += skip as u64;
            crate::logging::log(&format!(
                "store：寫不進去的待寫資料超過 {} KB，丟掉最舊的 {skip} 筆（啟動以來共 {} 筆）",
                self.pending_limit / 1024,
                self.dropped
            ));
        }
        self.pending
            .extend(back.into_iter().skip(skip).map(|(e, _)| e));

        // 2. 再寫索引（同一個交易）；失敗時交易回滾，整批留到下次
        // 有 Event 沒寫進 JSONL（放回 pending）時，tail 讀到的行號先不記（C15）：記了之後在下次成功前當掉，
        // 重啟會從這個行號之後讀，沒寫進去的那些行就永遠不會再進來。留到寫成功的那次一起記
        let lines_ok = failure.is_none();
        let tx = self.db.transaction()?;
        let mut batch = IndexBatch::new(&self.known_sessions, lines_ok.then_some(&self.read_lines));
        for (e, off) in &self.unindexed {
            index_event(
                &tx,
                &mut self.cwd_of,
                &mut self.path_of_tool,
                &mut batch,
                e,
                *off,
            )?;
        }
        batch.finish(&tx)?;
        let added = batch.added;
        if lines_ok {
            for (sid, files) in &self.read_lines {
                for (file, no) in files {
                    exec(&tx, UPSERT_TAIL_LINE, params![sid, file, *no as i64])?;
                }
            }
        }
        tx.commit()?;
        self.known_sessions.extend(added);
        self.unindexed.clear();
        if lines_ok {
            self.read_lines.clear();
        }
        match failure {
            Some(e) => Err(e.into()),
            None => {
                // 補回的 pending-*.jsonl 都寫進去了
                for f in std::mem::take(&mut self.replayed) {
                    let _ = fs::remove_file(f);
                }
                Ok(())
            }
        }
    }

    /// 追加一批到 session 檔；寫到一半失敗就截回原長度，不留半行。
    fn write_batch(&mut self, b: &Batch) -> io::Result<()> {
        let path = self.events_dir.join(&b.name);
        let mut f = open_private_append(&path)?;
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
            // 一個檔讀不到（被防毒或備份軟體鎖住）時跳過它，不讓整個 store 開不了（W-L8）
            match self.recover_file(&path) {
                Err(StoreError::Io(e)) => {
                    crate::logging::log(&format!("store：讀不到 {}，先跳過：{e}", path.display()));
                }
                other => other?,
            }
        }
        Ok(())
    }

    fn recover_file(&mut self, path: &Path) -> Result<(), StoreError> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let len = fs::metadata(path)?.len();
        let Some(session) = first_session(path)? else {
            // 一行完整的 Event 都還沒有：截掉沒寫完的半行，新的寫入才不會黏在後面（L-10）
            let bytes = fs::read(path)?;
            let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            if (complete as u64) < len {
                OpenOptions::new()
                    .write(true)
                    .open(path)?
                    .set_len(complete as u64)?;
            }
            self.file_len.insert(name, complete as u64);
            return Ok(());
        };
        {
            let indexed: Option<i64> = self.db.query_row(
                "SELECT MAX(offset) FROM events WHERE session = ?1",
                params![session],
                |r| r.get(0),
            )?;
            // 索引超前 JSONL（檔案被截短）時從頭讀
            let from = indexed.map(|o| o as u64).filter(|o| *o < len).unwrap_or(0);
            let mut file = File::open(path)?;
            file.seek(SeekFrom::Start(from))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            self.recover_bytes += bytes.len() as u64;
            let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            let end = from + complete as u64;
            if end < len {
                OpenOptions::new().write(true).open(path)?.set_len(end)?;
            }
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
            let mut batch = IndexBatch::new(&self.known_sessions, None);
            for (e, off) in &lines {
                if indexed.is_none_or(|max| (*off as i64) > max) {
                    index_event(
                        &tx,
                        &mut self.cwd_of,
                        &mut self.path_of_tool,
                        &mut batch,
                        e,
                        *off,
                    )?;
                }
            }
            batch.finish(&tx)?;
            let added = batch.added;
            tx.commit()?;
            self.known_sessions.extend(added);
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
        self.reader()
            .read(&self.stream_before_reads(session, before, limit)?)
    }

    /// 主頁時間軸回放（#729、#742）：專案根 `root` 底下（含 worktree、子目錄；cwd 的 `\` 和 `/api/sessions` 一樣當 `/`，C11）
    /// 在 [`since`, `until`] 之間還會出現在 live 的 session（開始不晚於 `until`，沒結束或結束不早於 `since`）的 Event，
    /// 與這些 session 注意項目的確認時間。
    ///
    /// 只讀到「本頁」為止（E-L1）：`at` 之後第 `limit` 個 Event 那個時間點的下一個時間點是本頁截止 `cut`
    /// （回放放滿 `limit` 個就在時間點邊界停下，讀到 `cut` 為止就夠判斷還有沒有下一段）。每個 session 只讀到
    /// 第一個 `ts_seen ≥ cut` 的 Event 之前（依 seq 的前綴：ts_seen 偶爾倒退也不會跳過中間的）。
    /// 回傳的 Event 依 session、seq 排（跨 session 的合併由 [`crate::timeline::replay_page`] 做），以及 `cut`（沒截時 None）。
    #[allow(clippy::type_complexity)]
    pub fn timeline(
        &self,
        root: &str,
        since: u64,
        at: u64,
        until: u64,
        limit: usize,
    ) -> Result<(Vec<Event>, Vec<crate::timeline::Ack>, Option<u64>), StoreError> {
        let (reads, acks, cut) = self.timeline_reads(root, since, at, until, limit)?;
        Ok((self.reader().read(&reads)?, acks, cut))
    }

    /// [`Store::timeline`] 的鎖內部分（X1、C3）：Event 的 offset、確認時間與 `cut`；Event 由 [`EventsReader`] 鎖外讀。
    #[allow(clippy::type_complexity)]
    pub fn timeline_reads(
        &self,
        root: &str,
        since: u64,
        at: u64,
        until: u64,
        limit: usize,
    ) -> Result<(Reads, Vec<crate::timeline::Ack>, Option<u64>), StoreError> {
        // 依平台的大小寫規則比對（L-11）：Windows、macOS 兩邊都轉小寫（SQLite 的 lower 只動 ASCII，和 paths 相同）
        let root = cwd_key(root);
        let cwd = cwd_expr("s.cwd", crate::paths::CASE_INSENSITIVE);
        let ms = |t: u64| t.min(i64::MAX as u64) as i64;
        let ids: Vec<String> = self
            .db
            .prepare(&format!(
                "SELECT id FROM sessions s
                 WHERE ({cwd} = ?1 OR substr({cwd}, 1, length(?1) + 1) = ?1 || '/')
                   AND s.started_at <= ?2 AND (s.ended_at IS NULL OR s.ended_at >= ?3)
                 ORDER BY id"
            ))?
            .query_map(params![root, ms(until), ms(since)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if ids.is_empty() {
            return Ok((Reads::default(), Vec::new(), None));
        }
        // 兩個 IN 清單用同一組編號（?1..?n）；混用匿名的 ? 會接著往後編號
        let list = (1..=ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut args: Vec<rusqlite::types::Value> =
            ids.iter().map(|id| id.clone().into()).collect();
        args.push(ms(at).into());
        args.push((limit.max(1) as i64 - 1).into());
        let n = ids.len();
        let cut: Option<i64> = self
            .db
            .prepare(&format!(
                "SELECT MIN(ts_seen) FROM events WHERE session IN ({list}) AND ts_seen > (
                   SELECT ts_seen FROM events WHERE session IN ({list}) AND ts_seen > ?{a}
                   ORDER BY ts_seen LIMIT 1 OFFSET ?{o})",
                a = n + 1,
                o = n + 2
            ))?
            .query_row(rusqlite::params_from_iter(args.iter()), |r| r.get(0))
            .optional()?
            .flatten();
        let cut = cut.map(|c| c as u64).filter(|c| *c <= until);
        let stop = cut.map_or(ms(until).saturating_add(1), ms);
        // 每個 session 一次的查詢只 prepare 一次；確認時間一次查完（E-S11）
        let mut reads = Reads::default();
        let mut stmt = self.db.prepare_cached(
            "SELECT offset FROM events WHERE session = ?1
               AND seq < COALESCE((SELECT MIN(seq) FROM events WHERE session = ?1 AND ts_seen >= ?2), 9223372036854775807)
             ORDER BY seq",
        )?;
        for id in &ids {
            let offsets: Vec<i64> = stmt
                .query_map(params![id, stop], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            reads.groups.push((id.clone(), offsets));
        }
        let acks = self
            .db
            .prepare(&format!(
                "SELECT session, kind, ref, acked_at FROM attention
                 WHERE session IN ({list}) AND acked_at IS NOT NULL ORDER BY session, kind, ref"
            ))?
            .query_map(rusqlite::params_from_iter(args[..n].iter()), |r| {
                Ok(crate::timeline::Ack {
                    session: r.get(0)?,
                    kind: r.get(1)?,
                    reference: r.get(2)?,
                    at: r.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((reads, acks, cut))
    }

    /// 某 session 在 seq 範圍（含兩端）內的所有 Event，依 seq 排序。
    pub fn events_by_seq(
        &self,
        session: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<Event>, StoreError> {
        self.reader()
            .read(&self.events_by_seq_reads(session, from, to)?)
    }

    /// [`Store::events_by_seq`] 的 offset（鎖內）。
    pub fn events_by_seq_reads(
        &self,
        session: &str,
        from: u64,
        to: u64,
    ) -> Result<Reads, StoreError> {
        let mut stmt = self.db.prepare_cached(
            "SELECT offset FROM events WHERE session = ?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq",
        )?;
        let offsets: Vec<i64> = stmt
            .query_map(params![session, from as i64, to as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(Reads::one(session, offsets))
    }

    /// 某 session 在 seq 範圍（含兩端）內、不帶 pos、種類是 `kinds` 之一的 Event，依 seq 排序。
    /// 「載入更多」只需要伴隨 Event 與狀態行，不用把範圍內的每一行都讀出來解析（E-L9）。
    pub fn unplaced_by_seq<'a>(
        &self,
        session: &str,
        from: u64,
        to: u64,
        kinds: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<Event>, StoreError> {
        self.reader()
            .read(&self.unplaced_by_seq_reads(session, from, to, kinds)?)
    }

    /// [`Store::unplaced_by_seq`] 的 offset（鎖內）。
    pub fn unplaced_by_seq_reads<'a>(
        &self,
        session: &str,
        from: u64,
        to: u64,
        kinds: impl IntoIterator<Item = &'a str>,
    ) -> Result<Reads, StoreError> {
        let kinds: Vec<&str> = kinds.into_iter().collect();
        let list = (0..kinds.len())
            .map(|i| format!("?{}", i + 4))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = self.db.prepare(&format!(
            "SELECT offset FROM events WHERE session = ?1 AND seq BETWEEN ?2 AND ?3
               AND pos IS NULL AND type IN ({list}) ORDER BY seq"
        ))?;
        let mut args: Vec<rusqlite::types::Value> = vec![
            session.to_string().into(),
            (from.min(i64::MAX as u64) as i64).into(),
            (to.min(i64::MAX as u64) as i64).into(),
        ];
        args.extend(
            kinds
                .iter()
                .map(|k| rusqlite::types::Value::from(k.to_string())),
        );
        let offsets: Vec<i64> = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(Reads::one(session, offsets))
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
        // 依主鍵（session, seq）讀：同一個 session 的列連在一起，每個檔只開一次（E-S4）；最後再依時間排
        let mut stmt = self.db.prepare(
            "SELECT session, offset FROM events WHERE ts_seen >= ?1 ORDER BY session, seq",
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
        let mut stmt = self.db.prepare_cached(USAGE_SINCE)?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![since as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut by_session: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for (session, off) in rows {
            by_session.entry(session).or_default().push(off);
        }
        let mut out = Vec::new();
        for (session, mut offsets) in by_session {
            // 同一個 session 的 offset 隨 seq 遞增：依 offset 排就是原本的 seq 順序
            offsets.sort_unstable();
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

    /// [`Store::usage_since`] 的查詢計畫（測試確認走 v6 的 `events_usage_ts`，E-S5）
    pub fn usage_since_plan(&self) -> Result<String, StoreError> {
        let mut stmt = self
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {USAGE_SINCE}"))?;
        let rows: Vec<String> = stmt
            .query_map(params![0i64], |r| r.get(3))?
            .collect::<Result<_, _>>()?;
        Ok(rows.join("\n"))
    }

    fn read_at(&self, session: &str, offsets: Vec<i64>) -> Result<Vec<Event>, StoreError> {
        let mut out = Vec::with_capacity(offsets.len());
        self.reader().read_session(session, &offsets, &mut out)?;
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
        // 和 crate::paths 同一套比對（L-11）：`e:\x` 與 `E:/x` 是同一個專案，Windows、macOS 不分大小寫
        let c = cwd_expr("cwd", crate::paths::CASE_INSENSITIVE);
        let mut stmt = self.db.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE ?1 IS NULL OR {c} = ?1
               OR substr({c}, 1, length(?1) + 1) = ?1 || '/'
             ORDER BY started_at DESC, id LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![cwd.map(cwd_key), limit as i64], session_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 所有 session 的 `cwd`（依 [`crate::paths::normalize`] 正規化、不重複）。
    pub fn cwds(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT DISTINCT {} FROM sessions WHERE cwd != ''",
            cwd_expr("cwd", false)
        ))?;
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

    /// [`Store::last_prompt_and_pos`] 的鎖內部分：pos 已經填好，prompt 的文字要讀 [`Reads`] 後用
    /// [`fill_last_prompts`] 補上。兩句查詢批次取。
    pub fn last_prompt_and_pos_reads(
        &self,
        ids: &[String],
    ) -> Result<(HashMap<String, LastSeen>, Reads), StoreError> {
        let mut out: HashMap<String, LastSeen> = HashMap::new();
        if ids.is_empty() {
            return Ok((out, Reads::default()));
        }
        let list = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
        let mut stmt = self.db.prepare_cached(LAST_PROMPT)?;
        let prompts: Vec<(String, i64)> = stmt
            .query_map(params![list], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let reads = Reads {
            groups: prompts.into_iter().map(|(s, o)| (s, vec![o])).collect(),
        };
        let mut stmt = self.db.prepare_cached(LAST_POS)?;
        let rows: Vec<(String, Option<i64>)> = stmt
            .query_map(params![list], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (session, pos) in rows {
            if let Some(pos) = pos {
                out.entry(session).or_default().1 = Some(pos as u64);
            }
        }
        Ok((out, reads))
    }

    /// [`Store::last_prompt_and_pos_reads`] 兩句查詢的查詢計畫（測試確認走索引，E-H3）
    pub fn last_prompt_and_pos_plans(&self) -> Result<(String, String), StoreError> {
        let plan = |q: &str| -> Result<String, StoreError> {
            let mut stmt = self.db.prepare(&format!("EXPLAIN QUERY PLAN {q}"))?;
            let rows: Vec<String> = stmt
                .query_map(params!["[]"], |r| r.get(3))?
                .collect::<Result<_, _>>()?;
            Ok(rows.join("\n"))
        };
        Ok((plan(LAST_PROMPT)?, plan(LAST_POS)?))
    }

    /// 這些 session 各自最後一個 `prompt.submitted` 的文字與串流最後一個 `pos`（#683，Sessions tab）。
    pub fn last_prompt_and_pos(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, LastSeen>, StoreError> {
        let (mut out, reads) = self.last_prompt_and_pos_reads(ids)?;
        fill_last_prompts(&mut out, self.reader().read(&reads)?);
        Ok(out)
    }

    /// 某 session 這些 type 的 Event，依 seq 排序。
    pub fn events_of_types(&self, session: &str, types: &[&str]) -> Result<Vec<Event>, StoreError> {
        self.reader()
            .read(&self.events_of_types_reads(session, types)?)
    }

    /// [`Store::events_of_types`] 的 offset（鎖內）。
    pub fn events_of_types_reads(
        &self,
        session: &str,
        types: &[&str],
    ) -> Result<Reads, StoreError> {
        if types.is_empty() {
            return Ok(Reads::one(session, Vec::new()));
        }
        // 一次查完（E-S11），走 v6 的 events_by_type
        let list = (2..types.len() + 2)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = self.db.prepare_cached(&format!(
            "SELECT offset FROM events WHERE session = ?1 AND type IN ({list}) ORDER BY seq"
        ))?;
        let args = std::iter::once(session).chain(types.iter().copied());
        let offsets: Vec<i64> = stmt
            .query_map(rusqlite::params_from_iter(args), |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(Reads::one(session, offsets))
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
    /// 還沒結束的 session：(id, cwd)。分支視窗把它們歸到各自的 worktree（E-H9：不讀出完整的 sessions 列再過濾）。
    pub fn open_session_cwds(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT id, cwd FROM sessions WHERE ended_at IS NULL")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// 某 session 留下的改動：(path, kind, seq)，依 seq。被拒或失敗的已經從表裡刪掉（#682）。
    /// `/api/files` 的 session 基準用它，不必把整個 session 的 Edit／Write／tool.updated 都從 JSONL 讀出來（E-H8）。
    pub fn session_change_rows(
        &self,
        session: &str,
    ) -> Result<Vec<(String, String, u64)>, StoreError> {
        let mut stmt = self.db.prepare_cached(
            "SELECT path, kind, seq FROM changes WHERE session = ?1 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![session], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

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

    /// 清掉最後活動早於 `before`（epoch ms）的 session（#655）：**先刪 JSONL，刪成功才刪索引列**（L-17）。
    /// 刪不掉的（Windows 上被防毒或備份軟體鎖住）索引照留、記在 `failed`，下次再清；不會先刪了索引、
    /// 留下孤兒檔在重啟時被 recover 重新索引。刪檔之後、刪索引之前當掉的話，下次 prune 找不到檔也算刪掉。
    /// `sessions` 只列真的清掉的，呼叫端照這份 forget。
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
        let mut gone = Vec::new();
        for sid in old {
            let name = file_name(&sid);
            let path = self.events_dir.join(&name);
            let len = fs::metadata(&path).map_or(0, |m| m.len());
            match fs::remove_file(&path) {
                Ok(()) => report.bytes += len,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    report.failed.push((sid, e.to_string()));
                    continue;
                }
            }
            self.file_len.remove(&name);
            gone.push(sid);
        }
        if gone.is_empty() {
            return Ok(report);
        }
        let tx = self.db.transaction()?;
        for sid in &gone {
            exec(&tx, "DELETE FROM sessions WHERE id = ?1", params![sid])?;
            for table in ["events", "file_touch", "changes", "attention", "tails"] {
                exec(
                    &tx,
                    &format!("DELETE FROM {table} WHERE session = ?1"),
                    params![sid],
                )?;
            }
        }
        tx.commit()?;
        for sid in gone {
            self.cwd_of.remove(&sid);
            self.known_sessions.remove(&sid);
            self.path_of_tool.retain(|(s, _), _| *s != sid);
            report.sessions.push(sid);
        }
        Ok(report)
    }

    /// collector 重啟時交給 TailPlanner：每個 transcript 檔讀到第幾行（0＝從頭讀、還沒讀到行，重啟從第 1 行）。
    /// `line_no` 是 NULL 的列（resume／compact 從檔尾接的檔、或 v5 之前的舊列）不回傳：照舊從檔尾讀（#713）。
    pub fn tail_lines(&self) -> Result<Vec<TailRow>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT session, file, path, line_no FROM tails WHERE line_no IS NOT NULL ORDER BY session, file",
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

    /// 上次開啟時 recover 讀了多少 bytes 的 JSONL（測試與 healthz 用）。
    pub fn recover_bytes(&self) -> u64 {
        self.recover_bytes
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(read_version(&self.db)?.unwrap_or(0))
    }
}

/// SQL 裡和 [`crate::paths::normalize`] 相同的正規化（L-11）：`\` 換成 `/`、開頭的磁碟機代號大寫；
/// `fold`（Windows、macOS 比對用）時再整個轉小寫（SQLite 的 lower 只動 ASCII，和 paths 相同）。舊資料裡小寫的
/// 磁碟機代號也比得到。
fn cwd_expr(col: &str, fold: bool) -> String {
    let slash = format!("REPLACE({col}, '\\', '/')");
    let norm = format!(
        "CASE WHEN substr({slash}, 2, 1) = ':' THEN upper(substr({slash}, 1, 1)) || substr({slash}, 2) ELSE {slash} END"
    );
    if fold { format!("lower({norm})") } else { norm }
}

/// [`cwd_expr`] 要比的另一邊：正規化、去掉結尾的 `/`，依平台轉小寫。
fn cwd_key(p: &str) -> String {
    let p = crate::paths::normalize(p);
    let p = p.trim_end_matches('/');
    if crate::paths::CASE_INSENSITIVE {
        p.to_ascii_lowercase()
    } else {
        p.to_string()
    }
}

/// 每個 session 各找一次：`MAX(seq)` 在 `events_by_type (session, type, seq)` 上是一次索引查找（E-H3）。
/// 寫成 `WHERE session IN (...) GROUP BY session` 時 SQLite 沿主鍵把 session 的每筆 Event 都看過一遍；
/// 寫成 `ORDER BY seq DESC LIMIT 1` 時也沿主鍵倒著找，上一個 prompt 很遠（長時間的 agent 工作）就一樣慢。
/// 實測（bundled SQLite 3.53，200 個 session × 1000 筆）：原本 22–25 ms，這樣 0.4 ms。
const LAST_PROMPT: &str = "SELECT j.value, e.offset FROM json_each(?1) j
             JOIN events e ON e.session = j.value AND e.seq = (
               SELECT MAX(x.seq) FROM events x WHERE x.session = j.value AND x.type = 'prompt.submitted')";
/// 同上：`MAX(pos)` 在 `events_by_pos (session, pos)` 上一次查找；沒有 pos 的 session 是 NULL。
const LAST_POS: &str = "SELECT j.value, (SELECT MAX(e.pos) FROM events e WHERE e.session = j.value AND e.pos IS NOT NULL)
             FROM json_each(?1) j";

/// `type = 'usage' AND ts_seen >= ?` 剛好是 v6 部分索引 `events_usage_ts` 的形狀（E-S5）。
/// 不在 SQL 裡排序：`ORDER BY session, seq` 會讓 SQLite 改成沿主鍵全表掃，分組與排序在呼叫端做
const USAGE_SINCE: &str =
    "SELECT session, offset FROM events WHERE type = 'usage' AND ts_seen >= ?1";

/// JSONL 第一行的 session（檔名經過清理，不一定等於 session id）。
/// 檔案裡第一個完整、讀得懂的 Event 的 session。第一行可能斷在多位元組字元中間或寫壞了（L-10），
/// 往下找；最後沒有換行的半行不算。
fn first_session(path: &Path) -> Result<Option<String>, StoreError> {
    const TRIES: usize = 64;
    let mut r = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    for _ in 0..TRIES {
        line.clear();
        if r.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
            return Ok(None);
        }
        if let Ok(e) = Event::from_json_line(String::from_utf8_lossy(&line).trim_end()) {
            return Ok(Some(e.session));
        }
    }
    Ok(None)
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
/// 回傳要不要回填 tails：舊版資料庫升到 v3 時記下、回填完才清掉（新建的不用）。
fn migrate(db: &Connection) -> Result<bool, StoreError> {
    let from = read_version(db)?;
    let mut v = match from {
        Some(v) if v > SCHEMA_VERSION => return Err(StoreError::FutureSchema(v)),
        Some(v) => v,
        // 建表與版本號在同一個交易（L-14）：做到一半當掉的話整個沒做，下次重來；
        // 舊版留下的半套表靠 SCHEMA_V1 的 IF NOT EXISTS 補完
        None => {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch(SCHEMA_V1)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
                [],
            )?;
            tx.commit()?;
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
            // 舊的資料庫要從 JSONL 回填 tails：記在 meta，回填成功才刪，中途失敗下次開啟再做（L-14）。
            // 新建的不用（JSONL 由 recover 從頭索引，順便填好）
            if from.is_some() {
                tx.execute(BACKFILL_PENDING, [])?;
            }
        }
        if v == 3 {
            tx.execute_batch(EVENTS_BY_TS_V4)?;
        }
        if v == 4 {
            tx.execute_batch(TAILS_V5)?;
        }
        if v == 5 {
            tx.execute_batch(INDEXES_V6)?;
        }
        v += 1;
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            params![v.to_string()],
        )?;
        tx.commit()?;
    }
    let pending: Option<String> = db
        .query_row(
            "SELECT value FROM meta WHERE key = 'backfill_tails'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(pending.is_some())
}

const BACKFILL_PENDING: &str =
    "INSERT OR REPLACE INTO meta (key, value) VALUES ('backfill_tails', '1')";

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

/// 一次交易裡 [`index_event`] 共用的狀態（E-S8）：哪些 session 已有列、每個 session 最大的 seq、
/// 哪些 transcript 檔的行號等一下會整批寫。
struct IndexBatch<'a> {
    known: &'a HashSet<String>,
    /// 這次交易新建了列的 session；commit 之後才併進 `known`（回滾時不能當成已有）
    added: HashSet<String>,
    last_seq: HashMap<String, u64>,
    /// 這次 flush 最後會整批寫的 tail 行號（`read_lines`）：同一個檔不必每筆 Event 各寫一次
    lines_later: Option<&'a BTreeMap<String, BTreeMap<String, u64>>>,
}

impl<'a> IndexBatch<'a> {
    fn new(
        known: &'a HashSet<String>,
        lines_later: Option<&'a BTreeMap<String, BTreeMap<String, u64>>>,
    ) -> Self {
        IndexBatch {
            known,
            added: HashSet::new(),
            last_seq: HashMap::new(),
            lines_later,
        }
    }

    /// 每個 session 的 last_seq 只更新一次
    fn finish(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        for (sid, seq) in &self.last_seq {
            exec(
                tx,
                "UPDATE sessions SET last_seq = MAX(last_seq, ?2) WHERE id = ?1",
                params![sid, *seq as i64],
            )?;
        }
        Ok(())
    }
}

fn index_event(
    tx: &Transaction<'_>,
    cwd_of: &mut HashMap<String, String>,
    path_of_tool: &mut HashMap<(String, String), String>,
    batch: &mut IndexBatch<'_>,
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
        let later = batch
            .lines_later
            .and_then(|r| r.get(sid))
            .is_some_and(|m| m.contains_key(&l.file));
        if !later {
            exec(tx, UPSERT_TAIL_LINE, params![sid, l.file, l.no as i64])?;
        }
    }
    if !batch.known.contains(sid) && !batch.added.contains(sid) {
        exec(
            tx,
            "INSERT OR IGNORE INTO sessions (id, host, cwd, started_at, status_since) VALUES (?1, ?2, '', ?3, ?3)",
            params![sid, e.host, ts],
        )?;
        batch.added.insert(sid.to_string());
    }
    let max = batch.last_seq.entry(sid.to_string()).or_default();
    *max = (*max).max(e.seq);

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
            // status 只由 session.status 寫（不變式 8）：/compact 時 merge 的狀態可能還是 done（K-5）
            exec(tx,
                "UPDATE sessions SET cwd = CASE WHEN cwd = '' THEN ?2 ELSE cwd END, transcript_path = COALESCE(?3, transcript_path),
                   model = COALESCE(?4, model), branch = COALESCE(?5, branch),
                   ended_at = NULL, end_reason = NULL WHERE id = ?1",
                params![sid, p.cwd, p.transcript_path, p.model, p.branch],
            )?;
            if let Some(path) = &p.transcript_path {
                exec(
                    tx,
                    UPSERT_TAIL_PATH,
                    params![sid, path, initial_tail_line(&p.source)],
                )?;
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
            }
            // 「失敗」只算 tool.updated 的 failed、不是 Esc 也不是 PermissionDenied（D6、contracts §4）；
            // 檔案工具的那個檔記一次 fail。tool.result 的 ok=false 不另外算
            if p.is_failure() {
                exec(
                    tx,
                    "UPDATE sessions SET failures = failures + 1 WHERE id = ?1",
                    params![sid],
                )?;
                if let Some(path) = path_of_tool.get(&(sid.to_string(), p.tool_use_id.clone())) {
                    touch("fail", path)?;
                }
            }
            path_of_tool.remove(&(sid.to_string(), p.tool_use_id));
        }
        // 同一張卡片只有一個 tool.result（K-3），之後不會再用到它碰的檔；結果之前的 tool.updated 已經記過 fail（E-S12）
        Payload::ToolResult(p) => {
            path_of_tool.remove(&(sid.to_string(), p.tool_use_id));
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

/// store 寫入的退避與對外狀態（W-M10）。寫不進去時從 1 秒退避、加倍到 30 秒；log 只在開始失敗、
/// 每 5 分鐘、恢復時各一行（原本每個 hook、每 100 ms 一行，9 小時就把 5 個輪替檔灌滿）。時間由呼叫端給（測試注入）。
#[derive(Default)]
pub struct FlushGate {
    failing: Option<Failing>,
    next_try: u64,
    backoff: u64,
}

struct Failing {
    since: u64,
    message: String,
    reported: u64,
}

const BACKOFF_FIRST_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 30_000;
const REPORT_EVERY_MS: u64 = 5 * 60 * 1000;

impl FlushGate {
    /// 現在可以試著 flush（失敗中時要等退避時間到）。
    pub fn ready(&self, now: u64) -> bool {
        now >= self.next_try
    }

    /// 記下一次 flush 的結果；要寫的 log 回傳（沒有就 None）。
    pub fn record(
        &mut self,
        result: Result<(), String>,
        now: u64,
        pending: usize,
        dropped: u64,
    ) -> Option<String> {
        match (result, &mut self.failing) {
            (Ok(()), None) => None,
            (Ok(()), Some(f)) => {
                let secs = now.saturating_sub(f.since) / 1000;
                self.failing = None;
                self.next_try = 0;
                self.backoff = 0;
                Some(format!(
                    "store 恢復寫入（寫不進去了 {secs} 秒，待寫的都補上了）"
                ))
            }
            (Err(e), failing) => {
                self.backoff = (self.backoff * 2).clamp(BACKOFF_FIRST_MS, BACKOFF_MAX_MS);
                self.next_try = now + self.backoff;
                let drop_note = if dropped > 0 {
                    format!("、超過上限丟掉 {dropped} 筆")
                } else {
                    String::new()
                };
                match failing {
                    None => {
                        self.failing = Some(Failing {
                            since: now,
                            message: e.clone(),
                            reported: now,
                        });
                        Some(format!(
                            "store 寫不進去：{e}；待寫 {pending} 筆{drop_note}。之後退避重試（最長每 {} 秒），每 5 分鐘報一次",
                            BACKOFF_MAX_MS / 1000
                        ))
                    }
                    Some(f) => {
                        f.message = e.clone();
                        if now.saturating_sub(f.reported) < REPORT_EVERY_MS {
                            return None;
                        }
                        f.reported = now;
                        Some(format!(
                            "store 仍然寫不進去（{} 秒）：{e}；待寫 {pending} 筆{drop_note}",
                            now.saturating_sub(f.since) / 1000
                        ))
                    }
                }
            }
        }
    }

    /// `/healthz`、welcome、snapshot 的 `storeError`（contracts §6、§7）；正常時 None。
    pub fn error(&self, pending: usize, dropped: u64) -> Option<serde_json::Value> {
        self.failing.as_ref().map(|f| {
            serde_json::json!({
                "message": f.message, "since": f.since, "pending": pending, "dropped": dropped,
            })
        })
    }
}
