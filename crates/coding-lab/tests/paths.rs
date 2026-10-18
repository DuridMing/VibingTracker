//! 路徑正規化與比對（#748 L-11、contracts §4）：`\` 換成 `/`、磁碟機代號大寫；Windows 與 macOS 比對不分大小寫。

use coding_lab::paths::{normalize, same_with, strip_dir_with, within_with};

#[test]
fn backslashes_and_lowercase_drive_letters_are_normalized() {
    assert_eq!(normalize("e:\\Repo\\src"), "E:/Repo/src");
    assert_eq!(normalize("C:/x"), "C:/x");
    assert_eq!(normalize("/home/u/p"), "/home/u/p");
    assert_eq!(normalize("ab:c"), "ab:c", "不是磁碟機代號");
    assert_eq!(normalize(""), "");
}

#[test]
fn comparison_follows_the_platform_case_rule() {
    // Windows／macOS：不分大小寫
    assert!(same_with("e:\\Repo", "E:/repo/", true));
    assert!(same_with("/Users/u/Proj", "/users/u/proj", true));
    // Linux：分大小寫，但 `\` 與結尾的 `/` 一樣不算差別
    assert!(!same_with("/home/u/Proj", "/home/u/proj", false));
    assert!(same_with("/home/u/p/", "/home/u/p", false));
}

#[test]
fn strip_dir_keeps_the_original_case_of_the_rest() {
    assert_eq!(
        strip_dir_with("e:\\repo", "E:\\Repo\\Src\\A.rs", true).as_deref(),
        Some("Src/A.rs")
    );
    assert_eq!(
        strip_dir_with("/home/u/Repo", "/home/u/repo/a.rs", false),
        None
    );
    assert_eq!(
        strip_dir_with("/home/u/p", "/home/u/p/a.rs", false).as_deref(),
        Some("a.rs")
    );
    assert_eq!(
        strip_dir_with("/home/u/p", "/home/u/pp/a.rs", false),
        None,
        "不是同一層"
    );
    assert_eq!(
        strip_dir_with("/home/u/p", "/home/u/p", false),
        None,
        "目錄本身"
    );
    assert_eq!(strip_dir_with("", "/a", false), None);
    assert_eq!(
        strip_dir_with("/x/甲", "/x/甲/乙.rs", true).as_deref(),
        Some("乙.rs"),
        "非 ASCII 不出錯"
    );
    assert_eq!(
        strip_dir_with("/x/ab", "/x/甲/乙", true),
        None,
        "長度切在字元中間也不出錯"
    );
    assert!(within_with("/Home/U/p", "/home/u/p/a", true));
    assert!(within_with("/home/u/p", "/home/u/p", false));
    assert!(!within_with("/home/u/p", "/home/u", false));
}
