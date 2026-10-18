//! 敏感輸出（#654）：讀 `.env`、金鑰、憑證設定這類工具的輸出標 `sensitive`，WS／API 對遠端連線一律剝除
//! （不變式 6、contracts §4）。只影響送出的副本，本機 JSONL 照存（截 8 KB）。
//!
//! 判斷只看路徑與指令，不看輸出內容：檔案工具看 `file_path`／`path`／`notebook_path`；指令型工具（Bash、PowerShell）依 `&&`／`|` 等切段，
//! 任何一段的參數是敏感路徑、或是 `printenv`／單獨的 `env`／`export -p`（把整個環境印出來）就算。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::derive::{normalize_command, shell_segments};
use crate::event::is_command_tool;

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

/// 一份清單合成一條 alternation（E-B6）：每個參數只比一次，不是每條各比一次。
fn compiled(
    list: &'static [&'static str],
    cell: &'static OnceLock<Option<Regex>>,
) -> Option<&'static Regex> {
    cell.get_or_init(|| {
        let alt: Vec<String> = list.iter().map(|p| format!("(?:{p})")).collect();
        Regex::new(&alt.join("|")).ok()
    })
    .as_ref()
}

/// 這個路徑的內容可能是機密。
pub fn is_sensitive_path(path: &str) -> bool {
    static NAMES: OnceLock<Option<Regex>> = OnceLock::new();
    static PATHS: OnceLock<Option<Regex>> = OnceLock::new();
    // 大多數參數沒有反斜線：不必配置新字串
    let path: std::borrow::Cow<str> = if path.contains('\\') {
        path.replace('\\', "/").into()
    } else {
        path.into()
    };
    let path = path.trim_matches(|c| c == '"' || c == '\'');
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.starts_with(".env") && ENV_TEMPLATES.iter().any(|t| name.ends_with(t)) {
        return false;
    }
    compiled(NAME_PATTERNS, &NAMES).is_some_and(|r| r.is_match(name))
        || compiled(PATH_PATTERNS, &PATHS).is_some_and(|r| r.is_match(path))
}

/// 這個工具呼叫的輸出會不會含機密。
pub fn is_sensitive(tool: &str, input: &Value) -> bool {
    match tool {
        t if is_command_tool(t) => input["command"]
            .as_str()
            .is_some_and(|c| command_is_sensitive(t, &normalize_command(t, c))),
        // Glob 只回檔名，不回內容
        "Glob" => false,
        _ => ["file_path", "path", "notebook_path"]
            .iter()
            .filter_map(|k| input[*k].as_str())
            .any(is_sensitive_path),
    }
}

/// PowerShell 讀環境變數磁碟機（`Get-ChildItem env:`、`Get-Content env:X`）的指令
const PS_ENV_READERS: &[&str] = &[
    "get-childitem",
    "gci",
    "dir",
    "ls",
    "get-item",
    "gi",
    "get-content",
    "gc",
    "cat",
    "type",
];

fn command_is_sensitive(tool: &str, command: &str) -> bool {
    if tool == "PowerShell" && powershell_dumps_env(command) {
        return true;
    }
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

/// PowerShell 把環境變數印出來：列舉或讀 `env:` 磁碟機、單獨一個 `$env:X` 運算式（印出值）、
/// `[Environment]::GetEnvironmentVariables()`。
fn powershell_dumps_env(command: &str) -> bool {
    let lower = command.to_lowercase();
    if lower.contains("environment]::getenvironmentvariables") {
        return true;
    }
    shell_segments(&lower)
        .iter()
        .any(|seg| match seg.as_slice() {
            [only] => only.starts_with("$env:"),
            [first, rest @ ..] => {
                PS_ENV_READERS.contains(&first.as_str())
                    && rest.iter().any(|w| w.starts_with("env:"))
            }
            [] => false,
        })
}

/// 這個系統自己發的金鑰（#666）：`clk_<8 碼 base62>_<43 碼 base62>`。
fn key_pattern() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"clk_([0-9A-Za-z]{8})_[0-9A-Za-z]{20,}")
            .unwrap_or_else(|e| panic!("金鑰正則有誤：{e}"))
    })
}

/// 遠端連線送出前（#720）：所有字串欄位裡的 `clk_<id>_<secret>` 只留 `clk_<id>_…`。
/// 金鑰是 collector 自己發的、形狀固定，所以精準比對不會誤殺；本機副本不剝（JSONL 本來就有）。
pub fn redact_keys(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.contains("clk_") {
                let re = key_pattern();
                if re.is_match(s) {
                    *s = re.replace_all(s, "clk_${1}_…").into_owned();
                }
            }
        }
        Value::Object(map) => map.values_mut().for_each(redact_keys),
        Value::Array(items) => items.iter_mut().for_each(redact_keys),
        _ => {}
    }
}
