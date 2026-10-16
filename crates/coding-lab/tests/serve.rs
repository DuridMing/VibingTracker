//! `coding-lab serve` 與 `ensure` 端到端（#629）。每個測試用自己的資料目錄與埠，collector 會閒置自動結束。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
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
    let deadline = Instant::now() + Duration::from_secs(8);
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
    let deadline = Instant::now() + Duration::from_secs(8);
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

fn ensure(e: &Env, stdin: &str) -> std::process::Output {
    let mut c = cmd(e, &["ensure"]).stdin(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    c.wait_with_output().unwrap()
}

fn kill_pid(pid: u64) {
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
