//! 衍生事件（#633）：Edit hunk、git 解析、相對路徑、價格、merge 的 file／git／agent／usage／model。

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use coding_lab::derive::{edit_hunk, git_op_from, relative_path};
use coding_lab::event::{Event, HunkSource};
use coding_lab::hook::HookInput;
use coding_lab::merge::{DangerRules, Input, Merger, SESSION_IDLE_EVICT_MS};
use coding_lab::prices::Prices;
use coding_lab::store::Store;
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
            // pre-commit hook 裡跑時會繼承 GIT_DIR／GIT_INDEX_FILE 等，不清掉的話 tmpdir 的
            // git init／add 會寫進正在 commit 的 repo（worktree 的 index 是絕對路徑）
            let o = Command::new("git")
                .current_dir(&dir)
                .env_remove("GIT_DIR")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_OBJECT_DIRECTORY")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_PREFIX")
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

fn session_start(sid: &str, cwd: &str, source: &str) -> Value {
    json!({"session_id": sid, "hook_event_name": "SessionStart", "cwd": cwd, "source": source})
}

/// hook 的 cwd 是「當下」的（session 裡 `cd` 過就會變），file_path 是絕對路徑。
fn edit_at(sid: &str, hook_cwd: &str, file_path: &str, id: &str) -> Value {
    json!({
        "session_id": sid, "hook_event_name": "PreToolUse", "cwd": hook_cwd, "tool_name": "Edit",
        "tool_input": {"file_path": file_path, "old_string": "a", "new_string": "b"}, "tool_use_id": id,
    })
}

fn edit_path(evs: &[Event]) -> String {
    of(evs, "file.edit")[0].payload["path"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn file_events_from_pre_tool_use() {
    let dir = tmpdir();
    std::fs::write(dir.join("calc.py"), "def sub(a, b):\n    return a + b\n").unwrap();
    let cwd = dir.to_str().unwrap();
    let mut m = Merger::new(Box::new(NoDanger));
    handle(&mut m, session_start("s1", cwd, "startup"), 0);
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
    assert!(
        of(&a, "agent.spawned").is_empty(),
        "有候選呼叫時等 meta.json（#680）"
    );
    // serve 讀到 meta.json 的 toolUseId 後送進來
    let a = m.handle(Input::AgentMeta {
        session: "s1".into(),
        agent_id: "a1".into(),
        meta: coding_lab::merge::AgentMetaRead::ToolUse("t1".into()),
        ts_seen: 3,
    });
    let s = of(&a, "agent.spawned")[0];
    assert_eq!(s.payload["name"], "找 .md 檔");
    assert_eq!(s.payload["task"], "列出所有 .md");
    assert_eq!(s.payload["parentToolUseId"], "t1");
    let b = handle(
        &mut m,
        json!({"session_id": "s1", "hook_event_name": "SubagentStop", "agent_id": "a1",
                                  "agent_type": "Explore", "last_assistant_message": "只有 notes.md",
                                  "agent_transcript_path": "/t/s1/subagents/agent-a1.jsonl"}),
        4,
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
    let u = of(&c, "usage")[0];
    assert_eq!(u.payload["model"], "claude-sonnet-5-5");
    assert!(
        (u.payload["costUsd"].as_f64().unwrap() - 2.0).abs() < 1e-9,
        "切換後改用 sonnet 的單價（輸入 $2／百萬）：{}",
        u.payload
    );
    let text_ev = of(&c, "assistant.text")[0];
    assert!(mc.pos < text_ev.pos, "狀態行在該行之前");
}

/// 同一則訊息拆成多行、collector 在行與行之間重啟（K-13）：重啟後接著讀的行不再算一次 usage，
/// 新的訊息照算。上一輪最後的 usage 由 store 的最近 Event 交回（`resume_staged`）。
#[test]
fn usage_is_not_counted_again_after_a_restart_mid_message() {
    let think = json!({"type": "thinking", "thinking": "…", "signature": "s"});
    let text = json!({"type": "text", "text": "好"});
    let mut a = Merger::new(Box::new(NoDanger));
    let before = line(&mut a, 1, assistant("u1", "m1", "claude-opus-5-5", think));
    assert_eq!(of(&before, "usage").len(), 1);

    let mut b = Merger::new(Box::new(NoDanger));
    let last = before.last().unwrap();
    b.resume("s1", last.seq, 0, "");
    b.resume_staged(&before, 100);
    let after = line(
        &mut b,
        2,
        assistant("u2", "m1", "claude-opus-5-5", text.clone()),
    );
    assert!(of(&after, "usage").is_empty(), "m1 重啟前已經算過");
    let next = line(&mut b, 3, assistant("u3", "m2", "claude-opus-5-5", text));
    assert_eq!(of(&next, "usage").len(), 1, "新的訊息照算");
}

/// 「同一則訊息的行一定連在一起」是以檔為單位：subagent 的行夾在 main 的同一則訊息中間，
/// main 那則訊息不算第二次（K-13 改成每個檔記最後一則）。
#[test]
fn usage_once_per_message_with_subagent_lines_in_between() {
    let mut m = Merger::new(Box::new(NoDanger));
    let text = json!({"type": "text", "text": "好"});
    let mut all = line(
        &mut m,
        1,
        assistant("u1", "m1", "claude-opus-5-5", text.clone()),
    );
    all.extend(m.handle(Input::Line {
        session: "s1".into(),
        file: "agent-a1".into(),
        no: 1,
        value: assistant("u9", "m9", "claude-opus-5-5", text.clone()),
        ts_seen: 2,
    }));
    all.extend(line(
        &mut m,
        2,
        assistant("u2", "m1", "claude-opus-5-5", text),
    ));
    let ids: Vec<&str> = of(&all, "usage")
        .iter()
        .map(|e| e.payload["messageId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["m1", "m9"]);
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

// ── #672：file.* 的 path 相對 session 第一次 session.started 的 cwd

/// 1. session 中途 `cd` 進 worktree：path 仍相對開始時的 cwd，不是 hook 當下的 cwd。
#[test]
fn file_path_is_relative_to_the_session_start_cwd_not_the_current_one() {
    let mut m = Merger::new(Box::new(NoDanger));
    handle(&mut m, session_start("s1", "/r", "startup"), 1);
    let e = handle(
        &mut m,
        edit_at("s1", "/r/worktrees/x", "/r/worktrees/x/README.md", "e1"),
        2,
    );
    assert_eq!(edit_path(&e), "worktrees/x/README.md");
}

/// 2. session cwd 以外的檔：絕對路徑。
#[test]
fn file_outside_the_session_cwd_keeps_its_absolute_path() {
    let mut m = Merger::new(Box::new(NoDanger));
    handle(&mut m, session_start("s1", "/r", "startup"), 1);
    let e = handle(&mut m, edit_at("s1", "/r", "/tmp/a.txt", "e1"), 2);
    assert_eq!(edit_path(&e), "/tmp/a.txt");
}

/// 3. 沒收到 session.started（session 比 collector 早開）：第一個 main hook 的 cwd 當基準，並補 session.started（late）。
///    #672 原本存絕對路徑、不拿 hook 的 cwd；#710 推翻：空的 cwd 讓 session 歸不到任何專案，比基準偏了更糟。
#[test]
fn without_session_started_the_first_main_hook_sets_the_base() {
    let mut m = Merger::new(Box::new(NoDanger));
    let e = handle(&mut m, edit_at("s1", "/r", "/r/a.rs", "e1"), 1);
    assert_eq!(e[0].kind, "session.started");
    assert_eq!(e[0].payload["source"], "late");
    assert_eq!(e[0].payload["cwd"], "/r");
    assert_eq!(edit_path(&e), "a.rs");
}

/// 4. merge → store：changes／file_touch 的 (cwd, path) 拼回來就是工具碰的那個檔。
#[test]
fn store_cwd_and_path_point_at_the_file_the_tool_touched() {
    let home = tmpdir();
    let mut st = Store::open(&home).unwrap();
    let mut m = Merger::new(Box::new(NoDanger));
    for e in handle(&mut m, session_start("s1", "/r", "startup"), 1) {
        st.append(e);
    }
    for e in handle(
        &mut m,
        edit_at("s1", "/r/worktrees/x", "/r/worktrees/x/README.md", "e1"),
        2,
    ) {
        st.append(e);
    }
    st.flush().unwrap();
    let cwd = st.session("s1").unwrap().unwrap().cwd;
    let c = st.recent_changes(&cwd, 10).unwrap();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(format!("{cwd}/{}", c[0].path), "/r/worktrees/x/README.md");
    assert_eq!(st.touches(&cwd, &c[0].path).unwrap().len(), 1);
}

/// 5. collector 重啟：基準從 store 交回來，不再補 session.started；store 也不知道（空字串，#710 之前存的）
///    就和沒收到 session.started 一樣，由下一個 main hook 的 cwd 補上。
#[test]
fn resumed_session_keeps_its_path_base() {
    let mut m = Merger::new(Box::new(NoDanger));
    m.resume("s1", 10, 5, "/r");
    m.resume("s2", 10, 5, "");
    let e = handle(
        &mut m,
        edit_at("s1", "/r/worktrees/x", "/r/worktrees/x/README.md", "e1"),
        1,
    );
    assert!(e.iter().all(|e| e.kind != "session.started"), "{e:?}");
    assert_eq!(edit_path(&e), "worktrees/x/README.md");
    let e = handle(&mut m, edit_at("s2", "/r", "/r/a.rs", "e2"), 2);
    assert_eq!(e[0].kind, "session.started");
    assert_eq!(edit_path(&e), "a.rs");
}

/// 6. 之後的 session.started（例如在 worktree 裡 /compact）不換基準。
#[test]
fn later_session_start_does_not_change_the_path_base() {
    let mut m = Merger::new(Box::new(NoDanger));
    handle(&mut m, session_start("s1", "/r", "startup"), 1);
    handle(&mut m, session_start("s1", "/r/worktrees/x", "compact"), 2);
    let e = handle(
        &mut m,
        edit_at("s1", "/r/worktrees/x", "/r/worktrees/x/README.md", "e1"),
        3,
    );
    assert_eq!(edit_path(&e), "worktrees/x/README.md");
}

/// 7. Windows 路徑：session.started 的 cwd 在入口就正規化成 `/`，store／live／WS 拿到同一個值。
#[test]
fn session_started_cwd_is_normalized_to_forward_slashes() {
    let mut m = Merger::new(Box::new(NoDanger));
    let s = handle(&mut m, session_start("s1", "C:\\r", "startup"), 1);
    assert_eq!(of(&s, "session.started")[0].payload["cwd"], "C:/r");
    let e = handle(&mut m, edit_at("s1", "C:\\r", "C:\\r\\src\\a.rs", "e1"), 2);
    assert_eq!(edit_path(&e), "src/a.rs");
}

/// 8. session 被移出記憶體（evict）後，回來的第一個事件就是 file.*：基準與 seq 都要接續。
#[test]
fn evicted_session_keeps_its_path_base() {
    let mut m = Merger::new(Box::new(NoDanger));
    handle(&mut m, session_start("s1", "/r", "startup"), 1);
    // 沒有 SessionEnd：移出記憶體前先補 session.ended（abandoned）與 status（#679）
    let gone = m.tick(1 + SESSION_IDLE_EVICT_MS);
    assert_eq!(m.session_count(), 0);
    let last_seq = gone.last().unwrap().seq;
    let e = handle(
        &mut m,
        edit_at("s1", "/r/worktrees/x", "/r/worktrees/x/README.md", "e1"),
        2 + SESSION_IDLE_EVICT_MS,
    );
    assert_eq!(edit_path(&e), "worktrees/x/README.md");
    assert_eq!(e[0].seq, last_seq + 1, "{e:?}");
}

/// file.write 帶全文行數 `lines`（#682）：content 只留 200 行，改動紀錄的 +N 用 lines。
#[test]
fn write_carries_the_full_line_count() {
    let mut m = Merger::new(Box::new(NoDanger));
    let content: String = (0..5000).map(|i| format!("l{i}\n")).collect();
    let w = handle(
        &mut m,
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Write",
               "tool_input": {"file_path": "/r/big.txt", "content": content}, "tool_use_id": "w1"}),
        1,
    );
    let p = &of(&w, "file.write")[0].payload;
    assert_eq!(p["truncated"], true);
    assert_eq!(p["lines"], 5000);
}

/// C7（#744）：Edit 的 new_string 很大時，file.edit.hunk.lines 合計也不超過 8 KB：放不下的行丟掉，
/// 最後一行截在字元邊界，標 truncated；oldLines／newLines 保留原值，+N／−M 照算。
#[test]
fn edit_hunk_is_bounded_in_bytes() {
    let dir = tmpdir();
    let f = dir.join("a.txt");
    std::fs::write(&f, "a\nOLD\nb\n").unwrap();
    let path = f.to_str().unwrap();
    let mut m = Merger::new(Box::new(NoDanger));
    let edit = |new: String, id: &str| {
        json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": dir,
               "tool_input": {"file_path": path, "old_string": "OLD", "new_string": new}, "tool_use_id": id})
    };
    // 一行 1.5 MB
    let evs = handle(&mut m, edit("中".repeat(500_000), "e1"), 1);
    let fe = &of(&evs, "file.edit")[0].payload;
    let lines: Vec<&str> = fe["hunk"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(lines.iter().map(|l| l.len()).sum::<usize>() <= 8 * 1024);
    assert_eq!(fe["truncated"], true);
    assert_eq!(fe["hunk"]["newLines"], 3, "a、新行、b");
    // 很多行
    let many: Vec<String> = (0..5000).map(|i| format!("line {i}")).collect();
    let evs = handle(&mut m, edit(many.join("\n"), "e2"), 2);
    let fe = &of(&evs, "file.edit")[0].payload;
    let n = fe["hunk"]["lines"].as_array().unwrap().len();
    assert!(n < 5000, "{n}");
    assert_eq!(fe["truncated"], true);
    assert_eq!(fe["hunk"]["newLines"], 5002);
    // 小改動照舊，不輸出 truncated
    let evs = handle(&mut m, edit("NEW".into(), "e3"), 3);
    let fe = &of(&evs, "file.edit")[0].payload;
    assert!(fe.get("truncated").is_none());
    assert_eq!(
        fe["hunk"]["lines"].as_array().unwrap().len(),
        4,
        "a、-OLD、+NEW、b"
    );
}
