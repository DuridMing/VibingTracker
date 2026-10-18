//! web 入口（collector 的 API、hub 的轉發）共用的小工具（#775 E-D5）：percent-decode、查詢字串、JSON 回應。
//! 原本 api、auth、hub 各寫一份，hub 那份在 `%` 後面接非 ASCII 時會切在字元中間而 panic。

use std::collections::HashMap;

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

/// `%XX` 解碼（路徑片段用：`+` 不是空白）；截斷或不是十六進位的 `%` 原樣保留，結果以 lossy UTF-8 轉成字串。
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `application/x-www-form-urlencoded` 的值（查詢字串、登入表單）：`+` 是空白，`%2B` 才是 `+`。
pub fn form_decode(s: &str) -> String {
    percent_decode(&s.replace('+', " "))
}

/// 查詢字串的所有參數；同名參數取最後一個。
pub fn query(uri: &Uri) -> HashMap<String, String> {
    uri.query()
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (form_decode(k), form_decode(v))
        })
        .collect()
}

/// 查詢字串的一個參數（第一個）。
pub fn query_param(uri: &Uri, name: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (form_decode(k) == name).then(|| form_decode(v))
    })
}

/// JSON 回應。
pub fn json(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}
