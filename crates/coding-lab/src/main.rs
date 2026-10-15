use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    Serve,
    /// 沒在跑就拉起 collector；由 SessionStart hook 呼叫，永遠 exit 0
    Ensure,
    /// 顯示合併後的設定
    Config,
    /// 清除超過保留期限的資料
    Prune,
    /// 查詢執行中 collector 的健康狀態
    Healthz,
    /// 安裝或移除常駐服務
    Service,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        // 不變式 4：ensure 失敗也不能讓 Claude Code 看到錯誤。
        Command::Ensure => {
            eprintln!("coding-lab ensure：尚未實作");
            ExitCode::SUCCESS
        }
        Command::Serve | Command::Config | Command::Prune | Command::Healthz | Command::Service => {
            eprintln!("coding-lab：此子命令尚未實作");
            ExitCode::FAILURE
        }
    }
}
