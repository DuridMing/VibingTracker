//! `coding-lab serve`：把 hooks、transcript tail、merge、store、ws 組成一個 collector（#629）。
//!
//! 先綁 hook 埠——這同時是單例鎖：綁不到就表示已有 collector 在跑，直接結束；綁到了再拿 store 鎖
//! （`collector.lock`）才開 store，避免兩個程序同時寫 SQLite。閒置結束時 hook 埠先放掉（#685），
//! 新的 collector 可能在舊的還在 flush 時就綁到埠，store 鎖讓它等舊的寫完。主迴圈是 merge 的唯一消費者（PLAN「collector 設計」）：
//! hook 與 transcript 的輸入都經同一個 channel 進來，同一個 session 內的順序因此固定。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::event::{AttentionKind, Event};
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
/// SubagentStart 後讀 meta.json 的重試間隔與次數（實測晚 40–60 ms 寫好；merge 最多等 1.5 秒）
const AGENT_META_EVERY: Duration = Duration::from_millis(25);
const AGENT_META_TRIES: u32 = 40;
/// 等上一個 collector 放掉 store 鎖最多多久（它只剩停 tail 與 flush）
const STORE_LOCK_WAIT: Duration = Duration::from_secs(10);
pub const STORE_LOCK: &str = "collector.lock";
/// 多久清一次超過保留期限的 session（啟動時先清一次）
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

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
    crate::logging::log(msg);
}

pub async fn run(cfg: Config) -> io::Result<Outcome> {
    std::fs::create_dir_all(&cfg.home)?;
    // 背景執行（ensure 拉起）時自己寫 collector.log 並輪替；前景跑時照舊印到終端機
    if !std::io::IsTerminal::is_terminal(&std::io::stderr())
        && let Err(e) = crate::logging::to_file(&cfg.home.join("collector.log"))
    {
        log(&format!("開不了 collector.log：{e}"));
    }
    let (tx, mut rx) = mpsc::channel::<Input>(INPUT_QUEUE);
    let hooks = match serve_hooks(cfg.hook_bind, tx.clone()).await {
        Ok(h) => h,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Ok(Outcome::AlreadyRunning),
        Err(e) => return Err(e),
    };
    let hook_addr = hooks.addr;
    let store_lock = match lock_store(&cfg.home).await {
        Ok(lock) => lock,
        Err(e) => {
            // 等鎖期間已經回 200 的 hook 還在佇列裡，結束就丟了：至少留下紀錄
            log(&format!(
                "{e}；放棄啟動，等鎖期間收到的 {} 個 hook 沒有寫入",
                INPUT_QUEUE - tx.capacity()
            ));
            return Err(e);
        }
    };

    let mut store = Store::open(&cfg.home).map_err(|e| io::Error::other(e.to_string()))?;
    // 啟動時先清超過保留期限的 session，再把計數交給 merge：反過來的話 merge 會把清掉的 session
    // 當成「被丟下」補發 session.ended，又把它寫回 store
    prune_store(&mut store);
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
    let open_attention = match store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open_attention()
    {
        Ok(rows) => rows,
        Err(e) => {
            log(&format!("讀取注意佇列失敗：{e}"));
            Vec::new()
        }
    };
    web.keep_attention(
        &open_attention
            .iter()
            .map(|a| format!("{}/{}/{}", a.session, a.kind, a.reference))
            .collect(),
    );
    web.flush(crate::now_ms());
    let (web_addr, web_task) = serve_web(cfg.web_bind, web.clone()).await?;
    let flusher = web.spawn_flusher();

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
            // 結束、中斷的狀態也種回（D3）：否則 idle 的 session 下一個 prompt 不送 run、
            // 已結束的被補花費建回記憶體後會被當成丟棄
            merger.resume_state(&sid, c.ended_at, c.status == "idle", c.last_seen);
        }
    }
    // 還沒解除的注意項目種回 merge（#679）：否則之後的 prompt、工具結束解不掉重啟前的列
    resume_attention(&mut merger, &open_attention, &recent);
    // 費用暴增（#675）：基準＋補回各 session 最近 24 小時的花費
    let now = crate::now_ms();
    let usage = cost_usage(&store, now);
    merger.set_cost_baseline(crate::status::cost_baseline(&usage, now));
    for (sid, at, usd) in usage.iter().filter(|(_, at, _)| at + DAY_MS >= now) {
        merger.seed_cost(sid, *at, *usd);
    }
    let mut planner = TailPlanner::default();
    // 重啟後從 store 記的行號接著讀（#684）：停機期間寫進 transcript 的行不漏
    match store.lock().unwrap_or_else(|e| e.into_inner()).tail_lines() {
        Ok(rows) => {
            for t in rows {
                planner.seed(&t.session, &t.file, t.path.as_deref(), t.line_no);
            }
        }
        Err(e) => log(&format!("讀取 tail 行號失敗，照舊從檔尾讀：{e}")),
    }
    let mut tails: HashMap<(String, String), TailHandle> = HashMap::new();
    // 每個 tail 一個編號：TailReset 帶新的編號，merge 不理會被取代的舊 tail 送來的降級（#684 R20）
    let mut next_tail: u64 = 1;
    let mut tick = tokio::time::interval(TICK);
    let mut flush = tokio::time::interval(STORE_FLUSH);
    let mut rescore = tokio::time::interval(SCORE_EVERY);
    let mut prune = tokio::time::interval(PRUNE_EVERY);
    prune.tick().await; // 第一次立即觸發，啟動時已經清過
    let mut rebase = tokio::time::interval(COST_BASELINE_EVERY);
    rebase.tick().await; // 第一次立即觸發，啟動時已經算過
    let idle = Duration::from_secs(cfg.idle_exit_seconds);
    let mut last_hook = Instant::now();
    // 決定閒置結束的時間；多等一個 tick，期間有 hook 進來就取消（佇列裡可能已有拿到 200 的 hook）
    let mut closing_since: Option<Instant> = None;
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
                if let Input::Line { session, file, no, .. } = &input {
                    planner.saw_line(session, file, *no);
                }
                if let Input::Hook { hook, .. } = &input {
                    last_hook = Instant::now();
                    if hook.event_name() == "SessionEnd" {
                        open_sessions.remove(hook.session_id());
                    } else {
                        open_sessions.insert(hook.session_id().to_string());
                    }
                    if hook.event_name() == "SubagentStart" {
                        read_agent_meta(hook, &tx);
                    }
                    let actions = planner.on_hook(hook);
                    if !actions.is_empty() {
                        for action in actions {
                            let id = next_tail;
                            next_tail += 1;
                            sink(merger.handle(action.reset(crate::now_ms(), id)));
                            apply_tail(action, id, &mut tails, &tx);
                        }
                        web.set_health(json!({ "tails": tails.len(), "schedLagMs": 0 }));
                    }
                }
                sink(merger.handle(input));
            }
            scheduled = tick.tick() => {
                let lag = Instant::now().saturating_duration_since(scheduled.into_std());
                let events = merger.tick(crate::now_ms());
                // merge 結束了被丟棄的 session（#679）：閒置結束不必再等它
                for e in events.iter().filter(|e| e.kind == "session.ended") {
                    open_sessions.remove(&e.session);
                }
                sink(events);
                web.set_health(json!({ "tails": tails.len(), "schedLagMs": lag.as_millis() as u64 }));
                let quiet = last_hook.elapsed();
                let idle_now = !idle.is_zero()
                    && quiet >= idle
                    && (open_sessions.is_empty() || quiet >= ABANDONED_AFTER.max(idle));
                match closing_since {
                    Some(at) if idle_now && last_hook < at => {
                        log("閒置，自動結束");
                        break;
                    }
                    Some(_) => {
                        hooks.set_closing(false);
                        closing_since = None;
                    }
                    None if idle_now => {
                        hooks.set_closing(true);
                        closing_since = Some(Instant::now());
                    }
                    None => {}
                }
            }
            _ = rebase.tick() => {
                let now = crate::now_ms();
                merger.set_cost_baseline(crate::status::cost_baseline(&cost_usage(&store, now), now));
            }
            _ = prune.tick() => {
                let gone = prune_store(&mut store.lock().unwrap_or_else(|e| e.into_inner()));
                for sid in gone {
                    merger.forget(&sid);
                }
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

    // 先放掉 hook 埠：之後 ensure 的 probe 會失敗而拉起新的 collector（它會等 store 鎖）
    hooks.set_closing(true);
    hooks.close().await;
    stop_all(tails.into_values(), &mut rx, |input| {
        sink(merger.handle(input))
    })
    .await;
    if let Err(e) = store.lock().unwrap_or_else(|e| e.into_inner()).flush() {
        log(&format!("寫入 store 失敗：{e}"));
    }
    for task in [web_task, flusher] {
        task.abort();
        let _ = task.await;
    }
    remove_info_if_mine(&info, me);
    drop(store_lock);
    Ok(Outcome::Stopped)
}

/// 試拿 `<home>/collector.lock` 的獨佔鎖；別的程序拿著時回 `None`。`prune` 也用它避免和 collector 同時寫。
pub fn try_lock_store(home: &Path) -> io::Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join(STORE_LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// 清掉最後活動超過保留期限的 session，回傳清掉的 id（#655）。
fn prune_store(store: &mut Store) -> Vec<String> {
    let before = crate::now_ms().saturating_sub(crate::store::RETENTION_DAYS * DAY_MS);
    match store.prune(before) {
        Ok(r) => {
            if !r.sessions.is_empty() {
                log(&format!(
                    "清掉 {} 個超過 {} 天的 session（{} KB）",
                    r.sessions.len(),
                    crate::store::RETENTION_DAYS,
                    r.bytes / 1024
                ));
            }
            r.sessions
        }
        Err(e) => {
            log(&format!("清理舊資料失敗：{e}"));
            Vec::new()
        }
    }
}

/// 拿 store 鎖；上一個 collector 還在結束時等它放掉。
async fn lock_store(home: &Path) -> io::Result<std::fs::File> {
    let deadline = Instant::now() + STORE_LOCK_WAIT;
    let mut waited = false;
    loop {
        match try_lock_store(home)? {
            Some(file) => return Ok(file),
            None if Instant::now() < deadline => {
                if !waited {
                    log("上一個 collector 還在結束，等它寫完 store");
                    waited = true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            None => {
                return Err(io::Error::other(format!(
                    "store 被另一個程序鎖住超過 {} 秒",
                    STORE_LOCK_WAIT.as_secs()
                )));
            }
        }
    }
}

/// 把 store 裡還沒解除的注意列交給 merge；fail 要知道是哪個 agent 的連續失敗，從重放的 Event 找，找不到當 main。
fn resume_attention(merger: &mut Merger, rows: &[crate::store::AttentionRow], recent: &[Event]) {
    let agent_of: HashMap<(&str, &str, &str), &str> = recent
        .iter()
        .filter(|e| e.kind == "attention.raised")
        .filter_map(|e| {
            let kind = e.payload["kind"].as_str()?;
            let r = e.payload["ref"].as_str()?;
            Some(((e.session.as_str(), kind, r), e.agent.as_str()))
        })
        .collect();
    for a in rows.iter().filter(|a| a.cleared_at.is_none()) {
        let Ok(kind) = serde_json::from_value::<AttentionKind>(json!(a.kind)) else {
            continue;
        };
        let agent = agent_of
            .get(&(a.session.as_str(), a.kind.as_str(), a.reference.as_str()))
            .copied()
            .unwrap_or("main");
        merger.resume_attention(&a.session, kind, &a.reference, agent, a.raised_at);
    }
}

fn apply_tail(
    action: TailAction,
    id: u64,
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
                TailOptions {
                    tail: id,
                    ..TailOptions::default()
                },
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

/// subagent 對應哪個 Agent／Task 呼叫：背景讀 meta.json，結果交給 merge（#680）。不擋主迴圈。
fn read_agent_meta(hook: &crate::hook::HookInput, tx: &mpsc::Sender<Input>) {
    let (Some(transcript), Some(agent)) = (
        hook.str_field("transcript_path").filter(|p| !p.is_empty()),
        hook.str_field("agent_id").filter(|a| !a.is_empty()),
    ) else {
        return;
    };
    let path = crate::transcript::subagent_file(Path::new(transcript), agent, "meta.json");
    let (session, agent, tx) = (hook.session_id().to_string(), agent.to_string(), tx.clone());
    tokio::spawn(async move {
        let meta = crate::transcript::read_agent_meta(path, AGENT_META_TRIES, || {
            tokio::time::sleep(AGENT_META_EVERY)
        })
        .await;
        // 讀不到也要說：merge 收到 Unreadable 就依呼叫順序配對，不必等滿上限
        let input = Input::AgentMeta {
            session,
            agent_id: agent,
            meta,
            ts_seen: crate::now_ms(),
        };
        let _ = tx.send(input).await;
    });
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
