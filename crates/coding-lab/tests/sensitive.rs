//! 敏感輸出（#654）：每條規則都有命中與不命中的案例。

use coding_lab::sensitive::{is_sensitive, redact_remote};
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
