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
    /// 沒有 hook 進來多久就自動結束（秒，0＝不結束）
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
        Command::Prune | Command::Service => {
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
const START_WAIT: Duration = Duration::from_secs(3);
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
    if !probe(cfg.hook_bind) {
        spawn_collector(&cfg)?;
        let deadline = Instant::now() + START_WAIT;
        while !probe(cfg.hook_bind) {
            if Instant::now() > deadline {
                return Err("collector 3 秒內沒有起來".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if !body.is_empty() {
        forward(cfg.hook_bind, &body)?;
    }
    Ok(())
}

fn probe(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
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
fn forward(addr: SocketAddr, body: &[u8]) -> Result<(), String> {
    let mut s = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(FORWARD_TIMEOUT))
        .map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(FORWARD_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-Coding-Lab-Via: ensure\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        coding_lab::hooks::HOOK_PATH,
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(body).map_err(|e| e.to_string())?;
    let mut resp = [0u8; 64];
    let n = s
        .read(&mut resp)
        .map_err(|e| format!("collector 沒有回應：{e}"))?;
    if resp[..n].starts_with(b"HTTP/1.1 200") {
        Ok(())
    } else {
        Err("collector 回應不是 200".into())
    }
}

fn append_log(msg: &str) {
    let home = Config::load(&Overrides::default())
        .map(|c| c.home)
        .unwrap_or_else(|_| coding_lab::config::default_home());
    let _ = std::fs::create_dir_all(&home);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("collector.log"))
    {
        let _ = writeln!(f, "[{}] coding-lab：{msg}", coding_lab::now_ms());
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
