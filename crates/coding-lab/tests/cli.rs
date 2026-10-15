use std::process::Command;

fn coding_lab() -> Command {
    Command::new(env!("CARGO_BIN_EXE_coding-lab"))
}

#[test]
fn version_matches_cargo_toml() {
    let out = coding_lab().arg("--version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        format!("coding-lab {}", env!("CARGO_PKG_VERSION"))
    );
}

/// `ensure` 由 SessionStart hook 呼叫：不論成功與否都不能讓 Claude Code 看到失敗。
#[test]
fn ensure_always_exits_zero() {
    let out = coding_lab().arg("ensure").output().unwrap();
    assert!(out.status.success());
}

#[test]
fn unknown_subcommand_fails() {
    let out = coding_lab().arg("no-such-command").output().unwrap();
    assert!(!out.status.success());
}
