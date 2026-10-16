//! hooks 入口（#630）與 TailPlanner。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use coding_lab::hook::HookInput;
use coding_lab::hooks::{HOOK_PATH, serve_hooks};
use coding_lab::merge::Input;
use coding_lab::transcript::{Start, TailAction, TailPlanner};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

async fn post(addr: SocketAddr, body: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST {HOOK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).to_string()
}

fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

async fn server(cap: usize) -> (SocketAddr, mpsc::Receiver<Input>) {
    let (tx, rx) = mpsc::channel(cap);
    let (addr, _task) = serve_hooks("127.0.0.1:0".parse().unwrap(), tx)
        .await
        .unwrap();
    (addr, rx)
}

#[tokio::test]
async fn hook_is_forwarded_and_answered_with_empty_200() {
    let (addr, mut rx) = server(8).await;
    let body =
        json!({"session_id": "s1", "hook_event_name": "Stop", "last_assistant_message": "ok"});
    let resp = post(addr, body.to_string().as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert_eq!(
        body_of(&resp),
        "",
        "回應不能帶任何內容（零 token、不影響 Claude Code）"
    );
    match rx.try_recv() {
        Ok(Input::Hook { hook, ts_seen }) => {
            assert_eq!(hook.event_name(), "Stop");
            assert!(ts_seen > 0);
        }
        _ => panic!("回 200 之前就該交給 merge"),
    }
}

#[tokio::test]
async fn bad_payload_still_gets_200_and_no_event() {
    let (addr, mut rx) = server(8).await;
    for body in [
        &b"not json"[..],
        br#"{"hook_event_name":"Stop"}"#,
        b"[1,2]",
        b"",
    ] {
        let resp = post(addr, body).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(
            rx.try_recv().is_err(),
            "壞 payload 不產生事件：{:?}",
            String::from_utf8_lossy(body)
        );
    }
}

#[tokio::test]
async fn full_channel_never_blocks_claude_code() {
    let (addr, _rx) = server(1).await;
    let body = json!({"session_id": "s1", "hook_event_name": "Stop"}).to_string();
    let t = Instant::now();
    for _ in 0..20 {
        assert!(
            post(addr, body.as_bytes())
                .await
                .starts_with("HTTP/1.1 200")
        );
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "merge 塞住時也要立刻回應"
    );
}

#[tokio::test]
async fn only_loopback_can_be_bound() {
    let (tx, _rx) = mpsc::channel(1);
    assert!(serve_hooks("0.0.0.0:0".parse().unwrap(), tx).await.is_err());
}

#[tokio::test]
async fn p95_latency_under_50ms() {
    let (addr, mut rx) = server(1024).await;
    let body = json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                      "tool_input": {"command": "ls"}, "tool_use_id": "t"})
    .to_string();
    let mut ms = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        post(addr, body.as_bytes()).await;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        while rx.try_recv().is_ok() {}
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p95 = ms[189];
    eprintln!("hook p50 {:.2} ms、p95 {:.2} ms", ms[99], p95);
    assert!(p95 < 50.0, "p95 {p95:.2} ms");
}

/// samples/hooks 重放（從 #624 的樣本）；沒有 samples/ 時略過。
#[tokio::test]
async fn every_sample_hook_replays() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/hooks");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("略過：沒有 samples/hooks");
        return;
    };
    let (addr, mut rx) = server(64).await;
    let mut n = 0;
    for e in entries.flatten() {
        let body = std::fs::read(e.path()).unwrap();
        assert!(post(addr, &body).await.starts_with("HTTP/1.1 200"));
        let Ok(Input::Hook { hook, .. }) = rx.try_recv() else {
            panic!("{}", e.path().display())
        };
        assert_eq!(
            hook.event_name(),
            e.path().file_stem().unwrap().to_str().unwrap()
        );
        n += 1;
    }
    assert!(n > 0);
}

// ── TailPlanner：看 hook 決定開／關哪些 tail

fn h(v: Value) -> HookInput {
    HookInput::from_json(v.to_string().as_bytes()).unwrap()
}

const T: &str = "/home/user/.claude/projects/-p/s1.jsonl";

#[test]
fn session_start_opens_main_tail_from_top_or_end() {
    let mut p = TailPlanner::default();
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "transcript_path": T})));
    assert_eq!(
        a,
        [TailAction::Start {
            session: "s1".into(),
            file: "main".into(),
            path: PathBuf::from(T),
            start: Start::Beginning
        }]
    );
    assert!(
        p.on_hook(&h(
            json!({"session_id": "s1", "hook_event_name": "Stop", "transcript_path": T})
        ))
        .is_empty()
    );

    let mut p = TailPlanner::default();
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "resume", "transcript_path": T})));
    assert_eq!(
        a[0],
        TailAction::Start {
            session: "s1".into(),
            file: "main".into(),
            path: PathBuf::from(T),
            start: Start::End
        }
    );
}

#[test]
fn first_sighting_without_session_start_tails_from_end() {
    let mut p = TailPlanner::default();
    let a = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": T,
                                "tool_name": "Bash", "tool_use_id": "t"}),
    ));
    assert_eq!(
        a,
        [TailAction::Start {
            session: "s1".into(),
            file: "main".into(),
            path: PathBuf::from(T),
            start: Start::End
        }]
    );
}

#[test]
fn compact_on_known_session_keeps_the_same_tail() {
    let mut p = TailPlanner::default();
    p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "transcript_path": T})));
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "compact", "transcript_path": T})));
    assert!(a.is_empty(), "實測 compact 沿用同一個檔，不重開：{a:?}");
}

#[test]
fn subagent_files_are_tailed_and_stopped() {
    let mut p = TailPlanner::default();
    p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "transcript_path": T})));
    let a = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "SubagentStart", "transcript_path": T,
                                "agent_id": "a1", "agent_type": "Explore"}),
    ));
    assert_eq!(
        a,
        [TailAction::Start {
            session: "s1".into(),
            file: "agent-a1".into(),
            path: PathBuf::from("/home/user/.claude/projects/-p/s1/subagents/agent-a1.jsonl"),
            start: Start::Beginning,
        }]
    );
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SubagentStop", "transcript_path": T, "agent_id": "a1"})));
    assert_eq!(
        a,
        [TailAction::Stop {
            session: "s1".into(),
            file: "agent-a1".into()
        }]
    );
    let a = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "SessionEnd", "transcript_path": T}),
    ));
    assert_eq!(
        a,
        [TailAction::StopSession {
            session: "s1".into()
        }]
    );
    // 結束後同一個 session 又出現（resume）要重新開
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "resume", "transcript_path": T})));
    assert_eq!(a.len(), 1);
}
