//! 設定（#629）：命令列 > `CODING_LAB_*` 環境變數 > `<home>/config.toml` > 預設。
//!
//! P6 才用的鍵（`[web].auth`、`remote_thinking` 等）先接受、不使用，README 的範例可以直接用。
//! P6 金鑰驗證完成前，hook 與 web 入口都只能綁 loopback。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_HOOK_BIND: &str = "127.0.0.1:4800";
pub const DEFAULT_WEB_BIND: &str = "127.0.0.1:4801";
pub const DEFAULT_IDLE_EXIT_SECONDS: u64 = 600;

/// 命令列給的值（沒給就是 None）。
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub home: Option<PathBuf>,
    pub hook_bind: Option<String>,
    pub web_bind: Option<String>,
    pub web_root: Option<PathBuf>,
    pub idle_exit_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub home: PathBuf,
    pub hook_bind: SocketAddr,
    pub web_bind: SocketAddr,
    /// 前端靜態檔；None 時只提供 WebSocket 與 /healthz
    pub web_root: Option<PathBuf>,
    /// 沒有任何 hook 進來多久就自動結束；0 表示不結束（形態 B 常駐）
    pub idle_exit_seconds: u64,
}

#[derive(Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    hook: HookSection,
    #[serde(default)]
    web: WebSection,
    #[serde(default)]
    collector: CollectorSection,
}

#[derive(Default, Deserialize)]
struct HookSection {
    bind: Option<String>,
}

#[derive(Default, Deserialize)]
struct WebSection {
    bind: Option<String>,
    root: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
struct CollectorSection {
    idle_exit_seconds: Option<u64>,
}

impl Config {
    /// 讀真正的環境變數與 `<home>/config.toml`。
    pub fn load(cli: &Overrides) -> Result<Config, String> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let home = home_dir(cli, &env, default_home());
        let file = match std::fs::read_to_string(home.join("config.toml")) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(format!(
                    "讀不到 {}：{e}",
                    home.join("config.toml").display()
                ));
            }
        };
        Config::resolve(cli, &env, file.as_deref(), default_home())
    }

    pub fn resolve(
        cli: &Overrides,
        env: &dyn Fn(&str) -> Option<String>,
        file: Option<&str>,
        default_home: PathBuf,
    ) -> Result<Config, String> {
        let home = home_dir(cli, env, default_home);
        let f: FileConfig = match file {
            Some(text) => toml::from_str(text).map_err(|e| format!("config.toml 格式錯誤：{e}"))?,
            None => FileConfig::default(),
        };
        let pick = |cli: &Option<String>, key: &str, file: &Option<String>, default: &str| {
            cli.clone()
                .or_else(|| env(key))
                .or_else(|| file.clone())
                .unwrap_or_else(|| default.to_string())
        };
        let addr = |s: String, what: &str| {
            s.parse::<SocketAddr>()
                .map_err(|_| format!("{what} 不是合法的位址：{s}"))
        };
        let hook_bind = addr(
            pick(
                &cli.hook_bind,
                "CODING_LAB_HOOK_BIND",
                &f.hook.bind,
                DEFAULT_HOOK_BIND,
            ),
            "[hook].bind",
        )?;
        let web_bind = addr(
            pick(
                &cli.web_bind,
                "CODING_LAB_WEB_BIND",
                &f.web.bind,
                DEFAULT_WEB_BIND,
            ),
            "[web].bind",
        )?;
        let web_root = cli
            .web_root
            .clone()
            .or_else(|| env("CODING_LAB_WEB_ROOT").map(PathBuf::from))
            .or(f.web.root);
        let idle_exit_seconds = match (cli.idle_exit_seconds, env("CODING_LAB_IDLE_EXIT_SECONDS")) {
            (Some(v), _) => v,
            (None, Some(v)) => v
                .parse()
                .map_err(|_| format!("CODING_LAB_IDLE_EXIT_SECONDS 不是非負整數：{v}"))?,
            (None, None) => f
                .collector
                .idle_exit_seconds
                .unwrap_or(DEFAULT_IDLE_EXIT_SECONDS),
        };

        if !hook_bind.ip().is_loopback() {
            return Err(format!(
                "[hook].bind 只能是 127.0.0.1（只給同機的 Claude Code），收到 {hook_bind}"
            ));
        }
        if !web_bind.ip().is_loopback() {
            return Err(format!(
                "[web].bind 在金鑰驗證（P6）完成前只能是 127.0.0.1，收到 {web_bind}；要從別台看請用 SSH 通道：ssh -N -L 4801:127.0.0.1:4801 <伺服器>"
            ));
        }
        Ok(Config {
            home,
            hook_bind,
            web_bind,
            web_root,
            idle_exit_seconds,
        })
    }
}

fn home_dir(
    cli: &Overrides,
    env: &dyn Fn(&str) -> Option<String>,
    default_home: PathBuf,
) -> PathBuf {
    cli.home
        .clone()
        .or_else(|| env("CODING_LAB_HOME").map(PathBuf::from))
        .unwrap_or(default_home)
}

/// `~/.coding-lab`（Windows 是 `%USERPROFILE%\.coding-lab`）。
pub fn default_home() -> PathBuf {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    Path::new(&base).join(".coding-lab")
}
