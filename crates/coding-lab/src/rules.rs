//! 危險規則（#634，PLAN「危險指令」）：只標示、不攔截。
//!
//! 內建規則在 `rules/builtin.json`（編進二進位）；使用者在 `~/.coding-lab/rules.json` 用同樣格式覆寫：
//! 同 id 取代、新 id 放在最前面（使用者規則優先）、`disable` 停用。檔案壞掉或任何 pattern 編譯失敗，
//! 整份不用、退回內建並回傳警告讓呼叫端寫 log。
//!
//! Bash 依 `&&`／`;`／`|`／換行切段（引號內不切），每段各取「第一條命中」的規則，整條指令取最嚴重的等級；
//! 所以 `rm -rf target && rm -rf /` 會被判 danger。`scope: command` 的規則（如 `curl … | sh`）比對整條指令。
//! 段落另外去掉 `sudo`、`env`、`VAR=值` 等前綴再比一次，`sudo rm -rf x` 兩種都看得到。

use std::path::Path;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::derive::shell_segments;
use crate::event::{Danger, DangerLevel};
use crate::merge::DangerRules;

const BUILTIN: &str = include_str!("rules/builtin.json");

#[derive(Deserialize)]
struct RuleFile {
    #[serde(default)]
    rules: Vec<RuleSpec>,
    #[serde(default)]
    disable: Vec<String>,
}

#[derive(Clone, Deserialize)]
struct RuleSpec {
    id: String,
    /// 套用的工具；也接受單數 `tool`（使用者手寫時常見）
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    tool: Option<String>,
    /// 比對 `tool_input` 的哪個欄位；預設 Bash 是 `command`、其他工具是 `file_path`
    field: Option<String>,
    #[serde(default)]
    scope: Scope,
    pattern: String,
    level: DangerLevel,
    why: String,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Scope {
    #[default]
    Segment,
    Command,
}

struct Rule {
    spec: RuleSpec,
    re: Regex,
}

pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    pub fn builtin() -> RuleSet {
        // 內建規則有測試保證能解析與編譯
        compile(parse(BUILTIN).map(|f| f.rules).unwrap_or_default())
            .unwrap_or(RuleSet { rules: Vec::new() })
    }

    /// 讀使用者規則；檔案不存在就只用內建。回傳的字串是要寫進 log 的警告。
    pub fn load(path: &Path) -> (RuleSet, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (RuleSet::builtin(), None);
            }
            Err(e) => {
                return (
                    RuleSet::builtin(),
                    Some(format!("讀不到 {}：{e}，改用內建規則", path.display())),
                );
            }
        };
        let merged = parse(&text).and_then(|user| {
            let mut builtin = parse(BUILTIN)?.rules;
            builtin.retain(|r| !user.disable.contains(&r.id));
            let mut fresh = Vec::new();
            for u in user.rules {
                match builtin.iter_mut().find(|b| b.id == u.id) {
                    Some(b) => *b = u,
                    None => fresh.push(u),
                }
            }
            fresh.extend(builtin);
            compile(fresh)
        });
        match merged {
            Ok(set) => (set, None),
            Err(e) => (
                RuleSet::builtin(),
                Some(format!("{} 有錯（{e}），改用內建規則", path.display())),
            ),
        }
    }

    pub fn ids(&self) -> Vec<String> {
        self.rules.iter().map(|r| r.spec.id.clone()).collect()
    }
}

fn parse(text: &str) -> Result<RuleFile, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

fn compile(specs: Vec<RuleSpec>) -> Result<RuleSet, String> {
    let rules = specs
        .into_iter()
        .map(|mut spec| {
            if let Some(t) = spec.tool.take() {
                spec.tools.push(t);
            }
            if spec.tools.is_empty() {
                return Err(format!("規則 {} 沒有指定 tools", spec.id));
            }
            let re = Regex::new(&spec.pattern).map_err(|e| format!("規則 {}：{e}", spec.id))?;
            Ok(Rule { spec, re })
        })
        .collect::<Result<_, String>>()?;
    Ok(RuleSet { rules })
}

fn more_severe(a: Option<Danger>, b: Option<Danger>) -> Option<Danger> {
    match (&a, &b) {
        (Some(x), Some(y)) if x.level == DangerLevel::Warn && y.level == DangerLevel::Danger => b,
        (None, _) => b,
        _ => a,
    }
}

/// 去掉 `sudo [-選項]`、`env`、`VAR=值`、`nohup`、`time`、`exec` 這類前綴。
fn strip_wrappers(tokens: &[String]) -> &[String] {
    let mut i = 0;
    while let Some(t) = tokens.get(i) {
        match t.as_str() {
            "sudo" => {
                i += 1;
                while tokens.get(i).is_some_and(|t| t.starts_with('-')) {
                    // -u／-g 後面帶一個值
                    let takes_value = matches!(tokens[i].as_str(), "-u" | "-g" | "-C");
                    i += if takes_value { 2 } else { 1 };
                }
            }
            "env" | "nohup" | "time" | "exec" | "command" => i += 1,
            "xargs" => {
                i += 1;
                while tokens.get(i).is_some_and(|t| t.starts_with('-')) {
                    let takes_value = matches!(
                        tokens[i].as_str(),
                        "-n" | "-I" | "-L" | "-P" | "-d" | "-s" | "-E" | "-a"
                    );
                    i += if takes_value { 2 } else { 1 };
                }
            }
            t if t.contains('=') && !t.starts_with('-') && !t.starts_with('=') => i += 1,
            _ => break,
        }
    }
    &tokens[i.min(tokens.len())..]
}

impl RuleSet {
    fn first_match(&self, tool: &str, field: &str, text: &str, scope: Scope) -> Option<Danger> {
        self.rules
            .iter()
            .filter(|r| r.spec.scope == scope && r.spec.tools.iter().any(|t| t == tool || t == "*"))
            .filter(|r| {
                r.spec.field.as_deref().unwrap_or(if tool == "Bash" {
                    "command"
                } else {
                    "file_path"
                }) == field
            })
            .find(|r| r.re.is_match(text))
            .map(|r| Danger {
                rule: r.spec.id.clone(),
                level: r.spec.level,
                why: r.spec.why.clone(),
            })
    }
}

/// 子殼、命令替換、`bash -c` 最多往裡看幾層。
const MAX_NESTING: usize = 4;

/// `( … )`、`$( … )`、`` ` … ` `` 裡的指令（單引號內的不算）。
fn nested_commands(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let (mut single, mut double) = (false, false);
    let mut opens: Vec<usize> = Vec::new();
    let mut tick: Option<usize> = None;
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if single {
            single = c != '\'';
        } else {
            match c {
                '\\' => i += 1,
                '\'' if !double => single = true,
                '"' => double = !double,
                '`' => match tick.take() {
                    Some(start) => out.push(chars[start..i].iter().collect()),
                    None => tick = Some(i + 1),
                },
                '(' if !double || (i > 0 && chars[i - 1] == '$') => opens.push(i + 1),
                ')' => {
                    if let Some(start) = opens.pop() {
                        out.push(chars[start..i].iter().collect());
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    out
}

/// `bash -c '…'`、`sh -lc "…"` 的指令字串（`tokens` 已去掉引號與 sudo 等前綴）。
fn shell_c_argument(tokens: &[String]) -> Option<&str> {
    let shell = tokens.first()?.rsplit('/').next()?;
    if !matches!(shell, "bash" | "sh" | "zsh" | "dash" | "ksh") {
        return None;
    }
    let flag = tokens[1..]
        .iter()
        .position(|t| t.starts_with('-') && !t.starts_with("--") && t.contains('c'))?;
    tokens.get(flag + 2).map(String::as_str)
}

impl RuleSet {
    fn check_command(&self, tool: &str, command: &str, depth: usize) -> Option<Danger> {
        let mut worst = self.first_match(tool, "command", command, Scope::Command);
        for seg in shell_segments(command) {
            let raw = seg.join(" ");
            let stripped = strip_wrappers(&seg);
            worst = more_severe(
                worst,
                self.first_match(tool, "command", &raw, Scope::Segment),
            );
            let joined = stripped.join(" ");
            if joined != raw {
                worst = more_severe(
                    worst,
                    self.first_match(tool, "command", &joined, Scope::Segment),
                );
            }
            if depth < MAX_NESTING
                && let Some(inner) = shell_c_argument(stripped)
            {
                worst = more_severe(worst, self.check_command(tool, inner, depth + 1));
            }
        }
        if depth < MAX_NESTING {
            for inner in nested_commands(command) {
                worst = more_severe(worst, self.check_command(tool, &inner, depth + 1));
            }
        }
        worst
    }
}

impl DangerRules for RuleSet {
    fn check(&self, tool: &str, input: &Value) -> Option<Danger> {
        if tool != "Bash" {
            let path = input["file_path"].as_str()?;
            return self.first_match(tool, "file_path", &path.replace('\\', "/"), Scope::Segment);
        }
        self.check_command(tool, input["command"].as_str()?, 0)
    }
}
