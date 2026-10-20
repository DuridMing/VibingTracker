//! plugin 的結構驗證與 wrapper 行為（#628）。hooks.json 必須和契約 §2 訂閱的事件一致。

use std::path::{Path, PathBuf};
use std::process::Stdio;

#[path = "common/sh.rs"]
mod sh;
use sh::{path_env, sh_or_skip, to_sh, wrapper_cmd, write_script};

use coding_lab::config::DEFAULT_HOOK_BIND;
use coding_lab::hooks::{HOOK_PATH, USER_HEADER, USERNAME_HEADER};
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
        // collector 沒在跑時每個 hook 都等到逾時（Windows 連沒在聽的埠約 2 秒才回拒絕，#776 EW7）；
        // 單位是秒、文件沒說收不收小數，所以用整數 1
        assert!(
            t <= 1,
            "{event}：collector 沒跑時不能拖慢 Claude Code（timeout {t}）"
        );
        // hook 入口不驗金鑰：只帶使用者（#777，4800 被別的帳號的 collector 佔著時它丟掉這個帳號的 hook）
        assert_eq!(
            h["headers"],
            serde_json::json!({ USER_HEADER: "${USER}", USERNAME_HEADER: "${USERNAME}" }),
            "{event}"
        );
        assert_eq!(
            h["allowedEnvVars"],
            serde_json::json!(["USER", "USERNAME"]),
            "{event}：沒列進來的變數會變成空字串"
        );
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

// ── wrapper（POSIX sh）：Unix 用 /bin/sh、Windows 用 Git Bash 的 sh 跑同一套（#783，見 common/sh.rs）。
// 找不到 sh 的 Windows 印出原因後略過；CODING_LAB_REQUIRE_SH=1 時失敗。

fn tmp_dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("plugin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 沒有 coding-lab 在 PATH 上、不自動下載的 `wrapper ensure`。
fn wrapper(sh: &Path, home: &Path, envs: &[(&str, String)]) -> std::process::Output {
    let mut c = wrapper_cmd(sh);
    c.arg("ensure")
        .env_remove("CODING_LAB_BIN")
        .env("CODING_LAB_HOME", to_sh(home))
        .env("PATH", path_env(sh, &[])) // PATH 上沒有 coding-lab
        .env("CODING_LAB_RELEASE_URL", "") // 空字串＝不自動下載（預設會連 GitHub）
        .stdin(Stdio::null());
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().unwrap()
}

#[test]
fn wrapper_without_a_binary_exits_zero_and_prints_instructions_to_stderr() {
    let Some(sh) = sh_or_skip("wrapper_without_a_binary") else {
        return;
    };
    let out = wrapper(&sh, &tmp_dir("none"), &[]);
    assert!(out.status.success(), "不能讓 SessionStart 失敗");
    assert!(out.stdout.is_empty(), "stdout 會進 Claude 的上下文");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("coding-lab") && err.contains("cargo install"),
        "{err}"
    );
    // cargo install 只裝二進位，沒有 web/：提示要說怎麼放（#733 R-13）
    assert!(err.contains("web/"), "{err}");
}

/// wrapper 寫死的版號要和 crate、plugin.json 一致：預設 Release URL 與版本比對都靠它（#733 R-3、R-4）
#[test]
fn wrapper_version_matches_the_crate() {
    let text = std::fs::read_to_string(plugin_dir().join("bin/coding-lab")).unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("VERSION="))
        .expect("wrapper 要有 VERSION=");
    assert_eq!(line, format!("VERSION={}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn wrapper_runs_the_binary_and_keeps_stdout_empty() {
    let Some(sh) = sh_or_skip("wrapper_runs_the_binary") else {
        return;
    };
    let dir = tmp_dir("bin");
    let fake = dir.join("coding-lab");
    // 假二進位：把參數寫到 stdout（wrapper 要把它擋掉）與檔案
    write_script(
        &fake,
        &format!(
            "#!/bin/sh\necho \"$@\"\necho \"$@\" > '{}/args'\n",
            to_sh(&dir)
        ),
    );

    let out = wrapper(&sh, &dir.join("home"), &[("CODING_LAB_BIN", to_sh(&fake))]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    let home = tmp_dir("home");
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::copy(&fake, home.join("bin/coding-lab")).unwrap();
    std::fs::remove_file(dir.join("args")).unwrap();
    let out = wrapper(&sh, &home, &[]);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(
        dir.join("args").exists(),
        "要用 $CODING_LAB_HOME/bin 的二進位"
    );
}

/// W-L10-4（#739，第 1 輪 Windows 驗收）：Windows 上資料目錄以 USERPROFILE 為準（和二進位的 config 一致）。
/// Git Bash 的 HOME 被改掉時，wrapper 原本用 `$HOME/.coding-lab` 找二進位，找不到就整個 session 不追蹤，
/// 自動下載還會裝出第二個資料目錄。平台用 bash 的 OSTYPE 判斷（msys／cygwin），不多叫一次 uname。
#[test]
fn wrapper_on_windows_uses_userprofile_not_a_changed_home() {
    let Some(sh) = sh_or_skip("wrapper_on_windows_uses_userprofile") else {
        return;
    };
    let dir = tmp_dir("userprofile");
    let profile = dir.join("profile");
    let fake_home = dir.join("fakehome");
    std::fs::create_dir_all(profile.join(".coding-lab/bin")).unwrap();
    std::fs::create_dir_all(&fake_home).unwrap();
    write_script(
        &profile.join(".coding-lab/bin/coding-lab"),
        &format!("#!/bin/sh\necho \"$@\" > '{}/args'\n", to_sh(&dir)),
    );
    // USERPROFILE 在 Windows 是原生路徑（wrapper 自己用 cygpath 換），其他平台直接給 sh 的路徑
    let userprofile = if cfg!(windows) {
        profile.to_string_lossy().into_owned()
    } else {
        to_sh(&profile)
    };
    let run = |ostype: &str| {
        let _ = std::fs::remove_file(dir.join("args"));
        let mut c = wrapper_cmd(&sh);
        c.arg("ensure")
            .env_remove("CODING_LAB_BIN")
            .env_remove("CODING_LAB_HOME")
            .env("HOME", to_sh(&fake_home))
            .env("USERPROFILE", &userprofile)
            .env("OSTYPE", ostype)
            .env("PATH", path_env(&sh, &[]))
            .env("CODING_LAB_RELEASE_URL", "")
            .stdin(Stdio::null());
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        dir.join("args").exists()
    };
    assert!(
        run("msys"),
        "Git Bash：要用 USERPROFILE 底下的二進位，不是改過的 HOME"
    );
    // 其他平台照舊看 HOME（Windows 的 bash 一定是 msys，這半段只在非 Windows 跑）
    if !cfg!(windows) {
        assert!(!run("linux-gnu"), "Linux：不看 USERPROFILE");
    }
}

/// ensure 不在 wrapper 讀 payload，stdin 直接交給二進位（#776 EW1）：二進位拿得到 SessionStart 的 payload；
/// 呼叫端的 stdin 一直沒關（終端機直接跑 `sh <wrapper> ensure`，R-9）時 wrapper 也不會卡在讀 stdin。
#[test]
fn wrapper_hands_stdin_to_the_binary_and_never_waits_for_it() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let Some(sh) = sh_or_skip("wrapper_hands_stdin_to_the_binary") else {
        return;
    };
    let dir = tmp_dir("stdin");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    // 讀 stdin 的假二進位：payload 寫進檔案
    let reader = dir.join("reader");
    write_script(
        &reader,
        &format!("#!/bin/sh\ncat > '{}/payload'\n", to_sh(&dir)),
    );
    let mut child = wrapper_cmd(&sh)
        .arg("ensure")
        .env("CODING_LAB_BIN", to_sh(&reader))
        .env("CODING_LAB_HOME", to_sh(&home))
        .env("CODING_LAB_RELEASE_URL", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"SessionStart"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success() && out.stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.join("payload")).unwrap(),
        r#"{"hook_event_name":"SessionStart"}"#
    );

    // 不讀 stdin 的假二進位，stdin 一直開著：wrapper 要跟著二進位馬上結束
    let quick = dir.join("quick");
    write_script(&quick, "#!/bin/sh\nexit 0\n");
    let mut child = wrapper_cmd(&sh)
        .arg("ensure")
        .env("CODING_LAB_BIN", to_sh(&quick))
        .env("CODING_LAB_HOME", to_sh(&home))
        .env("CODING_LAB_RELEASE_URL", "")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let _keep_open = child.stdin.take();
    let started = Instant::now();
    let done = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if started.elapsed() > Duration::from_secs(5) {
            break None;
        }
        std::thread::yield_now();
    };
    if done.is_none() {
        let _ = child.kill();
    }
    assert!(done.is_some_and(|s| s.success()), "wrapper 卡在讀 stdin");
}

// ── wrapper 下載二進位（#656）：第一次沒有二進位時，從 Release 下載對應平台的壓縮檔、校驗 sha256，
// 解到 $CODING_LAB_HOME/bin/。Release 的位置來自 CODING_LAB_RELEASE_URL；測試用 file:// 的假 Release。
// 壓縮檔用 tar 做（Windows 是內建的 bsdtar，zip 也是它做），sha256 在測試裡自己算，不靠 sha256sum、zip。

mod download {
    use super::plugin_dir;
    use super::sh::{file_url, path_env, sh_or_skip, to_sh, wrapper_cmd, write_script};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmp(name: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "plugin-dl-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// PATH 上的假 uname：-s、-m 照環境變數回答。
    fn stub_uname(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        write_script(
            &dir.join("uname"),
            "#!/bin/sh\ncase \"$1\" in -s) echo \"$FAKE_UNAME_S\";; -m) echo \"$FAKE_UNAME_M\";; *) echo \"$FAKE_UNAME_S\";; esac\n",
        );
    }

    /// 做壓縮檔的 tar：Windows 用內建的 bsdtar（PATH 上可能是 Git 的 GNU tar，它把 `C:` 當成遠端主機、也不會做 zip）。
    fn tar() -> Command {
        if cfg!(windows) {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            Command::new(PathBuf::from(root).join("System32").join("tar.exe"))
        } else {
            Command::new("tar")
        }
    }

    fn sha256_hex(p: &Path) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(std::fs::read(p).unwrap());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 假 Release 的內容：`stage/coding-lab-<target>/`（cargo-dist 的格式：最上層是同名目錄，裡面是二進位與 web/）。
    fn stage(dir: &Path, name: &str, exe: &str) {
        let stage = dir.join("stage").join(name);
        std::fs::create_dir_all(stage.join("web")).unwrap();
        write_script(
            &stage.join(exe),
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/ran\"\n",
        );
        std::fs::write(stage.join("web/index.html"), "<!doctype html>").unwrap();
    }

    fn write_sum(dir: &Path, archive: &str, bad: bool) {
        let hex = if bad {
            "0".repeat(64)
        } else {
            sha256_hex(&dir.join(archive))
        };
        std::fs::write(
            dir.join(format!("{archive}.sha256")),
            format!("{hex} *{archive}\n"),
        )
        .unwrap();
    }

    /// 假 Release：`coding-lab-<target>.tar.gz` 與 `<壓縮檔>.sha256`。`bad_sum` 時校驗值故意寫錯。
    fn fake_release(dir: &Path, target: &str, bad_sum: bool) {
        let name = format!("coding-lab-{target}");
        stage(dir, &name, "coding-lab");
        let archive = format!("{name}.tar.gz");
        let ok = tar()
            .arg("-czf")
            .arg(dir.join(&archive))
            .arg("-C")
            .arg(dir.join("stage"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(ok.success());
        write_sum(dir, &archive, bad_sum);
    }

    /// Windows 的假 Release：`coding-lab-<target>.zip`，內容同 tar.gz 版（二進位叫 coding-lab.exe）。
    fn fake_zip_release(dir: &Path, target: &str) {
        let name = format!("coding-lab-{target}");
        stage(dir, &name, "coding-lab.exe");
        let archive = format!("{name}.zip");
        let mut c = if cfg!(windows) {
            let mut c = tar();
            c.arg("-a").arg("-cf").arg(dir.join(&archive)).arg(&name);
            c
        } else {
            let mut c = Command::new("zip");
            c.arg("-qr").arg(dir.join(&archive)).arg(&name);
            c
        };
        let ok = c.current_dir(dir.join("stage")).status().unwrap();
        assert!(ok.success());
        write_sum(dir, &archive, false);
    }

    /// PATH 上的假 curl：把參數記到 $FAKE_CURL_LOG 後失敗（測試不連網路）。
    fn stub_curl(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        write_script(
            &dir.join("curl"),
            "#!/bin/sh\necho \"$@\" >> \"$FAKE_CURL_LOG\"\nexit 22\n",
        );
    }

    /// 只有 wrapper 安裝時用得到的工具、**沒有 unzip** 的 PATH 目錄（在 Linux 上模擬標準的 Git for Windows）。
    #[cfg(unix)]
    fn tools_without_unzip(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for t in [
            "mkdir",
            "find",
            "rmdir",
            "rm",
            "curl",
            "cut",
            "tr",
            "sha256sum",
            "chmod",
            "mv",
            "dirname",
            "sed",
        ] {
            let from = Path::new("/usr/bin").join(t);
            assert!(from.exists(), "測試機要有 {t}");
            std::os::unix::fs::symlink(&from, dir.join(t)).unwrap();
        }
    }

    /// PATH 上的假 powershell：把整串參數記到 $FAKE_PS_LOG，再用真的 unzip 做 Expand-Archive 的事。
    #[cfg(unix)]
    fn stub_powershell(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        write_script(
            &dir.join("powershell"),
            concat!(
                "#!/bin/sh\n",
                "echo \"$*\" > \"$FAKE_PS_LOG\"\n",
                "zip=$(printf '%s' \"$*\" | sed \"s/.*-LiteralPath '\\([^']*\\)'.*/\\1/\")\n",
                "dest=$(printf '%s' \"$*\" | sed \"s/.*-DestinationPath '\\([^']*\\)'.*/\\1/\")\n",
                "exec /usr/bin/unzip -q \"$zip\" -d \"$dest\"\n",
            ),
        );
    }

    struct Env {
        sh: PathBuf,
        home: PathBuf,
        stubs: PathBuf,
        release: PathBuf,
    }

    /// 找不到 sh（Windows 沒裝 Git Bash）時印出原因並回 None。
    fn env(name: &str) -> Option<Env> {
        let sh = sh_or_skip(name)?;
        let root = tmp(name);
        let e = Env {
            sh,
            home: root.join("home"),
            stubs: root.join("stubs"),
            release: root.join("release"),
        };
        std::fs::create_dir_all(&e.home).unwrap();
        std::fs::create_dir_all(&e.release).unwrap();
        stub_uname(&e.stubs);
        Some(e)
    }

    /// 跑 wrapper：PATH 是假的 uname 等加上基本工具。
    fn cmd(e: &Env, args: &[&str], uname: (&str, &str)) -> Command {
        let mut c = wrapper_cmd(&e.sh);
        c.args(args)
            .env_remove("CODING_LAB_BIN")
            .env_remove("CODING_LAB_RELEASE_URL")
            .env("CODING_LAB_HOME", to_sh(&e.home))
            .env("PATH", path_env(&e.sh, &[&e.stubs]))
            .env("FAKE_UNAME_S", uname.0)
            .env("FAKE_UNAME_M", uname.1)
            .stdin(Stdio::null());
        c
    }

    fn run(e: &Env, args: &[&str], uname: (&str, &str), url: Option<&str>) -> Output {
        let mut c = cmd(e, args, uname);
        if let Some(u) = url {
            c.env("CODING_LAB_RELEASE_URL", u);
        }
        c.output().unwrap()
    }

    fn url(e: &Env) -> String {
        file_url(&e.release)
    }

    #[test]
    fn platform_maps_uname_to_release_targets() {
        let Some(e) = env("platform") else { return };
        for (s, m, want) in [
            ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
            ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
            ("Linux", "arm64", "aarch64-unknown-linux-gnu"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
            ("MINGW64_NT-10.0-22631", "x86_64", "x86_64-pc-windows-msvc"),
            ("MSYS_NT-10.0", "x86_64", "x86_64-pc-windows-msvc"),
        ] {
            let out = run(&e, &["platform"], (s, m), None);
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), want, "{s} {m}");
        }
        // 沒有對應的 Release：不印任何平台、照樣 exit 0，stderr 說明
        for (s, m) in [
            ("FreeBSD", "x86_64"),
            ("Linux", "riscv64"),
            ("MINGW64_NT-10.0", "aarch64"),
        ] {
            let out = run(&e, &["platform"], (s, m), None);
            assert!(out.status.success(), "{s} {m}");
            assert!(out.stdout.is_empty(), "{s} {m}");
            assert!(!out.stderr.is_empty(), "{s} {m}");
        }
    }

    /// wrapper 認得的平台與 cargo-dist 出的 target 一致：少一個就有人下載不到，多一個就是死碼。
    #[test]
    fn platform_targets_match_the_dist_config() {
        let Some(e) = env("targets") else { return };
        let text = std::fs::read_to_string(plugin_dir().join("../dist-workspace.toml")).unwrap();
        let cfg: toml::Value = toml::from_str(&text).unwrap();
        let mut dist: Vec<String> = cfg["dist"]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect();
        dist.sort();
        let mut ours: Vec<String> = [
            ("Linux", "x86_64"),
            ("Linux", "aarch64"),
            ("Darwin", "x86_64"),
            ("Darwin", "arm64"),
            ("MINGW64_NT-10.0", "x86_64"),
        ]
        .iter()
        .map(|uname| {
            let out = run(&e, &["platform"], *uname, None);
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        })
        .collect();
        ours.sort();
        assert_eq!(ours, dist);
        assert_eq!(cfg["dist"]["unix-archive"].as_str(), Some(".tar.gz"));
        assert_eq!(cfg["dist"]["windows-archive"].as_str(), Some(".zip"));
        assert_eq!(cfg["dist"]["checksum"].as_str(), Some("sha256"));
    }

    #[test]
    fn install_downloads_verifies_and_unpacks_binary_with_web() {
        let Some(e) = env("install") else { return };
        fake_release(&e.release, "x86_64-unknown-linux-gnu", false);
        let out = run(&e, &["install"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty());
        let bin = e.home.join("bin/coding-lab");
        assert!(bin.exists(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(
            e.home.join("bin/web/index.html").exists(),
            "web/ 跟二進位放在一起"
        );
        // 裝好之後 wrapper 就用它
        let out = run(&e, &["ensure"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(out.status.success() && out.stdout.is_empty());
        assert_eq!(
            std::fs::read_to_string(e.home.join("bin/ran"))
                .unwrap()
                .trim(),
            "ensure"
        );
    }

    /// 資料目錄的路徑有反斜線時照樣裝得起來（Windows 驗收：`CODING_LAB_HOME=C:\\…`）。GNU sha256sum 遇到檔名含
    /// 反斜線會在輸出開頭多一個 `\\`（escaped 格式），原本拿到的雜湊值就對不上，自動下載與背景更新都失敗。
    /// Windows 直接給原生路徑（和使用者設的一樣）；其他平台用名字裡有反斜線的目錄重現同一件事。
    #[test]
    fn install_works_when_the_home_path_has_backslashes() {
        let Some(e) = env("backslash") else { return };
        fake_release(&e.release, "x86_64-unknown-linux-gnu", false);
        let (home, arg) = if cfg!(windows) {
            (e.home.clone(), e.home.to_string_lossy().into_owned())
        } else {
            let h = e.home.with_file_name("ho\\me");
            std::fs::create_dir_all(&h).unwrap();
            (h.clone(), h.to_string_lossy().into_owned()) // to_sh 會把反斜線換掉，這裡要原樣
        };
        let out = cmd(&e, &["install"], ("Linux", "x86_64"))
            .env("CODING_LAB_RELEASE_URL", url(&e))
            .env("CODING_LAB_HOME", arg)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(home.join("bin/coding-lab").exists(), "沒有裝起來：{err}");
    }

    /// Windows 的 Release 是 .zip：照常解開（二進位叫 coding-lab.exe）。Linux 上用 unzip；
    /// Windows 上標準的 Git 沒有 unzip，走 wrapper 退到的內建 tar.exe——就是實際使用者的路徑。
    #[test]
    fn install_unpacks_a_zip_release() {
        let Some(e) = env("zip") else { return };
        fake_zip_release(&e.release, "x86_64-pc-windows-msvc");
        let out = run(
            &e,
            &["install"],
            ("MINGW64_NT-10.0", "x86_64"),
            Some(&url(&e)),
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(out.stdout.is_empty());
        assert!(e.home.join("bin/coding-lab.exe").exists(), "{err}");
        assert!(e.home.join("bin/web/index.html").exists(), "{err}");
    }

    /// 標準的 Git for Windows 沒有 unzip（#699）：要退到 PowerShell 的 Expand-Archive，不能直接報解壓失敗。
    /// 只在 Linux 上模擬：Windows 實機上 wrapper 會先用到真的 tar.exe（上一個測試）。
    #[cfg(unix)]
    #[test]
    fn install_without_unzip_falls_back_to_powershell_expand_archive() {
        let Some(e) = env("zip-ps") else { return };
        fake_zip_release(&e.release, "x86_64-pc-windows-msvc");
        let tools = e.home.join("../tools");
        tools_without_unzip(&tools);
        let ps = e.home.join("../ps");
        stub_powershell(&ps);
        let log = e.home.join("../ps.log");
        let path = format!("{}:{}:{}", e.stubs.display(), ps.display(), tools.display());
        let out = cmd(&e, &["install"], ("MINGW64_NT-10.0", "x86_64"))
            .env("CODING_LAB_RELEASE_URL", url(&e))
            .env("PATH", &path)
            .env("FAKE_PS_LOG", &log)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(out.stdout.is_empty());
        let called = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            called.contains("Expand-Archive"),
            "要用 powershell 解壓：{called}\n{err}"
        );
        assert!(e.home.join("bin/coding-lab.exe").exists(), "{err}");
        assert!(e.home.join("bin/web/index.html").exists(), "{err}");
    }

    /// 連 PowerShell 都沒有：報解壓失敗、不裝、exit 0。（同上，只在 Linux 上模擬）
    #[cfg(unix)]
    #[test]
    fn install_without_any_unzipper_reports_and_installs_nothing() {
        let Some(e) = env("zip-none") else { return };
        fake_zip_release(&e.release, "x86_64-pc-windows-msvc");
        let tools = e.home.join("../tools");
        tools_without_unzip(&tools);
        let path = format!("{}:{}", e.stubs.display(), tools.display());
        let out = cmd(&e, &["install"], ("MINGW64_NT-10.0", "x86_64"))
            .env("CODING_LAB_RELEASE_URL", url(&e))
            .env("PATH", &path)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "失敗也 exit 0：{err}");
        assert!(out.stdout.is_empty());
        assert!(!e.home.join("bin/coding-lab.exe").exists());
        assert!(err.contains("解壓"), "{err}");
    }

    #[test]
    fn install_refuses_an_archive_whose_sha256_does_not_match() {
        let Some(e) = env("badsum") else { return };
        fake_release(&e.release, "aarch64-apple-darwin", true);
        let out = run(&e, &["install"], ("Darwin", "arm64"), Some(&url(&e)));
        assert!(out.status.success(), "失敗也 exit 0");
        assert!(out.stdout.is_empty());
        assert!(!e.home.join("bin/coding-lab").exists(), "校驗不符不能裝");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("sha256"), "{err}");
        // 暫存的下載也要清掉
        let left: Vec<_> = std::fs::read_dir(e.home.join("bin"))
            .map(|d| d.flatten().map(|x| x.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "留下了 {left:?}");
    }

    /// 沒設 CODING_LAB_RELEASE_URL 時，從 GitHub Release 下載**和 plugin 同版**的二進位（#733 R-3）
    #[test]
    fn install_defaults_to_the_github_release_of_the_plugin_version() {
        let Some(e) = env("default-url") else { return };
        stub_curl(&e.stubs);
        let log = e.home.join("curl.log");
        let out = cmd(&e, &["install"], ("Linux", "x86_64"))
            .env("FAKE_CURL_LOG", to_sh(&log))
            .output()
            .unwrap();
        assert!(out.status.success() && out.stdout.is_empty());
        let urls = std::fs::read_to_string(&log).unwrap_or_default();
        let want = format!(
            "https://github.com/DuridMing/VibingTracker/releases/download/v{}/coding-lab-x86_64-unknown-linux-gnu.tar.gz",
            env!("CARGO_PKG_VERSION")
        );
        assert!(urls.contains(&want), "{urls}");
    }

    /// 下載只接受 https（和測試用的 file://）：http 會被中間人換掉壓縮檔與 .sha256（#733 R-12）
    #[test]
    fn install_refuses_a_plain_http_release_url() {
        let Some(e) = env("http") else { return };
        stub_curl(&e.stubs);
        let log = e.home.join("curl.log");
        let out = cmd(&e, &["install"], ("Linux", "x86_64"))
            .env("CODING_LAB_RELEASE_URL", "http://example.invalid/rel")
            .env("FAKE_CURL_LOG", to_sh(&log))
            .output()
            .unwrap();
        assert!(out.status.success() && out.stdout.is_empty());
        assert!(!log.exists(), "不能用 http 下載");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("https"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// collector 還在跑（有 collector.json）時，背景更新不換掉 web/ 與二進位：新的 JS 配舊的二進位會壞，
    /// Windows 上執行中的 .exe 也換不掉。先放 bin/.staged/，下一次 ensure 而且沒有 collector 在跑時才換（#784 W-R1-5）
    #[test]
    fn an_update_while_the_collector_runs_is_staged_until_it_stops() {
        let Some(e) = env("staged") else { return };
        fake_release(&e.release, "x86_64-unknown-linux-gnu", false);
        let out = run(&e, &["install"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stamp = e.home.join("bin/coding-lab.version");
        // 舊版、而且 collector 在跑
        std::fs::write(&stamp, "0.0.1\n").unwrap();
        std::fs::write(e.home.join("bin/web/index.html"), "old").unwrap();
        std::fs::write(e.home.join("collector.json"), "{}").unwrap();
        let out = run(&e, &["install"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(e.home.join("bin/web/index.html")).unwrap(),
            "old",
            "collector 在跑時不能換 web/"
        );
        assert_eq!(
            std::fs::read_to_string(&stamp).unwrap().trim(),
            "0.0.1",
            "二進位也還沒換"
        );
        assert!(e.home.join("bin/.staged").is_dir(), "新版先放 staging");
        // collector 還在跑：ensure 照用舊的，不換，也不再下載一次（staging 裡已經是同版）
        let out = run(&e, &["ensure"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(out.status.success() && out.stdout.is_empty());
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("更新"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(e.home.join("bin/web/index.html")).unwrap(),
            "old"
        );
        // collector 停了：下一次 ensure 先換上新版再執行
        std::fs::remove_file(e.home.join("collector.json")).unwrap();
        let out = run(&e, &["ensure"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(
            out.status.success() && out.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(e.home.join("bin/web/index.html")).unwrap(),
            "<!doctype html>"
        );
        assert_eq!(
            std::fs::read_to_string(&stamp).unwrap().trim(),
            env!("CARGO_PKG_VERSION")
        );
        assert!(!e.home.join("bin/.staged").exists(), "換完清掉 staging");
        assert_eq!(
            std::fs::read_to_string(e.home.join("bin/ran"))
                .unwrap()
                .trim(),
            "ensure",
            "換上的新版照常執行"
        );
    }

    /// wrapper 裝的二進位版本落後 plugin 時，背景下載新版（這個 session 照用舊的）；同版不下載（#733 R-4）
    #[test]
    fn an_installed_binary_older_than_the_plugin_is_updated_in_the_background() {
        let Some(e) = env("update") else { return };
        fake_release(&e.release, "x86_64-unknown-linux-gnu", false);
        let out = run(&e, &["install"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stamp = e.home.join("bin/coding-lab.version");
        assert_eq!(
            std::fs::read_to_string(&stamp).unwrap().trim(),
            env!("CARGO_PKG_VERSION"),
            "安裝時記下版本"
        );

        // 同版：照常執行、不下載
        let out = run(&e, &["ensure"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(out.status.success() && out.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("更新"));

        // 舊版：這次照用舊的，同時在背景更新
        std::fs::write(&stamp, "0.0.1\n").unwrap();
        std::fs::remove_file(e.home.join("bin/ran")).unwrap();
        let out = run(&e, &["ensure"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(out.status.success() && out.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("更新"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(e.home.join("bin/ran").exists(), "這個 session 照用舊的");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(&stamp).unwrap_or_default().trim()
            != env!("CARGO_PKG_VERSION")
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(
            std::fs::read_to_string(&stamp).unwrap().trim(),
            env!("CARGO_PKG_VERSION"),
            "背景更新沒有完成"
        );
    }

    #[test]
    fn install_failure_exits_zero_and_prints_manual_instructions() {
        let Some(e) = env("offline") else { return };
        // Release 裡沒有這個平台的檔（等同下載失敗、斷網）
        let out = run(&e, &["install"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("cargo install"), "{err}");
        assert!(!e.home.join("bin/coding-lab").exists());
    }

    #[test]
    fn ensure_without_a_binary_starts_the_download_in_the_background() {
        let Some(e) = env("bg") else { return };
        fake_release(&e.release, "x86_64-unknown-linux-gnu", false);
        let started = Instant::now();
        let out = run(&e, &["ensure"], ("Linux", "x86_64"), Some(&url(&e)));
        assert!(out.status.success() && out.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("下載"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "不能等下載完才回"
        );
        // 背景的下載完成後二進位出現（這個 session 不追蹤，下一個開始）
        let bin = e.home.join("bin/coding-lab");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !bin.exists() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            bin.exists(),
            "背景下載沒有完成；install.log：{}",
            std::fs::read_to_string(e.home.join("install.log")).unwrap_or_default()
        );
    }
}
