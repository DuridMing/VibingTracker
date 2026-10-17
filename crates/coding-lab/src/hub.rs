//! `coding-lab hub`（#663，PLAN「網路綁定」形態 C 與第 5 點）：本機對多台 collector 各開一條 WS 訂 `*`，
//! 合流後在本機 127.0.0.1 開自己的 web（免金鑰；前端就是同一份 `web/`）。
//!
//! hub **只轉發不存資料**：彙總列（sessions、agents、attention）照各台算好的，只把 `host` 改成上游名稱；
//! KPI 把各台的加總；串流每台內照上游給的順序、跨台依到達（不變式 3）；`more`、`ack`、`session:<id>` 訂閱與
//! `/api/*` 都轉給擁有那個 session／目錄的那台（Bearer 金鑰）。某台斷線時它的 session 標 `offline`、自動重連，
//! 重連後重送合併的 snapshot（contracts.md §6「hub」）。
//!
//! 結構：[`Hub`] 是純邏輯（上游訊息進、要給客戶端的 [`Out`] 出），[`serve`] 把它接上 WS／HTTP 與上游連線。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::event::Event;
use crate::live::{CHANGES_KEPT, merge_streams};

pub const DEFAULT_HUB_BIND: &str = "127.0.0.1:4802";
/// snapshot 之後累積的 Event 上限（給新連上的客戶端當 snapshot 用；前端本來就只留 3000 行）
const SINCE_MAX: usize = 5000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// `[[hub.upstream]]`：一台 collector。`key` 省略時只適合本機或 SSH 通道（那台 `auth = "local-open"`）。
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Upstream {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub key: Option<String>,
}

/// `http://host:port[/]` → `host:port`。TLS 不內建（只接受 http://）：遠端請走 tailnet 或 SSH 通道。
fn host_port(http_url: &str) -> Result<String, String> {
    let rest = http_url.strip_prefix("http://").ok_or_else(|| {
        format!("上游網址要是 http://主機:埠（TLS 不內建，遠端請走 tailnet 或 SSH 通道），收到 {http_url}")
    })?;
    let hp = rest.split('/').next().unwrap_or_default();
    if hp.is_empty() || rest.trim_end_matches('/') != hp {
        return Err(format!(
            "上游網址只能是 http://主機:埠，不能帶路徑：{http_url}"
        ));
    }
    Ok(hp.to_string())
}

pub fn ws_url(http_url: &str) -> Result<String, String> {
    Ok(format!("ws://{}/ws", host_port(http_url)?))
}

// ── 純邏輯：合流 ───────────────────────────────────────────────────────────

/// 要給 hub 客戶端的東西
#[derive(Clone, Debug)]
pub enum Out {
    /// 給訂 `*` 的連線的 frame
    All(Value),
    /// 給訂 `session:<id>` 的連線（snapshot 或 frame）
    Session(String, Value),
    /// 某台（重新）連上：訂 `*` 的連線重拿合併後的 snapshot
    Resnapshot,
    /// 給每一條連線（`more` 的回覆、error）
    Any(Value),
}

#[derive(Default)]
struct UpCache {
    online: bool,
    /// `welcome.scope`（view 不能 ack）
    scope: String,
    sessions: BTreeMap<String, Value>,
    agents: BTreeMap<String, Value>,
    attention: BTreeMap<String, Value>,
    kpi: Map<String, Value>,
    changes: Vec<Value>,
    staged: Vec<Value>,
    /// 最近一次 `*` snapshot 的 stream（Event）
    stream: Vec<Event>,
}

pub struct Hub {
    names: Vec<String>,
    ups: HashMap<String, UpCache>,
    /// snapshot 之後到達的 Event（已改 host），跨台依到達
    since: VecDeque<(String, Value)>,
    frame_no: u64,
}

fn set_host(v: &mut Value, host: &str) {
    if let Some(o) = v.as_object_mut() {
        o.insert("host".into(), Value::String(host.into()));
    }
}

fn id_of(v: &Value) -> Option<String> {
    v.get("id").and_then(Value::as_str).map(str::to_string)
}

/// rows 的一種（sessions／agents／attention）依 `id` upsert；`deleted` 就移除；sessions 列順便改 host
fn upsert(map: &mut BTreeMap<String, Value>, rows: Option<&Value>, host: Option<&str>) {
    for row in rows.and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = id_of(row) else { continue };
        if row.get("deleted") == Some(&Value::Bool(true)) {
            map.remove(&id);
            continue;
        }
        let mut row = row.clone();
        if let Some(h) = host {
            set_host(&mut row, h);
        }
        match map.get_mut(&id) {
            Some(Value::Object(cur)) => {
                if let Value::Object(new) = row {
                    cur.extend(new);
                }
            }
            _ => {
                map.insert(id, row);
            }
        }
    }
}

fn add_num(total: &mut Map<String, Value>, key: &str, v: &Value) {
    let Some(n) = v.as_f64() else { return };
    let cur = total.get(key).and_then(Value::as_f64).unwrap_or(0.0) + n;
    let as_int = v.is_i64() || v.is_u64();
    let was_int = total.get(key).is_none_or(|t| t.is_i64() || t.is_u64());
    total.insert(
        key.into(),
        if as_int && was_int {
            json!(cur as i64)
        } else {
            json!(cur)
        },
    );
}

/// 幾台的 kpi 加總：數字欄位相加，`models` 依短名合併（costUsd、thinkingTokens 相加，ids 聯集）
fn sum_kpi<'a>(kpis: impl Iterator<Item = &'a Map<String, Value>>) -> Value {
    let mut total = Map::new();
    let mut models: BTreeMap<String, (f64, i64, Vec<String>)> = BTreeMap::new();
    let mut any_models = false;
    for k in kpis {
        for (key, v) in k {
            if key == "models" {
                any_models = true;
                for (name, m) in v.as_object().into_iter().flatten() {
                    let e = models.entry(name.clone()).or_default();
                    e.0 += m.get("costUsd").and_then(Value::as_f64).unwrap_or(0.0);
                    e.1 += m.get("thinkingTokens").and_then(Value::as_i64).unwrap_or(0);
                    for id in m.get("ids").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(s) = id.as_str()
                            && !e.2.iter().any(|x| x == s)
                        {
                            e.2.push(s.to_string());
                        }
                    }
                }
            } else {
                add_num(&mut total, key, v);
            }
        }
    }
    if any_models {
        let m: Map<String, Value> = models
            .into_iter()
            .map(|(name, (cost, think, mut ids))| {
                ids.sort();
                (
                    name,
                    json!({ "costUsd": cost, "thinkingTokens": think, "ids": ids }),
                )
            })
            .collect();
        total.insert("models".into(), Value::Object(m));
    }
    Value::Object(total)
}

impl Hub {
    pub fn new(names: Vec<String>) -> Hub {
        let ups = names
            .iter()
            .map(|n| (n.clone(), UpCache::default()))
            .collect();
        Hub {
            names,
            ups,
            since: VecDeque::new(),
            frame_no: 0,
        }
    }

    fn up(&mut self, name: &str) -> &mut UpCache {
        if !self.ups.contains_key(name) {
            self.names.push(name.to_string());
            self.ups.insert(name.to_string(), UpCache::default());
        }
        self.ups.get_mut(name).expect("剛放進去")
    }

    fn next_no(&mut self) -> u64 {
        self.frame_no += 1;
        self.frame_no
    }

    fn kpi_total(&self) -> Value {
        sum_kpi(self.ups.values().filter(|u| u.online).map(|u| &u.kpi))
    }

    pub fn on_welcome(&mut self, up: &str, welcome: &Value) {
        let scope = welcome["scope"].as_str().unwrap_or("operate").to_string();
        self.up(up).scope = scope;
    }

    pub fn can_ack(&self, up: &str) -> bool {
        self.ups.get(up).is_none_or(|u| u.scope != "view")
    }

    /// 每台上游是否連著（healthz 用）
    pub fn online(&self) -> Vec<(String, bool)> {
        self.names
            .iter()
            .map(|n| (n.clone(), self.ups.get(n).is_some_and(|u| u.online)))
            .collect()
    }

    /// 上游的一則 server → client 訊息
    pub fn on_message(&mut self, up: &str, msg: &Value) -> Vec<Out> {
        let ch = msg["ch"].as_str().unwrap_or("*").to_string();
        match msg["t"].as_str().unwrap_or_default() {
            "welcome" => {
                self.on_welcome(up, msg);
                Vec::new()
            }
            "snapshot" if ch == "*" => {
                self.on_all_snapshot(up, msg);
                vec![Out::Resnapshot]
            }
            "snapshot" => {
                let mut v = msg.clone();
                self.rewrite(up, &mut v);
                match ch.strip_prefix("session:") {
                    Some(id) => vec![Out::Session(id.to_string(), v)],
                    None => Vec::new(),
                }
            }
            "frame" if ch == "*" => vec![Out::All(self.on_all_frame(up, msg))],
            "frame" => {
                let mut v = msg.clone();
                self.rewrite(up, &mut v);
                match ch.strip_prefix("session:") {
                    Some(id) => vec![Out::Session(id.to_string(), v)],
                    None => Vec::new(),
                }
            }
            "more" | "error" => {
                let mut v = msg.clone();
                self.rewrite(up, &mut v);
                vec![Out::Any(v)]
            }
            _ => Vec::new(),
        }
    }

    /// 把一則訊息裡的 Event 與 sessions 列的 host 都改成上游名稱
    fn rewrite(&self, up: &str, v: &mut Value) {
        for key in ["events", "stream", "staged", "changes"] {
            for e in v
                .get_mut(key)
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                set_host(e, up);
            }
        }
        for s in v
            .get_mut("sessions")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            set_host(s, up);
        }
        for s in v
            .get_mut("rows")
            .and_then(|r| r.get_mut("sessions"))
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            set_host(s, up);
        }
    }

    fn on_all_snapshot(&mut self, up: &str, snap: &Value) {
        let mut v = snap.clone();
        self.rewrite(up, &mut v);
        self.since.retain(|(n, _)| n != up);
        let cache = self.up(up);
        cache.online = true;
        cache.sessions.clear();
        cache.agents.clear();
        cache.attention.clear();
        upsert(&mut cache.sessions, v.get("sessions"), Some(up));
        upsert(&mut cache.agents, v.get("agents"), None);
        upsert(&mut cache.attention, v.get("attention"), None);
        cache.kpi = v["kpi"].as_object().cloned().unwrap_or_default();
        cache.changes = v["changes"].as_array().cloned().unwrap_or_default();
        cache.staged = v["staged"].as_array().cloned().unwrap_or_default();
        cache.stream = v["stream"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| serde_json::from_value(e.clone()).ok())
            .collect();
    }

    fn on_all_frame(&mut self, up: &str, frame: &Value) -> Value {
        let mut v = frame.clone();
        self.rewrite(up, &mut v);
        let no = self.next_no();
        let events = v["events"].as_array().cloned().unwrap_or_default();
        let rows = v["rows"].as_object().cloned().unwrap_or_default();
        let had_kpi = rows.contains_key("kpi");
        {
            let cache = self.up(up);
            cache.online = true;
            upsert(&mut cache.sessions, rows.get("sessions"), Some(up));
            upsert(&mut cache.agents, rows.get("agents"), None);
            upsert(&mut cache.attention, rows.get("attention"), None);
            if let Some(Value::Object(k)) = rows.get("kpi") {
                cache.kpi.extend(k.clone());
            }
            for e in &events {
                if matches!(e["type"].as_str(), Some("file.edit" | "file.write")) {
                    cache.changes.push(e.clone());
                }
            }
            if cache.changes.len() > CHANGES_KEPT {
                let n = cache.changes.len() - CHANGES_KEPT;
                cache.changes.drain(..n);
            }
        }
        for e in &events {
            self.since.push_back((up.to_string(), e.clone()));
        }
        while self.since.len() > SINCE_MAX {
            self.since.pop_front();
        }
        let mut out_rows = rows;
        if had_kpi {
            out_rows.insert("kpi".into(), self.kpi_total());
        }
        json!({ "t": "frame", "frame": no, "ch": "*", "events": events, "rows": Value::Object(out_rows) })
    }

    /// 上游斷線：它的 session 標 `offline`（列留著，重連的 snapshot 才換掉），KPI 不再加總它
    pub fn on_offline(&mut self, up: &str) -> Vec<Out> {
        let no = self.next_no();
        let cache = self.up(up);
        if !cache.online {
            return Vec::new();
        }
        cache.online = false;
        let mut rows = Vec::new();
        for (id, row) in cache.sessions.iter_mut() {
            if let Some(o) = row.as_object_mut() {
                o.insert("offline".into(), Value::Bool(true));
            }
            rows.push(json!({ "id": id, "host": up, "offline": true }));
        }
        let kpi = self.kpi_total();
        vec![Out::All(json!({
            "t": "frame", "frame": no, "ch": "*", "events": [],
            "rows": { "sessions": rows, "kpi": kpi },
        }))]
    }

    pub fn owner_of_session(&self, id: &str) -> Option<String> {
        self.names
            .iter()
            .find(|n| {
                self.ups
                    .get(*n)
                    .is_some_and(|u| u.sessions.contains_key(id))
            })
            .cloned()
    }

    /// `cwd` 等於某台某個 session 的 cwd、在它底下（worktree、子目錄）或是它的上層：那台；多台符合取最長的
    pub fn owner_of_cwd(&self, cwd: &str) -> Option<String> {
        let want = cwd.replace('\\', "/");
        let want = want.trim_end_matches('/');
        let under = |p: &str, base: &str| p == base || p.starts_with(&format!("{base}/"));
        let mut best: Option<(usize, &str)> = None;
        for n in &self.names {
            let Some(u) = self.ups.get(n) else { continue };
            for s in u.sessions.values() {
                let Some(c) = s["cwd"].as_str() else { continue };
                let c = c.trim_end_matches('/');
                if under(want, c) || under(c, want) {
                    let len = c.len().min(want.len());
                    if best.is_none_or(|(l, _)| len > l) {
                        best = Some((len, n));
                    }
                }
            }
        }
        best.map(|(_, n)| n.to_string())
    }

    /// 給訂 `*` 的客戶端：所有上游合併後的 snapshot
    pub fn snapshot_all(&self) -> Value {
        let mut sessions = Vec::new();
        let mut agents = Vec::new();
        let mut attention = Vec::new();
        let mut staged = Vec::new();
        let mut changes: Vec<Value> = Vec::new();
        let mut per_session: BTreeMap<String, Vec<Event>> = BTreeMap::new();
        for n in &self.names {
            let Some(u) = self.ups.get(n) else { continue };
            sessions.extend(u.sessions.values().cloned());
            agents.extend(u.agents.values().cloned());
            attention.extend(u.attention.values().cloned());
            staged.extend(u.staged.iter().cloned());
            changes.extend(u.changes.iter().cloned());
            for e in &u.stream {
                per_session
                    .entry(format!("{n}\u{0}{}", e.session))
                    .or_default()
                    .push(e.clone());
            }
        }
        changes.sort_by_key(|c| c["ts_seen"].as_u64().unwrap_or(0));
        if changes.len() > CHANGES_KEPT {
            let n = changes.len() - CHANGES_KEPT;
            changes.drain(..n);
        }
        let mut stream: Vec<Value> = merge_streams(per_session.into_values().collect())
            .into_iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect();
        stream.extend(self.since.iter().map(|(_, e)| e.clone()));
        json!({
            "t": "snapshot", "ch": "*", "frame": self.frame_no,
            "sessions": sessions, "agents": agents, "attention": attention, "kpi": self.kpi_total(),
            "changes": changes, "staged": staged, "stream": stream,
        })
    }
}

// ── HTTP client（只有 http://；給 /api 轉發與 healthz）───────────────────────

pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// 解一個完整的 HTTP/1.1 回應（Content-Length、chunked，或讀到 EOF 的都接受）
pub fn parse_http_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("回應沒有標頭結尾")?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .strip_prefix("HTTP/1.")
        .and_then(|s| s.get(2..5))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("不是 HTTP 回應：{status_line}"))?;
    let mut content_type = String::new();
    let mut length: Option<usize> = None;
    let mut chunked = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-type" => content_type = v.to_string(),
            "content-length" => length = v.parse().ok(),
            "transfer-encoding" => chunked = v.eq_ignore_ascii_case("chunked"),
            _ => {}
        }
    }
    let rest = &raw[sep + 4..];
    let body = if chunked {
        let mut out = Vec::new();
        let mut p = 0;
        while let Some(nl) = rest[p..].windows(2).position(|w| w == b"\r\n") {
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&rest[p..p + nl])
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim(),
                16,
            )
            .map_err(|_| "chunked 長度不是十六進位")?;
            p += nl + 2;
            if size == 0 {
                break;
            }
            out.extend_from_slice(rest.get(p..p + size).ok_or("chunk 不完整")?);
            p += size + 2;
        }
        out
    } else {
        match length {
            Some(n) => rest.get(..n).ok_or("回應不完整")?.to_vec(),
            None => rest.to_vec(),
        }
    };
    Ok(HttpResponse {
        status,
        content_type,
        body,
    })
}

/// 對上游 `GET <path_and_query>`，帶 Bearer（有的話）。`Connection: close`，讀到 EOF。
pub async fn http_get(
    base_url: &str,
    path_and_query: &str,
    key: Option<&str>,
) -> Result<HttpResponse, String> {
    let hp = host_port(base_url)?;
    let mut s = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&hp))
        .await
        .map_err(|_| format!("連 {hp} 逾時"))?
        .map_err(|e| format!("連不上 {hp}：{e}"))?;
    let auth = key
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "GET {path_and_query} HTTP/1.1\r\nHost: {hp}\r\nAccept: application/json\r\nConnection: close\r\n{auth}\r\n"
    );
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("送請求到 {hp} 失敗：{e}"))?;
    let mut raw = Vec::new();
    tokio::time::timeout(HTTP_TIMEOUT, s.read_to_end(&mut raw))
        .await
        .map_err(|_| format!("等 {hp} 回應逾時"))?
        .map_err(|e| format!("讀 {hp} 回應失敗：{e}"))?;
    parse_http_response(&raw)
}

// ── `coding-lab hub add` ─────────────────────────────────────────────────────

/// 在 config.toml 尾端加一段 `[[hub.upstream]]`；原有內容不動；名稱重複、格式不對回錯誤；檔案權限 600（金鑰在裡面）
pub fn append_upstream(
    config_path: &Path,
    name: &str,
    url: &str,
    key: Option<&str>,
) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!("名稱只能用英數字與 . _ -：{name}"));
    }
    ws_url(url)?;
    if let Some(k) = key
        && (k.is_empty() || k.chars().any(|c| c.is_control() || c == '"'))
    {
        return Err("金鑰格式不對".into());
    }
    let existing = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("讀不到 {}：{e}", config_path.display())),
    };
    let table: toml::Table = toml::from_str(&existing)
        .map_err(|e| format!("{} 格式錯誤：{e}", config_path.display()))?;
    let taken = table
        .get("hub")
        .and_then(|h| h.get("upstream"))
        .and_then(|u| u.as_array())
        .into_iter()
        .flatten()
        .any(|u| u.get("name").and_then(|n| n.as_str()) == Some(name));
    if taken {
        return Err(format!(
            "已經有一台叫 {name}；要改網址或金鑰請直接編輯 {}",
            config_path.display()
        ));
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "\n[[hub.upstream]]\nname = \"{name}\"\nurl = \"{url}\"\n"
    ));
    if let Some(k) = key {
        text.push_str(&format!("key = \"{k}\"\n"));
    }
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建不出 {}：{e}", dir.display()))?;
    }
    std::fs::write(config_path, text)
        .map_err(|e| format!("寫不了 {}：{e}", config_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("改不了 {} 的權限：{e}", config_path.display()))?;
    }
    Ok(())
}

// ── 服務：上游連線＋本機 web ────────────────────────────────────────────────

struct UpHandle {
    up: Upstream,
    /// 要送給這台上游的 client → server 訊息（subscribe／unsubscribe／more／ack）
    tx: mpsc::UnboundedSender<Value>,
}

struct HubState {
    hub: Mutex<Hub>,
    ups: Vec<UpHandle>,
    out: broadcast::Sender<Arc<Out>>,
    web_root: Option<PathBuf>,
    instance: String,
    /// 客戶端的時區（上游的 KPI「今天」照它算；變了就重連上游）
    offset: AtomicI32,
    /// 每個 session 頻道有幾條客戶端連線在訂（最後一條退訂才對上游退訂）
    subs: Mutex<HashMap<String, usize>>,
}

static INSTANCES: AtomicU64 = AtomicU64::new(0);

impl HubState {
    fn hub(&self) -> std::sync::MutexGuard<'_, Hub> {
        self.hub.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn handle(&self, name: &str) -> Option<&UpHandle> {
        self.ups.iter().find(|u| u.up.name == name)
    }

    fn emit(&self, outs: Vec<Out>) {
        for o in outs {
            let _ = self.out.send(Arc::new(o));
        }
    }
}

pub async fn serve(
    bind: SocketAddr,
    upstreams: Vec<Upstream>,
    web_root: Option<PathBuf>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    serve_with_backoff(bind, upstreams, web_root, Duration::from_secs(1)).await
}

/// `backoff`：上游斷線後多久試著重連（之後翻倍到 30 秒；連上就歸零）
pub async fn serve_with_backoff(
    bind: SocketAddr,
    upstreams: Vec<Upstream>,
    web_root: Option<PathBuf>,
    backoff: Duration,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    let (out, _) = broadcast::channel(256);
    let mut ups = Vec::new();
    let mut rxs = Vec::new();
    for up in upstreams {
        let (tx, rx) = mpsc::unbounded_channel();
        ups.push(UpHandle { up: up.clone(), tx });
        rxs.push((up, rx));
    }
    let state = Arc::new(HubState {
        hub: Mutex::new(Hub::new(ups.iter().map(|u| u.up.name.clone()).collect())),
        ups,
        out,
        web_root,
        instance: format!(
            "hub-{}-{}",
            crate::now_ms(),
            INSTANCES.fetch_add(1, Ordering::Relaxed)
        ),
        offset: AtomicI32::new(0),
        subs: Mutex::new(HashMap::new()),
    });
    let mut tasks = Vec::new();
    for (up, rx) in rxs {
        tasks.push(tokio::spawn(upstream_loop(state.clone(), up, rx, backoff)));
    }
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/healthz", get(healthz))
        .route("/api/{*rest}", get(api_proxy))
        .fallback(get(static_file))
        .layer(middleware::from_fn(gate))
        .with_state(state);
    let task = tokio::spawn(async move {
        let _tasks = tasks;
        if let Err(e) = axum::serve(listener, app).await {
            crate::logging::log(&format!("hub：{e}"));
        }
    });
    Ok((local, task))
}

/// hub 只綁本機、免金鑰：`Host` 要是 loopback 名稱，`/api` 與 `/ws` 的 `Origin` 要等於 `Host`（同 collector 的本機規則）
async fn gate(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !crate::ws::host_is_loopback(req.headers()) {
        return (StatusCode::FORBIDDEN, "Host 不是本機").into_response();
    }
    if (path.starts_with("/api/") || path == "/ws") && !crate::auth::same_origin(req.headers()) {
        return (StatusCode::FORBIDDEN, "Origin 與 Host 不符").into_response();
    }
    next.run(req).await
}

type UpWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn upstream_loop(
    state: Arc<HubState>,
    up: Upstream,
    mut rx: mpsc::UnboundedReceiver<Value>,
    base_backoff: Duration,
) {
    let mut backoff = base_backoff;
    let mut logged_down = false;
    loop {
        let offset = state.offset.load(Ordering::Relaxed);
        match connect_upstream(&up).await {
            Ok(mut ws) => {
                crate::logging::log(&format!("hub：已連上 {}（{}）", up.name, up.url));
                logged_down = false;
                backoff = base_backoff;
                let hello = json!({ "op": "hello", "protocol": crate::ws::PROTOCOL, "utcOffsetMin": offset });
                let mut first = vec![hello, json!({ "op": "subscribe", "channel": "*" })];
                // 客戶端還訂著的 session 頻道：重連後補訂
                {
                    let subs = state.subs.lock().unwrap_or_else(|e| e.into_inner());
                    let hub = state.hub();
                    for sid in subs.keys() {
                        if hub.owner_of_session(sid).as_deref() == Some(up.name.as_str()) {
                            first.push(
                                json!({ "op": "subscribe", "channel": format!("session:{sid}") }),
                            );
                        }
                    }
                }
                let mut ok = true;
                for m in first {
                    if ws
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            m.to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    run_upstream(&state, &up, &mut ws, &mut rx, offset).await;
                }
                let outs = state.hub().on_offline(&up.name);
                state.emit(outs);
                crate::logging::log(&format!(
                    "hub：{} 斷線，{} 秒後重連",
                    up.name,
                    backoff.as_secs_f32()
                ));
            }
            Err(e) => {
                if !logged_down {
                    crate::logging::log(&format!(
                        "hub：連不上 {}（{}）：{e}；會一直重試",
                        up.name, up.url
                    ));
                    logged_down = true;
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connect_upstream(up: &Upstream) -> Result<UpWs, String> {
    let mut req = ws_url(&up.url)?
        .into_client_request()
        .map_err(|e| e.to_string())?;
    if let Some(k) = &up.key {
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {k}")
                .parse()
                .map_err(|_| "金鑰含不合法字元")?,
        );
    }
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| "連線逾時".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(ws)
}

/// 一條上游連線的主迴圈：收訊息進 Hub、把客戶端的請求送過去；時區變了就結束（呼叫端重連）
async fn run_upstream(
    state: &Arc<HubState>,
    up: &Upstream,
    ws: &mut UpWs,
    rx: &mut mpsc::UnboundedReceiver<Value>,
    offset: i32,
) {
    use tokio_tungstenite::tungstenite::Message as M;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(M::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                    let outs = state.hub().on_message(&up.name, &v);
                    state.emit(outs);
                }
                Some(Ok(M::Ping(p))) => { let _ = ws.send(M::Pong(p)).await; }
                Some(Ok(M::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
            cmd = rx.recv() => match cmd {
                Some(v) => { if ws.send(M::Text(v.to_string().into())).await.is_err() { return; } }
                None => return,
            },
            _ = tick.tick() => {
                if state.offset.load(Ordering::Relaxed) != offset {
                    let _ = ws.close(None).await;
                    return;
                }
            }
        }
    }
}

// ── 本機 web：WS ───────────────────────────────────────────────────────────

async fn ws_handler(State(state): State<Arc<HubState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| client(socket, state))
}

struct ClientSubs {
    all: bool,
    sessions: Vec<String>,
}

async fn send(socket: &mut WebSocket, v: &Value) -> bool {
    socket
        .send(Message::Text(v.to_string().into()))
        .await
        .is_ok()
}

async fn client(mut socket: WebSocket, state: Arc<HubState>) {
    let mut rx = state.out.subscribe();
    let mut subs = ClientSubs {
        all: false,
        sessions: Vec::new(),
    };
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if !on_client_message(&mut socket, &state, &mut subs, &t).await { break; }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            out = rx.recv() => match out {
                Ok(o) => {
                    let ok = match &*o {
                        Out::All(f) => !subs.all || send(&mut socket, f).await,
                        Out::Session(id, v) => !subs.sessions.contains(id) || send(&mut socket, v).await,
                        Out::Resnapshot => !subs.all || { let s = state.hub().snapshot_all(); send(&mut socket, &s).await },
                        Out::Any(v) => send(&mut socket, v).await,
                    };
                    if !ok { break; }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if subs.all { let s = state.hub().snapshot_all(); if !send(&mut socket, &s).await { break; } }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    // 連線結束：退訂它訂的 session 頻道
    for sid in std::mem::take(&mut subs.sessions) {
        unsubscribe_session(&state, &sid);
    }
}

fn forward(state: &HubState, up: &str, msg: Value) -> bool {
    state.handle(up).is_some_and(|h| h.tx.send(msg).is_ok())
}

fn unsubscribe_session(state: &HubState, sid: &str) {
    let last = {
        let mut subs = state.subs.lock().unwrap_or_else(|e| e.into_inner());
        match subs.get_mut(sid) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                subs.remove(sid);
                true
            }
            None => false,
        }
    };
    if last && let Some(owner) = state.hub().owner_of_session(sid) {
        forward(
            state,
            &owner,
            json!({ "op": "unsubscribe", "channel": format!("session:{sid}") }),
        );
    }
}

async fn on_client_message(
    socket: &mut WebSocket,
    state: &Arc<HubState>,
    subs: &mut ClientSubs,
    text: &str,
) -> bool {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return send(socket, &json!({ "t": "error", "message": "不是 JSON" })).await;
    };
    let err = |m: &str| json!({ "t": "error", "message": m });
    match msg["op"].as_str().unwrap_or_default() {
        "hello" => {
            let offset = msg["utcOffsetMin"]
                .as_i64()
                .unwrap_or(0)
                .clamp(-14 * 60, 14 * 60) as i32;
            state.offset.store(offset, Ordering::Relaxed); // 上游連線的時區跟著最近 hello 的客戶端
            let welcome = json!({
                "t": "welcome", "protocol": crate::ws::PROTOCOL, "instance": state.instance, "host": "hub",
                "version": env!("CARGO_PKG_VERSION"), "remote": false,
                "thinking": "on_demand", "scope": "operate",
            });
            send(socket, &welcome).await
        }
        "subscribe" => {
            let ch = msg["channel"].as_str().unwrap_or_default();
            if ch == "*" {
                subs.all = true;
                let s = state.hub().snapshot_all();
                return send(socket, &s).await;
            }
            let Some(sid) = ch.strip_prefix("session:").filter(|s| !s.is_empty()) else {
                return send(socket, &err("不認得的頻道")).await;
            };
            let Some(owner) = state.hub().owner_of_session(sid) else {
                return send(socket, &err("hub 不認得這個 session（它的那台可能離線）")).await;
            };
            if !subs.sessions.iter().any(|s| s == sid) {
                subs.sessions.push(sid.to_string());
                *state
                    .subs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(sid.to_string())
                    .or_insert(0) += 1;
            }
            // 每次都轉給上游：它會回一份 snapshot（就是這條連線在等的）
            forward(state, &owner, json!({ "op": "subscribe", "channel": ch }));
            true
        }
        "unsubscribe" => {
            let ch = msg["channel"].as_str().unwrap_or_default();
            if ch == "*" {
                subs.all = false;
            } else if let Some(sid) = ch.strip_prefix("session:")
                && let Some(i) = subs.sessions.iter().position(|s| s == sid)
            {
                subs.sessions.remove(i);
                unsubscribe_session(state, sid);
            }
            true
        }
        "ack" => {
            let sid = msg["session"].as_str().unwrap_or_default();
            let Some(owner) = state.hub().owner_of_session(sid) else {
                return send(socket, &err("hub 不認得這個 session")).await;
            };
            if !state.hub().can_ack(&owner) {
                return send(
                    socket,
                    &err(&format!(
                        "{owner} 的金鑰只能看（view）；「已確認」需要 operate 金鑰"
                    )),
                )
                .await;
            }
            forward(state, &owner, msg.clone());
            true
        }
        "more" => {
            let sid = msg["session"].as_str().unwrap_or_default();
            let owner = state.hub().owner_of_session(sid);
            match owner {
                Some(owner) => {
                    forward(state, &owner, msg.clone());
                    true
                }
                None => {
                    send(socket, &json!({ "t": "more", "session": sid, "beforePos": msg["beforePos"], "events": [] })).await
                }
            }
        }
        other => send(socket, &err(&format!("不認得的 op：{other}"))).await,
    }
}

// ── 本機 web：HTTP ─────────────────────────────────────────────────────────

fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn healthz(State(state): State<Arc<HubState>>) -> Response {
    let online = state.hub().online();
    let ups: Vec<Value> = state
        .ups
        .iter()
        .map(|u| {
            json!({ "name": u.up.name, "url": u.up.url,
                    "online": online.iter().any(|(n, on)| *n == u.up.name && *on) })
        })
        .collect();
    json_response(
        StatusCode::OK,
        &json!({ "version": env!("CARGO_PKG_VERSION"), "protocol": crate::ws::PROTOCOL, "hub": true, "upstreams": ups }),
    )
}

fn query_param(uri: &Uri, name: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn relay(r: HttpResponse) -> Response {
    let ct = if r.content_type.is_empty() {
        "application/json".to_string()
    } else {
        r.content_type
    };
    (
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY),
        [(header::CONTENT_TYPE, ct)],
        r.body,
    )
        .into_response()
}

/// `/api/*` 轉給擁有那個 session（路徑裡的 id）或目錄（`cwd` 參數）的那台；`/api/sessions` 問每一台再合併
async fn api_proxy(State(state): State<Arc<HubState>>, uri: Uri) -> Response {
    let path = uri.path().to_string();
    let pq = uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or(path.clone());
    let not_found = |why: &str| json_response(StatusCode::NOT_FOUND, &json!({ "error": why }));
    let gateway = |why: String| json_response(StatusCode::BAD_GATEWAY, &json!({ "error": why }));
    if path == "/api/sessions" {
        let cwd = query_param(&uri, "cwd");
        let owner = cwd.as_deref().map(|c| state.hub().owner_of_cwd(c));
        let targets: Vec<&UpHandle> = match owner {
            Some(Some(o)) => state.ups.iter().filter(|u| u.up.name == o).collect(),
            Some(None) => return not_found("hub 不認得這個目錄（沒有任何一台有它的 session）"),
            None => state.ups.iter().collect(),
        };
        let online = state.hub().online();
        let mut rows = Vec::new();
        for u in targets {
            if !online.iter().any(|(n, on)| *n == u.up.name && *on) {
                continue;
            }
            match http_get(&u.up.url, &pq, u.up.key.as_deref()).await {
                Ok(r) if r.status == 200 => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&r.body) {
                        for s in v["sessions"].as_array().into_iter().flatten() {
                            let mut s = s.clone();
                            set_host(&mut s, &u.up.name);
                            rows.push(s);
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    crate::logging::log(&format!("hub：問 {} 的 sessions 失敗：{e}", u.up.name))
                }
            }
        }
        rows.sort_by_key(|s| std::cmp::Reverse(s["startedAt"].as_u64().unwrap_or(0)));
        return json_response(StatusCode::OK, &json!({ "sessions": rows }));
    }
    let owner = if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let id = rest.split('/').next().unwrap_or_default();
        state.hub().owner_of_session(&percent_decode(id))
    } else {
        match query_param(&uri, "cwd") {
            Some(c) => state.hub().owner_of_cwd(&c),
            None => None,
        }
    };
    let Some(owner) = owner else {
        return not_found("hub 不認得這個 session 或目錄（沒有任何一台有它）");
    };
    let Some(u) = state.handle(&owner) else {
        return not_found("上游不存在");
    };
    match http_get(&u.up.url, &pq, u.up.key.as_deref()).await {
        Ok(mut r) => {
            // 回應裡的 Event 與 sessions 列也標 host
            if let Ok(mut v) = serde_json::from_slice::<Value>(&r.body) {
                state.hub().rewrite(&owner, &mut v);
                r.body = v.to_string().into_bytes();
            }
            relay(r)
        }
        Err(e) => gateway(format!("{owner}：{e}")),
    }
}

async fn static_file(State(state): State<Arc<HubState>>, uri: Uri) -> Response {
    crate::ws::static_response(state.web_root.as_deref(), &uri).await
}
