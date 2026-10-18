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
/// 心跳（#718）：每 10 秒 Ping、15 秒沒有任何訊息就當斷線（Wi-Fi 斷掉時 TCP 不會自己 FIN）
pub const PING_EVERY_MS: u64 = 10_000;
pub const DEAD_AFTER_MS: u64 = 15_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beat {
    Nothing,
    Ping,
    Dead,
}

/// 一條上游連線的心跳狀態（純邏輯，時間由外面給）
pub struct Heartbeat {
    last_activity: u64,
    last_ping: u64,
}

impl Heartbeat {
    pub fn new(now: u64) -> Heartbeat {
        Heartbeat {
            last_activity: now,
            last_ping: now,
        }
    }

    /// 收到任何訊息（frame、snapshot、Pong、Ping）
    pub fn on_activity(&mut self, now: u64) {
        self.last_activity = now;
        self.last_ping = now; // 對方有在說話就不必急著 Ping
    }

    pub fn tick(&mut self, now: u64) -> Beat {
        if now.saturating_sub(self.last_activity) >= DEAD_AFTER_MS {
            return Beat::Dead;
        }
        if now.saturating_sub(self.last_ping) >= PING_EVERY_MS {
            self.last_ping = now;
            return Beat::Ping;
        }
        Beat::Nothing
    }
}

/// `[[hub.upstream]]`：一台 collector。`key` 省略時只適合本機或 SSH 通道（那台 `auth = "local-open"`）。
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Upstream {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub key: Option<String>,
}

/// `coding-lab config` 用 Debug 印整份設定：金鑰只留 `clk_<id>_…`，疑難排解時整段貼出去也不會外洩（R-5）。
impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = self
            .key
            .as_deref()
            .map(|k| match crate::keys::parse_token(k) {
                Some((id, _)) => format!("clk_{id}_…"),
                None => "…".to_string(),
            });
        f.debug_struct("Upstream")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("key", &key)
            .finish()
    }
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
    /// 上游 welcome 的 `thinking`（遠端連線拿到的是哪一種佔位），隨 sessions 列帶給前端（#724）
    thinking: String,
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
        }
        self.ups.entry(name.to_string()).or_default()
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
        let thinking = welcome["thinking"]
            .as_str()
            .unwrap_or("on_demand")
            .to_string();
        let u = self.up(up);
        u.scope = scope;
        u.thinking = thinking;
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
        self.on_message_owned(up, msg.clone())
    }

    /// 同 [`Hub::on_message`]，收下整則訊息（上游迴圈用）：frame 的 events、rows 直接拿走，不再各拷一份（E-H4）
    pub fn on_message_owned(&mut self, up: &str, mut msg: Value) -> Vec<Out> {
        let ch = msg["ch"].as_str().unwrap_or("*").to_string();
        let t = msg["t"].as_str().unwrap_or_default().to_string();
        match t.as_str() {
            "welcome" => {
                self.on_welcome(up, &msg);
                Vec::new()
            }
            "snapshot" if ch == "*" => {
                // （重）連上：快取裡有、新 snapshot 沒有的 session 先明確送 deleted，已開著的客戶端才不會留著
                // 只有 offline 旗標的舊列（#723）；然後重送合併的 snapshot
                let gone = self.on_all_snapshot(up, &msg);
                let mut outs = Vec::new();
                if !gone.is_empty() {
                    let no = self.next_no();
                    let rows: Vec<Value> = gone
                        .into_iter()
                        .map(|id| json!({ "id": id, "host": up, "deleted": true }))
                        .collect();
                    outs.push(Out::All(json!({
                        "t": "frame", "frame": no, "ch": "*", "events": [],
                        "rows": { "sessions": rows },
                    })));
                }
                outs.push(Out::Resnapshot);
                outs
            }
            "snapshot" | "frame" if ch != "*" => {
                self.rewrite(up, &mut msg);
                match ch.strip_prefix("session:") {
                    Some(id) => vec![Out::Session(id.to_string(), msg)],
                    None => Vec::new(),
                }
            }
            "frame" => vec![Out::All(self.on_all_frame(up, msg))],
            "more" | "error" => {
                self.rewrite(up, &mut msg);
                vec![Out::Any(msg)]
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
        let thinking = self
            .ups
            .get(up)
            .map(|u| u.thinking.clone())
            .filter(|t| !t.is_empty());
        for s in v
            .get_mut("sessions")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            set_host(s, up);
            if let (Some(t), Some(o)) = (&thinking, s.as_object_mut()) {
                // 這台上游對遠端的 thinking 模式（#724）：前端依列的 host 決定佔位能不能點開
                o.insert("thinking".into(), Value::String(t.clone()));
            }
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
        // 主頁時間軸回放（#729）：snapshot 與每個 frame 是同樣的形狀，包在裡面
        if let Some(snap) = v.get_mut("snapshot") {
            self.rewrite(up, snap);
        }
        for f in v
            .get_mut("frames")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            self.rewrite(up, f);
        }
    }

    /// 回傳快取裡有、新 snapshot 沒有的 session id（這台重連前後消失的列）
    fn on_all_snapshot(&mut self, up: &str, snap: &Value) -> Vec<String> {
        let mut v = snap.clone();
        self.rewrite(up, &mut v);
        self.since.retain(|(n, _)| n != up);
        let cache = self.up(up);
        cache.online = true;
        let old_ids: Vec<String> = cache.sessions.keys().cloned().collect();
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
        old_ids
            .into_iter()
            .filter(|id| !cache.sessions.contains_key(id))
            .collect()
    }

    fn on_all_frame(&mut self, up: &str, mut v: Value) -> Value {
        self.rewrite(up, &mut v);
        let no = self.next_no();
        let events = match v["events"].take() {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let rows = match v["rows"].take() {
            Value::Object(o) => o,
            _ => Map::new(),
        };
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
        // 每一步都檢查邊界、加法都 checked：上游走區網的 http，殘缺或惡意的回應只能回 Err，
        // 不能 panic（release 是 panic=abort，整個 hub 會結束，W-L5）
        while let Some(nl) = rest
            .get(p..)
            .ok_or("chunk 不完整")?
            .windows(2)
            .position(|w| w == b"\r\n")
        {
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
            let end = p.checked_add(size).ok_or("chunk 長度太大")?;
            out.extend_from_slice(rest.get(p..end).ok_or("chunk 不完整")?);
            p = end.checked_add(2).ok_or("chunk 長度太大")?;
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
    // 只收這個系統發的 clk_ 金鑰（W-L4）：其他字元（例如 \）寫進 TOML 會讓 config.toml 解析失敗、所有子命令起不來
    if let Some(k) = key
        && crate::keys::parse_token(k).is_none()
    {
        return Err("金鑰格式不對：要是 `coding-lab key create` 印出的 clk_ 開頭那一串".into());
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
    /// 連上幾次（healthz 的 `connects`；#718 驗證時區變了不會重連）
    connects: AtomicU64,
}

struct HubState {
    hub: Mutex<Hub>,
    ups: Vec<UpHandle>,
    out: broadcast::Sender<Arc<Outgoing>>,
    web_root: Option<PathBuf>,
    instance: String,
    /// 上游 KPI「今天」用的時區：第一個明確帶 `utcOffsetMin` 的客戶端決定，之後不再變（#718：不然瀏覽器與
    /// 不帶時區的腳本輪流連就一直讓所有上游斷線重連）
    offset: AtomicI32,
    offset_fixed: std::sync::atomic::AtomicBool,
    /// 每個 session 頻道有幾條客戶端連線在訂（最後一條退訂才對上游退訂）
    subs: Mutex<HashMap<String, usize>>,
    /// snapshot 快取以外的 session 屬於哪台（#726）：`/api/sessions` 合併時與逐台試出來時記下。
    /// 已結束、被移出 live 的 session 只有這裡找得到
    known: Mutex<HashMap<String, String>>,
    /// 同上，目錄 → 哪台（C13）：live 快取裡沒有 session 的專案，逐台問出來後記下
    known_cwds: Mutex<HashMap<String, String>>,
}

/// 記下的 session 對照最多幾筆（E-H11）
const KNOWN_SESSIONS_MAX: usize = 4096;

/// 記下的目錄對照最多幾筆，超過就整個重來（逐台問一次的成本很低，不用 LRU）
const KNOWN_CWDS_MAX: usize = 1024;

static INSTANCES: AtomicU64 = AtomicU64::new(0);

impl HubState {
    /// 不發網路請求就知道的擁有者：live 快取，再來是記下的對照表
    fn owner_known(&self, sid: &str) -> Option<String> {
        self.hub().owner_of_session(sid).or_else(|| {
            self.known
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(sid)
                .cloned()
        })
    }

    fn remember(&self, sid: &str, up: &str) {
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        // 有上限（E-H11）：滿了就整個重來，之後用到再逐台問（成本很低，不用 LRU）
        if known.len() >= KNOWN_SESSIONS_MAX && !known.contains_key(sid) {
            known.clear();
        }
        known.insert(sid.to_string(), up.to_string());
    }

    fn hub(&self) -> std::sync::MutexGuard<'_, Hub> {
        self.hub.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn handle(&self, name: &str) -> Option<&UpHandle> {
        self.ups.iter().find(|u| u.up.name == name)
    }

    /// 廣播給客戶端。Resnapshot 在這裡算一次合併的 snapshot（呼叫端已放掉 hub 鎖），
    /// 不再每條連線各自在 hub 鎖裡算（E-H6）。
    fn emit(&self, outs: Vec<Out>) {
        for o in outs {
            let o = match o {
                Out::Resnapshot => Out::All(self.hub().snapshot_all()),
                o => o,
            };
            let _ = self.out.send(Arc::new(Outgoing {
                out: o,
                text: std::sync::OnceLock::new(),
            }));
        }
    }
}

/// 廣播的一則訊息：第一條連線序列化後，其他連線共用同一份 bytes（E-H6）。
/// 客戶端都是本機（hub 不收遠端），不用依連線剝除，同一份文字可以給每一條。
struct Outgoing {
    out: Out,
    text: std::sync::OnceLock<axum::extract::ws::Utf8Bytes>,
}

impl Outgoing {
    fn text(&self) -> Option<axum::extract::ws::Utf8Bytes> {
        let v = match &self.out {
            Out::All(v) | Out::Session(_, v) | Out::Any(v) => v,
            Out::Resnapshot => return None,
        };
        Some(self.text.get_or_init(|| v.to_string().into()).clone())
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
        ups.push(UpHandle {
            up: up.clone(),
            tx,
            connects: AtomicU64::new(0),
        });
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
        offset_fixed: std::sync::atomic::AtomicBool::new(false),
        subs: Mutex::new(HashMap::new()),
        known: Mutex::new(HashMap::new()),
        known_cwds: Mutex::new(HashMap::new()),
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
        .layer(middleware::map_response(crate::ws::no_framing))
        .with_state(state);
    let task = tokio::spawn(async move {
        let _tasks = tasks;
        if let Err(e) = axum::serve(listener, app).await {
            crate::logging::log(&format!("hub：{e}"));
        }
    });
    Ok((local, task))
}

/// hub 只綁本機、免金鑰：`Host` 要是 loopback 名稱，`/api` 與 `/ws` 的 `Origin` 要等於 `Host`（同 collector 的本機規則）。
/// hub 沒有金鑰驗證，反向代理轉來的（帶轉發標頭）一律拒絕：代理後面的人都會變成本機（S-1）。
async fn gate(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if crate::ws::proxied(req.headers()) {
        return (StatusCode::FORBIDDEN, "hub 不接受反向代理轉來的連線").into_response();
    }
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
                if let Some(h) = state.handle(&up.name) {
                    h.connects.fetch_add(1, Ordering::Relaxed);
                }
                crate::logging::log(&format!("hub：已連上 {}（{}）", up.name, up.url));
                logged_down = false;
                backoff = base_backoff;
                let hello = json!({ "op": "hello", "protocol": crate::ws::PROTOCOL, "utcOffsetMin": offset });
                let mut first = vec![hello, json!({ "op": "subscribe", "channel": "*" })];
                // 客戶端還訂著的 session 頻道：重連後補訂
                {
                    let sids: Vec<String> = state
                        .subs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .keys()
                        .cloned()
                        .collect();
                    for sid in &sids {
                        if state.owner_known(sid).as_deref() == Some(up.name.as_str()) {
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
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut hb = Heartbeat::new(crate::now_ms());
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(M::Text(t))) => {
                    hb.on_activity(crate::now_ms());
                    let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                    let outs = state.hub().on_message_owned(&up.name, v);
                    state.emit(outs);
                }
                Some(Ok(M::Ping(p))) => { hb.on_activity(crate::now_ms()); let _ = ws.send(M::Pong(p)).await; }
                Some(Ok(M::Pong(_))) => hb.on_activity(crate::now_ms()),
                Some(Ok(M::Close(_))) | None | Some(Err(_)) => return,
                _ => hb.on_activity(crate::now_ms()),
            },
            cmd = rx.recv() => match cmd {
                Some(v) => { if ws.send(M::Text(v.to_string().into())).await.is_err() { return; } }
                None => return,
            },
            _ = tick.tick() => {
                match hb.tick(crate::now_ms()) {
                    Beat::Dead => {
                        crate::logging::log(&format!("hub：{} {} 秒沒有任何訊息，當作斷線", up.name, DEAD_AFTER_MS / 1000));
                        let _ = ws.close(None).await;
                        return;
                    }
                    Beat::Ping => { if ws.send(M::Ping(Vec::new().into())).await.is_err() { return; } }
                    Beat::Nothing => {}
                }
                // 時區只會定一次（第一個明確帶時區的客戶端）：這裡最多重連一次
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
                    let wanted = match &o.out {
                        Out::All(_) | Out::Resnapshot => subs.all,
                        Out::Session(id, _) => subs.sessions.contains(id),
                        Out::Any(_) => true,
                    };
                    if wanted && let Some(text) = o.text()
                        && socket.send(Message::Text(text)).await.is_err()
                    {
                        break;
                    }
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

/// 這個 session 屬於哪台：先查快取與對照表；都沒有就逐台（只問 online 的）打 `/api/sessions/<id>/events?limit=1`，
/// 第一個回 200 的就是，記下來（collector 對不認得的 session 回 404）。哪台都沒有回 None（#726）
async fn resolve_owner(state: &HubState, sid: &str) -> Option<String> {
    if let Some(o) = state.owner_known(sid) {
        return Some(o);
    }
    if sid.is_empty() {
        return None;
    }
    let online = state.hub().online();
    let path = format!("/api/sessions/{}/events?limit=1", percent_encode(sid));
    for u in &state.ups {
        if !online.iter().any(|(n, on)| *n == u.up.name && *on) {
            continue;
        }
        if let Ok(r) = http_get(&u.up.url, &path, u.up.key.as_deref()).await
            && r.status == 200
        {
            state.remember(sid, &u.up.name);
            return Some(u.up.name.clone());
        }
    }
    None
}

/// `/api/sessions` 合併後的預設與最大筆數（同 collector 的 `/api/sessions`）
const SESSIONS_DEFAULT: usize = 100;
const SESSIONS_MAX: usize = 1000;

/// 擁有這個目錄的上游（C13）：先看 live 快取與記下的對照；都沒有就逐台問上線的上游
/// `/api/sessions?cwd=<目錄>&limit=1`，有列的那台就是，記下來。已結束、被移出 live 的專案（回放舊的工作）靠這個找到。
async fn resolve_cwd_owner(state: &HubState, cwd: &str) -> Option<String> {
    let cached = state.hub().owner_of_cwd(cwd);
    if cached.is_some() {
        return cached;
    }
    let key = cwd.replace('\\', "/").trim_end_matches('/').to_string();
    let remembered = state
        .known_cwds
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    if remembered.is_some() {
        return remembered;
    }
    let online = state.hub().online();
    let path = format!("/api/sessions?cwd={}&limit=1", percent_encode(&key));
    for u in &state.ups {
        if !online.iter().any(|(n, on)| *n == u.up.name && *on) {
            continue;
        }
        let Ok(r) = http_get(&u.up.url, &path, u.up.key.as_deref()).await else {
            continue;
        };
        let has = r.status == 200
            && serde_json::from_slice::<Value>(&r.body)
                .ok()
                .is_some_and(|v| v["sessions"].as_array().is_some_and(|a| !a.is_empty()));
        if has {
            let mut known = state.known_cwds.lock().unwrap_or_else(|e| e.into_inner());
            if known.len() >= KNOWN_CWDS_MAX {
                known.clear();
            }
            known.insert(key, u.up.name.clone());
            return Some(u.up.name.clone());
        }
    }
    None
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
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
    if last && let Some(owner) = state.owner_known(sid) {
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
            // 上游 KPI 的時區由第一個明確帶 utcOffsetMin 的客戶端決定；之後不同的時區不再換（#718）
            if let Some(o) = msg["utcOffsetMin"].as_i64() {
                let o = o.clamp(-14 * 60, 14 * 60) as i32;
                if !state.offset_fixed.swap(true, Ordering::Relaxed) {
                    state.offset.store(o, Ordering::Relaxed);
                } else if state.offset.load(Ordering::Relaxed) != o {
                    crate::logging::log(&format!(
                        "hub：客戶端時區 {o} 和上游連線用的 {} 不同，KPI 的「今天」照後者",
                        state.offset.load(Ordering::Relaxed)
                    ));
                }
            }
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
            let Some(owner) = resolve_owner(state, sid).await else {
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
            let Some(owner) = resolve_owner(state, sid).await else {
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
            let owner = resolve_owner(state, sid).await;
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
                    "online": online.iter().any(|(n, on)| *n == u.up.name && *on),
                    "connects": u.connects.load(Ordering::Relaxed) })
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
        // 快取裡找得到這個目錄就只問那台；找不到（已結束、被移出 live 的專案，C13）就每台都問
        let owner = cwd.as_deref().and_then(|c| state.hub().owner_of_cwd(c));
        let targets: Vec<&UpHandle> = match owner {
            Some(o) => state.ups.iter().filter(|u| u.up.name == o).collect(),
            None => state.ups.iter().collect(),
        };
        let online = state.hub().online();
        // 並行問各台（E-H7）：某台卡住時不用一台一台等到逾時
        let pq = pq.as_str();
        let asks = targets
            .into_iter()
            .filter(|u| online.iter().any(|(n, on)| *n == u.up.name && *on))
            .map(|u| async move { (u, http_get(&u.up.url, pq, u.up.key.as_deref()).await) });
        let mut rows = Vec::new();
        for (u, res) in futures_util::future::join_all(asks).await {
            match res {
                Ok(r) if r.status == 200 => {
                    if let Ok(mut v) = serde_json::from_slice::<Value>(&r.body)
                        && let Value::Array(list) = v["sessions"].take()
                    {
                        for mut s in list {
                            if let Some(id) = s["id"].as_str() {
                                state.remember(id, &u.up.name);
                            }
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
        // 依開始時間由新到舊（同時間依 host、id，結果穩定），合併後截到 limit：每台各給 limit 筆，合起來會超過
        let key = |s: &Value| {
            (
                std::cmp::Reverse(s["startedAt"].as_u64().unwrap_or(0)),
                s["host"].as_str().unwrap_or_default().to_string(),
                s["id"].as_str().unwrap_or_default().to_string(),
            )
        };
        rows.sort_by_cached_key(key);
        let limit = query_param(&uri, "limit")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(SESSIONS_DEFAULT)
            .clamp(1, SESSIONS_MAX);
        rows.truncate(limit);
        return json_response(StatusCode::OK, &json!({ "sessions": rows }));
    }
    let owner = if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let id = rest.split('/').next().unwrap_or_default();
        resolve_owner(&state, &percent_decode(id)).await
    } else {
        match query_param(&uri, "cwd") {
            Some(c) => resolve_cwd_owner(&state, &c).await,
            None => None,
        }
    };
    let Some(owner) = owner else {
        return not_found("hub 不認得這個 session 或目錄（沒有任何一台有它）");
    };
    let Some(u) = state.handle(&owner) else {
        return not_found("上游不存在");
    };
    // 只有帶 Event 或 sessions 列的回應要改寫、標 host（E-H5）；其他（檔案樹、diff、分支、thinking 全文…）
    // 原封轉送 bytes，不解析也不重新序列化。原封送沒有剝除的顧慮：hub 對上游是遠端，需要剝的上游已經剝過；
    // hub 自己沒有遠端客戶端（只綁 loopback、帶轉發標頭的一律 403，見 gate）。這個前提一改，這裡要回到
    // ws::redact_remote_copy。
    let rewrite = path.ends_with("/events") && path.starts_with("/api/sessions/")
        || path == "/api/projects/replay";
    match http_get(&u.up.url, &pq, u.up.key.as_deref()).await {
        Ok(mut r) => {
            if rewrite && let Ok(mut v) = serde_json::from_slice::<Value>(&r.body) {
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
