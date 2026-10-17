use std::io::{IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use coding_lab::config::{Config, Overrides};
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
    /// 安裝或移除常駐服務
    Service,
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
        Command::Prune => prune(),
        Command::Service => {
            eprintln!("coding-lab：此子命令尚未實作");
            ExitCode::FAILURE
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("拉不起 collector：{e}"))
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
