//! E-H1（#756）：同一個 API 請求裡，每個目錄的 filter driver 只查一次（`git config --get-regexp`）。
//! 用 PATH 前面的假 git 記下每次呼叫；PATH 是整個程序共用的，所以獨立成一個測試檔、只有這一條測試。

#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use coding_lab::event::Event;
use coding_lab::store::Store;
use coding_lab::ws::{WebState, serve_web};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("gitcalls-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
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
}

#[tokio::test]
async fn filter_drivers_are_looked_up_once_per_directory_per_request() {
    let base = tmp("repo");
    let root = base.join("r");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    for wt in ["x", "y"] {
        git(
            &root,
            &["worktree", "add", "-q", "-b", wt, &format!("wt/{wt}")],
        );
        std::fs::write(root.join("wt").join(wt).join("a.txt"), format!("{wt}\n")).unwrap();
    }
    // 假 git：記下參數再交給真的 git
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = base.join("calls.log");
    let fake = bin.join("git");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.trim()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY 註記不需要：這個測試程序只有這一條測試，沒有別的執行緒在讀環境變數
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
    }

    let home = tmp("home");
    let mut store = Store::open(&home).unwrap();
    let cwd = root.join("wt/x").to_string_lossy().into_owned();
    store.append(
        Event::from_json_line(
            &json!({"v": 1, "session": "s1", "seq": 1, "type": "session.started", "agent": "main", "line": null,
                    "pos": null, "ts_model": null, "ts_exec": null, "ts_seen": 1, "host": "local",
                    "payload": {"cwd": cwd, "source": "startup", "model": "m", "transcriptPath": "/t", "branch": "x"}})
            .to_string(),
        )
        .unwrap(),
    );
    store.flush().unwrap();
    let state = WebState::new(Some(Arc::new(Mutex::new(store))), None, 8);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    let _ = std::fs::remove_file(&log);
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let path = format!("/api/files?cwd={}&base=main", cwd.replace('/', "%2F"));
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");

    let calls = std::fs::read_to_string(&log).unwrap();
    let mut lookups: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0;
    for line in calls.lines() {
        total += 1;
        if line.contains("--get-regexp") {
            let words: Vec<&str> = line.split_whitespace().collect();
            let dir = words
                .iter()
                .position(|w| *w == "-C")
                .and_then(|i| words.get(i + 1))
                .copied()
                .unwrap_or("?");
            *lookups.entry(dir.to_string()).or_default() += 1;
        }
    }
    eprintln!("一次 /api/files（2 個 worktree）：{total} 個 git 程序");
    assert!(!lookups.is_empty(), "要有 filter 查詢：{calls}");
    // E-H2（#767）：未追蹤檔每個目錄只掃一次（樹的 ls-files 一併分出來，主工作樹算改動時不再掃）
    let mut others: BTreeMap<String, usize> = BTreeMap::new();
    for line in calls
        .lines()
        .filter(|l| l.contains("ls-files") && l.contains("--others"))
    {
        let words: Vec<&str> = line.split_whitespace().collect();
        let dir = words
            .iter()
            .position(|w| *w == "-C")
            .and_then(|i| words.get(i + 1))
            .copied()
            .unwrap_or("?");
        *others.entry(dir.to_string()).or_default() += 1;
    }
    assert!(
        others.values().all(|n| *n == 1),
        "未追蹤檔掃了不只一次：{others:?}"
    );
    assert!(
        lookups.values().all(|n| *n == 1),
        "同一個目錄查了不只一次（共 {total} 個 git 程序）：{lookups:?}"
    );
    // E-H2：約 1 秒內同一個 (cwd, base) 共用結果，不再跑 git（多個分頁同時輪詢）
    let _ = std::fs::remove_file(&log);
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut again = String::new();
    s.read_to_string(&mut again).await.unwrap();
    assert!(again.starts_with("HTTP/1.1 200"), "{again}");
    let n = std::fs::read_to_string(&log)
        .map(|c| c.lines().count())
        .unwrap_or(0);
    assert_eq!(n, 0, "1 秒內的同一個請求不該再跑 git（跑了 {n} 個）");

    // E-H9（#756）：分支視窗的 git 程序數（印出來比較；正確性由 tests/api.rs 的 branches 測試保證）
    let _ = std::fs::remove_file(&log);
    let path = format!("/api/branches?cwd={}", cwd.replace('/', "%2F"));
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    eprintln!(
        "一次 /api/branches（2 個 worktree）：{} 個 git 程序",
        calls.lines().count()
    );
    assert!(
        !calls.contains("status --porcelain"),
        "dirty 不再用 status 重掃未追蹤檔：{calls}"
    );
}
