use hook_capture::probe::edit_probe;
use serde_json::json;

fn tmpfile(name: &str, content: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hook-capture-probe-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(name);
    std::fs::write(&p, content).unwrap();
    p
}

fn edit(event: &str, path: &std::path::Path, old: &str, new: &str) -> serde_json::Value {
    json!({
        "hook_event_name": event, "tool_name": "Edit",
        "tool_input": { "file_path": path, "old_string": old, "new_string": new },
    })
}

#[test]
fn counts_old_and_new_and_first_line() {
    let p = tmpfile("a.py", "x = 1\ndef sub(a, b):\n    return a + b\n# a + b\n");
    let r = edit_probe(&edit("PreToolUse", &p, "a + b", "a - b")).unwrap();
    assert_eq!(r["exists"], true);
    assert_eq!(r["old_count"], 2);
    assert_eq!(r["new_count"], 0);
    assert_eq!(r["old_line"], 3);
}

#[test]
fn post_tool_use_also_probed() {
    let p = tmpfile("b.py", "y = a - b\n");
    let r = edit_probe(&edit("PostToolUse", &p, "a + b", "a - b")).unwrap();
    assert_eq!(
        (r["old_count"].as_u64(), r["new_count"].as_u64()),
        (Some(0), Some(1))
    );
    assert!(r["old_line"].is_null());
}

#[test]
fn missing_file() {
    let p = std::env::temp_dir().join("hook-capture-probe-does-not-exist.py");
    let r = edit_probe(&edit("PreToolUse", &p, "a", "b")).unwrap();
    assert_eq!(r["exists"], false);
}

#[test]
fn other_tools_and_events_are_ignored() {
    let p = tmpfile("c.py", "z\n");
    let mut w = edit("PreToolUse", &p, "z", "w");
    w["tool_name"] = json!("Write");
    assert!(edit_probe(&w).is_none());
    assert!(edit_probe(&edit("PermissionRequest", &p, "z", "w")).is_none());
}
