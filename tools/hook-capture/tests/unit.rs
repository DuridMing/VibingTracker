use std::io::BufReader;

use hook_capture::http::read_request;
use hook_capture::settings::{EVENTS, settings_json};
use hook_capture::splitter::LineSplitter;
use hook_capture::store::{Via, record_hook, sanitize};

#[test]
fn splitter_keeps_partial_line_until_newline() {
    let mut s = LineSplitter::default();
    assert!(s.push(b"{\"a\":1}\n{\"b\"").len() == 1);
    assert_eq!(s.pending_len(), 4);
    let lines = s.push(b":2}\n");
    assert_eq!(lines, vec![b"{\"b\":2}".to_vec()]);
    assert_eq!(s.pending_len(), 0);
}

#[test]
fn splitter_handles_many_lines_in_one_read() {
    let mut s = LineSplitter::default();
    assert_eq!(s.push(b"1\n2\n3\n").len(), 3);
}

#[test]
fn reads_content_length_request() {
    let raw = b"POST /hook HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
    let req = read_request(&mut BufReader::new(&raw[..])).unwrap();
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/hook");
    assert_eq!(req.body, b"{\"a\":1}");
}

#[test]
fn reads_chunked_request() {
    let raw = b"POST /hook HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4\r\n\":1}\r\n0\r\n\r\n";
    let req = read_request(&mut BufReader::new(&raw[..])).unwrap();
    assert_eq!(req.body, b"{\"a\":1}");
}

#[test]
fn settings_hook_every_event_both_ways() {
    assert!(EVENTS.contains(&"PreToolUse") && EVENTS.contains(&"SessionEnd"));
    let v = settings_json(4899, "/bin/hook-capture");
    for ev in EVENTS {
        let hooks = &v["hooks"][ev][0]["hooks"];
        let types: Vec<&str> = hooks
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["http", "command"], "{ev}");
        assert_eq!(hooks[0]["url"], "http://127.0.0.1:4899/hook");
        assert_eq!(hooks[1]["command"], "/bin/hook-capture cmd");
    }
}

#[test]
fn sanitize_blocks_path_traversal() {
    assert_eq!(sanitize("../../etc"), "______etc");
    assert_eq!(sanitize("abc-123_X"), "abc-123_X");
    assert_eq!(sanitize(""), "unknown");
}

#[test]
fn record_hook_appends_with_arrival_time() {
    let dir = tempdir();
    let body = br#"{"session_id":"s1","hook_event_name":"Stop"}"#;
    let sid = record_hook(&dir, Via::Http, body).unwrap();
    assert_eq!(sid, "s1");
    record_hook(&dir, Via::Command, body).unwrap();
    let text = std::fs::read_to_string(dir.join("s1/hooks.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["via"], "http");
    assert_eq!(rows[1]["via"], "command");
    assert_eq!(rows[0]["event"], "Stop");
    assert!(rows[0]["ts_seen_ms"].as_u64().unwrap() > 0);
    assert_eq!(rows[0]["payload"]["session_id"], "s1");
}

#[test]
fn record_hook_keeps_unparseable_body() {
    let dir = tempdir();
    let sid = record_hook(&dir, Via::Http, b"not json").unwrap();
    assert_eq!(sid, "unknown");
    let text = std::fs::read_to_string(dir.join("unknown/hooks.jsonl")).unwrap();
    assert!(text.contains("not json"));
}

pub fn tempdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hook-capture-test-{}-{}",
        std::process::id(),
        hook_capture::now_ms()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}
