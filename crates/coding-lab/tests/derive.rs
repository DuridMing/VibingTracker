//! 衍生事件（#633）：Edit hunk、git 解析、相對路徑、價格、merge 的 file／git／agent／usage／model。

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use coding_lab::derive::{edit_hunk, git_op_from, relative_path};
use coding_lab::event::{Event, HunkSource};
use coding_lab::hook::HookInput;
use coding_lab::merge::{DangerRules, Input, Merger};
use coding_lab::prices::Prices;
use serde_json::{Value, json};

static N: AtomicU32 = AtomicU32::new(0);

fn tmpdir() -> std::path::PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "derive-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ── hunk

fn lines(h: &coding_lab::event::Hunk) -> Vec<&str> {
    h.lines.iter().map(String::as_str).collect()
}

#[test]
fn hunk_in_the_middle_has_three_lines_of_context() {
    let text = "1\n2\n3\n4\n5\nold\n7\n8\n9\n10\n";
    let h = edit_hunk(text, "old", "new").unwrap();
    assert_eq!((h.start, h.old_lines, h.new_lines), (3, 7, 7));
    assert_eq!(
        lines(&h),
        [" 3", " 4", " 5", "-old", "+new", " 7", " 8", " 9"]
    );
}

#[test]
fn hunk_at_file_start_and_end() {
    let h = edit_hunk("a\nb\nc\nd\ne\n", "a", "A").unwrap();
    assert_eq!(h.start, 1);
    assert_eq!(lines(&h), ["-a", "+A", " b", " c", " d"]);
    let h = edit_hunk("a\nb\nc\nd\ne", "e", "E").unwrap();
    assert_eq!(h.start, 2);
    assert_eq!(lines(&h), [" b", " c", " d", "-e", "+E"]);
}

#[test]
fn hunk_partial_line_and_multi_line_replacement() {
    let h = edit_hunk(
        "x = 1\ndef sub(a, b):\n    return a + b\n",
        "a + b",
        "a - b",
    )
    .unwrap();
    assert_eq!(
        lines(&h),
        [
            " x = 1",
            " def sub(a, b):",
            "-    return a + b",
            "+    return a - b"
        ]
    );
    let h = edit_hunk("p\nq\nr\n", "q\n", "q1\nq2\n").unwrap();
    assert_eq!(lines(&h), [" p", "-q", "+q1", "+q2", " r"]);
    assert_eq!((h.old_lines, h.new_lines), (3, 4));
}

#[test]
fn hunk_uses_first_occurrence_and_handles_missing() {
    let h = edit_hunk("dup\nmid\ndup\n", "dup", "D").unwrap();
    assert_eq!(h.start, 1, "多處相同時取第一處（replace_all 只畫第一段）");
    assert!(edit_hunk("abc\n", "zzz", "y").is_none());
    assert!(edit_hunk("abc\n", "", "y").is_none());
}

/// 驗收：與 `git diff -U3` 抽查 20 筆一致（起始行號與內容行）。
#[test]
fn hunk_matches_git_diff_for_20_edits() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("略過：沒有 git");
        return;
    }
    let base: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    let mut cases: Vec<(String, String, String)> = Vec::new();
    for i in [1, 2, 3, 4, 10, 20, 37, 38, 39, 40] {
        cases.push((
            base.clone(),
            format!("line {i}\n"),
            format!("LINE {i}\nadded\n"),
        ));
    }
    for i in [1, 5, 15, 25, 33, 38, 39, 40] {
        cases.push((base.clone(), format!("line {i}"), format!("changed {i}")));
    }
    cases.push((
        base.clone(),
        "line 7\nline 8\nline 9\n".into(),
        "merged\n".into(),
    ));
    cases.push((base.trim_end().to_string(), "line 40".into(), "last".into()));
    assert_eq!(cases.len(), 20);

    for (n, (before, old, new)) in cases.iter().enumerate() {
        let dir = tmpdir();
        let git = |args: &[&str]| {
            let o = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap();
            String::from_utf8(o.stdout).unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("f.txt"), before).unwrap();
        git(&["add", "f.txt"]);
        let h = edit_hunk(before, old, new).unwrap();
        std::fs::write(dir.join("f.txt"), before.replacen(old.as_str(), new, 1)).unwrap();
        let diff = git(&["diff", "-U3", "--no-color", "f.txt"]);
        let header = diff.lines().find(|l| l.starts_with("@@")).unwrap();
        let start: u32 = header[4..]
            .split([',', ' '])
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let body: Vec<&str> = diff
            .lines()
            .skip_while(|l| !l.starts_with("@@"))
            .skip(1)
            .filter(|l| !l.starts_with('\\'))
            .collect();
        assert_eq!(h.start, start.max(1), "case {n}: {header}");
        assert_eq!(lines(&h), body, "case {n}");
    }
}

// ── git

#[test]
fn git_commit_from_structured_git_operation() {
    let cmd = "git add a.py && git commit -m \"$(cat <<'EOF'\n修正 sub\nEOF\n)\"";
    let resp = json!({"stdout": "[main 4cf93bc] 修正 sub\n 1 file changed", "stderr": "",
                      "gitOperation": {"commit": {"branch": "main", "kind": "committed", "sha": "4cf93bc"}}});
    let g = git_op_from(cmd, Some(&resp), true).unwrap();
    assert_eq!(
        (g.op.as_str(), g.branch.as_deref(), g.hash.as_deref()),
        ("commit", Some("main"), Some("4cf93bc"))
    );
    assert_eq!(g.message.as_deref(), Some("修正 sub"));
    assert!(g.ok);
}

#[test]
fn git_commit_parsed_from_stdout_without_git_operation() {
    let resp = json!({"stdout": "[feature/x 1a2b3c4] 加入 mul\n 2 files changed", "stderr": ""});
    let g = git_op_from("git commit -m x", Some(&resp), true).unwrap();
    assert_eq!(
        (g.branch.as_deref(), g.hash.as_deref(), g.message.as_deref()),
        (Some("feature/x"), Some("1a2b3c4"), Some("加入 mul"))
    );
}

#[test]
fn git_subcommands_and_non_git() {
    assert_eq!(
        git_op_from("git push origin main", None, true).unwrap().op,
        "push"
    );
    assert_eq!(
        git_op_from("git -C ../repo reset --hard HEAD~1", None, true)
            .unwrap()
            .op,
        "reset"
    );
    assert_eq!(
        git_op_from("cd x && git worktree add ../wt -b feat", None, true)
            .unwrap()
            .branch
            .as_deref(),
        Some("feat")
    );
    assert_eq!(
        git_op_from("git switch -c topic", None, true)
            .unwrap()
            .branch
            .as_deref(),
        Some("topic")
    );
    let failed = git_op_from("git push", None, false).unwrap();
    assert!(!failed.ok);
    assert!(
        git_op_from("git status && git diff", None, true).is_none(),
        "只讀的 git 指令不算"
    );
    assert!(git_op_from("ls -la", None, true).is_none());
    assert!(
        git_op_from("echo 'git commit'", None, true).is_none(),
        "字串裡的 git 不算"
    );
}

#[test]
fn paths_are_made_relative_with_forward_slashes() {
    assert_eq!(relative_path("/home/u/p", "/home/u/p/src/a.rs"), "src/a.rs");
    assert_eq!(relative_path("/home/u/p/", "/home/u/p/a.rs"), "a.rs");
    assert_eq!(relative_path("/home/u/p", "/etc/hosts"), "/etc/hosts");
    assert_eq!(relative_path("C:\\w\\p", "C:\\w\\p\\src\\a.rs"), "src/a.rs");
}

// ── prices

#[test]
fn cost_uses_builtin_prices_per_million_tokens() {
    let p = Prices::builtin();
    // claude-opus-5-5：輸入 $4、輸出 $20、cache 讀 $0.20、寫 5m 1.25×、寫 1h 2×
    let usage = json!({"input_tokens": 1_000_000, "output_tokens": 100_000, "cache_read_input_tokens": 1_000_000,
                       "cache_creation": {"ephemeral_5m_input_tokens": 1_000_000, "ephemeral_1h_input_tokens": 1_000_000}});
    let c = p.cost("claude-opus-5-5", &usage);
    assert!((c - (4.0 + 2.0 + 0.2 + 5.0 + 8.0)).abs() < 1e-9, "{c}");
    assert_eq!(
        p.cost("some-unknown-model", &usage),
        0.0,
        "不認得的模型不猜價格"
    );
}

// ── merge 整合

struct NoDanger;
impl DangerRules for NoDanger {
    fn check(&self, _: &str, _: &Value) -> Option<coding_lab::event::Danger> {
        None
    }
}

fn hook_with_probe(v: Value) -> HookInput {
    let mut h = HookInput::from_json(v.to_string().as_bytes()).unwrap();
    coding_lab::derive::attach_probe(&mut h);
    h
}

fn handle(m: &mut Merger, v: Value, ts: u64) -> Vec<Event> {
    m.handle(Input::Hook {
        hook: hook_with_probe(v),
        ts_seen: ts,
    })
}

fn of<'a>(evs: &'a [Event], t: &str) -> Vec<&'a Event> {
    evs.iter().filter(|e| e.type_name() == t).collect()
}

#[test]
fn file_events_from_pre_tool_use() {
    let dir = tmpdir();
    std::fs::write(dir.join("calc.py"), "def sub(a, b):\n    return a + b\n").unwrap();
    let cwd = dir.to_str().unwrap();
    let mut m = Merger::new(Box::new(NoDanger));
    let pre = |tool: &str, input: Value, id: &str| {
        json!({
            "session_id": "s1", "hook_event_name": "PreToolUse", "cwd": cwd,
            "tool_name": tool, "tool_input": input, "tool_use_id": id,
        })
    };

    let a = handle(
        &mut m,
        pre("Read", json!({"file_path": dir.join("calc.py")}), "r1"),
        1,
    );
    assert_eq!(of(&a, "file.read")[0].payload["path"], "calc.py");

    let e = handle(
        &mut m,
        pre(
            "Edit",
            json!({"file_path": dir.join("calc.py"), "old_string": "a + b", "new_string": "a - b"}),
            "e1",
        ),
        2,
    );
    let fe = of(&e, "file.edit")[0];
    assert_eq!(
        fe.payload["hunkSource"],
        serde_json::to_value(HunkSource::Pre).unwrap()
    );
    assert_eq!(fe.payload["hunk"]["start"], 1);
    assert_eq!(fe.pos, None, "衍生事件不佔串流位置");

    let w = handle(
        &mut m,
        pre(
            "Write",
            json!({"file_path": dir.join("new.py"), "content": "x\n"}),
            "w1",
        ),
        3,
    );
    assert_eq!(of(&w, "file.write")[0].payload["isNew"], true);
    let w = handle(
        &mut m,
        pre(
            "Write",
            json!({"file_path": dir.join("calc.py"), "content": "y\n"}),
            "w2",
        ),
        4,
    );
    assert_eq!(of(&w, "file.write")[0].payload["isNew"], false);

    // 檔案太大或不存在：hunk 為 null、來源 none，但 file.edit 照發
    let e = handle(
        &mut m,
        pre(
            "Edit",
            json!({"file_path": dir.join("missing.py"), "old_string": "a", "new_string": "b"}),
            "e2",
        ),
        5,
    );
    assert_eq!(of(&e, "file.edit")[0].payload["hunkSource"], "none");
}

#[test]
fn git_op_from_post_tool_use() {
    let mut m = Merger::new(Box::new(NoDanger));
    let cmd = "git commit -m x";
    handle(
        &mut m,
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                          "tool_input": {"command": cmd}, "tool_use_id": "g1"}),
        1,
    );
    let a = handle(
        &mut m,
        json!({"session_id": "s1", "hook_event_name": "PostToolUse", "tool_name": "Bash",
                                  "tool_input": {"command": cmd}, "tool_use_id": "g1",
                                  "tool_response": {"stdout": "[main abc1234] x\n", "stderr": "",
                                                    "gitOperation": {"commit": {"branch": "main", "sha": "abc1234"}}}}),
        2,
    );
    let g = of(&a, "git.op")[0];
    assert_eq!(
        (g.payload["op"].as_str(), g.payload["hash"].as_str()),
        (Some("commit"), Some("abc1234"))
    );
}

#[test]
fn agent_spawned_takes_name_from_the_agent_tool_call() {
    let mut m = Merger::new(Box::new(NoDanger));
    handle(
        &mut m,
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Agent",
                          "tool_input": {"description": "找 .md 檔", "prompt": "列出所有 .md", "subagent_type": "Explore"},
                          "tool_use_id": "t1"}),
        1,
    );
    let a = handle(
        &mut m,
        json!({"session_id": "s1", "hook_event_name": "SubagentStart",
                                  "agent_id": "a1", "agent_type": "Explore"}),
        2,
    );
    let s = of(&a, "agent.spawned")[0];
    assert_eq!(s.payload["name"], "找 .md 檔");
    assert_eq!(s.payload["task"], "列出所有 .md");
    assert_eq!(s.payload["parentToolUseId"], "t1");
    let b = handle(
        &mut m,
        json!({"session_id": "s1", "hook_event_name": "SubagentStop", "agent_id": "a1",
                                  "agent_type": "Explore", "last_assistant_message": "只有 notes.md",
                                  "agent_transcript_path": "/t/s1/subagents/agent-a1.jsonl"}),
        3,
    );
    let f = of(&b, "agent.finished")[0];
    assert_eq!(f.payload["agentId"], "a1");
    assert_eq!(f.agent, "a1");
}

fn assistant(uuid: &str, msg: &str, model: &str, block: Value) -> Value {
    json!({"type": "assistant", "uuid": uuid, "timestamp": "2026-10-16T00:00:00Z",
           "message": {"id": msg, "model": model, "content": [block],
                       "usage": {"input_tokens": 1_000_000, "output_tokens": 0,
                                 "output_tokens_details": {"thinking_tokens": 12}}}})
}

fn line(m: &mut Merger, no: u64, v: Value) -> Vec<Event> {
    m.handle(Input::Line {
        session: "s1".into(),
        file: "main".into(),
        no,
        value: v,
        ts_seen: no,
    })
}

#[test]
fn usage_once_per_message_and_model_change_line() {
    let mut m = Merger::new(Box::new(NoDanger));
    let think = json!({"type": "thinking", "thinking": "…", "signature": "s"});
    let text = json!({"type": "text", "text": "好"});
    let a = line(
        &mut m,
        1,
        assistant("u1", "m1", "claude-opus-5-5", think.clone()),
    );
    let b = line(
        &mut m,
        2,
        assistant("u2", "m1", "claude-opus-5-5", text.clone()),
    );
    assert_eq!(of(&a, "usage").len(), 1);
    assert!(of(&b, "usage").is_empty(), "同一個 message.id 只算一次");
    let u = of(&a, "usage")[0];
    assert_eq!(u.payload["thinking"], 12);
    assert!((u.payload["costUsd"].as_f64().unwrap() - 4.0).abs() < 1e-9);
    assert!(of(&a, "model.changed").is_empty(), "第一個模型不算切換");

    let c = line(&mut m, 3, assistant("u3", "m2", "claude-sonnet-5-5", text));
    let mc = of(&c, "model.changed")[0];
    assert_eq!(
        (mc.payload["from"].as_str(), mc.payload["to"].as_str()),
        (Some("claude-opus-5-5"), Some("claude-sonnet-5-5"))
    );
    assert!(mc.pos.is_some(), "模型切換是串流裡的一行狀態");
    let text_ev = of(&c, "assistant.text")[0];
    assert!(mc.pos < text_ev.pos, "狀態行在該行之前");
}

#[test]
fn synthetic_model_is_ignored() {
    let mut m = Merger::new(Box::new(NoDanger));
    let text = json!({"type": "text", "text": "x"});
    line(
        &mut m,
        1,
        assistant("u1", "m1", "claude-opus-5-5", text.clone()),
    );
    let a = line(&mut m, 2, assistant("u2", "m2", "<synthetic>", text));
    assert!(of(&a, "model.changed").is_empty());
    assert!(of(&a, "usage").is_empty());
}
