use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use serde_json::json;

use hook_capture::http::{OK_RESPONSE, read_request};
use hook_capture::settings::settings_json;
use hook_capture::store::{
    Via, append_json, parse_payload, record_hook, record_hook_at, sanitize, session_dir,
};
use hook_capture::tail::{Tail, snapshot};

/// 階段 0：收集 Claude Code hook payload 與 transcript 行的到達時間。
#[derive(Parser)]
#[command(name = "hook-capture", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 接收 http hook 並 tail transcript（前景執行，Ctrl-C 結束）
    Serve {
        #[arg(long, default_value_t = 4899)]
        port: u16,
        /// 輸出目錄（預設 $HOOK_CAPTURE_OUT 或 ~/.cache/coding-lab-capture）
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// command hook 版：從 stdin 讀 payload，永遠 exit 0、不輸出任何東西
    Cmd,
    /// 印出掛全部事件的 settings JSON，放進測試專案的 .claude/settings.local.json
    Settings {
        #[arg(long, default_value_t = 4899)]
        port: u16,
        /// command hook 要呼叫的執行檔（預設為目前這支）
        #[arg(long)]
        bin: Option<String>,
    },
    /// SessionEnd 沒觸發時，手動把某個 session 的 transcript 複製進樣本目錄
    Snapshot {
        session_id: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn default_out() -> PathBuf {
    if let Some(p) = std::env::var_os("HOOK_CAPTURE_OUT") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    PathBuf::from(home.unwrap_or_default()).join(".cache/coding-lab-capture")
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Action::Serve { port, out } => serve(port, out.unwrap_or_else(default_out)),
        Action::Cmd => {
            let mut body = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut body);
            if let Err(e) = record_hook(&default_out(), Via::Command, &body) {
                eprintln!("hook-capture cmd: {e}");
            }
            ExitCode::SUCCESS
        }
        Action::Settings { port, bin } => {
            let bin = bin.unwrap_or_else(|| {
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "hook-capture".into())
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&settings_json(port, &bin)).unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Action::Snapshot { session_id, out } => {
            let dir = session_dir(&out.unwrap_or_else(default_out), &session_id);
            let meta = std::fs::read(dir.join("meta.jsonl")).unwrap_or_default();
            let last = String::from_utf8_lossy(&meta)
                .lines()
                .last()
                .map(str::to_string);
            match last.and_then(|l| parse_payload(l.as_bytes())) {
                Some(m) if m["transcript_path"].is_string() => {
                    snapshot(
                        Path::new(m["transcript_path"].as_str().unwrap_or_default()),
                        &dir,
                    );
                    ExitCode::SUCCESS
                }
                _ => {
                    eprintln!("找不到 {} 的 meta.jsonl", dir.display());
                    ExitCode::FAILURE
                }
            }
        }
    }
}

type Tails = Arc<Mutex<HashMap<String, Tail>>>;

fn serve(port: u16, out: PathBuf) -> ExitCode {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hook-capture: bind 127.0.0.1:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("hook-capture：127.0.0.1:{port} → {}", out.display());
    let out = Arc::new(out);
    let tails: Tails = Arc::default();
    for stream in listener.incoming().flatten() {
        let (out, tails) = (out.clone(), tails.clone());
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &out, &tails) {
                eprintln!("hook-capture: {e}");
            }
        });
    }
    ExitCode::SUCCESS
}

fn handle(mut stream: TcpStream, out: &Path, tails: &Tails) -> std::io::Result<()> {
    let req = read_request(&mut BufReader::new(&stream))?;
    let ts = hook_capture::now_ms();
    stream.write_all(OK_RESPONSE)?;
    drop(stream);

    let sid = record_hook_at(out, Via::Http, &req.body, ts)?;
    let Some(p) = parse_payload(&req.body) else {
        return Ok(());
    };
    let dir = session_dir(out, &sid);
    let event = p["hook_event_name"].as_str().unwrap_or_default();
    let mut map = tails.lock().unwrap_or_else(|e| e.into_inner());

    // SessionStart 不送 http hook（實測），所以任何事件第一次出現就開始 tail 主 transcript。
    if let Some(path) = p["transcript_path"].as_str() {
        let path = PathBuf::from(path);
        if map.get(&sid).is_none_or(|t| t.transcript != path) {
            if let Some(old) = map.remove(&sid) {
                old.finish();
            }
            append_json(
                &dir.join("meta.jsonl"),
                &json!({ "ts_seen_ms": ts, "first_event": event, "transcript_path": path, "cwd": p["cwd"] }),
            )?;
            let arrivals = dir.join("transcript-arrivals.jsonl");
            map.insert(sid.clone(), Tail::start(path, arrivals, Some(dir.clone())));
        }
    }

    let agent_key = p["agent_id"]
        .as_str()
        .map(|a| format!("{sid}/{}", sanitize(a)));
    match (event, agent_key) {
        ("SubagentStart", Some(key)) => {
            if let (Some(t), Some(agent)) = (p["transcript_path"].as_str(), p["agent_id"].as_str())
            {
                let t = Path::new(t);
                let stem = t.file_stem().unwrap_or_default();
                let sub = t
                    .with_file_name(stem)
                    .join(format!("subagents/agent-{agent}.jsonl"));
                let arrivals = dir.join(format!("subagent-{}-arrivals.jsonl", sanitize(agent)));
                if let Some(old) = map.insert(key, Tail::start(sub, arrivals, None)) {
                    old.finish();
                }
            }
        }
        ("SubagentStop", Some(key)) => {
            if let Some(t) = map.remove(&key) {
                t.finish();
            }
        }
        ("SessionEnd", _) => {
            let prefix = format!("{sid}/");
            let keys: Vec<String> = map
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            for k in keys {
                if let Some(t) = map.remove(&k) {
                    t.finish();
                }
            }
            // 主 transcript 最後收：它的快照會連 subagents/ 一起複製
            if let Some(t) = map.remove(&sid) {
                t.finish();
            }
        }
        _ => {}
    }
    Ok(())
}
