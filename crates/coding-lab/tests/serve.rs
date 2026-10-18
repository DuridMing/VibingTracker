//! `coding-lab serve` 與 `ensure` 端到端（#629）。每個測試用自己的資料目錄與埠，collector 會閒置自動結束。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "common/sh.rs"]
mod sh;

static N: AtomicU32 = AtomicU32::new(0);

struct Env {
    home: PathBuf,
    hook: u16,
    web: u16,
}

static NEXT_PORT: AtomicU32 = AtomicU32::new(0);

/// 從各平台的 ephemeral 範圍（Linux 32768 起、macOS／Windows 49152 起）以下挑埠，同一個 process 不重複發。
/// 綁 0 再放掉的埠，平行測試再綁 0 時核心可能又發同一個，兩個 collector 就搶同一個埠。
/// 起點混入時間，連續幾輪測試的範圍才不會重疊到上一輪還在閒置結束的 collector。
fn free_port() -> u16 {
    const LOW: u32 = 20_000;
    const SPAN: u32 = 12_000;
    static START: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let start = *START.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        (std::process::id() ^ nanos).wrapping_mul(2_654_435_761) % SPAN
    });
    loop {
        let port = (LOW + (start + NEXT_PORT.fetch_add(1, Ordering::SeqCst)) % SPAN) as u16;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// 綁住的空埠：測試自己要先佔著 hook 埠時用。free_port 探完就放掉，並行的別的測試程序
/// 可能在測試綁上去之前先綁走同一個埠（#725）。
fn bound_free_port() -> (TcpListener, u16) {
    loop {
        let p = free_port();
        if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
            return (l, p);
        }
    }
}

fn env(idle_secs: u64) -> Env {
    env_on(idle_secs, free_port())
}

fn env_on(idle_secs: u64, hook: u16) -> Env {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "serve-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let web = free_port();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[hook]\nbind = \"127.0.0.1:{hook}\"\n[web]\nbind = \"127.0.0.1:{web}\"\n[collector]\nidle_exit_seconds = {idle_secs}\n"
        ),
    )
    .unwrap();
    Env { home, hook, web }
}

fn cmd(e: &Env, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_coding-lab"));
    c.args(args)
        .env("CODING_LAB_HOME", &e.home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 等 collector 起停的上限（#725）。collector 開、關 store 時 SQLite 會做 WAL 復原、checkpoint 與 fsync，
/// 機器 I/O 很忙時（workspace 測試並行、cargo 在連結）實測要 2～8 秒以上；測的是「會不會」，不是「多快」。
const DEADLINE: Duration = Duration::from_secs(30);

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !cond() {
        assert!(Instant::now() < deadline, "等不到：{what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn collector(e: &Env) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(e.home.join("collector.json")).ok()?).ok()
}

fn http(port: u16, method: &str, path: &str, body: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn start_serve(e: &Env) -> Kill {
    let child = cmd(e, &["serve"]).spawn().unwrap();
    let pid = child.id();
    wait_for("collector.json", || {
        collector(e).is_some_and(|c| c["pid"] == pid)
    });
    Kill(child)
}

#[test]
fn end_to_end_hook_and_transcript_reach_store_and_healthz() {
    let e = env(0);
    let _serve = start_serve(&e);
    let transcript = e.home.join("t.jsonl");
    std::fs::write(&transcript, "").unwrap();

    let start = json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": transcript, "cwd": "/p", "model": "claude-opus-5-5"});
    assert!(http(e.hook, "POST", "/hook", &start.to_string()).starts_with("HTTP/1.1 200"));
    let pre = json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                     "tool_input": {"command": "ls"}, "tool_use_id": "t1", "transcript_path": transcript});
    http(e.hook, "POST", "/hook", &pre.to_string());
    let line = json!({"type": "assistant", "uuid": "u1", "timestamp": "2026-10-16T00:00:00Z",
                      "message": {"id": "m1", "model": "claude-opus-5-5",
                                  "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}]}});
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(format!("{line}\n").as_bytes())
        .unwrap();

    // transcript 的行讓暫存卡片進串流，寫進 store
    let jsonl = e.home.join("events/s1.jsonl");
    wait_for("tool.placed 進 store", || {
        std::fs::read_to_string(&jsonl).is_ok_and(|t| t.contains("tool.placed"))
    });
    let text = std::fs::read_to_string(&jsonl).unwrap();
    assert!(text.contains("session.started") && text.contains("tool.called"));

    let health = http(e.web, "GET", "/healthz", "");
    let body: Value = serde_json::from_str(health.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        (body["sessions"].as_u64(), body["tails"].as_u64()),
        (Some(1), Some(1)),
        "{body}"
    );
}

#[test]
fn second_serve_exits_and_the_first_keeps_running() {
    let e = env(0);
    let first = start_serve(&e);
    let pid = first.0.id();
    let out = cmd(&e, &["serve"]).output().unwrap();
    assert!(out.status.success(), "已有 collector 時第二個以 0 結束");
    assert_eq!(
        collector(&e).unwrap()["pid"],
        pid,
        "不覆蓋第一個的 collector.json"
    );
}

#[test]
fn stale_collector_json_is_taken_over() {
    let e = env(0);
    std::fs::write(
        e.home.join("collector.json"),
        json!({"pid": 999_999, "hook": format!("127.0.0.1:{}", e.hook), "web": format!("127.0.0.1:{}", e.web)}).to_string(),
    )
    .unwrap();
    let _serve = start_serve(&e); // start_serve 會等到 collector.json 換成自己的 pid
}

#[test]
fn idle_collector_exits_by_itself() {
    let e = env(1);
    let mut s = start_serve(&e);
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = s.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "閒置 1 秒後應自動結束");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(collector(&e).is_none(), "結束時移除 collector.json");
}

/// session 還開著（沒有 SessionEnd）時不能閒置結束：結束後使用者回來一輸入，http hook 就會連不上，
/// Claude Code 會顯示 hook 錯誤（官方文件沒有方法能關掉這個提示）。
#[test]
fn idle_exit_waits_until_every_session_has_ended() {
    let e = env(1);
    let mut s = start_serve(&e);
    let start = json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("t1.jsonl"), "cwd": "/p"});
    http(e.hook, "POST", "/hook", &start.to_string());
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        s.0.try_wait().unwrap().is_none(),
        "還有 session 開著，不能結束"
    );

    let end = json!({"session_id": "s1", "hook_event_name": "SessionEnd", "reason": "exit"});
    http(e.hook, "POST", "/hook", &end.to_string());
    let deadline = Instant::now() + DEADLINE;
    while s.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "所有 session 結束、閒置 1 秒後要結束"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 重啟後從 store 重建：上一輪還開著的 session 也算進行中。
#[test]
fn sessions_open_before_a_restart_still_keep_the_collector_alive() {
    let e = env(1);
    {
        let _first = start_serve(&e);
        let start = json!({"session_id": "s2", "hook_event_name": "SessionStart", "source": "startup",
                           "transcript_path": e.home.join("t2.jsonl"), "cwd": "/p"});
        http(e.hook, "POST", "/hook", &start.to_string());
        let jsonl = e.home.join("events/s2.jsonl");
        wait_for("session.started 進 store", || {
            std::fs::read_to_string(&jsonl).is_ok_and(|t| t.contains("session.started"))
        });
    } // Kill：模擬 collector 當掉
    let mut s = start_serve(&e);
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        s.0.try_wait().unwrap().is_none(),
        "s2 沒有 SessionEnd，重啟後也不能閒置結束"
    );
}

/// #670-1：重啟後續跑的 session，seq 與 pos 接著上一輪編號，新事件也進得了索引。
#[test]
fn seq_and_pos_continue_after_a_restart() {
    let e = env(1);
    let jsonl = e.home.join("events/s3.jsonl");
    let events = || -> Vec<Value> {
        std::fs::read_to_string(&jsonl)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let prompt = |text: &str| {
        json!({"session_id": "s3", "hook_event_name": "UserPromptSubmit", "prompt": text, "cwd": "/p"})
            .to_string()
    };
    {
        let _first = start_serve(&e);
        let start = json!({"session_id": "s3", "hook_event_name": "SessionStart", "source": "startup",
                           "transcript_path": e.home.join("t3.jsonl"), "cwd": "/p"});
        http(e.hook, "POST", "/hook", &start.to_string());
        http(e.hook, "POST", "/hook", &prompt("一"));
        wait_for("第一輪的 prompt 進 store", || {
            events().iter().any(|v| v["type"] == "prompt.submitted")
        });
    } // Kill：模擬 collector 當掉
    let before = events().len();
    let _second = start_serve(&e);
    http(e.hook, "POST", "/hook", &prompt("二"));
    wait_for("第二輪的 prompt 進 store", || events().len() > before);

    let all = events();
    let seqs: Vec<u64> = all.iter().map(|v| v["seq"].as_u64().unwrap()).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq 要嚴格遞增：{seqs:?}"
    );
    let pos: Vec<u64> = all.iter().filter_map(|v| v["pos"].as_u64()).collect();
    assert!(
        pos.windows(2).all(|w| w[0] < w[1]),
        "pos 要嚴格遞增：{pos:?}"
    );
}

fn ensure(e: &Env, stdin: &str) -> std::process::Output {
    let mut c = cmd(e, &["ensure"]).stdin(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    c.wait_with_output().unwrap()
}

/// 收掉測試拉起的 collector。Windows 沒有 `kill` 指令，不收的話每跑一次測試就漏一個 collector，
/// target/tmp 被它鎖住刪不掉（#712）。
fn kill_pid(pid: u64) {
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
    #[cfg(not(windows))]
    let _ = Command::new("kill").arg(pid.to_string()).status();
}

/// 驗收：連跑兩次 ensure 只有一個 collector；SessionStart 由 ensure 轉送進來。
#[test]
fn ensure_twice_starts_one_collector_and_forwards_session_start() {
    let e = env(5);
    let start = json!({"session_id": "s9", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("t9.jsonl"), "cwd": "/p"})
    .to_string();
    let t = Instant::now();
    let out = ensure(&e, &start);
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "stdout 會進 Claude 的上下文，必須是空的"
    );
    assert!(t.elapsed() < Duration::from_secs(5));
    wait_for("ensure 拉起的 collector 寫好 collector.json", || {
        collector(&e).is_some()
    });
    let pid = collector(&e).expect("ensure 要拉起 collector")["pid"]
        .as_u64()
        .unwrap();

    let out = ensure(&e, &start);
    assert!(out.status.success() && out.stdout.is_empty());
    assert_eq!(
        collector(&e).unwrap()["pid"].as_u64(),
        Some(pid),
        "第二次不再拉起"
    );

    let jsonl = e.home.join("events/s9.jsonl");
    wait_for("session.started 進 store", || {
        std::fs::read_to_string(&jsonl).is_ok_and(|t| t.contains("session.started"))
    });
    kill_pid(pid);
}

/// L-9（#739）：ensure 拉起的 collector 的工作目錄是 home，不是第一個 session 的專案目錄
/// （Windows 上程序的 cwd 會鎖住那個資料夾，刪不掉也改不了名）。用 /proc 看，只在 Linux 跑。
#[cfg(target_os = "linux")]
#[test]
fn ensure_starts_the_collector_in_its_home() {
    let e = env(5);
    let project = e.home.with_extension("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut c = cmd(&e, &["ensure"]);
    c.current_dir(&project).stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    assert!(child.wait().unwrap().success());
    wait_for("collector.json", || collector(&e).is_some());
    let pid = collector(&e).unwrap()["pid"].as_u64().unwrap();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap();
    kill_pid(pid);
    assert_eq!(cwd, e.home.canonicalize().unwrap());
}

/// EW9（#760）：主 runtime 的 worker 最多 4 條。預設是邏輯核心數（Windows 實機 24 核閒置 29 條執行緒），
/// collector 的工作量用不到；hook 入口另有自己的執行緒（L-7）。用 /proc 數，只在 Linux 跑。
#[cfg(target_os = "linux")]
#[test]
fn serve_uses_at_most_four_runtime_workers() {
    let e = env(0);
    let _c = start_serve(&e);
    let pid = collector(&e).unwrap()["pid"].as_u64().unwrap();
    let workers = std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|c| c.trim() == "tokio-rt-worker")
        .count();
    assert!((1..=4).contains(&workers), "worker {workers} 條");
}

#[test]
fn ensure_never_fails_even_if_the_port_is_taken_by_something_else() {
    // 先占住再把埠寫進設定：先挑埠再綁的話，中間可能被別人拿走
    let squatter = TcpListener::bind("127.0.0.1:0").unwrap(); // 接受連線但從不回應
    let e = env_on(0, squatter.local_addr().unwrap().port());
    let t = Instant::now();
    let out = ensure(&e, r#"{"session_id":"s","hook_event_name":"SessionStart"}"#);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(t.elapsed() < Duration::from_secs(5), "不能卡住 Claude Code");
    drop(squatter);
}

fn ev(session: &str, seq: u64, kind: &str, ts: u64) -> coding_lab::event::Event {
    coding_lab::event::Event::from_json_line(
        &json!({
            "v": 1, "session": session, "seq": seq, "type": kind, "agent": "main",
            "line": null, "pos": null, "ts_model": null, "ts_exec": null, "ts_seen": ts,
            "host": "local", "payload": {},
        })
        .to_string(),
    )
    .unwrap()
}

/// #670-8：SessionEnd 之後 tail 最後一次讀到的行、tail 關掉時的 transcript.recovered 都排在
/// session.ended 後面，不能讓已結束的 session 在重啟後又算成開著（閒置結束會等到 24 小時）。
#[test]
fn events_after_session_end_do_not_reopen_it() {
    let events = [
        ev("a", 1, "session.started", 1),
        ev("a", 2, "session.ended", 2),
        ev("a", 3, "assistant.text", 3),
        ev("a", 4, "transcript.recovered", 4),
        ev("b", 1, "prompt.submitted", 5),
        ev("c", 1, "session.started", 6),
        ev("c", 2, "session.ended", 7),
        ev("c", 3, "session.started", 8), // resume
    ];
    let mut open: Vec<String> = coding_lab::serve::open_sessions(&events)
        .into_iter()
        .collect();
    open.sort();
    assert_eq!(open, ["b", "c"]);
}

fn ev_with(
    session: &str,
    seq: u64,
    kind: &str,
    pos: Option<u64>,
    ts: u64,
    payload: Value,
) -> coding_lab::event::Event {
    coding_lab::event::Event::from_json_line(
        &json!({
            "v": 1, "session": session, "seq": seq, "type": kind, "agent": "main",
            "line": null, "pos": pos, "ts_model": null, "ts_exec": null, "ts_seen": ts,
            "host": "local", "payload": payload,
        })
        .to_string(),
    )
    .unwrap()
}

fn json_body(resp: &str) -> Value {
    serde_json::from_str(resp.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap_or(Value::Null)
}

/// #671：真的 collector 起來後，store 裡的 session 從三個 API 都查得到。
#[test]
fn http_api_serves_stored_sessions() {
    let e = env(30);
    let cwd = e.home.join("proj").to_string_lossy().replace('\\', "/");
    {
        let mut s = coding_lab::store::Store::open(&e.home).unwrap();
        let now = coding_lab::now_ms();
        s.append(ev_with(
            "s1",
            1,
            "session.started",
            None,
            now,
            json!({"cwd": cwd, "source": "startup",
            "model": "claude-opus-5-5", "transcriptPath": "/t", "branch": "main"}),
        ));
        s.append(ev_with(
            "s1",
            2,
            "prompt.submitted",
            None,
            now + 1,
            json!({"text": "嗨", "promptId": "p"}),
        ));
        s.append(ev_with(
            "s1",
            3,
            "assistant.text",
            Some(1),
            now + 2,
            json!({"text": "x", "messageId": "m", "model": "claude-opus-5-5"}),
        ));
        s.append(ev_with(
            "s1",
            4,
            "file.write",
            None,
            now + 3,
            json!({"toolUseId": "w", "path": "n.txt",
            "isNew": true, "content": "", "truncated": false}),
        ));
        s.flush().unwrap();
    }
    let _c = start_serve(&e);

    let r = http(e.web, "GET", "/api/sessions", "");
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    let rows = json_body(&r)["sessions"].clone();
    assert_eq!(
        (rows[0]["id"].as_str(), rows[0]["lastPos"].as_u64()),
        (Some("s1"), Some(1)),
        "{rows}"
    );

    let r = http(e.web, "GET", "/api/sessions/s1/events?limit=10", "");
    let types: Vec<String> = json_body(&r)["events"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| e["type"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(types, ["prompt.submitted", "assistant.text"], "{r}");

    let r = http(
        e.web,
        "GET",
        &format!("/api/files?cwd={}&base=session:s1", cwd.replace('/', "%2F")),
        "",
    );
    assert_eq!(json_body(&r)["changes"][0]["path"], "n.txt", "{r}");
}

// ── prices.json 覆寫（#678）

/// 送一個 session 與一行 100 萬輸入 tokens 的 assistant 行，回傳寫進 store 的 usage.costUsd。
fn cost_of_one_million_input(e: &Env) -> f64 {
    let transcript = e.home.join("tp.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let start = json!({"session_id": "sp", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": transcript, "cwd": "/p", "model": "claude-opus-5-5"});
    http(e.hook, "POST", "/hook", &start.to_string());
    let prompt = json!({"session_id": "sp", "hook_event_name": "UserPromptSubmit", "prompt": "hi",
                        "transcript_path": transcript});
    http(e.hook, "POST", "/hook", &prompt.to_string());
    let line = json!({"type": "assistant", "uuid": "u1", "timestamp": "2026-10-16T00:00:00Z",
                      "message": {"id": "m1", "model": "claude-opus-5-5",
                                  "content": [{"type": "text", "text": "好。"}],
                                  "usage": {"input_tokens": 1_000_000, "output_tokens": 0}}});
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(format!("{line}\n").as_bytes())
        .unwrap();
    let jsonl = e.home.join("events/sp.jsonl");
    wait_for("usage 進 store", || {
        std::fs::read_to_string(&jsonl).is_ok_and(|t| t.contains("\"usage\""))
    });
    std::fs::read_to_string(&jsonl)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "usage")
        .and_then(|v| v["payload"]["costUsd"].as_f64())
        .unwrap()
}

#[test]
fn prices_json_overrides_the_builtin_price() {
    let e = env(0);
    std::fs::write(
        e.home.join("prices.json"),
        r#"{"claude-opus-5-5": {"input": 10.0, "output": 50.0, "cacheRead": 1.0}}"#,
    )
    .unwrap();
    let _serve = start_serve(&e);
    assert_eq!(
        cost_of_one_million_input(&e),
        10.0,
        "內建是 4.0，覆寫成 10.0"
    );
}

#[test]
fn broken_prices_json_warns_and_keeps_the_builtin_price() {
    let e = env(0);
    std::fs::write(e.home.join("prices.json"), "{ 壞掉").unwrap();
    let _serve = start_serve(&e);
    let cost = cost_of_one_million_input(&e);
    assert_eq!(cost, 4.0, "照內建表");
    // 背景執行時警告寫進 collector.log（#655）
    let log = std::fs::read_to_string(e.home.join("collector.log")).unwrap_or_default();
    assert!(log.contains("prices.json"), "log 要有警告：{log}");
}

// ── 重啟後的 sessions 列（#676）

/// 真的重啟 collector：還在跑的 session 收到新 prompt 後，`/api/sessions` 的列除了這一輪的欄位都和重啟前相同。
#[test]
fn sessions_row_survives_a_restart() {
    let e = env(30);
    let transcript = e.home.join("tr.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let hook = |name: &str, extra: Value| {
        let mut body = json!({"session_id": "sr", "hook_event_name": name,
                              "transcript_path": transcript, "cwd": "/p"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        http(e.hook, "POST", "/hook", &body.to_string());
    };
    let row = || {
        json_body(&http(e.web, "GET", "/api/sessions", ""))["sessions"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["id"] == "sr").cloned())
            .unwrap_or(Value::Null)
    };
    let before = {
        let _first = start_serve(&e);
        hook(
            "SessionStart",
            json!({"source": "startup", "model": "claude-opus-5-5"}),
        );
        hook("UserPromptSubmit", json!({"prompt": "一"}));
        let line = json!({"type": "assistant", "uuid": "u1", "timestamp": "2026-10-16T00:00:00Z",
                          "message": {"id": "m1", "model": "claude-opus-5-5",
                                      "content": [{"type": "text", "text": "好。"}],
                                      "usage": {"input_tokens": 1_000_000, "output_tokens": 0}}});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap()
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
        wait_for("重啟前的列有費用", || {
            row()["costUsd"].as_f64().is_some_and(|c| c > 0.0)
        });
        row()
    }; // Kill：模擬 collector 當掉
    let _second = start_serve(&e);
    hook("UserPromptSubmit", json!({"prompt": "二"}));
    wait_for("重啟後的 prompt 進 live", || {
        row()["lastPrompt"] == "二"
    });
    std::thread::sleep(Duration::from_millis(1_000)); // tail 重開後若重算 usage 會在這段時間進來
    let after = row();
    for k in [
        "cwd",
        "firstPrompt",
        "startedAt",
        "costUsd",
        "thinkingTokens",
        "commits",
        "model",
        "branch",
    ] {
        assert_eq!(after[k], before[k], "{k}：重啟前 {before}\n重啟後 {after}");
    }
}

// ── 重啟後的注意佇列（#679）

/// 真的重啟 collector：重啟前「完成待確認」的 session 收到新 prompt 後，done 解除、狀態回到 run。
#[test]
fn done_from_before_a_restart_is_cleared_by_the_next_prompt() {
    let e = env(30);
    let hook = |name: &str, extra: Value| {
        let mut body = json!({"session_id": "sd", "hook_event_name": name,
                              "transcript_path": e.home.join("td.jsonl"), "cwd": "/p"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        http(e.hook, "POST", "/hook", &body.to_string());
    };
    let status = || {
        json_body(&http(e.web, "GET", "/api/sessions", ""))["sessions"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["id"] == "sd").cloned())
            .map_or(Value::Null, |r| r["status"].clone())
    };
    {
        let _first = start_serve(&e);
        hook("SessionStart", json!({"source": "startup"}));
        hook(
            "UserPromptSubmit",
            json!({"prompt": "一", "prompt_id": "p1"}),
        );
        hook("Stop", json!({}));
        wait_for("Stop 5 秒後標完成待確認", || status() == "done");
        // live 先更新、store 每 100 ms 才 flush：等 done 寫進 JSONL 才殺
        wait_for("done 寫進 store", || {
            std::fs::read_to_string(e.home.join("events/sd.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .any(|v| v["type"] == "session.status" && v["payload"]["status"] == "done")
        });
    } // Kill：模擬 collector 當掉
    let _second = start_serve(&e);
    assert_eq!(status(), "done", "前提：重啟後重放出 done");
    hook(
        "UserPromptSubmit",
        json!({"prompt": "二", "prompt_id": "p2"}),
    );
    wait_for("新的 prompt 撤回 done", || status() == "run");
    // D3：merge 發出解除、store 記下解除與 run，和 live 的 run 一致
    wait_for("store 記下解除 done 與 run", || {
        let lines: Vec<Value> = std::fs::read_to_string(e.home.join("events/sd.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let cleared = lines.iter().any(|v| {
            v["type"] == "attention.cleared"
                && v["payload"]["kind"] == "done"
                && v["payload"]["ref"] == "p1"
        });
        let last = lines
            .iter()
            .rfind(|v| v["type"] == "session.status")
            .map(|v| v["payload"]["status"].clone());
        cleared && last == Some(json!("run"))
    });
}

/// D3（#679）：重啟前按 Esc 的 session（idle）重啟後下 prompt 要回到 run；store、live 的 status
/// 與注意佇列三方一致。
#[test]
fn idle_from_before_a_restart_returns_to_run_on_the_next_prompt() {
    let e = env(31);
    let hook = |name: &str, extra: Value| {
        let mut body = json!({"session_id": "si", "hook_event_name": name,
                              "transcript_path": e.home.join("ti.jsonl"), "cwd": "/p"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        http(e.hook, "POST", "/hook", &body.to_string());
    };
    let status = || {
        json_body(&http(e.web, "GET", "/api/sessions", ""))["sessions"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["id"] == "si").cloned())
            .map_or(Value::Null, |r| r["status"].clone())
    };
    let stored_status = || {
        std::fs::read_to_string(e.home.join("events/si.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .rfind(|v| v["type"] == "session.status")
            .map_or(Value::Null, |v| v["payload"]["status"].clone())
    };
    // Esc 中斷：transcript 的 `[Request interrupted by user]` 行
    let transcript = e.home.join("ti.jsonl");
    {
        let _first = start_serve(&e);
        hook("SessionStart", json!({"source": "startup"}));
        hook(
            "UserPromptSubmit",
            json!({"prompt": "一", "prompt_id": "p1"}),
        );
        std::fs::write(
            &transcript,
            json!({"type": "user", "uuid": "u1", "message": {"role": "user",
                   "content": [{"type": "text", "text": "[Request interrupted by user]"}]}})
            .to_string()
                + "\n",
        )
        .unwrap();
        wait_for("Esc 後是 idle", || status() == "idle");
        wait_for("idle 寫進 store", || stored_status() == "idle");
        // flush 先寫 JSONL、再在 SQLite 的交易裡記 tail 讀到的行號：只看 JSONL 就殺掉的話，可能殺在兩者之間，
        // 重啟後從舊的行號重讀那行中斷，狀態在新的 prompt 之後又回到 idle（負載高時偶發，T-4）
        let tail_line = || {
            rusqlite::Connection::open_with_flags(
                e.home.join("index.sqlite"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .and_then(|db| {
                db.query_row(
                    "SELECT line_no FROM tails WHERE session = 'si' AND file = 'main'",
                    [],
                    |r| r.get::<_, Option<i64>>(0),
                )
            })
            .ok()
            .flatten()
        };
        wait_for("讀到的行號寫進索引", || tail_line() == Some(1));
    }
    let _second = start_serve(&e);
    assert_eq!(status(), "idle", "前提：重啟後重放出 idle");
    hook(
        "UserPromptSubmit",
        json!({"prompt": "二", "prompt_id": "p2"}),
    );
    wait_for("新的 prompt 讓 live 回到 run", || status() == "run");
    wait_for("store 也回到 run", || stored_status() == "run");
}

/// #685：hook 埠放掉之後，新的 collector 要等舊的寫完 store 才開（store 鎖），不能兩個同時寫。
#[test]
fn serve_waits_for_the_store_lock_before_opening_the_store() {
    let e = env(0);
    let lock = std::fs::File::create(e.home.join("collector.lock")).unwrap();
    lock.lock().unwrap();
    let child = Kill(cmd(&e, &["serve"]).spawn().unwrap());
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        collector(&e).is_none(),
        "還拿不到 store 鎖時不能開始服務（collector.json 不能出現）"
    );
    lock.unlock().unwrap();
    let pid = child.0.id();
    wait_for("拿到鎖後開始服務", || {
        collector(&e).is_some_and(|c| c["pid"] == pid)
    });
}

/// 模擬 #685 閒置結束中的舊 collector：拿著 store 鎖、collector.json 標 `closing`；hook 埠上 ensure 的轉送回 503
/// （`reset` 時改成接了就斷線，像埠剛被放掉時 backlog 裡的連線被 RST），然後放掉埠；過一會刪 collector.json、放鎖，
/// 和真正的結束順序一樣（contracts §5）。
fn old_collector_closing(e: &Env, old: TcpListener, reset: bool) -> std::thread::JoinHandle<()> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(e.home.join("collector.lock"))
        .unwrap();
    lock.lock().unwrap();
    let info = e.home.join("collector.json");
    std::fs::write(
        &info,
        json!({"pid": 999_998, "hook": format!("127.0.0.1:{}", e.hook), "web": format!("127.0.0.1:{}", e.web),
               "closing": true}).to_string(),
    )
    .unwrap();
    std::thread::spawn(move || {
        // reset 時接了連線就丟掉（不回應），像埠剛被放掉時 backlog 裡的連線被 RST
        if let Some(Ok(mut c)) = old.incoming().next()
            && !reset
        {
            c.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf);
            let _ = c.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
        }
        drop(old);
        // 放掉埠之後還在 flush、關 SQLite
        std::thread::sleep(Duration::from_millis(400));
        let _ = std::fs::remove_file(&info);
        drop(lock);
    })
}

/// #685：ensure 轉送時拿到 503（舊的 collector 正在閒置結束）：拉起新的，新的等舊的放掉 hook 埠就先綁上收 hook、
/// 等舊的放鎖才開 store；SessionStart 轉送到新的那一個。
#[test]
fn ensure_hands_over_to_a_new_collector_when_the_old_one_is_exiting() {
    for reset in [false, true] {
        let (old, port) = bound_free_port();
        let e = env_on(5, port);
        let exiting = old_collector_closing(&e, old, reset);
        let start =
            json!({"session_id": "s7", "hook_event_name": "SessionStart", "source": "startup",
                           "transcript_path": e.home.join("t7.jsonl"), "cwd": "/p"})
            .to_string();
        let t = Instant::now();
        let out = ensure(&e, &start);
        assert!(out.status.success() && out.stdout.is_empty());
        assert!(t.elapsed() < Duration::from_secs(5), "不能卡住 Claude Code");
        exiting.join().unwrap();
        let jsonl = e.home.join("events/s7.jsonl");
        wait_for("SessionStart 轉送到新的 collector", || {
            std::fs::read_to_string(&jsonl).is_ok_and(|t| t.contains("session.started"))
        });
        if let Some(c) = collector(&e) {
            kill_pid(c["pid"].as_u64().unwrap());
        }
    }
}

// ── collector 在不在跑（contracts §5，#738）：鎖＝在跑、collector.json＝位址、collector.failed＝起不來

fn failed(e: &Env) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(e.home.join("collector.failed")).ok()?).ok()
}

/// 背景的 collector 寫好 collector.failed：ensure 可能比它先回來（例如佔著埠的程式接了連線不回應，
/// ensure 轉送逾時就結束），負載高時 collector 判斷「埠被別的程式佔用」也要更久（T-4）
fn wait_failed(e: &Env) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(f) = failed(e) {
            return f;
        }
        assert!(Instant::now() < deadline, "要寫 collector.failed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

const SESSION_START: &str =
    r#"{"session_id":"s","hook_event_name":"SessionStart","source":"startup","cwd":"/p"}"#;

/// L-1：最近 60 秒內起不來過，ensure 不再拉起、立刻結束（不讓每個 SessionStart 都卡 4 秒）。
#[test]
fn ensure_does_not_respawn_right_after_a_failed_start() {
    let e = env(5);
    std::fs::write(
        e.home.join("collector.failed"),
        json!({"ts": now_ms(), "error": "測試：起不來"}).to_string(),
    )
    .unwrap();
    let t = Instant::now();
    let out = ensure(&e, SESSION_START);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(collector(&e).is_none(), "不能再拉起 collector");
    assert!(
        TcpStream::connect(("127.0.0.1", e.hook)).is_err(),
        "不能再拉起 collector"
    );
    // 超過 60 秒就照常再試
    std::fs::write(
        e.home.join("collector.failed"),
        json!({"ts": now_ms() - 61_000, "error": "很久以前"}).to_string(),
    )
    .unwrap();
    let out = ensure(&e, SESSION_START);
    assert!(out.status.success());
    wait_for("照常拉起", || collector(&e).is_some());
    kill_pid(collector(&e).unwrap()["pid"].as_u64().unwrap());
}

/// L-1：store 比支援的新（降版之後），collector 起不來：ensure 很快結束、不等滿 4 秒，
/// collector.failed 寫清楚是哪個檔；hook 埠也不綁（綁了就會回 200、把 hook 丟掉）。
#[test]
fn ensure_gives_up_fast_when_the_store_is_from_a_newer_version() {
    let e = env(5);
    let db = rusqlite::Connection::open(e.home.join("index.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version', '99');",
    )
    .unwrap();
    drop(db);
    let t = Instant::now();
    let out = ensure(&e, SESSION_START);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let f = wait_failed(&e);
    assert!(
        f["file"]
            .as_str()
            .is_some_and(|p| p.contains("index.sqlite")),
        "{f}"
    );
    assert!(f["error"].as_str().is_some_and(|m| m.contains("99")), "{f}");
    assert!(
        TcpStream::connect(("127.0.0.1", e.hook)).is_err(),
        "沒有 collector 綁著 hook 埠"
    );
    // 接下來的 SessionStart 不再等
    let t = Instant::now();
    let _ = ensure(&e, SESSION_START);
    assert!(t.elapsed() < Duration::from_millis(500));
}

/// L-1：hook 埠被別的程式佔著（Windows 的保留埠也是）：很快結束，collector.failed 帶位址。
#[test]
fn ensure_gives_up_fast_when_the_hook_port_belongs_to_another_program() {
    let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
    let e = env_on(0, squatter.local_addr().unwrap().port());
    let t = Instant::now();
    let out = ensure(&e, SESSION_START);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let f = wait_failed(&e);
    assert_eq!(f["addr"], format!("127.0.0.1:{}", e.hook), "{f}");
    drop(squatter);
}

/// L-4：web 埠綁不到時 hook 照收、照寫 store；之後定期重試，埠空出來就開始服務。
#[test]
fn hooks_are_kept_while_the_web_port_is_taken() {
    let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
    let web = squatter.local_addr().unwrap().port();
    let e = env(0);
    std::fs::write(
        e.home.join("config.toml"),
        format!(
            "[hook]\nbind = \"127.0.0.1:{}\"\n[web]\nbind = \"127.0.0.1:{web}\"\n",
            e.hook
        ),
    )
    .unwrap();
    let mut s = Kill(cmd(&e, &["serve"]).spawn().unwrap());
    wait_for("collector.json（web 還沒綁上）", || {
        collector(&e).is_some()
    });
    let start = json!({"session_id": "w1", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("tw.jsonl"), "cwd": "/p"});
    assert!(http(e.hook, "POST", "/hook", &start.to_string()).starts_with("HTTP/1.1 200"));
    wait_for("hook 進 store", || {
        std::fs::read_to_string(e.home.join("events/w1.jsonl"))
            .is_ok_and(|t| t.contains("session.started"))
    });
    assert!(
        s.0.try_wait().unwrap().is_none(),
        "collector 不能因為 web 埠就結束"
    );
    drop(squatter);
    wait_for("web 埠空出來後開始服務", || {
        TcpStream::connect(("127.0.0.1", web))
            .is_ok_and(|_| http(web, "GET", "/healthz", "").starts_with("HTTP/1.1 200"))
    });
    wait_for("collector.json 補上 web", || {
        collector(&e).is_some_and(|c| c["web"] == format!("127.0.0.1:{web}"))
    });
}

/// R-11：沒有 collector.json（也沒有人拿著鎖）時，stop 與 healthz 不能去碰設定裡的預設埠：
/// 那可能是別的資料目錄的 collector（2026-10-18 審查時就這樣停掉了自我追蹤的 collector）。
#[test]
fn stop_and_healthz_never_touch_a_port_without_collector_json() {
    let hook = TcpListener::bind("127.0.0.1:0").unwrap();
    let web = TcpListener::bind("127.0.0.1:0").unwrap();
    let e = env(0);
    std::fs::write(
        e.home.join("config.toml"),
        format!(
            "[hook]\nbind = \"{}\"\n[web]\nbind = \"{}\"\n",
            hook.local_addr().unwrap(),
            web.local_addr().unwrap()
        ),
    )
    .unwrap();
    hook.set_nonblocking(true).unwrap();
    web.set_nonblocking(true).unwrap();
    let out = cmd(&e, &["stop"]).stderr(Stdio::piped()).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("沒有在跑"));
    let out = cmd(&e, &["healthz"])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("沒有在跑"));
    assert!(hook.accept().is_err(), "stop 連了設定裡的 hook 埠");
    assert!(web.accept().is_err(), "healthz 連了設定裡的 web 埠");
}

/// L-13：stop 回報結束時，SQLite 已經關了、鎖也放了（新的 collector 馬上就能開 store）。
#[test]
fn stop_reports_only_after_the_store_lock_is_released() {
    let e = env(0);
    let mut s = start_serve(&e);
    let out = cmd(&e, &["stop"]).output().unwrap();
    assert!(out.status.success());
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(e.home.join("collector.lock"))
        .unwrap();
    assert!(
        lock.try_lock().is_ok(),
        "stop 回來時 collector.lock 要已經放掉"
    );
    assert!(collector(&e).is_none());
    assert!(
        !e.home.join("index.sqlite-wal").exists(),
        "SQLite 要已經關閉"
    );
    let _ = s.0.wait();
}

fn old_event(session: &str, seq: u64, kind: &str, ts: u64) -> coding_lab::event::Event {
    ev_with(
        session,
        seq,
        kind,
        None,
        ts,
        json!({"cwd": "/p", "source": "startup"}),
    )
}

fn seed_old_and_recent(e: &Env) -> u64 {
    let day = 24 * 60 * 60 * 1000;
    let now = coding_lab::now_ms();
    let mut s = coding_lab::store::Store::open(&e.home).unwrap();
    s.append(old_event("old", 1, "session.started", now - 31 * day));
    s.append(old_event("recent", 1, "session.started", now - day));
    s.flush().unwrap();
    now
}

/// #655：`coding-lab prune` 清掉 30 天前的 session，近期的留著。
#[test]
fn prune_command_removes_sessions_older_than_30_days() {
    let e = env(0);
    seed_old_and_recent(&e);
    let out = cmd(&e, &["prune"]).stderr(Stdio::piped()).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!e.home.join("events/old.jsonl").exists());
    assert!(e.home.join("events/recent.jsonl").exists());
}

/// #655：collector 在跑時 prune 不碰 store（兩個程序同時寫），說明 collector 自己會清。
#[test]
fn prune_command_refuses_while_the_collector_runs() {
    let e = env(0);
    seed_old_and_recent(&e);
    let _serve = start_serve(&e);
    let out = cmd(&e, &["prune"]).stderr(Stdio::piped()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("collector"));
}

/// #655：collector 啟動時自己清一次 30 天前的資料。
#[test]
fn serve_prunes_old_sessions_on_start() {
    let e = env(0);
    seed_old_and_recent(&e);
    let _serve = start_serve(&e);
    wait_for("啟動時清掉 old", || {
        !e.home.join("events/old.jsonl").exists()
    });
    assert!(e.home.join("events/recent.jsonl").exists());
}

/// #655：背景執行（stderr 不是終端機）時 collector 自己寫 collector.log。
#[test]
fn detached_serve_writes_its_own_log() {
    let e = env(0);
    let _serve = start_serve(&e);
    wait_for("collector.log 有啟動訊息", || {
        std::fs::read_to_string(e.home.join("collector.log"))
            .is_ok_and(|t| t.contains("hook 127.0.0.1"))
    });
}

/// #685（驗證找到的缺口）：probe 連上之後、轉送之前舊的 collector 就放掉了埠——轉送被拒或被 RST，
/// ensure 要在預算內改拉新的 collector，不能直接放棄（SessionStart 會掉）。
/// #685：進入「結束中」（ensure 轉送回 503）之後有一般 hook 進來，取消結束、繼續服務。
#[test]
fn a_hook_during_closing_cancels_the_idle_exit() {
    let e = env(1);
    let mut s = start_serve(&e);
    let via_ensure = |body: &str| {
        let mut c = TcpStream::connect(("127.0.0.1", e.hook)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        write!(c, "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nX-Coding-Lab-Via: ensure\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out
    };
    // 壞掉的 body 不會變成 hook（不重設閒置計時），只用來看 collector 進入結束中了沒
    wait_for("進入結束中", || {
        via_ensure("x").starts_with("HTTP/1.1 503")
    });
    let stop = json!({"session_id": "s1", "hook_event_name": "Stop"});
    assert!(http(e.hook, "POST", "/hook", &stop.to_string()).starts_with("HTTP/1.1 200"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        s.0.try_wait().unwrap().is_none(),
        "結束中收到 hook 要取消結束"
    );
    assert!(
        via_ensure("x").starts_with("HTTP/1.1 200"),
        "取消後 ensure 轉送照常收"
    );
}

fn text_line(uuid: &str, msg: &str, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type": "assistant", "uuid": uuid, "timestamp": "2026-10-17T00:00:00Z",
               "message": {"id": msg, "model": "claude-opus-5-5",
                           "content": [{"type": "text", "text": text}]}})
    )
}

/// #684 驗收：collector 停掉期間 transcript 寫進的行，重啟後從 store 記的行號接著讀，進串流且不重複。
#[test]
fn lines_written_while_the_collector_was_down_are_read_after_restart() {
    let e = env(0);
    let transcript = e.home.join("t5.jsonl");
    std::fs::write(
        &transcript,
        text_line("u1", "m1", "一") + &text_line("u2", "m2", "二"),
    )
    .unwrap();
    let jsonl = e.home.join("events/s5.jsonl");
    let texts = || -> Vec<String> {
        std::fs::read_to_string(&jsonl)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["type"] == "assistant.text")
            .map(|v| {
                v["payload"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    };
    {
        let _first = start_serve(&e);
        let start = json!({"session_id": "s5", "hook_event_name": "SessionStart", "source": "startup",
                           "transcript_path": transcript, "cwd": "/p"});
        http(e.hook, "POST", "/hook", &start.to_string());
        wait_for("前兩行進 store", || texts().len() == 2);
    } // Kill：collector 停掉
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    f.write_all((text_line("u3", "m3", "三") + &text_line("u4", "m4", "四")).as_bytes())
        .unwrap();
    drop(f);

    let _second = start_serve(&e);
    let prompt = json!({"session_id": "s5", "hook_event_name": "UserPromptSubmit", "prompt": "繼續",
                        "transcript_path": transcript, "cwd": "/p"});
    http(e.hook, "POST", "/hook", &prompt.to_string());
    wait_for("停機期間的兩行補進來", || texts().len() >= 4);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(texts(), ["一", "二", "三", "四"], "不漏、不重複");
}

/// #652 驗收：transcript 被弄壞後，hook 的工具事件照樣進串流（降級放置），恢復後回到正常。
#[test]
fn broken_transcript_still_shows_tool_events() {
    let e = env(0);
    let _serve = start_serve(&e);
    let transcript = e.home.join("t6.jsonl");
    std::fs::write(&transcript, "{壞掉的一行\n").unwrap();
    let start = json!({"session_id": "s6", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": transcript, "cwd": "/p"});
    http(e.hook, "POST", "/hook", &start.to_string());
    let jsonl = e.home.join("events/s6.jsonl");
    let types = || -> Vec<Value> {
        std::fs::read_to_string(&jsonl)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    };
    wait_for("transcript.degraded", || {
        types().iter().any(|v| v["type"] == "transcript.degraded")
    });
    let pre = json!({"session_id": "s6", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                     "tool_input": {"command": "ls"}, "tool_use_id": "t1", "transcript_path": transcript});
    http(e.hook, "POST", "/hook", &pre.to_string());
    wait_for("工具卡片直接進串流", || {
        types()
            .iter()
            .any(|v| v["type"] == "tool.placed" && v["payload"]["placement"] == "fallback")
    });
    // 之後寫進好的行：恢復
    let good = json!({"type": "assistant", "uuid": "u1", "timestamp": "2026-10-17T00:00:00Z",
                      "message": {"id": "m1", "model": "claude-opus-5-5",
                                  "content": [{"type": "text", "text": "好了"}]}});
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    writeln!(f, "{good}").unwrap();
    wait_for("transcript.recovered", || {
        types().iter().any(|v| v["type"] == "transcript.recovered")
    });
}

/// `coding-lab stop`（#700）：背景 collector 沒有 console、按不到 Ctrl-C，要有一條三平台都通的正常結束路徑——
/// 放掉 hook 埠、flush store、刪 collector.json、程序 exit 0；不是 pkill／taskkill 那種硬殺。
#[test]
fn stop_subcommand_shuts_the_collector_down_cleanly() {
    let e = env(0);
    let mut s = start_serve(&e);
    let start = json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("t1.jsonl"), "cwd": "/p"});
    assert!(http(e.hook, "POST", "/hook", &start.to_string()).starts_with("HTTP/1.1 200"));

    let out = cmd(&e, &["stop"]).output().unwrap();
    assert!(out.status.success(), "stop 要 exit 0");
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(st) = s.0.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "stop 之後 collector 要自己結束");
        std::thread::sleep(Duration::from_millis(30));
    };
    assert!(status.success(), "正常結束，不是被殺");
    assert!(collector(&e).is_none(), "結束時移除 collector.json");
    assert!(
        TcpStream::connect(("127.0.0.1", e.hook)).is_err(),
        "stop 回來時 hook 埠已放掉"
    );
    // 結束前有 flush：剛送的 SessionStart 在 store 裡
    let text = std::fs::read_to_string(e.home.join("events/s1.jsonl")).unwrap_or_default();
    assert!(text.contains("session.started"), "{text}");
}

/// #727：結束時先關 SQLite、才放 `collector.lock`。WAL 模式下連線開著時 `index.sqlite-wal` 一直在，
/// 最後一條連線關閉（checkpoint 完）才刪：一拿到鎖時 -wal 必須已經不在。順序反了的話，ensure 拉起的
/// 新 collector 會在舊的還在 checkpoint 時開 store，I/O 慢時超過 busy timeout 就以 database is locked 起不來。
#[test]
fn store_is_closed_before_the_store_lock_is_released() {
    let e = env(0);
    let _serve = start_serve(&e);
    // WAL 裡有一些內容，關閉時的 checkpoint 才要花點時間、順序錯時看得到
    let input = "x".repeat(4000);
    for i in 0..300 {
        let pre = json!({"session_id": "s727", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                         "tool_input": {"command": input}, "tool_use_id": format!("t{i}"),
                         "transcript_path": e.home.join("t727.jsonl"), "cwd": "/p"});
        http(e.hook, "POST", "/hook", &pre.to_string());
    }
    let wal = e.home.join("index.sqlite-wal");
    assert!(wal.exists(), "前提：WAL 模式、連線開著時有 -wal");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(e.home.join("collector.lock"))
        .unwrap();
    let home = e.home.clone();
    let stopper = std::thread::spawn(move || {
        Command::new(env!("CARGO_BIN_EXE_coding-lab"))
            .arg("stop")
            .env("CODING_LAB_HOME", home)
            .output()
            .unwrap()
    });
    let deadline = Instant::now() + DEADLINE;
    while lock.try_lock().is_err() {
        assert!(Instant::now() < deadline, "stop 後 collector.lock 要放掉");
        std::hint::spin_loop();
    }
    let wal_left = wal.exists();
    assert!(stopper.join().unwrap().status.success());
    assert!(
        !wal_left,
        "拿到 collector.lock 時 SQLite 還沒關（index.sqlite-wal 還在）"
    );
}

/// collector 沒在跑時 `stop` 也是 exit 0，只在 stderr 說明（手冊裡的更新與卸載步驟可以無條件先跑它）。
#[test]
fn stop_without_a_collector_exits_zero() {
    let e = env(0);
    let out = cmd(&e, &["stop"]).stderr(Stdio::piped()).output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("沒有在跑"));
}

// ── ensure 不能把呼叫端的 std handle 交給 collector（#712）

/// 像 Claude Code 或 `| cat` 那樣，stdin／stdout／stderr 三個都接 pipe 跑指令：
/// 回 (結束狀態, stdout 收到 EOF 的時間, stderr 收到 EOF 的時間)；3 秒內沒收到 EOF 的是 `None`。
/// 現有的 `cmd()` 把 stderr 設成 null，測不出 stderr 外洩，所以另外寫。
fn run_piped(
    mut c: Command,
    stdin: &str,
) -> (std::process::ExitStatus, Option<Duration>, Option<Duration>) {
    let t = Instant::now();
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let eof_of = |mut r: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = tx.send((t.elapsed(), buf));
        });
        rx
    };
    let out = eof_of(Box::new(child.stdout.take().unwrap()));
    let err = eof_of(Box::new(child.stderr.take().unwrap()));
    let status = child.wait().unwrap();
    let wait = Duration::from_secs(3);
    let out = out.recv_timeout(wait).ok();
    let err = err.recv_timeout(wait).ok();
    assert!(
        out.as_ref().is_none_or(|(_, b)| b.is_empty()),
        "stdout 會進 Claude 的上下文，必須是空的"
    );
    (status, out.map(|(d, _)| d), err.map(|(d, _)| d))
}

fn assert_pipes_released(
    e: &Env,
    status: std::process::ExitStatus,
    out: Option<Duration>,
    err: Option<Duration>,
) {
    assert!(status.success());
    assert!(
        out.is_some(),
        "stdout 3 秒內要收到 EOF：collector 握著 stdout 的寫入端"
    );
    assert!(
        err.is_some(),
        "stderr 3 秒內要收到 EOF：collector 握著 stderr 的寫入端"
    );
    // ensure 只等 hook 埠就結束，collector.json 是 collector 開好 store、綁好 web 之後才寫的（#725）
    wait_for("ensure 拉起的 collector 寫好 collector.json", || {
        collector(e).is_some()
    });
    let c = collector(e).expect("ensure 拉起的 collector 要還在跑");
    assert!(
        http(e.web, "GET", "/healthz", "").starts_with("HTTP/1.1 200"),
        "collector {} 要還活著",
        c["pid"]
    );
    let stop = cmd(e, &["stop"]).output().unwrap();
    assert!(stop.status.success());
    wait_for("stop 後 collector 結束、移除 collector.json", || {
        collector(e).is_none()
    });
}

/// Windows 的子程序會繼承所有可繼承的 handle：ensure 拉起 collector 時，呼叫端給 ensure 的 pipe
/// 不能跟著進 collector，否則呼叫端要等到 collector 結束才收到 EOF（#712）。Linux 與 macOS 本來就會過。
#[test]
fn ensure_releases_the_callers_std_pipes_while_the_collector_keeps_running() {
    let e = env(30);
    let start =
        json!({"session_id": "s712", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("t712.jsonl"), "cwd": "/p"})
        .to_string();
    let mut c = Command::new(env!("CARGO_BIN_EXE_coding-lab"));
    c.arg("ensure").env("CODING_LAB_HOME", &e.home);
    let (status, out, err) = run_piped(c, &start);
    assert_pipes_released(&e, status, out, err);
}

/// ensure 只等 hook 埠綁好就結束，collector 開 store、綁 web、寫 collector.json 是之後的事（#725）：
/// 機器負載高時 ensure 結束那一刻 collector.json 可能還不在。這裡用 store 鎖把那段空檔撐開——
/// collector 綁好 hook 埠後停在等鎖，ensure 照樣結束；放掉鎖後 collector 要接著起來。
#[test]
fn ensure_returns_before_the_collector_has_written_its_info() {
    let e = env(30);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(e.home.join("collector.lock"))
        .unwrap();
    lock.lock().unwrap();
    let start =
        json!({"session_id": "s725", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("t725.jsonl"), "cwd": "/p"})
        .to_string();
    let mut c = Command::new(env!("CARGO_BIN_EXE_coding-lab"));
    c.arg("ensure").env("CODING_LAB_HOME", &e.home);
    let (status, out, err) = run_piped(c, &start);
    assert!(
        collector(&e).is_none(),
        "還拿不到 store 鎖時不會有 collector.json"
    );
    drop(lock);
    assert_pipes_released(&e, status, out, err);
}

/// 同一件事經過 plugin 的 wrapper（Claude Code 實際的呼叫路徑）：wrapper 自己也不能把 pipe 交給 ensure。
/// Windows 用 Git Bash 的 sh（不一定在 PATH 上，#783）；找不到時印出原因後略過，CODING_LAB_REQUIRE_SH=1 時失敗。
#[test]
fn wrapper_ensure_releases_the_callers_std_pipes_too() {
    let Some(sh) = sh::sh_or_skip("wrapper_ensure_releases_the_callers_std_pipes_too") else {
        return;
    };
    let e = env(30);
    let start =
        json!({"session_id": "s713", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("t713.jsonl"), "cwd": "/p"})
        .to_string();
    // 真的 coding-lab.exe 是 Windows 原生程式：它讀的路徑照原樣給，不換成 MSYS 的
    let mut c = sh::wrapper_cmd(&sh);
    c.arg("ensure")
        .env("CODING_LAB_BIN", env!("CARGO_BIN_EXE_coding-lab"))
        .env("CODING_LAB_HOME", &e.home);
    let (status, out, err) = run_piped(c, &start);
    assert_pipes_released(&e, status, out, err);
}

// ── #713：collector 當掉不能掉 hook 事件；還沒 flush 的 transcript 行重啟後要補回來

/// 像 `start_serve`，但 store 的批次 flush 調成 60 秒：測試裡「還沒 flush 就當掉」的時序由此控制，不靠 sleep。
fn start_serve_slow_flush(e: &Env) -> Kill {
    let child = cmd(e, &["serve"])
        .env("CODING_LAB_STORE_FLUSH_MS", "60000")
        .spawn()
        .unwrap();
    let pid = child.id();
    wait_for("collector.json", || {
        collector(e).is_some_and(|c| c["pid"] == pid)
    });
    Kill(child)
}

/// live 裡的 session 列（WS `hello` 後 `subscribe *` 的 snapshot）：`/api/sessions` 讀的是 store，
/// 看不到還沒 flush 的部分，這裡要的是「merge 已經處理」的訊號。
fn live_session(e: &Env, id: &str) -> Value {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let Ok(Ok((mut ws, _))) = tokio::time::timeout(
            Duration::from_secs(2),
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws", e.web)),
        )
        .await
        else {
            return Value::Null;
        };
        for v in [
            json!({"op": "hello", "protocol": 1}),
            json!({"op": "subscribe", "channel": "*"}),
        ] {
            let _ = ws.send(Message::Text(v.to_string().into())).await;
        }
        for _ in 0..8 {
            let Ok(Some(Ok(Message::Text(t)))) =
                tokio::time::timeout(Duration::from_secs(2), ws.next()).await
            else {
                return Value::Null;
            };
            let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
            if v["t"] == "snapshot"
                && let Some(rows) = v["sessions"].as_array()
            {
                return rows
                    .iter()
                    .find(|r| r["id"] == id)
                    .cloned()
                    .unwrap_or(Value::Null);
            }
        }
        Value::Null
    })
}

fn session_row(e: &Env, id: &str) -> Value {
    json_body(&http(e.web, "GET", "/api/sessions", ""))["sessions"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == id).cloned())
        .unwrap_or(Value::Null)
}

/// hook 事件沒有別的來源可以補：merge 處理完就要寫進 store，不等 100 ms 的批次。
/// 這裡 flush 間隔是 60 秒，prompt 進了 live（merge 已處理）就 kill：JSONL 裡要已經有它。
#[test]
fn hook_events_are_stored_before_the_periodic_flush() {
    let e = env(30);
    let transcript = e.home.join("t713a.jsonl");
    std::fs::write(&transcript, "").unwrap();
    {
        let _c = start_serve_slow_flush(&e);
        http(
            e.hook,
            "POST",
            "/hook",
            &json!({"session_id": "sh", "hook_event_name": "SessionStart",
            "source": "startup", "transcript_path": transcript, "cwd": "/p"})
            .to_string(),
        );
        http(
            e.hook,
            "POST",
            "/hook",
            &json!({"session_id": "sh", "hook_event_name": "UserPromptSubmit",
            "transcript_path": transcript, "cwd": "/p", "prompt": "一"})
            .to_string(),
        );
        wait_for("prompt 進 live", || {
            live_session(&e, "sh")["lastPrompt"] == "一"
        });
    } // kill：模擬當掉
    let jsonl = std::fs::read_to_string(e.home.join("events/sh.jsonl")).unwrap_or_default();
    assert!(
        jsonl.contains("session.started"),
        "SessionStart 要在 store 裡：{jsonl}"
    );
    assert!(
        jsonl.contains("\"一\""),
        "當掉前 merge 已處理的 prompt 要在 store 裡：{jsonl}"
    );
}

/// transcript 的行可以補：session.started 已寫進 store（tails 記 0＝從頭讀、還沒讀到行），之後讀到的行還沒
/// flush 就當掉，重啟後 tail 要從第 1 行再讀，usage 與 assistant 內容補回來，而不是從檔尾開始、永遠漏掉。
#[test]
fn transcript_lines_lost_in_a_crash_are_reread_after_a_restart() {
    let e = env(30);
    let transcript = e.home.join("t713b.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let line = json!({"type": "assistant", "uuid": "u1", "timestamp": "2026-10-16T00:00:00Z",
                      "message": {"id": "m713", "model": "claude-opus-5-5",
                                  "content": [{"type": "text", "text": "好。"}],
                                  "usage": {"input_tokens": 1_000_000, "output_tokens": 0}}});
    let before = {
        let _c = start_serve_slow_flush(&e);
        http(e.hook, "POST", "/hook", &json!({"session_id": "st", "hook_event_name": "SessionStart",
            "source": "startup", "transcript_path": transcript, "cwd": "/p", "model": "claude-opus-5-5"}).to_string());
        wait_for("session 進 live", || live_session(&e, "st")["id"] == "st");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap()
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
        wait_for("usage 進 live", || {
            live_session(&e, "st")["costUsd"]
                .as_f64()
                .is_some_and(|c| c > 0.0)
        });
        live_session(&e, "st")
    }; // kill：usage 那行的 Event 還在緩衝裡
    let jsonl = std::fs::read_to_string(e.home.join("events/st.jsonl")).unwrap_or_default();
    assert!(jsonl.contains("session.started"), "{jsonl}");
    assert!(
        !jsonl.contains("m713"),
        "前提：當掉時那行還沒 flush：{jsonl}"
    );

    let _second = start_serve(&e);
    // session 還在跑：下一個 hook 讓 collector 重開 tail（從 tails 記的 0 之後，也就是第 1 行）
    http(
        e.hook,
        "POST",
        "/hook",
        &json!({"session_id": "st", "hook_event_name": "UserPromptSubmit",
        "transcript_path": transcript, "cwd": "/p", "prompt": "二"})
        .to_string(),
    );
    wait_for("重啟後從第 1 行補回 usage", || {
        session_row(&e, "st")["costUsd"]
            .as_f64()
            .is_some_and(|c| c > 0.0)
    });
    wait_for("補回的行寫進 store", || {
        std::fs::read_to_string(e.home.join("events/st.jsonl")).is_ok_and(|t| t.contains("m713"))
    });
    let after = session_row(&e, "st");
    for k in ["costUsd", "model", "cwd"] {
        assert_eq!(after[k], before[k], "{k}：重啟前 {before}\n重啟後 {after}");
    }
}

/// #722：`[web].bind` 是陣列時每個位址都提供服務，collector.json 的 `web` 是第一個、`webs` 全部。
#[test]
fn web_bind_list_serves_every_address() {
    let e = env(0);
    let second = free_port();
    std::fs::write(
        e.home.join("config.toml"),
        format!(
            "[hook]\nbind = \"127.0.0.1:{}\"\n[web]\nbind = [\"127.0.0.1:{}\", \"127.0.0.1:{second}\"]\n[collector]\nidle_exit_seconds = 0\n",
            e.hook, e.web
        ),
    )
    .unwrap();
    let _serve = start_serve(&e);
    assert!(
        http(e.web, "GET", "/healthz", "").starts_with("HTTP/1.1 200"),
        "第一個位址"
    );
    assert!(
        http(second, "GET", "/healthz", "").starts_with("HTTP/1.1 200"),
        "第二個位址"
    );
    let c = collector(&e).unwrap();
    assert_eq!(c["web"], format!("127.0.0.1:{}", e.web));
    assert_eq!(
        c["webs"],
        json!([
            format!("127.0.0.1:{}", e.web),
            format!("127.0.0.1:{second}")
        ])
    );
}

/// S-6：collector 自己寫的 collector.json、collector.log 也是 0600。
#[cfg(unix)]
#[test]
fn collector_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let e = env(0);
    let mut c = cmd(&e, &["serve"]);
    // 背景執行才會寫 collector.log（stderr 不是終端機）
    let _s = Kill(c.spawn().unwrap());
    wait_for("collector.json", || collector(&e).is_some());
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&e.home), 0o700);
    assert_eq!(mode(&e.home.join("collector.json")), 0o600);
    assert_eq!(mode(&e.home.join("collector.log")), 0o600);
}

/// L-6（#740）：tail 補讀大量行時塞滿輸入佇列，hook 原本被 try_send 丟掉（實測 100 個丟 32 個）。
/// hook 走自己的 channel、主迴圈先處理 hook：另一個 session 的 hook 一個都不丟；/healthz 有 dropped。
#[test]
fn hooks_are_not_dropped_while_a_tail_catches_up() {
    let e = env(0);
    let transcript = e.home.join("big.jsonl");
    let mut text = String::new();
    for i in 0..60_000 {
        text.push_str(&text_line(&format!("u{i}"), &format!("m{i}"), "補讀"));
    }
    std::fs::write(&transcript, text).unwrap();
    let _s = start_serve(&e);
    let start = json!({"session_id": "big", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": transcript, "cwd": "/p"});
    http(e.hook, "POST", "/hook", &start.to_string());
    for i in 0..100 {
        let p = json!({"session_id": "other", "hook_event_name": "UserPromptSubmit",
                       "prompt": format!("p{i}"), "cwd": "/q"});
        assert!(http(e.hook, "POST", "/hook", &p.to_string()).starts_with("HTTP/1.1 200"));
    }
    let jsonl = e.home.join("events/other.jsonl");
    let prompts = || {
        std::fs::read_to_string(&jsonl)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("prompt.submitted"))
            .count()
    };
    wait_for("100 個 prompt 都進 store", || prompts() == 100);
    let h = http(e.web, "GET", "/healthz", "");
    let v: Value = serde_json::from_str(h.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap();
    assert_eq!(v["dropped"], 0, "{v}");
}

/// L-7（#740）：回放與 events 請求並行、web 的 worker 很忙時，hook 仍然很快回 200（不變式 4）。
/// hook 入口在自己的執行緒上，不和 web 搶 worker。
#[test]
fn hooks_stay_fast_while_web_requests_pile_up() {
    let e = env(0);
    let _s = start_serve(&e);
    let start = json!({"session_id": "busy", "hook_event_name": "SessionStart", "source": "startup",
                       "transcript_path": e.home.join("tb.jsonl"), "cwd": "/p"});
    http(e.hook, "POST", "/hook", &start.to_string());
    for i in 0..200 {
        let p = json!({"session_id": "busy", "hook_event_name": "UserPromptSubmit", "prompt": format!("p{i}"), "cwd": "/p"});
        http(e.hook, "POST", "/hook", &p.to_string());
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let web = e.web;
    let load: Vec<_> = (0..16)
        .map(|i| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let path = if i % 2 == 0 {
                    "/api/projects/replay?cwd=/p&at=0&until=99999999999999"
                } else {
                    "/api/sessions/busy/events?limit=500"
                };
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = http(web, "GET", path, "");
                }
            })
        })
        .collect();
    let mut worst = Duration::ZERO;
    for i in 0..30 {
        let body = json!({"session_id": "other", "hook_event_name": "UserPromptSubmit", "prompt": format!("q{i}"), "cwd": "/q"});
        let t = Instant::now();
        assert!(http(e.hook, "POST", "/hook", &body.to_string()).starts_with("HTTP/1.1 200"));
        worst = worst.max(t.elapsed());
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for t in load {
        let _ = t.join();
    }
    // 預算是 100 ms；check.sh 並行跑時機器很忙，留一點餘裕，測的是「不會被 web 卡住幾秒」
    assert!(
        worst < Duration::from_millis(500),
        "最慢一個 hook 花了 {worst:?}"
    );
}

/// #777：/stop 要帶 collector.json 的 stopToken；別人（另一個帳號讀不到這個檔）送的被拒、collector 照常。
/// healthz 帶 user，看得出是誰的 collector。
#[test]
fn stop_needs_the_token_from_collector_json() {
    let e = env(0);
    let mut s = start_serve(&e);
    let info = collector(&e).unwrap();
    let token = info["stopToken"]
        .as_str()
        .expect("collector.json 要有 stopToken")
        .to_string();
    assert!(token.len() >= 32);
    let mut c = TcpStream::connect(("127.0.0.1", e.hook)).unwrap();
    write!(c, "POST /stop HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Coding-Lab-Stop: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = c.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        s.0.try_wait().unwrap().is_none(),
        "錯的 token 不能停掉 collector"
    );
    let h = http(e.web, "GET", "/healthz", "");
    let v: Value = serde_json::from_str(h.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap();
    assert!(v["user"].as_str().is_some_and(|u| !u.is_empty()), "{v}");
    // coding-lab stop 讀自己的 collector.json 帶上
    assert!(cmd(&e, &["stop"]).output().unwrap().status.success());
    wait_for("stop 之後結束", || s.0.try_wait().unwrap().is_some());
}

fn log_text(e: &Env) -> String {
    std::fs::read_to_string(e.home.join("collector.log")).unwrap_or_default()
}

/// W-L15（#781）：store 鎖被一個正常在跑的 collector 拿著時，等鎖的 log 說「另一個 collector 已經在用」，
/// 不是「上一個 collector 還在結束」。同一個 home、別的埠再起一個 serve。
#[test]
fn waiting_for_the_store_lock_names_the_running_collector() {
    let e = env(0);
    let _a = start_serve(&e);
    let b = Command::new(env!("CARGO_BIN_EXE_coding-lab"))
        .args([
            "serve",
            "--hook-bind",
            &format!("127.0.0.1:{}", free_port()),
        ])
        .args(["--web-bind", &format!("127.0.0.1:{}", free_port())])
        .env("CODING_LAB_HOME", &e.home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _b = Kill(b);
    wait_for("第二個 serve 寫等鎖的原因", || {
        log_text(&e).contains("另一個 collector 已經在用這個資料目錄")
    });
    assert!(
        !log_text(&e).contains("上一個 collector 還在結束"),
        "{}",
        log_text(&e)
    );
}

/// W-L15：`coding-lab prune` 拿著鎖時留標記檔，等鎖的 serve 說「prune 正在執行」。
#[test]
fn waiting_for_the_store_lock_names_a_running_prune() {
    let e = env(0);
    let lock = coding_lab::serve::try_lock_store(&e.home).unwrap().unwrap();
    std::fs::write(
        e.home.join(coding_lab::serve::PRUNE_MARKER),
        format!("{}", std::process::id()),
    )
    .unwrap();
    let child = cmd(&e, &["serve"]).spawn().unwrap();
    let _c = Kill(child);
    wait_for("serve 寫等鎖的原因", || {
        log_text(&e).contains("prune 正在執行")
    });
    drop(lock);
}

#[cfg(unix)]
fn healthz(e: &Env) -> Value {
    let r = http(e.web, "GET", "/healthz", "");
    serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap_or("{}")).unwrap_or(Value::Null)
}

/// W-M10（#781）：store 一直寫不進去（這裡把 session 的 JSONL 改成唯讀）時，`/healthz` 有 storeError、log 不會每個 hook
/// 一行；結束時沒寫進去的 hook 事件存進 `pending-*.jsonl`，下次啟動補回。
#[cfg(unix)]
#[test]
fn an_unwritable_store_is_reported_and_its_hooks_survive_a_stop() {
    use std::os::unix::fs::PermissionsExt;
    let e = env(0);
    let child = cmd(&e, &["serve"]).spawn().unwrap();
    let pid = child.id();
    let mut serve = Kill(child);
    wait_for("collector.json", || {
        collector(&e).is_some_and(|c| c["pid"] == pid)
    });
    assert!(
        healthz(&e)["storeError"].is_null(),
        "正常時是 null：{}",
        healthz(&e)
    );
    let start = json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "cwd": "/p"});
    http(e.hook, "POST", "/hook", &start.to_string());
    let jsonl = e.home.join("events/s1.jsonl");
    wait_for("session.started 進 store", || {
        std::fs::read_to_string(&jsonl).is_ok_and(|t| t.contains("session.started"))
    });
    std::fs::set_permissions(&jsonl, std::fs::Permissions::from_mode(0o400)).unwrap();
    for i in 0..50 {
        let pre = json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                         "tool_input": {"command": "ls"}, "tool_use_id": format!("locked-{i}"), "cwd": "/p"});
        http(e.hook, "POST", "/hook", &pre.to_string());
    }
    wait_for("healthz 的 storeError", || {
        healthz(&e)["storeError"]["pending"]
            .as_u64()
            .is_some_and(|n| n > 0)
    });
    let err = healthz(&e)["storeError"].clone();
    assert!(
        err["message"].is_string() && err["since"].is_u64() && err["dropped"] == 0,
        "{err}"
    );
    let failures = log_text(&e).matches("store 寫不進去").count();
    assert_eq!(failures, 1, "只在開始失敗時寫一行：{}", log_text(&e));

    let out = cmd(&e, &["stop"]).output().unwrap();
    assert!(out.status.success());
    let _ = serve.0.wait();
    std::fs::set_permissions(&jsonl, std::fs::Permissions::from_mode(0o600)).unwrap();
    let dumps: Vec<_> = std::fs::read_dir(&e.home)
        .unwrap()
        .flatten()
        .filter(|f| f.file_name().to_string_lossy().starts_with("pending-"))
        .collect();
    assert_eq!(dumps.len(), 1, "{}", log_text(&e));

    let _again = start_serve(&e);
    let text = std::fs::read_to_string(&jsonl).unwrap();
    assert!(
        text.contains("locked-0") && text.contains("locked-49"),
        "補回來了"
    );
    assert!(!dumps[0].path().exists(), "補完就刪");
}
