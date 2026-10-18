//! 遠端剝除的端到端（#749「發佈前最該補的 5 個測試」之一）：同一個 collector、同一組資料，
//! 本機（loopback、無金鑰）與遠端（反向代理轉來、帶 view 金鑰）各拉一次 WS（`*`、`session:<id>` 的
//! snapshot 與之後的 frame）和 HTTP（sessions、events、專案回放、diff），比對：
//!
//! - C1：diff 的 gitignored、敏感路徑、指到外面的 symlink
//! - C2：取自 thinking 的 `doing`（與 thinking 本身）
//! - C8：敏感的 Write／Edit／Read 的 input、content、hunk、output
//! - S-5：`/api/sessions` 等處的 clk_ 金鑰
//!
//! 每一類都有命中（本機看得到、遠端看不到）與不命中（遠端照樣看得到）的對照。
//! 遠端不用 `set_force_remote`，走真的金鑰驗證與 `X-Forwarded-For` 判斷。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coding_lab::config::AuthMode;
use coding_lab::event::Event;
use coding_lab::keys::{self, KeyStore, Scope};
use coding_lab::store::Store;
use coding_lab::ws::{WebState, serve_web};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

static N: AtomicU32 = AtomicU32::new(0);
const T0: u64 = 1_760_000_000_000;

/// 遠端不該看到的字串（本機的輸出裡全部都要找得到，證明情境真的有放進去）。
const EVENT_SECRETS: [&str; 6] = [
    "THINK-SECRET",
    "WRITE-SECRET",
    "EDIT-OLD-SECRET",
    "EDIT-NEW-SECRET",
    "RESULT-SECRET",
    "THINK-LATER-SECRET",
];
/// 不敏感、遠端照樣要看得到的字串。
const PUBLIC: [&str; 5] = [
    "PUBLIC-WRITE",
    "PUBLIC-EDIT",
    "PUBLIC-RESULT",
    "PUBLIC-DOING",
    "PUBLIC-PROMPT",
];

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "redact-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let mut c = Command::new("git");
    c.current_dir(dir);
    for (k, _) in std::env::vars() {
        if k.starts_with("GIT_") {
            c.env_remove(k);
        }
    }
    let o = c
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}：{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// 有已追蹤的一般檔與敏感檔（都改過）、gitignored 的檔、指到 repo 外的 symlink。
fn repo() -> PathBuf {
    let root = tmp("repo");
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, ".gitignore", "build/\n");
    write(&root, "a.txt", "a\n");
    write(&root, ".env.local", "OLD=1\n");
    git(&root, &["add", "."]);
    git(&root, &["add", "-f", ".env.local"]);
    git(&root, &["commit", "-qm", "init"]);
    write(&root, "a.txt", "a\nPUBLIC-DIFF\n");
    write(&root, ".env.local", "OLD=1\nDIFF-SECRET=1\n");
    write(&root, "build/out.log", "IGNORED-SECRET\n");
    #[cfg(unix)]
    {
        let outside = tmp("outside");
        write(&outside, "id_ed", "OUTSIDE-SECRET\n");
        std::os::unix::fs::symlink(outside.join("id_ed"), root.join("linkfile")).unwrap();
    }
    root
}

fn ev(session: &str, seq: u64, kind: &str, pos: Option<u64>, payload: Value) -> Event {
    Event::from_json_line(
        &json!({"v": 1, "session": session, "seq": seq, "type": kind, "agent": "main", "line": null,
                "pos": pos, "ts_model": null, "ts_exec": null, "ts_seen": T0 + seq * 10, "host": "local",
                "payload": payload})
        .to_string(),
    )
    .unwrap()
}

fn started(session: &str, cwd: &str) -> Event {
    ev(
        session,
        1,
        "session.started",
        None,
        json!({"cwd": cwd, "source": "startup", "model": "m", "transcriptPath": "/t", "branch": "main"}),
    )
}

fn called(session: &str, seq: u64, id: &str, tool: &str, input: Value, sensitive: bool) -> Event {
    ev(
        session,
        seq,
        "tool.called",
        Some(seq),
        json!({"toolUseId": id, "tool": tool, "input": input, "status": "running",
               "placement": "stream", "danger": null, "sensitive": sensitive}),
    )
}

fn result(session: &str, seq: u64, id: &str, output: &str, sensitive: bool) -> Event {
    ev(
        session,
        seq,
        "tool.result",
        None,
        json!({"toolUseId": id, "ok": true, "exitCode": 0, "output": output, "truncated": false,
               "sensitive": sensitive}),
    )
}

/// 先有的資料（連線時的 snapshot）：
/// s1 的 doing 取自 thinking、敏感與不敏感的 Write；s2 的 doing 是帶金鑰的 prompt；s3 的 doing 取自回覆文字。
fn before(root: &str, token: &str) -> Vec<Event> {
    let env = format!("{root}/.env");
    let a = format!("{root}/a.txt");
    vec![
        started("s1", root),
        ev(
            "s1",
            2,
            "prompt.submitted",
            None,
            json!({"text": "PUBLIC-PROMPT 開始", "promptId": "p1"}),
        ),
        ev(
            "s1",
            3,
            "assistant.thinking",
            Some(1),
            json!({"text": "THINK-SECRET 先讀設定", "summarized": false, "messageId": "m1", "model": "m"}),
        ),
        called(
            "s1",
            4,
            "w1",
            "Write",
            json!({"file_path": env, "content": "WRITE-SECRET=1"}),
            true,
        ),
        ev(
            "s1",
            5,
            "file.write",
            None,
            json!({"toolUseId": "w1", "path": ".env", "isNew": true, "content": "WRITE-SECRET=1",
                   "truncated": false, "sensitive": true}),
        ),
        called(
            "s1",
            6,
            "w2",
            "Write",
            json!({"file_path": a, "content": "PUBLIC-WRITE"}),
            false,
        ),
        ev(
            "s1",
            7,
            "file.write",
            None,
            json!({"toolUseId": "w2", "path": "a.txt", "isNew": false, "content": "PUBLIC-WRITE",
                   "truncated": false}),
        ),
        started("s2", root),
        ev(
            "s2",
            2,
            "prompt.submitted",
            None,
            json!({"text": format!("部署用 {token}"), "promptId": "p2"}),
        ),
        started("s3", root),
        ev(
            "s3",
            2,
            "prompt.submitted",
            None,
            json!({"text": "改 README", "promptId": "p3"}),
        ),
        ev(
            "s3",
            3,
            "assistant.text",
            Some(1),
            json!({"text": "PUBLIC-DOING 正在改 README。", "messageId": "m3", "model": "m"}),
        ),
    ]
}

/// 連上之後才到的資料（之後的 frame）：敏感與不敏感的 Edit、Read 的輸出，和第二段 thinking。
fn after(root: &str) -> Vec<Event> {
    let env_local = format!("{root}/.env.local");
    let a = format!("{root}/a.txt");
    vec![
        called(
            "s1",
            8,
            "e1",
            "Edit",
            json!({"file_path": env_local, "old_string": "EDIT-OLD-SECRET", "new_string": "EDIT-NEW-SECRET",
                   "replace_all": false}),
            true,
        ),
        ev(
            "s1",
            9,
            "file.edit",
            None,
            json!({"toolUseId": "e1", "path": ".env.local", "hunkSource": "pre", "sensitive": true,
                   "hunk": {"start": 1, "oldLines": 1, "newLines": 1,
                            "lines": ["-EDIT-OLD-SECRET", "+EDIT-NEW-SECRET"]}}),
        ),
        called(
            "s1",
            10,
            "r1",
            "Read",
            json!({"file_path": format!("{root}/.env")}),
            true,
        ),
        result("s1", 11, "r1", "RESULT-SECRET=1", true),
        called(
            "s1",
            12,
            "e2",
            "Edit",
            json!({"file_path": a, "old_string": "a", "new_string": "PUBLIC-EDIT", "replace_all": false}),
            false,
        ),
        ev(
            "s1",
            13,
            "file.edit",
            None,
            json!({"toolUseId": "e2", "path": "a.txt", "hunkSource": "pre",
                   "hunk": {"start": 1, "oldLines": 1, "newLines": 1, "lines": ["-a", "+PUBLIC-EDIT"]}}),
        ),
        called("s1", 14, "b1", "Bash", json!({"command": "echo hi"}), false),
        result("s1", 15, "b1", "PUBLIC-RESULT", false),
        ev(
            "s1",
            16,
            "assistant.thinking",
            Some(16),
            json!({"text": "THINK-LATER-SECRET 再想想", "summarized": false, "messageId": "m4", "model": "m"}),
        ),
    ]
}

struct Srv {
    addr: SocketAddr,
    state: Arc<WebState>,
    store: Arc<Mutex<Store>>,
    root: PathBuf,
    view: String,
    /// 出現在 prompt 裡的另一把金鑰（S-5）
    token: String,
}

async fn start() -> Srv {
    let root = repo();
    let home = tmp("home");
    let mut keys = KeyStore::new_with_secret([7u8; 32]);
    let mut rng = keys::test_rng(749);
    let view = keys.create("laptop", Scope::View, None, T0, &mut rng);
    let token = keys.create("ci", Scope::Operate, None, T0, &mut rng);
    keys.save(&home).unwrap();

    let evs = before(&root.to_string_lossy(), &token);
    let mut store = Store::open(&home).unwrap();
    for e in &evs {
        store.append(e.clone());
    }
    store.flush().unwrap();
    let store = Arc::new(Mutex::new(store));
    let state = WebState::new_with_auth(
        Some(store.clone()),
        None,
        64,
        home.clone(),
        AuthMode::LocalOpen,
    );
    state.apply(&evs);
    state.flush(T0 + 1_000);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state.clone())
        .await
        .unwrap();
    Srv {
        addr,
        state,
        store,
        root,
        view,
        token,
    }
}

impl Srv {
    fn remote_headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("X-Forwarded-For", "203.0.113.9".into()),
            ("Authorization", format!("Bearer {}", self.view)),
        ]
    }

    fn headers(&self, remote: bool) -> Vec<(&'static str, String)> {
        if remote {
            self.remote_headers()
        } else {
            Vec::new()
        }
    }

    /// 連上之後才到的資料：先進 store 再給 live（和 serve 的順序一樣），打包成一個 frame。
    fn push_after(&self) {
        let evs = after(&self.root.to_string_lossy());
        {
            let mut s = self.store.lock().unwrap();
            for e in &evs {
                s.append(e.clone());
            }
            s.flush().unwrap();
        }
        self.state.apply(&evs);
        self.state.flush(T0 + 2_000);
    }

    fn enc(&self, s: &str) -> String {
        s.replace('%', "%25")
            .replace('/', "%2F")
            .replace(' ', "%20")
            .replace(':', "%3A")
            .replace('\\', "%5C")
    }
}

async fn http(addr: SocketAddr, path: &str, headers: &[(&str, String)]) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap();
    (code, body.to_string())
}

async fn ws_connect(addr: SocketAddr, headers: &[(&str, String)]) -> Ws {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    for (k, v) in headers {
        let name: tokio_tungstenite::tungstenite::http::HeaderName = k.parse().unwrap();
        req.headers_mut().insert(name, v.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

async fn recv(ws: &mut Ws) -> Value {
    match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str(&t).unwrap(),
        other => panic!("沒等到訊息：{other:?}"),
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// hello、訂 `*` 與 `session:s1`，回 (連線, [welcome, 兩個 snapshot])。
async fn open_ws(srv: &Srv, remote: bool) -> (Ws, Vec<Value>) {
    let mut ws = ws_connect(srv.addr, &srv.headers(remote)).await;
    send(&mut ws, json!({"op": "hello", "protocol": 1})).await;
    let welcome = recv(&mut ws).await;
    assert_eq!(welcome["t"], "welcome", "{welcome}");
    let mut got = vec![welcome];
    for ch in ["*", "session:s1"] {
        send(&mut ws, json!({"op": "subscribe", "channel": ch})).await;
        let snap = recv(&mut ws).await;
        assert_eq!(snap["t"], "snapshot", "{ch}：{snap}");
        got.push(snap);
    }
    (ws, got)
}

/// 兩個頻道的 frame 都收到為止。
async fn frames(ws: &mut Ws) -> Vec<Value> {
    let mut chans = std::collections::BTreeSet::new();
    let mut got = Vec::new();
    while chans.len() < 2 {
        let m = recv(ws).await;
        if m["t"] == "frame" {
            chans.insert(m["ch"].as_str().unwrap_or("").to_string());
            got.push(m);
        }
    }
    got
}

fn session_row<'a>(sessions: &'a Value, id: &str) -> &'a Value {
    sessions
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == id))
        .unwrap_or_else(|| panic!("沒有 {id}：{sessions}"))
}

/// 每一類的 tool／file 事件在遠端都帶 `redacted`，路徑類欄位保留。
fn assert_redacted_events(all: &str, what: &str) {
    let v: Value = serde_json::from_str(all).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    fn walk(v: &Value, seen: &mut std::collections::BTreeSet<String>, what: &str) {
        match v {
            Value::Object(m) => {
                if let (Some(kind), Some(p)) =
                    (m.get("type").and_then(Value::as_str), m.get("payload"))
                    && p["sensitive"] == true
                {
                    assert_eq!(p["redacted"], true, "{what}：{v}");
                    if kind == "tool.called" {
                        let keys: Vec<&String> = p["input"].as_object().unwrap().keys().collect();
                        assert_eq!(keys, ["file_path"], "{what}：只留路徑：{v}");
                    }
                    seen.insert(kind.to_string());
                }
                m.values().for_each(|c| walk(c, seen, what));
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, seen, what)),
            _ => {}
        }
    }
    walk(&v, &mut seen, what);
    for kind in ["tool.called", "file.write", "file.edit", "tool.result"] {
        assert!(
            seen.contains(kind),
            "{what}：沒看到敏感的 {kind}（情境沒送到？）"
        );
    }
}

/// 一個 client 看到的所有東西：WS（snapshot＋frame）、sessions、三個 session 的 events、專案回放。
async fn everything(srv: &Srv, remote: bool, ws: &mut Ws, snaps: Vec<Value>) -> (String, Value) {
    let h = srv.headers(remote);
    let mut all = snaps;
    all.extend(frames(ws).await);
    let (code, body) = http(srv.addr, "/api/sessions", &h).await;
    assert_eq!(code, 200, "{body}");
    let sessions: Value = serde_json::from_str(&body).unwrap();
    all.push(sessions.clone());
    for s in ["s1", "s2", "s3"] {
        let (code, body) = http(srv.addr, &format!("/api/sessions/{s}/events"), &h).await;
        assert_eq!(code, 200, "{s}：{body}");
        all.push(serde_json::from_str(&body).unwrap());
    }
    let cwd = srv.enc(&srv.root.to_string_lossy());
    let (code, body) = http(
        srv.addr,
        &format!("/api/projects/replay?cwd={cwd}&at={}", T0 + 1_000),
        &h,
    )
    .await;
    assert_eq!(code, 200, "回放：{body}");
    all.push(serde_json::from_str(&body).unwrap());
    (Value::Array(all).to_string(), sessions)
}

/// C2、C8、S-5：事件、doing 與金鑰，經過 WS 與 HTTP 的每一條路。
#[tokio::test]
async fn remote_view_key_never_sees_sensitive_content_but_sees_the_rest() {
    let srv = start().await;
    let (mut local_ws, local_snaps) = open_ws(&srv, false).await;
    let (mut remote_ws, remote_snaps) = open_ws(&srv, true).await;
    assert_eq!(remote_snaps[0]["scope"], "view", "{}", remote_snaps[0]);
    srv.push_after();

    let (local, local_sessions) = everything(&srv, false, &mut local_ws, local_snaps).await;
    let (remote, remote_sessions) = everything(&srv, true, &mut remote_ws, remote_snaps).await;

    // 命中：本機都看得到（情境真的有放），遠端一個都看不到
    for secret in EVENT_SECRETS.iter().copied().chain([srv.token.as_str()]) {
        assert!(local.contains(secret), "本機要看得到 {secret}");
        assert!(!remote.contains(secret), "遠端看到了 {secret}");
    }
    assert_redacted_events(&remote, "遠端");
    // 不命中：一般的內容照送
    for public in PUBLIC {
        assert!(remote.contains(public), "遠端少了 {public}");
    }

    // C2：doing 取自 thinking 的 s1 對遠端是 null，取自文字的 s3 照送
    for (sessions, is_remote) in [
        (&local_sessions["sessions"], false),
        (&remote_sessions["sessions"], true),
    ] {
        let s1 = session_row(sessions, "s1");
        assert_eq!(s1["doingSource"], "thinking", "{s1}");
        if is_remote {
            assert_eq!(s1["doing"], Value::Null, "{s1}");
        } else {
            assert!(
                s1["doing"].as_str().unwrap().contains("THINK-LATER-SECRET"),
                "{s1}"
            );
        }
        let s3 = session_row(sessions, "s3");
        assert!(
            s3["doing"].as_str().unwrap().contains("PUBLIC-DOING"),
            "{s3}"
        );
    }
    // S-5：s2 的 doing（帶金鑰的 prompt）對遠端只留 id
    let id = keys::parse_token(&srv.token).unwrap().0.to_string();
    let s2 = session_row(&remote_sessions["sessions"], "s2");
    assert!(
        s2["doing"]
            .as_str()
            .unwrap()
            .contains(&format!("clk_{id}_…")),
        "{s2}"
    );
    assert!(
        session_row(&local_sessions["sessions"], "s2")["doing"]
            .as_str()
            .unwrap()
            .contains(&srv.token),
        "本機照給完整的金鑰"
    );
}

async fn diff(srv: &Srv, remote: bool, api: &str, path: &str) -> (u16, String) {
    let cwd = srv.enc(&srv.root.to_string_lossy());
    http(
        srv.addr,
        &format!("/api/{api}?cwd={cwd}&worktree={cwd}&path={}", srv.enc(path)),
        &srv.headers(remote),
    )
    .await
}

/// C1：diff 的 gitignored、敏感路徑、symlink；一般檔的 diff 遠端照給。
#[tokio::test]
async fn remote_diffs_skip_ignored_sensitive_and_symlinked_files() {
    let srv = start().await;
    for api in ["commit-diff", "branch-diff"] {
        // 不命中：一般檔兩邊都有
        for remote in [false, true] {
            let (code, body) = diff(&srv, remote, api, "a.txt").await;
            assert_eq!(code, 200, "{api} remote={remote}：{body}");
            assert!(
                body.contains("PUBLIC-DIFF"),
                "{api} remote={remote}：{body}"
            );
        }
        // 敏感路徑：本機給、遠端 403
        let (code, body) = diff(&srv, false, api, ".env.local").await;
        assert_eq!(code, 200, "{api}：{body}");
        assert!(body.contains("DIFF-SECRET"), "{api}：本機照給：{body}");
        let (code, body) = diff(&srv, true, api, ".env.local").await;
        assert_eq!(code, 403, "{api}：{body}");
        assert!(!body.contains("DIFF-SECRET"), "{api}：{body}");
        // gitignored：本機也不讀（空的 diff）、遠端 403
        let (code, body) = diff(&srv, false, api, "build/out.log").await;
        assert_eq!(code, 200, "{api}：{body}");
        assert!(!body.contains("IGNORED-SECRET"), "{api}：{body}");
        let (code, body) = diff(&srv, true, api, "build/out.log").await;
        assert_eq!(code, 403, "{api}：{body}");
        assert!(!body.contains("IGNORED-SECRET"), "{api}：{body}");
        // 指到 repo 外的 symlink：兩邊都不跟著讀
        #[cfg(unix)]
        for remote in [false, true] {
            let (_, body) = diff(&srv, remote, api, "linkfile").await;
            assert!(
                !body.contains("OUTSIDE-SECRET"),
                "{api} remote={remote}：{body}"
            );
        }
    }
    // 沒帶金鑰的代理請求一律 401，什麼都不給
    let cwd = srv.enc(&srv.root.to_string_lossy());
    let (code, body) = http(
        srv.addr,
        &format!("/api/commit-diff?cwd={cwd}&worktree={cwd}&path=a.txt"),
        &[("X-Forwarded-For", "203.0.113.9".into())],
    )
    .await;
    assert_eq!(code, 401, "{body}");
    assert!(!body.contains("PUBLIC-DIFF"));
}
