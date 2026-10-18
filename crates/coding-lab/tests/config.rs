//! 設定優先序：命令列 > `CODING_LAB_*` > config.toml > 預設（#629）。

use std::collections::HashMap;
use std::path::PathBuf;

use coding_lab::config::{Config, Overrides};

fn resolve(cli: Overrides, env: &[(&str, &str)], file: Option<&str>) -> Result<Config, String> {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Config::resolve(
        &cli,
        &|k: &str| env.get(k).cloned(),
        file,
        PathBuf::from("/home/u/.coding-lab"),
    )
}

#[test]
fn defaults() {
    let c = resolve(Overrides::default(), &[], None).unwrap();
    assert_eq!(c.hook_bind.to_string(), "127.0.0.1:4800");
    assert_eq!(c.web_bind.to_string(), "127.0.0.1:4801");
    assert_eq!(c.idle_exit_seconds, 600);
    assert_eq!(c.home, PathBuf::from("/home/u/.coding-lab"));
    assert_eq!(c.web_root, None);
}

#[test]
fn priority_matrix() {
    let file = r#"
        [hook]
        bind = "127.0.0.1:5000"
        [web]
        bind = "127.0.0.1:5001"
        root = "/srv/web"
        [collector]
        idle_exit_seconds = 30
    "#;
    let c = resolve(Overrides::default(), &[], Some(file)).unwrap();
    assert_eq!(
        (c.hook_bind.port(), c.web_bind.port(), c.idle_exit_seconds),
        (5000, 5001, 30)
    );
    assert_eq!(c.web_root, Some(PathBuf::from("/srv/web")));

    let env = [
        ("CODING_LAB_HOOK_BIND", "127.0.0.1:6000"),
        ("CODING_LAB_IDLE_EXIT_SECONDS", "0"),
    ];
    let c = resolve(Overrides::default(), &env, Some(file)).unwrap();
    assert_eq!(
        (c.hook_bind.port(), c.web_bind.port(), c.idle_exit_seconds),
        (6000, 5001, 0),
        "環境變數蓋過檔案"
    );

    let cli = Overrides {
        hook_bind: Some("127.0.0.1:7000".into()),
        ..Overrides::default()
    };
    let c = resolve(cli, &env, Some(file)).unwrap();
    assert_eq!(c.hook_bind.port(), 7000, "命令列蓋過環境變數");
}

#[test]
fn home_comes_from_cli_then_env() {
    let c = resolve(Overrides::default(), &[("CODING_LAB_HOME", "/tmp/x")], None).unwrap();
    assert_eq!(c.home, PathBuf::from("/tmp/x"));
    let cli = Overrides {
        home: Some("/tmp/y".into()),
        ..Overrides::default()
    };
    assert_eq!(
        resolve(cli, &[("CODING_LAB_HOME", "/tmp/x")], None)
            .unwrap()
            .home,
        PathBuf::from("/tmp/y")
    );
}

/// W-M6（#739）：home 結尾的分隔符號拿掉。Windows 的 `"C:\x\"` 在命令列上會把結尾的引號跳脫掉，
/// 工作排程器的 `<Arguments>` 因此壞掉；collector.json 的位址、log 的路徑也跟著一致。
/// （`PathBuf` 的 `==` 不看結尾的分隔符號，所以比字串）
#[test]
fn home_has_no_trailing_separator() {
    let c = resolve(
        Overrides::default(),
        &[("CODING_LAB_HOME", "/tmp/x/")],
        None,
    )
    .unwrap();
    assert_eq!(c.home.to_str(), Some("/tmp/x"));
    let cli = Overrides {
        home: Some("/tmp/y//".into()),
        ..Overrides::default()
    };
    assert_eq!(
        resolve(cli, &[], None).unwrap().home.to_str(),
        Some("/tmp/y")
    );
    let root = Overrides {
        home: Some("/".into()),
        ..Overrides::default()
    };
    assert_eq!(resolve(root, &[], None).unwrap().home.to_str(), Some("/"));
}

/// W-L10-4（#739）：家目錄在 Windows 先看 USERPROFILE、其他平台先看 HOME。Git Bash 會設 HOME，
/// 和 PowerShell、工作排程器（只有 USERPROFILE）拉起的 collector 會用到不同的資料目錄。空字串當作沒設。
#[test]
fn user_home_prefers_userprofile_on_windows_and_home_elsewhere() {
    use coding_lab::config::user_home_from;
    let pick = |vars: &[(&str, &str)], windows: bool| {
        let m: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        user_home_from(&|k: &str| m.get(k).cloned(), windows)
    };
    let both = [("HOME", "/c/Users/u"), ("USERPROFILE", r"C:\Users\u")];
    assert_eq!(pick(&both, true), Some(PathBuf::from(r"C:\Users\u")));
    assert_eq!(pick(&both, false), Some(PathBuf::from("/c/Users/u")));
    assert_eq!(
        pick(&[("HOME", "/home/u")], true),
        Some(PathBuf::from("/home/u"))
    );
    assert_eq!(
        pick(&[("HOME", "/home/u"), ("USERPROFILE", "")], true),
        Some(PathBuf::from("/home/u"))
    );
    assert_eq!(pick(&[("HOME", "")], false), None);
}

#[test]
fn web_auth_keys_parse_with_defaults() {
    use coding_lab::config::{AuthMode, RemoteThinking};
    let c = resolve(Overrides::default(), &[], None).unwrap();
    assert_eq!(c.auth, AuthMode::LocalOpen);
    assert_eq!(c.remote_thinking, RemoteThinking::OnDemand);
    assert!(!c.allow_insecure);

    let file = "[web]\nbind = \"127.0.0.1:4801\"\nauth = \"always\"\nremote_thinking = \"hide\"\nallow_insecure = true\n";
    let c = resolve(Overrides::default(), &[], Some(file)).unwrap();
    assert_eq!(c.auth, AuthMode::Always);
    assert_eq!(c.remote_thinking, RemoteThinking::Hide);
    assert!(c.allow_insecure);

    // 環境變數蓋過檔案
    let c = resolve(
        Overrides::default(),
        &[
            ("CODING_LAB_WEB_AUTH", "local-open"),
            ("CODING_LAB_REMOTE_THINKING", "show"),
        ],
        Some(file),
    )
    .unwrap();
    assert_eq!(c.auth, AuthMode::LocalOpen);
    assert_eq!(c.remote_thinking, RemoteThinking::Show);

    let err = resolve(Overrides::default(), &[], Some("[web]\nauth = \"open\"\n")).unwrap_err();
    assert!(err.contains("auth"), "{err}");
    let err = resolve(
        Overrides::default(),
        &[],
        Some("[web]\nremote_thinking = \"full\"\n"),
    )
    .unwrap_err();
    assert!(err.contains("remote_thinking"), "{err}");
}

/// PLAN「網路綁定」第 4 點：tailnet／私有網段可以直接綁；公開 IP 或 0.0.0.0 沒有 TLS 時拒絕，除非 allow_insecure。
#[test]
fn non_loopback_web_bind_private_ok_public_needs_allow_insecure() {
    for bind in [
        "100.64.0.1:4801",
        "192.168.1.5:4801",
        "10.0.0.2:4801",
        "172.16.3.4:4801",
        "[fd7a:115c:a1e0::1]:4801",
    ] {
        let c = resolve(Overrides::default(), &[("CODING_LAB_WEB_BIND", bind)], None)
            .unwrap_or_else(|e| panic!("{bind}：{e}"));
        assert_eq!(c.web_bind.to_string(), bind);
    }
    for bind in ["0.0.0.0:4801", "[::]:4801", "203.0.113.9:4801"] {
        let err =
            resolve(Overrides::default(), &[("CODING_LAB_WEB_BIND", bind)], None).unwrap_err();
        assert!(
            err.contains("allow_insecure") && err.contains("SSH"),
            "{bind}：{err}"
        );
        let c = resolve(
            Overrides::default(),
            &[
                ("CODING_LAB_WEB_BIND", bind),
                ("CODING_LAB_ALLOW_INSECURE", "true"),
            ],
            None,
        )
        .unwrap_or_else(|e| panic!("{bind} + allow_insecure：{e}"));
        assert_eq!(c.web_bind.to_string(), bind);
    }
    // hook 入口永遠只能 loopback（不變式 6）
    let err = resolve(
        Overrides::default(),
        &[("CODING_LAB_HOOK_BIND", "100.64.0.1:4800")],
        None,
    )
    .unwrap_err();
    assert!(err.contains("127.0.0.1"), "{err}");
}

#[test]
fn bad_values_are_reported() {
    assert!(resolve(Overrides::default(), &[], Some("not = [toml")).is_err());
    assert!(
        resolve(
            Overrides::default(),
            &[("CODING_LAB_WEB_BIND", "nope")],
            None
        )
        .is_err()
    );
    assert!(
        resolve(
            Overrides::default(),
            &[("CODING_LAB_IDLE_EXIT_SECONDS", "-1")],
            None
        )
        .is_err()
    );
}

/// #670-9：plugin 的 hooks.json 只能寫死 127.0.0.1:4800（http hook 的 url 不能內插變數），
/// hook 埠改掉的話除了 SessionStart 以外的 hook 都會連不上，要警告。
#[test]
fn hook_port_other_than_the_plugins_is_warned() {
    let c = resolve(Overrides::default(), &[], None).unwrap();
    assert_eq!(c.plugin_warning(), None);
    let c = resolve(
        Overrides::default(),
        &[],
        Some("[hook]\nbind = \"127.0.0.1:4900\"\n"),
    )
    .unwrap();
    let w = c.plugin_warning().expect("要警告");
    assert!(w.contains("4800") && w.contains("4900"), "{w}");
}

/// 沒設定 web root 時用二進位旁邊的 web/（wrapper 下載的 Release 把兩者解在一起，#656）。
#[test]
fn bundled_web_is_next_to_the_binary() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("config-web-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let exe = dir.join("bin/coding-lab");
    assert_eq!(
        coding_lab::config::bundled_web(&exe),
        None,
        "沒有 web/ 就不設"
    );
    std::fs::create_dir_all(dir.join("bin/web")).unwrap();
    assert_eq!(
        coding_lab::config::bundled_web(&exe),
        None,
        "空的 web/（沒有 index.html）不算"
    );
    std::fs::write(dir.join("bin/web/index.html"), "<!doctype html>").unwrap();
    assert_eq!(
        coding_lab::config::bundled_web(&exe),
        Some(dir.join("bin/web"))
    );
}

/// #713：store 的批次 flush 間隔可調（測試把它調大讓時序可控）；預設 100 ms。
#[test]
fn store_flush_interval_is_configurable() {
    let c = resolve(Overrides::default(), &[], None).unwrap();
    assert_eq!(c.store_flush_ms, 100);
    let file = "[collector]\nstore_flush_ms = 250\n";
    let c = resolve(Overrides::default(), &[], Some(file)).unwrap();
    assert_eq!(c.store_flush_ms, 250);
    let env = [("CODING_LAB_STORE_FLUSH_MS", "60000")];
    let c = resolve(Overrides::default(), &env, Some(file)).unwrap();
    assert_eq!(c.store_flush_ms, 60_000, "環境變數蓋過檔案");
    assert!(
        resolve(
            Overrides::default(),
            &[("CODING_LAB_STORE_FLUSH_MS", "x")],
            None
        )
        .is_err()
    );
    assert!(
        resolve(
            Overrides::default(),
            &[("CODING_LAB_STORE_FLUSH_MS", "0")],
            None
        )
        .is_err(),
        "0 會讓 flush 迴圈空轉"
    );
}

/// #706：「等你」保底判斷的門檻；0 表示關閉。
#[test]
fn wait_guess_threshold_is_configurable() {
    let c = resolve(Overrides::default(), &[], None).unwrap();
    assert_eq!(c.wait_guess_seconds, 30);
    let file = "[collector]\nwait_guess_seconds = 0\n";
    let c = resolve(Overrides::default(), &[], Some(file)).unwrap();
    assert_eq!(c.wait_guess_seconds, 0);
    let env = [("CODING_LAB_WAIT_GUESS_SECONDS", "45")];
    let c = resolve(Overrides::default(), &env, Some(file)).unwrap();
    assert_eq!(c.wait_guess_seconds, 45, "環境變數蓋過檔案");
    assert!(
        resolve(
            Overrides::default(),
            &[("CODING_LAB_WAIT_GUESS_SECONDS", "半分鐘")],
            None
        )
        .is_err()
    );
}

/// #722：`[web].bind` 可以是一個字串或陣列（區網直連與 SSH 通道的 127.0.0.1 並存）；環境變數用逗號分隔。
#[test]
fn web_bind_accepts_a_list_and_keeps_the_first_as_primary() {
    let one = resolve(
        Overrides::default(),
        &[],
        Some("[web]\nbind = \"127.0.0.1:4801\"\n"),
    )
    .unwrap();
    assert_eq!(one.web_bind.to_string(), "127.0.0.1:4801");
    assert_eq!(one.web_binds.len(), 1);

    let many = resolve(
        Overrides::default(),
        &[],
        Some("[web]\nbind = [\"127.0.0.1:4801\", \"192.168.1.5:4801\"]\n"),
    )
    .unwrap();
    assert_eq!(
        many.web_bind.to_string(),
        "127.0.0.1:4801",
        "第一個是主要位址"
    );
    assert_eq!(
        many.web_binds
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>(),
        ["127.0.0.1:4801", "192.168.1.5:4801"]
    );

    let env = resolve(
        Overrides::default(),
        &[("CODING_LAB_WEB_BIND", "127.0.0.1:5001, 10.0.0.2:5001")],
        Some("[web]\nbind = \"127.0.0.1:4801\"\n"),
    )
    .unwrap();
    assert_eq!(
        env.web_binds
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>(),
        ["127.0.0.1:5001", "10.0.0.2:5001"],
        "環境變數逗號分隔，蓋過檔案"
    );

    let err = resolve(
        Overrides::default(),
        &[],
        Some("[web]\nbind = [\"127.0.0.1:4801\", \"127.0.0.1:4801\"]\n"),
    )
    .unwrap_err();
    assert!(err.contains("重複"), "{err}");
    let err = resolve(Overrides::default(), &[], Some("[web]\nbind = []\n")).unwrap_err();
    assert!(err.contains("至少"), "{err}");
    // 陣列裡有公開 IP 一樣要 allow_insecure
    let err = resolve(
        Overrides::default(),
        &[],
        Some("[web]\nbind = [\"127.0.0.1:4801\", \"203.0.113.9:4801\"]\n"),
    )
    .unwrap_err();
    assert!(err.contains("allow_insecure"), "{err}");
}

/// L-16（#750）：打錯的鍵（`bnd`、`[colector]`、upstream 的 `kye`）原本被 serde 默默略過、用預設值；
/// 改成照樣啟動（打錯一個字不該讓 collector 起不來、hook 全丟）但每個都列成警告。
#[test]
fn unknown_keys_in_config_toml_are_warned() {
    let file = r#"
        unknown_top = 1
        [web]
        bnd = "127.0.0.1:4801"
        [colector]
        idle_exit_seconds = 5
        [[hub.upstream]]
        name = "a"
        url = "http://127.0.0.1:4801"
        kye = "clk_x"
    "#;
    let c = resolve(Overrides::default(), &[], Some(file)).unwrap();
    let w = c.warnings.join("\n");
    for key in [
        "unknown_top",
        "[web].bnd",
        "[colector]",
        "[[hub.upstream]].kye",
    ] {
        assert!(w.contains(key), "沒警告 {key}：{w}");
    }
    assert_eq!(c.idle_exit_seconds, 600, "打錯的段落不生效");

    let known = r#"
        [hook]
        bind = "127.0.0.1:4800"
        [web]
        bind = ["127.0.0.1:4801"]
        root = "/srv/web"
        auth = "local-open"
        remote_thinking = "hide"
        allow_insecure = false
        [collector]
        idle_exit_seconds = 5
        store_flush_ms = 100
        wait_guess_seconds = 60
        [hub]
        bind = "127.0.0.1:4802"
        [[hub.upstream]]
        name = "a"
        url = "http://127.0.0.1:4801"
        key = "clk_x"
    "#;
    let c = resolve(Overrides::default(), &[], Some(known)).unwrap();
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);
}
