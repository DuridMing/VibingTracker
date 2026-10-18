//! `coding-lab serve`：把 hooks、transcript tail、merge、store、ws 組成一個 collector（#629）。
//!
//! 先綁 hook 埠——這同時是單例鎖：綁不到就表示已有 collector 在跑，直接結束；綁到了再拿 store 鎖
//! （`collector.lock`）才開 store，避免兩個程序同時寫 SQLite。閒置結束時 hook 埠先放掉（#685），
//! 新的 collector 可能在舊的還在 flush 時就綁到埠，store 鎖讓它等舊的寫完。主迴圈是 merge 的唯一消費者（PLAN「collector 設計」）：
//! hook 與 transcript 的輸入都經同一個 channel 進來，同一個 session 內的順序因此固定。

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::event::{AttentionKind, Event};
use crate::hooks::{HookServer, serve_hooks};
use crate::merge::{Input, Merger};
use crate::prices::Prices;
use crate::rules::RuleSet;
use crate::store::Store;
use crate::transcript::{TailAction, TailHandle, TailOptions, TailPlanner, spawn_tail, stop_all};
use crate::ws::{WebState, serve_web};

/// 重啟時從 store 重建多久以內的 live 狀態（涵蓋任何時區的「今天」）。
const REBUILD_WINDOW_MS: u64 = 36 * 60 * 60 * 1000;
const TICK: Duration = Duration::from_millis(500);
/// sessions 列的分數多久重算一次（契約 §6）
const SCORE_EVERY: Duration = Duration::from_secs(10);
/// 費用暴增的基準多久重算一次（#675）
const COST_BASELINE_EVERY: Duration = Duration::from_secs(60 * 60);
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const INPUT_QUEUE: usize = 8192;
/// hook 的 channel（L-6）
const HOOK_QUEUE: usize = 4096;
/// 處理完一個 hook 後，最多再收掉幾個已經在佇列裡的才 flush（E-S7）
const HOOK_BATCH: usize = 256;
/// 進行中的 session 沒送 SessionEnd（終端機被直接關掉）時，最多再撐這麼久沒有 hook 才結束。
const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const FRAME_QUEUE: usize = 256;
/// SubagentStart 後讀 meta.json 的重試間隔與次數（實測晚 40–60 ms 寫好；merge 最多等 1.5 秒）
const AGENT_META_EVERY: Duration = Duration::from_millis(25);
const AGENT_META_TRIES: u32 = 40;
/// 等上一個 collector 放掉 store 鎖最多多久（它只剩停 tail 與 flush）
const STORE_LOCK_WAIT: Duration = Duration::from_secs(10);
pub const STORE_LOCK: &str = "collector.lock";
/// 執行中 collector 的位址（contracts §5）
pub const INFO_FILE: &str = "collector.json";
/// collector 起不來的紀錄（L-1）
pub const FAILED_FILE: &str = "collector.failed";
/// collector.failed 多久以內算「最近起不來」：這段時間 ensure 不再拉起
pub const FAILED_FRESH_MS: u64 = 60_000;
/// hook 埠被佔用、卻沒有 collector 拿著鎖：等這麼久還是這樣，就當成被別的程式佔走
/// （剛綁好埠、還沒拿到鎖的另一個 collector 也會短暫是這樣）
const FOREIGN_PORT_AFTER: Duration = Duration::from_secs(1);
/// web 埠綁不到時多久重試一次（L-4）
const WEB_RETRY: Duration = Duration::from_secs(2);
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

// ── collector 在不在跑（contracts §5、#738）：鎖＝在跑、collector.json＝位址、collector.failed＝起不來。
// ensure、stop、healthz、service status 都用這一套判斷，不靠 TCP 探測猜（Windows 連沒在聽的埠要約 2 秒才回拒絕）

/// collector.json 的內容。
#[derive(Clone, Debug)]
pub struct Info {
    pub pid: u64,
    pub hook: Option<SocketAddr>,
    /// web 埠還沒綁上時是 None（L-4）
    pub web: Option<SocketAddr>,
    /// 正在結束（#685）：hook 埠放掉後新的 collector 可以接手
    pub closing: bool,
    /// `POST /stop` 要帶的值（#777）；舊版沒寫時是 None
    pub stop_token: Option<String>,
}

pub fn read_info(home: &Path) -> Option<Info> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(INFO_FILE)).ok()?).ok()?;
    let addr = |k: &str| v[k].as_str().and_then(|a| a.parse().ok());
    Some(Info {
        pid: v["pid"].as_u64().unwrap_or(0),
        hook: addr("hook"),
        web: addr("web"),
        closing: v["closing"].as_bool().unwrap_or(false),
        stop_token: v["stopToken"].as_str().map(str::to_string),
    })
}

pub enum State {
    /// 沒有人拿著 store 鎖
    Down,
    /// 有 collector 拿著鎖；collector.json 還沒寫（啟動中）或已刪（結束中）時是 None
    Up(Option<Info>),
}

pub fn state(home: &Path) -> State {
    match try_lock_store(home) {
        Ok(Some(lock)) => {
            drop(lock);
            State::Down
        }
        Ok(None) => State::Up(read_info(home)),
        // 鎖檔開不了（目錄不在、權限）：只能看 collector.json
        Err(_) => read_info(home).map_or(State::Down, |i| State::Up(Some(i))),
    }
}

/// 記下起不來的原因：之後的 ensure 在 [`FAILED_FRESH_MS`] 內不再拉起（L-1），訊息帶位址或檔名。
pub fn write_failed(home: &Path, error: &str, addr: Option<SocketAddr>, file: Option<&Path>) {
    let mut v = json!({ "ts": crate::now_ms(), "error": error });
    if let Some(a) = addr {
        v["addr"] = json!(a.to_string());
    }
    if let Some(f) = file {
        v["file"] = json!(f.to_string_lossy().replace('\\', "/"));
    }
    log(&format!("起不來：{v}"));
    let _ = crate::store::write_private(&home.join(FAILED_FILE), v.to_string().as_bytes());
}

/// `since`（epoch ms）之後寫的 collector.failed 的錯誤訊息。
pub fn failed_since(home: &Path, since: u64) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(FAILED_FILE)).ok()?).ok()?;
    (v["ts"].as_u64()? >= since).then(|| v["error"].as_str().unwrap_or("").to_string())
}

fn write_info(home: &Path, me: u32, hooks: &HookServer, webs: &[SocketAddr], closing: bool) {
    let hook = hooks.addr;
    let v = json!({
        "pid": me, "hook": hook.to_string(), "web": webs.first().map(|a| a.to_string()),
        "webs": webs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "version": env!("CARGO_PKG_VERSION"), "startedAt": crate::now_ms(), "closing": closing,
        "stopToken": hooks.stop_token(),
    });
    if let Err(e) = crate::store::write_private(&home.join(INFO_FILE), v.to_string().as_bytes()) {
        log(&format!("寫不了 {INFO_FILE}：{e}"));
    }
}

/// `/healthz` 的動態部分（contracts §7）：`dropped` 是啟動以來因 hook 佇列滿丟掉的數量（L-6、E-D3）。
fn health(tails: usize, lag_ms: u64, hooks: &HookServer) -> serde_json::Value {
    json!({ "tails": tails, "schedLagMs": lag_ms, "dropped": hooks.dropped(), "user": hooks.user() })
}

/// 綁 hook 埠（這也是單例鎖）。被佔用時分三種：上一個 collector 正在結束（collector.json 標 closing，#685）
/// → 等它放掉再綁；另一個 collector 在跑 → `Ok(None)`；沒有 collector 卻被佔用（別的程式、別的資料目錄）→ 錯誤。
async fn bind_hooks(cfg: &Config, tx: &mpsc::Sender<Input>) -> io::Result<Option<HookServer>> {
    let deadline = Instant::now() + STORE_LOCK_WAIT;
    let mut down_since: Option<Instant> = None;
    loop {
        match serve_hooks(cfg.hook_bind, tx.clone()).await {
            Ok(h) => return Ok(Some(h)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => match state(&cfg.home) {
                State::Up(Some(i)) if i.closing && Instant::now() < deadline => down_since = None,
                State::Up(_) => return Ok(None),
                State::Down => {
                    if down_since.get_or_insert_with(Instant::now).elapsed() >= FOREIGN_PORT_AFTER {
                        return Err(io::Error::new(
                            e.kind(),
                            format!(
                                "{e}（這個帳號沒有 collector 在跑：埠被別的帳號的 collector 或別的程式佔用，這個帳號的 session 不會被追蹤）"
                            ),
                        ));
                    }
                }
            },
            Err(e) => return Err(e),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn run(cfg: Config) -> io::Result<Outcome> {
    crate::store::create_private_dir(&cfg.home)?;
    // 背景執行（ensure 拉起）時自己寫 collector.log 並輪替；前景跑時照舊印到終端機
    if !std::io::IsTerminal::is_terminal(&std::io::stderr())
        && let Err(e) = crate::logging::to_file(&cfg.home.join("collector.log"))
    {
        log(&format!("開不了 collector.log：{e}"));
    }
    crate::logging::log_panics();
    // 能預先知道會失敗的在綁 hook 埠之前檢查（L-1）：綁了就會回 200，之後才起不來的話那些 hook 就丟了
    if let Err(e) = crate::store::preflight(&cfg.home) {
        let msg = e.to_string();
        write_failed(&cfg.home, &msg, None, Some(&cfg.home.join("index.sqlite")));
        return Err(io::Error::other(msg));
    }
    let (tx, mut rx) = mpsc::channel::<Input>(INPUT_QUEUE);
    // hook 走自己的 channel（L-6）：tail 補讀把 rx 塞滿時 hook 不會被 try_send 丟掉
    let (hook_tx, mut hook_rx) = mpsc::channel::<Input>(HOOK_QUEUE);
    let hooks = match bind_hooks(&cfg, &hook_tx).await {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(Outcome::AlreadyRunning),
        Err(e) => {
            let msg = format!("hook 入口 {} 綁不到：{e}", cfg.hook_bind);
            write_failed(&cfg.home, &msg, Some(cfg.hook_bind), None);
            return Err(io::Error::new(e.kind(), msg));
        }
    };
    let hook_addr = hooks.addr;
    // 等鎖期間已經回 200 的 hook 還在佇列裡，起不來就丟了：至少留下紀錄
    let lost = |tx: &mpsc::Sender<Input>| HOOK_QUEUE - tx.capacity();
    let store_lock = match lock_store(&cfg.home).await {
        Ok(lock) => lock,
        Err(e) => {
            let msg = format!(
                "{e}；放棄啟動，等鎖期間收到的 {} 個 hook 沒有寫入",
                lost(&hook_tx)
            );
            write_failed(&cfg.home, &msg, None, Some(&cfg.home.join(STORE_LOCK)));
            return Err(io::Error::other(msg));
        }
    };

    let mut store = match Store::open(&cfg.home) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{e}；放棄啟動，收到的 {} 個 hook 沒有寫入", lost(&hook_tx));
            write_failed(&cfg.home, &msg, None, Some(&cfg.home.join("index.sqlite")));
            return Err(io::Error::other(msg));
        }
    };
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
    for w in &cfg.warnings {
        log(w);
    }
    let (rules, warning) = RuleSet::load(&cfg.home.join("rules.json"));
    if let Some(w) = warning {
        log(&w);
    }

    let web = WebState::new_with_auth(
        Some(store.clone()),
        cfg.web_root.clone(),
        FRAME_QUEUE,
        cfg.home.clone(),
        cfg.auth,
    );
    web.set_remote_thinking(cfg.remote_thinking);
    if cfg.web_binds.iter().any(|b| !b.ip().is_loopback()) && !web.auth().has_keys() {
        log(
            "web 入口綁在非本機位址，但還沒有任何金鑰：遠端連線都會被拒，請先 coding-lab key create --name <名稱>",
        );
    }
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
    // 每個 [web].bind 各一個 listener，共用同一個 WebState（#722）；第一個是主要位址。
    // 綁不到的不讓整個 collector 結束（已回 200 的 hook 會跟著丟），照收 hook、定期重試（L-4）
    let mut web_addrs = Vec::new();
    let mut web_tasks = Vec::new();
    let mut web_pending = Vec::new();
    for b in std::iter::once(cfg.web_bind).chain(cfg.web_binds.iter().skip(1).copied()) {
        match serve_web(b, web.clone()).await {
            Ok((a, t)) => {
                web_addrs.push(a);
                web_tasks.push(t);
            }
            Err(e) => {
                log(&format!(
                    "web 入口 {b} 綁不到：{e}；hook 照收，每 {} 秒重試",
                    WEB_RETRY.as_secs()
                ));
                web_pending.push(b);
            }
        }
    }
    let flusher = web.spawn_flusher();

    let me = std::process::id();
    write_info(&cfg.home, me, &hooks, &web_addrs, false);
    let _ = std::fs::remove_file(cfg.home.join(FAILED_FILE));
    log(&format!(
        "hook {hook_addr}、web {}、資料 {}",
        web_addrs
            .iter()
            .map(|a| format!("http://{a}"))
            .collect::<Vec<_>>()
            .join(" "),
        cfg.home.display()
    ));

    let mut merger = Merger::new(Box::new(rules));
    let (prices, warning) = Prices::load(&cfg.home.join("prices.json"));
    if let Some(w) = warning {
        log(&w);
    }
    merger.set_prices(prices);
    merger.set_wait_guess_ms(cfg.wait_guess_seconds.saturating_mul(1000));
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
    let now = crate::now_ms();
    // 還在暫存區的卡片種回（K-8）：tool_use 行在重啟後才到時不再發第二筆 tool.called
    merger.resume_staged(&recent, now);
    // 費用暴增（#675）：基準＋補回各 session 最近 24 小時的花費
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
    // transcript 行的 Event 批次寫（可從 transcript 補回來）；hook 事件處理完立刻寫，見下面（#713）
    let mut flush = tokio::time::interval(Duration::from_millis(cfg.store_flush_ms));
    let mut rescore = tokio::time::interval(SCORE_EVERY);
    let mut prune = tokio::time::interval(PRUNE_EVERY);
    prune.tick().await; // 第一次立即觸發，啟動時已經清過
    let mut web_retry = tokio::time::interval(WEB_RETRY);
    web_retry.tick().await;
    let mut rebase = tokio::time::interval(COST_BASELINE_EVERY);
    rebase.tick().await; // 第一次立即觸發，啟動時已經算過
    let idle = Duration::from_secs(cfg.idle_exit_seconds);
    let mut last_hook = Instant::now();
    // 決定閒置結束的時間；多等一個 tick，期間有 hook 進來就取消（佇列裡可能已有拿到 200 的 hook）
    let mut closing_since: Option<Instant> = None;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let stop = hooks.stop_requested();

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

    // 一個輸入（hook 或 tail 的行）交給 merge 前後要做的事；hook 與 tail 的兩條 channel 共用
    macro_rules! process {
        ($input:expr) => {{
            let input = $input;
            if let Input::Line { session, file, no, .. } = &input {
                planner.saw_line(session, file, *no);
                // 讀到的位置跟著這批 Event 一起寫進 store：沒產生 Event 的行重啟也不重讀（#713）
                store.lock().unwrap_or_else(|e| e.into_inner()).saw_line(session, file, *no);
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
                    web.set_health(health(tails.len(), 0, &hooks));
                }
            }
            sink(merger.handle(input));
        }};
    }
    loop {
        tokio::select! {
            // 依寫的順序挑：hook 永遠先處理（L-6）
            biased;
            // hook 先處理（L-6）：tail 補讀大量行時，hook 不會排在幾千行後面
            Some(input) = hook_rx.recv() => {
                process!(input);
                // 一串平行工具呼叫的 hook 合成一次 flush（E-S7）：只收已經在佇列裡的，不等待，仍符合 #713
                for _ in 0..HOOK_BATCH {
                    match hook_rx.try_recv() {
                        Ok(more) => process!(more),
                        Err(_) => break,
                    }
                }
                // hook 事件沒有別的來源可以補（transcript 的行有）：處理完立刻寫進 store，不等批次（#713）。
                // hook 的 200 早在 try_send 時就回了，這裡的寫入不在 Claude Code 的等待路徑上
                if let Err(e) = store.lock().unwrap_or_else(|e| e.into_inner()).flush() {
                    log(&format!("寫入 store 失敗：{e}"));
                }
            }
            Some(input) = rx.recv() => process!(input),
            scheduled = tick.tick() => {
                let lag = Instant::now().saturating_duration_since(scheduled.into_std());
                let events = merger.tick(crate::now_ms());
                // merge 結束了被丟棄的 session（#679）：閒置結束不必再等它
                for e in events.iter().filter(|e| e.kind == "session.ended") {
                    open_sessions.remove(&e.session);
                }
                sink(events);
                web.set_health(health(tails.len(), lag.as_millis() as u64, &hooks));
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
                        write_info(&cfg.home, me, &hooks, &web_addrs, false);
                        closing_since = None;
                    }
                    None if idle_now => {
                        hooks.set_closing(true);
                        // 新的 collector 看到 closing 才會等這個埠放掉、接手（#685）
                        write_info(&cfg.home, me, &hooks, &web_addrs, true);
                        closing_since = Some(Instant::now());
                    }
                    None => {}
                }
            }
            _ = web_retry.tick(), if !web_pending.is_empty() => {
                let before = web_pending.len();
                let mut still = Vec::new();
                for b in web_pending.drain(..) {
                    match serve_web(b, web.clone()).await {
                        Ok((a, t)) => {
                            log(&format!("web 入口 http://{a} 開始服務"));
                            web_addrs.push(a);
                            web_tasks.push(t);
                        }
                        Err(_) => still.push(b),
                    }
                }
                if still.len() < before {
                    write_info(&cfg.home, me, &hooks, &web_addrs, closing_since.is_some());
                }
                web_pending = still;
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
            // `coding-lab stop`（#700）：和閒置結束、Ctrl-C 同一條路——先放埠、停 tail、flush、刪 collector.json
            _ = stop.notified() => {
                log("stop：結束");
                break;
            }
        }
    }

    // 先放掉 hook 埠：ensure 會拉起新的 collector，它看到 closing 就等這個埠、綁上之後收 hook、等 store 鎖（#685）
    hooks.set_closing(true);
    write_info(&cfg.home, me, &hooks, &web_addrs, true);
    let dropped = hooks.dropped();
    hooks.close().await;
    // 埠放掉前已經收下（回了 200）的 hook 不能丟
    while let Ok(input) = hook_rx.try_recv() {
        sink(merger.handle(input));
    }
    if dropped > 0 {
        log(&format!("這次執行因佇列滿丟掉 {dropped} 個 hook"));
    }
    stop_all(tails.into_values(), &mut rx, |input| {
        sink(merger.handle(input))
    })
    .await;
    if let Err(e) = store.lock().unwrap_or_else(|e| e.into_inner()).flush() {
        log(&format!("寫入 store 失敗：{e}"));
    }
    for task in std::iter::once(flusher).chain(web_tasks) {
        task.abort();
        let _ = task.await;
    }
    // 先關 SQLite（最後一條連線關閉時做 checkpoint，I/O 忙時要好幾秒）才放 store 鎖：
    // 鎖一放掉 ensure 拉起的新 collector 就會開 store，不能撞上還在 checkpoint 的資料庫（#727）
    drop(web);
    match Arc::try_unwrap(store) {
        Ok(store) => drop(store.into_inner().unwrap_or_else(|e| e.into_inner())),
        Err(_) => log("結束時 store 還有別的持有者，放鎖前沒能先關閉 SQLite"),
    }
    // 順序：放 hook 埠 → flush、關 SQLite → 刪 collector.json → 放鎖（contracts §5、L-13）
    remove_info_if_mine(&cfg.home.join(INFO_FILE), me);
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
    let unc = crate::derive::is_unc(transcript);
    let (session, agent, tx) = (hook.session_id().to_string(), agent.to_string(), tx.clone());
    tokio::spawn(async move {
        // UNC 路徑不碰（#762 W-M5）：當成讀不到，merge 依呼叫順序配對
        let meta = if unc {
            crate::merge::AgentMetaRead::Unreadable
        } else {
            crate::transcript::read_agent_meta(path, AGENT_META_TRIES, || {
                tokio::time::sleep(AGENT_META_EVERY)
            })
            .await
        };
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
    // W-M4：關掉主控台視窗、登出、關機也要照常結束（flush、刪 collector.json）。tokio 收到這三種時讓
    // handler 停著不回，系統等幾秒（關視窗約 5 秒）才結束程序，結束流程就在這段時間裡跑完
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let (Ok(mut close), Ok(mut logoff), Ok(mut shutdown), Ok(mut brk)) = (
            windows::ctrl_close(),
            windows::ctrl_logoff(),
            windows::ctrl_shutdown(),
            windows::ctrl_break(),
        ) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = brk.recv() => {}
            _ = close.recv() => {}
            _ = logoff.recv() => {}
            _ = shutdown.recv() => {}
        }
    }
    #[cfg(not(any(unix, windows)))]
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
