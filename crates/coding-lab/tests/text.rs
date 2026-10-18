//! 共用截斷工具（E-D6）：bytes 截在字元邊界、依字元截、截短 Value 裡的長字串。

use coding_lab::text::{
    floor_boundary, truncate_bytes, truncate_chars, truncate_string, truncated_clone,
};
use serde_json::{Value, json};

// 「中」是 3 bytes、「😀」是 4 bytes
const MIXED: &str = "ab中😀c";

#[test]
fn floor_boundary_backs_off_to_a_char_boundary() {
    assert_eq!(floor_boundary(MIXED, 0), 0);
    assert_eq!(floor_boundary(MIXED, 2), 2);
    assert_eq!(floor_boundary(MIXED, 3), 2, "切在「中」中間要退回它前面");
    assert_eq!(floor_boundary(MIXED, 4), 2);
    assert_eq!(floor_boundary(MIXED, 5), 5);
    assert_eq!(floor_boundary(MIXED, 8), 5, "切在 emoji 中間");
    assert_eq!(floor_boundary(MIXED, 9), 9);
    assert_eq!(floor_boundary(MIXED, 10), 10);
    assert_eq!(floor_boundary(MIXED, 100), MIXED.len(), "超過長度回長度");
}

#[test]
fn truncate_bytes_keeps_utf8_valid() {
    assert_eq!(truncate_bytes(MIXED, 100), (MIXED, false));
    assert_eq!(
        truncate_bytes(MIXED, MIXED.len()),
        (MIXED, false),
        "剛好在上限不算截斷"
    );
    assert_eq!(truncate_bytes(MIXED, MIXED.len() - 1), ("ab中😀", true));
    assert_eq!(truncate_bytes(MIXED, 4), ("ab", true));
    assert_eq!(truncate_bytes(MIXED, 5), ("ab中", true));
    assert_eq!(truncate_bytes(MIXED, 0), ("", true));
    assert_eq!(truncate_bytes("", 0), ("", false));
}

#[test]
fn truncate_chars_counts_code_points() {
    assert_eq!(truncate_chars(MIXED, 5), (MIXED, false), "剛好 5 個字元");
    assert_eq!(truncate_chars(MIXED, 6), (MIXED, false));
    assert_eq!(truncate_chars(MIXED, 4), ("ab中😀", true));
    assert_eq!(truncate_chars(MIXED, 3), ("ab中", true));
    assert_eq!(truncate_chars("中文字", 2), ("中文", true));
    assert_eq!(truncate_chars(MIXED, 0), ("", true));
}

#[test]
fn truncate_string_in_place() {
    let mut s = MIXED.to_string();
    assert!(!truncate_string(&mut s, MIXED.len()));
    assert_eq!(s, MIXED);
    assert!(truncate_string(&mut s, 8));
    assert_eq!(s, "ab中");
    let mut long = "中".repeat(3000);
    assert!(truncate_string(&mut long, 8 * 1024));
    assert!(long.len() <= 8 * 1024);
    assert_eq!(long.len() % 3, 0, "沒有切壞字元");
}

#[test]
fn truncated_clone_cuts_only_long_strings() {
    let long = "x".repeat(20);
    let v = json!({
        "command": "ls",
        "content": long,
        "nested": [{"s": "中".repeat(10)}, 1, true, null],
        "n": 3.5,
    });
    let (out, cut) = truncated_clone(&v, 10);
    assert!(cut);
    assert_eq!(out["command"], "ls");
    assert_eq!(out["content"], "x".repeat(10));
    assert_eq!(
        out["nested"][0]["s"],
        "中".repeat(3),
        "10 bytes 以內只能放 3 個「中」"
    );
    assert_eq!(out["nested"][1], 1);
    assert_eq!(out["nested"][2], true);
    assert_eq!(out["nested"][3], Value::Null);
    assert_eq!(out["n"], 3.5);
}

#[test]
fn truncated_clone_without_long_strings_is_an_exact_copy() {
    let v = json!({"file_path": "/a/b.rs", "old_string": "中", "replace_all": false, "k": [1, 2]});
    let (out, cut) = truncated_clone(&v, 10);
    assert!(!cut);
    assert_eq!(out, v);
    let (out, cut) = truncated_clone(&json!("剛好十個bytes"), "剛好十個bytes".len());
    assert!(!cut, "剛好在上限不算截斷");
    assert_eq!(out, "剛好十個bytes");
}
