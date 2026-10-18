//! web 入口（collector、hub）共用的 percent-decode 與查詢字串解析（#775 E-D5）。

use coding_lab::http_util::{form_decode, percent_decode, query};

#[test]
fn percent_decode_handles_utf8_truncated_and_invalid_escapes() {
    assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
    assert_eq!(percent_decode("%2Fp%2Fq"), "/p/q");
    assert_eq!(percent_decode("a%4"), "a%4", "截斷的 % 原樣保留");
    assert_eq!(percent_decode("a%"), "a%");
    assert_eq!(percent_decode("%zz%41"), "%zzA", "不是十六進位的原樣保留");
    assert_eq!(percent_decode("%é"), "%é", "% 後面是非 ASCII 時不能 panic");
    assert_eq!(percent_decode("a+b"), "a+b", "路徑裡的 + 不是空白");
}

#[test]
fn form_decoding_turns_plus_into_space_but_keeps_encoded_plus() {
    assert_eq!(form_decode("a+b"), "a b");
    assert_eq!(form_decode("a%2Bb"), "a+b");
}

#[test]
fn query_takes_the_last_value_and_decodes_keys_and_values() {
    let uri: axum::http::Uri = "/api/files?cwd=%2Fa&base=main&cwd=%2Fb&x+y=%E4%B8%AD+1&flag"
        .parse()
        .unwrap();
    let q = query(&uri);
    assert_eq!(q["cwd"], "/b", "同名參數取最後一個");
    assert_eq!(q["base"], "main");
    assert_eq!(q["x y"], "中 1");
    assert_eq!(q["flag"], "");
}
