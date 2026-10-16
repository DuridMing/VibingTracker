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

// `ensure` 的行為（永遠 exit 0、stdout 為空）在 tests/serve.rs，那裡用暫存的資料目錄；
// 這裡不跑，否則會在真正的 ~/.coding-lab 拉起 collector。

#[test]
fn unknown_subcommand_fails() {
    let out = coding_lab().arg("no-such-command").output().unwrap();
    assert!(!out.status.success());
}
