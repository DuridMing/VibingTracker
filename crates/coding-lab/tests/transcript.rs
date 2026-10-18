//! transcript tail（#631）。檔案都在這個 worktree 的 target/tmp/。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use coding_lab::merge::{AgentMetaRead, Input};
use coding_lab::transcript::{SharedWatcher, Start, TailOptions, TailReader, spawn_tail, stop_all};
use serde_json::json;
use tokio::sync::mpsc;

static N: AtomicU32 = AtomicU32::new(0);

fn tmpfile(content: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "transcript-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("t.jsonl");
    std::fs::write(&p, content).unwrap();
    p
}

fn append(p: &Path, s: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
    f.write_all(s.as_bytes()).unwrap();
}

fn line(uuid: &str) -> String {
    format!("{}\n", json!({"type": "assistant", "uuid": uuid}))
}

#[test]
fn reads_from_start_with_line_numbers() {
    let p = tmpfile(&(line("a") + &line("b")));
    let mut r = TailReader::open(&p, Start::Beginning).unwrap();
    let got = r.poll().unwrap();
    let nos: Vec<u64> = got.lines.iter().map(|l| l.no).collect();
    assert_eq!(nos, [1, 2]);
    assert_eq!(got.lines[1].value["uuid"], "b");
    assert!(r.poll().unwrap().lines.is_empty(), "沒有新資料就沒有新行");
}

#[test]
fn half_line_waits_for_newline() {
    let p = tmpfile("");
    let mut r = TailReader::open(&p, Start::Beginning).unwrap();
    append(&p, r#"{"type":"assistant","uu"#);
    assert!(r.poll().unwrap().lines.is_empty());
    append(&p, "id\":\"x\"}\n");
    let got = r.poll().unwrap();
    assert_eq!(got.lines.len(), 1);
    assert_eq!(got.lines[0].value["uuid"], "x");
    assert_eq!(got.lines[0].no, 1);
}

#[test]
fn start_at_end_skips_existing_but_keeps_line_numbers() {
    let p = tmpfile(&(line("a") + &line("b") + r#"{"half"#));
    let mut r = TailReader::open(&p, Start::End).unwrap();
    assert!(r.poll().unwrap().lines.is_empty(), "既有的完整行不重讀");
    append(&p, "\":1}\n");
    append(&p, &line("c"));
    let nos: Vec<u64> = r.poll().unwrap().lines.iter().map(|l| l.no).collect();
    assert_eq!(nos, [3, 4], "從檔尾開始時，尾端的半行屬於下一行");
}

#[test]
fn file_that_does_not_exist_yet_is_picked_up() {
    let p = tmpfile("");
    std::fs::remove_file(&p).unwrap();
    let mut r = TailReader::open(&p, Start::Beginning).unwrap();
    assert!(r.poll().unwrap().lines.is_empty());
    std::fs::write(&p, line("a")).unwrap();
    assert_eq!(r.poll().unwrap().lines.len(), 1);
}

#[test]
fn replaced_or_truncated_file_restarts_from_top() {
    let p = tmpfile(&(line("a") + &line("b")));
    let mut r = TailReader::open(&p, Start::Beginning).unwrap();
    assert_eq!(r.poll().unwrap().lines.len(), 2);
    // 換成較短的新檔（rename 替換）
    let tmp = p.with_extension("new");
    std::fs::write(&tmp, line("z")).unwrap();
    std::fs::rename(&tmp, &p).unwrap();
    let got = r.poll().unwrap();
    assert!(got.restarted);
    assert_eq!(
        got.lines
            .iter()
            .map(|l| l.value["uuid"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["z"]
    );
    assert_eq!(got.lines[0].no, 1);
}

#[test]
fn invalid_json_is_reported_with_line_number_and_reading_continues() {
    let p = tmpfile(&(line("a") + "not json\n" + &line("c")));
    let mut r = TailReader::open(&p, Start::Beginning).unwrap();
    let got = r.poll().unwrap();
    assert_eq!(got.lines.iter().map(|l| l.no).collect::<Vec<_>>(), [1, 3]);
    assert_eq!(got.errors.len(), 1);
    assert_eq!(got.errors[0].no, 2);
}

#[test]
fn large_line_is_still_parsed() {
    let big = "x".repeat(600 * 1024);
    let p = tmpfile(&format!(
        "{}\n",
        json!({"type": "user", "uuid": "big", "message": {"content": big}})
    ));
    let mut r = TailReader::open(&p, Start::Beginning).unwrap();
    let got = r.poll().unwrap();
    assert_eq!(got.lines[0].value["uuid"], "big");
}

// ── tokio task

async fn next(rx: &mut mpsc::Receiver<Input>) -> Input {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("5 秒內沒收到")
        .expect("channel 關了")
}

fn opts(notify: bool) -> TailOptions {
    TailOptions {
        poll_every: Duration::from_millis(50),
        use_notify: notify,
        ..TailOptions::default()
    }
}

#[tokio::test]
async fn task_sends_lines_as_merge_input() {
    for use_notify in [true, false] {
        let p = tmpfile("");
        let (tx, mut rx) = mpsc::channel(64);
        let h = spawn_tail(
            p.clone(),
            "s1".into(),
            "agent-a1".into(),
            Start::Beginning,
            tx,
            opts(use_notify),
        );
        append(&p, &line("a"));
        match next(&mut rx).await {
            Input::Line {
                session,
                file,
                no,
                value,
                ..
            } => {
                assert_eq!((session.as_str(), file.as_str(), no), ("s1", "agent-a1", 1));
                assert_eq!(value["uuid"], "a");
            }
            _ => panic!("應該是 Line（notify={use_notify}）"),
        }
        h.stop().await;
    }
}

#[tokio::test]
async fn task_reports_degraded_then_recovered() {
    let p = tmpfile("");
    let (tx, mut rx) = mpsc::channel(64);
    let h = spawn_tail(
        p.clone(),
        "s1".into(),
        "main".into(),
        Start::Beginning,
        tx,
        opts(false),
    );
    append(&p, "broken\n");
    assert!(matches!(next(&mut rx).await, Input::Degraded { .. }));
    append(&p, &line("ok"));
    assert!(matches!(next(&mut rx).await, Input::Recovered { .. }));
    assert!(matches!(next(&mut rx).await, Input::Line { .. }));
    h.stop().await;
}

#[tokio::test]
async fn stop_drains_lines_written_before_stop() {
    let p = tmpfile("");
    let (tx, mut rx) = mpsc::channel(64);
    let h = spawn_tail(
        p.clone(),
        "s1".into(),
        "main".into(),
        Start::Beginning,
        tx,
        TailOptions {
            poll_every: Duration::from_secs(3600),
            use_notify: false,
            ..TailOptions::default()
        },
    );
    append(&p, &line("last"));
    h.stop().await; // 不等下一次輪詢，結束前最後讀一次
    assert!(matches!(next(&mut rx).await, Input::Line { .. }));
}

/// 從 #625 移來：真實 transcript（含 subagent 檔）全部能讀、沒有壞行、行數與檔案一致。沒有 samples/ 時略過。
#[test]
fn every_sample_transcript_reads_cleanly() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/sessions");
    let Ok(dirs) = std::fs::read_dir(&root) else {
        eprintln!("略過：沒有 samples/sessions");
        return;
    };
    let mut files = Vec::new();
    for d in dirs.flatten() {
        files.push(d.path().join("transcript.jsonl"));
        for s in std::fs::read_dir(d.path().join("subagents"))
            .into_iter()
            .flatten()
            .flatten()
        {
            files.push(s.path());
        }
    }
    let files: Vec<PathBuf> = files.into_iter().filter(|f| f.exists()).collect();
    assert!(!files.is_empty());
    for f in files {
        let expected = std::fs::read_to_string(&f)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .count();
        let got = TailReader::open(&f, Start::Beginning)
            .unwrap()
            .poll()
            .unwrap();
        assert!(got.errors.is_empty(), "{}: {:?}", f.display(), got.errors);
        assert_eq!(got.lines.len(), expected, "{}", f.display());
        assert!(
            got.lines
                .iter()
                .enumerate()
                .all(|(i, l)| l.no == i as u64 + 1),
            "{}",
            f.display()
        );
    }
}

/// #670-7：關機時佇列是滿的（主迴圈已不再收），tail 最後一次讀檔的 `send().await` 會一直等；
/// 停 tail 的同時要繼續消化佇列，否則 collector 卡住、store 不 flush。
#[tokio::test]
async fn stop_all_keeps_draining_so_a_full_queue_cannot_hang_shutdown() {
    let p = tmpfile("");
    let (tx, mut rx) = mpsc::channel(1);
    let h = spawn_tail(
        p.clone(),
        "s1".into(),
        "main".into(),
        Start::Beginning,
        tx,
        TailOptions {
            poll_every: Duration::from_secs(3600),
            use_notify: false,
            ..TailOptions::default()
        },
    );
    for i in 0..50 {
        append(&p, &line(&format!("u{i}")));
    }
    let mut got = 0;
    let done = tokio::time::timeout(
        Duration::from_secs(5),
        stop_all([h], &mut rx, |input| {
            if matches!(input, Input::Line { .. }) {
                got += 1;
            }
        }),
    )
    .await;
    assert!(done.is_ok(), "關機卡住");
    assert_eq!(got, 50, "停之前寫進去的行都要送出");
}

/// #670-11：merge 只看 `message.content` 的第一個區塊，依據是 Claude Code 一行只寫一個區塊——
/// 平行的多個工具呼叫也是 tool_use、tool_result 各自一行。格式改了這裡會紅。沒有 samples/ 時略過。
#[test]
fn every_sample_message_line_has_a_single_content_block() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/sessions");
    let Ok(dirs) = std::fs::read_dir(&root) else {
        eprintln!("略過：沒有 samples/sessions");
        return;
    };
    let mut checked = 0;
    for d in dirs.flatten() {
        let mut files = vec![d.path().join("transcript.jsonl")];
        for s in std::fs::read_dir(d.path().join("subagents"))
            .into_iter()
            .flatten()
            .flatten()
        {
            files.push(s.path());
        }
        for f in files.into_iter().filter(|f| f.exists()) {
            for (i, l) in std::fs::read_to_string(&f).unwrap().lines().enumerate() {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                if let Some(blocks) = v["message"]["content"].as_array() {
                    assert!(
                        blocks.len() <= 1,
                        "{}:{} 有 {} 個區塊",
                        f.display(),
                        i + 1,
                        blocks.len()
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 0);
}

/// #670-14：從檔尾開始時分段數換行（不把整個 transcript 讀進記憶體）；行跨過讀取區塊的邊界、
/// 換行剛好落在邊界上、尾端有半行，行號與位移都要對。
#[test]
fn start_at_end_on_a_large_file_counts_lines_across_chunks() {
    let mut content = String::new();
    let mut n = 0u64;
    while content.len() < 300 * 1024 {
        n += 1;
        let pad = "x".repeat((n as usize * 37) % 5000);
        content.push_str(&format!(
            "{}\n",
            json!({"type": "assistant", "uuid": format!("u{n}"), "pad": pad})
        ));
    }
    content.push_str(r#"{"half"#);
    let p = tmpfile(&content);
    let mut r = TailReader::open(&p, Start::End).unwrap();
    assert!(r.poll().unwrap().lines.is_empty());
    append(&p, "\":1}\n");
    append(&p, &line("next"));
    let got = r.poll().unwrap();
    let nos: Vec<u64> = got.lines.iter().map(|l| l.no).collect();
    assert_eq!(nos, [n + 1, n + 2]);
    assert_eq!(got.lines[1].value["uuid"], "next");
}

// ── subagent 的 meta.json（#680）

/// SubagentStart 送達時 meta.json 通常還沒寫好（實測晚 40–60 ms）：重試到讀到為止。等待由呼叫端注入，測試不 sleep。
#[tokio::test]
async fn agent_meta_is_read_once_it_appears() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("meta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let transcript = dir.join("s1.jsonl");
    let meta = coding_lab::transcript::subagent_file(&transcript, "a1", "meta.json");
    assert_eq!(meta, dir.join("s1/subagents/agent-a1.meta.json"));
    // 第 2 次等待時檔案才出現
    let waits = std::cell::Cell::new(0);
    let got = coding_lab::transcript::read_agent_meta(meta.clone(), 10, || {
        waits.set(waits.get() + 1);
        if waits.get() == 2 {
            std::fs::create_dir_all(meta.parent().unwrap()).unwrap();
            std::fs::write(
                &meta,
                r#"{"agentType":"Explore","description":"d","toolUseId":"toolu_1"}"#,
            )
            .unwrap();
        }
        async {}
    })
    .await;
    assert_eq!(got, AgentMetaRead::ToolUse("toolu_1".into()));
    assert_eq!(waits.get(), 2);
}

/// Skill 開的 fork：meta.json 在但沒有 toolUseId。
#[tokio::test]
async fn agent_meta_without_tool_use_id_is_a_fork() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("meta-fork-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let meta = dir.join("agent-f.meta.json");
    std::fs::write(&meta, r#"{"agentType":"general-purpose"}"#).unwrap();
    let got = coding_lab::transcript::read_agent_meta(meta, 3, || async {}).await;
    assert_eq!(got, AgentMetaRead::NoToolUse);
}

#[tokio::test]
async fn missing_agent_meta_gives_up_after_the_tries() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("meta-none-{}", std::process::id()));
    let waits = std::cell::Cell::new(0);
    let got = coding_lab::transcript::read_agent_meta(dir.join("agent-x.meta.json"), 3, || {
        waits.set(waits.get() + 1);
        async {}
    })
    .await;
    assert_eq!(got, AgentMetaRead::Unreadable);
    assert_eq!(waits.get(), 2, "試 3 次、中間等 2 次");
}

/// #684 R11：從第 n 行之後接著讀；檔案不到 n 行（被替換或截短）就從頭讀。
#[test]
fn after_n_skips_lines_already_read_or_rereads_a_shorter_file() {
    let content: String = ["a", "b", "c", "d", "e"].iter().map(|u| line(u)).collect();
    let p = tmpfile(&content);
    let mut r = TailReader::open(&p, Start::After(3)).unwrap();
    let got: Vec<u64> = r.poll().unwrap().lines.iter().map(|l| l.no).collect();
    assert_eq!(got, [4, 5]);

    let mut r = TailReader::open(&p, Start::After(9)).unwrap();
    let got: Vec<u64> = r.poll().unwrap().lines.iter().map(|l| l.no).collect();
    assert_eq!(got, [1, 2, 3, 4, 5], "不到 9 行：從頭讀");
}

/// #684 R28、R21：1 MB 的一行分 100 次寫入，每次只掃描新增的部分；> 256 KB 的行只留必要欄位，
/// 長字串截短（記憶體不隨行長倍數成長）。
#[test]
fn big_line_written_in_pieces_is_scanned_once_and_kept_small() {
    let p = tmpfile("");
    let mut r = TailReader::open(&p, Start::Beginning).unwrap();
    let big = "x".repeat(1024 * 1024);
    let text = format!(
        "{}\n",
        json!({"type": "user", "uuid": "big", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": big}]}})
    );
    let bytes = text.as_bytes();
    let chunk = bytes.len().div_ceil(100);
    let mut lines = Vec::new();
    for piece in bytes.chunks(chunk) {
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(piece).unwrap();
        lines.extend(r.poll().unwrap().lines);
    }
    assert_eq!(lines.len(), 1);
    assert!(
        r.scanned_bytes() <= 2 * bytes.len() as u64,
        "掃描了 {} bytes（行長 {}）：每次重掃整個半行",
        r.scanned_bytes(),
        bytes.len()
    );
    let v = &lines[0].value;
    assert_eq!(
        (v["type"].as_str(), v["uuid"].as_str()),
        (Some("user"), Some("big"))
    );
    assert_eq!(v["message"]["content"][0]["tool_use_id"], "t1");
    let kept = v["message"]["content"][0]["content"].as_str().unwrap();
    assert!(kept.len() <= 70 * 1024, "長字串要截短：{}", kept.len());
}

/// #684 R20：Degraded 帶著 tail 的編號，merge 才能忽略被取代的舊 tail。
#[tokio::test]
async fn degraded_carries_the_tail_number() {
    let p = tmpfile("{壞掉\n");
    let (tx, mut rx) = mpsc::channel(16);
    let h = spawn_tail(
        p,
        "s1".into(),
        "main".into(),
        Start::Beginning,
        tx,
        TailOptions {
            poll_every: Duration::from_millis(50),
            use_notify: false,
            tail: 5,
            ..TailOptions::default()
        },
    );
    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(got, Input::Degraded { tail: 5, .. }));
    h.stop().await;
}

/// #713：`After(0)`（store 記 0：從頭讀、還沒有行產生 Event）等於從檔頭讀。
#[test]
fn after_zero_reads_from_the_beginning() {
    let content: String = ["a", "b", "c"].iter().map(|u| line(u)).collect();
    let p = tmpfile(&content);
    let mut r = TailReader::open(&p, Start::After(0)).unwrap();
    let got: Vec<u64> = r.poll().unwrap().lines.iter().map(|l| l.no).collect();
    assert_eq!(got, [1, 2, 3]);
}

/// E-S6：整個 collector 只開一個 notify watcher。資料夾依引用計數 watch／unwatch，事件依檔名分給各 tail。
#[tokio::test]
async fn shared_watcher_counts_directories_and_routes_by_file() {
    let x = tmpfile("");
    let y = x.with_file_name("y.jsonl");
    std::fs::write(&y, "").unwrap();
    let z = tmpfile("");
    let w = Arc::new(SharedWatcher::default());
    let (na, nb, nc) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let ga = w.register(&x, na.clone()).expect("watch 失敗");
    let gb = w.register(&y, nb.clone()).expect("watch 失敗");
    let gc = w.register(&z, nc.clone()).expect("watch 失敗");
    assert_eq!(w.watched_dirs(), 2, "同一個資料夾只 watch 一次");

    append(&x, &line("a"));
    tokio::time::timeout(Duration::from_secs(5), na.notified())
        .await
        .expect("寫 x 要叫醒 x 的 tail");
    assert!(
        tokio::time::timeout(Duration::ZERO, nb.notified())
            .await
            .is_err(),
        "同資料夾別的檔不該被叫醒"
    );
    assert!(
        tokio::time::timeout(Duration::ZERO, nc.notified())
            .await
            .is_err()
    );

    drop(ga);
    assert_eq!(w.watched_dirs(), 2, "y 還在用 x 的資料夾");
    drop(gb);
    assert_eq!(w.watched_dirs(), 1);
    drop(gc);
    assert_eq!(w.watched_dirs(), 0);

    // 全部放掉之後再登記還是收得到
    let _ga = w.register(&x, na.clone()).expect("watch 失敗");
    append(&x, &line("b"));
    tokio::time::timeout(Duration::from_secs(5), na.notified())
        .await
        .expect("重新登記後要收得到");
}

/// E-S6：tail 用共用 watcher（輪詢關到一小時，只靠 notify 叫醒），停掉 tail 時資料夾跟著 unwatch。
#[tokio::test]
async fn tails_share_one_watcher_and_release_it_on_stop() {
    let first = tmpfile("");
    let paths: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = first.with_file_name(format!("s{i}.jsonl"));
            std::fs::write(&p, "").unwrap();
            p
        })
        .collect();
    let w = Arc::new(SharedWatcher::default());
    let (tx, mut rx) = mpsc::channel(64);
    let tails: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            spawn_tail(
                p.clone(),
                format!("s{i}"),
                "main".into(),
                Start::Beginning,
                tx.clone(),
                TailOptions {
                    poll_every: Duration::from_secs(3600),
                    watcher: Some(w.clone()),
                    ..TailOptions::default()
                },
            )
        })
        .collect();
    // tail 開檔在 blocking 執行緒，等三個都登記好
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while w.watched_files() < 3 {
        assert!(
            std::time::Instant::now() < deadline,
            "tail 沒有登記到共用 watcher"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(w.watched_dirs(), 1);
    for p in &paths {
        append(p, &line("x"));
    }
    let mut got = std::collections::BTreeSet::new();
    while got.len() < 3 {
        if let Input::Line { session, .. } = next(&mut rx).await {
            got.insert(session);
        }
    }
    let mut it = tails.into_iter();
    it.next().unwrap().stop().await;
    assert_eq!((w.watched_files(), w.watched_dirs()), (2, 1));
    for t in it {
        t.stop().await;
    }
    assert_eq!((w.watched_files(), w.watched_dirs()), (0, 0));
}
