//! 敏感輸出（#654）：每條規則都有命中與不命中的案例。

use coding_lab::sensitive::{is_sensitive, redact_keys, redact_remote};
use serde_json::{Value, json};

fn read(path: &str) -> bool {
    is_sensitive("Read", &json!({ "file_path": path }))
}

fn bash(cmd: &str) -> bool {
    is_sensitive("Bash", &json!({ "command": cmd }))
}

#[test]
fn env_files_hit_but_templates_and_code_do_not() {
    for p in [
        "/p/.env",
        "/p/.env.local",
        "/p/.env.production",
        "C:\\p\\.env",
    ] {
        assert!(read(p), "{p}");
    }
    for p in [
        "/p/.env.example",
        "/p/.env.sample",
        "/p/.env.template",
        "/p/src/env.rs",
        "/p/.envrc.md",
    ] {
        assert!(!read(p), "{p}");
    }
}

#[test]
fn keys_and_credential_files_hit() {
    for p in [
        "/home/u/.ssh/id_rsa",
        "/home/u/.ssh/id_ed25519",
        "/p/certs/server.pem",
        "/p/tls/server.key",
        "/p/store.p12",
        "/home/u/.aws/credentials",
        "/home/u/.netrc",
        "/home/u/.npmrc",
        "/home/u/.pypirc",
        "/home/u/.kube/config",
        "/p/config/secrets.yaml",
        "/p/gcp-credentials.json",
    ] {
        assert!(read(p), "{p}");
    }
    for p in [
        "/home/u/.ssh/id_rsa.pub",
        "/home/u/.ssh/known_hosts",
        "/p/src/keyboard.rs",
        "/p/docs/secrets-handling.md",
        "/p/src/credentials.rs",
    ] {
        assert!(!read(p), "{p}");
    }
}

#[test]
fn other_file_tools_use_their_path_fields() {
    assert!(is_sensitive(
        "Write",
        &json!({ "file_path": "/p/.env", "content": "A=1" })
    ));
    assert!(is_sensitive(
        "Edit",
        &json!({ "file_path": "/p/.env", "old_string": "a", "new_string": "b" })
    ));
    assert!(is_sensitive(
        "Grep",
        &json!({ "pattern": "KEY", "path": "/p/.env" })
    ));
    assert!(!is_sensitive(
        "Grep",
        &json!({ "pattern": ".env", "path": "/p/src" })
    ));
    assert!(
        !is_sensitive("Glob", &json!({ "pattern": "**/.env" })),
        "Glob 只列檔名"
    );
}

#[test]
fn bash_reading_secrets_or_dumping_the_environment_hits() {
    for c in [
        "cat .env",
        "cd app && cat ./config/.env.local",
        "grep TOKEN .env",
        "head -5 ~/.aws/credentials",
        "printenv",
        "printenv GITHUB_TOKEN",
        "env",
        "env | sort",
        "export -p",
        "sudo cat /root/.ssh/id_rsa",
    ] {
        assert!(bash(c), "{c}");
    }
    for c in [
        "cargo test",
        "echo env",
        "env RUST_LOG=debug cargo run",
        "cat .env.example",
        "ls -la",
        "git diff",
        "cat src/env.rs",
    ] {
        assert!(!bash(c), "{c}");
    }
}

#[test]
fn redact_remote_blanks_sensitive_results_anywhere_in_a_frame() {
    let ev = |sensitive: bool| {
        json!({"type": "tool.result", "payload": {"toolUseId": "t", "ok": true, "output": "A=secret",
               "truncated": false, "sensitive": sensitive}})
    };
    let mut frame = json!({"t": "frame", "events": [ev(true), ev(false)],
                           "stream": [ev(true)], "nested": {"events": [ev(true)]}});
    redact_remote(&mut frame);
    let out = |v: &Value| {
        (
            v["payload"]["output"].clone(),
            v["payload"]["redacted"].clone(),
        )
    };
    assert_eq!(out(&frame["events"][0]), (json!(""), json!(true)));
    assert_eq!(
        out(&frame["events"][1]),
        (json!("A=secret"), Value::Null),
        "沒命中的不動"
    );
    assert_eq!(out(&frame["stream"][0]), (json!(""), json!(true)));
    assert_eq!(out(&frame["nested"]["events"][0]), (json!(""), json!(true)));
}

/// #720：遠端送出的副本裡，這個系統自己發的金鑰（`clk_<8 碼>_<43 碼>`）不論出現在哪個字串欄位都只留 id。
#[test]
fn redact_keys_masks_the_secret_half_of_every_clk_token_anywhere() {
    let k1 = "clk_TestKey1_FakeSecretForTestsOnly000000000000000000000";
    let k2 = "clk_TestKey2_AnotherFakeSecretForTests111111111111111111";
    let mut v = json!({
        "events": [
            {"type": "assistant.text", "payload": {"text": format!("金鑰是 {k1}，另一把 {k2}。")}},
            {"type": "tool.called", "payload": {"tool": "Bash", "input": {"command": format!("curl -H 'Authorization: Bearer {k1}' http://x")}}},
            {"type": "tool.result", "payload": {"output": format!("{{\"token\":\"{k2}\"}}"), "sensitive": false}},
            {"type": "prompt.submitted", "payload": {"text": format!("用 {k1} 登入")}},
        ],
        "rows": {"sessions": [{"id": "s", "doing": format!("正在貼 {k1}")}],
                 "agents": [{"id": "a", "current": k2}]},
        // 形狀不對的不動：id 不是 8 碼、secret 太短、不是 clk_ 開頭
        "keep": ["clk_abc_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "clk_abcdefgh_short", "xlk_abcdefgh_ffffffffffffffffffffffffffffffffffffffffffff"]
    });
    redact_keys(&mut v);
    let text = v.to_string();
    assert!(
        !text.contains("FakeSecretForTestsOnly000000000000000000000"),
        "{text}"
    );
    assert!(
        !text.contains("AnotherFakeSecretForTests111111111111111111"),
        "{text}"
    );
    assert_eq!(
        v["events"][0]["payload"]["text"],
        "金鑰是 clk_TestKey1_…，另一把 clk_TestKey2_…。"
    );
    assert_eq!(
        v["events"][1]["payload"]["input"]["command"],
        "curl -H 'Authorization: Bearer clk_TestKey1_…' http://x"
    );
    assert_eq!(v["rows"]["agents"][0]["current"], "clk_TestKey2_…");
    assert_eq!(v["rows"]["sessions"][0]["doing"], "正在貼 clk_TestKey1_…");
    assert_eq!(
        v["keep"][0],
        "clk_abc_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    );
    assert_eq!(v["keep"][1], "clk_abcdefgh_short");
    assert_eq!(
        v["keep"][2],
        "xlk_abcdefgh_ffffffffffffffffffffffffffffffffffffffffffff"
    );
    // 已經遮過的再遮一次不變
    let again = v.clone();
    redact_keys(&mut v);
    assert_eq!(v, again);
}

// ── PowerShell（W-H3）：欄位名假設和 Bash 一樣是 `command`，尚未經實機確認（排在第 1 輪 Windows 驗收）

fn ps(cmd: &str) -> bool {
    is_sensitive("PowerShell", &json!({ "command": cmd }))
}

#[test]
fn powershell_reading_secrets_or_listing_the_environment_hits() {
    for c in [
        "Get-Content .env",
        "gc .env.local",
        "type .env",
        "cat .env",
        "Get-Content -Path C:\\proj\\.env",
        "Get-Content -Raw 'C:\\Users\\me\\.aws\\credentials'",
        "Set-Location app; Get-Content config\\.env.production",
        "Get-Content .e`nv",
        "gc `\n  .env",
        "Get-ChildItem env:",
        "gci Env:",
        "dir env:",
        "ls env:*",
        "Get-Item env:GITHUB_TOKEN",
        "Get-Content env:AWS_SECRET_ACCESS_KEY",
        "$env:GITHUB_TOKEN",
        "[Environment]::GetEnvironmentVariables()",
        "[System.Environment]::GetEnvironmentVariables() | Format-List",
    ] {
        assert!(ps(c), "{c}");
    }
    for c in [
        "Get-Content README.md",
        "Get-Content .env.example",
        "Get-ChildItem src",
        "$env:RUST_LOG = 'debug'; cargo test",
        "Write-Output 'env:'",
        "cargo test",
        "git status",
        "Get-Content src\\env.rs",
    ] {
        assert!(!ps(c), "{c}");
    }
}

#[test]
fn powershell_without_a_command_field_is_not_sensitive() {
    assert!(!is_sensitive("PowerShell", &json!({})));
    assert!(!is_sensitive(
        "PowerShell",
        &json!({ "script": "Get-Content .env" })
    ));
}
