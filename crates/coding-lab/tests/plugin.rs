//! plugin 的結構驗證與 wrapper 行為（#628）。hooks.json 必須和契約 §2 訂閱的事件一致。

use std::path::{Path, PathBuf};
#[cfg(unix)]
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
        .env("CODING_LAB_RELEASE_URL", "") // 空字串＝不自動下載（預設會連 GitHub）
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

/// ensure 不在 wrapper 讀 payload，stdin 直接交給二進位（#776 EW1）：二進位拿得到 SessionStart 的 payload；
/// 呼叫端的 stdin 一直沒關（終端機直接跑 `sh <wrapper> ensure`，R-9）時 wrapper 也不會卡在讀 stdin。
#[cfg(unix)]
#[test]
fn wrapper_hands_stdin_to_the_binary_and_never_waits_for_it() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("plugin-stdin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    // 讀 stdin 的假二進位：payload 寫進檔案
    let reader = dir.join("reader");
    std::fs::write(
        &reader,
        format!("#!/bin/sh\ncat > '{}/payload'\n", dir.display()),
    )
    .unwrap();
    std::fs::set_permissions(&reader, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(plugin_dir().join("bin/coding-lab"))
        .arg("ensure")
        .env("CODING_LAB_BIN", &reader)
        .env("CODING_LAB_HOME", &home)
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
    std::fs::write(&quick, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&quick, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(plugin_dir().join("bin/coding-lab"))
        .arg("ensure")
        .env("CODING_LAB_BIN", &quick)
        .env("CODING_LAB_HOME", &home)
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

#[cfg(unix)]
mod download {
    use super::plugin_dir;
    use std::os::unix::fs::PermissionsExt;
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

    fn exec(p: &Path, body: &str) {
        std::fs::write(p, body).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// PATH 上的假 uname：-s、-m 照環境變數回答。
    fn stub_uname(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        exec(
            &dir.join("uname"),
            "#!/bin/sh\ncase \"$1\" in -s) echo \"$FAKE_UNAME_S\";; -m) echo \"$FAKE_UNAME_M\";; *) echo \"$FAKE_UNAME_S\";; esac\n",
        );
    }

    /// 假 Release：`coding-lab-<target>.tar.gz`（cargo-dist 的格式：最上層是同名目錄，裡面是二進位與 web/）
    /// 與 `<壓縮檔>.sha256`。`bad_sum` 時校驗值故意寫錯。
    fn fake_release(dir: &Path, target: &str, bad_sum: bool) {
        let name = format!("coding-lab-{target}");
        let stage = dir.join("stage").join(&name);
        std::fs::create_dir_all(stage.join("web")).unwrap();
        exec(
            &stage.join("coding-lab"),
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/ran\"\n",
        );
        std::fs::write(stage.join("web/index.html"), "<!doctype html>").unwrap();
        let archive = dir.join(format!("{name}.tar.gz"));
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.join("stage"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(ok.success());
        let sum = Command::new("sha256sum").arg(&archive).output().unwrap();
        let hex = String::from_utf8_lossy(&sum.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let hex = if bad_sum { "0".repeat(64) } else { hex };
        std::fs::write(
            dir.join(format!("{name}.tar.gz.sha256")),
            format!("{hex} *{name}.tar.gz\n"),
        )
        .unwrap();
    }

    /// Windows 的假 Release：`coding-lab-<target>.zip`，內容同 tar.gz 版（二進位叫 coding-lab.exe）。
    fn fake_zip_release(dir: &Path, target: &str) {
        let name = format!("coding-lab-{target}");
        let stage = dir.join("stage").join(&name);
        std::fs::create_dir_all(stage.join("web")).unwrap();
        exec(
            &stage.join("coding-lab.exe"),
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/ran\"\n",
        );
        std::fs::write(stage.join("web/index.html"), "<!doctype html>").unwrap();
        let archive = dir.join(format!("{name}.zip"));
        let ok = Command::new("zip")
            .arg("-qr")
            .arg(&archive)
            .arg(&name)
            .current_dir(dir.join("stage"))
            .status()
            .unwrap();
        assert!(ok.success());
        let sum = Command::new("sha256sum").arg(&archive).output().unwrap();
        let hex = String::from_utf8_lossy(&sum.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        std::fs::write(
            dir.join(format!("{name}.zip.sha256")),
            format!("{hex} *{name}.zip\n"),
        )
        .unwrap();
    }

    /// PATH 上的假 curl：把參數記到 $FAKE_CURL_LOG 後失敗（測試不連網路）。
    fn stub_curl(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        exec(
            &dir.join("curl"),
            "#!/bin/sh\necho \"$@\" >> \"$FAKE_CURL_LOG\"\nexit 22\n",
        );
    }

    /// 只有 wrapper 安裝時用得到的工具、**沒有 unzip** 的 PATH 目錄（模擬標準的 Git for Windows）。
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
    fn stub_powershell(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        exec(
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
        home: PathBuf,
        stubs: PathBuf,
        release: PathBuf,
    }

    fn env(name: &str) -> Env {
        let root = tmp(name);
        let e = Env {
            home: root.join("home"),
            stubs: root.join("stubs"),
            release: root.join("release"),
        };
        std::fs::create_dir_all(&e.home).unwrap();
        std::fs::create_dir_all(&e.release).unwrap();
        stub_uname(&e.stubs);
        e
    }

    fn run(e: &Env, args: &[&str], uname: (&str, &str), url: Option<&str>) -> Output {
        run_with_path(
            e,
            args,
            uname,
            url,
            &format!("{}:/usr/bin:/bin", e.stubs.display()),
        )
    }

    fn run_with_path(
        e: &Env,
        args: &[&str],
        uname: (&str, &str),
        url: Option<&str>,
        path: &str,
    ) -> Output {
        let mut c = Command::new(plugin_dir().join("bin/coding-lab"));
        c.args(args)
            .env_remove("CODING_LAB_BIN")
            .env_remove("CODING_LAB_RELEASE_URL")
            .env("CODING_LAB_HOME", &e.home)
            .env("PATH", path)
            .env("FAKE_UNAME_S", uname.0)
            .env("FAKE_UNAME_M", uname.1)
            .stdin(Stdio::null());
        if let Some(u) = url {
            c.env("CODING_LAB_RELEASE_URL", u);
        }
        c.output().unwrap()
    }

    fn url(e: &Env) -> String {
        format!("file://{}", e.release.display())
    }

    #[test]
    fn platform_maps_uname_to_release_targets() {
        let e = env("platform");
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
        let text = std::fs::read_to_string(plugin_dir().join("../dist-workspace.toml")).unwrap();
        let cfg: toml::Value = toml::from_str(&text).unwrap();
        let mut dist: Vec<String> = cfg["dist"]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect();
        dist.sort();
        let e = env("targets");
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
        let e = env("install");
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

    /// Windows 的 Release 是 .zip：有 unzip 時照常解開（二進位叫 coding-lab.exe）。
    #[test]
    fn install_unpacks_a_zip_release_with_unzip() {
        let e = env("zip");
        fake_zip_release(&e.release, "x86_64-pc-windows-msvc");
        let out = run(
            &e,
            &["install"],
            ("MINGW64_NT-10.0", "x86_64"),
            Some(&url(&e)),
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(e.home.join("bin/coding-lab.exe").exists(), "{err}");
        assert!(e.home.join("bin/web/index.html").exists(), "{err}");
    }

    /// 標準的 Git for Windows 沒有 unzip（#699）：要退到 PowerShell 的 Expand-Archive，不能直接報解壓失敗。
    #[test]
    fn install_without_unzip_falls_back_to_powershell_expand_archive() {
        let e = env("zip-ps");
        fake_zip_release(&e.release, "x86_64-pc-windows-msvc");
        let tools = e.home.join("../tools");
        tools_without_unzip(&tools);
        let ps = e.home.join("../ps");
        stub_powershell(&ps);
        let log = e.home.join("../ps.log");
        let path = format!("{}:{}:{}", e.stubs.display(), ps.display(), tools.display());
        let out = {
            let mut c = Command::new(plugin_dir().join("bin/coding-lab"));
            c.arg("install")
                .env_remove("CODING_LAB_BIN")
                .env("CODING_LAB_HOME", &e.home)
                .env("CODING_LAB_RELEASE_URL", url(&e))
                .env("PATH", &path)
                .env("FAKE_UNAME_S", "MINGW64_NT-10.0")
                .env("FAKE_UNAME_M", "x86_64")
                .env("FAKE_PS_LOG", &log)
                .stdin(Stdio::null());
            c.output().unwrap()
        };
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

    /// 連 PowerShell 都沒有：報解壓失敗、不裝、exit 0。
    #[test]
    fn install_without_any_unzipper_reports_and_installs_nothing() {
        let e = env("zip-none");
        fake_zip_release(&e.release, "x86_64-pc-windows-msvc");
        let tools = e.home.join("../tools");
        tools_without_unzip(&tools);
        let path = format!("{}:{}", e.stubs.display(), tools.display());
        let out = run_with_path(
            &e,
            &["install"],
            ("MINGW64_NT-10.0", "x86_64"),
            Some(&url(&e)),
            &path,
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "失敗也 exit 0：{err}");
        assert!(out.stdout.is_empty());
        assert!(!e.home.join("bin/coding-lab.exe").exists());
        assert!(err.contains("解壓"), "{err}");
    }

    #[test]
    fn install_refuses_an_archive_whose_sha256_does_not_match() {
        let e = env("badsum");
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
        let e = env("default-url");
        stub_curl(&e.stubs);
        let log = e.home.join("curl.log");
        let out = Command::new(plugin_dir().join("bin/coding-lab"))
            .arg("install")
            .env_remove("CODING_LAB_BIN")
            .env_remove("CODING_LAB_RELEASE_URL")
            .env("CODING_LAB_HOME", &e.home)
            .env("PATH", format!("{}:/usr/bin:/bin", e.stubs.display()))
            .env("FAKE_UNAME_S", "Linux")
            .env("FAKE_UNAME_M", "x86_64")
            .env("FAKE_CURL_LOG", &log)
            .stdin(Stdio::null())
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
        let e = env("http");
        stub_curl(&e.stubs);
        let log = e.home.join("curl.log");
        let out = Command::new(plugin_dir().join("bin/coding-lab"))
            .arg("install")
            .env_remove("CODING_LAB_BIN")
            .env("CODING_LAB_RELEASE_URL", "http://example.invalid/rel")
            .env("CODING_LAB_HOME", &e.home)
            .env("PATH", format!("{}:/usr/bin:/bin", e.stubs.display()))
            .env("FAKE_UNAME_S", "Linux")
            .env("FAKE_UNAME_M", "x86_64")
            .env("FAKE_CURL_LOG", &log)
            .stdin(Stdio::null())
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

    /// wrapper 裝的二進位版本落後 plugin 時，背景下載新版（這個 session 照用舊的）；同版不下載（#733 R-4）
    #[test]
    fn an_installed_binary_older_than_the_plugin_is_updated_in_the_background() {
        let e = env("update");
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
        let e = env("offline");
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
        let e = env("bg");
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
        assert!(bin.exists(), "背景下載沒有完成");
    }
}
