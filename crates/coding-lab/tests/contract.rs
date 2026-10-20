//! 資料契約測試（docs/plan/contracts.md）。

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use coding_lab::event::{EVENT_TYPES, Event};
use coding_lab::hook::HookInput;
use serde_json::{Value, json};

const FIXTURES: &str = include_str!("fixtures/events.jsonl");

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[test]
fn every_event_type_roundtrips() {
    for line in FIXTURES.lines() {
        let ev = Event::from_json_line(line).unwrap_or_else(|e| panic!("{e:?}: {line}"));
        assert_eq!(json(&ev.to_json_line()), json(line), "round-trip 不一致");
        assert_eq!(ev.type_name(), json(line)["type"]);
    }
}

/// event.rs 的型別與 contracts.md 一致：每種 payload 轉成型別再轉回 JSON，要與範例完全相同。
#[test]
fn typed_payload_matches_contract_fields() {
    for line in FIXTURES.lines() {
        let ev = Event::from_json_line(line).unwrap();
        let typed = ev
            .payload_typed()
            .unwrap_or_else(|e| panic!("{e:?}: {line}"))
            .unwrap_or_else(|| panic!("契約的 type 不該是未知：{line}"));
        assert_eq!(
            serde_json::to_value(&typed).unwrap(),
            json(line)["payload"],
            "{line}"
        );
    }
}

#[test]
fn fixtures_cover_every_contract_type() {
    let seen: BTreeSet<String> = FIXTURES
        .lines()
        .map(|l| json(l)["type"].as_str().unwrap().to_string())
        .collect();
    let want: BTreeSet<String> = EVENT_TYPES.iter().map(|s| s.to_string()).collect();
    assert_eq!(seen, want);
}

#[test]
fn pos_is_null_until_placed() {
    let called = FIXTURES
        .lines()
        .find(|l| l.contains("\"tool.called\""))
        .unwrap();
    let placed = FIXTURES
        .lines()
        .find(|l| l.contains("\"tool.placed\""))
        .unwrap();
    assert_eq!(Event::from_json_line(called).unwrap().pos(), None);
    assert_eq!(Event::from_json_line(placed).unwrap().pos(), Some(3));
}

/// 讀取寬鬆：新版加的 type 與欄位，舊版讀了要原樣保留。
#[test]
fn unknown_type_and_fields_survive_roundtrip() {
    let mut v = json(FIXTURES.lines().next().unwrap());
    v["type"] = json!("future.thing");
    v["payload"] = json!({ "anything": [1, 2, 3] });
    v["newTopLevel"] = json!("x");
    let line = v.to_string();
    let ev = Event::from_json_line(&line).unwrap();
    assert_eq!(ev.type_name(), "future.thing");
    assert_eq!(json(&ev.to_json_line()), v);

    let mut called = json(
        FIXTURES
            .lines()
            .find(|l| l.contains("\"tool.called\""))
            .unwrap(),
    );
    called["payload"]["addedLater"] = json!(true);
    let ev = Event::from_json_line(&called.to_string()).unwrap();
    assert_eq!(json(&ev.to_json_line()), called);
}

#[test]
fn rejects_newer_contract_version() {
    let mut v = json(FIXTURES.lines().next().unwrap());
    v["v"] = json!(2);
    assert!(Event::from_json_line(&v.to_string()).is_err());
}

#[test]
fn raw_is_omitted_when_absent() {
    let line = FIXTURES
        .lines()
        .find(|l| l.contains("\"prompt.submitted\""))
        .unwrap();
    let out = json(&Event::from_json_line(line).unwrap().to_json_line());
    assert!(out.get("raw").is_none());
    assert!(out.get("pos").is_some(), "pos 一律輸出（可為 null）");
}

fn samples_hooks() -> Option<Vec<PathBuf>> {
    let dir = common::samples_root().join("hooks");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    Some(files)
}

/// 從 #624 移來；samples/ 不進 git，沒有就略過。
#[test]
fn every_sample_hook_parses() {
    let Some(files) = samples_hooks() else {
        eprintln!("略過：沒有 samples/hooks");
        return;
    };
    assert!(!files.is_empty());
    for f in files {
        let body = std::fs::read(&f).unwrap();
        let h = HookInput::from_json(&body).unwrap_or_else(|e| panic!("{}: {e:?}", f.display()));
        assert_eq!(h.event_name(), f.file_stem().unwrap().to_str().unwrap());
        assert!(!h.session_id().is_empty());
    }
}

#[test]
fn hook_tool_use_id_and_raw() {
    let pre = json!({
        "session_id": "s1", "hook_event_name": "PreToolUse", "transcript_path": "/t.jsonl",
        "cwd": "/p", "tool_name": "Bash", "tool_input": { "command": "ls" },
        "tool_use_id": "toolu_1", "brand_new_field": 42,
    });
    let h = HookInput::from_json(pre.to_string().as_bytes()).unwrap();
    assert_eq!(h.tool_use_id(), Some("toolu_1"));
    assert_eq!(h.raw().get("brand_new_field"), Some(&json!(42)));
    assert!(h.raw().get("tool_name").is_none(), "契約用到的欄位不進 raw");

    let req = json!({ "session_id": "s1", "hook_event_name": "PermissionRequest",
                      "tool_name": "Write", "tool_input": {} });
    assert_eq!(
        HookInput::from_json(req.to_string().as_bytes())
            .unwrap()
            .tool_use_id(),
        None
    );
}

#[test]
fn unsubscribed_hook_event_still_parses() {
    let v = json!({ "session_id": "s1", "hook_event_name": "FileChanged", "x": 1 });
    let h = HookInput::from_json(v.to_string().as_bytes()).unwrap();
    assert_eq!(h.event_name(), "FileChanged");
    assert_eq!(h.raw().get("x"), Some(&json!(1)));
}

#[test]
fn hook_without_session_id_is_rejected() {
    assert!(HookInput::from_json(br#"{"hook_event_name":"Stop"}"#).is_err());
    assert!(HookInput::from_json(b"not json").is_err());
}
