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

/// L-16（#750）：不是 UTF-8 的 CODING_LAB_HOME 原本被當成沒設、默默用 ~/.coding-lab；路徑本來就可以不是 UTF-8，照用。
#[cfg(unix)]
#[test]
fn a_non_utf8_coding_lab_home_is_used_not_ignored() {
    use std::os::unix::ffi::OsStrExt;
    let mut home = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("cli-home")
        .into_os_string();
    home.push(std::ffi::OsStr::from_bytes(b"-\xff"));
    let out = coding_lab()
        .arg("config")
        .env("CODING_LAB_HOME", &home)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(r"cli-home-\xFF"), "{stdout}");
}

/// L-16：config.toml 打錯的鍵原本被 serde 默默略過；`config` 子命令把警告印到 stderr。
#[test]
fn config_command_warns_about_unknown_keys() {
    let home = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli-unknown-keys");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[colector]\nidle_exit_seconds = 5\n",
    )
    .unwrap();
    let out = coding_lab()
        .arg("config")
        .env("CODING_LAB_HOME", &home)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("[colector]"), "{stderr}");
}
