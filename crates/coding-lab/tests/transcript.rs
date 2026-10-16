//! transcript tail（#631）。檔案都在這個 worktree 的 target/tmp/。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use coding_lab::merge::Input;
use coding_lab::transcript::{Start, TailOptions, TailReader, spawn_tail};
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
