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

/// 壞掉的 config.toml 與它所在的資料目錄；HOME 也指到暫存目錄，預設的 ~/.coding-lab 不會是真的那個。
fn broken_config(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("data");
    let user = root.join("user");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(home.join("config.toml"), "[hook\nbind = 1\n").unwrap();
    (home, user)
}

fn with_broken_config(
    home: &std::path::Path,
    user: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    coding_lab()
        .args(args)
        .env("CODING_LAB_HOME", home)
        .env("HOME", user)
        .env("USERPROFILE", user)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// E-D7（#774）：設定壞掉時每個子命令都 exit 2（原本 hub、service、key 是 1，和其他的不一致）。
#[test]
fn every_subcommand_exits_2_when_the_config_is_broken() {
    let (home, user) = broken_config("exit-codes");
    for args in [
        &["config"][..],
        &["stop"],
        &["healthz"],
        &["prune"],
        &["serve"],
        &["key", "list"],
        &["service", "status"],
        &["hub", "add", "a", "http://127.0.0.1:1", "--no-key"],
    ] {
        let out = with_broken_config(&home, &user, args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("config.toml"),
            "{args:?} 要說是設定的問題"
        );
    }
}

/// append_log（#774）：設定壞掉時 ensure 的錯誤原本退回 `default_home()`，不看 CODING_LAB_HOME，
/// 寫進真正的 ~/.coding-lab/collector.log。
#[test]
fn ensure_logs_a_broken_config_into_coding_lab_home() {
    let (home, user) = broken_config("ensure-log");
    let out = with_broken_config(&home, &user, &["ensure"]);
    assert!(out.status.success() && out.stdout.is_empty());
    let log = std::fs::read_to_string(home.join("collector.log")).unwrap_or_default();
    assert!(
        log.contains("ensure") && log.contains("config.toml"),
        "{log}"
    );
    assert!(!user.join(".coding-lab").exists(), "不能寫到預設的 home");
}
