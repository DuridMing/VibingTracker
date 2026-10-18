//! 「現在在做什麼」一句話（#648，contracts.md §6 `sessions.doing`、PLAN「大量 session 下…」）。

use coding_lab::doing::{Doing, Source, first_sentence};

fn first(s: &str) -> Option<String> {
    first_sentence(s)
}

// ── 切句

#[test]
fn english_sentence_ends_at_punctuation_followed_by_space() {
    assert_eq!(
        first("I'll start by reading calc.py to see what's there. Then fix it."),
        Some("I'll start by reading calc.py to see what's there.".into()),
        "檔名裡的 . 不切"
    );
    assert_eq!(first("Done! Next step"), Some("Done!".into()));
    assert_eq!(first("Really? Yes"), Some("Really?".into()));
}

#[test]
fn chinese_sentence_ends_at_full_width_punctuation() {
    assert_eq!(
        first("測試通過。接下來是刻意失敗的兩個指令。"),
        Some("測試通過。".into())
    );
    assert_eq!(first("真的嗎？是的"), Some("真的嗎？".into()));
    assert_eq!(first("好了！下一步"), Some("好了！".into()));
}

#[test]
fn urls_versions_and_decimals_are_not_split() {
    assert_eq!(
        first("See https://example.com/a.b?x=1 for details. More"),
        Some("See https://example.com/a.b?x=1 for details.".into())
    );
    assert_eq!(
        first("Upgraded to 1.2.3 today. Done"),
        Some("Upgraded to 1.2.3 today.".into())
    );
    assert_eq!(
        first("It costs 3.5 dollars! ok"),
        Some("It costs 3.5 dollars!".into())
    );
}

#[test]
fn backticks_are_removed_and_never_split_inside() {
    assert_eq!(
        first("問題確認：`sub` 回傳的是 `a + b`，應該是 `a - b`。後面"),
        Some("問題確認：sub 回傳的是 a + b，應該是 a - b。".into())
    );
    assert_eq!(
        first("Run `echo a. b` now. Next"),
        Some("Run echo a. b now.".into()),
        "反引號裡的句點不切"
    );
}

#[test]
fn markdown_marks_are_stripped_and_newlines_collapsed() {
    assert_eq!(
        first("- **Step one** is reading\nthe file. Two"),
        Some("Step one is reading the file.".into())
    );
    assert_eq!(first("## 計畫\n1. 先讀檔。"), Some("計畫 先讀檔。".into()));
    assert_eq!(first("> 引用的話。"), Some("引用的話。".into()));
    assert_eq!(
        first("刪掉 __pycache__ 就好。"),
        Some("刪掉 __pycache__ 就好。".into()),
        "__ 不是粗體記號"
    );
}

#[test]
fn blank_line_ends_the_sentence() {
    assert_eq!(
        first("Subagent 回報\n\n注意事項很多"),
        Some("Subagent 回報".into())
    );
}

#[test]
fn no_terminator_takes_the_whole_text() {
    assert_eq!(first("Working on it"), Some("Working on it".into()));
}

#[test]
fn truncated_to_80_code_points_with_ellipsis() {
    let long: String = "字".repeat(100);
    let got = first(&long).unwrap();
    assert_eq!(got.chars().count(), 81);
    assert!(got.ends_with('…'));
    assert_eq!(got.trim_end_matches('…'), "字".repeat(80));

    let exact: String = "字".repeat(80);
    assert_eq!(first(&exact), Some(exact.clone()), "剛好 80 不加…");
}

#[test]
fn empty_or_blank_is_none() {
    assert_eq!(first(""), None);
    assert_eq!(first("  \n\n "), None);
    assert_eq!(first("``"), None);
}

// ── 取句順序（只看這一輪）

#[test]
fn falls_back_text_thinking_tool_prompt_within_the_round() {
    let mut d = Doing::default();
    assert_eq!(d.local(), None);

    d.prompt("修好 sub 的 bug", false);
    assert_eq!(d.local(), Some((Source::Prompt, "修好 sub 的 bug")));

    d.tool("$ cargo test");
    assert_eq!(d.local(), Some((Source::Tool, "$ cargo test")));

    d.thinking("");
    assert_eq!(
        d.local(),
        Some((Source::Tool, "$ cargo test")),
        "空 thinking 跳過"
    );

    d.thinking("Let me check the tests. Then fix.");
    assert_eq!(
        d.local(),
        Some((Source::Thinking, "Let me check the tests."))
    );

    d.text("測試通過。接下來改 sub。");
    assert_eq!(d.local(), Some((Source::Text, "測試通過。")));

    d.tool("edit calc.py");
    assert_eq!(
        d.local(),
        Some((Source::Text, "測試通過。")),
        "這一輪有文字就用文字"
    );
}

#[test]
fn new_prompt_starts_a_new_round() {
    let mut d = Doing::default();
    d.prompt("第一個", false);
    d.text("上一輪的結尾。");
    d.prompt("第二個", false);
    assert_eq!(d.local(), Some((Source::Prompt, "第二個")));
    d.tool("$ ls");
    assert_eq!(
        d.local(),
        Some((Source::Tool, "$ ls")),
        "上一輪的文字不會蓋過這一輪的工具"
    );
}

#[test]
fn task_notification_round_shows_background_done() {
    let mut d = Doing::default();
    d.prompt(
        "<task-notification><task-id>x</task-id></task-notification>",
        true,
    );
    assert_eq!(d.local(), Some((Source::Prompt, "背景工作完成")));
}
