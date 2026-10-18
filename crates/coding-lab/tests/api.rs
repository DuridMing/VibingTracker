//! HTTP API：/api/sessions、/api/sessions/<id>/events、/api/files（contracts.md §7、#671）。

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coding_lab::event::Event;
use coding_lab::store::Store;
use coding_lab::ws::{WebState, serve_web};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

static N: AtomicU32 = AtomicU32::new(0);

/// 真實路徑，和正式碼從 `git rev-parse`／`git worktree list` 拿到的一樣：
/// Windows 的 `canonicalize()` 會帶 `\\?\` 前綴，git 印的路徑沒有，直接比會不相等。
fn canon(p: &Path) -> PathBuf {
    let c = p.canonicalize().unwrap();
    match c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(plain) => PathBuf::from(plain),
        None => c,
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "api-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ev(s: &str, seq: u64, kind: &str, pos: Option<u64>, ts: u64, payload: Value) -> Event {
    Event::from_json_line(
        &json!({"v": 1, "session": s, "seq": seq, "type": kind, "agent": "main", "line": null, "pos": pos,
                "ts_model": null, "ts_exec": null, "ts_seen": ts, "host": "local", "payload": payload})
        .to_string(),
    )
    .unwrap()
}

fn started(s: &str, seq: u64, ts: u64, cwd: &str) -> Event {
    ev(
        s,
        seq,
        "session.started",
        None,
        ts,
        json!({"cwd": cwd, "source": "startup", "model": "claude-opus-5-5",
               "transcriptPath": "/t", "branch": "main"}),
    )
}

fn text(s: &str, seq: u64, pos: u64, ts: u64) -> Event {
    ev(
        s,
        seq,
        "assistant.text",
        Some(pos),
        ts,
        json!({"text": "x", "messageId": "m", "model": "claude-opus-5-5"}),
    )
}

fn prompt(s: &str, seq: u64, ts: u64, t: &str) -> Event {
    ev(
        s,
        seq,
        "prompt.submitted",
        None,
        ts,
        json!({"text": t, "promptId": format!("p{seq}")}),
    )
}

fn store_with(home: &Path, events: &[Event]) -> Arc<Mutex<Store>> {
    let mut s = Store::open(home).unwrap();
    for e in events {
        s.append(e.clone());
    }
    s.flush().unwrap();
    Arc::new(Mutex::new(s))
}

async fn start(state: Arc<WebState>) -> SocketAddr {
    serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap()
        .0
}

/// 回傳 (狀態碼, body)。
async fn get_with(addr: SocketAddr, path: &str, host: &str, origin: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let origin = origin.map_or(String::new(), |o| format!("Origin: {o}\r\n"));
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: close\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let code = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = out
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (code, body)
}

async fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let (code, body) = get_with(addr, path, &addr.to_string(), None).await;
    (code, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ── /api/sessions

/// 已結束、不在 live 裡的 session 從 store 組；live 裡的 session 與 WS snapshot 的列相同。
#[tokio::test]
async fn sessions_api_lists_live_and_stored_sessions() {
    let home = tmp("sessions");
    let old = [
        started("old", 1, 1_000, "/repo/worktrees/feature/x"),
        prompt("old", 2, 1_001, "第一個"),
        text("old", 3, 1, 1_002),
        prompt("old", 4, 1_003, "最後一個"),
        text("old", 5, 2, 1_004),
        ev(
            "old",
            6,
            "session.ended",
            None,
            1_005,
            json!({"reason": "exit"}),
        ),
    ];
    let live = [
        started("new", 1, 5_000, "/other"),
        prompt("new", 2, 5_001, "嗨"),
        text("new", 3, 1, 5_002),
    ];
    let all: Vec<Event> = old.iter().chain(live.iter()).cloned().collect();
    let store = store_with(&home, &all);
    let state = WebState::new(Some(store), None, 8);
    state.apply(&live); // 只有 new 在 live（模擬 old 在重建窗口之外）
    state.flush(6_000);
    let addr = start(state.clone()).await;

    let (code, body) = get(addr, "/api/sessions").await;
    assert_eq!(code, 200, "{body}");
    let rows = body["sessions"].as_array().unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["new", "old"], "新到舊");

    let mut ws = ws_connect(addr).await;
    ws_send(&mut ws, json!({"op": "hello", "protocol": 1})).await;
    ws_recv(&mut ws).await;
    ws_send(
        &mut ws,
        json!({"op": "subscribe", "channel": "session:new"}),
    )
    .await;
    let snap = ws_recv(&mut ws).await;
    assert_eq!(rows[0], snap["sessions"][0], "live 的列與 WS 相同");

    let o = &rows[1];
    assert_eq!(o["status"], "ended");
    assert_eq!(o["cwd"], "/repo/worktrees/feature/x");
    assert_eq!(o["model"], "opus 5.5", "短名，同 WS");
    assert_eq!(o["firstPrompt"], "第一個");
    assert_eq!(o["lastPrompt"], "最後一個");
    assert_eq!(o["lastPos"], 2);
    assert_eq!(
        (o["startedAt"].as_u64(), o["endedAt"].as_u64()),
        (Some(1_000), Some(1_005))
    );
    assert_eq!(
        (o["agentsLive"].as_u64(), o["score"].as_u64()),
        (Some(0), Some(0))
    );
    assert!(o["doing"].is_null());
    let keys = |v: &Value| {
        v.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(keys(o), keys(&rows[0]), "兩種來源的欄位一樣");

    let (_, by_cwd) = get(addr, &format!("/api/sessions?cwd={}", enc("/repo"))).await;
    let ids: Vec<&str> = by_cwd["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["old"], "cwd 底下的 worktree 也算");
    let (_, one) = get(addr, "/api/sessions?limit=1").await;
    assert_eq!(one["sessions"].as_array().unwrap().len(), 1);
}

// ── /api/sessions/<id>/events

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn ws_connect(addr: SocketAddr) -> Ws {
    tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap()
        .0
}

async fn ws_send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

async fn ws_recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("5 秒內沒收到")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test]
async fn events_api_matches_ws_more() {
    let home = tmp("events");
    let mut evs = vec![prompt("s1", 1, 1_001, "修 bug")];
    evs.push(ev(
        "s1",
        2,
        "tool.called",
        None,
        1_002,
        json!({"toolUseId": "t1", "tool": "Bash",
        "input": {"command": "ls"}, "status": "running", "placement": "staged", "danger": null}),
    ));
    evs.push(ev(
        "s1",
        3,
        "tool.placed",
        Some(1),
        1_003,
        json!({"toolUseId": "t1", "placement": "stream"}),
    ));
    evs.push(ev(
        "s1",
        4,
        "tool.result",
        None,
        1_004,
        json!({"toolUseId": "t1", "ok": true,
        "exitCode": 0, "output": "a\n", "truncated": false}),
    ));
    for i in 0..20 {
        evs.push(text("s1", 5 + i, 2 + i, 1_005 + i));
    }
    let store = store_with(&home, &evs);
    let addr = start(WebState::new(Some(store), None, 8)).await;
    let mut ws = ws_connect(addr).await;
    ws_send(&mut ws, json!({"op": "hello", "protocol": 1})).await;
    ws_recv(&mut ws).await;

    for (before, limit) in [(Some(5), 10), (None, 7), (Some(15), 3)] {
        let mut req = json!({"op": "more", "session": "s1", "limit": limit});
        let mut path = format!("/api/sessions/s1/events?limit={limit}");
        if let Some(b) = before {
            req["beforePos"] = json!(b);
            path.push_str(&format!("&beforePos={b}"));
        }
        ws_send(&mut ws, req).await;
        let m = ws_recv(&mut ws).await;
        let (code, body) = get(addr, &path).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(
            body["events"], m["events"],
            "beforePos={before:?} limit={limit}"
        );
        assert_eq!(body["session"], "s1");
        assert_eq!(body["beforePos"], m["beforePos"]);
        assert!(!body["events"].as_array().unwrap().is_empty());
    }

    let (code, _) = get(addr, "/api/sessions/s1/events?limit=9999").await;
    assert_eq!(code, 200, "limit 超過上限就截到 500");
    let (code, _) = get(addr, "/api/sessions/nope/events").await;
    assert_eq!(code, 404);
}

// ── 安全

#[tokio::test]
async fn api_rejects_foreign_origin_and_non_loopback_host() {
    let home = tmp("origin");
    let store = store_with(&home, &[started("s1", 1, 1_000, "/p")]);
    let addr = start(WebState::new(Some(store), None, 8)).await;
    let me = addr.to_string();
    let evil = format!("evil.example:{}", addr.port());
    for path in [
        "/api/sessions",
        "/api/sessions/s1/events",
        "/api/files?cwd=%2Fp",
        "/api/projects/events?cwd=%2Fp",
    ] {
        let (code, _) = get_with(addr, path, &me, Some("http://evil.example")).await;
        assert_eq!(code, 403, "{path}：別的網頁的 Origin");
        let (code, _) = get_with(addr, path, &evil, Some(&format!("http://{evil}"))).await;
        assert_eq!(code, 403, "{path}：DNS rebinding");
    }
    let (code, _) = get_with(addr, "/api/sessions", &me, Some(&format!("http://{me}"))).await;
    assert_eq!(code, 200, "同源的前端可以");
    let (code, _) = get_with(addr, "/api/sessions", &me, None).await;
    assert_eq!(code, 200, "沒帶 Origin 的程式（curl）可以");
}

// ── /api/projects/events（全專案回放，#702）

/// 專案 /p：s1 在根目錄、s2 在 worktree；s3 是別的專案。時間交錯，s1 有一張完整的工具卡片。
fn project_events_fixture() -> Vec<Event> {
    let mut evs = vec![
        started("s1", 1, 1_000, "/p"),
        started("s2", 1, 1_000, "/p/worktrees/feature/x"),
        started("s3", 1, 1_000, "/other"),
        ev(
            "s1",
            2,
            "tool.called",
            None,
            1_001,
            json!({"toolUseId": "t1", "tool": "Bash",
            "input": {"command": "ls"}, "status": "running", "placement": "staged", "danger": null}),
        ),
        ev(
            "s1",
            3,
            "tool.placed",
            Some(1),
            1_002,
            json!({"toolUseId": "t1", "placement": "stream"}),
        ),
        ev(
            "s1",
            4,
            "tool.result",
            None,
            1_003,
            json!({"toolUseId": "t1", "ok": true,
            "exitCode": 0, "output": "a\n", "truncated": false}),
        ),
    ];
    for i in 0..6u64 {
        evs.push(text("s1", 5 + i, 2 + i, 1_010 + 2 * i));
        evs.push(text("s2", 2 + i, 1 + i, 1_011 + 2 * i));
        evs.push(text("s3", 2 + i, 1 + i, 1_011 + 2 * i));
    }
    evs
}

fn positions(body: &Value) -> Vec<(String, u64)> {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| !e["pos"].is_null())
        .map(|e| {
            (
                e["session"].as_str().unwrap().to_string(),
                e["pos"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn project_events_merge_the_projects_sessions_with_their_cards() {
    let home = tmp("project-events");
    let store = store_with(&home, &project_events_fixture());
    let addr = start(WebState::new(Some(store), None, 8)).await;

    let (code, body) = get(addr, "/api/projects/events?cwd=%2Fp&limit=100").await;
    assert_eq!(code, 200, "{body}");
    let mut got = positions(&body);
    got.sort();
    let mut want: Vec<_> = (1..=7).map(|p| ("s1".to_string(), p)).collect();
    want.extend((1..=6).map(|p| ("s2".to_string(), p)));
    want.sort();
    assert_eq!(got, want, "worktree 的 s2 算進來，別的專案的 s3 不算");
    let kinds: Vec<_> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["payload"]["toolUseId"] == "t1")
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect();
    assert!(
        kinds.contains(&"tool.called".into()) && kinds.contains(&"tool.result".into()),
        "卡片的呼叫與結果一起帶回來：{kinds:?}"
    );
    assert!(body["next"].is_null(), "一頁就拿完時沒有 next");
}

#[tokio::test]
async fn project_events_page_back_to_the_start_without_gaps() {
    let home = tmp("project-pages");
    let store = store_with(&home, &project_events_fixture());
    let addr = start(WebState::new(Some(store), None, 8)).await;

    let mut seen = Vec::new();
    let mut path = "/api/projects/events?cwd=%2Fp&limit=4".to_string();
    for _ in 0..20 {
        let (code, body) = get(addr, &path).await;
        assert_eq!(code, 200, "{body}");
        seen.extend(positions(&body));
        let next = &body["next"];
        if next.is_null() {
            break;
        }
        path = format!(
            "/api/projects/events?cwd=%2Fp&limit=4&before={},{},{}",
            next["ts"],
            enc(next["session"].as_str().unwrap()),
            next["pos"]
        );
    }
    let total = seen.len();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), total, "分頁之間不重複");
    assert_eq!(seen.len(), 13, "s1 的 7 個與 s2 的 6 個都拿到");
}

#[tokio::test]
async fn project_events_respect_the_time_range_and_need_a_cwd() {
    let home = tmp("project-range");
    let store = store_with(&home, &project_events_fixture());
    let addr = start(WebState::new(Some(store), None, 8)).await;

    // s1 的 text 在 1010、1012…；s2 在 1011、1013…
    let (code, body) = get(addr, "/api/projects/events?cwd=%2Fp&from=1011&to=1013").await;
    assert_eq!(code, 200, "{body}");
    let mut got = positions(&body);
    got.sort();
    assert_eq!(
        got,
        vec![("s1".into(), 3), ("s2".into(), 1), ("s2".into(), 2)],
        "兩端都包含"
    );
    let (code, _) = get(addr, "/api/projects/events").await;
    assert_eq!(code, 400, "沒有 cwd");
    let (code, _) = get(addr, "/api/projects/events?cwd=%2Fp&before=x").await;
    assert_eq!(code, 400, "游標格式錯");
}

// ── /api/files

fn git(dir: &Path, args: &[&str]) -> String {
    // pre-commit hook 裡跑時會繼承 GIT_DIR／GIT_INDEX_FILE 等，不清掉會寫進正在 commit 的 repo
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
    String::from_utf8(o.stdout).unwrap()
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

struct Repo {
    root: PathBuf,
    x: PathBuf,
    y: PathBuf,
    develop_tip: String,
}

/// main ← dev（改 shared.txt）；主工作樹在 dev；worktree x、y 從 dev 開（dev 不是整合分支的候選，「相對 main」才比 main）：
/// 兩邊都改 a.txt（內容不同＝衝突候選），y 另改 b.txt，x 有未追蹤的 new.txt；之後 dev 再改 shared.txt。
fn repo() -> Repo {
    let root = canon(&tmp("repo"));
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, ".gitignore", "wt/\n*.log\n");
    for f in ["a.txt", "b.txt", "shared.txt"] {
        write(&root, f, "base\n");
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    git(&root, &["switch", "-qc", "dev"]);
    write(&root, "shared.txt", "develop\n");
    git(&root, &["commit", "-qam", "develop"]);
    let develop_tip = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
    let (x, y) = (root.join("wt/x"), root.join("wt/y"));
    git(
        &root,
        &["worktree", "add", "-q", "-b", "feature/x", "wt/x", "dev"],
    );
    git(
        &root,
        &["worktree", "add", "-q", "-b", "feature/y", "wt/y", "dev"],
    );
    write(&x, "a.txt", "x\n");
    git(&x, &["commit", "-qam", "x"]);
    write(&x, "new.txt", "new\n");
    write(&x, "debug.log", "ignored\n");
    write(&y, "a.txt", "y\n");
    write(&y, "b.txt", "y\n");
    git(&y, &["commit", "-qam", "y"]);
    // x、y 開出來之後 develop 又改了 shared.txt：x、y 的 shared.txt 是舊的、內容和主工作樹不同，
    // 但它們自己沒碰過，不是衝突
    write(&root, "shared.txt", "develop 2\n");
    git(&root, &["commit", "-qam", "develop 2"]);
    Repo {
        root,
        x,
        y,
        develop_tip,
    }
}

fn s(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// 一個 worktree 的 (status, path)：照 contracts §7 用 git 自己算，與 API 比對。
fn manual(dir: &Path, range: &str) -> BTreeSet<(String, String)> {
    let mut out: BTreeSet<(String, String)> = git(dir, &["diff", "--name-status", range])
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let st = f.next()?.chars().next()?.to_string();
            Some((st, f.next_back()?.to_string()))
        })
        .collect();
    for p in git(dir, &["ls-files", "--others", "--exclude-standard"]).lines() {
        out.insert(("?".into(), p.to_string()));
    }
    out
}

fn by_source(body: &Value, key: &str, source: &str) -> BTreeSet<(String, String)> {
    body["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            c["by"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b[key] == source)
                .map(|b| {
                    (
                        b["status"].as_str().unwrap().to_string(),
                        c["path"].as_str().unwrap().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn change<'a>(body: &'a Value, path: &str) -> &'a Value {
    body["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["path"] == path)
        .unwrap_or_else(|| panic!("changes 裡沒有 {path}：{body}"))
}

async fn files_server(r: &Repo) -> SocketAddr {
    let home = tmp("files-home");
    let x = s(&r.x);
    let store = store_with(
        &home,
        &[
            started("sx", 1, 1_000, &x),
            ev(
                "sx",
                2,
                "file.edit",
                None,
                1_001,
                json!({"toolUseId": "e1", "path": "a.txt",
                "hunk": null, "hunkSource": "none"}),
            ),
            ev(
                "sx",
                3,
                "file.write",
                None,
                1_002,
                json!({"toolUseId": "w1", "path": "new.txt",
                "isNew": true, "content": "new\n", "truncated": false}),
            ),
            ev(
                "sx",
                4,
                "file.write",
                None,
                1_003,
                json!({"toolUseId": "w2", "path": "a.txt",
                "isNew": false, "content": "x\n", "truncated": false}),
            ),
            // 被拒的 Edit：卡片以 failed 結束，不算改動（#682）
            ev(
                "sx",
                5,
                "file.edit",
                None,
                1_004,
                json!({"toolUseId": "e9", "path": "b.txt",
                "hunk": null, "hunkSource": "none"}),
            ),
            ev(
                "sx",
                6,
                "tool.updated",
                None,
                1_005,
                json!({"toolUseId": "e9", "status": "failed", "durationMs": null,
                "error": "denied", "interrupted": null}),
            ),
        ],
    );
    start(WebState::new(Some(store), None, 8)).await
}

#[tokio::test]
async fn files_relative_to_main_aggregates_every_worktree() {
    let r = repo();
    let addr = files_server(&r).await;
    let (code, body) = get(
        addr,
        &format!("/api/files?cwd={}&base=main", enc(&s(&r.root))),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["root"], s(&r.root));
    assert_eq!(
        (body["base"].as_str(), body["baseRef"].as_str()),
        (Some("main"), Some("main"))
    );

    for wt in [&r.root, &r.x, &r.y] {
        let base = git(wt, &["merge-base", "main", "HEAD"]).trim().to_string();
        assert_eq!(
            by_source(&body, "worktree", &s(wt)),
            manual(wt, &base),
            "{} 與手動 git diff --name-status $(git merge-base main HEAD)（比到工作目錄）一致",
            wt.display()
        );
    }
    let a = change(&body, "a.txt");
    assert_eq!(a["conflict"], true, "x、y 都改 a.txt 而且內容不同");
    let branches: BTreeSet<&str> = a["by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["branch"].as_str().unwrap())
        .collect();
    assert_eq!(branches, BTreeSet::from(["feature/x", "feature/y"]));
    assert_eq!(
        change(&body, "shared.txt")["conflict"],
        false,
        "共同歷史、或對方分支開得早而內容較舊，都不算衝突"
    );
    assert_eq!(change(&body, "b.txt")["conflict"], false);
    let n = change(&body, "new.txt");
    assert_eq!(
        (n["conflict"].as_bool(), n["by"][0]["status"].as_str()),
        (Some(false), Some("?"))
    );
    assert!(
        body["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["path"] != "debug.log"),
        "遵守 .gitignore"
    );
    let paths: Vec<&str> = body["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "依 path 排序");
}

/// 前端要知道哪些目錄是 worktree，才能把 session 的 file.* 路徑對到樹上（被 ignore 的目錄不是）。
/// 每種基準都帶，從哪個 worktree 問都是同一份清單。
#[tokio::test]
async fn files_lists_every_worktree_for_any_base() {
    let r = repo();
    let addr = files_server(&r).await;
    let want = BTreeSet::from([
        (s(&r.root), "dev".to_string()),
        (s(&r.x), "feature/x".to_string()),
        (s(&r.y), "feature/y".to_string()),
    ]);
    for (cwd, base) in [
        (&r.root, "main".to_string()),
        (&r.x, format!("commit:{}", r.develop_tip)),
        (&r.x, "session:sx".to_string()),
    ] {
        let (code, body) = get(
            addr,
            &format!("/api/files?cwd={}&base={}", enc(&s(cwd)), enc(&base)),
        )
        .await;
        assert_eq!(code, 200, "{base}：{body}");
        let got: BTreeSet<(String, String)> = body["worktrees"]
            .as_array()
            .unwrap_or_else(|| panic!("{base}：沒有 worktrees：{body}"))
            .iter()
            .map(|w| {
                (
                    w["path"].as_str().unwrap_or_default().to_string(),
                    w["branch"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        assert_eq!(got, want, "{base}");
    }
}

#[tokio::test]
async fn files_tree_and_commit_base_use_the_cwd_worktree() {
    let r = repo();
    let addr = files_server(&r).await;
    let path = format!(
        "/api/files?cwd={}&base=commit:{}",
        enc(&s(&r.x)),
        r.develop_tip
    );
    let (code, body) = get(addr, &path).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["root"], s(&r.x));
    assert_eq!(body["baseRef"], r.develop_tip.as_str());
    let tree: Vec<&str> = body["tree"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(
        tree,
        [".gitignore", "a.txt", "b.txt", "new.txt", "shared.txt"],
        "含未追蹤、不含 ignore"
    );
    assert_eq!(body["treeTruncated"], false);
    assert_eq!(
        by_source(&body, "worktree", &s(&r.x)),
        manual(&r.x, &format!("{}...HEAD", r.develop_tip))
    );
    assert_eq!(
        body["changes"].as_array().unwrap().len(),
        2,
        "只看 x：a.txt、new.txt"
    );

    // 短 hash 也可以，baseRef 回完整 hash
    let short = &r.develop_tip[..8];
    let (code, body) = get(
        addr,
        &format!("/api/files?cwd={}&base=commit:{short}", enc(&s(&r.x))),
    )
    .await;
    assert_eq!(
        (code, body["baseRef"].as_str()),
        (200, Some(r.develop_tip.as_str()))
    );
}

#[tokio::test]
async fn files_session_base_comes_from_events() {
    let r = repo();
    let addr = files_server(&r).await;
    let (code, body) = get(
        addr,
        &format!("/api/files?cwd={}&base=session:sx", enc(&s(&r.x))),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(body["baseRef"].is_null());
    assert_eq!(
        by_source(&body, "session", "sx"),
        BTreeSet::from([
            ("A".to_string(), "new.txt".to_string()),
            ("M".to_string(), "a.txt".to_string())
        ]),
        "isNew 的 write 是 A；edit 與覆寫是 M；被拒的 edit 不算"
    );
}

#[tokio::test]
async fn files_rejects_unknown_dirs_and_bad_bases() {
    let r = repo();
    let addr = files_server(&r).await;
    let cwd = enc(&s(&r.x));
    let cases = [
        (
            format!("/api/files?cwd={}", enc(&s(&r.y))),
            404,
            "沒有 session 在 y",
        ),
        (format!("/api/files?cwd={}", enc("/etc")), 404, "任意目錄"),
        ("/api/files".to_string(), 400, "cwd 必填"),
        (
            format!("/api/files?cwd={cwd}&base=nope"),
            400,
            "不認得的 base",
        ),
        (
            format!("/api/files?cwd={cwd}&base=commit:--output=x"),
            400,
            "hash 只收十六進位",
        ),
        (format!("/api/files?cwd={cwd}&base=commit:abc"), 400, "太短"),
    ];
    for (path, want, why) in cases {
        let (code, body) = get(addr, &path).await;
        assert_eq!(code, want, "{why}：{body}");
        assert!(body["error"].is_string(), "{why}：錯誤要有說明");
    }
    let (code, body) = get(addr, &format!("/api/files?cwd={cwd}")).await;
    assert_eq!(
        (code, body["base"].as_str()),
        (200, Some("main")),
        "預設 main"
    );
}

/// #673：重啟後 live 列還沒看到 assistant 行，sessions.model 用 store 從 usage 補上的值。
#[tokio::test]
async fn sessions_api_takes_the_model_from_store_until_live_sees_it() {
    let home = tmp("model");
    let mut start_ev = started("s1", 1, 1_000, "/p");
    start_ev.payload["model"] = Value::Null;
    let usage = ev(
        "s1",
        2,
        "usage",
        None,
        1_001,
        json!({"messageId": "m", "model": "claude-opus-5-5", "input": 0, "output": 0, "thinking": 0,
               "cacheRead": 0, "cacheWrite1h": 0, "cacheWrite5m": 0, "costUsd": 0.0}),
    );
    let after_restart = prompt("s1", 3, 2_000, "重啟之後");
    let store = store_with(&home, &[start_ev, usage, after_restart.clone()]);
    let state = WebState::new(Some(store), None, 8);
    state.apply(&[after_restart]); // 重啟後 live 只看到新的 hook
    state.flush(3_000);
    let addr = start(state.clone()).await;

    let (code, body) = get(addr, "/api/sessions").await;
    assert_eq!(code, 200, "{body}");
    let row = &body["sessions"][0];
    assert_eq!(row["lastPrompt"], "重啟之後", "用的是 live 列：{row}");
    assert_eq!(row["model"], "opus 5.5", "{row}");
}

/// 「相對 main」比到工作目錄（#682，PLAN「改動基準」）：還沒 commit 的已追蹤檔也算，
/// 兩個 worktree 都改了同一檔（一邊已 commit、一邊還沒）而且內容不同就是衝突候選。
#[tokio::test]
async fn files_relative_to_main_includes_uncommitted_changes() {
    let r = repo();
    write(&r.x, "b.txt", "x 還沒 commit\n");
    let addr = files_server(&r).await;
    let (code, body) = get(
        addr,
        &format!("/api/files?cwd={}&base=main", enc(&s(&r.root))),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(
        by_source(&body, "worktree", &s(&r.x)).contains(&("M".to_string(), "b.txt".to_string())),
        "x 還沒 commit 的 b.txt：{body}"
    );
    assert_eq!(
        change(&body, "b.txt")["conflict"],
        true,
        "y 已 commit、x 還沒 commit，內容不同"
    );
}

/// repo 設定裡的 clean filter 是外部程式：經由網頁開的 /api/files 不能把它叫起來（#682）。
#[tokio::test]
async fn files_never_run_clean_filters() {
    let r = repo();
    let marker = r.root.join("filter-ran");
    git(
        &r.root,
        &[
            "config",
            "filter.mark.clean",
            &format!("sh -c 'touch {}; cat'", s(&marker)),
        ],
    );
    let info = PathBuf::from(git(&r.root, &["rev-parse", "--git-common-dir"]).trim()).join("info");
    let info = if info.is_absolute() {
        info
    } else {
        r.root.join(info)
    };
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("attributes"), "*.txt filter=mark\n").unwrap();
    // 兩個 worktree 都有同名的未追蹤檔、也都動過已追蹤檔：比內容時會用到 hash-object 與工作目錄的 diff
    write(&r.y, "new.txt", "other\n");
    write(&r.x, "b.txt", "x 還沒 commit\n");
    let addr = files_server(&r).await;
    let (code, body) = get(
        addr,
        &format!("/api/files?cwd={}&base=main", enc(&s(&r.root))),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(!marker.exists(), "clean filter 被執行了");
}

/// 還沒有任何 commit 的 repo（HEAD 指向還不存在的 main，e2e 腳本 git init 後就是這樣）：
/// 相對 main 以空樹為基準，tree 照常、檔案都算新增；不能整棵樹載入失敗（#691）。
#[tokio::test]
async fn files_relative_to_main_on_a_repo_without_commits() {
    let root = canon(&tmp("unborn"));
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "staged.txt", "s\n");
    write(&root, "loose.txt", "l\n");
    git(&root, &["add", "staged.txt"]);
    let home = tmp("unborn-home");
    let store = store_with(&home, &[started("su", 1, 1_000, &s(&root))]);
    let addr = start(WebState::new(Some(store), None, 8)).await;

    let (code, body) = get(
        addr,
        &format!("/api/files?cwd={}&base=main", enc(&s(&root))),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["baseRef"], Value::Null, "沒有 commit 可指：{body}");
    assert_eq!(
        by_source(&body, "worktree", &s(&root)),
        BTreeSet::from([
            ("?".to_string(), "loose.txt".to_string()),
            ("A".to_string(), "staged.txt".to_string()),
        ]),
        "{body}"
    );
    let tree: BTreeSet<&str> = body["tree"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        tree.contains("staged.txt") && tree.contains("loose.txt"),
        "{body}"
    );
}

/// 有 commit、但分支既不是 main 也不是 master：仍是錯誤，不能拿空樹當基準（#691）。
#[tokio::test]
async fn files_relative_to_main_without_main_branch_is_an_error() {
    let root = canon(&tmp("trunk"));
    git(&root, &["init", "-q", "-b", "trunk"]);
    write(&root, "a.txt", "a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    let home = tmp("trunk-home");
    let store = store_with(&home, &[started("st", 1, 1_000, &s(&root))]);
    let addr = start(WebState::new(Some(store), None, 8)).await;

    let (code, body) = get(
        addr,
        &format!("/api/files?cwd={}&base=main", enc(&s(&root))),
    )
    .await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].is_string(), "{body}");
}

/// 只有一個 session 在 root 的 /api/files?base=main。
async fn files_main(root: &Path) -> (u16, Value) {
    let home = tmp("integ-home");
    let store = store_with(&home, &[started("si", 1, 1_000, &s(root))]);
    let addr = start(WebState::new(Some(store), None, 8)).await;
    get(addr, &format!("/api/files?cwd={}&base=main", enc(&s(root)))).await
}

/// 整合分支自動偵測（#693）：git-flow 的 repo 有 develop 就比 develop，
/// develop 上已 commit 的改動不算；baseRef 回實際用的分支。
#[tokio::test]
async fn files_relative_to_main_prefers_develop() {
    let root = canon(&tmp("integ-develop"));
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "a.txt", "a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    git(&root, &["switch", "-qc", "develop"]);
    write(&root, "merged.txt", "已在 develop\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "develop"]);
    git(&root, &["switch", "-qc", "feature/z"]);
    write(&root, "feat.txt", "z\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "z"]);

    let (code, body) = files_main(&root).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["baseRef"], "develop", "{body}");
    assert_eq!(
        by_source(&body, "worktree", &s(&root)),
        BTreeSet::from([("A".to_string(), "feat.txt".to_string())]),
        "merged.txt 已在 develop，不算：{body}"
    );
}

/// 沒有 develop、也沒有 main，只有 master：用 master（#693）。
#[tokio::test]
async fn files_relative_to_main_falls_back_to_master() {
    let root = canon(&tmp("integ-master"));
    git(&root, &["init", "-q", "-b", "master"]);
    write(&root, "a.txt", "a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    git(&root, &["switch", "-qc", "topic"]);
    write(&root, "a.txt", "b\n");

    let (code, body) = files_main(&root).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["baseRef"], "master", "{body}");
    assert_eq!(
        by_source(&body, "worktree", &s(&root)),
        BTreeSet::from([("M".to_string(), "a.txt".to_string())]),
        "{body}"
    );
}

// ── /api/branches（#696）

/// sx 在 x、sr 在主工作樹、sy 在 y 但已結束（不列）。
async fn branches_server(r: &Repo) -> SocketAddr {
    let home = tmp("branches-home");
    let store = store_with(
        &home,
        &[
            started("sx", 1, 1_000, &s(&r.x)),
            started("sr", 1, 1_000, &s(&r.root)),
            started("sy", 1, 1_000, &s(&r.y)),
            ev(
                "sy",
                2,
                "session.ended",
                None,
                2_000,
                json!({"reason": "other"}),
            ),
        ],
    );
    start(WebState::new(Some(store), None, 8)).await
}

fn branch_row<'a>(body: &'a Value, branch: &str) -> &'a Value {
    body["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["branch"] == branch)
        .unwrap_or_else(|| panic!("branches 裡沒有 {branch}：{body}"))
}

#[tokio::test]
async fn branches_lists_every_worktree_with_sessions_and_counts() {
    let r = repo();
    let addr = branches_server(&r).await;
    let (code, body) = get(addr, &format!("/api/branches?cwd={}", enc(&s(&r.x)))).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["integration"], "main", "{body}");
    assert_eq!(body["branches"].as_array().unwrap().len(), 3, "{body}");

    for (wt, name) in [(&r.root, "dev"), (&r.x, "feature/x"), (&r.y, "feature/y")] {
        let row = branch_row(&body, name);
        assert_eq!(row["worktree"], s(wt));
        let lr = git(wt, &["rev-list", "--left-right", "--count", "main...HEAD"]);
        let mut n = lr.split_whitespace().map(|v| v.parse::<u64>().unwrap());
        let (behind, ahead) = (n.next().unwrap(), n.next().unwrap());
        assert_eq!(
            (row["ahead"].as_u64(), row["behind"].as_u64()),
            (Some(ahead), Some(behind)),
            "{name} 與手動 git rev-list --left-right --count 一致：{row}"
        );
        let head = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
        assert_eq!(row["last"]["hash"], head.as_str(), "{row}");
        assert!(row["last"]["message"].is_string(), "{row}");
        let base = git(wt, &["merge-base", "main", "HEAD"]).trim().to_string();
        assert_eq!(
            row["changed"].as_u64(),
            Some(manual(wt, &base).len() as u64),
            "{name} 相對整合分支改了幾個檔：{row}"
        );
    }

    let ids = |name: &str| -> Vec<String> {
        branch_row(&body, name)["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    // x 在主工作樹底下（wt/x），依最長的 worktree 前綴歸屬，不能算到主工作樹
    assert_eq!(ids("feature/x"), ["sx"]);
    assert_eq!(ids("dev"), ["sr"]);
    assert!(ids("feature/y").is_empty(), "已結束的 session 不列");

    let x = branch_row(&body, "feature/x");
    assert_eq!(
        x["dirty"].as_u64(),
        Some(1),
        "x 只有未追蹤的 new.txt（debug.log 被 ignore）：{x}"
    );
    let a = x["shared"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "a.txt")
        .unwrap_or_else(|| panic!("x 的 shared 沒有 a.txt：{x}"));
    assert_eq!(a["conflict"], true, "{a}");
    assert!(
        a["with"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w == "feature/y"),
        "{a}"
    );
}

#[tokio::test]
async fn branches_rejects_unknown_dirs() {
    let r = repo();
    let addr = branches_server(&r).await;
    let (code, _) = get(addr, &format!("/api/branches?cwd={}", enc("/etc"))).await;
    assert_eq!(code, 404);
    let (code, _) = get(addr, "/api/branches").await;
    assert_eq!(code, 400);
}

/// 衝突候選的檔：某個 worktree 對整合分支的 merge-base 的 diff（比到工作目錄）。
#[tokio::test]
async fn branch_diff_is_against_the_merge_base() {
    let r = repo();
    let addr = branches_server(&r).await;
    let q = |wt: &Path, path: &str| {
        format!(
            "/api/branch-diff?cwd={}&worktree={}&path={}",
            enc(&s(&r.x)),
            enc(&s(wt)),
            enc(path)
        )
    };
    let (code, body) = get(addr, &q(&r.y, "a.txt")).await;
    assert_eq!(code, 200, "{body}");
    let diff = body["diff"].as_str().unwrap();
    assert!(diff.contains("-base") && diff.contains("+y"), "{diff}");
    assert_eq!(body["branch"], "feature/y");

    let (code, _) = get(addr, &q(Path::new("/etc"), "a.txt")).await;
    assert_eq!(code, 404, "不在 worktree 清單裡的目錄");
    for bad in ["../a.txt", "/etc/passwd", ":(glob)*"] {
        let (code, body) = get(addr, &q(&r.y, bad)).await;
        assert_eq!(code, 400, "{bad}：{body}");
    }
}

// ── /api/graph、/api/commit、/api/commit-diff（#696 改成 git graph）

#[tokio::test]
async fn graph_matches_git_log_of_every_local_branch() {
    let r = repo();
    let addr = branches_server(&r).await;
    let (code, body) = get(addr, &format!("/api/graph?cwd={}", enc(&s(&r.x)))).await;
    assert_eq!(code, 200, "{body}");
    let want: Vec<(String, String)> = git(
        &r.root,
        &["log", "--branches", "--topo-order", "--format=%H %P"],
    )
    .lines()
    .map(|l| {
        let (h, p) = l.split_once(' ').unwrap_or((l, ""));
        (h.to_string(), p.to_string())
    })
    .collect();
    let got: Vec<(String, String)> = body["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let parents: Vec<&str> = c["parents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap())
                .collect();
            (c["hash"].as_str().unwrap().to_string(), parents.join(" "))
        })
        .collect();
    assert_eq!(got, want, "和 git log --branches --topo-order 一致");
    assert_eq!(body["truncated"], false);

    let x_head = git(&r.x, &["rev-parse", "HEAD"]).trim().to_string();
    let c = body["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["hash"] == x_head.as_str())
        .unwrap();
    assert_eq!(c["refs"], json!(["feature/x"]), "{c}");
    assert_eq!(c["subject"], "x");
    assert!(
        c["ts"].as_u64().unwrap() > 1_000_000_000_000,
        "epoch ms：{c}"
    );

    let (_, body) = get(addr, &format!("/api/graph?cwd={}&limit=2", enc(&s(&r.x)))).await;
    assert_eq!(body["commits"].as_array().unwrap().len(), 2, "{body}");
    assert_eq!(body["truncated"], true);
}

#[tokio::test]
async fn commit_lists_files_against_its_first_parent() {
    let r = repo();
    let addr = branches_server(&r).await;
    let cwd = enc(&s(&r.x));
    let y_head = git(&r.y, &["rev-parse", "HEAD"]).trim().to_string();
    let (code, body) = get(addr, &format!("/api/commit?cwd={cwd}&hash={y_head}")).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["subject"], "y");
    let files: BTreeSet<(String, String)> = body["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["status"].as_str().unwrap().to_string(),
                f["path"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let want: BTreeSet<(String, String)> = git(
        &r.y,
        &["diff-tree", "--no-commit-id", "-r", "--name-status", "HEAD"],
    )
    .lines()
    .map(|l| {
        let (st, p) = l.split_once('\t').unwrap();
        (st.to_string(), p.to_string())
    })
    .collect();
    assert_eq!(files, want, "和 git diff-tree --name-status 一致");

    // 未 commit 的改動：worktree 對 HEAD，含未追蹤檔
    let (code, body) = get(
        addr,
        &format!("/api/commit?cwd={cwd}&worktree={}", enc(&s(&r.x))),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(
        body["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "new.txt" && f["status"] == "?"),
        "{body}"
    );

    for (q, want) in [
        (format!("/api/commit?cwd={cwd}&hash=zz"), 400),
        (
            format!("/api/commit?cwd={cwd}&hash={}", "0".repeat(40)),
            404,
        ),
        (format!("/api/commit?cwd={cwd}"), 400),
        (format!("/api/commit?cwd={cwd}&worktree=%2Fetc"), 404),
    ] {
        let (code, body) = get(addr, &q).await;
        assert_eq!(code, want, "{q}：{body}");
    }
}

#[tokio::test]
async fn commit_diff_of_a_commit_and_of_the_worktree() {
    let r = repo();
    let addr = branches_server(&r).await;
    let cwd = enc(&s(&r.x));
    let y_head = git(&r.y, &["rev-parse", "HEAD"]).trim().to_string();
    let (code, body) = get(
        addr,
        &format!("/api/commit-diff?cwd={cwd}&hash={y_head}&path=a.txt"),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let d = body["diff"].as_str().unwrap();
    assert!(d.contains("-base") && d.contains("+y"), "{d}");

    let (code, body) = get(
        addr,
        &format!(
            "/api/commit-diff?cwd={cwd}&worktree={}&path=new.txt",
            enc(&s(&r.x))
        ),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(body["diff"].as_str().unwrap().contains("+new"), "{body}");

    for bad in ["../a.txt", "/etc/passwd", ":(glob)*"] {
        let (code, body) = get(
            addr,
            &format!("/api/commit-diff?cwd={cwd}&hash={y_head}&path={}", enc(bad)),
        )
        .await;
        assert_eq!(code, 400, "{bad}：{body}");
    }
}

// ── /api/projects/replay（主頁時間軸回放，#729）

fn attention_ev(s: &str, seq: u64, ts: u64, kind: &str, r: &str, raised: bool) -> Event {
    ev(
        s,
        seq,
        if raised {
            "attention.raised"
        } else {
            "attention.cleared"
        },
        None,
        ts,
        json!({"kind": kind, "ref": r, "reason": "測試"}),
    )
}

#[tokio::test]
async fn project_replay_rebuilds_the_main_page_at_any_time() {
    let home = tmp("project-replay");
    let store = store_with(
        &home,
        &[
            started("s1", 1, 1000, "/p"),
            text("s1", 2, 1, 1500),
            attention_ev("s1", 3, 4000, "done", "p1", true),
            started("s2", 1, 5000, "/p/worktrees/a"),
            started("s3", 1, 1000, "/other"),
            text("s3", 2, 1, 1600),
        ],
    );
    store.lock().unwrap().ack("s1", "done", "p1", 6000).unwrap();
    let addr = start(WebState::new(Some(store), None, 8)).await;

    let sessions = |b: &Value| -> Vec<String> {
        b["snapshot"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let attention = |b: &Value| b["snapshot"]["attention"].as_array().unwrap().len();

    let (code, b) = get(addr, "/api/projects/replay?cwd=%2Fp&at=4500").await;
    assert_eq!(code, 200, "{b}");
    assert_eq!(sessions(&b), ["s1"], "別的專案的 s3 不算、s2 還沒開始");
    assert_eq!(attention(&b), 1, "4000 的完成待確認還在");

    let (_, b) = get(addr, "/api/projects/replay?cwd=%2Fp&at=6500").await;
    assert_eq!(sessions(&b), ["s1", "s2"], "worktree 的 s2 算進來");
    assert_eq!(attention(&b), 0, "6000 已確認（store 的 acked_at）");

    let (_, b) = get(addr, "/api/projects/replay?cwd=%2Fp&at=1200&until=8000").await;
    let ts: Vec<u64> = b["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["ts"].as_u64().unwrap())
        .collect();
    assert!(
        ts.contains(&4000) && ts.contains(&5000) && ts.contains(&6000),
        "{ts:?}"
    );
    assert!(!ts.contains(&1600), "s3 的事件不能出現：{ts:?}");

    for bad in [
        "/api/projects/replay?at=1",
        "/api/projects/replay?cwd=%2Fp",
        "/api/projects/replay?cwd=%2Fp&at=x",
    ] {
        assert_eq!(get(addr, bad).await.0, 400, "{bad}");
    }
}

// ── diff API 的 untracked 備援與遠端剝除（C1、A-N1、C20，#734） ───────────────────────

const ENV_SECRET: &str = "sk-live-VERY-SECRET-0123456789";

/// main 上一個 commit；`.env` 被 .gitignore 忽略、`.env.local` 已追蹤（敏感路徑）；
/// `secrets.json` 是沒被忽略的 untracked 敏感檔；`notes.txt` 是一般的 untracked 檔。
fn diff_repo() -> PathBuf {
    let root = canon(&tmp("diffrepo"));
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, ".gitignore", ".env\n");
    write(&root, "a.txt", "a\n");
    write(&root, ".env.local", "OLD=1\n");
    git(&root, &["add", "."]);
    git(&root, &["add", "-f", ".env.local"]);
    git(&root, &["commit", "-qm", "init"]);
    write(&root, ".env", &format!("API_KEY={ENV_SECRET}\n"));
    write(&root, ".env.local", &format!("OLD=1\nNEW={ENV_SECRET}\n"));
    write(
        &root,
        "secrets.json",
        &format!("{{\"k\":\"{ENV_SECRET}\"}}\n"),
    );
    write(&root, "notes.txt", "hello untracked\n");
    root
}

async fn diff_server(root: &Path, remote: bool) -> SocketAddr {
    let home = tmp("diffhome");
    let store = store_with(&home, &[started("s1", 1, 1_000, &s(root))]);
    let state = WebState::new(Some(store), None, 8);
    state.set_force_remote(remote);
    start(state).await
}

async fn diff_of(addr: SocketAddr, root: &Path, api: &str, path: &str) -> (u16, Value) {
    let cwd = enc(&s(root));
    get(
        addr,
        &format!("/api/{api}?cwd={cwd}&worktree={cwd}&path={}", enc(path)),
    )
    .await
}

/// gitignored 的檔在 UI 清單裡本來就不會出現，直接打 API 也不讀：本機回空的 diff，遠端 403。
#[tokio::test]
async fn untracked_fallback_never_reads_ignored_files() {
    let root = diff_repo();
    for remote in [false, true] {
        let addr = diff_server(&root, remote).await;
        for api in ["commit-diff", "branch-diff"] {
            let (code, body) = diff_of(addr, &root, api, ".env").await;
            assert_eq!(code, if remote { 403 } else { 200 }, "{api}：{body}");
            assert!(
                !body.to_string().contains(ENV_SECRET),
                "remote={remote} {api} 讀到 gitignored 的 .env：{body}"
            );
        }
        // 對照：沒被忽略的一般 untracked 檔照樣整檔當新增
        let (_, body) = diff_of(addr, &root, "commit-diff", "notes.txt").await;
        assert!(
            body["diff"].as_str().unwrap().contains("+hello untracked"),
            "{body}"
        );
    }
}

/// 遠端看敏感路徑（不論追蹤與否）一律 403；本機照給。
#[tokio::test]
async fn remote_diff_of_a_sensitive_path_is_forbidden() {
    let root = diff_repo();
    let remote = diff_server(&root, true).await;
    let local = diff_server(&root, false).await;
    for path in [".env.local", "secrets.json"] {
        for api in ["commit-diff", "branch-diff"] {
            let (code, body) = diff_of(remote, &root, api, path).await;
            assert_eq!(code, 403, "{api} {path}：{body}");
            assert!(!body.to_string().contains(ENV_SECRET), "{path}：{body}");
        }
        let (_, body) = diff_of(local, &root, "commit-diff", path).await;
        assert!(
            body["diff"].as_str().unwrap().contains(ENV_SECRET),
            "本機照給 {path}：{body}"
        );
    }
    // 不敏感的檔遠端照給
    let (_, body) = diff_of(remote, &root, "commit-diff", "notes.txt").await;
    assert!(
        body["diff"].as_str().unwrap().contains("+hello untracked"),
        "{body}"
    );
}

/// diff 內容裡的 clk_ 金鑰對遠端只留 id（#720 的規則套到 diff）。
#[tokio::test]
async fn remote_diff_strips_clk_keys() {
    let root = diff_repo();
    let key = "clk_ABCDEFGH_0123456789abcdefghijABCDEFGHIJ0123456789abcde";
    write(&root, "notes.txt", &format!("token {key}\n"));
    let addr = diff_server(&root, true).await;
    let (_, body) = diff_of(addr, &root, "commit-diff", "notes.txt").await;
    let diff = body["diff"].as_str().unwrap();
    assert!(
        !diff.contains(key) && diff.contains("clk_ABCDEFGH_…"),
        "{diff}"
    );
}

/// symlink（檔或目錄）指到 worktree 外：不跟著讀（A-N1）。
#[cfg(unix)]
#[tokio::test]
async fn untracked_fallback_does_not_follow_symlinks_out_of_the_worktree() {
    let root = diff_repo();
    let outside = canon(&tmp("outside"));
    write(&outside, "id_ed", "OUTSIDE-PRIVATE-KEY\n");
    std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();
    std::os::unix::fs::symlink(outside.join("id_ed"), root.join("linkfile")).unwrap();
    let addr = diff_server(&root, false).await;
    for path in ["linkdir/id_ed", "linkfile"] {
        let (_, body) = diff_of(addr, &root, "commit-diff", path).await;
        assert!(
            !body.to_string().contains("OUTSIDE-PRIVATE-KEY"),
            "{path}：{body}"
        );
    }
}

/// FIFO 不是一般檔：馬上回，不能卡住 blocking 執行緒（C20）。
#[cfg(unix)]
#[tokio::test]
async fn untracked_fallback_skips_fifos() {
    let root = diff_repo();
    let fifo = root.join("pipe");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let addr = diff_server(&root, false).await;
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        diff_of(addr, &root, "commit-diff", "pipe"),
    )
    .await;
    // 讀端若卡在 FIFO 上（修正前），開一次讀寫端再關掉讓它讀到 EOF，runtime 才關得掉。
    // 用讀寫模式：沒有讀端時只開寫端會一直等（修正後就是這樣），讀寫模式在 Linux／macOS 都不會卡。
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo);
    let (code, body) = res.expect("5 秒內沒回：讀 FIFO 卡住");
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["diff"], "", "{body}");
}

/// 大的 untracked 檔只讀前段、標 truncated，回應不超過上限（C20）。
#[tokio::test]
async fn untracked_fallback_reads_only_the_head_of_big_files() {
    let root = diff_repo();
    let line = "x".repeat(99) + "\n";
    write(&root, "big.log", &line.repeat(100_000)); // 10 MB
    let addr = diff_server(&root, false).await;
    let (code, body) = diff_of(addr, &root, "commit-diff", "big.log").await;
    assert_eq!(code, 200);
    assert_eq!(body["truncated"], true);
    let diff = body["diff"].as_str().unwrap();
    assert!(diff.len() <= 64 * 1024, "{}", diff.len());
    assert!(
        diff.starts_with("--- /dev/null\n+++ b/big.log\n+xxx"),
        "{}",
        &diff[..80]
    );
}

/// X-1：cwd 白名單只放行已知 cwd、它底下的目錄、它所屬 repo 的主工作樹根；任意的上層目錄（例如 home）不放行。
#[tokio::test]
async fn cwd_whitelist_does_not_accept_arbitrary_ancestors() {
    let r = repo();
    std::fs::create_dir_all(r.x.join("sub")).unwrap();
    let addr = files_server(&r).await;
    let parent = r.root.parent().unwrap();
    let cases = [
        (s(&r.x), 200, "session 自己的 cwd"),
        (s(&r.x.join("sub")), 200, "在 session 的 cwd 底下"),
        (s(&r.root), 200, "session 所屬 repo 的主工作樹根"),
        (s(&r.root.join("wt")), 404, "只是上層目錄、不是 repo 根"),
        (s(parent), 404, "repo 外的上層目錄（像 home）"),
    ];
    for (cwd, want, why) in cases {
        let (code, body) = get(addr, &format!("/api/files?cwd={}", enc(&cwd))).await;
        assert_eq!(code, want, "{why}（{cwd}）：{body}");
    }
}

// ── S-2：git 呼叫不執行 repo 設定或 .git/hooks 裡的外部程式（#743） ─────────────────────

/// 植入三種「git 會替你執行的程式」：`.git/hooks/post-index-change`（git status 刷新 index 時跑）、
/// submodule 自己 config 裡的 clean filter、`log.showSignature`＋假的 `gpg.program`。每支都只在 `marks/` 留下標記檔。
#[cfg(unix)]
fn booby_trapped_repo() -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let base = canon(&tmp("trap"));
    let marks = base.join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let script = |name: &str| {
        let p = base.join(name);
        std::fs::write(
            &p,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marks.join(name).display()),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    // submodule 的來源
    let sub = base.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    write(&sub, "a.txt", "sub\n");
    write(&sub, ".gitattributes", "*.txt filter=evil\n");
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-qm", "sub"]);
    // 主 repo
    let root = base.join("super");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "x.txt", "x\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    git(
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &s(&sub),
            "sub",
        ],
    );
    git(&root, &["commit", "-qm", "sub"]);
    let filter = script("filter");
    git(
        &root.join("sub"),
        &["config", "filter.evil.clean", &s(&filter)],
    );
    let gpg = script("gpg");
    git(&root, &["config", "log.showSignature", "true"]);
    git(&root, &["config", "gpg.program", &s(&gpg)]);
    let hook = script("hook");
    std::fs::copy(&hook, root.join(".git/hooks/post-index-change")).unwrap();
    // 讓 index 的 stat 資訊過期（內容不變、mtime 改掉）：git status 會想刷新 index，就會觸發 post-index-change
    let later = std::time::SystemTime::now() + Duration::from_secs(10);
    std::fs::File::options()
        .write(true)
        .open(root.join("x.txt"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    write(&root.join("sub"), "a.txt", "sub changed\n");
    (root, marks)
}

#[cfg(unix)]
#[tokio::test]
async fn git_calls_never_run_repo_hooks_filters_or_gpg() {
    let (root, marks) = booby_trapped_repo();
    let home = tmp("trap-home");
    let store = store_with(&home, &[started("s1", 1, 1_000, &s(&root))]);
    let addr = start(WebState::new(Some(store), None, 8)).await;
    let cwd = enc(&s(&root));
    let head = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
    for path in [
        format!("/api/files?cwd={cwd}"),
        format!("/api/files?cwd={cwd}&base=session:s1"),
        format!("/api/files?cwd={cwd}&base=commit:{head}"),
        format!("/api/branches?cwd={cwd}"),
        format!("/api/graph?cwd={cwd}"),
        format!("/api/commit?cwd={cwd}&hash={head}"),
        format!("/api/commit-diff?cwd={cwd}&worktree={cwd}&path=x.txt"),
        format!("/api/commit-diff?cwd={cwd}&worktree={cwd}&path=sub"),
        format!("/api/branch-diff?cwd={cwd}&worktree={cwd}&path=x.txt"),
    ] {
        let (code, body) = get(addr, &path).await;
        assert!(code < 500, "{path}：{code} {body}");
    }
    let ran: Vec<String> = std::fs::read_dir(&marks)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(ran.is_empty(), "collector 執行了 repo 設定的程式：{ran:?}");
}
