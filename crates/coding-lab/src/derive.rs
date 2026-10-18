//! 衍生事件用的純函式（#633）：Edit hunk、git 指令與輸出解析、相對路徑、Pre 時的檔案探測。

use serde_json::Value;

use crate::event::{GitOp, Hunk};
use crate::hook::{FileProbe, HookInput};

/// 超過這個大小的檔案不在 PreToolUse 讀（PLAN「Edit 的行號」）。
pub const PROBE_MAX_BYTES: u64 = 1024 * 1024;
const CONTEXT: usize = 3;

/// 依 Edit 的 `old_string` → `new_string` 算出 unified diff 的 hunk（上下文 3 行，與 `git diff -U3` 相同）。
/// 多處相同時取第一處；找不到或 `old_string` 為空回 None。
/// 含 `\r\n` 時比對與 hunk 一律以 `\n` 計（W-M1：Windows 的 autocrlf 讓檔案是 CRLF、`old_string` 是 LF）；
/// 行數不變，行號照舊。
pub fn edit_hunk(text: &str, old: &str, new: &str) -> Option<Hunk> {
    if [text, old, new].iter().any(|s| s.contains("\r\n")) {
        let lf = |s: &str| s.replace("\r\n", "\n");
        return edit_hunk(&lf(text), &lf(old), &lf(new));
    }
    if old.is_empty() {
        return None;
    }
    let pos = text.find(old)?;
    let end = pos + old.len();

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

    // 後面沒變的部分從 `suffix_at(last)` 開始；若新內容不是以換行結尾，下一行會被併進同一行，一起算進受影響範圍。
    // 只組受影響的這一段，不組出整個新檔（E-M12）
    let suffix_at = |last: usize| offsets.get(last + 1).copied().unwrap_or(text.len());
    let block = |to: usize| format!("{}{}{}", &text[block_start..pos], new, &text[end..to]);
    let mut new_block = block(suffix_at(last));
    while !new_block.is_empty() && !new_block.ends_with('\n') && suffix_at(last) < text.len() {
        last += 1;
        new_block = block(suffix_at(last));
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
    // 絕大多數指令跟 git 無關：不必斷詞（E-M8）
    if !command.contains("git") {
        return None;
    }
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
            .or_else(|| detail["hash"].as_str())
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

/// 分詞後的一個詞。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWord {
    /// 去掉引號與跳脫後的文字。命令替換只留 `$(第一個詞…)`（內容另外放在 [`ShellParse::nested`]）；
    /// heredoc 本文的換行接成空白
    pub text: String,
    /// 有任何部分來自引號或 heredoc 本文：是字面的文字，不是 shell 語法
    pub quoted: bool,
}

/// 一段指令（`&&`、`||`、`;`、`|`、換行之間）。
#[derive(Clone, Debug, Default)]
pub struct ShellSegment {
    pub words: Vec<ShellWord>,
    /// 這段後面接的是管線 `|`
    pub piped: bool,
}

#[derive(Debug, Default)]
pub struct ShellParse {
    pub segments: Vec<ShellSegment>,
    /// 最外層的 `$( … )`、`` ` … ` ``、`( … )` 裡的指令；更深的由呼叫端對它們再分詞（E-B5）
    pub nested: Vec<String>,
}

/// 依 sh 的規則分詞（#745 K-9）：引號、反斜線跳脫與續行、雙引號內的跳脫、`#` 註解、重導向（自己一個詞，
/// `2>&1` 的 `&` 不切段）、heredoc（本文接在 `<<` 之後當成一個詞，不當成指令）、命令替換。
pub fn shell_parse(command: &str) -> ShellParse {
    let mut l = Lexer {
        c: command.chars().collect(),
        i: 0,
        out: ShellParse {
            segments: vec![ShellSegment::default()],
            nested: Vec::new(),
        },
        word: String::new(),
        in_word: false,
        quoted: false,
        heredocs: Vec::new(),
        parens: Vec::new(),
    };
    l.run();
    l.out.segments.retain(|s| !s.words.is_empty());
    l.out
}

/// 把指令切段、每段分詞（規則見 [`shell_parse`]），引號內的內容不切（所以 `echo 'git commit'` 不算 git）。
pub fn shell_segments(command: &str) -> Vec<Vec<String>> {
    shell_parse(command)
        .segments
        .into_iter()
        .map(|s| s.words.into_iter().map(|w| w.text).collect())
        .collect()
}

/// 還在等本文的 heredoc：本文放進第 `segment` 段的第 `word` 個詞。
struct Heredoc {
    delim: String,
    strip_tabs: bool,
    segment: usize,
    word: usize,
}

struct Lexer {
    c: Vec<char>,
    i: usize,
    out: ShellParse,
    word: String,
    in_word: bool,
    quoted: bool,
    heredocs: Vec<Heredoc>,
    /// 還沒關上的 `(` 的下一個位置
    parens: Vec<usize>,
}

impl Lexer {
    fn at(&self, i: usize) -> Option<char> {
        self.c.get(i).copied()
    }

    fn push(&mut self, ch: char) {
        self.word.push(ch);
        self.in_word = true;
    }

    fn flush(&mut self) {
        if self.in_word
            && let Some(seg) = self.out.segments.last_mut()
        {
            seg.words.push(ShellWord {
                text: std::mem::take(&mut self.word),
                quoted: self.quoted,
            });
        }
        self.in_word = false;
        self.quoted = false;
    }

    fn push_word(&mut self, text: &str, quoted: bool) {
        self.flush();
        self.word.push_str(text);
        self.in_word = true;
        self.quoted = quoted;
        self.flush();
    }

    fn end_segment(&mut self, piped: bool) {
        self.flush();
        if let Some(last) = self.out.segments.last_mut()
            && !last.words.is_empty()
        {
            last.piped = piped;
            self.out.segments.push(ShellSegment::default());
        }
    }

    fn run(&mut self) {
        while let Some(ch) = self.at(self.i) {
            match ch {
                ' ' | '\t' | '\r' => {
                    self.flush();
                    self.i += 1;
                }
                '\n' => {
                    self.end_segment(false);
                    self.i += 1;
                    self.read_heredocs();
                }
                '\\' => match self.at(self.i + 1) {
                    // 續行
                    Some('\n') => self.i += 2,
                    Some(n) => {
                        self.push(n);
                        self.i += 2;
                    }
                    None => {
                        self.push('\\');
                        self.i += 1;
                    }
                },
                '\'' => {
                    self.i += 1;
                    self.in_word = true;
                    self.quoted = true;
                    while let Some(n) = self.at(self.i) {
                        self.i += 1;
                        if n == '\'' {
                            break;
                        }
                        self.word.push(n);
                    }
                }
                '"' => {
                    self.i += 1;
                    self.in_word = true;
                    self.quoted = true;
                    self.double_quoted();
                }
                '`' => {
                    self.i += 1;
                    let inner = self.backtick();
                    self.substitution(inner, '`');
                }
                '$' if self.at(self.i + 1) == Some('(') => {
                    self.i += 2;
                    let inner = self.dollar_paren();
                    self.substitution(inner, '$');
                }
                '#' if !self.in_word => {
                    while self.at(self.i).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                ';' => {
                    self.end_segment(false);
                    self.i += 1;
                }
                '|' => {
                    if self.at(self.i + 1) == Some('|') {
                        self.end_segment(false);
                        self.i += 2;
                    } else {
                        self.end_segment(true);
                        self.i += 1;
                        // `|&`
                        if self.at(self.i) == Some('&') {
                            self.i += 1;
                        }
                    }
                }
                '&' if self.at(self.i + 1) == Some('>') => self.redirect(),
                '&' => {
                    self.end_segment(false);
                    self.i += 1;
                }
                '<' | '>' => self.redirect(),
                '(' => {
                    self.parens.push(self.i + 1);
                    self.push('(');
                    self.i += 1;
                }
                ')' => {
                    if let Some(start) = self.parens.pop()
                        && self.parens.is_empty()
                    {
                        self.out.nested.push(self.c[start..self.i].iter().collect());
                    }
                    self.push(')');
                    self.i += 1;
                }
                ch => {
                    self.push(ch);
                    self.i += 1;
                }
            }
        }
        self.end_segment(false);
    }

    /// 雙引號內：`\"`、`\\`、`\$`、`` \` `` 是跳脫，`\` 加換行是續行，其他的反斜線照留；命令替換照樣展開。
    fn double_quoted(&mut self) {
        while let Some(n) = self.at(self.i) {
            match n {
                '"' => {
                    self.i += 1;
                    return;
                }
                '\\' => match self.at(self.i + 1) {
                    Some('\n') => self.i += 2,
                    Some(e @ ('"' | '\\' | '$' | '`')) => {
                        self.word.push(e);
                        self.i += 2;
                    }
                    _ => {
                        self.word.push('\\');
                        self.i += 1;
                    }
                },
                '$' if self.at(self.i + 1) == Some('(') => {
                    self.i += 2;
                    let inner = self.dollar_paren();
                    self.substitution(inner, '$');
                }
                '`' => {
                    self.i += 1;
                    let inner = self.backtick();
                    self.substitution(inner, '`');
                }
                _ => {
                    self.word.push(n);
                    self.i += 1;
                }
            }
        }
    }

    /// `$(` 之後到對應的 `)`（只數括號）。
    fn dollar_paren(&mut self) -> String {
        let start = self.i;
        let mut depth = 1usize;
        while let Some(n) = self.at(self.i) {
            match n {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let inner = self.c[start..self.i].iter().collect();
                        self.i += 1;
                        return inner;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
        self.c[start..].iter().collect()
    }

    /// `` ` `` 之後到下一個沒跳脫的 `` ` ``。
    fn backtick(&mut self) -> String {
        let start = self.i;
        while let Some(n) = self.at(self.i) {
            match n {
                '\\' => self.i += 2,
                '`' => {
                    let inner = self.c[start..self.i].iter().collect();
                    self.i += 1;
                    return inner;
                }
                _ => self.i += 1,
            }
        }
        self.c[start.min(self.c.len())..].iter().collect()
    }

    /// 命令替換：詞裡只留 `$(第一個詞…)`，免得裡面的選項被當成外面指令的（`docker rm $(docker ps -f …)`）；
    /// 內容交給呼叫端遞迴。在 `( … )` 裡的不另外記，外層的內容已經含它。
    fn substitution(&mut self, inner: String, kind: char) {
        let first = inner.split_whitespace().next().unwrap_or_default();
        let shown = if kind == '$' {
            format!("$({first}…)")
        } else {
            format!("`{first}…`")
        };
        self.word.push_str(&shown);
        self.in_word = true;
        if self.parens.is_empty() {
            self.out.nested.push(inner);
        }
    }

    /// 重導向自己一個詞：`<`、`>`、`>>`、`2>` 的 `>`、`>&`、`&>`、`<<<`；`<<`、`<<-` 是 heredoc。
    fn redirect(&mut self) {
        self.flush();
        let mut op = String::new();
        if self.at(self.i) == Some('&') {
            op.push('&');
            self.i += 1;
        }
        while let Some(ch @ ('<' | '>')) = self.at(self.i) {
            op.push(ch);
            self.i += 1;
        }
        match self.at(self.i) {
            Some('&') if !op.starts_with('&') => {
                op.push('&');
                self.i += 1;
            }
            Some('-') if op == "<<" => {
                op.push('-');
                self.i += 1;
            }
            Some('|') if op == ">" => self.i += 1,
            _ => {}
        }
        if op == "<<" || op == "<<-" {
            self.push_word("<<", false);
            self.heredoc_delimiter(op == "<<-");
        } else {
            self.push_word(&op, false);
        }
    }

    /// heredoc 的結束字（去掉引號與反斜線），先放一個空的詞，換行之後讀到本文再填進去。
    fn heredoc_delimiter(&mut self, strip_tabs: bool) {
        while matches!(self.at(self.i), Some(' ' | '\t')) {
            self.i += 1;
        }
        let mut delim = String::new();
        while let Some(n) = self.at(self.i) {
            if n.is_whitespace() || matches!(n, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
                break;
            }
            if !matches!(n, '\'' | '"' | '\\') {
                delim.push(n);
            }
            self.i += 1;
        }
        let segment = self.out.segments.len().saturating_sub(1);
        let word = self.out.segments.last().map_or(0, |s| s.words.len());
        self.push_word("", true);
        self.heredocs.push(Heredoc {
            delim,
            strip_tabs,
            segment,
            word,
        });
    }

    /// 換行之後：依序讀完每個 heredoc 的本文（到只有結束字的那一行；沒有就到結尾）。
    fn read_heredocs(&mut self) {
        for h in std::mem::take(&mut self.heredocs) {
            let mut body: Vec<String> = Vec::new();
            while self.i < self.c.len() {
                let end = (self.i..self.c.len())
                    .find(|&k| self.c[k] == '\n')
                    .unwrap_or(self.c.len());
                let line: String = self.c[self.i..end].iter().collect();
                self.i = (end + 1).min(self.c.len());
                let line = line.strip_suffix('\r').unwrap_or(&line);
                let line = if h.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                if line == h.delim {
                    break;
                }
                body.push(line.to_string());
            }
            if let Some(w) = self
                .out
                .segments
                .get_mut(h.segment)
                .and_then(|s| s.words.get_mut(h.word))
            {
                w.text = body.join(" ");
            }
        }
    }
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
/// 同步 I/O，async 的呼叫端要放進 `spawn_blocking`；要設時限時改用 [`probe_job`]。
pub fn attach_probe(h: &mut HookInput) {
    if let Some(job) = probe_job(h) {
        h.set_probe(job.run());
    }
}

/// 讀檔探測要用到的欄位（從 hook 複製出來）：可以丟到 blocking 執行緒、逾時就放著不管，hook 照常處理（#762 W-M5）。
pub struct ProbeJob {
    edit: bool,
    path: String,
    /// 只有 cwd 底下的檔才讀內容；cwd 外的只看在不在（hook 可以被偽造，S-4）
    read: bool,
    old: String,
    new: String,
}

/// UNC 路徑（`\\server\share`、`//server/share`、`\\?\UNC\…`）：碰了 Windows 就會去連那台主機、送出 NTLM 憑證，
/// 離線的主機還會讓呼叫卡住幾十秒。
pub fn is_unc(path: &str) -> bool {
    let p = path.replace('\\', "/");
    p.starts_with("//")
}

/// `path` 在 `cwd` 底下（`/` 與 `\\` 視為相同；Windows、macOS 不分大小寫）。
fn is_under(cwd: &str, path: &str) -> bool {
    let norm = |p: &str| {
        let p = p.replace('\\', "/");
        let p = p.trim_end_matches('/').to_string();
        if cfg!(any(windows, target_os = "macos")) {
            p.to_lowercase()
        } else {
            p
        }
    };
    let (cwd, path) = (norm(cwd), norm(path));
    !cwd.is_empty()
        && path.len() > cwd.len()
        && path.starts_with(&cwd)
        && path[cwd.len()..].starts_with('/')
}

/// 這個 hook 要不要讀檔探測，要的話把需要的欄位複製出來。UNC 路徑一律不碰。
pub fn probe_job(h: &HookInput) -> Option<ProbeJob> {
    if !needs_probe(h) {
        return None;
    }
    let input = h.field("tool_input")?;
    let path = input["file_path"].as_str()?;
    if is_unc(path) {
        return None;
    }
    // replace_all 的 Edit 不畫 hunk（D7），不用讀檔
    let edit =
        h.str_field("tool_name") == Some("Edit") && input["replace_all"].as_bool() != Some(true);
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    Some(ProbeJob {
        edit,
        path: path.to_string(),
        read: h.str_field("cwd").is_some_and(|cwd| is_under(cwd, path)),
        old: if edit { s("old_string") } else { String::new() },
        new: if edit { s("new_string") } else { String::new() },
    })
}

impl ProbeJob {
    /// 同步讀檔；網路磁碟上可能很慢，呼叫端自己設時限。
    pub fn run(self) -> FileProbe {
        let meta = std::fs::metadata(&self.path).ok();
        let mut probe = FileProbe {
            exists: meta.is_some(),
            hunk: None,
        };
        // 只讀一般檔案：FIFO、裝置檔讀了可能不會回來
        if self.edit
            && self.read
            && meta.is_some_and(|m| m.is_file() && m.len() <= PROBE_MAX_BYTES)
            && let Ok(text) = std::fs::read_to_string(&self.path)
        {
            probe.hunk = edit_hunk(&text, &self.old, &self.new);
        }
        probe
    }
}
