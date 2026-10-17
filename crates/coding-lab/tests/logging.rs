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
