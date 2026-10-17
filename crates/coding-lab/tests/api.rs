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

/// main ← develop（改 shared.txt）；主工作樹在 develop；worktree x、y 從 develop 開：
/// 兩邊都改 a.txt（內容不同＝衝突候選），y 另改 b.txt，x 有未追蹤的 new.txt；之後 develop 再改 shared.txt。
fn repo() -> Repo {
    let root = canon(&tmp("repo"));
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, ".gitignore", "wt/\n*.log\n");
    for f in ["a.txt", "b.txt", "shared.txt"] {
        write(&root, f, "base\n");
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    git(&root, &["switch", "-qc", "develop"]);
    write(&root, "shared.txt", "develop\n");
    git(&root, &["commit", "-qam", "develop"]);
    let develop_tip = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
    let (x, y) = (root.join("wt/x"), root.join("wt/y"));
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/x",
            "wt/x",
            "develop",
        ],
    );
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/y",
            "wt/y",
            "develop",
        ],
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
        (s(&r.root), "develop".to_string()),
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
