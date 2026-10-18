//! 路徑正規化與比對（#748 L-11，contracts §4「路徑正規化與比對」）：collector 內部一律 `/` 分隔（不變式 11）、
//! Windows 的磁碟機代號一律大寫（VS Code 的終端機給小寫的 `e:\`）。比較兩個路徑時 Windows 與 macOS 不分大小寫
//! （ASCII 轉小寫後比），Linux 分大小寫；存下來的值保留原本的大小寫。前端的同一套規則在 `web/js/paths.js`。
//!
//! `*_with` 版本把「分不分大小寫」當參數，測試在任何平台都能驗兩種規則；平常用不帶 `_with` 的。

/// 這個平台比對路徑時不分大小寫（Windows、macOS 預設的檔案系統）。
pub const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// `\` 換成 `/`、開頭的磁碟機代號轉大寫（`e:\x` → `E:/x`）。要存下來的路徑用這個。
pub fn normalize(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_lowercase() {
        p[..1].make_ascii_uppercase();
    }
    p
}

/// 比對用的形式：正規化、去掉結尾的 `/`。
fn trimmed(path: &str) -> String {
    let mut p = normalize(path);
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// 兩個路徑指的是同一個（依這個平台的大小寫規則）。
pub fn same(a: &str, b: &str) -> bool {
    same_with(a, b, CASE_INSENSITIVE)
}

pub fn same_with(a: &str, b: &str, insensitive: bool) -> bool {
    let (a, b) = (trimmed(a), trimmed(b));
    if insensitive {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

/// `path` 在 `dir` 底下時回傳相對路徑（`/` 分隔、保留 `path` 原本的大小寫）；就是 `dir` 本身或不在底下時回 None。
pub fn strip_dir(dir: &str, path: &str) -> Option<String> {
    strip_dir_with(dir, path, CASE_INSENSITIVE)
}

pub fn strip_dir_with(dir: &str, path: &str, insensitive: bool) -> Option<String> {
    let dir = trimmed(dir);
    if dir.is_empty() || dir == "/" {
        return None;
    }
    let path = normalize(path);
    let n = dir.len();
    if path.len() <= n || !path.is_char_boundary(n) || path.as_bytes()[n] != b'/' {
        return None;
    }
    let head = &path[..n];
    let same = if insensitive {
        head.eq_ignore_ascii_case(&dir)
    } else {
        head == dir
    };
    same.then(|| path[n + 1..].to_string())
}

/// `path` 就是 `dir`，或在它底下。
pub fn within(dir: &str, path: &str) -> bool {
    within_with(dir, path, CASE_INSENSITIVE)
}

pub fn within_with(dir: &str, path: &str, insensitive: bool) -> bool {
    same_with(dir, path, insensitive) || strip_dir_with(dir, path, insensitive).is_some()
}
