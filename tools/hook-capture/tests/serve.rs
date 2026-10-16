//! 端到端：起 serve，模擬 Claude Code 送 hook、寫 transcript。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn post(port: u16, body: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut s = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("connect: {e}"),
        }
    };
    write!(
        s,
        "POST /hook HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    resp
}

fn wait_for(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[test]
fn serve_records_hooks_and_transcript_arrivals() {
    let root = std::env::temp_dir().join(format!("hook-capture-serve-{}", std::process::id()));
    let out = root.join("out");
    let transcript = root.join("s1.jsonl");
    std::fs::create_dir_all(&root).unwrap();
    // 第一個 http 事件到之前就已經寫好的行（實測：SessionStart 不會送 http hook）
    std::fs::write(&transcript, "{\"type\":\"user\",\"uuid\":\"u0\"}\n").unwrap();
    let port = free_port();
    let _child = Kill(
        Command::new(env!("CARGO_BIN_EXE_hook-capture"))
            .args(["serve", "--port", &port.to_string(), "--out"])
            .arg(&out)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let prompt = serde_json::json!({
        "session_id": "s1", "hook_event_name": "UserPromptSubmit",
        "transcript_path": transcript, "cwd": root,
    })
    .to_string();
    let resp = post(port, &prompt);
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.ends_with("\r\n\r\n"), "body must be empty: {resp:?}");

    // transcript 分兩次寫入同一行，tail 只能記一筆
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    f.write_all(br#"{"type":"assistant","uuid":"u1","#).unwrap();
    f.flush().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    f.write_all(b"\"timestamp\":\"2026-10-16T00:00:00Z\"}\n")
        .unwrap();
    f.flush().unwrap();

    let arrivals = out.join("s1/transcript-arrivals.jsonl");
    let rows = || -> Vec<serde_json::Value> {
        std::fs::read_to_string(&arrivals)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    wait_for(|| rows().len() == 2);
    let r = rows();
    assert_eq!(
        (r[0]["line_no"].as_u64(), r[0]["backfill"].as_bool()),
        (Some(1), Some(true))
    );
    assert_eq!(r[0]["uuid"], "u0");
    assert_eq!(
        (r[1]["line_no"].as_u64(), r[1]["backfill"].as_bool()),
        (Some(2), Some(false))
    );
    assert_eq!(r[1]["uuid"], "u1");

    // subagent：另一個檔案 <stem>/subagents/agent-<id>.jsonl
    let sub = root.join("s1/subagents/agent-a1.jsonl");
    std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
    let start = serde_json::json!({
        "session_id": "s1", "hook_event_name": "SubagentStart", "agent_id": "a1",
        "transcript_path": transcript,
    })
    .to_string();
    post(port, &start);
    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(
        &sub,
        "{\"type\":\"assistant\",\"uuid\":\"s-u1\",\"isSidechain\":true}\n",
    )
    .unwrap();
    let sub_arrivals = out.join("s1/subagent-a1-arrivals.jsonl");
    wait_for(|| std::fs::read_to_string(&sub_arrivals).is_ok_and(|t| t.contains("s-u1")));

    let end = r#"{"session_id":"s1","hook_event_name":"SessionEnd"}"#;
    post(port, end);
    let copy = out.join("s1/transcript.jsonl");
    wait_for(|| {
        copy.exists()
            && out
                .join("s1/transcript-dir/subagents/agent-a1.jsonl")
                .exists()
    });
    assert!(std::fs::read_to_string(&copy).unwrap().contains("\"u1\""));

    let hooks = std::fs::read_to_string(out.join("s1/hooks.jsonl")).unwrap();
    assert_eq!(hooks.lines().count(), 3);
}

#[test]
fn cmd_reads_stdin_and_always_exits_zero() {
    let out = std::env::temp_dir().join(format!("hook-capture-cmd-{}", std::process::id()));
    for input in [r#"{"session_id":"c1","hook_event_name":"Stop"}"#, "garbage"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hook-capture"))
            .arg("cmd")
            .env("HOOK_CAPTURE_OUT", &out)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let o = child.wait_with_output().unwrap();
        assert!(o.status.success());
        assert!(
            o.stdout.is_empty(),
            "must not print anything to Claude Code"
        );
    }
    let text = std::fs::read_to_string(out.join("c1/hooks.jsonl")).unwrap();
    assert!(text.contains("\"via\":\"command\""));
}
