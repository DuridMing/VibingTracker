//! plugin 的結構驗證與 wrapper 行為（#628）。hooks.json 必須和契約 §2 訂閱的事件一致。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use coding_lab::config::DEFAULT_HOOK_BIND;
use coding_lab::hooks::HOOK_PATH;
use serde_json::Value;

/// contracts.md §2「訂閱的事件」：SessionStart 只能用 command hook（ensure 轉送），其餘用 http。
const HTTP_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "MessageDisplay",
    "PreCompact",
    "PostCompact",
    "SessionEnd",
];

fn plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin")
}

fn read_json(rel: &str) -> Value {
    let p = plugin_dir().join(rel);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_str(&text).unwrap()
}

/// 每個事件底下的所有 handler。
fn handlers(hooks: &Value, event: &str) -> Vec<Value> {
    hooks[event]
        .as_array()
        .unwrap_or_else(|| panic!("hooks.json 沒有 {event}"))
        .iter()
        .flat_map(|m| m["hooks"].as_array().unwrap().clone())
        .collect()
}

#[test]
fn manifest_name_and_version_match_the_crate() {
    let m = read_json(".claude-plugin/plugin.json");
    assert_eq!(m["name"], "coding-lab");
    assert_eq!(
        m["version"],
        env!("CARGO_PKG_VERSION"),
        "plugin 與二進位同版號"
    );
    assert!(
        m.get("hooks").is_none(),
        "hooks 只放 hooks/hooks.json，避免載入兩次"
    );
}

#[test]
fn hooks_json_subscribes_exactly_the_contract_events() {
    let root = read_json("hooks/hooks.json");
    let hooks = root["hooks"].as_object().expect("外層要包一層 \"hooks\"");
    let mut got: Vec<&str> = hooks.keys().map(String::as_str).collect();
    let mut want: Vec<&str> = HTTP_EVENTS.to_vec();
    want.push("SessionStart");
    got.sort();
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn http_hooks_post_to_the_loopback_hook_port_with_a_short_timeout() {
    let root = read_json("hooks/hooks.json");
    let url = format!("http://{DEFAULT_HOOK_BIND}{HOOK_PATH}");
    for event in HTTP_EVENTS {
        let hs = handlers(&root["hooks"], event);
        assert_eq!(hs.len(), 1, "{event}：只掛一個 handler");
        let h = &hs[0];
        assert_eq!(h["type"], "http", "{event}");
        assert_eq!(h["url"], url.as_str(), "{event}");
        let t = h["timeout"].as_u64().unwrap_or(600);
        assert!(
            t <= 2,
            "{event}：collector 沒跑時不能拖慢 Claude Code（timeout {t}）"
        );
        assert!(h.get("headers").is_none(), "{event}：hook 入口不驗金鑰");
    }
}

#[test]
fn session_start_runs_ensure_through_the_wrapper() {
    let root = read_json("hooks/hooks.json");
    let hs = handlers(&root["hooks"], "SessionStart");
    assert_eq!(hs.len(), 1);
    let h = &hs[0];
    assert_eq!(h["type"], "command");
    // 路徑有空白也要能跑：${CLAUDE_PLUGIN_ROOT} 要加引號
    assert_eq!(
        h["command"],
        "\"${CLAUDE_PLUGIN_ROOT}/bin/coding-lab\" ensure"
    );
    assert!(h["timeout"].as_u64().is_some_and(|t| t <= 10));
    assert!(plugin_dir().join("bin/coding-lab").is_file());
}

#[test]
fn no_hook_can_inject_context_or_block() {
    let text = std::fs::read_to_string(plugin_dir().join("hooks/hooks.json")).unwrap();
    for word in [
        "additionalContext",
        "systemMessage",
        "decision",
        "prompt",
        "agent",
    ] {
        assert!(
            !text.contains(&format!("\"{word}\"")),
            "零 token、不攔截：hooks.json 不能有 {word}"
        );
    }
}

// ── wrapper（POSIX sh；Windows 的 Git Bash 也走這支）

#[cfg(unix)]
fn wrapper(envs: &[(&str, &Path)]) -> std::process::Output {
    let home =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("plugin-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    let mut c = Command::new(plugin_dir().join("bin/coding-lab"));
    c.arg("ensure")
        .env_remove("CODING_LAB_BIN")
        .env("CODING_LAB_HOME", &home)
        .env("PATH", "/usr/bin:/bin") // PATH 上沒有 coding-lab
        .stdin(Stdio::null());
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().unwrap()
}

#[cfg(unix)]
#[test]
fn wrapper_without_a_binary_exits_zero_and_prints_instructions_to_stderr() {
    let out = wrapper(&[]);
    assert!(out.status.success(), "不能讓 SessionStart 失敗");
    assert!(out.stdout.is_empty(), "stdout 會進 Claude 的上下文");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("coding-lab") && err.contains("cargo install"),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn wrapper_runs_the_binary_and_keeps_stdout_empty() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("plugin-bin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("coding-lab");
    // 假二進位：把參數寫到 stdout（wrapper 要把它擋掉）與檔案
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\"\necho \"$@\" > '{}/args'\n",
            dir.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = wrapper(&[("CODING_LAB_BIN", &fake)]);
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("args")).unwrap().trim(),
        "ensure"
    );

    // 沒有 CODING_LAB_BIN 時找 $CODING_LAB_HOME/bin/coding-lab
    let home =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("plugin-home-{}", std::process::id()));
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::copy(&fake, home.join("bin/coding-lab")).unwrap();
    std::fs::remove_file(dir.join("args")).unwrap();
    let out = wrapper(&[("CODING_LAB_HOME", &home)]);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(
        dir.join("args").exists(),
        "要用 $CODING_LAB_HOME/bin 的二進位"
    );
}
