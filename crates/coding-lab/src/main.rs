use std::io::{IsTerminal, Read, Write};
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
    /// 安裝或移除常駐服務
    Service,
    /// 遠端查看用的金鑰：create／list／revoke（存 <home>/keys.json）
    Key(KeyArgs),
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
        Command::Serve(args) => run_serve(&args.overrides()),
        // 不變式 4：ensure 失敗也不能讓 Claude Code 看到錯誤，stdout 也不能有任何輸出
        Command::Ensure => {
            if let Err(e) = ensure() {
                append_log(&format!("ensure：{e}"));
            }
            ExitCode::SUCCESS
        }
        Command::Config(args) => match Config::load(&args.overrides()) {
            Ok(c) => {
                println!("{c:#?}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("coding-lab：{e}");
                ExitCode::from(2)
            }
        },
        Command::Healthz => healthz(),
        Command::Stop => stop(),
        Command::Prune => prune(),
        Command::Service => {
            eprintln!("coding-lab：此子命令尚未實作");
            ExitCode::FAILURE
        }
        Command::Key(args) => key(args.command),
    }
}

/// `coding-lab key …`（#666）。collector 每次驗證都重讀 keys.json，所以這裡直接改檔就生效，不必重啟。
fn key(cmd: KeyCommand) -> ExitCode {
    match key_inner(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("coding-lab：{e}");
            ExitCode::FAILURE
        }
    }
}

fn key_inner(cmd: KeyCommand) -> Result<(), String> {
    let cfg = Config::load(&Overrides::default())?;
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

fn run_serve(overrides: &Overrides) -> ExitCode {
    let cfg = match Config::load(overrides) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("coding-lab：{e}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("coding-lab：無法建立 runtime：{e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(serve::run(cfg)) {
        Ok(Outcome::Stopped) => ExitCode::SUCCESS,
        Ok(Outcome::AlreadyRunning) => {
            eprintln!("coding-lab：已經有 collector 在跑");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("coding-lab：{e}");
            ExitCode::FAILURE
        }
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// ensure 整體最多花多久（plugin 的 SessionStart hook timeout 是 5 秒）
const ENSURE_BUDGET: Duration = Duration::from_secs(4);
const FORWARD_TIMEOUT: Duration = Duration::from_secs(1);

/// SessionStart command hook：確保 collector 在跑，再把 stdin 的 payload 轉送給它。
fn ensure() -> Result<(), String> {
    let cfg = Config::load(&Overrides::default())?;
    let mut body = Vec::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        stdin
            .lock()
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
    }
    // 轉送拿到 503，或 probe 之後埠就被放掉（連線被拒、還沒回應就被 RST），表示舊的 collector 正在閒置結束
    // （#685）：等它放掉埠，拉起新的再送一次。每一步的 timeout 都用剩下的預算夾住，整體不超過 hook timeout
    let deadline = Instant::now() + ENSURE_BUDGET;
    let mut spawned = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "collector {} 秒內沒有起來",
                ENSURE_BUDGET.as_secs()
            ));
        }
        if probe(cfg.hook_bind, left) {
            if body.is_empty() {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match forward(cfg.hook_bind, &body, left)? {
                Forwarded::Accepted => return Ok(()),
                Forwarded::Exiting => {}
            }
        } else if !spawned {
            spawn_collector(&cfg)?;
            spawned = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(PartialEq)]
enum Forwarded {
    Accepted,
    /// collector 正在結束、或連線在送達前就斷了：確定沒有收下，可以再送
    Exiting,
}

fn probe(addr: SocketAddr, left: Duration) -> bool {
    !left.is_zero() && TcpStream::connect_timeout(&addr, PROBE_TIMEOUT.min(left)).is_ok()
}

fn spawn_collector(cfg: &Config) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.home).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.home.join("collector.log"))
        .map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
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
    let Ok(mut s) = TcpStream::connect_timeout(&addr, step(PROBE_TIMEOUT)) else {
        return Ok(Forwarded::Exiting);
    };
    s.set_write_timeout(Some(step(FORWARD_TIMEOUT)))
        .map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(step(FORWARD_TIMEOUT)))
        .map_err(|e| e.to_string())?;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-Coding-Lab-Via: ensure\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
    let home = Config::load(&Overrides::default())
        .map(|c| c.home)
        .unwrap_or_else(|_| coding_lab::config::default_home());
    let _ = std::fs::create_dir_all(&home);
    coding_lab::logging::append(&home.join("collector.log"), msg);
}

/// 清掉超過保留期限的 session。collector 在跑時不動（它啟動時與每天會自己清），避免兩個程序同時寫 store。
fn prune() -> ExitCode {
    let cfg = match Config::load(&Overrides::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("coding-lab：{e}");
            return ExitCode::from(2);
        }
    };
    let days = coding_lab::store::RETENTION_DAYS;
    let _lock = match serve::try_lock_store(&cfg.home) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "coding-lab：collector 正在執行，它啟動時與每天會自動清掉 {days} 天前的資料；要立刻清請先結束 collector"
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("coding-lab：{e}");
            return ExitCode::FAILURE;
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
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("coding-lab：{e}");
            ExitCode::FAILURE
        }
    }
}

/// `coding-lab stop`（#700）：對 hook 埠 `POST /stop`，等 collector 放掉埠、刪掉 collector.json 才回。
/// 背景 collector 沒有 console（Windows 用 CREATE_NO_WINDOW），這是三平台都通、不丟資料的停法；
/// 沒在跑時 exit 0 只印一句，手冊的更新與卸載步驟可以無條件先跑它。
fn stop() -> ExitCode {
    const STOP_WAIT: Duration = Duration::from_secs(15);
    let cfg = match Config::load(&Overrides::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("coding-lab：{e}");
            return ExitCode::from(2);
        }
    };
    let info = cfg.home.join("collector.json");
    let hook = std::fs::read_to_string(&info)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v["hook"]
                .as_str()
                .and_then(|w| w.parse::<SocketAddr>().ok())
        })
        .unwrap_or(cfg.hook_bind);
    let Ok(mut s) = TcpStream::connect_timeout(&hook, PROBE_TIMEOUT) else {
        eprintln!("coding-lab：collector 沒有在跑（{hook}）");
        return ExitCode::SUCCESS;
    };
    let _ = s.set_read_timeout(Some(FORWARD_TIMEOUT));
    let _ = s.set_write_timeout(Some(FORWARD_TIMEOUT));
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {hook}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        coding_lab::hooks::STOP_PATH
    );
    let mut resp = [0u8; 64];
    let accepted = s.write_all(req.as_bytes()).is_ok()
        && matches!(s.read(&mut resp), Ok(n) if resp[..n].starts_with(b"HTTP/1.1 200"));
    drop(s);
    if !accepted {
        eprintln!(
            "coding-lab：collector 沒有接受 stop（{hook}）；請直接結束 coding-lab serve 這個程序"
        );
        return ExitCode::FAILURE;
    }
    // 埠放掉之後它還在停 tail、flush：等 collector.json 被刪掉（它是最後一步）
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        let port_free = TcpStream::connect_timeout(&hook, PROBE_TIMEOUT).is_err();
        let info_gone = !info.exists();
        if port_free && info_gone {
            eprintln!("coding-lab：collector 已結束");
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "coding-lab：collector 收到 stop 但 {} 秒內還沒結束；看 {}",
        STOP_WAIT.as_secs(),
        cfg.home.join("collector.log").display()
    );
    ExitCode::FAILURE
}

fn healthz() -> ExitCode {
    let cfg = match Config::load(&Overrides::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("coding-lab：{e}");
            return ExitCode::from(2);
        }
    };
    let web = std::fs::read_to_string(cfg.home.join("collector.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["web"].as_str().and_then(|w| w.parse::<SocketAddr>().ok()))
        .unwrap_or(cfg.web_bind);
    let mut s = match TcpStream::connect_timeout(&web, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("coding-lab：collector 沒有在跑（{web}）");
            return ExitCode::FAILURE;
        }
    };
    let _ = s.set_read_timeout(Some(FORWARD_TIMEOUT));
    let _ = write!(
        s,
        "GET /healthz HTTP/1.1\r\nHost: {web}\r\nConnection: close\r\n\r\n"
    );
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    println!("{}", out.split("\r\n\r\n").nth(1).unwrap_or(&out));
    ExitCode::SUCCESS
}
