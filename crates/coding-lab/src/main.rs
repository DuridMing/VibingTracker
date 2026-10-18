use std::io::{BufRead, IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use coding_lab::config::{Config, Overrides};
use coding_lab::keys::{KeyStore, Scope, SystemRng};
use coding_lab::serve::{self, Outcome};

/// Claude Code 多 session 專案追蹤 collector。
#[derive(Parser)]
#[command(name = "coding-lab", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 前景執行 collector（hook 入口 4800、web／WS 4801）
    Serve(ServeArgs),
    /// 沒在跑就拉起 collector，並轉送 stdin 的 SessionStart；由 SessionStart hook 呼叫，永遠 exit 0
    Ensure,
    /// 顯示合併後的設定
    Config(ServeArgs),
    /// 清除超過保留期限的資料
    Prune,
    /// 查詢執行中 collector 的健康狀態
    Healthz,
    /// 請執行中的 collector 正常結束（放掉埠、寫完紀錄）；沒在跑也 exit 0
    Stop,
    /// 常駐服務：install／uninstall／status；Linux 用 systemd user unit、macOS 用 LaunchAgent、Windows 用工作排程器
    Service(ServiceArgs),
    /// 遠端查看用的金鑰：create／list／revoke／login-link（存 <home>/keys.json）
    Key(KeyArgs),
    /// 多機總覽：對 config.toml 的每個 [[hub.upstream]] 開 WS 合流，在本機開 web；`hub add` 新增一台
    Hub(HubArgs),
}

#[derive(Args)]
struct HubArgs {
    #[command(subcommand)]
    command: Option<HubCommand>,
    /// hub 的 web 入口（預設 [hub].bind 或 127.0.0.1:4802；只能是本機位址）
    #[arg(long)]
    bind: Option<String>,
}

#[derive(Subcommand)]
enum HubCommand {
    /// 在 config.toml 加一台上游；金鑰從終端機互動輸入（不出現在 shell 歷史），本機或 SSH 通道的上游可以不給
    Add {
        /// 顯示名稱（串流前綴的 [host]）
        name: String,
        /// 那台的 web 入口，例如 http://100.64.0.5:4801 或 http://127.0.0.1:14801
        url: String,
        /// 不問金鑰（那台是 local-open 且走 SSH 通道）
        #[arg(long)]
        no_key: bool,
    },
}

#[derive(Args)]
struct ServiceArgs {
    #[command(subcommand)]
    command: ServiceCommand,
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// 寫 unit 檔並啟用：開機（登入）後 collector 自動起來、不會閒置結束
    Install {
        /// 只印出會寫的檔與會跑的指令，不執行
        #[arg(long)]
        dry_run: bool,
    },
    /// 停掉並移除常駐服務（只刪 install 寫的檔）
    Uninstall {
        #[arg(long)]
        dry_run: bool,
    },
    /// 有沒有裝、collector 現在有沒有在跑
    Status,
}

#[derive(Args)]
struct KeyArgs {
    #[command(subcommand)]
    command: KeyCommand,
}

#[derive(Subcommand)]
enum KeyCommand {
    /// 建一把金鑰；明文只在這裡印一次
    Create {
        /// 這把金鑰給誰用（例如 laptop、phone）
        #[arg(long)]
        name: String,
        /// view：只能看；operate：另外可按「已確認」、改通知設定
        #[arg(long, default_value = "view")]
        scope: Scope,
        /// 多久後到期：90d、12h、30m 或 never
        #[arg(long, default_value = "never")]
        expires: String,
    },
    /// 列出所有金鑰（不含 secret）
    List,
    /// 撤銷一把金鑰（立即失效）
    Revoke {
        /// `key list` 顯示的 8 碼 id
        id: String,
    },
    /// 印一個 5 分鐘內、只能用一次的登入連結（在伺服器終端機上點一下就在本機登入，金鑰不經過網址列）
    LoginLink {
        /// 用哪一把金鑰（只有一把有效金鑰時可省略）
        #[arg(long)]
        key: Option<String>,
    },
}

#[derive(Args)]
struct ServeArgs {
    /// 資料目錄（預設 $CODING_LAB_HOME 或 ~/.coding-lab）
    #[arg(long)]
    home: Option<PathBuf>,
    #[arg(long)]
    hook_bind: Option<String>,
    #[arg(long)]
    web_bind: Option<String>,
    /// 前端靜態檔的資料夾
    #[arg(long)]
    web_root: Option<PathBuf>,
    /// 所有 session 都結束後，閒置多久就自動結束（秒，0＝不結束）
    #[arg(long)]
    idle_exit_seconds: Option<u64>,
}

impl ServeArgs {
    fn overrides(&self) -> Overrides {
        Overrides {
            home: self.home.clone(),
            hook_bind: self.hook_bind.clone(),
            web_bind: self.web_bind.clone(),
            web_root: self.web_root.clone(),
            idle_exit_seconds: self.idle_exit_seconds,
        }
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve(args) => run(|| run_serve(&args.overrides())),
        // 不變式 4：ensure 失敗也不能讓 Claude Code 看到錯誤，stdout 也不能有任何輸出
        Command::Ensure => {
            if let Err(e) = ensure() {
                append_log(&format!("ensure：{e}"));
            }
            ExitCode::SUCCESS
        }
        Command::Config(args) => run(|| {
            let c = config(&args.overrides())?;
            for w in &c.warnings {
                eprintln!("coding-lab：{w}");
            }
            println!("{c:#?}");
            Ok(ExitCode::SUCCESS)
        }),
        Command::Healthz => run(healthz),
        Command::Stop => run(stop),
        Command::Prune => run(prune),
        Command::Service(args) => service(args.command),
        Command::Key(args) => key(args.command),
        Command::Hub(args) => hub(args),
    }
}

/// 子命令失敗的原因（E-D7）：設定錯誤一律 exit 2，其他失敗 exit 1。
enum Failure {
    Config(String),
    Other(String),
}

impl From<String> for Failure {
    fn from(e: String) -> Failure {
        Failure::Other(e)
    }
}

impl From<&str> for Failure {
    fn from(e: &str) -> Failure {
        Failure::Other(e.to_string())
    }
}

/// 讀設定；讀不了算設定錯誤。
fn config(overrides: &Overrides) -> Result<Config, Failure> {
    Config::load(overrides).map_err(Failure::Config)
}

/// 跑一個子命令：錯誤印到 stderr，退出碼見 [`Failure`]。
fn run(f: impl FnOnce() -> Result<ExitCode, Failure>) -> ExitCode {
    match f() {
        Ok(code) => code,
        Err(Failure::Config(e)) => {
            eprintln!("coding-lab：{e}");
            ExitCode::from(2)
        }
        Err(Failure::Other(e)) => {
            eprintln!("coding-lab：{e}");
            ExitCode::FAILURE
        }
    }
}

/// `coding-lab hub [--bind]`／`hub add <name> <url>`（#663）
fn hub(args: HubArgs) -> ExitCode {
    run(|| {
        let cfg = config(&Overrides::default())?;
        hub_inner(cfg, args)?;
        Ok(ExitCode::SUCCESS)
    })
}

fn hub_inner(cfg: Config, args: HubArgs) -> Result<(), String> {
    match args.command {
        Some(HubCommand::Add { name, url, no_key }) => {
            let key = if no_key {
                None
            } else {
                let k = read_secret(&format!(
                    "{name} 的金鑰（在那台用 coding-lab key create 建立的 clk_…；本機或 SSH 通道的上游直接按 Enter）："
                ))?;
                let k = k.trim().to_string();
                (!k.is_empty()).then_some(k)
            };
            let path = cfg.home.join("config.toml");
            coding_lab::hub::append_upstream(&path, &name, &url, key.as_deref())?;
            println!(
                "已加入 {name}（{url}{}）到 {}；執行 coding-lab hub 開始看",
                if key.is_some() {
                    "，有金鑰"
                } else {
                    "，不用金鑰"
                },
                path.display()
            );
            Ok(())
        }
        None => {
            if cfg.hub_upstreams.is_empty() {
                return Err(format!(
                    "config.toml 裡沒有 [[hub.upstream]]；先 coding-lab hub add <名稱> <http://主機:4801>（{}）",
                    cfg.home.join("config.toml").display()
                ));
            }
            let bind: SocketAddr = match args.bind {
                Some(b) => b
                    .parse()
                    .map_err(|_| format!("--bind 不是合法的位址：{b}"))?,
                None => cfg.hub_bind,
            };
            if !bind.ip().is_loopback() {
                return Err(format!("hub 的 web 只能綁本機位址，收到 {bind}"));
            }
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads())
                .enable_all()
                .build()
                .map_err(|e| format!("無法建立 runtime：{e}"))?;
            // 127.0.0.1 是整台機器共用的：每次啟動產生新的 token，只印在這個終端機（W-M8）
            let token = coding_lab::hub::new_token(&mut coding_lab::keys::SystemRng);
            rt.block_on(async {
                let (addr, task) = coding_lab::hub::serve(
                    bind,
                    cfg.hub_upstreams.clone(),
                    cfg.web_root_or_bundled(),
                    token.clone(),
                )
                .await
                .map_err(|e| format!("hub 綁不到 {bind}：{e}"))?;
                eprintln!(
                    "coding-lab hub：http://{addr}/?token={token}（上游：{}）\n用這個網址開；hub 重新啟動後要用新印出的網址。Ctrl-C 結束",
                    cfg.hub_upstreams
                        .iter()
                        .map(|u| format!("{} {}", u.name, u.url))
                        .collect::<Vec<_>>()
                        .join("、")
                );
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = task => {}
                }
                Ok::<(), String>(())
            })
        }
    }
}

/// 從終端機讀一行機密（unix 用 stty 關回顯；其他平台照常讀，先提醒會顯示）
fn read_secret(prompt: &str) -> Result<String, String> {
    let stdin = std::io::stdin();
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    // 終端機：不回顯（三個平台都是，#743 W-L2）。rpassword 讀的是終端機本身，所以管線輸入（腳本、測試）照舊讀 stdin
    if stdin.is_terminal() {
        return rpassword::read_password().map_err(|e| format!("讀不到輸入：{e}"));
    }
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("讀不到輸入：{e}"))?;
    Ok(line)
}

/// `coding-lab service …`（#662）：Plan 由 service.rs 算，這裡只負責環境、印出與執行。
fn service(cmd: ServiceCommand) -> ExitCode {
    run(|| {
        let cfg = config(&Overrides::default())?;
        service_inner(cfg, cmd)?;
        Ok(ExitCode::SUCCESS)
    })
}

fn service_inner(cfg: Config, cmd: ServiceCommand) -> Result<(), String> {
    use coding_lab::service::{self, Platform};
    let platform = Platform::current().ok_or_else(|| {
        format!(
            "{} 沒有常駐服務的做法；請自己用系統的方式跑 coding-lab serve --idle-exit-seconds 0",
            std::env::consts::OS
        )
    })?;
    let ctx = service::Ctx::detect(&cfg.home)?;
    // 子命令裡 logging::log 要進 collector.log（#724：失敗訊息說「已寫進 log」就要真的在）
    let _ = std::fs::create_dir_all(&cfg.home);
    let _ = coding_lab::logging::to_file(&cfg.home.join("collector.log"));
    let describe = |plan: &service::Plan| {
        for (p, _) in &plan.files {
            println!("寫入 {}", p.display());
        }
        for p in &plan.removes {
            println!("刪除 {}", p.display());
        }
        for c in &plan.commands {
            println!("執行 {}", c.join(" "));
        }
    };
    match cmd {
        ServiceCommand::Install { dry_run } => {
            if let Some(w) = cfg.plugin_warning() {
                eprintln!("coding-lab：{w}");
            }
            let plan = service::plan_install(platform, &ctx);
            describe(&plan);
            if dry_run {
                for (p, c) in &plan.files {
                    println!("\n--- {} ---\n{c}", p.display());
                }
                return Ok(());
            }
            // 已經有 ensure 拉起的 collector 在跑時，服務會搶不到埠：先請它結束；登記失敗再把它拉回（#719）
            let was_running = matches!(
                coding_lab::serve::state(&cfg.home),
                coding_lab::serve::State::Up(_)
            );
            if was_running
                && std::process::Command::new(&ctx.exe)
                    .arg("stop")
                    .env("CODING_LAB_HOME", &cfg.home)
                    .status()
                    .map(|s| !s.success())
                    .unwrap_or(false)
            {
                eprintln!(
                    "coding-lab：舊的 collector 沒有停下來，服務可能綁不到埠；稍後用 service status 確認"
                );
            }
            if platform == Platform::Macos && service::installed(platform, &ctx) {
                // 重裝：先卸掉舊的（沒裝過時 bootout 會失敗，所以只在檔案在時做）
                let _ = service::run_command(&service::plan_uninstall(platform, &ctx).commands[0]);
            }
            let mut restore = || {
                if !was_running {
                    return;
                }
                // `ensure` 沒有 stdin 時就只是「沒在跑就拉起」
                let ok = std::process::Command::new(&ctx.exe)
                    .arg("ensure")
                    .env("CODING_LAB_HOME", &cfg.home)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                eprintln!(
                    "coding-lab：常駐服務沒有裝成，{}",
                    if ok {
                        "原本在跑的 collector 已重新拉起"
                    } else {
                        "而且拉不回原本的 collector；下一個 session 開始時會再拉起"
                    }
                );
            };
            service::install(&plan, &mut service::run_command, &mut restore, &mut |m| {
                coding_lab::logging::log(m)
            })?;
            println!(
                "已安裝常駐服務；web 入口 {}，hook 入口 {}",
                cfg.web_bind, cfg.hook_bind
            );
            for n in &plan.notes {
                println!("・{n}");
            }
            Ok(())
        }
        ServiceCommand::Uninstall { dry_run } => {
            let plan = service::plan_uninstall(platform, &ctx);
            describe(&plan);
            if dry_run {
                return Ok(());
            }
            let registered =
                service::query_installed(platform, &ctx, &mut service::run_command_capture_quiet);
            if !service::installed(platform, &ctx) {
                println!(
                    "沒有裝過常駐服務（{}）",
                    service::files_for(platform, &ctx)[0].display()
                );
            }
            // 停掉／取消註冊失敗（例如早就被手動移除）不該擋住刪檔：有登記才把失敗印出來，沒登記時停掉／刪除
            // 本來就會失敗，安靜略過（#724）
            let mut warn = Vec::new();
            service::apply(&plan, &mut |c| {
                if let Err(e) = service::run_command_capture_quiet(c)
                    && registered
                {
                    warn.push(format!("{} 失敗：{e}", c.join(" ")));
                }
                Ok(())
            })?;
            for w in warn {
                coding_lab::logging::log(&format!("service：uninstall {w}"));
                eprintln!("coding-lab：{w}（略過）");
            }
            for n in &plan.notes {
                println!("・{n}");
            }
            Ok(())
        }
        ServiceCommand::Status => {
            let files = service::files_for(platform, &ctx);
            // 以系統的登記為準（#719）：install 失敗後 unit／XML 檔可能還在
            let registered =
                service::query_installed(platform, &ctx, &mut service::run_command_capture_quiet);
            let file = service::installed(platform, &ctx);
            println!(
                "常駐服務：{}（{}：{}）",
                if registered {
                    "已安裝"
                } else if file {
                    "未登記（檔案還在，請重跑 service install 或 uninstall）"
                } else {
                    "未安裝"
                },
                service::status_command(platform, &ctx).join(" "),
                files[0].display()
            );
            let running = running_info(&cfg, Duration::ZERO);
            match running.as_ref().and_then(|i| i.hook) {
                Some(hook) => println!("collector：執行中（hook 入口 {hook}）"),
                None => println!("collector：沒有在跑"),
            }
            Ok(())
        }
    }
}

/// `coding-lab key …`（#666）。collector 每次驗證都重讀 keys.json，所以這裡直接改檔就生效，不必重啟。
fn key(cmd: KeyCommand) -> ExitCode {
    run(|| {
        let cfg = config(&Overrides::default())?;
        key_inner(cfg, cmd)?;
        Ok(ExitCode::SUCCESS)
    })
}

fn key_inner(cfg: Config, cmd: KeyCommand) -> Result<(), String> {
    let now = coding_lab::now_ms();
    let mut rng = SystemRng;
    let mut store = KeyStore::load(&cfg.home)?.unwrap_or_else(|| KeyStore::new(&mut rng));
    match cmd {
        KeyCommand::Create {
            name,
            scope,
            expires,
        } => {
            let expires_ms = coding_lab::keys::parse_expires(&expires, now)?;
            let token = store.create(&name, scope, expires_ms, now, &mut rng);
            store.save(&cfg.home)?;
            let id = store.list().last().map(|k| k.id.as_str()).unwrap_or("");
            println!("金鑰（只顯示這一次，請現在複製）：\n\n    {token}\n");
            println!(
                "id {id}　name {name}　scope {}　到期 {}",
                scope.as_str(),
                expires_ms.map_or("永不".to_string(), coding_lab::keys::fmt_utc)
            );
            println!(
                "瀏覽器：打開 web 入口時貼上；程式：Authorization: Bearer {}…；撤銷：coding-lab key revoke {id}",
                &token[..12.min(token.len())]
            );
            Ok(())
        }
        KeyCommand::List => {
            if store.list().is_empty() {
                println!("還沒有金鑰；用 coding-lab key create --name <名稱> 建一把");
                return Ok(());
            }
            println!(
                "{:<8}  {:<16}  {:<7}  {:<16}  {:<16}  {:<16}  狀態",
                "id", "name", "scope", "建立", "最後使用", "到期"
            );
            for k in store.list() {
                let status = match k.usable_at(now) {
                    Ok(()) => "有效".to_string(),
                    Err(e) => format!("{e}（已撤銷／已到期）"),
                };
                println!(
                    "{:<8}  {:<16}  {:<7}  {:<16}  {:<16}  {:<16}  {status}",
                    k.id,
                    k.name,
                    k.scope.as_str(),
                    coding_lab::keys::fmt_utc(k.created_ms),
                    k.last_used_ms
                        .map_or("—".to_string(), coding_lab::keys::fmt_utc),
                    k.expires_ms
                        .map_or("永不".to_string(), coding_lab::keys::fmt_utc),
                );
            }
            Ok(())
        }
        KeyCommand::LoginLink { key } => {
            let usable: Vec<&coding_lab::keys::KeyRecord> = store
                .list()
                .iter()
                .filter(|k| k.usable_at(now).is_ok())
                .collect();
            let id = match key {
                Some(id) => {
                    let k = store.get(&id).ok_or_else(|| {
                        format!("沒有 id 是 {id} 的金鑰；coding-lab key list 看現有的")
                    })?;
                    k.usable_at(now).map_err(|e| format!("金鑰 {id}：{e}"))?;
                    id
                }
                None => match usable.as_slice() {
                    [] => {
                        return Err(
                            "還沒有有效的金鑰；先 coding-lab key create --name <名稱>".into()
                        );
                    }
                    [one] => one.id.clone(),
                    many => {
                        return Err(format!(
                            "有 {} 把有效金鑰，請用 --key <id> 指定：{}",
                            many.len(),
                            many.iter()
                                .map(|k| format!("{}（{}）", k.id, k.name))
                                .collect::<Vec<_>>()
                                .join("、")
                        ));
                    }
                },
            };
            let code = coding_lab::auth::create_once_link(&cfg.home, &id, now, &mut rng)?;
            // 有綁區網／tailnet 位址就用它組網址（連結是給別台用的）；全是本機時才用 127.0.0.1 並提醒開 SSH 通道
            let link_bind = cfg
                .web_binds
                .iter()
                .find(|b| !b.ip().is_loopback())
                .copied()
                .unwrap_or(cfg.web_bind);
            let ip = link_bind.ip();
            let host = if ip.is_unspecified() {
                "<這台的位址>".to_string()
            } else if ip.is_ipv6() {
                format!("[{ip}]")
            } else {
                ip.to_string()
            };
            println!("http://{host}:{}/auth/once/{code}", link_bind.port());
            println!(
                "{} 分鐘內有效、只能用一次；用金鑰 {id}。",
                coding_lab::auth::ONCE_TTL_MS / 60_000
            );
            if ip.is_loopback() {
                println!(
                    "web 入口綁在 {ip}：請先在本機開 SSH 通道（ssh -N -L {0}:127.0.0.1:{0} <伺服器>）再打開這個網址。",
                    link_bind.port()
                );
            }
            Ok(())
        }
        KeyCommand::Revoke { id } => {
            if store.get(&id).is_none() {
                return Err(format!(
                    "沒有 id 是 {id} 的金鑰；coding-lab key list 看現有的"
                ));
            }
            if store.revoke(&id, now) {
                store.save(&cfg.home)?;
                println!("已撤銷 {id}，持有這把金鑰的連線下一次請求起失效");
            } else {
                println!("{id} 先前已撤銷");
            }
            Ok(())
        }
    }
}

fn run_serve(overrides: &Overrides) -> Result<ExitCode, Failure> {
    let cfg = config(overrides)?;
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("coding-lab：無法建立 runtime：{e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    match rt.block_on(serve::run(cfg)) {
        Ok(Outcome::Stopped) => Ok(ExitCode::SUCCESS),
        Ok(Outcome::AlreadyRunning) => {
            eprintln!("coding-lab：已經有 collector 在跑");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("coding-lab：{e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// 連本機的 collector：它在跑（拿著鎖）時連線不到 1 ms；沒在聽的埠在 Windows 要約 2 秒才回拒絕，不能等那麼久（EW2）
const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);
/// ensure 整體最多花多久（plugin 的 SessionStart hook timeout 是 5 秒）
const ENSURE_BUDGET: Duration = Duration::from_secs(4);
const FORWARD_TIMEOUT: Duration = Duration::from_secs(1);

/// SessionStart command hook：確保 collector 在跑，再把 stdin 的 payload 轉送給它。
///
/// 在不在跑看 store 鎖、位址看 collector.json、起不來看 collector.failed（contracts §5），不靠 TCP 探測猜。
/// 轉送拿到 503 或連線斷掉，表示舊的 collector 正在結束（#685）：拉起新的，新的看到 closing 會等舊的放掉 hook 埠、
/// 綁上之後先收 hook，所以照樣送到同一個位址就好。每一步的 timeout 都用剩下的預算夾住，整體不超過 hook timeout。
fn ensure() -> Result<(), String> {
    use coding_lab::serve::{self, State};
    let cfg = Config::load(&Overrides::default())?;
    let mut body = Vec::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        stdin
            .lock()
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
    }
    let home = &cfg.home;
    // L-1：最近起不來過就不再拉起，不讓每個 SessionStart 都等滿預算
    let fresh = coding_lab::now_ms().saturating_sub(serve::FAILED_FRESH_MS);
    if let Some(e) = serve::failed_since(home, fresh) {
        return Err(format!(
            "collector 最近起不來，{} 秒內不再拉起：{e}",
            serve::FAILED_FRESH_MS / 1000
        ));
    }
    let deadline = Instant::now() + ENSURE_BUDGET;
    // 拉起 collector 的時間（epoch ms）：之後寫的 collector.failed 就是它的
    let mut spawned: Option<u64> = None;
    let spawn = |spawned: &mut Option<u64>| -> Result<(), String> {
        if spawned.is_none() {
            *spawned = Some(coding_lab::now_ms());
            spawn_collector(&cfg)?;
        }
        Ok(())
    };
    // 我們拉起的 collector 用這份設定，位址就是 cfg.hook_bind
    let mut target = cfg.hook_bind;
    loop {
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err(format!(
                "collector {} 秒內沒有起來",
                ENSURE_BUDGET.as_secs()
            ));
        }
        if let Some(e) = spawned.and_then(|t| serve::failed_since(home, t)) {
            return Err(format!("collector 起不來：{e}"));
        }
        match serve::state(home) {
            State::Up(Some(info)) => {
                if let Some(a) = info.hook {
                    target = a;
                }
                if info.closing {
                    spawn(&mut spawned)?;
                } else if body.is_empty() {
                    return Ok(());
                }
            }
            // 啟動中（綁好 hook 埠、還沒寫 collector.json）或結束中（刪了 collector.json、還沒放鎖）：
            // 多拉起一個沒關係，在跑的那個會讓它以「已經有 collector」結束
            State::Up(None) | State::Down => spawn(&mut spawned)?,
        }
        if body.is_empty() {
            if spawned.is_some() && matches!(serve::state(home), State::Up(_)) {
                return Ok(());
            }
        } else {
            let left = deadline.saturating_duration_since(Instant::now());
            match forward(target, &body, left)? {
                Forwarded::Accepted => return Ok(()),
                Forwarded::Exiting => {}
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(PartialEq)]
enum Forwarded {
    Accepted,
    /// collector 正在結束、或連線在送達前就斷了：確定沒有收下，可以再送
    Exiting,
}

fn spawn_collector(cfg: &Config) -> Result<(), String> {
    coding_lab::store::create_private_dir(&cfg.home).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = coding_lab::store::open_private_append(&cfg.home.join("collector.log"))
        .map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    // 工作目錄用 home，不沿用第一個 session 的專案目錄：Windows 上程序的 cwd 會鎖住那個資料夾（L-9）
    cmd.arg("serve")
        .current_dir(&cfg.home)
        .env("CODING_LAB_HOME", &cfg.home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // 脫離 Claude Code 的程序群組：Claude Code 結束或收到 Ctrl-C 時 collector 不跟著死
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Windows 用 CREATE_NO_WINDOW 而不是 DETACHED_PROCESS：沒有 console 的 collector 再叫 git 等 console 程式時，
    // 系統會替每個子程序開新的 CMD 視窗；給它一個隱藏的 console，子程序就沿用它、不會彈窗
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        no_inherit_std_handles();
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("拉不起 collector：{e}"))
}

/// Windows 的 `Command` 一律用 `bInheritHandles = TRUE`：ensure 自己的 stdin／stdout／stderr 如果是呼叫端給的
/// 可繼承 pipe，會被複製進 collector，呼叫端要等到 collector 結束才收到 EOF（#712）。上面給 collector 的
/// NUL／log 只決定它自己的三個標準 handle，擋不住這個繼承，所以 spawn 前先把 ensure 這個程序的三個 handle
/// 標成不可繼承。只改旗標、不關 handle；失敗（例如 handle 無效）就照舊，不影響 ensure exit 0。
///
/// `unsafe`：stable Rust 沒有安全 API 能改 handle 的繼承旗標（`raw_attribute` 對應的
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 還在 nightly），只有這一個 FFI 呼叫、只在 Windows 編譯；
/// 不拉 `windows-sys`（Cargo.lock 已有兩個版本，不想再多一個直接依賴）。理由寫在 PLAN.md「跨平台」。
#[cfg(windows)]
fn no_inherit_std_handles() {
    use std::os::windows::io::AsRawHandle;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    for h in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        if !h.is_null() {
            // SAFETY：h 是這個程序自己還開著的標準 handle；只改旗標，不關、不複製
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

/// 把 SessionStart 轉送到 `/hook`（實測 SessionStart 不送 http hook）。
///
/// 連線被拒、寫入失敗、還沒回應就斷線（EOF、RST）都表示 collector 沒收下，回 [`Forwarded::Exiting`] 讓呼叫端再送；
/// 讀取逾時不重送：對方可能已經收下，重送會多一個 SessionStart。
fn forward(addr: SocketAddr, body: &[u8], left: Duration) -> Result<Forwarded, String> {
    use std::io::ErrorKind;
    let step = |d: Duration| d.min(left).max(Duration::from_millis(1));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, step(CONNECT_TIMEOUT)) else {
        return Ok(Forwarded::Exiting);
    };
    s.set_write_timeout(Some(step(FORWARD_TIMEOUT)))
        .map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(step(FORWARD_TIMEOUT)))
        .map_err(|e| e.to_string())?;
    // 和 plugin 的 http hook 一樣帶使用者：4800 被別的帳號的 collector 佔著時它會丟掉（#777）
    let user = coding_lab::hooks::current_user()
        .filter(|u| !u.contains(['\r', '\n']))
        .map(|u| format!("{}: {u}\r\n", coding_lab::hooks::USER_HEADER))
        .unwrap_or_default();
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-Coding-Lab-Via: ensure\r\n{user}Content-Length: {}\r\nConnection: close\r\n\r\n",
        coding_lab::hooks::HOOK_PATH,
        body.len()
    );
    if s.write_all(head.as_bytes()).is_err() || s.write_all(body).is_err() {
        return Ok(Forwarded::Exiting);
    }
    let mut resp = [0u8; 64];
    let n = match s.read(&mut resp) {
        Ok(0) => return Ok(Forwarded::Exiting),
        Ok(n) => n,
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            return Err(format!("collector 沒有回應：{e}"));
        }
        Err(_) => return Ok(Forwarded::Exiting),
    };
    if resp[..n].starts_with(b"HTTP/1.1 200") {
        Ok(Forwarded::Accepted)
    } else if resp[..n].starts_with(b"HTTP/1.1 503") {
        Ok(Forwarded::Exiting)
    } else {
        Err("collector 回應不是 200".into())
    }
}

fn append_log(msg: &str) {
    // 不讀 config.toml：設定壞掉正是常見的原因，那時也要寫進 CODING_LAB_HOME 而不是預設的 home
    let home = coding_lab::config::resolve_home(&Overrides::default());
    let _ = coding_lab::store::create_private_dir(&home);
    coding_lab::logging::append(&home.join("collector.log"), msg);
}

/// 清掉超過保留期限的 session。collector 在跑時不動（它啟動時與每天會自己清），避免兩個程序同時寫 store。
fn prune() -> Result<ExitCode, Failure> {
    let cfg = config(&Overrides::default())?;
    let days = coding_lab::store::RETENTION_DAYS;
    let _lock = match serve::try_lock_store(&cfg.home) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "coding-lab：collector 正在執行，它啟動時與每天會自動清掉 {days} 天前的資料；要立刻清請先結束 collector"
            );
            return Ok(ExitCode::FAILURE);
        }
        Err(e) => {
            eprintln!("coding-lab：{e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let before = coding_lab::now_ms().saturating_sub(days * 24 * 60 * 60 * 1000);
    let result = coding_lab::store::Store::open(&cfg.home).and_then(|mut s| s.prune(before));
    match result {
        Ok(r) => {
            println!(
                "清掉 {} 個超過 {days} 天的 session（{} KB）",
                r.sessions.len(),
                r.bytes / 1024
            );
            // 刪不掉的檔（被其他程式鎖住）索引照留，下次再清（L-17）
            for (sid, why) in &r.failed {
                eprintln!("coding-lab：{sid} 的事件檔刪不掉（{why}），下次再清");
            }
            Ok(if r.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err(e) => {
            eprintln!("coding-lab：{e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// 一次 HTTP 請求（`Connection: close`），回傳整個回應。只連 collector.json 寫的位址（R-11）。
fn http_once(addr: SocketAddr, request: &str) -> std::io::Result<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&addr, FORWARD_TIMEOUT)?;
    s.set_read_timeout(Some(FORWARD_TIMEOUT))?;
    s.set_write_timeout(Some(FORWARD_TIMEOUT))?;
    s.write_all(request.as_bytes())?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    Ok(out)
}

/// 執行中 collector 的位址（contracts §5）：拿著鎖、collector.json 有寫才算；啟動或結束到一半時等 `wait`。
/// 沒有 collector.json 時不退回設定裡的預設埠：那可能是別的資料目錄的 collector（R-11）。
/// serve 與 hub 的 tokio worker 數：核心數，最多 4（EW9）。預設等於邏輯核心數（24 核的機器閒置 29 條執行緒），
/// collector 用不到；hook 入口另有自己的執行緒（L-7）。
fn worker_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().min(4))
}

fn running_info(cfg: &Config, wait: Duration) -> Option<coding_lab::serve::Info> {
    use coding_lab::serve::State;
    let deadline = Instant::now() + wait;
    loop {
        match coding_lab::serve::state(&cfg.home) {
            State::Down => return None,
            State::Up(Some(info)) => return Some(info),
            State::Up(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            State::Up(None) => return None,
        }
    }
}

/// `coding-lab stop`（#700）：對 hook 埠 `POST /stop`，等 collector 關好 SQLite、刪掉 collector.json、放掉 store 鎖才回
/// （L-13：回報結束時新的 collector 馬上就能開 store）。背景 collector 沒有 console（Windows 用 CREATE_NO_WINDOW），
/// 這是三平台都通、不丟資料的停法；沒在跑時 exit 0 只印一句，手冊的更新與卸載步驟可以無條件先跑它。
fn stop() -> Result<ExitCode, Failure> {
    const STOP_WAIT: Duration = Duration::from_secs(15);
    let cfg = config(&Overrides::default())?;
    let Some((pid, hook, token)) = running_info(&cfg, STOP_WAIT).and_then(|i| {
        i.hook
            .map(|h| (i.pid, h, i.stop_token.unwrap_or_else(|| "1".into())))
    }) else {
        eprintln!("coding-lab：collector 沒有在跑（{}）", cfg.home.display());
        return Ok(ExitCode::SUCCESS);
    };
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {hook}\r\n{}: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        coding_lab::hooks::STOP_PATH,
        coding_lab::hooks::STOP_HEADER
    );
    if !http_once(hook, &req).is_ok_and(|r| r.starts_with(b"HTTP/1.1 200")) {
        eprintln!(
            "coding-lab：collector 沒有接受 stop（{hook}）；請直接結束 coding-lab serve 這個程序"
        );
        return Ok(ExitCode::FAILURE);
    }
    // 它刪 collector.json 時 SQLite 已經關好，緊接著放鎖。鎖一放掉可能馬上被新的 collector 拿走，
    // 所以 collector.json 不見之後鎖還被拿著一陣子、或換成別的 pid 寫的，也算它結束了
    const HANDED_OVER: Duration = Duration::from_secs(1);
    let deadline = Instant::now() + STOP_WAIT;
    let mut gone_at: Option<Instant> = None;
    while Instant::now() < deadline {
        use coding_lab::serve::State;
        let done = match coding_lab::serve::state(&cfg.home) {
            State::Down => true,
            State::Up(Some(i)) => i.pid != pid,
            State::Up(None) => gone_at.get_or_insert_with(Instant::now).elapsed() >= HANDED_OVER,
        };
        if done {
            eprintln!("coding-lab：collector 已結束");
            service_start_hint(&cfg.home);
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "coding-lab：collector 收到 stop 但 {} 秒內還沒結束；看 {}",
        STOP_WAIT.as_secs(),
        cfg.home.join("collector.log").display()
    );
    Ok(ExitCode::FAILURE)
}

/// 裝了常駐服務時，stop 停掉的就是它，而且不會自己回來（R-6）：提示怎麼叫回來。服務管理員自己呼叫
/// （systemd 的 ExecStop）時 stderr 不是終端機，不印。
fn service_start_hint(home: &std::path::Path) {
    use coding_lab::service::{self, Platform};
    if !std::io::stderr().is_terminal() {
        return;
    }
    let (Some(platform), Ok(ctx)) = (Platform::current(), service::Ctx::detect(home)) else {
        return;
    };
    if service::installed(platform, &ctx) {
        eprintln!(
            "coding-lab：常駐服務也跟著停了，不會自己回來；要叫回來：{}（之後的 session 也會照常拉起一個閒置會結束的 collector）",
            service::start_command(platform, &ctx).join(" ")
        );
    }
}

fn healthz() -> Result<ExitCode, Failure> {
    let cfg = config(&Overrides::default())?;
    let Some(web) = running_info(&cfg, Duration::from_secs(2)).and_then(|i| i.web) else {
        eprintln!("coding-lab：collector 沒有在跑（{}）", cfg.home.display());
        return Ok(ExitCode::FAILURE);
    };
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {web}\r\nConnection: close\r\n\r\n");
    match http_once(web, &req) {
        Ok(out) => {
            let out = String::from_utf8_lossy(&out);
            println!("{}", out.split("\r\n\r\n").nth(1).unwrap_or(&out));
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("coding-lab：連不上 collector（{web}）：{e}");
            Ok(ExitCode::FAILURE)
        }
    }
}
