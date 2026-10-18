//! 截斷工具（E-D6）：依 bytes 截在字元邊界、依字元截、截短 Value 裡的長字串。
//! merge、transcript、api 原本各寫一份「往回找字元邊界」的迴圈，之後都用這裡的。

use serde_json::Value;

/// `max` 以內、最靠近 `max` 的字元邊界（`max` 超過長度時回長度）。
pub fn floor_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// 截到 `max` bytes 以內、不切壞 UTF-8；回傳截過的切片與是否有截斷。
pub fn truncate_bytes(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    (&s[..floor_boundary(s, max)], true)
}

/// 截到 `max` 個字元（code point）以內；回傳截過的切片與是否有截斷。
pub fn truncate_chars(s: &str, max: usize) -> (&str, bool) {
    match s.char_indices().nth(max) {
        Some((end, _)) => (&s[..end], true),
        None => (s, false),
    }
}

/// 就地截到 `max` bytes 以內、不切壞 UTF-8；回傳是否有截斷。
pub fn truncate_string(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let end = floor_boundary(s, max);
    s.truncate(end);
    true
}

/// `v` 的副本，其中每個超過 `max` bytes 的字串都截到 `max` bytes 以內；回傳是否有截到。
/// 從借用直接產出，長字串只拷貝留下的部分。
pub fn truncated_clone(v: &Value, max: usize) -> (Value, bool) {
    match v {
        Value::String(s) => {
            let (kept, cut) = truncate_bytes(s, max);
            (Value::String(kept.to_string()), cut)
        }
        Value::Array(items) => {
            let mut cut = false;
            let items = items
                .iter()
                .map(|x| {
                    let (x, c) = truncated_clone(x, max);
                    cut |= c;
                    x
                })
                .collect();
            (Value::Array(items), cut)
        }
        Value::Object(map) => {
            let mut cut = false;
            let map = map
                .iter()
                .map(|(k, x)| {
                    let (x, c) = truncated_clone(x, max);
                    cut |= c;
                    (k.clone(), x)
                })
                .collect();
            (Value::Object(map), cut)
        }
        other => (other.clone(), false),
    }
}
