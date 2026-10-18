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
    let body = json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": dir,
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
/// 不用「連不上」判斷（#725）：並行的別的測試程序隨時可能綁到同一個埠號。listener 屬於 serve 的 task，
/// task 結束時 router（拿著 tx）一起丟掉，rx 收到 None——那時 listener 也已經丟了。
#[tokio::test]
async fn closed_hook_server_releases_the_port() {
    let (tx, mut rx) = mpsc::channel(8);
    let server = serve_hooks("127.0.0.1:0".parse().unwrap(), tx)
        .await
        .unwrap();
    let addr = server.addr;
    // 前提：入口在收。整個請求送完、回應讀完，連線在 close 之前就結束（留著半開的連線，
    // axum 為它 spawn 的 task 會拿著 router 撐過 close 的寬限期）
    let stop = json!({"session_id": "s1", "hook_event_name": "Stop"});
    assert!(
        post(addr, stop.to_string().as_bytes())
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(rx.recv().await.is_some());
    tokio::time::timeout(Duration::from_secs(5), server.close())
        .await
        .expect("close 不能卡住");
    let _other = std::net::TcpListener::bind(addr); // 模擬並行的別的程序剛好綁到同一個埠：不影響判斷
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await,
            Ok(None)
        ),
        "close 之後 hook 入口（listener 與 router）要整個收掉"
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

/// #713：store 記的是 0（這個檔從頭讀、還沒有行產生 Event）：重啟從第 1 行讀（`After(0)`），不是從檔尾。
#[test]
fn restart_reads_a_fresh_file_with_no_recorded_line_from_the_start() {
    let mut p = TailPlanner::default();
    p.seed("s1", "main", Some(T), 0);
    let a = p.on_hook(&h(
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": T,
                                "tool_name": "Bash", "tool_use_id": "t"}),
    ));
    assert!(
        matches!(
            a[0],
            TailAction::Start {
                start: Start::After(0),
                ..
            }
        ),
        "{a:?}"
    );
}

// ── hook 埠的來源檢查（#762 S-4、D10、contracts §7）：瀏覽器裡任何網頁都能對 127.0.0.1:4800 送 no-cors 的
// POST（text/plain、不觸發 preflight），偽造 session、危險指令，或 POST /stop 關掉 collector

async fn raw(addr: SocketAddr, req: String) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).to_string()
}

fn hook_req(headers: &str, body: &str) -> String {
    format!(
        "POST {HOOK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn stop_req(headers: &str) -> String {
    format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n",
        coding_lab::hooks::STOP_PATH
    )
}

#[tokio::test]
async fn hook_with_an_origin_is_refused() {
    let (addr, mut rx) = server(8).await;
    let body = json!({"session_id": "evil", "hook_event_name": "UserPromptSubmit", "prompt": "x"})
        .to_string();
    let resp = raw(
        addr,
        hook_req(
            "Origin: https://attacker.example\r\nContent-Type: application/json\r\n",
            &body,
        ),
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
    assert!(rx.try_recv().is_err(), "偽造的 hook 不能進 merge");
}

#[tokio::test]
async fn hook_that_is_not_json_is_refused() {
    let (addr, mut rx) = server(8).await;
    let body = json!({"session_id": "evil", "hook_event_name": "UserPromptSubmit", "prompt": "x"})
        .to_string();
    for headers in ["Content-Type: text/plain;charset=UTF-8\r\n", ""] {
        let resp = raw(addr, hook_req(headers, &body)).await;
        assert!(resp.starts_with("HTTP/1.1 415"), "{headers:?}：{resp}");
    }
    assert!(rx.try_recv().is_err());
    // Claude Code 送的是 application/json（帶參數、大小寫不同也算）
    for headers in [
        "Content-Type: application/json\r\n",
        "content-type: Application/JSON; charset=utf-8\r\n",
    ] {
        let resp = raw(addr, hook_req(headers, &body)).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{headers:?}：{resp}");
        assert!(matches!(rx.try_recv(), Ok(Input::Hook { .. })));
    }
}

#[tokio::test]
async fn stop_needs_its_own_header_and_no_origin() {
    let (tx, _rx) = mpsc::channel(8);
    let server = serve_hooks("127.0.0.1:0".parse().unwrap(), tx)
        .await
        .unwrap();
    let addr = server.addr;
    let stop = server.stop_requested();
    // #777：標頭的值要等於這次啟動的 stopToken（寫在 0600 的 collector.json 裡，只有同一個帳號讀得到）
    let token = server.stop_token().to_string();
    assert!(token.len() >= 32, "隨機值：{token}");
    for headers in [
        "".to_string(),
        "Content-Type: text/plain\r\n".to_string(),
        "X-Coding-Lab-Stop: 1\r\n".to_string(),
        format!("Origin: https://attacker.example\r\nX-Coding-Lab-Stop: {token}\r\n"),
    ] {
        let resp = raw(addr, stop_req(&headers)).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{headers:?}：{resp}");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), stop.notified())
            .await
            .is_err(),
        "被拒的 stop 不能觸發結束"
    );
    let resp = raw(addr, stop_req(&format!("X-Coding-Lab-Stop: {token}\r\n"))).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    tokio::time::timeout(Duration::from_secs(2), stop.notified())
        .await
        .expect("coding-lab stop 帶的標頭要能結束");
}

// ── 讀檔探測（#762 W-M5）：UNC 路徑不碰（Windows 會去連那台主機、送出 NTLM 憑證），只讀 cwd 底下的檔，
// 探測有時限（網路磁碟、OneDrive 隨選檔案、離線 NAS 會讓 hook 卡 21 秒）

fn edit_hook(cwd: &str, path: &str) -> HookInput {
    h(
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": cwd,
             "tool_use_id": "t1", "tool_input": {"file_path": path, "old_string": "a", "new_string": "b"}}),
    )
}

#[test]
fn unc_paths_are_never_touched() {
    for path in [
        "\\\\10.255.255.1\\share\\x.txt",
        "//10.255.255.1/share/x.txt",
        "\\\\?\\UNC\\10.255.255.1\\share\\x.txt",
        "\\\\localhost\\C$\\Windows\\win.ini",
    ] {
        let mut hook = edit_hook("//10.255.255.1/share", path);
        coding_lab::derive::attach_probe(&mut hook);
        assert!(hook.probe().is_none(), "{path}");
    }
}

#[test]
fn files_outside_the_cwd_are_not_read() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("probe-cwd-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    std::fs::write(dir.join("secret.txt"), "a\n").unwrap();
    std::fs::write(dir.join("proj/a.txt"), "a\n").unwrap();
    let cwd = dir.join("proj");
    let mut outside = edit_hook(
        cwd.to_str().unwrap(),
        dir.join("secret.txt").to_str().unwrap(),
    );
    coding_lab::derive::attach_probe(&mut outside);
    let p = outside.probe().expect("存在與否還是看（不讀內容）");
    assert!(p.exists);
    assert!(p.hunk.is_none(), "cwd 外的檔不讀內容");
    let mut inside = edit_hook(
        cwd.to_str().unwrap(),
        dir.join("proj/a.txt").to_str().unwrap(),
    );
    coding_lab::derive::attach_probe(&mut inside);
    assert!(
        inside.probe().is_some_and(|p| p.hunk.is_some()),
        "cwd 底下照常算 hunk"
    );
}

#[tokio::test]
async fn probe_has_a_time_limit_and_the_hook_still_goes_through() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("probe-limit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    let (tx, mut rx) = mpsc::channel(8);
    let server = serve_hooks("127.0.0.1:0".parse().unwrap(), tx)
        .await
        .unwrap();
    // 時限設 0：探測一定逾時（不 sleep，等同探測卡住）
    server.set_probe_limit(Duration::ZERO);
    let body = json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Edit",
                      "cwd": dir, "tool_use_id": "t1",
                      "tool_input": {"file_path": dir.join("a.txt"), "old_string": "a", "new_string": "b"}});
    let resp = post(server.addr, body.to_string().as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    match rx.try_recv() {
        Ok(Input::Hook { hook, .. }) => assert!(hook.probe().is_none(), "逾時就不帶探測結果"),
        _ => panic!("逾時也要照常交給 merge"),
    }
}

/// W-M5：transcript_path 是 UNC 時不 tail（hook 照收，10 秒後依不變式 2 降級進串流）。
#[test]
fn unc_transcript_is_not_tailed() {
    for t in ["\\\\nas\\share\\s1.jsonl", "//nas/share/s1.jsonl"] {
        let mut p = TailPlanner::default();
        let a = p.on_hook(&h(
            json!({"session_id": "s1", "hook_event_name": "SessionStart",
                                    "source": "startup", "transcript_path": t}),
        ));
        assert!(a.is_empty(), "{t}：{a:?}");
        let a = p.on_hook(&h(
            json!({"session_id": "s1", "hook_event_name": "SubagentStart",
                                    "agent_id": "a1", "transcript_path": t}),
        ));
        assert!(a.is_empty(), "{t}：{a:?}");
    }
}

// ── body 上限（#740 L-2）：axum 預設 2 MB；編輯超過約 2 MB 的檔時 Edit 的 PostToolUse 帶整份 originalFile，
// 原本回 413（Claude Code 顯示 hook 錯誤，違反不變式 4）。上限 64 MB，超過的也回 200、寫 log

#[tokio::test]
async fn big_hook_is_answered_200_and_forwarded() {
    let (addr, mut rx) = server(8).await;
    let body = json!({"session_id": "s1", "hook_event_name": "PostToolUse", "tool_name": "Edit",
        "tool_use_id": "e1", "tool_response": {"originalFile": "x".repeat(3_000_000)}})
    .to_string();
    let resp = raw(addr, hook_req("Content-Type: application/json\r\n", &body)).await;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "{}",
        resp.lines().next().unwrap_or("")
    );
    assert!(
        matches!(rx.try_recv(), Ok(Input::Hook { .. })),
        "3 MB 的 hook 要交給 merge"
    );
}

#[tokio::test]
async fn hook_over_the_limit_is_still_answered_200() {
    let (addr, mut rx) = server(8).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    // 只送一點 body：宣告的長度超過上限就不讀，直接回 200
    s.write_all(
        format!(
            "POST {HOOK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{\"session_id\"",
            65 * 1024 * 1024
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut resp = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut resp)).await;
    let resp = String::from_utf8_lossy(&resp);
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(rx.try_recv().is_err());
}

/// L-7（#740）：hook 入口跑在自己的執行緒與 runtime 上。web 的 async handler 鎖 std Mutex<Store> 被卡住、
/// 主 runtime 的 worker 全被佔住時，hook 照樣立刻回 200（不變式 4）。
#[test]
fn hooks_are_answered_while_every_main_worker_is_stuck() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (addr, _rx) = rt.block_on(server(8));
    // 佔住唯一的 worker（像 handler 在 async 裡等 store 的鎖）
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        started_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
    });
    started_rx.recv().unwrap();
    let body = json!({"session_id": "s1", "hook_event_name": "Stop"}).to_string();
    let t = Instant::now();
    assert!(
        post_blocking(addr, &body, Duration::from_secs(5)),
        "hook 要回 200"
    );
    // 不變式 4 的預算是 100 ms；測的是「不用等那 3 秒」，負載高的機器上留一點餘裕
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    drop(rt);
}

// ── 多帳號（#777 W-M7）：4800 是整台機器共用的，第二個帳號的 hook 會送進第一個帳號的 collector。
// plugin 的 hook 帶 X-Coding-Lab-User（${USER}）與 X-Coding-Lab-Username（${USERNAME}）

async fn server_as(user: &str) -> (SocketAddr, mpsc::Receiver<Input>) {
    let (tx, rx) = mpsc::channel(32);
    let server =
        coding_lab::hooks::serve_hooks_as("127.0.0.1:0".parse().unwrap(), tx, Some(user.into()))
            .await
            .unwrap();
    (server.addr, rx)
}

#[tokio::test]
async fn hooks_of_another_account_are_ignored() {
    let (addr, mut rx) = server_as("alice").await;
    let body = json!({"session_id": "s1", "hook_event_name": "Stop"}).to_string();
    for headers in [
        "X-Coding-Lab-User: bob\r\n",
        "X-Coding-Lab-Username: bob\r\n",
        "X-Coding-Lab-User: \r\nX-Coding-Lab-Username: bob\r\n",
        "X-Coding-Lab-User: ${USER}\r\nX-Coding-Lab-Username: bob\r\n",
    ] {
        let resp = raw(
            addr,
            hook_req(
                &format!("Content-Type: application/json\r\n{headers}"),
                &body,
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "照樣回 200：{resp}");
        assert!(
            rx.try_recv().is_err(),
            "別的帳號的 hook 要丟掉：{headers:?}"
        );
    }
}

#[tokio::test]
async fn hooks_of_the_same_account_or_without_a_user_are_kept() {
    let (addr, mut rx) = server_as("alice").await;
    let body = json!({"session_id": "s1", "hook_event_name": "Stop"}).to_string();
    for headers in [
        "X-Coding-Lab-User: alice\r\n",
        "X-Coding-Lab-User: \r\nX-Coding-Lab-Username: alice\r\n",
        // 舊版 plugin、設定擋掉環境變數：沒有、空的、原樣留下的 ${…}
        "",
        "X-Coding-Lab-User: \r\nX-Coding-Lab-Username: \r\n",
        "X-Coding-Lab-User: ${USER}\r\nX-Coding-Lab-Username: $USERNAME\r\n",
    ] {
        let resp = raw(
            addr,
            hook_req(
                &format!("Content-Type: application/json\r\n{headers}"),
                &body,
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(
            matches!(rx.try_recv(), Ok(Input::Hook { .. })),
            "要照收：{headers:?}"
        );
    }
}

/// C12（#741）：SessionEnd 之後（例如 /clear 換了新 id），同一個舊 id 帶 transcript_path 的 hook（背景 agent 晚到的結果）
/// 不重開 tail，subagent 的也不開；下一個 SessionStart（resume）才重新開始。
#[test]
fn hooks_after_session_end_do_not_reopen_tails_until_the_next_session_start() {
    let mut p = TailPlanner::default();
    p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "transcript_path": T})));
    let end = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionEnd", "reason": "clear", "transcript_path": T})));
    assert_eq!(
        end,
        [TailAction::StopSession {
            session: "s1".into()
        }]
    );
    assert!(p.is_ended("s1"));
    for late in [
        json!({"session_id": "s1", "hook_event_name": "PostToolUse", "transcript_path": T, "tool_name": "Agent", "tool_use_id": "t"}),
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": T, "tool_name": "Bash", "tool_use_id": "u",
               "agent_id": "a1", "agent_type": "Explore"}),
    ] {
        assert!(
            p.on_hook(&h(late)).is_empty(),
            "結束之後晚到的 hook 不開 tail"
        );
    }
    let resumed = p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "resume", "transcript_path": T})));
    assert!(
        matches!(resumed.as_slice(), [TailAction::Start { .. }]),
        "{resumed:?}"
    );
    assert!(!p.is_ended("s1"));
}

/// L-8（#741）：沒有 SessionEnd、被 merge 判 abandoned 的 session 要停掉它的 tail（含 subagent）；之後晚到的 hook 也不重開。
#[test]
fn abandoned_session_stops_its_tails() {
    let mut p = TailPlanner::default();
    p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "startup", "transcript_path": T})));
    p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "SubagentStart", "transcript_path": T, "agent_id": "a1", "agent_type": "Explore"})));
    assert_eq!(
        p.on_session_ended("s1"),
        [TailAction::StopSession {
            session: "s1".into()
        }]
    );
    assert!(p.on_session_ended("s1").is_empty(), "第二次沒有東西可停");
    assert!(p.on_session_ended("nobody").is_empty());
    assert!(
        p.on_hook(&h(
            json!({"session_id": "s1", "hook_event_name": "Stop", "transcript_path": T})
        ))
        .is_empty()
    ); // 終端機一直開著、閒置被補 abandoned 後又用起來（merge 依 #679 重新打開，不會有 SessionStart）：
    // main 的 UserPromptSubmit／PreToolUse 重開 tail，從讀過的行接著讀；subagent 的 hook 不算
    assert!(
        p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": T,
                            "agent_id": "a1", "agent_type": "Explore", "tool_name": "Read", "tool_use_id": "t1"})))
            .is_empty()
    );
    assert!(matches!(
        p.on_hook(&h(json!({"session_id": "s1", "hook_event_name": "UserPromptSubmit", "transcript_path": T, "prompt": "再來"})))
            .as_slice(),
        [TailAction::Start { file, start: Start::End, .. }] if file == "main"
    ));
    assert!(!p.is_ended("s1"));
}
