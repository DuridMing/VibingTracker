//! 測試裡怎麼跑 plugin 的 wrapper（POSIX sh 腳本，#783）：Unix 用 /bin/sh；Windows 用 Git Bash 的 sh——
//! 精簡過的 wrapper（#776 EW1）最該在那裡驗。Windows 上找不到 sh 時略過並印出原因；
//! 設了 `CODING_LAB_REQUIRE_SH=1` 就直接失敗（CI 與 Windows 驗收用，不能靜默通過）。

#![allow(dead_code)] // 每個測試檔只用到其中幾個

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn wrapper_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin/bin/coding-lab")
}

/// 找 sh：`CODING_LAB_TEST_SH`；Unix 是 /bin/sh；Windows 依序找 Git 常見的安裝位置與 PATH。找不到回原因。
pub fn find_sh() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("CODING_LAB_TEST_SH") {
        return Ok(p.into());
    }
    if cfg!(unix) {
        return Ok("/bin/sh".into());
    }
    let mut candidates = Vec::new();
    for var in [
        "ProgramFiles",
        "ProgramW6432",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
    ] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            let git = if var == "LOCALAPPDATA" {
                base.join("Programs").join("Git")
            } else {
                base.join("Git")
            };
            candidates.push(git.join("usr").join("bin").join("sh.exe"));
            candidates.push(git.join("bin").join("sh.exe"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join("sh.exe")));
    }
    if let Some(found) = candidates.iter().find(|c| c.is_file()) {
        return Ok(found.clone());
    }
    let tried: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    Err(format!(
        "找不到 Git Bash 的 sh（試過 {}；可用 CODING_LAB_TEST_SH 指定）",
        tried.join("、")
    ))
}

/// 要跑 wrapper 的測試先呼叫這個：回 sh；找不到時印出原因並回 None（呼叫端略過），
/// `CODING_LAB_REQUIRE_SH=1` 時直接失敗。
pub fn sh_or_skip(test: &str) -> Option<PathBuf> {
    match find_sh() {
        Ok(p) => Some(p),
        Err(why) => {
            if std::env::var_os("CODING_LAB_REQUIRE_SH").is_some_and(|v| v == "1") {
                panic!("{test}：{why}");
            }
            eprintln!("略過 {test}：{why}");
            None
        }
    }
}

/// `sh <wrapper>`，參數由呼叫端接著加。
pub fn wrapper_cmd(sh: &Path) -> Command {
    let mut c = Command::new(sh);
    c.arg(to_sh(&wrapper_path()));
    c
}

/// 給 sh 腳本用的路徑：Windows 的 `C:\a\b` 換成 MSYS 的 `/c/a/b`（wrapper 裡 Git 的 GNU tar 會把 `C:` 當成
/// 遠端主機）；Unix 原樣。給 Windows 原生程式（真的 coding-lab.exe）的路徑不要用這個。
pub fn to_sh(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if cfg!(windows)
        && let Some((drive, rest)) = s.split_once(':')
        && drive.len() == 1
    {
        return format!("/{}{rest}", drive.to_ascii_lowercase());
    }
    s
}

/// 跑 wrapper 時的 PATH：`dirs`（假的 uname、curl…）在前，再來是基本工具——Unix 的 /usr/bin、/bin，
/// Windows 是 Git 的 usr\bin。Windows 用 Windows 的格式（`;` 分隔），Git Bash 啟動時自己轉成 POSIX 的。
pub fn path_env(sh: &Path, dirs: &[&Path]) -> OsString {
    let mut all: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    if cfg!(windows) {
        all.push(tools_dir(sh));
    } else {
        all.push("/usr/bin".into());
        all.push("/bin".into());
    }
    std::env::join_paths(all).unwrap_or_default()
}

/// Git for Windows 放 coreutils 的目錄：`...\Git\usr\bin`（sh 在 `Git\bin` 時往旁邊找）。
fn tools_dir(sh: &Path) -> PathBuf {
    let dir = sh.parent().unwrap_or(Path::new("."));
    if dir.ends_with("usr/bin") || dir.ends_with("usr\\bin") {
        return dir.to_path_buf();
    }
    let usr = dir.parent().map(|g| g.join("usr").join("bin"));
    usr.filter(|u| u.is_dir())
        .unwrap_or_else(|| dir.to_path_buf())
}

/// 測試用的假 Release 網址。Windows 上 Git 的 curl 是原生程式，要 `file:///C:/…`。
pub fn file_url(p: &Path) -> String {
    if cfg!(windows) {
        format!("file:///{}", p.to_string_lossy().replace('\\', "/"))
    } else {
        format!("file://{}", p.display())
    }
}

/// 寫一個可執行的 sh 腳本（Windows 上 Git Bash 依 `#!` 判斷能不能執行，不用設權限）。
pub fn write_script(p: &Path, body: &str) {
    std::fs::write(p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
