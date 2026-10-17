//! 設定（#629）：命令列 > `CODING_LAB_*` 環境變數 > `<home>/config.toml` > 預設。
//!
//! `[web].auth`、`remote_thinking`、`allow_insecure` 是遠端查看（P6，PLAN「網路綁定」第 4 點）的設定。
//! hook 入口永遠只能綁 loopback（不變式 6）；web 入口可以綁私有／tailnet 網段，公開 IP 要明確 `allow_insecure`。

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_HOOK_BIND: &str = "127.0.0.1:4800";
pub const DEFAULT_WEB_BIND: &str = "127.0.0.1:4801";
pub const DEFAULT_IDLE_EXIT_SECONDS: u64 = 600;
/// store 批次 flush 的間隔（transcript 行的 Event；hook 事件不等批次，#713）
pub const DEFAULT_STORE_FLUSH_MS: u64 = 100;

/// web 入口什麼時候要金鑰：`local-open`（預設）localhost 免金鑰；`always` 連 localhost 也要（多人共用的伺服器）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    LocalOpen,
    Always,
}

impl std::str::FromStr for AuthMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local-open" => Ok(AuthMode::LocalOpen),
            "always" => Ok(AuthMode::Always),
            _ => Err(format!("[web].auth 只能是 local-open 或 always，收到 {s}")),
        }
    }
}

/// 遠端連線看到的 thinking：`on_demand`（預設）點開才載入、`hide` 只有佔位、`show` 和本機一樣。只影響遠端連線。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteThinking {
    OnDemand,
    Hide,
    Show,
}

impl std::str::FromStr for RemoteThinking {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "on_demand" => Ok(RemoteThinking::OnDemand),
            "hide" => Ok(RemoteThinking::Hide),
            "show" => Ok(RemoteThinking::Show),
            _ => Err(format!(
                "[web].remote_thinking 只能是 on_demand、hide 或 show，收到 {s}"
            )),
        }
    }
}

/// 私有網段（RFC 1918）、tailnet／CGNAT（100.64/10）、link-local、IPv6 ULA：不會被網際網路上的人直接連到，
/// 明文 http 的風險只剩同網段竊聽（tailnet 本身有 WireGuard 加密）。
pub fn is_private_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || v.is_link_local() || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v) => {
            let s = v.segments()[0];
            (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
    }
}

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
    /// store 的批次 flush 間隔（毫秒，≥ 1）；只影響 transcript 行的 Event，hook 事件處理完就寫（#713）
    pub store_flush_ms: u64,
    pub auth: AuthMode,
    pub remote_thinking: RemoteThinking,
    /// 公開 IP 又沒有 TLS 時也照綁（使用者自己承擔明文風險）
    pub allow_insecure: bool,
    /// `coding-lab hub` 的 web 入口（#663）；只能是 loopback（hub 免金鑰）
    pub hub_bind: SocketAddr,
    /// `[[hub.upstream]]`：hub 要看的每台 collector
    pub hub_upstreams: Vec<crate::hub::Upstream>,
}

#[derive(Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    hook: HookSection,
    #[serde(default)]
    web: WebSection,
    #[serde(default)]
    collector: CollectorSection,
    #[serde(default)]
    hub: HubSection,
}

#[derive(Default, Deserialize)]
struct HubSection {
    bind: Option<String>,
    #[serde(default)]
    upstream: Vec<crate::hub::Upstream>,
}

#[derive(Default, Deserialize)]
struct HookSection {
    bind: Option<String>,
}

#[derive(Default, Deserialize)]
struct WebSection {
    bind: Option<String>,
    root: Option<PathBuf>,
    auth: Option<String>,
    remote_thinking: Option<String>,
    allow_insecure: Option<bool>,
}

#[derive(Default, Deserialize)]
struct CollectorSection {
    idle_exit_seconds: Option<u64>,
    store_flush_ms: Option<u64>,
}

impl Config {
    /// plugin 的 hooks.json 寫死 POST 到 [`DEFAULT_HOOK_BIND`]（Claude Code 的 http hook 只有 headers
    /// 能內插環境變數，url 不行）；hook 埠改掉的話，除了經 `ensure` 轉送的 SessionStart 都會連不上。
    pub fn plugin_warning(&self) -> Option<String> {
        let plugin = DEFAULT_HOOK_BIND;
        (self.hook_bind.to_string() != plugin).then(|| {
            format!(
                "hook 入口是 {}，但 plugin 的 hook 固定送到 {plugin}：除了 SessionStart 以外的事件都收不到（改埠只適合測試）",
                self.hook_bind
            )
        })
    }

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
        let mut c = Config::resolve(cli, &env, file.as_deref(), default_home())?;
        if c.web_root.is_none() {
            c.web_root = std::env::current_exe().ok().and_then(|e| bundled_web(&e));
        }
        Ok(c)
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

        let store_flush_ms = match env("CODING_LAB_STORE_FLUSH_MS") {
            Some(v) => v
                .parse()
                .map_err(|_| format!("CODING_LAB_STORE_FLUSH_MS 不是正整數：{v}"))?,
            None => f.collector.store_flush_ms.unwrap_or(DEFAULT_STORE_FLUSH_MS),
        };
        if store_flush_ms == 0 {
            return Err("[collector].store_flush_ms 要大於 0".into());
        }
        let auth: AuthMode =
            pick(&None, "CODING_LAB_WEB_AUTH", &f.web.auth, "local-open").parse()?;
        let remote_thinking: RemoteThinking = pick(
            &None,
            "CODING_LAB_REMOTE_THINKING",
            &f.web.remote_thinking,
            "on_demand",
        )
        .parse()?;
        let allow_insecure = match env("CODING_LAB_ALLOW_INSECURE") {
            Some(v) => match v.as_str() {
                "1" | "true" | "yes" => true,
                "0" | "false" | "no" => false,
                _ => {
                    return Err(format!(
                        "CODING_LAB_ALLOW_INSECURE 要是 true 或 false，收到 {v}"
                    ));
                }
            },
            None => f.web.allow_insecure.unwrap_or(false),
        };

        if !hook_bind.ip().is_loopback() {
            return Err(format!(
                "[hook].bind 只能是 127.0.0.1（只給同機的 Claude Code），收到 {hook_bind}"
            ));
        }
        let ip = web_bind.ip();
        if !ip.is_loopback() && !is_private_network(ip) && !allow_insecure {
            return Err(format!(
                "[web].bind 是 {web_bind}：不是本機也不是私有／tailnet 網段，金鑰與 cookie 走明文 http 會被竊聽。\
                 建議綁 tailnet IP（100.x.x.x）或維持 127.0.0.1 走 SSH 通道：ssh -N -L 4801:127.0.0.1:4801 <伺服器>；\
                 前面有 TLS（Caddy）或接受風險時設 [web].allow_insecure = true"
            ));
        }
        let hub_bind = addr(
            pick(
                &None,
                "CODING_LAB_HUB_BIND",
                &f.hub.bind,
                crate::hub::DEFAULT_HUB_BIND,
            ),
            "[hub].bind",
        )?;
        if !hub_bind.ip().is_loopback() {
            return Err(format!(
                "[hub].bind 只能是本機位址（hub 的 web 免金鑰；要從別台看 hub 請走 SSH 通道），收到 {hub_bind}"
            ));
        }
        let mut names: Vec<&str> = Vec::new();
        for u in &f.hub.upstream {
            if names.contains(&u.name.as_str()) {
                return Err(format!("[[hub.upstream]] 的 name 重複：{}", u.name));
            }
            names.push(&u.name);
            crate::hub::ws_url(&u.url).map_err(|e| format!("[[hub.upstream]] {}：{e}", u.name))?;
        }
        Ok(Config {
            home,
            hook_bind,
            web_bind,
            web_root,
            idle_exit_seconds,
            store_flush_ms,
            auth,
            remote_thinking,
            allow_insecure,
            hub_bind,
            hub_upstreams: f.hub.upstream,
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

/// 二進位旁邊的 `web/`（有 index.html 才算）：wrapper 從 Release 下載時把兩者解在一起（#656）。
pub fn bundled_web(exe: &Path) -> Option<PathBuf> {
    let web = exe.parent()?.join("web");
    web.join("index.html").is_file().then_some(web)
}

/// `~/.coding-lab`（Windows 是 `%USERPROFILE%\.coding-lab`）。
pub fn default_home() -> PathBuf {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    Path::new(&base).join(".coding-lab")
}
