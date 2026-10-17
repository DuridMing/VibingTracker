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
    post_with(addr, "", body).await
}

async fn post_with(addr: SocketAddr, extra_headers: &str, body: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST {HOOK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
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
    let server = serve_hooks("127.0.0.1:0".parse().unwrap(), tx)
        .await
        .unwrap();
    (server.addr, rx)
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

/// #626 的結論：Edit 的 hunk 在回應 hook **之前**讀檔計算；回應之後檔案被改掉也不影響。
#[tokio::test]
async fn edit_hunk_is_read_before_the_response() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hooks-edit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("calc.py");
    std::fs::write(&file, "def sub(a, b):\n    return a + b\n").unwrap();
    let (addr, mut rx) = server(8).await;
    let body = json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Edit",
                      "tool_input": {"file_path": file, "old_string": "a + b", "new_string": "a - b"},
                      "tool_use_id": "e1"});
    post(addr, body.to_string().as_bytes()).await;
    std::fs::write(&file, "def sub(a, b):\n    return a - b\n").unwrap(); // Claude Code 執行 Edit
    let Ok(Input::Hook { hook, .. }) = rx.try_recv() else {
        panic!()
    };
    let probe = hook.probe().expect("PreToolUse 的 Edit 要有探測結果");
    assert!(probe.exists);
    let hunk = probe.hunk.as_ref().expect("old_string 找得到");
    assert!(
        hunk.lines.contains(&"-    return a + b".to_string()),
        "{:?}",
        hunk.lines
    );
}

/// #670-2：每個開／關 tail 的動作都要讓 merge 清掉那個檔的降級（新 tail 從健康狀態開始）。
#[test]
fn every_tail_action_resets_degraded_in_merge() {
    let reset = |a: TailAction| match a.reset(7, 3) {
        Input::TailReset {
            session,
            file,
            ts_seen,
            tail,
        } => {
            assert_eq!(tail, 3);
            (session, file, ts_seen)
        }
        _ => panic!("要是 TailReset"),
    };
    let start = TailAction::Start {
        session: "s".into(),
        file: "agent-a1".into(),
        path: PathBuf::from("/t/agent-a1.jsonl"),
        start: Start::Beginning,
    };
    assert_eq!(reset(start), ("s".into(), Some("agent-a1".into()), 7));
    let stop = TailAction::Stop {
        session: "s".into(),
        file: "main".into(),
    };
    assert_eq!(reset(stop), ("s".into(), Some("main".into()), 7));
    let all = TailAction::StopSession {
        session: "s".into(),
    };
    assert_eq!(reset(all), ("s".into(), None, 7));
}

/// 同步送一個 hook，回傳是否在 `wait` 內收到回應。只有 unix 的測試用到。
#[cfg(unix)]
fn post_blocking(addr: SocketAddr, body: &str, wait: Duration) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(addr) else {
        return false;
    };
    s.set_read_timeout(Some(wait)).unwrap();
    let req = format!(
        "POST {HOOK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = [0u8; 64];
    matches!(s.read(&mut buf), Ok(n) if n > 0 && buf.starts_with(b"HTTP/1.1 200"))
}

/// #670-13：Edit 探針讀檔不能卡住處理 hook 的執行緒（大檔、慢的磁碟、FIFO 這類讀了就不回來的檔案）。
/// 只有 1 個 worker 的 runtime 上，卡住的探針會讓其他 hook 都等不到回應。
#[cfg(unix)]
#[test]
fn probe_never_blocks_the_hook_server() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hooks-fifo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("pipe.txt");
    let _ = std::fs::remove_file(&fifo);
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (addr, _rx) = rt.block_on(server(8));

    let edit = json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Edit",
                      "tool_input": {"file_path": fifo, "old_string": "a", "new_string": "b"},
                      "tool_use_id": "e1"})
    .to_string();
    let first = std::thread::spawn(move || post_blocking(addr, &edit, Duration::from_secs(2)));
    std::thread::sleep(Duration::from_millis(200)); // 讓 Edit 先進 handler
    let other = json!({"session_id": "s1", "hook_event_name": "UserPromptSubmit", "prompt": "hi"})
        .to_string();
    let other_ok = post_blocking(addr, &other, Duration::from_secs(2));
    let first_ok = first.join().unwrap();
    // 卡在讀 FIFO 的執行緒（修好之前）不等它，測試程式結束時一起收掉
    rt.shutdown_background();
    assert!(other_ok, "其他 hook 被 Edit 探針卡住");
    assert!(first_ok, "不是一般檔案就不讀，Edit 的 hook 也照常回應");
}

/// `/clear` 換一個新的 session（新 id、新 transcript 檔，SessionStart 的 source 是 clear；#688 實測）：
/// 和 startup 一樣從檔頭讀，否則新檔開頭幾行會漏掉。
#[test]
fn clear_starts_a_new_session_read_from_the_top() {
    let mut p = TailPlanner::default();
    let a = p.on_hook(&h(json!({"session_id": "s2", "hook_event_name": "SessionStart", "source": "clear", "transcript_path": T})));
    assert_eq!(
        a,
        [TailAction::Start {
            session: "s2".into(),
            file: "main".into(),
            path: PathBuf::from(T),
            start: Start::Beginning
        }]
    );
}

/// #685：決定閒置結束後，ensure 轉送的 SessionStart 回 503 讓 ensure 改拉新的 collector；
/// 已經在跑的 session 的 hook 照收（之後 stop_all 會處理掉，不會被吞掉）。
#[tokio::test]
async fn closing_hook_server_refuses_ensure_forwards_but_keeps_other_hooks() {
    let (tx, mut rx) = mpsc::channel(8);
    let server = serve_hooks("127.0.0.1:0".parse().unwrap(), tx)
        .await
        .unwrap();
    let addr = server.addr;
    server.set_closing(true);
    let start = json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup"});
    let resp = post_with(
        addr,
        "X-Coding-Lab-Via: ensure\r\n",
        start.to_string().as_bytes(),
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
    assert!(rx.try_recv().is_err(), "503 的 SessionStart 不能交給 merge");

    let stop = json!({"session_id": "s2", "hook_event_name": "Stop"});
    let resp = post(addr, stop.to_string().as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(rx.try_recv().is_ok(), "一般 hook 照收");

    server.set_closing(false);
    let resp = post_with(
        addr,
        "X-Coding-Lab-Via: ensure\r\n",
        start.to_string().as_bytes(),
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200"), "取消結束後照常收：{resp}");
}

/// #685：close() 回來時埠已經放掉，ensure 的 probe 會失敗而重新拉起 collector。
#[tokio::test]
async fn closed_hook_server_releases_the_port() {
    let (tx, _rx) = mpsc::channel(8);
    let server = serve_hooks("127.0.0.1:0".parse().unwrap(), tx)
        .await
        .unwrap();
    let addr = server.addr;
    assert!(TcpStream::connect(addr).await.is_ok());
    tokio::time::timeout(Duration::from_secs(5), server.close())
        .await
        .expect("close 不能卡住");
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "close 之後 hook 埠要放掉"
    );
}

// ── #684：重啟後從記錄的行號接著讀、補開 subagent 的 tail

/// R11：collector 重啟後，store 記著讀到第 42 行：第一次看到 session（沒有 SessionStart 的 hook、或 resume）
/// 從第 43 行接著讀，不再從檔尾（停機期間寫入的行會漏）。
#[test]
fn restart_resumes_main_tail_after_the_stored_line() {
    let mut p = TailPlanner::default();
    p.seed("s1", "main", Some(T), 42);
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
            start: Start::After(42)
        }]
    );

    let mut p = TailPlanner::default();
    p.seed("s1", "main", Some(T), 42);
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "resume", "transcript_path": T})));
    assert_eq!(
        a,
        [TailAction::Start {
            session: "s1".into(),
            file: "main".into(),
            path: PathBuf::from(T),
            start: Start::After(42)
        }]
    );
}

/// 記錄的是別的檔（路徑不同）就不用它的行號。
#[test]
fn stored_line_of_another_file_is_not_used() {
    let mut p = TailPlanner::default();
    p.seed("s1", "main", Some("/elsewhere/s1.jsonl"), 42);
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "resume", "transcript_path": T})));
    assert!(
        matches!(
            a[0],
            TailAction::Start {
                start: Start::End,
                ..
            }
        ),
        "{a:?}"
    );
}

/// 同一輪裡 SessionEnd 後再 resume：從實際讀到的行接著讀，resume 前寫進檔案的行不漏。
#[test]
fn resume_in_the_same_run_continues_after_the_last_line_read() {
    let mut p = TailPlanner::default();
    p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "transcript_path": T})));
    p.saw_line("s1", "main", 7);
    p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "SessionEnd", "reason": "exit"}),
    ));
    let a = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "resume", "transcript_path": T})));
    assert!(
        matches!(
            a[0],
            TailAction::Start {
                start: Start::After(7),
                ..
            }
        ),
        "{a:?}"
    );
}

/// N1：重啟時還在跑的 subagent 不會再有 SubagentStart：第一個帶 agent_id 的 hook 補開它的 tail
/// （有記錄就接著讀，沒有就從檔頭）。背景 agent（agent_type 空字串）不開。
#[test]
fn subagent_tail_is_reopened_by_its_first_hook_after_a_restart() {
    let sub = PathBuf::from("/home/user/.claude/projects/-p/s1/subagents/agent-a1.jsonl");
    let mut p = TailPlanner::default();
    p.seed("s1", "agent-a1", None, 5);
    let a = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": T,
               "tool_name": "Glob", "tool_use_id": "t", "agent_id": "a1", "agent_type": "Explore"}),
    ));
    assert_eq!(
        a,
        [
            TailAction::Start {
                session: "s1".into(),
                file: "main".into(),
                path: PathBuf::from(T),
                start: Start::End
            },
            TailAction::Start {
                session: "s1".into(),
                file: "agent-a1".into(),
                path: sub.clone(),
                start: Start::After(5)
            }
        ]
    );
    // 同一個 agent 的下一個 hook 不再重開
    let again = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "PostToolUse", "transcript_path": T,
               "tool_name": "Glob", "tool_use_id": "t", "agent_id": "a1", "agent_type": "Explore"}),
    ));
    assert!(again.is_empty(), "{again:?}");

    // 沒有記錄的 subagent 從檔頭讀
    let b = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": T,
               "tool_name": "Glob", "tool_use_id": "t2", "agent_id": "a2", "agent_type": "Explore"}),
    ));
    assert!(
        matches!(&b[..], [TailAction::Start { file, start: Start::Beginning, .. }] if file == "agent-a2"),
        "{b:?}"
    );

    // 背景 agent 不開
    let bg = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": T,
               "tool_name": "Glob", "tool_use_id": "t3", "agent_id": "bg1", "agent_type": ""}),
    ));
    assert!(bg.is_empty(), "{bg:?}");
}

/// 結束了的 subagent 晚到的 hook、或沒開過的 SubagentStop，都不開 tail。
#[test]
fn stopped_subagent_is_not_reopened() {
    let mut p = TailPlanner::default();
    p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "transcript_path": T})));
    let stop = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SubagentStop", "transcript_path": T, "agent_id": "a9", "agent_type": "Explore"})));
    assert!(
        stop.is_empty(),
        "沒開過的 SubagentStop 什麼都不做：{stop:?}"
    );
    let late = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "PostToolUse", "transcript_path": T,
               "tool_name": "Glob", "tool_use_id": "t", "agent_id": "a9", "agent_type": "Explore"}),
    ));
    assert!(late.is_empty(), "已結束的 agent 不重開：{late:?}");
}
