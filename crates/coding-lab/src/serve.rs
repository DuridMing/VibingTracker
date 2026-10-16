//! `coding-lab serve`：把 hooks、transcript tail、merge、store、ws 組成一個 collector（#629）。
//!
//! 先綁 hook 埠——這同時是單例鎖：綁不到就表示已有 collector 在跑，直接結束；綁到了才開 store，
//! 避免兩個程序同時寫 SQLite。主迴圈是 merge 的唯一消費者（PLAN「collector 設計」）：
//! hook 與 transcript 的輸入都經同一個 channel 進來，同一個 session 內的順序因此固定。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::event::Event;
use crate::hooks::serve_hooks;
use crate::merge::{Input, Merger};
use crate::prices::Prices;
use crate::rules::RuleSet;
use crate::store::Store;
use crate::transcript::{TailAction, TailHandle, TailOptions, TailPlanner, spawn_tail, stop_all};
use crate::ws::{WebState, serve_web};

/// 重啟時從 store 重建多久以內的 live 狀態（涵蓋任何時區的「今天」）。
const REBUILD_WINDOW_MS: u64 = 36 * 60 * 60 * 1000;
const TICK: Duration = Duration::from_millis(500);
const STORE_FLUSH: Duration = Duration::from_millis(100);
/// sessions 列的分數多久重算一次（契約 §6）
const SCORE_EVERY: Duration = Duration::from_secs(10);
/// 費用暴增的基準多久重算一次（#675）
const COST_BASELINE_EVERY: Duration = Duration::from_secs(60 * 60);
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const INPUT_QUEUE: usize = 8192;
/// 進行中的 session 沒送 SessionEnd（終端機被直接關掉）時，最多再撐這麼久沒有 hook 才結束。
const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const FRAME_QUEUE: usize = 256;

pub enum Outcome {
    Stopped,
    /// hook 埠已被佔用（通常是另一個 collector）
    AlreadyRunning,
}

/// 依 ts_seen 排序的事件重建「還開著的 session」。結束後只有 session.started（resume）會重新打開：
/// tail 最後一次讀到的行、關 tail 時的 transcript.recovered 都排在 session.ended 之後。
pub fn open_sessions(events: &[Event]) -> HashSet<String> {
    let mut open = HashSet::new();
    let mut ended = HashSet::new();
    for e in events {
        match e.kind.as_str() {
            "session.ended" => {
                open.remove(&e.session);
                ended.insert(e.session.clone());
            }
            "session.started" => {
                ended.remove(&e.session);
                open.insert(e.session.clone());
            }
            _ if !ended.contains(&e.session) => {
                open.insert(e.session.clone());
            }
            _ => {}
        }
    }
    open
}

pub fn log(msg: &str) {
    eprintln!("[{}] coding-lab：{msg}", crate::now_ms());
}

pub async fn run(cfg: Config) -> io::Result<Outcome> {
    std::fs::create_dir_all(&cfg.home)?;
    let (tx, mut rx) = mpsc::channel::<Input>(INPUT_QUEUE);
    let hook_addr = match serve_hooks(cfg.hook_bind, tx.clone()).await {
        Ok((addr, _task)) => addr,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Ok(Outcome::AlreadyRunning),
        Err(e) => return Err(e),
    };

    let store = Store::open(&cfg.home).map_err(|e| io::Error::other(e.to_string()))?;
    let recent = store
        .recent_events(crate::now_ms().saturating_sub(REBUILD_WINDOW_MS))
        .map_err(|e| io::Error::other(e.to_string()))?;
    let store = Arc::new(Mutex::new(store));
    // 閒置結束只在沒有進行中的 session 時才算（PLAN「10 分鐘無 session 自動退出」）：session 還開著時
    // 結束的話，使用者回來一輸入 http hook 就連不上，Claude Code 會顯示 hook 錯誤。
    let mut open_sessions = open_sessions(&recent);

    if let Some(w) = cfg.plugin_warning() {
        log(&w);
    }
    let (rules, warning) = RuleSet::load(&cfg.home.join("rules.json"));
    if let Some(w) = warning {
        log(&w);
    }

    let web = WebState::new(Some(store.clone()), cfg.web_root.clone(), FRAME_QUEUE);
    web.rebuild(&recent);
    // ack 不是 Event，重放會讓已確認的注意列跑回來：依 store 的佇列對齊
    match store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open_attention()
    {
        Ok(rows) => web.keep_attention(
            &rows
                .iter()
                .map(|a| format!("{}/{}/{}", a.session, a.kind, a.reference))
                .collect(),
        ),
        Err(e) => log(&format!("讀取注意佇列失敗：{e}")),
    }
    web.flush(crate::now_ms());
    let (web_addr, _web_task) = serve_web(cfg.web_bind, web.clone()).await?;
    let _flusher = web.spawn_flusher();

    let info = cfg.home.join("collector.json");
    let me = std::process::id();
    std::fs::write(
        &info,
        json!({
            "pid": me, "hook": hook_addr.to_string(), "web": web_addr.to_string(),
            "version": env!("CARGO_PKG_VERSION"), "startedAt": crate::now_ms(),
        })
        .to_string(),
    )?;
    log(&format!(
        "hook {hook_addr}、web http://{web_addr}、資料 {}",
        cfg.home.display()
    ));

    let mut merger = Merger::new(Box::new(rules));
    let (prices, warning) = Prices::load(&cfg.home.join("prices.json"));
    if let Some(w) = warning {
        log(&w);
    }
    merger.set_prices(prices);
    {
        let counters = store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counters()
            .map_err(|e| io::Error::other(e.to_string()))?;
        for (sid, c) in counters {
            merger.resume(&sid, c.seq, c.pos, &c.cwd);
        }
    }
    // 費用暴增（#675）：基準＋補回各 session 最近 24 小時的花費
    let now = crate::now_ms();
    let usage = cost_usage(&store, now);
    merger.set_cost_baseline(crate::status::cost_baseline(&usage, now));
    for (sid, at, usd) in usage.iter().filter(|(_, at, _)| at + DAY_MS >= now) {
        merger.seed_cost(sid, *at, *usd);
    }
    let mut planner = TailPlanner::default();
    let mut tails: HashMap<(String, String), TailHandle> = HashMap::new();
    let mut tick = tokio::time::interval(TICK);
    let mut flush = tokio::time::interval(STORE_FLUSH);
    let mut rescore = tokio::time::interval(SCORE_EVERY);
    let mut rebase = tokio::time::interval(COST_BASELINE_EVERY);
    rebase.tick().await; // 第一次立即觸發，啟動時已經算過
    let idle = Duration::from_secs(cfg.idle_exit_seconds);
    let mut last_hook = Instant::now();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let sink = |events: Vec<Event>| {
        if events.is_empty() {
            return;
        }
        {
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            for e in &events {
                s.append(e.clone());
            }
        }
        web.apply(&events);
    };

    loop {
        tokio::select! {
            Some(input) = rx.recv() => {
                if let Input::Hook { hook, .. } = &input {
                    last_hook = Instant::now();
                    if hook.event_name() == "SessionEnd" {
                        open_sessions.remove(hook.session_id());
                    } else {
                        open_sessions.insert(hook.session_id().to_string());
                    }
                    let actions = planner.on_hook(hook);
                    if !actions.is_empty() {
                        for action in actions {
                            sink(merger.handle(action.reset(crate::now_ms())));
                            apply_tail(action, &mut tails, &tx);
                        }
                        web.set_health(json!({ "tails": tails.len(), "schedLagMs": 0 }));
                    }
                }
                sink(merger.handle(input));
            }
            scheduled = tick.tick() => {
                let lag = Instant::now().saturating_duration_since(scheduled.into_std());
                sink(merger.tick(crate::now_ms()));
                web.set_health(json!({ "tails": tails.len(), "schedLagMs": lag.as_millis() as u64 }));
                let quiet = last_hook.elapsed();
                if !idle.is_zero()
                    && quiet >= idle
                    && (open_sessions.is_empty() || quiet >= ABANDONED_AFTER.max(idle))
                {
                    log("閒置，自動結束");
                    break;
                }
            }
            _ = rebase.tick() => {
                let now = crate::now_ms();
                merger.set_cost_baseline(crate::status::cost_baseline(&cost_usage(&store, now), now));
            }
            _ = rescore.tick() => {
                // 只更新彙總列，不發 session.status（那是串流裡的狀態行，只在狀態改變時發）
                web.set_scores(&merger.scores(crate::now_ms()));
            }
            _ = flush.tick() => {
                if let Err(e) = store.lock().unwrap_or_else(|e| e.into_inner()).flush() {
                    log(&format!("寫入 store 失敗：{e}"));
                }
            }
            _ = &mut shutdown => {
                log("收到結束訊號");
                break;
            }
        }
    }

    stop_all(tails.into_values(), &mut rx, |input| {
        sink(merger.handle(input))
    })
    .await;
    if let Err(e) = store.lock().unwrap_or_else(|e| e.into_inner()).flush() {
        log(&format!("寫入 store 失敗：{e}"));
    }
    remove_info_if_mine(&info, me);
    Ok(Outcome::Stopped)
}

fn apply_tail(
    action: TailAction,
    tails: &mut HashMap<(String, String), TailHandle>,
    tx: &mpsc::Sender<Input>,
) {
    let stop = |h: TailHandle| {
        tokio::spawn(h.stop());
    };
    match action {
        TailAction::Start {
            session,
            file,
            path,
            start,
        } => {
            if let Some(old) = tails.remove(&(session.clone(), file.clone())) {
                stop(old);
            }
            let h = spawn_tail(
                path,
                session.clone(),
                file.clone(),
                start,
                tx.clone(),
                TailOptions::default(),
            );
            tails.insert((session, file), h);
        }
        TailAction::Stop { session, file } => {
            if let Some(h) = tails.remove(&(session, file)) {
                stop(h);
            }
        }
        TailAction::StopSession { session } => {
            let keys: Vec<_> = tails
                .keys()
                .filter(|(s, _)| *s == session)
                .cloned()
                .collect();
            for k in keys {
                if let Some(h) = tails.remove(&k) {
                    stop(h);
                }
            }
        }
    }
}

/// 只刪自己寫的 collector.json（另一個 collector 可能已經接手）。
fn remove_info_if_mine(path: &Path, me: u32) {
    let mine = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| v["pid"] == me);
    if mine {
        let _ = std::fs::remove_file(path);
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return std::future::pending().await,
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// 費用基準要的 usage：最近 8 天（基準 7 天＋滾動 24 小時）；讀不到就當沒有資料（只寫 log）。
fn cost_usage(store: &Mutex<Store>, now: u64) -> Vec<(String, u64, f64)> {
    let since = now.saturating_sub((crate::status::COST_BASELINE_DAYS + 1) * DAY_MS);
    match store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .usage_since(since)
    {
        Ok(rows) => rows,
        Err(e) => {
            log(&format!("讀取費用紀錄失敗：{e}"));
            Vec::new()
        }
    }
}
