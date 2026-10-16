//! session.status 與注意佇列的狀態轉移（#645，docs/plan/PLAN.md「注意佇列」、contracts.md `attention.*`）。
//!
//! 全部透過 merge 的 `handle`／`tick` 驅動，時間由輸入帶入（假時鐘），不 sleep。

use coding_lab::event::{AttentionKind, Danger, DangerLevel, Event};
use coding_lab::hook::HookInput;
use coding_lab::merge::{DangerRules, Input, Merger};
use coding_lab::status::{ScoreConfig, Signal, score};
use serde_json::{Value, json};

const S: &str = "s1";

fn hook(event: &str, extra: Value, ts: u64) -> Input {
    let mut v = json!({ "session_id": S, "hook_event_name": event });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    Input::Hook {
        hook: HookInput::from_json(v.to_string().as_bytes()).unwrap(),
        ts_seen: ts,
    }
}

fn bash(cmd: &str) -> Value {
    json!({ "command": cmd })
}

fn pre(id: &str, cmd: &str, ts: u64) -> Input {
    hook(
        "PreToolUse",
        json!({ "tool_name": "Bash", "tool_input": bash(cmd), "tool_use_id": id }),
        ts,
    )
}

fn pre_agent(id: &str, cmd: &str, agent: &str, ts: u64) -> Input {
    hook(
        "PreToolUse",
        json!({ "tool_name": "Bash", "tool_input": bash(cmd), "tool_use_id": id,
                "agent_id": agent, "agent_type": "Explore" }),
        ts,
    )
}

fn ask(cmd: &str, ts: u64) -> Input {
    hook(
        "PermissionRequest",
        json!({ "tool_name": "Bash", "tool_input": bash(cmd) }),
        ts,
    )
}

fn ok(id: &str, ts: u64) -> Input {
    hook(
        "PostToolUse",
        json!({ "tool_name": "Bash", "tool_use_id": id, "duration_ms": 10,
                "tool_response": { "stdout": "", "stderr": "" } }),
        ts,
    )
}

fn fail(id: &str, ts: u64) -> Input {
    hook(
        "PostToolUseFailure",
        json!({ "tool_name": "Bash", "tool_use_id": id, "duration_ms": 10,
                "error": "Exit code 1\nboom", "is_interrupt": false }),
        ts,
    )
}

fn fail_agent(id: &str, agent: &str, ts: u64) -> Input {
    hook(
        "PostToolUseFailure",
        json!({ "tool_name": "Bash", "tool_use_id": id, "duration_ms": 10,
                "error": "Exit code 1\nboom", "is_interrupt": false,
                "agent_id": agent, "agent_type": "Explore" }),
        ts,
    )
}

fn prompt(text: &str, ts: u64) -> Input {
    hook(
        "UserPromptSubmit",
        json!({ "prompt": text, "prompt_id": format!("p{ts}") }),
        ts,
    )
}

fn stop(ts: u64) -> Input {
    hook("Stop", json!({ "last_assistant_message": "好了" }), ts)
}

fn line(no: u64, v: Value, ts: u64) -> Input {
    Input::Line {
        session: S.into(),
        file: "main".into(),
        no,
        value: v,
        ts_seen: ts,
    }
}

/// 按 Esc 拒絕或中斷工具時 transcript 寫的 tool_result（實測，樣本 a41c94d2、5d21db12）。
fn rejected(no: u64, id: &str, ts: u64) -> Input {
    line(
        no,
        json!({ "type": "user", "uuid": format!("r{no}"), "toolUseResult": "User rejected tool use",
                "message": { "content": [{ "type": "tool_result", "tool_use_id": id, "is_error": true,
                                           "content": "The user doesn't want to proceed with this tool use." }] } }),
        ts,
    )
}

fn interrupted(no: u64, text: &str, ts: u64) -> Input {
    line(
        no,
        json!({ "type": "user", "uuid": format!("i{no}"),
                "message": { "content": [{ "type": "text", "text": text }] } }),
        ts,
    )
}

struct NoDanger;
impl DangerRules for NoDanger {
    fn check(&self, _: &str, _: &Value) -> Option<Danger> {
        None
    }
}

/// `rm -rf` 是 danger、`sudo` 是 warn。
struct Rules;
impl DangerRules for Rules {
    fn check(&self, _: &str, input: &Value) -> Option<Danger> {
        let c = input["command"].as_str()?;
        let level = if c.contains("rm -rf") {
            DangerLevel::Danger
        } else if c.contains("sudo") {
            DangerLevel::Warn
        } else {
            return None;
        };
        Some(Danger {
            rule: "r".into(),
            level,
            why: "測試".into(),
        })
    }
}

/// 收集所有事件，方便斷言「到目前為止」發生過什麼。
struct Run {
    m: Merger,
    evs: Vec<Event>,
}

impl Run {
    fn new() -> Run {
        Run {
            m: Merger::new(Box::new(NoDanger)),
            evs: Vec::new(),
        }
    }
    fn with_rules() -> Run {
        Run {
            m: Merger::new(Box::new(Rules)),
            evs: Vec::new(),
        }
    }
    fn feed(&mut self, i: Input) -> &mut Run {
        let e = self.m.handle(i);
        self.evs.extend(e);
        self
    }
    fn tick(&mut self, now: u64) -> &mut Run {
        let e = self.m.tick(now);
        self.evs.extend(e);
        self
    }
    /// 依序列出 attention 事件：("raised"|"cleared", kind, ref)
    fn attention(&self) -> Vec<(String, String, String)> {
        self.evs
            .iter()
            .filter_map(|e| {
                let what = e.type_name().strip_prefix("attention.")?;
                Some((
                    what.to_string(),
                    e.payload["kind"].as_str()?.to_string(),
                    e.payload["ref"].as_str()?.to_string(),
                ))
            })
            .collect()
    }
    /// 依序列出 session.status 的狀態
    fn statuses(&self) -> Vec<String> {
        self.evs
            .iter()
            .filter(|e| e.type_name() == "session.status")
            .map(|e| e.payload["status"].as_str().unwrap_or_default().to_string())
            .collect()
    }
    fn last_status(&self) -> String {
        self.statuses().pop().unwrap_or_else(|| "run".into())
    }
}

fn a(what: &str, kind: &str, r: &str) -> (String, String, String) {
    (what.into(), kind.into(), r.into())
}

// ── wait

#[test]
fn wait_is_raised_only_after_one_second() {
    let mut r = Run::new();
    r.feed(pre("t1", "ls", 1000))
        .feed(ask("ls", 1050))
        .tick(2000);
    assert!(r.attention().is_empty(), "1 秒內不發");
    r.tick(2050);
    assert_eq!(r.attention(), [a("raised", "wait", "t1")]);
    assert_eq!(r.last_status(), "wait");
}

#[test]
fn wait_resolved_within_one_second_is_never_raised() {
    let mut r = Run::new();
    r.feed(pre("t1", "ls", 1000))
        .feed(ask("ls", 1050))
        .feed(ok("t1", 1500))
        .tick(5000);
    assert!(r.attention().is_empty());
    assert!(r.statuses().iter().all(|s| s != "wait"));
}

#[test]
fn wait_matches_the_latest_running_card_with_same_input() {
    let mut r = Run::new();
    r.feed(pre("t1", "ls", 1000))
        .feed(ok("t1", 1100))
        .feed(pre("t2", "ls", 1200))
        .feed(ask("ls", 1250))
        .tick(2300);
    assert_eq!(r.attention(), [a("raised", "wait", "t2")]);
}

#[test]
fn wait_is_cleared_by_post_tool_use() {
    let mut r = Run::new();
    r.feed(pre("t1", "ls", 1000))
        .feed(ask("ls", 1050))
        .tick(2100)
        .feed(ok("t1", 3000));
    assert_eq!(
        r.attention(),
        [a("raised", "wait", "t1"), a("cleared", "wait", "t1")]
    );
    assert_eq!(r.last_status(), "run");
}

#[test]
fn wait_is_cleared_by_post_tool_batch_when_auto_denied() {
    let mut r = Run::new();
    r.feed(pre("t1", "ls", 1000))
        .feed(ask("ls", 1050))
        .tick(2100)
        .feed(hook("PostToolBatch", json!({}), 3000));
    assert_eq!(r.attention().last(), Some(&a("cleared", "wait", "t1")));
}

#[test]
fn wait_is_cleared_by_transcript_tool_result_when_esc() {
    let mut r = Run::new();
    r.feed(pre("t1", "ls", 1000))
        .feed(ask("ls", 1050))
        .tick(2100)
        .feed(rejected(5, "t1", 4000));
    assert_eq!(r.attention().last(), Some(&a("cleared", "wait", "t1")));
}

#[test]
fn wait_is_cleared_by_prompt_stop_and_session_end() {
    for clear in [
        prompt("下一步", 3000),
        stop(3000),
        hook("SessionEnd", json!({}), 3000),
    ] {
        let mut r = Run::new();
        r.feed(pre("t1", "ls", 1000))
            .feed(ask("ls", 1050))
            .tick(2100)
            .feed(clear);
        assert_eq!(r.attention().last(), Some(&a("cleared", "wait", "t1")));
    }
}

// ── done

#[test]
fn done_is_raised_five_seconds_after_stop() {
    let mut r = Run::new();
    r.feed(prompt("做事", 0)).feed(stop(1000)).tick(5999);
    assert!(r.attention().is_empty(), "靜默期內不發");
    r.tick(6000);
    assert_eq!(r.attention(), [a("raised", "done", "p0")]);
    assert_eq!(r.last_status(), "done");
}

#[test]
fn prompt_within_silence_cancels_done() {
    let mut r = Run::new();
    r.feed(prompt("做事", 0))
        .feed(stop(1000))
        .feed(prompt("再做", 3000))
        .tick(10_000);
    assert!(r.attention().is_empty());
}

#[test]
fn prompt_after_done_withdraws_it() {
    let mut r = Run::new();
    r.feed(prompt("做事", 0))
        .feed(stop(1000))
        .tick(6000)
        .feed(prompt("再做", 9000));
    assert_eq!(
        r.attention(),
        [a("raised", "done", "p0"), a("cleared", "done", "p0")]
    );
    assert_eq!(r.last_status(), "run");
}

#[test]
fn task_notification_withdraws_done_and_is_marked_as_task() {
    let mut r = Run::new();
    r.feed(prompt("做事", 0))
        .feed(stop(1000))
        .tick(6000)
        .feed(prompt("<task-notification>\n<task-id>b1</task-id>", 30_000));
    assert_eq!(r.attention().last(), Some(&a("cleared", "done", "p0")));
    let task = r
        .evs
        .iter()
        .rfind(|e| e.type_name() == "prompt.submitted")
        .unwrap();
    assert_eq!(task.payload["source"], "task");
    let user = r
        .evs
        .iter()
        .find(|e| e.type_name() == "prompt.submitted")
        .unwrap();
    assert!(
        user.payload.get("source").is_none(),
        "使用者輸入不帶 source"
    );
}

#[test]
fn subagent_stop_does_not_count_as_done() {
    let mut r = Run::new();
    r.feed(prompt("做事", 0))
        .feed(hook(
            "SubagentStop",
            json!({ "agent_id": "a1", "agent_type": "" }),
            1000,
        ))
        .tick(10_000);
    assert!(r.attention().is_empty());
}

// ── fail

#[test]
fn single_failure_is_not_raised() {
    let mut r = Run::new();
    r.feed(pre("t1", "x", 1000)).feed(fail("t1", 1100));
    assert!(r.attention().is_empty());
}

#[test]
fn two_consecutive_failures_raise_and_success_clears() {
    let mut r = Run::new();
    r.feed(pre("t1", "x", 1000))
        .feed(fail("t1", 1100))
        .feed(pre("t2", "y", 1200))
        .feed(fail("t2", 1300));
    assert_eq!(r.attention(), [a("raised", "fail", "t2")]);
    assert_eq!(r.last_status(), "fail");
    r.feed(pre("t3", "z", 1400)).feed(ok("t3", 1500));
    assert_eq!(r.attention().last(), Some(&a("cleared", "fail", "t2")));
    assert_eq!(r.last_status(), "run");
}

#[test]
fn failures_of_parallel_agents_are_counted_separately() {
    let mut r = Run::new();
    r.feed(hook(
        "SubagentStart",
        json!({ "agent_id": "a1", "agent_type": "Explore" }),
        900,
    ))
    .feed(pre("t1", "x", 1000))
    .feed(fail("t1", 1100))
    .feed(pre_agent("t2", "y", "a1", 1200))
    .feed(fail_agent("t2", "a1", 1300));
    assert!(
        r.attention().is_empty(),
        "main 一次、subagent 一次，不算連續"
    );
}

#[test]
fn interrupted_tool_is_not_a_failure() {
    let mut r = Run::new();
    r.feed(pre("t1", "x", 1000))
        .feed(fail("t1", 1100))
        .feed(pre("t2", "sleep", 1200))
        .feed(rejected(9, "t2", 5000));
    assert!(r.attention().is_empty(), "Esc 中斷不算第二次失敗");
}

// ── danger

#[test]
fn only_danger_level_enters_the_queue() {
    let mut r = Run::with_rules();
    r.feed(pre("t1", "sudo ls", 1000));
    assert!(r.attention().is_empty(), "warn 只在串流標示");
    r.feed(pre("t2", "rm -rf build", 1100));
    assert_eq!(r.attention(), [a("raised", "danger", "t2")]);
    assert_eq!(r.last_status(), "danger");
}

#[test]
fn danger_is_cleared_when_the_tool_ends_and_status_falls_back() {
    let mut r = Run::with_rules();
    r.feed(pre("t1", "rm -rf build", 1000)).feed(ok("t1", 1500));
    assert_eq!(
        r.attention(),
        [a("raised", "danger", "t1"), a("cleared", "danger", "t1")]
    );
    assert_eq!(r.last_status(), "run");
}

#[test]
fn status_is_the_highest_open_item() {
    let mut r = Run::with_rules();
    r.feed(pre("t1", "ls", 1000))
        .feed(ask("ls", 1050))
        .tick(2100)
        .feed(pre("t2", "rm -rf build", 2200));
    assert_eq!(r.last_status(), "danger", "danger > wait");
    r.feed(ok("t2", 2500));
    assert_eq!(r.last_status(), "wait", "danger 解除後退回 wait");
}

// ── idle 與 ended

#[test]
fn interrupt_puts_session_into_idle_without_attention() {
    for text in [
        "[Request interrupted by user]",
        "[Request interrupted by user for tool use]",
    ] {
        let mut r = Run::new();
        r.feed(prompt("寫短文", 0)).feed(interrupted(3, text, 2000));
        assert_eq!(r.last_status(), "idle", "{text}");
        assert!(r.attention().is_empty(), "idle 不進佇列");
        r.feed(prompt("繼續", 5000));
        assert_eq!(r.last_status(), "run");
    }
}

#[test]
fn session_end_is_ended_and_cancels_pending_done() {
    let mut r = Run::new();
    r.feed(prompt("做事", 0))
        .feed(stop(1000))
        .feed(hook(
            "SessionEnd",
            json!({ "reason": "prompt_input_exit" }),
            2000,
        ))
        .tick(10_000);
    assert_eq!(r.last_status(), "ended");
    assert!(r.attention().is_empty());
}

#[test]
fn status_event_only_on_change() {
    let mut r = Run::new();
    r.feed(pre("t1", "x", 1000))
        .feed(ok("t1", 1100))
        .feed(pre("t2", "y", 1200))
        .feed(ok("t2", 1300))
        .tick(60_000);
    assert!(r.statuses().is_empty(), "一直是 run 就不發");
}

// ── 分數（PLAN「session 分數」）

const MIN: u64 = 60_000;

#[test]
fn pending_items_grow_with_waiting_time_up_to_the_cap() {
    let c = ScoreConfig::default();
    let w = |age: u64| score(&c, &[(AttentionKind::Wait, 0)], &[], age);
    assert_eq!(w(0), 50.0);
    assert_eq!(w(2 * MIN), 60.0);
    assert_eq!(w(30 * MIN), 100.0, "上限 100");
    let d = score(&c, &[(AttentionKind::Danger, 0)], &[], 30 * MIN);
    assert_eq!(d, 100.0, "危險固定 100");
}

#[test]
fn signals_decay_with_ten_minute_half_life() {
    let c = ScoreConfig::default();
    let s = |age: u64| score(&c, &[], &[(Signal::Conflict, 0)], age);
    assert_eq!(s(0), 30.0);
    assert!((s(10 * MIN) - 15.0).abs() < 1e-9);
    assert!((s(20 * MIN) - 7.5).abs() < 1e-9);
}

#[test]
fn longer_wait_ranks_first_unless_a_conflict_adds_more() {
    let c = ScoreConfig::default();
    let now = 10 * MIN;
    // A 等了 2 分鐘；B 剛開始等，但 5 分鐘前碰了別人改的檔
    let a = score(&c, &[(AttentionKind::Wait, now - 2 * MIN)], &[], now);
    let b_plain = score(&c, &[(AttentionKind::Wait, now)], &[], now);
    let b = score(
        &c,
        &[(AttentionKind::Wait, now)],
        &[(Signal::Conflict, now - 5 * MIN)],
        now,
    );
    assert!(a > b_plain, "等越久越前面");
    assert!(b > a, "衝突讓分數反超");
    // 再過 30 分鐘，衝突衰減，兩邊都封頂在 100，B 不再超過 A
    let later = now + 30 * MIN;
    let a2 = score(&c, &[(AttentionKind::Wait, now - 2 * MIN)], &[], later);
    let b2 = score(
        &c,
        &[(AttentionKind::Wait, now)],
        &[(Signal::Conflict, now - 5 * MIN)],
        later,
    );
    assert!(b2 - a2 < 3.0, "衰減後差距只剩零頭");
}

#[test]
fn every_signal_weight_can_change_the_order() {
    let c = ScoreConfig::default();
    let base = score(&c, &[(AttentionKind::Done, 0)], &[], 0);
    for sig in [
        Signal::Conflict,
        Signal::SubagentFail,
        Signal::CostSpike,
        Signal::BigEdit,
        Signal::NewFile,
    ] {
        let with = score(&c, &[(AttentionKind::Done, 0)], &[(sig, 0)], 0);
        assert!(with > base, "{sig:?} 沒有作用");
    }
}

#[test]
fn empty_score_is_positive_zero() {
    // f64 的空加總是 -0.0，序列化成 JSON 會變成 `-0.0`
    let s = score(&ScoreConfig::default(), &[], &[], 0);
    assert!(s == 0.0 && s.is_sign_positive(), "{s}");
}

// ── samples 回放（實測樣本，沒有 samples/ 時略過）

/// 依到達時間把 hook 與 transcript 行餵進 merge；同一行號取最早的到達時間（resume 會重記 backfill）。
fn replay_sample(name: &str) -> Option<Vec<Event>> {
    replay_merger(name).map(|(_, evs, _)| evs)
}

/// 同 [`replay_sample`]，另外回傳回放完的 merge 與最後的時間（看分數用）。
fn replay_merger(name: &str) -> Option<(Merger, Vec<Event>, u64)> {
    replay_with(name, None)
}

/// 同 [`replay_merger`]，回放前先設好費用基準（#675）。
fn replay_with(name: &str, cost_baseline: Option<f64>) -> Option<(Merger, Vec<Event>, u64)> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/sessions")
        .join(name);
    let read = |f: &str| -> Vec<Value> {
        std::fs::read_to_string(dir.join(f))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let hooks = read("hooks.jsonl");
    if hooks.is_empty() {
        return None;
    }
    let session = hooks[0]["payload"]["session_id"].as_str()?.to_string();
    let mut inputs: Vec<(u64, u8, Input)> = Vec::new();
    for h in hooks.iter().filter(|h| h["via"] == "http") {
        let ts = h["ts_seen_ms"].as_u64()?;
        let hook = HookInput::from_json(h["payload"].to_string().as_bytes()).ok()?;
        inputs.push((ts, 0, Input::Hook { hook, ts_seen: ts }));
    }
    let mut arrival: HashMap<u64, u64> = HashMap::new();
    for a in read("transcript-arrivals.jsonl") {
        if let (Some(no), Some(ts)) = (a["line_no"].as_u64(), a["ts_seen_ms"].as_u64()) {
            let t = arrival.entry(no).or_insert(ts);
            *t = (*t).min(ts);
        }
    }
    for (i, v) in read("transcript.jsonl").into_iter().enumerate() {
        let no = i as u64 + 1;
        if let Some(&ts) = arrival.get(&no) {
            let file = "main".to_string();
            inputs.push((
                ts,
                1,
                Input::Line {
                    session: session.clone(),
                    file,
                    no,
                    value: v,
                    ts_seen: ts,
                },
            ));
        }
    }
    inputs.sort_by_key(|(ts, k, _)| (*ts, *k));
    let mut m = Merger::new(Box::new(NoDanger));
    m.set_cost_baseline(cost_baseline);
    let mut evs = Vec::new();
    let mut last = 0;
    for (ts, _, input) in inputs {
        evs.extend(m.tick(ts));
        evs.extend(m.handle(input));
        last = ts;
    }
    evs.extend(m.tick(last + 60_000));
    Some((m, evs, last))
}

fn statuses_of(evs: &[Event]) -> Vec<String> {
    evs.iter()
        .filter(|e| e.type_name() == "session.status")
        .map(|e| e.payload["status"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn samples_esc_goes_idle_and_is_never_a_failure() {
    // 377fa132：權限詢問時按 Esc；5d21db12：工具執行中按 Esc
    for name in ["377fa132", "5d21db12"] {
        let Some(evs) = replay_sample(name) else {
            eprintln!("略過：沒有 samples/sessions/{name}");
            continue;
        };
        let st = statuses_of(&evs);
        assert!(st.contains(&"idle".to_string()), "{name}: {st:?}");
        assert!(
            !evs.iter()
                .any(|e| e.type_name() == "attention.raised" && e.payload["kind"] == "fail"),
            "{name}: Esc 不算失敗"
        );
    }
}

#[test]
fn samples_background_task_is_marked_and_withdraws_done() {
    // 64c1a1a7：背景 sleep 在 Stop 後完成，以 <task-notification> 叫醒 Claude
    let Some(evs) = replay_sample("64c1a1a7") else {
        eprintln!("略過：沒有 samples/sessions/64c1a1a7");
        return;
    };
    assert!(
        evs.iter()
            .any(|e| e.type_name() == "prompt.submitted" && e.payload["source"] == "task")
    );
    let raised = evs
        .iter()
        .filter(|e| e.type_name() == "attention.raised" && e.payload["kind"] == "done")
        .count();
    let cleared = evs
        .iter()
        .filter(|e| e.type_name() == "attention.cleared" && e.payload["kind"] == "done")
        .count();
    assert!(
        raised >= 1 && cleared >= 1,
        "done 有發也有被撤回：{raised}／{cleared}"
    );
}

// ── sessions 列的分數（契約 §6：整數、每 10 秒重算）

#[test]
fn scores_lists_each_session_rounded() {
    let mut r = Run::with_rules();
    r.feed(pre("t1", "rm -rf build", 1000));
    assert_eq!(
        r.m.scores(1000),
        [(S.to_string(), 100)],
        "未解除的 danger 100 分"
    );
    r.feed(ok("t1", 1500));
    assert_eq!(
        r.m.scores(1500),
        [(S.to_string(), 0)],
        "解除後留在佇列等 ack 的 danger 不算分（使用者決定）"
    );
    // wait 在 tick 3000 發出，50 分起每分鐘 +5：再過 1 分 6 秒 → 55.5 → 四捨五入 56
    r.feed(pre("t2", "ls", 2000))
        .feed(ask("ls", 2000))
        .tick(3000);
    assert_eq!(r.attention().last(), Some(&a("raised", "wait", "t2")));
    assert_eq!(r.m.scores(3000 + 66_000), [(S.to_string(), 56)]);
}

#[test]
fn score_changes_do_not_emit_status_events() {
    let mut r = Run::new();
    r.feed(pre("t1", "ls", 1000))
        .feed(ask("ls", 1000))
        .tick(2000);
    assert_eq!(r.last_status(), "wait");
    let n = r.statuses().len();
    r.tick(2000 + 5 * MIN);
    assert_eq!(r.statuses().len(), n, "分數變了不發 session.status");
}

// ── 背景 agent 的結束通知（#677：被 TaskStop 停掉時沒有 SubagentStop）

fn notification(task_id: &str, status: Option<&str>) -> String {
    let status = status
        .map(|s| format!("<status>{s}</status>\n"))
        .unwrap_or_default();
    format!(
        "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>toolu_x</tool-use-id>\n\
         <output-file>/tmp/out</output-file>\n{status}<summary>Agent \"x\" was stopped</summary>\n</task-notification>"
    )
}

#[test]
fn task_notification_is_parsed() {
    use coding_lab::status::parse_task_notification;
    let n = parse_task_notification(&notification("a1", Some("killed"))).unwrap();
    assert_eq!(
        (
            n.task_id.as_str(),
            n.tool_use_id.as_deref(),
            n.status.as_deref()
        ),
        ("a1", Some("toolu_x"), Some("killed"))
    );
    let n = parse_task_notification(&notification("a1", None)).unwrap();
    assert_eq!(n.status, None, "沒有 status 也能解析");
    assert!(
        parse_task_notification("<task-notification></task-notification>").is_none(),
        "沒有 task-id"
    );
    assert!(parse_task_notification("一般的 prompt").is_none());
}

#[test]
fn task_failed_only_for_statuses_other_than_completed_and_killed() {
    use coding_lab::status::task_failed;
    assert!(!task_failed(Some("completed")));
    assert!(
        !task_failed(Some("killed")),
        "主動停掉不算失敗（使用者決定）"
    );
    assert!(task_failed(Some("failed")));
    assert!(!task_failed(None), "不知道就不算");
}

fn subagent_start(id: &str, ts: u64) -> Input {
    hook(
        "SubagentStart",
        json!({ "agent_id": id, "agent_type": "general-purpose" }),
        ts,
    )
}

fn subagent_stop(id: &str, ts: u64) -> Input {
    hook(
        "SubagentStop",
        json!({ "agent_id": id, "agent_type": "general-purpose" }),
        ts,
    )
}

fn finished(r: &Run) -> Vec<(String, Option<String>)> {
    r.evs
        .iter()
        .filter(|e| e.type_name() == "agent.finished")
        .map(|e| {
            (
                e.payload["agentId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                e.payload["status"].as_str().map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn killed_background_agent_is_finished_by_its_notification() {
    let mut r = Run::new();
    r.feed(subagent_start("a1", 1000))
        .feed(prompt(&notification("a1", Some("killed")), 2000));
    assert_eq!(
        finished(&r),
        [("a1".to_string(), Some("killed".to_string()))]
    );
}

#[test]
fn notification_after_subagent_stop_does_not_finish_twice() {
    let mut r = Run::new();
    r.feed(subagent_start("a1", 1000))
        .feed(subagent_stop("a1", 1500))
        .feed(prompt(&notification("a1", Some("completed")), 2000));
    assert_eq!(finished(&r), [("a1".to_string(), None)]);
}

#[test]
fn notification_for_unknown_task_is_ignored() {
    // 背景 Bash（shell task）的通知也長這樣，task-id 不是 agent
    let mut r = Run::new();
    r.feed(prompt(&notification("bz0e4vabl", Some("completed")), 2000));
    assert!(finished(&r).is_empty());
}

#[test]
fn samples_killed_background_agent_is_finished() {
    // cc94acff：general-purpose 背景 agent 啟動後被 TaskStop 停掉，沒有 SubagentStop
    let Some(evs) = replay_sample("cc94acff") else {
        eprintln!("略過：沒有 samples/sessions/cc94acff");
        return;
    };
    let spawned: Vec<&str> = evs
        .iter()
        .filter(|e| e.type_name() == "agent.spawned")
        .filter_map(|e| e.payload["agentId"].as_str())
        .collect();
    assert!(!spawned.is_empty());
    for id in spawned {
        let fin: Vec<&Event> = evs
            .iter()
            .filter(|e| e.type_name() == "agent.finished" && e.payload["agentId"] == id)
            .collect();
        assert_eq!(fin.len(), 1, "{id} 要結束且只結束一次");
        assert_eq!(fin[0].payload["status"], "killed");
    }
}

// ── 分數訊號：subagent 失敗（#674，失敗定義見 contracts.md §4）

fn agent_pre(id: &str, ts: u64) -> Input {
    hook(
        "PreToolUse",
        json!({ "tool_name": "Agent", "tool_input": { "subagent_type": "no-such-agent", "prompt": "x" }, "tool_use_id": id }),
        ts,
    )
}

fn agent_fail(id: &str, interrupt: bool, ts: u64) -> Input {
    hook(
        "PostToolUseFailure",
        json!({ "tool_name": "Agent", "tool_use_id": id, "duration_ms": 5,
                "error": "Agent type 'no-such-agent' not found.", "is_interrupt": interrupt }),
        ts,
    )
}

fn score_of(r: &Run, now: u64) -> i64 {
    r.m.scores(now).first().map_or(0, |(_, v)| *v)
}

#[test]
fn agent_tool_failure_adds_subagent_fail_signal() {
    let mut r = Run::new();
    r.feed(agent_pre("t1", 1000))
        .feed(agent_fail("t1", false, 1100));
    assert_eq!(score_of(&r, 1100), 20, "subagent 失敗 +20");
    assert_eq!(score_of(&r, 1100 + 10 * MIN), 10, "半衰期 10 分鐘");
    assert!(r.attention().is_empty(), "不進注意佇列");
    assert_ne!(r.last_status(), "fail", "不改 session.status");
}

#[test]
fn interrupted_agent_tool_is_not_a_subagent_failure() {
    let mut r = Run::new();
    r.feed(agent_pre("t1", 1000))
        .feed(agent_fail("t1", true, 1100));
    assert_eq!(score_of(&r, 1100), 0);
}

#[test]
fn notification_status_decides_subagent_failure() {
    for (status, want) in [("failed", 20), ("completed", 0), ("killed", 0)] {
        let mut r = Run::new();
        r.feed(subagent_start("a1", 1000))
            .feed(prompt(&notification("a1", Some(status)), 2000));
        assert_eq!(score_of(&r, 2000), want, "status {status}");
    }
    // 背景 Bash（不是 agent）失敗不算 subagent 失敗
    let mut r = Run::new();
    r.feed(prompt(&notification("bz0e4vabl", Some("failed")), 2000));
    assert_eq!(score_of(&r, 2000), 0);
}

#[test]
fn samples_agent_launch_failure_counts_once_and_killed_does_not() {
    // cc94acff：Agent 類型不存在（PostToolUseFailure）＋背景 agent 被 TaskStop（killed）
    let Some((m, _, end)) = replay_merger("cc94acff") else {
        eprintln!("略過：沒有 samples/sessions/cc94acff");
        return;
    };
    let score = m.scores(end).first().map_or(0, |(_, v)| *v);
    assert_eq!(score, 20, "啟動失敗算一次，killed 不算");
}

// ── 分數訊號：單 session 費用超過日均 3 倍（#675，方案 C）

const HOUR: u64 = 60 * MIN;
const DAY_MS: u64 = 24 * HOUR;
/// 2026-10-16 00:00 UTC
const NOW: u64 = 1_792_108_800_000;

fn u(session: &str, at: u64, usd: f64) -> (String, u64, f64) {
    (session.to_string(), at, usd)
}

#[test]
fn cost_baseline_needs_three_days() {
    use coding_lab::status::cost_baseline;
    let two_days = [u("a", NOW - 2 * DAY_MS, 1.0), u("b", NOW - 3 * DAY_MS, 1.0)];
    assert_eq!(cost_baseline(&two_days, NOW), None, "不足 3 天不算");
    let three = [
        u("a", NOW - 2 * DAY_MS, 1.0),
        u("b", NOW - 3 * DAY_MS, 2.0),
        u("c", NOW - 4 * DAY_MS, 3.0),
    ];
    assert_eq!(cost_baseline(&three, NOW), Some(2.0));
}

#[test]
fn cost_baseline_averages_session_days() {
    use coding_lab::status::cost_baseline;
    let rows = [
        // a 在同一天兩筆 → 一個 (session, 日)，4.0
        u("a", NOW - 2 * DAY_MS, 1.0),
        u("a", NOW - 2 * DAY_MS + HOUR, 3.0),
        // a 跨到另一天 → 另一組，2.0
        u("a", NOW - 3 * DAY_MS, 2.0),
        u("b", NOW - 4 * DAY_MS, 6.0),
        // 最近 24 小時不算進基準
        u("c", NOW - HOUR, 100.0),
        // 超過 7 天（從 24 小時前往回算）不算
        u("d", NOW - 9 * DAY_MS, 100.0),
        // 0 元的不算一組
        u("e", NOW - 5 * DAY_MS, 0.0),
    ];
    assert_eq!(cost_baseline(&rows, NOW), Some(4.0), "(4 + 2 + 6) / 3");
}

#[test]
fn cost_spike_fires_once_per_day_above_three_times_baseline() {
    use coding_lab::status::Tracker;
    let mut t = Tracker::default();
    let base = Some(1.0);
    t.cost(NOW, 2.0, base);
    assert_eq!(t.score(NOW), 0.0, "2 ≤ 3，不觸發");
    t.cost(NOW + MIN, 1.5, base);
    assert_eq!(t.score(NOW + MIN), 15.0, "累計 3.5 > 3，+15");
    t.cost(NOW + 2 * MIN, 5.0, base);
    assert!(
        t.score(NOW + 2 * MIN) < 15.0,
        "24 小時內不重複，只剩衰減的那一次"
    );
    // 25 小時後：之前的花費滾出 24 小時，要重新累計才會再觸發
    let later = NOW + 25 * HOUR;
    t.cost(later, 1.0, base);
    assert!(t.score(later) < 1.0, "舊的花費不算，1 ≤ 3");
    t.cost(later + MIN, 3.0, base);
    assert!(t.score(later + MIN) >= 15.0, "隔天可以再觸發");
}

#[test]
fn cost_spike_needs_a_baseline_and_seed_does_not_fire() {
    use coding_lab::status::Tracker;
    let mut t = Tracker::default();
    t.cost(NOW, 100.0, None);
    assert_eq!(t.score(NOW), 0.0, "沒有基準（不足 3 天）不觸發");

    let mut t = Tracker::default();
    t.seed_cost(NOW - HOUR, 100.0);
    assert_eq!(t.score(NOW), 0.0, "重啟時補回的花費不觸發");
    t.cost(NOW, 0.01, Some(1.0));
    assert_eq!(t.score(NOW), 15.0, "補回的花費算進滾動區間");
}

#[test]
fn samples_usage_feeds_cost_spike_with_a_baseline() {
    // dba6a637 有多筆 usage：基準設得很小就一定會觸發一次，沒有基準就不會
    let Some((plain, _, end)) = replay_with("dba6a637", None) else {
        eprintln!("略過：沒有 samples/sessions/dba6a637");
        return;
    };
    let (spiky, _, _) = replay_with("dba6a637", Some(0.000_001)).unwrap();
    let score = |m: &Merger| m.scores(end).first().map_or(0, |(_, v)| *v);
    let diff = score(&spiky) - score(&plain);
    assert!(
        (1..=15).contains(&diff),
        "多了一次 CostSpike（衰減後）：{diff}"
    );
}

#[test]
fn seeded_cost_survives_the_next_tick() {
    // 重啟時補回的花費不能被下一次 tick 的 evict 清掉（新建的 session last_seen 是 0）
    let mut m = Merger::new(Box::new(NoDanger));
    m.seed_cost(S, NOW - HOUR, 100.0);
    m.tick(NOW);
    assert!(
        m.scores(NOW).iter().any(|(id, _)| id == S),
        "補回花費的 session 還在 merge 裡"
    );
}

// ── resume、重啟與被丟棄的 session（#679）

fn start(source: &str, ts: u64) -> Input {
    hook("SessionStart", json!({ "source": source, "cwd": "/p" }), ts)
}

fn end(ts: u64) -> Input {
    hook("SessionEnd", json!({ "reason": "exit" }), ts)
}

#[test]
fn resume_after_end_reports_status_again() {
    let mut r = Run::with_rules();
    r.feed(start("startup", 1_000))
        .feed(end(2_000))
        .feed(start("resume", 3_000))
        .feed(pre("t1", "rm -rf build", 4_000));
    assert_eq!(r.statuses(), ["ended", "run", "danger"]);
}

#[test]
fn resume_after_eviction_still_clears_old_done() {
    let mut r = Run::new();
    r.feed(start("startup", 1_000))
        .feed(prompt("hi", 2_000))
        .feed(stop(3_000))
        .tick(9_000)
        .feed(end(10_000))
        .tick(10_000 + 11 * 60 * 1000); // 結束 10 分鐘後移出記憶體
    assert_eq!(r.m.session_count(), 0, "前提：已移出記憶體");
    r.feed(start("resume", 20 * 60 * 1000))
        .feed(prompt("again", 21 * 60 * 1000));
    assert_eq!(
        r.attention(),
        [a("raised", "done", "p2000"), a("cleared", "done", "p2000")]
    );
    assert_eq!(r.last_status(), "run");
}

#[test]
fn restart_seeded_attention_is_cleared_like_before() {
    // collector 重啟：merge 只拿到 store 的計數與還開著的注意項目
    let mut r = Run::with_rules();
    r.m.resume(S, 40, 3, "/p");
    r.m.resume_attention(S, AttentionKind::Done, "p1", "main", 9_000);
    r.m.resume_attention(S, AttentionKind::Danger, "t9", "main", 9_500);
    r.m.resume_attention(S, AttentionKind::Fail, "t8", "main", 9_600);
    // t9 的 Bash 成功同時結束 danger、讓 main 的連續失敗歸零；新的 prompt 撤回 done
    r.feed(ok("t9", 20_000)).feed(prompt("again", 21_000));
    assert_eq!(
        r.attention(),
        [
            a("cleared", "danger", "t9"),
            a("cleared", "fail", "t8"),
            a("cleared", "done", "p1"),
        ]
    );
    assert_eq!(r.last_status(), "run");
    assert!(r.evs.iter().all(|e| e.seq > 40), "seq 接著 store 的編號");
}

#[test]
fn abandoned_session_is_ended_when_evicted() {
    let mut r = Run::new();
    r.feed(start("startup", 1_000))
        .feed(prompt("hi", 2_000))
        .tick(2_000 + 24 * 60 * 60 * 1000);
    let ended: Vec<&Event> = r
        .evs
        .iter()
        .filter(|e| e.type_name() == "session.ended")
        .collect();
    assert_eq!(
        ended.len(),
        1,
        "沒有 SessionEnd 的 session 被丟棄時要結束它"
    );
    assert_eq!(ended[0].payload["reason"], "abandoned");
    assert_eq!(r.last_status(), "ended");
    assert_eq!(r.m.session_count(), 0);
}

#[test]
fn ended_session_is_not_abandoned_again() {
    let mut r = Run::new();
    r.feed(start("startup", 1_000))
        .feed(end(2_000))
        .tick(2_000 + 24 * 60 * 60 * 1000);
    let ended = r
        .evs
        .iter()
        .filter(|e| e.type_name() == "session.ended")
        .count();
    assert_eq!(ended, 1, "已經結束的 session 不再補一次");
}
