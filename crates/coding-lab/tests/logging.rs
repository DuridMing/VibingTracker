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

/// W-L12（#781）：別的程序把 collector.log 輪替（改名成 `.1`）之後，collector 手上的 handle 跟著改名後的檔走；
/// 寫下一行前發現路徑上的檔已經不是自己開的那個，就重開，訊息才會出現在使用者看的 collector.log。
#[test]
fn a_log_replaced_by_someone_else_is_reopened() {
    let d = dir("replaced");
    let path = d.join("collector.log");
    let mut log = RotatingLog::open(&path, 1000, 5).unwrap();
    log.write_line("before").unwrap();
    std::fs::rename(&path, d.join("collector.log.1")).unwrap();
    std::fs::write(&path, "someone else\n").unwrap();
    log.write_line("after").unwrap();
    let current = read(&path);
    assert!(current.contains("after"), "{current}");
    assert!(!read(&d.join("collector.log.1")).contains("after"));
}

/// W-L12：短命的程序（ensure、service、還沒拿到 store 鎖的 serve）只追加、不輪替；
/// 輪替只由拿著 store 鎖的 collector 做，否則它手上的 handle 會被推進 `.1`。
#[test]
fn append_only_logs_never_rotate() {
    let d = dir("append-only");
    let path = d.join("collector.log");
    // 已經到上限的檔（稀疏檔，不真的寫 10 MB）
    std::fs::File::create(&path)
        .unwrap()
        .set_len(coding_lab::logging::LOG_MAX_BYTES)
        .unwrap();
    let mut log = RotatingLog::append_only(&path).unwrap();
    log.write_line("0123456789").unwrap();
    assert!(!d.join("collector.log.1").exists());
    coding_lab::logging::append(&path, "short-lived");
    assert!(read(&path).contains("short-lived"));
    assert!(!d.join("collector.log.1").exists());
}

/// B-S2（#739，Windows 驗收）：沒有設定檔案時 log 寫 stderr；stderr 寫不出去（服務的主控台已經不在、pipe 的讀取端關了）
/// 時不能 panic。release 是 panic = "abort"，原本的 `eprintln!` 一寫失敗就讓 collector 直接死掉、不走正常結束。
/// 用子程序重跑這個測試：stderr 接一個讀取端已關閉的 pipe，`--nocapture` 讓它真的寫到 stderr。
#[test]
fn log_to_a_closed_stderr_does_not_panic() {
    if std::env::var_os("CODING_LAB_CLOSED_STDERR_CHILD").is_some() {
        for i in 0..100 {
            coding_lab::logging::log(&format!("line {i}"));
        }
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "log_to_a_closed_stderr_does_not_panic",
            "--nocapture",
        ])
        .env("CODING_LAB_CLOSED_STDERR_CHILD", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take()); // 讀取端關掉：之後寫 stderr 都是 EPIPE
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "寫不出 stderr 時 log 讓程序失敗了：{status}"
    );
}

/// B-S1（#739）：服務用 `conhost --headless` 跑時 stderr 是看不到的主控台（is_terminal 成立），只看終端機的話
/// collector.log 一行都沒有。帶了 `--log-file` 就一律寫檔；沒帶時照舊：stderr 是終端機（開發時前景跑）才印畫面。
#[test]
fn log_file_flag_wins_over_a_terminal_stderr() {
    use coding_lab::logging::use_log_file;
    assert!(use_log_file(true, true), "服務的 headless 主控台也要寫檔");
    assert!(use_log_file(true, false));
    assert!(
        use_log_file(false, false),
        "ensure 拉起的（stderr 導到檔案）照舊寫檔"
    );
    assert!(!use_log_file(false, true), "前景開發照舊印到畫面");
}
