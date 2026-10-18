//! collector.log 輪替（#655）。

use std::path::{Path, PathBuf};

use coding_lab::logging::RotatingLog;

fn dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("logging-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn log_rotates_at_the_size_limit_and_keeps_five() {
    let d = dir("rotate");
    let path = d.join("collector.log");
    let mut log = RotatingLog::open(&path, 100, 5).unwrap();
    // 每行 20 bytes（含換行），一個檔放 5 行；寫 40 行 → 8 個檔的量，只留目前的＋5 個
    for i in 0..40 {
        log.write_line(&format!("line {i:03} xxxxxxxxx")).unwrap();
    }
    for i in 1..=5 {
        let p = d.join(format!("collector.log.{i}"));
        assert!(p.exists(), "要有 .{i}");
        assert!(read(&p).len() <= 100, ".{i} 不能超過上限");
    }
    assert!(!d.join("collector.log.6").exists(), "只留 5 個舊檔");
    let current = read(&path);
    assert!(current.len() <= 100);
    assert!(current.contains("line 039"), "最新的在 collector.log");
    assert!(read(&d.join("collector.log.1")).contains("line 034"));
    let all: String = (1..=5)
        .map(|i| read(&d.join(format!("collector.log.{i}"))))
        .collect();
    assert!(!all.contains("line 000"), "最舊的被丟掉");
}

#[test]
fn reopening_continues_from_the_existing_size() {
    let d = dir("reopen");
    let path = d.join("collector.log");
    std::fs::write(&path, "x".repeat(95)).unwrap();
    let mut log = RotatingLog::open(&path, 100, 5).unwrap();
    log.write_line("0123456789").unwrap();
    assert_eq!(
        read(&d.join("collector.log.1")).len(),
        95,
        "加上去會超過就先輪替"
    );
    assert_eq!(read(&path), "0123456789\n");
}

/// L-15（#750）：輪替失敗（Windows 上 collector.log 被不共用刪除權限的程式開著時 rename 會失敗）不能讓之後的
/// log 全部丟掉：照樣寫進目前的檔，過一陣子再試輪替。這裡讓 `.1` 是有東西的資料夾，rename 一定失敗。
#[test]
fn a_failed_rotation_keeps_appending_to_the_current_file() {
    let d = dir("rotate-fails");
    let path = d.join("collector.log");
    std::fs::create_dir_all(d.join("collector.log.1/x")).unwrap();
    let mut log = RotatingLog::open(&path, 50, 1).unwrap();
    for i in 0..10 {
        let _ = log.write_line(&format!("line {i:03} xxxxxxxxx"));
    }
    let current = read(&path);
    for i in 0..10 {
        assert!(current.contains(&format!("line {i:03}")), "{current}");
    }
}

/// L-15：panic 訊息原本只到 stderr，而背景 collector 的 stderr 是 ensure 開的那個 collector.log；
/// 輪替之後那個 handle 指到 `.1`（最後被刪掉）。panic hook 改寫進目前的 log。
#[test]
fn panics_are_written_to_the_current_log() {
    let d = dir("panic");
    let path = d.join("collector.log");
    coding_lab::logging::to_file(&path).unwrap();
    coding_lab::logging::log_panics();
    let r = std::panic::catch_unwind(|| panic!("boom 1234"));
    assert!(r.is_err());
    let text = read(&path);
    assert!(
        text.contains("panic") && text.contains("boom 1234"),
        "{text}"
    );
    assert!(text.contains("logging.rs"), "要有位置：{text}");
}
