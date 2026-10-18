//! 衍生事件用的純函式（#633）：Edit hunk、git 指令與輸出解析、相對路徑、Pre 時的檔案探測。

use serde_json::Value;

use crate::event::{GitOp, Hunk};
use crate::hook::{FileProbe, HookInput};

/// 超過這個大小的檔案不在 PreToolUse 讀（PLAN「Edit 的行號」）。
pub const PROBE_MAX_BYTES: u64 = 1024 * 1024;
const CONTEXT: usize = 3;

/// 依 Edit 的 `old_string` → `new_string` 算出 unified diff 的 hunk（上下文 3 行，與 `git diff -U3` 相同）。
/// 多處相同時取第一處；找不到或 `old_string` 為空回 None。
pub fn edit_hunk(text: &str, old: &str, new: &str) -> Option<Hunk> {
    if old.is_empty() {
        return None;
    }
    let pos = text.find(old)?;
    let end = pos + old.len();
    let new_text = format!("{}{}{}", &text[..pos], new, &text[end..]);

    // 以行為單位：受影響的舊行從 pos 所在行到 end-1 所在行
    let old_lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut offsets = Vec::with_capacity(old_lines.len());
    let mut acc = 0;
    for l in &old_lines {
        offsets.push(acc);
        acc += l.len();
    }
    let line_of = |byte: usize| offsets.partition_point(|&o| o <= byte).saturating_sub(1);
    let first = line_of(pos);
    let mut last = line_of(end - 1);
    let block_start = offsets[first];

    // 後面沒變的部分；若新內容不是以換行結尾，下一行會被併進同一行，一起算進受影響範圍
    let suffix_at = |last: usize| offsets.get(last + 1).copied().unwrap_or(text.len());
    let mut suffix = &text[suffix_at(last)..];
    let mut new_block = &new_text[block_start..new_text.len() - suffix.len()];
    while !new_block.is_empty() && !new_block.ends_with('\n') && !suffix.is_empty() {
        last += 1;
        suffix = &text[suffix_at(last)..];
        new_block = &new_text[block_start..new_text.len() - suffix.len()];
    }

    let strip = |l: &str| l.strip_suffix('\n').unwrap_or(l).to_string();
    let before_from = first.saturating_sub(CONTEXT);
    let before: Vec<String> = old_lines[before_from..first]
        .iter()
        .map(|l| strip(l))
        .collect();
    let removed: Vec<String> = old_lines[first..=last].iter().map(|l| strip(l)).collect();
    let added: Vec<String> = new_block.split_inclusive('\n').map(strip).collect();
    let after: Vec<String> = old_lines[last + 1..]
        .iter()
        .take(CONTEXT)
        .map(|l| strip(l))
        .collect();

    let mut lines = Vec::new();
    lines.extend(before.iter().map(|l| format!(" {l}")));
    lines.extend(removed.iter().map(|l| format!("-{l}")));
    lines.extend(added.iter().map(|l| format!("+{l}")));
    lines.extend(after.iter().map(|l| format!(" {l}")));
    Some(Hunk {
        start: before_from as u32 + 1,
        old_lines: (before.len() + removed.len() + after.len()) as u32,
        new_lines: (before.len() + added.len() + after.len()) as u32,
        lines,
    })
}

/// 會改變 repo 狀態、值得記下來的 git 子命令（status、diff、log、add 等只讀或暫存的不算）。
const GIT_OPS: &[&str] = &[
    "commit",
    "push",
    "pull",
    "reset",
    "stash",
    "branch",
    "worktree",
    "checkout",
    "switch",
    "merge",
    "rebase",
    "tag",
    "cherry-pick",
    "revert",
    "restore",
    "clean",
];

/// 從 Bash 指令（與 PostToolUse 的 `tool_response`）判斷 git 操作。一串指令裡取最後一個有意義的 git 子命令。
/// 優先用 Claude Code 給的結構化 `gitOperation`，沒有才解析 stdout 的 `[branch hash] message`。
pub fn git_op_from(command: &str, response: Option<&Value>, ok: bool) -> Option<GitOp> {
    let (sub, args) = shell_segments(command)
        .into_iter()
        .filter_map(|seg| git_subcommand(&seg))
        .rfind(|(sub, _)| GIT_OPS.contains(&sub.as_str()))?;

    let mut branch = None;
    let mut hash = None;
    if let Some(op) = response.and_then(|r| r["gitOperation"].as_object())
        && let Some(detail) = op.get(sub.as_str()).or_else(|| op.values().next())
    {
        branch = detail["branch"].as_str().map(str::to_string);
        hash = detail["sha"]
            .as_str()
            .or(detail["hash"].as_str())
            .map(str::to_string);
    }
    let stdout = response
        .and_then(|r| r["stdout"].as_str())
        .unwrap_or_default();
    let mut message = None;
    if let Some((b, h, m)) = stdout.lines().find_map(parse_commit_line) {
        branch = branch.or(Some(b));
        hash = hash.or(Some(h));
        message = Some(m);
    }
    // 建分支的子命令：分支名稱在 -b／-c 之後
    if branch.is_none() && matches!(sub.as_str(), "worktree" | "checkout" | "switch" | "branch") {
        branch = args
            .windows(2)
            .find(|w| matches!(w[0].as_str(), "-b" | "-B" | "-c" | "-C"))
            .map(|w| w[1].clone());
    }
    Some(GitOp {
        tool_use_id: String::new(),
        op: sub,
        branch,
        hash,
        message,
        ok,
    })
}

/// `[main 4cf93bc] 訊息` → (branch, hash, message)
fn parse_commit_line(line: &str) -> Option<(String, String, String)> {
    let rest = line.strip_prefix('[')?;
    let (head, msg) = rest.split_once("] ")?;
    let (branch, hash) = head.rsplit_once(' ')?;
    let looks_like_hash = hash.len() >= 7 && hash.chars().all(|c| c.is_ascii_hexdigit());
    looks_like_hash.then(|| (branch.to_string(), hash.to_string(), msg.to_string()))
}

/// 一段指令的第一個詞是 git：略過全域選項（`-C <dir>`、`-c k=v`），回傳子命令與其餘參數。
fn git_subcommand(seg: &[String]) -> Option<(String, Vec<String>)> {
    let mut it = seg
        .iter()
        .skip_while(|t| t.contains('=') && !t.starts_with('-'));
    let first = it.next()?;
    if first != "git" && !first.ends_with("/git") {
        return None;
    }
    let mut rest: Vec<String> = it.cloned().collect();
    while let Some(t) = rest.first() {
        if t == "-C" || t == "-c" {
            rest.drain(..2.min(rest.len()));
        } else if t.starts_with('-') {
            rest.remove(0);
        } else {
            break;
        }
    }
    if rest.is_empty() {
        return None;
    }
    let sub = rest.remove(0);
    Some((sub, rest))
}

/// 指令型工具的指令在切段前先正規化：PowerShell 的跳脫字元是反引號（`` `x `` 就是 `x`，行尾的
/// `` ` `` 是續行），路徑用反斜線；切段用的是 sh 的規則，所以先去掉反引號、反斜線換成 `/`。Bash 原樣。
pub fn normalize_command<'a>(tool: &str, command: &'a str) -> std::borrow::Cow<'a, str> {
    if tool != "PowerShell" {
        return std::borrow::Cow::Borrowed(command);
    }
    let mut out = String::with_capacity(command.len());
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => match chars.next() {
                Some('\r') => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    out.push(' ');
                }
                Some('\n') => out.push(' '),
                Some(n) => out.push(n),
                None => {}
            },
            '\\' => out.push('/'),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// 把指令依 `&&`、`||`、`;`、`|`、換行切段，引號內的內容不切（所以 `echo 'git commit'` 不算 git）。
pub fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = vec![Vec::new()];
    let mut token = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize; // $( … ) 的層數
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let flush = |token: &mut String, segments: &mut Vec<Vec<String>>| {
        if !token.is_empty()
            && let Some(seg) = segments.last_mut()
        {
            seg.push(std::mem::take(token));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q && depth == 0 {
                    quote = None;
                } else {
                    if c == '(' && i > 0 && chars[i - 1] == '$' {
                        depth += 1;
                    } else if c == ')' && depth > 0 {
                        depth -= 1;
                    }
                    token.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                ' ' | '\t' => flush(&mut token, &mut segments),
                '\n' | ';' | '|' | '&' => {
                    flush(&mut token, &mut segments);
                    if segments.last().is_some_and(|s| !s.is_empty()) {
                        segments.push(Vec::new());
                    }
                }
                _ => token.push(c),
            },
        }
        i += 1;
    }
    flush(&mut token, &mut segments);
    segments.into_iter().filter(|s| !s.is_empty()).collect()
}

/// 路徑相對 `cwd`、一律 `/` 分隔；不在 `cwd` 底下就回傳正規化後的完整路徑。
pub fn relative_path(cwd: &str, path: &str) -> String {
    let norm = |s: &str| s.replace('\\', "/");
    let cwd = norm(cwd);
    let cwd = cwd.trim_end_matches('/');
    let path = norm(path);
    match path.strip_prefix(cwd).and_then(|r| r.strip_prefix('/')) {
        Some(rel) if !cwd.is_empty() => rel.to_string(),
        _ => path,
    }
}

/// 這個 hook 要不要讀檔探測（PreToolUse 的 Edit／Write）。
pub fn needs_probe(h: &HookInput) -> bool {
    h.event_name() == "PreToolUse"
        && matches!(
            h.str_field("tool_name").unwrap_or_default(),
            "Edit" | "Write"
        )
}

/// PreToolUse 的 Edit／Write：**回應 hook 之前**讀檔（#626：Claude Code 等 hook 回應才執行工具）。
/// 同步 I/O，async 的呼叫端要放進 `spawn_blocking`。
pub fn attach_probe(h: &mut HookInput) {
    if !needs_probe(h) {
        return;
    }
    let tool = h.str_field("tool_name").unwrap_or_default();
    let Some(input) = h.field("tool_input") else {
        return;
    };
    let Some(path) = input["file_path"].as_str() else {
        return;
    };
    let meta = std::fs::metadata(path).ok();
    let mut probe = FileProbe {
        exists: meta.is_some(),
        hunk: None,
    };
    // 只讀一般檔案：FIFO、裝置檔讀了可能不會回來
    if tool == "Edit"
        && meta.is_some_and(|m| m.is_file() && m.len() <= PROBE_MAX_BYTES)
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let old = input["old_string"].as_str().unwrap_or_default();
        let new = input["new_string"].as_str().unwrap_or_default();
        probe.hunk = edit_hunk(&text, old, new);
    }
    h.set_probe(probe);
}
