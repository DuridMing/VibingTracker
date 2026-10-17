//! 敏感輸出（#654）：讀 `.env`、金鑰、憑證設定這類工具的輸出標 `sensitive`，WS／API 對遠端連線一律剝除
//! （不變式 6、contracts §4）。只影響送出的副本，本機 JSONL 照存（截 8 KB）。
//!
//! 判斷只看路徑與指令，不看輸出內容：檔案工具看 `file_path`／`path`／`notebook_path`；Bash 依 `&&`／`|` 等切段，
//! 任何一段的參數是敏感路徑、或是 `printenv`／單獨的 `env`／`export -p`（把整個環境印出來）就算。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::derive::shell_segments;

/// 檔名（basename）規則；`.env` 的範本（example／sample／template／dist）不算。
const NAME_PATTERNS: &[&str] = &[
    r"^\.env(\.[A-Za-z0-9_-]+)?$",
    r"^id_(rsa|dsa|ecdsa|ed25519)$",
    r"\.(pem|key|p12|pfx|jks|keystore)$",
    r"^\.(netrc|npmrc|pypirc|pgpass|git-credentials)$",
    r"^secrets?\.(ya?ml|json|toml|env)$",
    r"^credentials$",
    r"(^|[-_.])credentials\.json$",
];
const ENV_TEMPLATES: &[&str] = &[".example", ".sample", ".template", ".dist"];
/// 要看整段路徑的
const PATH_PATTERNS: &[&str] = &[
    r"(^|/)\.aws/(credentials|config)$",
    r"(^|/)\.kube/config$",
    r"(^|/)\.docker/config\.json$",
];

fn compiled(
    list: &'static [&'static str],
    cell: &'static OnceLock<Vec<Regex>>,
) -> &'static [Regex] {
    cell.get_or_init(|| list.iter().filter_map(|p| Regex::new(p).ok()).collect())
}

/// 這個路徑的內容可能是機密。
pub fn is_sensitive_path(path: &str) -> bool {
    static NAMES: OnceLock<Vec<Regex>> = OnceLock::new();
    static PATHS: OnceLock<Vec<Regex>> = OnceLock::new();
    let path = path.replace('\\', "/");
    let path = path.trim_matches(|c| c == '"' || c == '\'');
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.starts_with(".env") && ENV_TEMPLATES.iter().any(|t| name.ends_with(t)) {
        return false;
    }
    compiled(NAME_PATTERNS, &NAMES)
        .iter()
        .any(|r| r.is_match(name))
        || compiled(PATH_PATTERNS, &PATHS)
            .iter()
            .any(|r| r.is_match(path))
}

/// 這個工具呼叫的輸出會不會含機密。
pub fn is_sensitive(tool: &str, input: &Value) -> bool {
    match tool {
        "Bash" => input["command"].as_str().is_some_and(command_is_sensitive),
        // Glob 只回檔名，不回內容
        "Glob" => false,
        _ => ["file_path", "path", "notebook_path"]
            .iter()
            .filter_map(|k| input[*k].as_str())
            .any(is_sensitive_path),
    }
}

fn command_is_sensitive(command: &str) -> bool {
    shell_segments(command).iter().any(|seg| {
        // 去掉 sudo 前綴
        let words: Vec<&str> = seg
            .iter()
            .map(String::as_str)
            .skip_while(|w| *w == "sudo")
            .collect();
        match words.as_slice() {
            ["printenv", ..] | ["env"] | ["export", "-p"] => true,
            _ => words.iter().skip(1).any(|w| is_sensitive_path(w)),
        }
    })
}

/// 遠端連線送出前剝除：所有 `sensitive` 的 `tool.result` 把 `output` 換成 `""`、加 `redacted: true`。
/// frame、snapshot、載入更多的形狀不同，所以整個走一遍。
pub fn redact_remote(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("tool.result")
                && let Some(Value::Object(p)) = map.get_mut("payload")
                && p.get("sensitive").and_then(Value::as_bool) == Some(true)
            {
                p.insert("output".into(), Value::String(String::new()));
                p.insert("redacted".into(), Value::Bool(true));
                return;
            }
            for child in map.values_mut() {
                redact_remote(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                redact_remote(child);
            }
        }
        _ => {}
    }
}
