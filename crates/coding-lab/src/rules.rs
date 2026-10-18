//! 危險規則（#634，PLAN「危險指令」）：只標示、不攔截。
//!
//! 內建規則在 `rules/builtin.json`（編進二進位）；使用者在 `~/.coding-lab/rules.json` 用同樣格式覆寫：
//! 同 id 取代、新 id 放在最前面（使用者規則優先）、`disable` 停用。檔案壞掉或任何 pattern 編譯失敗，
//! 整份不用、退回內建並回傳警告讓呼叫端寫 log。
//!
//! 指令型工具（[`COMMAND_TOOLS`]：Bash、PowerShell）先依 sh 的規則分詞（[`shell_parse`]：引號、跳脫與續行、
//! heredoc 本文不當成指令），依 `&&`／`;`／`|`／換行切段，每段各取「第一條命中」的規則，整條指令取最嚴重的等級；
//! 所以 `rm -rf target && rm -rf /` 會被判 danger。`scope: command` 的規則（如 `curl … | sh`）比對依原本分隔符號
//! 接回來的整條指令，引號內的 `| ; &` 先中和（`echo 'curl x | sh'` 不算）。
//! 段落另外去掉 `sudo`、`env`、`timeout` 這類包裝與 `VAR=值` 前綴、指令只留檔名（`/bin/rm` → `rm`）再比一次；
//! `$( … )`、`( … )`、`bash -c`、`eval`、`find -exec` 裡的指令遞迴檢查。規則的 `unless` 也命中時不算（#745）。

use std::path::Path;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::derive::{ShellParse, normalize_command, shell_parse};
use crate::event::{COMMAND_TOOLS, COMMAND_TOOLS_ALIAS, Danger, DangerLevel, is_command_tool};
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
    /// 套用的工具；也接受單數 `tool`（使用者手寫時常見）。`@command` 代表所有指令型工具
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    tool: Option<String>,
    /// 比對 `tool_input` 的哪個欄位；預設指令型工具是 `command`、其他工具是 `file_path`
    field: Option<String>,
    #[serde(default)]
    scope: Scope,
    pattern: String,
    /// 這個也命中時不算（例如 `--dry-run`）：比對的文字和 `pattern` 相同（#745）
    #[serde(default)]
    unless: Option<String>,
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
    unless: Option<Regex>,
}

impl Rule {
    fn hits(&self, text: &str) -> bool {
        self.re.is_match(text) && !self.unless.as_ref().is_some_and(|u| u.is_match(text))
    }
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
            if let Some(i) = spec.tools.iter().position(|t| t == COMMAND_TOOLS_ALIAS) {
                spec.tools.remove(i);
                for t in COMMAND_TOOLS.iter().chain([&CMD]) {
                    if !spec.tools.iter().any(|x| x == t) {
                        spec.tools.push(t.to_string());
                    }
                }
            }
            // 指令型工具只比對指令（依段切開）；設成別的欄位以前會默默永不命中（#687 R17）
            if let Some(t) = spec.tools.iter().find(|t| is_shell(t))
                && spec.field.as_deref().is_some_and(|f| f != "command")
            {
                return Err(format!("規則 {} 套用在 {t}，field 只能是 command", spec.id));
            }
            let re = Regex::new(&spec.pattern).map_err(|e| format!("規則 {}：{e}", spec.id))?;
            let unless = spec
                .unless
                .as_deref()
                .map(Regex::new)
                .transpose()
                .map_err(|e| format!("規則 {} 的 unless：{e}", spec.id))?;
            Ok(Rule { spec, re, unless })
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

/// 包裝指令的選項裡，後面帶一個值的（`sudo -u root`、`timeout -s KILL`）。
fn takes_value(wrapper: &str, opt: &str) -> bool {
    if let Some(long) = opt.strip_prefix("--") {
        return !long.contains('=')
            && match wrapper {
                "sudo" | "doas" => matches!(
                    long,
                    "user" | "group" | "chdir" | "prompt" | "role" | "type" | "host" | "other-user"
                ),
                "env" => matches!(long, "unset" | "chdir" | "split-string"),
                "timeout" => matches!(long, "signal" | "kill-after"),
                _ => false,
            };
    }
    match wrapper {
        // 合併的短選項看最後一個字：`-iu root`
        "sudo" | "doas" => opt.ends_with(['u', 'g', 'C', 'p', 'h', 'r', 't', 'U', 'D', 'T']),
        "env" => matches!(opt, "-u" | "-C" | "-S"),
        "nice" => opt == "-n",
        "ionice" => matches!(opt, "-c" | "-n" | "-p" | "-P" | "-u"),
        "stdbuf" => matches!(opt, "-i" | "-o" | "-e"),
        "time" => matches!(opt, "-f" | "-o"),
        "timeout" => matches!(opt, "-s" | "-k"),
        "exec" => opt == "-a",
        "xargs" => matches!(opt, "-n" | "-I" | "-L" | "-P" | "-d" | "-s" | "-E" | "-a"),
        _ => false,
    }
}

/// 去掉 `sudo`、`doas`、`env`、`timeout`、`nice`、`ionice`、`stdbuf`、`time`、`nohup`、`exec`、`command`、`xargs`
/// 這類包裝（連同它們的選項；`timeout` 還有時間長度）與 `VAR=值` 前綴；指令只留檔名（`/bin/rm` → `rm`，C25、A-N4）。
fn strip_wrappers(tokens: &[String]) -> Vec<String> {
    let mut i = 0;
    while let Some(t) = tokens.get(i) {
        let wrapper = t.as_str();
        match wrapper {
            "sudo" | "doas" | "env" | "timeout" | "nice" | "ionice" | "stdbuf" | "time"
            | "nohup" | "exec" | "command" | "xargs" => {
                i += 1;
                while let Some(o) = tokens.get(i) {
                    if o == "--" {
                        i += 1;
                        break;
                    }
                    // `env -` 等於 `env -i`
                    if !o.starts_with('-') || (o == "-" && wrapper != "env") {
                        break;
                    }
                    i += if takes_value(wrapper, o) { 2 } else { 1 };
                }
                if wrapper == "timeout" {
                    i += 1; // 時間長度
                }
            }
            t if t.contains('=') && !t.starts_with('-') && !t.starts_with('=') => i += 1,
            _ => break,
        }
    }
    let mut out = tokens[i.min(tokens.len())..].to_vec();
    if let Some(first) = out.first_mut()
        && let Some(name) = first.rsplit('/').next().filter(|n| !n.is_empty())
    {
        *first = name.to_string();
    }
    out
}

/// 一段指令裡要再檢查的指令：`bash -c '…'`、`cmd /c …`、`powershell -Command …`、`eval …`、`find -exec … ;`。
fn inner_commands(tool: &str, words: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some((t, inner)) = shell_c_argument(words) {
        out.push((t.to_string(), inner));
    }
    match words.first().map(String::as_str) {
        Some("eval") => out.push((tool.to_string(), words[1..].join(" "))),
        Some("find") => {
            let mut rest = words.iter();
            while rest
                .by_ref()
                .any(|w| matches!(w.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
            {
                let cmd: Vec<&str> = rest
                    .by_ref()
                    .take_while(|w| *w != ";" && *w != "+")
                    .map(String::as_str)
                    .collect();
                out.push((tool.to_string(), cmd.join(" ")));
            }
        }
        _ => {}
    }
    out
}

/// `scope: command` 比對的文字：依原本的分隔符號接回來的整條指令；引號與 heredoc 裡的 `| ; &` 換成空白，
/// 字串裡的「管線」不會被當成真的管線。
fn command_view(parsed: &ShellParse) -> String {
    let mut out = String::new();
    for seg in &parsed.segments {
        for (k, w) in seg.words.iter().enumerate() {
            if k > 0 {
                out.push(' ');
            }
            if w.quoted {
                out.extend(
                    w.text
                        .chars()
                        .map(|c| if matches!(c, '|' | ';' | '&') { ' ' } else { c }),
                );
            } else {
                out.push_str(&w.text);
            }
        }
        out.push_str(if seg.piped { " | " } else { " ; " });
    }
    out
}

impl RuleSet {
    fn first_match(&self, tool: &str, field: &str, text: &str, scope: Scope) -> Option<Danger> {
        self.rules
            .iter()
            .filter(|r| r.spec.scope == scope && r.spec.tools.iter().any(|t| t == tool || t == "*"))
            .filter(|r| {
                r.spec.field.as_deref().unwrap_or(if is_shell(tool) {
                    "command"
                } else {
                    "file_path"
                }) == field
            })
            .find(|r| r.hits(text))
            .map(|r| Danger {
                rule: r.spec.id.clone(),
                level: r.spec.level,
                why: r.spec.why.clone(),
            })
    }
}

/// 子殼、命令替換、`bash -c` 最多往裡看幾層。
const MAX_NESTING: usize = 4;

/// `bash -c '…'`、`sh -lc "…"` 的指令字串（`tokens` 已去掉引號與 sudo 等前綴）。
fn shell_c_argument(tokens: &[String]) -> Option<(&'static str, String)> {
    let shell = tokens.first()?.rsplit('/').next()?.to_lowercase();
    let rest = &tokens[1..];
    match shell.trim_end_matches(".exe") {
        "bash" | "sh" | "zsh" | "dash" | "ksh" => {
            let flag = rest
                .iter()
                .position(|t| t.starts_with('-') && !t.starts_with("--") && t.contains('c'))?;
            Some(("Bash", rest.get(flag + 1)?.clone()))
        }
        // `/c`、`/k` 之後整串是指令；Git Bash 裡要寫成 `//c`
        "cmd" => {
            let flag = rest
                .iter()
                .position(|t| matches!(t.to_lowercase().as_str(), "/c" | "/k" | "//c" | "//k"))?;
            Some((CMD, rest[flag + 1..].join(" ")))
        }
        // `-c`、`-Command` 與它的縮寫，之後整串是指令
        "powershell" | "pwsh" => {
            let flag = rest.iter().position(|t| {
                let t = t.to_lowercase();
                t.len() >= 2 && "-command".starts_with(&t)
            })?;
            Some(("PowerShell", rest[flag + 1..].join(" ")))
        }
        _ => None,
    }
}

/// cmd 不是 Claude Code 的工具：只在 `cmd /c …` 裡比對（D12）；規則的 `tools` 可以寫 `"cmd"`，
/// `@command` 也包含它。
const CMD: &str = "cmd";

/// 依段比對 `command` 的工具：指令型工具，加上 `cmd /c` 裡的指令。
fn is_shell(tool: &str) -> bool {
    is_command_tool(tool) || tool == CMD
}

impl RuleSet {
    fn check_command(&self, tool: &str, command: &str, depth: usize) -> Option<Danger> {
        let parsed = shell_parse(command);
        let mut worst = self.first_match(tool, "command", &command_view(&parsed), Scope::Command);
        for seg in &parsed.segments {
            let words: Vec<String> = seg.words.iter().map(|w| w.text.clone()).collect();
            let raw = words.join(" ");
            worst = more_severe(
                worst,
                self.first_match(tool, "command", &raw, Scope::Segment),
            );
            let stripped = strip_wrappers(&words);
            let joined = stripped.join(" ");
            if joined != raw {
                worst = more_severe(
                    worst,
                    self.first_match(tool, "command", &joined, Scope::Segment),
                );
            }
            if depth < MAX_NESTING {
                for (inner_tool, inner) in inner_commands(tool, &stripped) {
                    let inner = normalize_command(&inner_tool, &inner);
                    worst = more_severe(worst, self.check_command(&inner_tool, &inner, depth + 1));
                }
            }
        }
        // 只有最外層的 `$( … )`、`( … )`；更深的在遞迴裡分詞時才拿到（E-B5）
        if depth < MAX_NESTING {
            for inner in &parsed.nested {
                worst = more_severe(worst, self.check_command(tool, inner, depth + 1));
            }
        }
        worst
    }
}

impl DangerRules for RuleSet {
    /// 不是指令型的工具：每條規則比對它自己的 `field`（預設 `file_path`，例如 Write 的 `content`），
    /// 依規則順序取第一個命中的（使用者規則在前）。
    fn check(&self, tool: &str, input: &Value) -> Option<Danger> {
        if is_command_tool(tool) {
            let command = normalize_command(tool, input["command"].as_str()?);
            return self.check_command(tool, &command, 0);
        }
        self.rules
            .iter()
            .filter(|r| r.spec.scope == Scope::Segment)
            .filter(|r| r.spec.tools.iter().any(|t| t == tool || t == "*"))
            .find(|r| {
                let field = r.spec.field.as_deref().unwrap_or("file_path");
                input[field].as_str().is_some_and(|text| {
                    if field == "file_path" {
                        r.hits(&text.replace('\\', "/"))
                    } else {
                        r.hits(text)
                    }
                })
            })
            .map(|r| Danger {
                rule: r.spec.id.clone(),
                level: r.spec.level,
                why: r.spec.why.clone(),
            })
    }
}
