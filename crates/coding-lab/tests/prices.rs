//! 單價表覆寫（#678）：`<home>/prices.json` 蓋過內建表，壞掉就整個退回內建並警告。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use coding_lab::prices::{Price, Prices};

static N: AtomicU32 = AtomicU32::new(0);

/// 寫一個 prices.json 到這個 worktree 的 target/tmp/ 底下，回傳路徑。
fn file(text: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "prices-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("prices.json");
    std::fs::write(&p, text).unwrap();
    p
}

fn price(input: f64, output: f64, cache_read: f64) -> Option<Price> {
    Some(Price {
        input,
        output,
        cache_read,
    })
}

#[test]
fn overrides_listed_models_and_keeps_the_rest() {
    let p = file(r#"{"claude-opus-5-5": {"input": 10.0, "output": 50.0, "cacheRead": 1.0}}"#);
    let (prices, warning) = Prices::load(&p);
    assert_eq!(warning, None);
    assert_eq!(
        prices.get("claude-opus-5-5"),
        price(10.0, 50.0, 1.0),
        "覆寫"
    );
    assert_eq!(
        prices.get("claude-sonnet-5-5"),
        Prices::builtin().get("claude-sonnet-5-5"),
        "沒列的照用內建"
    );
}

#[test]
fn adds_models_the_builtin_table_lacks() {
    let p = file(
        r#"{"claude-new-9": {"input": 1, "output": 2, "cacheRead": 0.1}, "_comment": "說明可以放這裡"}"#,
    );
    let (prices, warning) = Prices::load(&p);
    assert_eq!(warning, None, "底線開頭的鍵是註解");
    assert_eq!(prices.get("claude-new-9"), price(1.0, 2.0, 0.1));
}

#[test]
fn model_suffix_still_matches() {
    let p = file(r#"{"claude-opus-5-5": {"input": 10, "output": 50, "cacheRead": 1}}"#);
    let (prices, _) = Prices::load(&p);
    assert_eq!(prices.get("claude-opus-5-5[1m]"), price(10.0, 50.0, 1.0));
}

#[test]
fn missing_file_is_not_an_error() {
    let p = file("");
    std::fs::remove_file(&p).unwrap();
    let (prices, warning) = Prices::load(&p);
    assert_eq!(warning, None);
    assert_eq!(
        prices.get("claude-opus-5-5"),
        Prices::builtin().get("claude-opus-5-5")
    );
}

#[test]
fn broken_or_unreasonable_files_fall_back_to_builtin_with_a_warning() {
    for text in [
        "{ 壞掉的 json",
        r#"{"claude-opus-5-5": {"input": -1, "output": 50, "cacheRead": 1}}"#,
        r#"{"claude-opus-5-5": {"input": "10", "output": 50, "cacheRead": 1}}"#,
        r#"{"claude-opus-5-5": {"input": 10, "output": 50}}"#,
        r#"{"claude-opus-5-5": 10}"#,
        r#"[1, 2]"#,
    ] {
        let (prices, warning) = Prices::load(&file(text));
        assert!(warning.is_some(), "要有警告：{text}");
        assert_eq!(
            prices.get("claude-opus-5-5"),
            Prices::builtin().get("claude-opus-5-5"),
            "整個退回內建：{text}"
        );
    }
}

/// #687（R3）：帶日期後綴的模型 id 用去掉日期的單價。
#[test]
fn dated_model_id_uses_the_undated_price() {
    let p = Prices::builtin();
    let undated = p.get("claude-haiku-4-5").expect("內建有 claude-haiku-4-5");
    assert_eq!(p.get("claude-haiku-4-5-20251001"), Some(undated));
    assert_eq!(
        p.get("claude-haiku-4-5-2025"),
        None,
        "不是 8 碼日期就不去掉"
    );
}
