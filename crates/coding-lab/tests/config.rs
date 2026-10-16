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

#[test]
fn keys_planned_for_p6_are_accepted_but_unused() {
    let file = "[web]\nbind = \"127.0.0.1:4801\"\nauth = \"local-open\"\nremote_thinking = \"on_demand\"\n";
    assert!(
        resolve(Overrides::default(), &[], Some(file)).is_ok(),
        "README 的設定範例要能直接用"
    );
}

#[test]
fn non_loopback_web_is_refused_until_keys_exist() {
    let err = resolve(
        Overrides::default(),
        &[("CODING_LAB_WEB_BIND", "0.0.0.0:4801")],
        None,
    )
    .unwrap_err();
    assert!(err.contains("SSH"), "{err}");
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
