//! 「現在在做什麼」一句話（#648，contracts.md §6 `sessions.doing`）：純字串處理。
//!
//! 只看這一輪（最近一次 prompt 之後），依序取 main 的文字 → 非空 thinking → 工具動作 → prompt。

/// 截成幾個字元（code point）。
pub const DOING_LIMIT: usize = 80;

/// 這一句從哪裡取的；遠端連線不送 thinking（不變式 6、7）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Text,
    Thinking,
    Tool,
    Prompt,
}

/// 一個 session 這一輪（最近一次 prompt 之後）的各順位來源。
#[derive(Clone, Debug, Default)]
pub struct Doing {
    text: Option<String>,
    thinking: Option<String>,
    tool: Option<String>,
    prompt: Option<String>,
}

impl Doing {
    /// 新的一輪；背景工作完成的 `<task-notification>` 顯示「背景工作完成」，不顯示原始 XML。
    pub fn prompt(&mut self, text: &str, task: bool) {
        *self = Doing {
            prompt: if task {
                Some("背景工作完成".into())
            } else {
                first_sentence(text)
            },
            ..Doing::default()
        };
    }

    pub fn text(&mut self, text: &str) {
        if let Some(s) = first_sentence(text) {
            self.text = Some(s);
        }
    }

    /// redacted 的空 thinking 跳過。
    pub fn thinking(&mut self, text: &str) {
        if let Some(s) = first_sentence(text) {
            self.thinking = Some(s);
        }
    }

    /// 工具動作（agents 列的 `current`）不切句，只整理空白與長度。
    pub fn tool(&mut self, current: &str) {
        let s = clip(&collapse(current));
        if !s.is_empty() {
            self.tool = Some(s);
        }
    }

    /// 本機連線看到的。
    pub fn local(&self) -> Option<(Source, &str)> {
        [
            (Source::Text, &self.text),
            (Source::Thinking, &self.thinking),
            (Source::Tool, &self.tool),
            (Source::Prompt, &self.prompt),
        ]
        .into_iter()
        .find_map(|(src, v)| v.as_deref().map(|s| (src, s)))
    }
}

/// 第一句：去掉 markdown 標記、換行壓成空白，截 80 個字元（code point）加「…」。
///
/// 句尾是 `。！？`，或後面接空白／結尾的 `.!?`（所以檔名、網址、版本號、小數不會被切）；
/// 反引號裡不切；空行也算一句結束。
pub fn first_sentence(text: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = strip_line_marks(line.trim());
        if line.is_empty() {
            if lines.is_empty() {
                continue;
            }
            break;
        }
        lines.push(line);
    }
    let joined = lines.join(" ");

    let mut out = String::new();
    let mut in_code = false;
    let mut chars = joined.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            in_code = !in_code;
            continue;
        }
        out.push(c);
        if in_code {
            continue;
        }
        let ends = matches!(c, '。' | '！' | '？')
            || (matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|n| n.is_whitespace()));
        if ends {
            break;
        }
    }
    let s = clip(&collapse(&out.replace("**", "")));
    (!s.is_empty()).then_some(s)
}

/// 行首的 markdown 標記：標題、引用、列表。
fn strip_line_marks(line: &str) -> &str {
    let l = line.trim_start_matches('#').trim_start();
    let l = l.strip_prefix('>').map_or(l, str::trim_start);
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = l.strip_prefix(m) {
            return rest.trim_start();
        }
    }
    let digits = l.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = l[digits..]
            .strip_prefix(". ")
            .or_else(|| l[digits..].strip_prefix(") "))
    {
        return rest.trim_start();
    }
    l
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str) -> String {
    if s.chars().count() <= DOING_LIMIT {
        return s.to_string();
    }
    let mut out: String = s.chars().take(DOING_LIMIT).collect();
    out.push('…');
    out
}
