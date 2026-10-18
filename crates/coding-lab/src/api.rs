//! web 入口的 HTTP API（contracts.md §7、#671）：Sessions tab、回放／載入更多、檔案樹與改動基準。
//!
//! 路由與 Host／Origin 檢查在 [`crate::ws::serve_web`]。git 只對 store 裡有 session 的目錄跑，
//! 而且一律關掉 fsmonitor 與 repo 設定的 filter：repo 設定裡的外部程式不能經由瀏覽器開的網頁被叫起來（#682）。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde_json::{Value, json};

use crate::event::Payload;
use crate::http_util::query;
use crate::live::{clip, short_model};
use crate::store::{SessionRow, Store};
use crate::ws::{MORE_LIMIT, Remote, WebState, more_events};

const SESSIONS_DEFAULT: usize = 100;
const SESSIONS_MAX: usize = 1000;
const EVENTS_DEFAULT: usize = 200;
const TREE_MAX: usize = 20_000;

fn reply(code: StatusCode, body: &Value) -> Response {
    crate::http_util::json(code, body)
}

fn ok(body: &Value) -> Response {
    reply(StatusCode::OK, body)
}

fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    reply(code, &json!({ "error": msg.into() }))
}

fn store_error(e: impl std::fmt::Display) -> Response {
    crate::logging::log(&format!("web：API 讀取 store 失敗：{e}"));
    error(StatusCode::INTERNAL_SERVER_ERROR, "讀取資料失敗")
}

fn limit(q: &HashMap<String, String>, default: usize, max: usize) -> usize {
    q.get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

/// `/` 分隔、去掉結尾的 `/`。
fn norm(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_string()
}

// ── /api/sessions

pub async fn sessions(
    State(state): State<Arc<WebState>>,
    Extension(Remote(remote)): Extension<Remote>,
    uri: Uri,
) -> Response {
    let q = query(&uri);
    let cwd = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty());
    let Some(store) = state.store().cloned() else {
        return ok(&json!({ "sessions": [] }));
    };
    let n = limit(&q, SESSIONS_DEFAULT, SESSIONS_MAX);
    // live 裡的 session 整列用 live 的（只有 model 可能沿用 store 的），最後一個 prompt 不用從 JSONL 讀（E-H3）
    let in_live = state.live().session_ids();
    // SQLite 與 JSONL 都會阻塞：放到 blocking 執行緒，不佔住 async worker（#683）。先讀完 store 再碰 live，不同時拿兩把鎖
    let stored =
        tokio::task::spawn_blocking(move || stored_rows(&store, cwd.as_deref(), n, &in_live)).await;
    let stored = match stored {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => return store_error(e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let live = state.live();
    let rows: Vec<Value> = stored
        .into_iter()
        .map(|(id, v)| match live.session_row(&id) {
            // 重啟後 live 列還沒看到 assistant 行時，模型用 store 的（#673）
            Some(mut row) => {
                if row["model"].is_null() {
                    row["model"] = v["model"].clone();
                    row["modelId"] = v["modelId"].clone();
                }
                row
            }
            None => v,
        })
        .collect();
    // 遠端連線：prompt 裡的 clk_ 金鑰、來源是 thinking 的 doing 都要剝（S-5、C2），和 WS 同一個剝除
    let mut body = json!({ "sessions": rows });
    state.redact_for(remote, &mut body);
    ok(&body)
}

/// store 的 sessions 列，組成與 WS 相同欄位（不在 live 狀態裡的 session 直接用）。
/// 最後一個 prompt 與 `lastPos` 用兩句批次查詢取，不是每列各查一次。
/// `skip`：live 裡已有的 session，它們的列會整列換成 live 的，不用查最後一個 prompt（E-H3）。
fn stored_rows(
    store: &std::sync::Mutex<Store>,
    cwd: Option<&str>,
    limit: usize,
    skip: &std::collections::HashSet<String>,
) -> Result<Vec<(String, Value)>, crate::store::StoreError> {
    // 鎖內只查 SQLite 與 offset，最後一個 prompt 的文字在鎖外讀 JSONL（L-7、X1）
    let (rows, mut extra, reads, reader) = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        let rows = s.sessions(cwd, limit)?;
        let ids: Vec<String> = rows
            .iter()
            .filter(|r| !skip.contains(&r.id))
            .map(|r| r.id.clone())
            .collect();
        let (extra, reads) = s.last_prompt_and_pos_reads(&ids)?;
        (rows, extra, reads, s.reader())
    };
    crate::store::fill_last_prompts(&mut extra, reader.read(&reads)?);
    Ok(rows
        .into_iter()
        .map(|r| {
            let (last_prompt, last_pos) = extra.get(&r.id).cloned().unwrap_or_default();
            let v = stored_row(&r, last_prompt.as_deref().map(clip), last_pos);
            (r.id, v)
        })
        .collect())
}

fn stored_row(r: &SessionRow, last_prompt: Option<String>, last_pos: Option<u64>) -> Value {
    json!({
        "id": r.id, "host": r.host, "cwd": r.cwd.replace('\\', "/"), "branch": r.branch,
        "model": r.model.as_deref().map(short_model), "modelId": r.model,
        "firstPrompt": r.first_prompt.as_deref().map(clip), "lastPrompt": last_prompt,
        "startedAt": r.started_at, "endedAt": r.ended_at,
        "status": r.status, "statusSince": r.status_since, "doing": Value::Null, "doingSource": Value::Null,
        "score": 0,
        "commits": r.commits, "dangers": r.dangers, "failures": r.failures,
        "agentsLive": 0, "costUsd": r.cost_usd, "thinkingTokens": r.thinking_tokens,
        "lastPos": last_pos,
        // 降級只在 live 裡追蹤（#652）；store 的列（多半是已結束的）沒有進行中的降級
        "degraded": [],
    })
}

// ── /api/sessions/<id>/events

pub async fn events(
    State(state): State<Arc<WebState>>,
    Extension(Remote(remote)): Extension<Remote>,
    UrlPath(id): UrlPath<String>,
    uri: Uri,
) -> Response {
    let q = query(&uri);
    let before = q.get("beforePos").and_then(|v| v.parse::<u64>().ok());
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有這個 session");
    };
    let n = limit(&q, EVENTS_DEFAULT, MORE_LIMIT as usize);
    // SQLite 與讀 JSONL 都會阻塞：放到 blocking 執行緒，讀 JSONL 時不握 store 鎖（L-7）
    tokio::task::spawn_blocking(move || {
        match store.lock().unwrap_or_else(|e| e.into_inner()).session(&id) {
            Ok(Some(_)) => {}
            Ok(None) => return error(StatusCode::NOT_FOUND, "沒有這個 session"),
            Err(e) => return store_error(e),
        }
        match more_events(&store, &id, before, n) {
            Ok(events) => {
                let mut body = json!({ "session": id, "beforePos": before, "events": events });
                // 遠端連線剝除敏感輸出（#654）與 thinking（#668），和 WS 一致
                state.redact_for(remote, &mut body);
                ok(&body)
            }
            Err(e) => store_error(e),
        }
    })
    .await
    .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ── /api/projects/replay（主頁時間軸回放，#729）

const REPLAY_DEFAULT_SPAN_MS: u64 = 60 * 60 * 1000;
/// 一次回放請求的範圍上限（#742 B-N1）：再長就一段一段要
const REPLAY_MAX_SPAN_MS: u64 = 24 * 60 * 60 * 1000;
const REPLAY_DEFAULT_LIMIT: usize = 2000;
const REPLAY_MAX_LIMIT: usize = 10_000;
/// 前端的 UTC 位移上限（分）：±14 小時
const MAX_OFFSET_MIN: i32 = 14 * 60;

/// 挑 session 的下界（C6）：`at` 前 2 小時（已結束的 session 在 live 留多久），或 `at` 那天的當地午夜，取早的——
/// KPI 的「今天」算的是當天所有 session，早上結束的 session 到了下午也要算進去。
fn replay_since(at: u64, offset_min: i32) -> u64 {
    let day = 24 * 60 * 60 * 1000_i64;
    let local = at as i64 + i64::from(offset_min) * 60_000;
    let midnight = (local.div_euclid(day) * day - i64::from(offset_min) * 60_000).max(0) as u64;
    at.saturating_sub(crate::live::ENDED_KEPT_MS).min(midnight)
}

/// `cwd`、`at`（epoch ms）必填；`until` 預設 at 之後 1 小時、最多 24 小時；`limit` 是這一段最多放幾個 Event；
/// `offset` 是前端的 UTC 位移（分，同 hello 的 utcOffsetMin），KPI 的「今天」依它算；`snapshot=0` 只要 frame（接續下一段）；
/// `from`：挑 session 的下界，續頁帶第一段回應的 `from`（沒帶時依 `at` 算，見 [`replay_since`]）。
/// 同時最多 [`crate::ws::MAX_REPLAYS`] 個回放請求，超過 429（#742 B-N1）。
pub async fn project_replay(
    State(state): State<Arc<WebState>>,
    Extension(Remote(remote)): Extension<Remote>,
    uri: Uri,
) -> Response {
    let q = query(&uri);
    let Some(cwd) = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "缺少 cwd（專案根目錄）");
    };
    let Some(Ok(at)) = q.get("at").map(|v| v.parse::<u64>()) else {
        return error(StatusCode::BAD_REQUEST, "at 要是 epoch ms");
    };
    let until = match q.get("until").map(|v| v.parse::<u64>()) {
        None => at.saturating_add(REPLAY_DEFAULT_SPAN_MS),
        Some(Ok(u)) => u.max(at),
        Some(Err(_)) => return error(StatusCode::BAD_REQUEST, "until 要是 epoch ms"),
    };
    if until - at > REPLAY_MAX_SPAN_MS {
        return error(StatusCode::BAD_REQUEST, "回放範圍一次最多 24 小時");
    }
    let offset = match q.get("offset").map(|v| v.parse::<i32>()) {
        None => 0,
        Some(Ok(o)) if (-MAX_OFFSET_MIN..=MAX_OFFSET_MIN).contains(&o) => o,
        Some(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "offset 是 UTC 位移（分，±14 小時以內）",
            );
        }
    };
    let since = match q.get("from").map(|v| v.parse::<u64>()) {
        None => replay_since(at, offset),
        Some(Ok(f)) => f.min(at),
        Some(Err(_)) => return error(StatusCode::BAD_REQUEST, "from 要是 epoch ms"),
    };
    let with_snapshot = q.get("snapshot").is_none_or(|v| v != "0");
    let n = limit(&q, REPLAY_DEFAULT_LIMIT, REPLAY_MAX_LIMIT);
    let Some(store) = state.store().cloned() else {
        return ok(&json!({ "at": at, "until": until, "from": since, "frames": [], "next": null }));
    };
    let Some(slot) = state.replay_slot() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "同時進行的回放太多，請稍後再試",
        );
    };
    let state2 = state.clone();
    // SQLite 與讀 JSONL 都會阻塞；重放也是 CPU 工作，一起放到 blocking 執行緒
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        // 鎖內只查 session、offset 與確認時間，讀 JSONL 時放掉鎖：主迴圈的寫入不用等回放（C3、X1）
        let read = {
            let s = store.lock().unwrap_or_else(|e| e.into_inner());
            s.timeline_reads(&cwd, since, at, until, n)
                .map(|(reads, acks, cut)| (reads, acks, cut, s.reader()))
        }
        .and_then(|(reads, acks, cut, reader)| Ok((reader.read(&reads)?, acks, cut)));
        match read {
            Ok((events, acks, cut)) => {
                let page = crate::timeline::Page {
                    at,
                    until,
                    cut,
                    limit: n,
                    offset_min: offset,
                    with_snapshot,
                };
                let mut body = crate::timeline::replay_page(&events, &acks, &page);
                body["cwd"] = json!(cwd);
                body["from"] = json!(since);
                // 遠端連線剝除敏感輸出（#654）與 thinking（#668），和 WS 一致
                state2.redact_for(remote, &mut body);
                ok(&body)
            }
            Err(e) => store_error(e),
        }
    })
    .await
    .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ── /api/files

enum Base {
    Main,
    Commit(String),
    Session(String),
}

fn parse_base(s: Option<&str>) -> Option<Base> {
    match s.unwrap_or("main") {
        "main" => Some(Base::Main),
        s => {
            if let Some(h) = s.strip_prefix("commit:") {
                let hex = (4..=64).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_hexdigit());
                hex.then(|| Base::Commit(h.to_ascii_lowercase()))
            } else {
                s.strip_prefix("session:")
                    .filter(|id| !id.is_empty())
                    .map(|id| Base::Session(id.to_string()))
            }
        }
    }
}

pub async fn files(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
    let q = query(&uri);
    let Some(cwd) = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "缺少 cwd");
    };
    let Some(base) = parse_base(q.get("base").map(String::as_str)) else {
        return error(
            StatusCode::BAD_REQUEST,
            "base 只能是 main、commit:<hash>（4–64 位十六進位）或 session:<id>",
        );
    };
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄");
    };
    let have = q.get("have").cloned();
    let memo_key = (
        cwd.clone(),
        q.get("base").cloned().unwrap_or_else(|| "main".into()),
    );
    // git 與 SQLite 都會阻塞，整段放到 blocking 執行緒
    spawn_git(move || files_blocking(&state, &store, &cwd, base, memo_key, have))
        .await
        .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// web 入口可以對哪些目錄跑 git（X-1，contracts §7）：store 裡某個 session 的 cwd、在它底下，或是那個 session
/// 所屬 repo 的主工作樹根（專案根目錄底下只有 worktree 裡的 session 時要能查）。任意的上層目錄（例如 home）不放行。
fn cwd_allowed(known: &[String], cwd: &str) -> bool {
    if known.iter().any(|k| {
        k == cwd
            || cwd
                .strip_prefix(k.as_str())
                .is_some_and(|r| r.starts_with('/'))
    }) {
        return true;
    }
    // 上層目錄只在是那個 session 的主工作樹根時放行：只對底下真的有 session 的候選問 git
    let prefix = format!("{cwd}/");
    known
        .iter()
        .filter(|k| k.starts_with(&prefix))
        .any(|k| worktrees(Path::new(k)).first().map(|(p, _)| p.as_str()) == Some(cwd))
}

fn files_blocking(
    state: &WebState,
    store: &std::sync::Mutex<Store>,
    cwd: &str,
    base: Base,
    memo_key: (String, String),
    have: Option<String>,
) -> Response {
    if let Some(resp) = guard(store, cwd) {
        return resp;
    }
    // 約 1 秒內的同一個 (cwd, base) 共用結果（E-H2）：多個分頁各自每 3 秒輪詢時不用每個都重跑一遍 git
    if let Some(body) = state.recent_files(&memo_key) {
        return ok(&without_known_tree(body, have));
    }
    let session_paths = {
        match &base {
            Base::Session(id) => match session_changes(store, id) {
                Ok(Some(c)) => Some(c),
                Ok(None) => return error(StatusCode::NOT_FOUND, "沒有這個 session"),
                Err(e) => return store_error(e),
            },
            _ => None,
        }
    };

    let dir = Path::new(cwd);
    let root = git(dir, &["rev-parse", "--show-toplevel"]).map(|s| norm(s.trim()));
    let root_path = root.clone().unwrap_or_else(|| cwd.to_string());
    let (tree, truncated) = match &root {
        Some(r) => {
            // -t 標出每個路徑的狀態（`? ` 是未追蹤）：樹與未追蹤檔一次拿到，算改動時不再掃一次（E-H2）
            let listing = git(
                Path::new(r),
                &[
                    "ls-files",
                    "-z",
                    "-t",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                ],
            )
            .unwrap_or_default();
            let mut t = Vec::new();
            let mut untracked = Vec::new();
            for entry in listing.split('\0').filter(|e| e.len() > 2) {
                let path = norm(&entry[2..]);
                if entry.starts_with("? ") {
                    untracked.push(path.clone());
                }
                t.push(path);
            }
            UNTRACKED.with(|u| u.borrow_mut().insert(norm(r), untracked));
            t.sort();
            t.dedup();
            let truncated = t.len() > TREE_MAX;
            t.truncate(TREE_MAX);
            (t, truncated)
        }
        None => (Vec::new(), false),
    };

    // 每種基準都帶 worktree 清單：前端靠它把 session 的 file.* 路徑對到樹上（#644）
    let wts = root
        .as_ref()
        .map(|r| worktrees(Path::new(r)))
        .unwrap_or_default();

    let (base_name, base_ref, sources): (String, Value, Vec<Source>) = match base {
        Base::Session(id) => {
            let entries = session_paths.unwrap_or_default();
            let by = json!({ "session": id });
            (
                format!("session:{id}"),
                Value::Null,
                vec![Source {
                    by,
                    entries,
                    dir: None,
                    last: None,
                }],
            )
        }
        Base::Commit(hash) => {
            let Some(r) = &root else {
                return error(StatusCode::BAD_REQUEST, "cwd 不是 git repo");
            };
            let rp = Path::new(r);
            let Some(full) = git(
                rp,
                &["rev-parse", "--verify", "-q", &format!("{hash}^{{commit}}")],
            )
            .map(|s| s.trim().to_string()) else {
                return error(StatusCode::BAD_REQUEST, format!("找不到 commit {hash}"));
            };
            let branch =
                git(rp, &["symbolic-ref", "-q", "--short", "HEAD"]).map(|s| s.trim().to_string());
            let by = json!({ "worktree": r, "branch": branch });
            let entries = git_changes(rp, &[&format!("{full}...HEAD")]);
            (
                format!("commit:{hash}"),
                json!(full),
                vec![Source {
                    by,
                    entries,
                    dir: Some(r.clone()),
                    last: None,
                }],
            )
        }
        Base::Main => match &root {
            None => ("main".into(), Value::Null, Vec::new()),
            Some(r) => match integration_sources(Path::new(r), &wts) {
                Ok((main, sources)) => ("main".into(), json!(main), sources),
                Err(msg) => return error(StatusCode::CONFLICT, msg),
            },
        },
    };

    let tree_hash = tree_hash(&tree, truncated);
    let body = json!({
        "root": root_path, "base": base_name, "baseRef": base_ref,
        "tree": tree, "treeHash": tree_hash, "treeTruncated": truncated, "changes": merge_sources(&sources),
        "worktrees": wts.iter().map(|(path, branch)| json!({ "path": path, "branch": branch })).collect::<Vec<_>>(),
    });
    state.remember_files(memo_key, body.clone());
    ok(&without_known_tree(body, have))
}

/// `tree` 的雜湊（16 碼十六進位，E-H2）：前端帶回 `have`，相同就不用再送 2 萬筆路徑。
fn tree_hash(tree: &[String], truncated: bool) -> String {
    let mut text = tree.join("\n");
    text.push_str(if truncated { "\n…" } else { "" });
    crate::keys::sha256_hex(text.as_bytes())[..16].to_string()
}

/// 前端手上已經有這份樹（`have` 等於 `treeHash`）時拿掉 `tree`，其他欄位照給。
fn without_known_tree(mut body: Value, have: Option<String>) -> Value {
    if have.is_some_and(|h| body["treeHash"].as_str() == Some(h.as_str()))
        && let Some(o) = body.as_object_mut()
    {
        o.remove("tree");
    }
    body
}

// ── /api/branches、/api/branch-diff（#696）

/// 分支視窗的 diff 上限，超過就截斷。
const DIFF_MAX: usize = 64 * 1024;

pub async fn branches(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
    let q = query(&uri);
    let Some(cwd) = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "缺少 cwd");
    };
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄");
    };
    spawn_git(move || branches_blocking(&store, &cwd))
        .await
        .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn branches_blocking(store: &std::sync::Mutex<Store>, cwd: &str) -> Response {
    // 還沒結束的 session：(id, cwd)
    if let Some(resp) = guard(store, cwd) {
        return resp;
    }
    // 只查還沒結束的 (id, cwd)，不讀出 1000 列完整資料再過濾（E-H9）
    let live: Vec<(String, String)> = {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        match store.open_session_cwds() {
            Ok(rows) => rows.into_iter().map(|(id, cwd)| (id, norm(&cwd))).collect(),
            Err(e) => return store_error(e),
        }
    };
    let Some(root) = git(Path::new(cwd), &["rev-parse", "--show-toplevel"]).map(|s| norm(s.trim()))
    else {
        return ok(&json!({ "root": cwd, "integration": null, "branches": [] }));
    };
    let wts = worktrees(Path::new(&root));
    let (main, sources) = match integration_sources(Path::new(&root), &wts) {
        Ok(v) => v,
        Err(msg) => return error(StatusCode::CONFLICT, msg),
    };

    // 和其他分支同時改到的檔：merge_sources 的 by 有兩個以上來源的
    let mut shared: HashMap<String, Vec<Value>> = HashMap::new();
    for c in merge_sources(&sources) {
        let by = c["by"].as_array().cloned().unwrap_or_default();
        if by.len() < 2 {
            continue;
        }
        let name = |b: &Value| {
            b["branch"]
                .as_str()
                .or(b["worktree"].as_str())
                .map(str::to_string)
        };
        for (i, b) in by.iter().enumerate() {
            let with: Vec<String> = by
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .filter_map(|(_, o)| name(o))
                .collect();
            if let Some(wt) = b["worktree"].as_str() {
                shared
                    .entry(wt.to_string())
                    .or_default()
                    .push(json!({ "path": c["path"], "conflict": c["conflict"], "with": with }));
            }
        }
    }

    // session 依 cwd 歸到最長前綴的 worktree（主工作樹底下的 worktrees/… 不能算主工作樹）
    let owner = |sc: &str| {
        wts.iter()
            .map(|(p, _)| p)
            .filter(|p| sc == p.as_str() || sc.starts_with(&format!("{p}/")))
            .max_by_key(|p| p.len())
            .cloned()
    };
    let mut by_wt: HashMap<String, Vec<String>> = HashMap::new();
    for (id, sc) in &live {
        if let Some(wt) = owner(sc) {
            by_wt.entry(wt).or_default().push(id.clone());
        }
    }

    let rows: Vec<Value> = wts
        .iter()
        .zip(&sources)
        .map(|((wt, branch), src)| {
            let dir = Path::new(wt);
            let counts = main
                .and_then(|m| {
                    git(
                        dir,
                        &[
                            "rev-list",
                            "--left-right",
                            "--count",
                            &format!("{m}...HEAD"),
                        ],
                    )
                })
                .and_then(|s| {
                    let mut n = s.split_whitespace().map(|v| v.parse::<u64>().ok());
                    Some((n.next()??, n.next()??))
                });
            // 未 commit 的改動：和 HEAD 比的已追蹤檔＋未追蹤檔（算改動時已經掃過，不再讓 status 重掃，E-H9）。
            // 還沒有 commit 的 repo 沒有 HEAD 可比，退回 status
            let dirty = match src.head() {
                Some(_) => git(dir, &["diff", "HEAD", "--name-only", "-z"])
                    .map(|s| nul_split(&s).len() + untracked_files(dir).len()),
                None => git(dir, &["status", "--porcelain"]).map(|s| s.lines().count()),
            };
            let last = src
                .last
                .as_ref()
                .map(|l| json!({ "hash": l.hash, "message": l.message, "ts": l.ts }));
            json!({
                "branch": branch, "worktree": wt, "head": src.head(),
                "integration": main.is_some() && branch.as_deref() == main,
                "ahead": counts.map(|c| c.1), "behind": counts.map(|c| c.0),
                "dirty": dirty, "changed": src.entries.len(), "last": last,
                "sessions": by_wt.remove(wt).unwrap_or_default(),
                "shared": shared.remove(wt).unwrap_or_default(),
            })
        })
        .collect();
    ok(&json!({ "root": root, "integration": main, "branches": rows }))
}

/// 相對 repo 根目錄的一般路徑：不能是絕對路徑、不能有 `..`、不能用 pathspec magic（`:` 開頭）。
fn plain_rel_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with(':')
        && !p.contains('\0')
        && p.split('/').all(|c| !c.is_empty() && c != "..")
}

pub async fn branch_diff(
    State(state): State<Arc<WebState>>,
    Extension(Remote(remote)): Extension<Remote>,
    uri: Uri,
) -> Response {
    let q = query(&uri);
    let field = |k: &str| q.get(k).map(|c| norm(c)).filter(|c| !c.is_empty());
    let (Some(cwd), Some(worktree), Some(path)) = (field("cwd"), field("worktree"), field("path"))
    else {
        return error(StatusCode::BAD_REQUEST, "cwd、worktree、path 都要有");
    };
    if !plain_rel_path(&path) {
        return error(StatusCode::BAD_REQUEST, "path 要是 repo 裡的相對路徑");
    }
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄");
    };
    spawn_git(move || branch_diff_blocking(&state, &store, &cwd, &worktree, &path, remote))
        .await
        .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn branch_diff_blocking(
    state: &WebState,
    store: &std::sync::Mutex<Store>,
    cwd: &str,
    worktree: &str,
    path: &str,
    remote: bool,
) -> Response {
    if let Some(resp) = guard(store, cwd) {
        return resp;
    }
    let root = git(Path::new(cwd), &["rev-parse", "--show-toplevel"]).map(|s| norm(s.trim()));
    let wts = root
        .as_ref()
        .map(|r| worktrees(Path::new(r)))
        .unwrap_or_default();
    let Some((wt, branch)) = wts.into_iter().find(|(p, _)| p == worktree) else {
        return error(StatusCode::NOT_FOUND, "不是這個 repo 的 worktree");
    };
    let dir = Path::new(&wt);
    if let Some(resp) = remote_denied(remote, dir, path) {
        return resp;
    }
    let main = integration(dir);
    let empty = main.is_none().then(|| empty_tree_if_unborn(dir)).flatten();
    let Some(base) = fork_point(dir, main, empty.as_deref()) else {
        return error(StatusCode::CONFLICT, NO_INTEGRATION);
    };
    let (diff, truncated) = file_diff(dir, &[&base], path, true);
    let mut body = json!({
        "worktree": wt, "branch": branch, "base": base, "integration": main,
        "path": path, "diff": diff, "truncated": truncated,
    });
    state.redact_for(remote, &mut body);
    ok(&body)
}

/// 一個檔的 diff（比較 `range`，只有一個時比到工作目錄）。`untracked`：未追蹤的新檔整檔當新增。
/// 超過 DIFF_MAX 截斷。`--no-ext-diff`、`--no-textconv`：不叫起 repo 設定裡的外部程式（#682）。
fn file_diff(dir: &Path, range: &[&str], path: &str, untracked: bool) -> (String, bool) {
    let literal = format!(":(literal){path}");
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv"];
    args.extend(range);
    args.extend(["--", &literal]);
    let mut diff = git(dir, &args).unwrap_or_default();
    let mut head_cut = false;
    if untracked
        && diff.is_empty()
        && git(dir, &["ls-files", "--error-unmatch", "--", &literal]).is_none()
        // 被忽略的檔（.env 之類）UI 清單本來就不列，直接打 API 也不讀（C1）
        && git(dir, &["check-ignore", "-q", "--", path]).is_none()
        && let Some((text, cut)) = read_untracked(dir, path)
    {
        head_cut = cut;
        diff = format!("--- /dev/null\n+++ b/{path}\n")
            + &text.lines().map(|l| format!("+{l}\n")).collect::<String>();
    }
    let cut = crate::text::truncate_string(&mut diff, DIFF_MAX);
    (diff, head_cut || cut)
}

/// 未追蹤檔的內容（整檔當新增用）：只讀 worktree 底下的一般檔，最多 DIFF_MAX+1 bytes（C20、A-N1）。
/// symlink（不論指到檔或目錄、或路徑中間有 symlink 目錄）、FIFO、裝置檔都不讀；回傳（內容, 是否只讀了前段）。
fn read_untracked(dir: &Path, path: &str) -> Option<(String, bool)> {
    use std::io::Read;
    let full = dir.join(path);
    if !std::fs::symlink_metadata(&full).ok()?.is_file() {
        return None;
    }
    let real = full.canonicalize().ok()?;
    if !real.starts_with(dir.canonicalize().ok()?) {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&real)
        .ok()?
        .take(DIFF_MAX as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    let cut = buf.len() > DIFF_MAX;
    Some((String::from_utf8_lossy(&buf).into_owned(), cut))
}

/// 遠端連線不能看的檔：敏感路徑（§4 的敏感規則，不論追蹤與否、哪個版本）或被 gitignore 的檔，回 403（C1、#734）。
fn remote_denied(remote: bool, dir: &Path, path: &str) -> Option<Response> {
    (remote
        && (crate::sensitive::is_sensitive_path(path)
            || git(dir, &["check-ignore", "-q", "--", path]).is_some()))
    .then(|| error(StatusCode::FORBIDDEN, "這個檔不提供給遠端連線"))
}

/// 不在白名單（[`cwd_allowed`]）的目錄回 404。只在取 cwd 清單時拿 store 鎖，跑 git 時已經放掉。
fn guard(store: &std::sync::Mutex<Store>, cwd: &str) -> Option<Response> {
    let known = match store.lock().unwrap_or_else(|e| e.into_inner()).cwds() {
        Ok(k) => k,
        Err(e) => return Some(store_error(e)),
    };
    (!cwd_allowed(&known, cwd)).then(|| error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄"))
}

fn toplevel(cwd: &str) -> Option<String> {
    git(Path::new(cwd), &["rev-parse", "--show-toplevel"]).map(|s| norm(s.trim()))
}

/// `Changes` → `[{status, path, from?}]`
fn files_json(c: &Changes) -> Vec<Value> {
    c.iter()
        .map(|(path, (status, from))| {
            let mut f = json!({ "status": status, "path": path });
            if let Some(from) = from {
                f["from"] = json!(from);
            }
            f
        })
        .collect()
}

const GRAPH_DEFAULT: usize = 300;
const GRAPH_MAX: usize = 2000;

pub async fn graph(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
    let q = query(&uri);
    let Some(cwd) = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "缺少 cwd");
    };
    let n = limit(&q, GRAPH_DEFAULT, GRAPH_MAX);
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄");
    };
    spawn_git(move || {
        if let Some(resp) = guard(&store, &cwd) {
            return resp;
        }
        let Some(root) = toplevel(&cwd) else {
            return ok(&json!({ "root": cwd, "integration": null, "commits": [], "truncated": false }));
        };
        let rp = Path::new(&root);
        // 本機分支的 commit，拓撲順序；decoration 只留本機分支
        let out = git(
            rp,
            &[
                "log",
                "--branches",
                "--topo-order",
                "--decorate-refs=refs/heads/",
                &format!("-n{}", n + 1),
                "--format=%H%x1f%P%x1f%s%x1f%an%x1f%ct%x1f%D%x1e",
            ],
        )
        .unwrap_or_default();
        let mut commits: Vec<Value> = out
            .split('\x1e')
            .map(|r| r.trim_start_matches('\n'))
            .filter(|r| !r.is_empty())
            .filter_map(|r| {
                let f: Vec<&str> = r.split('\x1f').collect();
                let [hash, parents, subject, author, ct, deco] = f[..] else {
                    return None;
                };
                let refs: Vec<&str> = deco
                    .split(", ")
                    .map(|d| d.trim_start_matches("HEAD -> "))
                    .filter(|d| !d.is_empty() && *d != "HEAD")
                    .collect();
                Some(json!({
                    "hash": hash, "parents": parents.split_whitespace().collect::<Vec<_>>(),
                    "subject": subject, "author": author,
                    "ts": ct.parse::<u64>().ok().map(|t| t * 1000), "refs": refs,
                }))
            })
            .collect();
        let truncated = commits.len() > n;
        commits.truncate(n);
        ok(&json!({ "root": root, "integration": integration(rp), "commits": commits, "truncated": truncated }))
    })
    .await
    .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// commit 或 worktree（未 commit 的改動）：/api/commit 與 /api/commit-diff 共用的目標。
enum Target {
    Commit(String),
    Worktree(String),
}

fn parse_target(q: &HashMap<String, String>) -> Result<Target, &'static str> {
    if let Some(h) = q.get("hash") {
        let hex = (4..=64).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_hexdigit());
        return hex
            .then(|| Target::Commit(h.to_ascii_lowercase()))
            .ok_or("hash 只收 4–64 位十六進位");
    }
    q.get("worktree")
        .map(|w| norm(w))
        .filter(|w| !w.is_empty())
        .map(Target::Worktree)
        .ok_or("要有 hash 或 worktree")
}

/// 目標解析成 (目錄, 比較的範圍, 這個目標的說明)：commit 對第一個 parent（root commit 對空樹），worktree 對 HEAD（還沒 commit 的 repo 對空樹）。
fn resolve(
    cwd: &str,
    t: &Target,
) -> Result<(String, Vec<String>, Value), (StatusCode, &'static str)> {
    let root = toplevel(cwd).ok_or((StatusCode::NOT_FOUND, "不是 git repo"))?;
    let rp = Path::new(&root);
    let empty = || git(rp, &["hash-object", "-t", "tree", "--stdin"]).map(|h| h.trim().to_string());
    match t {
        Target::Commit(h) => {
            let full = git(
                rp,
                &["rev-parse", "--verify", "-q", &format!("{h}^{{commit}}")],
            )
            .map(|s| s.trim().to_string())
            .ok_or((StatusCode::NOT_FOUND, "找不到這個 commit"))?;
            let parent = git(rp, &["rev-parse", "--verify", "-q", &format!("{full}^1")])
                .map(|s| s.trim().to_string())
                .or_else(empty)
                .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "git 失敗"))?;
            let info = git(
                rp,
                &["show", "-s", "--format=%P%x1f%an%x1f%ct%x1f%B", &full],
            )
            .unwrap_or_default();
            let mut f = info.splitn(4, '\x1f');
            let (parents, author, ct, msg) = (
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
            );
            let msg = msg.trim_end();
            let (subject, body) = msg.split_once('\n').unwrap_or((msg, ""));
            let v = json!({
                "hash": full, "parents": parents.split_whitespace().collect::<Vec<_>>(),
                "author": author, "ts": ct.trim().parse::<u64>().ok().map(|t| t * 1000),
                "subject": subject, "body": body.trim(),
            });
            Ok((root.clone(), vec![parent, full], v))
        }
        Target::Worktree(w) => {
            let (wt, branch) = worktrees(rp)
                .into_iter()
                .find(|(p, _)| p == w)
                .ok_or((StatusCode::NOT_FOUND, "不是這個 repo 的 worktree"))?;
            let base = if git(Path::new(&wt), &["rev-parse", "--verify", "-q", "HEAD"]).is_some() {
                "HEAD".to_string()
            } else {
                empty().ok_or((StatusCode::INTERNAL_SERVER_ERROR, "git 失敗"))?
            };
            let v = json!({ "worktree": wt, "branch": branch });
            Ok((wt, vec![base], v))
        }
    }
}

pub async fn commit(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
    let q = query(&uri);
    let Some(cwd) = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "缺少 cwd");
    };
    let target = match parse_target(&q) {
        Ok(t) => t,
        Err(msg) => return error(StatusCode::BAD_REQUEST, msg),
    };
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄");
    };
    spawn_git(move || {
        if let Some(resp) = guard(&store, &cwd) {
            return resp;
        }
        let (dir, range, mut v) = match resolve(&cwd, &target) {
            Ok(x) => x,
            Err((code, msg)) => return error(code, msg),
        };
        let dir = Path::new(&dir);
        let files = match target {
            Target::Commit(_) => {
                let mut args = vec![
                    "diff-tree",
                    "--no-commit-id",
                    "-r",
                    "-M",
                    "--name-status",
                    "-z",
                ];
                args.extend(range.iter().map(String::as_str));
                parse_name_status(&git(dir, &args).unwrap_or_default())
            }
            Target::Worktree(_) => git_changes(dir, &[&range[0]]),
        };
        v["files"] = json!(files_json(&files));
        ok(&v)
    })
    .await
    .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn commit_diff(
    State(state): State<Arc<WebState>>,
    Extension(Remote(remote)): Extension<Remote>,
    uri: Uri,
) -> Response {
    let q = query(&uri);
    let Some(cwd) = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "缺少 cwd");
    };
    let Some(path) = q.get("path").map(|p| norm(p)).filter(|p| plain_rel_path(p)) else {
        return error(StatusCode::BAD_REQUEST, "path 要是 repo 裡的相對路徑");
    };
    let target = match parse_target(&q) {
        Ok(t) => t,
        Err(msg) => return error(StatusCode::BAD_REQUEST, msg),
    };
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄");
    };
    spawn_git(move || {
        if let Some(resp) = guard(&store, &cwd) {
            return resp;
        }
        let (dir, range, _) = match resolve(&cwd, &target) {
            Ok(x) => x,
            Err((code, msg)) => return error(code, msg),
        };
        if let Some(resp) = remote_denied(remote, Path::new(&dir), &path) {
            return resp;
        }
        let range: Vec<&str> = range.iter().map(String::as_str).collect();
        let untracked = matches!(target, Target::Worktree(_));
        let (diff, truncated) = file_diff(Path::new(&dir), &range, &path, untracked);
        let mut body = json!({ "path": path, "diff": diff, "truncated": truncated });
        state.redact_for(remote, &mut body);
        ok(&body)
    })
    .await
    .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// 「相對 main」的整合分支候選，依序找第一個存在的。
const INTEGRATION: [&str; 3] = ["develop", "main", "master"];

/// 整合分支自動偵測（#693，PLAN「改動基準」）：git-flow 的 main 只收 release，有 develop 就比 develop。
fn integration(root: &Path) -> Option<&'static str> {
    INTEGRATION.into_iter().find(|b| {
        git(
            root,
            &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")],
        )
        .is_some()
    })
}

/// 還沒有任何 commit 的 repo（git init 之後）用的空樹 hash（#691）；有 commit 時 None。
fn empty_tree_if_unborn(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "--verify", "-q", "HEAD"])
        .is_none()
        .then(|| git(root, &["hash-object", "-t", "tree", "--stdin"]))
        .flatten()
        .map(|h| h.trim().to_string())
}

/// 某個 worktree 比較的起點：和整合分支的 merge-base；沒有 commit 的 repo 用空樹。
fn fork_point(wt: &Path, main: Option<&str>, empty: Option<&str>) -> Option<String> {
    match (main, empty) {
        (Some(main), _) => git(wt, &["merge-base", main, "HEAD"]).map(|b| b.trim().to_string()),
        (None, Some(tree)) => Some(tree.to_string()),
        (None, None) => None,
    }
}

const NO_INTEGRATION: &str = "repo 沒有 develop、main 或 master 分支，無法比「相對 main」";

/// 「相對 main」：找整合分支，逐 worktree 對它的 merge-base 算改動（比到工作目錄，PLAN「改動基準」，#682）。
/// 有 commit 卻找不到整合分支時回錯誤訊息。
fn integration_sources(
    root: &Path,
    wts: &[(String, Option<String>)],
) -> Result<(Option<&'static str>, Vec<Source>), &'static str> {
    let main = integration(root);
    let empty = match main {
        Some(_) => None,
        None => Some(empty_tree_if_unborn(root).ok_or(NO_INTEGRATION)?),
    };
    let sources = wts
        .iter()
        .cloned()
        .map(|(wt, branch)| {
            let dir = Path::new(&wt);
            Source {
                entries: fork_point(dir, main, empty.as_deref())
                    .map(|base| git_changes(dir, &[&base]))
                    .unwrap_or_default(),
                by: json!({ "worktree": wt, "branch": branch }),
                last: last_commit(dir),
                dir: Some(wt),
            }
        })
        .collect();
    Ok((main, sources))
}

/// path → (status, 原路徑)；原路徑只有 R／C 有。
type Changes = BTreeMap<String, (String, Option<String>)>;

/// 一個改動來源（worktree 或 session）與它的 path → (status, from)。
struct Source {
    by: Value,
    entries: Changes,
    /// git 來源的 worktree 目錄（判斷衝突時讀內容）
    dir: Option<String>,
    /// 該 worktree 的 HEAD 那個 commit（判斷衝突時找共同祖先；分支視窗的「最後一個 commit」）。
    /// 一次 `log -1` 同時拿 hash、訊息與時間，不再另跑 `rev-parse HEAD`（E-H9）
    last: Option<LastCommit>,
}

struct LastCommit {
    hash: String,
    message: String,
    ts: Option<u64>,
}

impl Source {
    fn head(&self) -> Option<&str> {
        self.last.as_ref().map(|l| l.hash.as_str())
    }
}

fn last_commit(dir: &Path) -> Option<LastCommit> {
    let s = git(dir, &["log", "-1", "--format=%H%x00%s%x00%ct"])?;
    let mut f = s.trim_end().splitn(3, '\0');
    let (hash, message, ct) = (f.next()?, f.next()?, f.next()?);
    Some(LastCommit {
        hash: hash.to_string(),
        message: message.to_string(),
        ts: ct.parse::<u64>().ok().map(|t| t * 1000),
    })
}

fn merge_sources(sources: &[Source]) -> Vec<Value> {
    let mut paths: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in sources.iter().enumerate() {
        for p in s.entries.keys() {
            paths.entry(p).or_default().push(i);
        }
    }
    // 出現在兩個以上來源的檔：比內容（HEAD 的 blob、未追蹤檔的 hash、刪除）
    let mut ids: HashMap<(usize, &str), String> = HashMap::new();
    for (i, s) in sources.iter().enumerate() {
        let Some(dir) = &s.dir else { continue };
        let shared: Vec<&str> = s
            .entries
            .keys()
            .map(String::as_str)
            .filter(|p| paths.get(p).is_some_and(|v| v.len() > 1))
            .collect();
        for (p, id) in content_ids(Path::new(dir), &s.entries, &shared) {
            ids.insert((i, p), id);
        }
    }
    let mut touched = Touched::default();
    paths
        .into_iter()
        .map(|(path, idx)| {
            // 任兩個來源從共同祖先之後都改過、而且內容不同才算：只是帶著同一段歷史，
            // 或一邊分支開得早所以內容較舊，都不是衝突
            let conflict = idx.iter().enumerate().any(|(n, &i)| {
                idx[n + 1..].iter().any(|&j| {
                    let differ = match (ids.get(&(i, path)), ids.get(&(j, path))) {
                        (Some(a), Some(b)) => a != b,
                        _ => true,
                    };
                    differ
                        && touched.since_fork(sources, i, j, path)
                        && touched.since_fork(sources, j, i, path)
                })
            });
            let by: Vec<Value> = idx
                .iter()
                .map(|i| {
                    let mut b = sources[*i].by.clone();
                    let (status, from) = &sources[*i].entries[path];
                    b["status"] = json!(status);
                    if let Some(f) = from {
                        b["from"] = json!(f);
                    }
                    b
                })
                .collect();
            json!({ "path": path, "conflict": conflict, "by": by })
        })
        .collect()
}

/// 兩個 worktree 的共同祖先，以及從祖先到某個 HEAD 改過的檔（快取，同一次請求內重用）。
#[derive(Default)]
struct Touched {
    forks: HashMap<(String, String), Option<String>>,
    names: HashMap<(String, String), std::collections::HashSet<String>>,
}

impl Touched {
    /// 來源 `i` 從它與 `j` 的共同祖先之後（到工作目錄）是否改過 `path`；未追蹤檔一定算，資訊不足時保守算有。
    fn since_fork(&mut self, sources: &[Source], i: usize, j: usize, path: &str) -> bool {
        let (a, b) = (&sources[i], &sources[j]);
        if a.entries.get(path).is_some_and(|(st, _)| st == "?") {
            return true;
        }
        let (Some(dir), Some(ha), Some(hb)) = (&a.dir, a.head(), b.head()) else {
            return true;
        };
        let key = if ha <= hb {
            (ha.to_string(), hb.to_string())
        } else {
            (hb.to_string(), ha.to_string())
        };
        let fork = self
            .forks
            .entry(key)
            .or_insert_with(|| {
                git(Path::new(dir), &["merge-base", ha, hb]).map(|s| s.trim().to_string())
            })
            .clone();
        let Some(fork) = fork else { return true };
        // 共同祖先到工作目錄（含還沒 commit 的改動，#682）
        self.names
            .entry((fork.clone(), dir.clone()))
            .or_insert_with(|| {
                let out =
                    git(Path::new(dir), &["diff", "--name-only", "-z", &fork]).unwrap_or_default();
                nul_split(&out).into_iter().collect()
            })
            .contains(path)
    }
}

/// 這些檔在 worktree 裡的內容識別：工作目錄的內容（`git hash-object --no-filters`，不套 repo 設定的 filter），
/// 刪除的是 `-`。基準比到工作目錄（#682），所以比的也是工作目錄的內容，不是 HEAD 的 blob。
fn content_ids<'a>(dir: &Path, entries: &Changes, paths: &[&'a str]) -> Vec<(&'a str, String)> {
    let (deleted, present): (Vec<&str>, Vec<&str>) =
        paths.iter().partition(|p| entries[**p].0 == "D");
    let mut out: Vec<(&str, String)> = deleted.iter().map(|p| (*p, "-".to_string())).collect();
    if !present.is_empty() {
        let mut args = vec!["hash-object", "--no-filters", "--"];
        args.extend(&present);
        let hashes = git(dir, &args).unwrap_or_default();
        for (p, h) in present.into_iter().zip(hashes.lines()) {
            out.push((p, h.to_string()));
        }
    }
    out
}

/// `git diff --name-status <range>` 加上未追蹤檔。`range` 只有一個 commit 時比到工作目錄。
fn git_changes(dir: &Path, range: &[&str]) -> Changes {
    let mut args = vec!["diff", "--name-status", "-z"];
    args.extend(range);
    let mut out = parse_name_status(&git(dir, &args).unwrap_or_default());
    let others = untracked_files(dir);
    for p in others {
        out.entry(p).or_insert_with(|| ("?".to_string(), None));
    }
    out
}

/// 這個目錄的未追蹤檔；同一個請求裡掃過就用快取，沒掃過就掃並記下（E-H2、E-H9：樹、改動、分支的 dirty 共用一次）。
fn untracked_files(dir: &Path) -> Vec<String> {
    let key = norm(&dir.to_string_lossy());
    if let Some(cached) = UNTRACKED.with(|u| u.borrow().get(&key).cloned()) {
        return cached;
    }
    let found = nul_split(
        &git(dir, &["ls-files", "-z", "--others", "--exclude-standard"]).unwrap_or_default(),
    );
    UNTRACKED.with(|u| u.borrow_mut().insert(key, found.clone()));
    found
}

/// `--name-status -z` 的輸出 → path → (status, 原路徑)。
fn parse_name_status(diff: &str) -> Changes {
    let mut out = BTreeMap::new();
    let mut f = diff.split('\0').filter(|s| !s.is_empty());
    while let Some(st) = f.next() {
        let letter = st.chars().next().unwrap_or('M').to_string();
        if letter == "R" || letter == "C" {
            let (Some(from), Some(to)) = (f.next(), f.next()) else {
                break;
            };
            out.insert(norm(to), (letter, Some(norm(from))));
        } else if let Some(p) = f.next() {
            out.insert(norm(p), (letter, None));
        }
    }
    out
}

/// `git worktree list` 裡還存在、不是 bare 的 worktree：(路徑, 分支)。
fn worktrees(root: &Path) -> Vec<(String, Option<String>)> {
    let listing = git(root, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    listing
        .split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            let mut branch = None;
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path = Some(norm(p));
                } else if let Some(b) = line.strip_prefix("branch ") {
                    branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
                } else if line == "bare" || line.starts_with("prunable") {
                    return None;
                }
            }
            let path = path?;
            Path::new(&path).is_dir().then_some((path, branch))
        })
        .collect()
}

/// `session` 基準：那個 session 的 file.edit／file.write；None 表示沒有這個 session。
fn session_changes(
    store: &std::sync::Mutex<Store>,
    id: &str,
) -> Result<Option<Changes>, crate::store::StoreError> {
    // 改了哪些檔查 changes 表（被拒或失敗的已經刪掉，#682）；只有「是不是新檔」要看 file.write 的 Event，
    // 所以只讀 file.write、不讀整個 session 的 Edit 與 tool.updated（E-H8）。鎖內只查，讀 JSONL 時放掉鎖（L-7）
    let (rows, reads, reader) = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        if s.session(id)?.is_none() {
            return Ok(None);
        }
        let rows = s.session_change_rows(id)?;
        let reads = rows
            .iter()
            .any(|(_, kind, _)| kind == "write")
            .then(|| s.events_of_types_reads(id, &["file.write"]))
            .transpose()?;
        (rows, reads, s.reader())
    };
    let kept: std::collections::HashSet<u64> = rows.iter().map(|(_, _, seq)| *seq).collect();
    let mut new_files = std::collections::HashSet::new();
    if let Some(reads) = reads {
        for e in reader.read(&reads)? {
            if kept.contains(&e.seq)
                && let Ok(Some(Payload::FileWrite(p))) = e.payload_typed()
                && p.is_new
            {
                new_files.insert(norm(&p.path));
            }
        }
    }
    let mut out: Changes = BTreeMap::new();
    for (path, _, _) in rows {
        let path = norm(&path);
        let status = if new_files.contains(&path) { "A" } else { "M" };
        let entry = out.entry(path).or_insert_with(|| ("M".into(), None));
        if status == "A" {
            entry.0 = "A".into();
        }
    }
    Ok(Some(out))
}

/// 跑 git 的請求放到 blocking 執行緒，並在這個請求期間開一個 [`GitCtx`]（filter driver 的查詢快取）。
fn spawn_git<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _ctx = GitCtx::begin();
        f()
    })
}

thread_local! {
    /// 這個 blocking 執行緒上正在處理的請求的 filter driver 快取（目錄 → driver 名稱）；請求之外是 None。
    static FILTERS: std::cell::RefCell<Option<HashMap<std::path::PathBuf, Vec<String>>>> =
        const { std::cell::RefCell::new(None) };
    /// 同一個請求裡已經掃過的未追蹤檔（目錄 → 路徑）：樹的 ls-files 一併分出來，算改動時不再掃一次（E-H2）
    static UNTRACKED: std::cell::RefCell<HashMap<String, Vec<String>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// 一個請求內的 git 快取（E-H1、W-L7）：每個 git 指令前原本都多跑一次 `git config` 查 filter driver，行程數加倍；
/// 同一個請求裡設定不會變，每個目錄查一次就好。請求結束（drop）就清掉，下一個請求重查，改了設定立刻生效。
struct GitCtx;

impl GitCtx {
    fn begin() -> GitCtx {
        FILTERS.with(|f| *f.borrow_mut() = Some(HashMap::new()));
        GitCtx
    }
}

impl Drop for GitCtx {
    fn drop(&mut self) {
        FILTERS.with(|f| *f.borrow_mut() = None);
        UNTRACKED.with(|u| u.borrow_mut().clear());
    }
}

/// [`filter_drivers`]，請求內有快取就用快取。
fn cached_filter_drivers(dir: &Path) -> Vec<String> {
    FILTERS.with(|f| match f.borrow_mut().as_mut() {
        Some(cache) => cache
            .entry(dir.to_path_buf())
            .or_insert_with(|| filter_drivers(dir))
            .clone(),
        None => filter_drivers(dir),
    })
}

/// `core.hooksPath` 指到不可能有檔案的位置：repo 的 `.git/hooks` 一律不跑（S-2）。
const NO_HOOKS: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

fn base_git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(k);
        }
    }
    // collector 在背景跑時，Windows 不加這個旗標會替每次 git 開一個 CMD 視窗
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    // 不執行 repo 設定或 .git/hooks 裡的外部程式（S-2，contracts §7）：
    // --no-optional-locks：status 不順手寫回 index（寫 index 會觸發 post-index-change）；
    // hooksPath 指到空處；不進 submodule（submodule 自己的 config 有 filter，頂層的停用管不到）；不驗簽章（gpg.program）。
    c.arg("--no-optional-locks").arg("-C").arg(dir).args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.quotePath=false",
        "-c",
        &format!("core.hooksPath={NO_HOOKS}"),
        "-c",
        "diff.ignoreSubmodules=all",
        "-c",
        "submodule.recurse=false",
        "-c",
        "log.showSignature=false",
        "-c",
        "gpg.program=",
    ]);
    c
}

/// repo 設定（含全域）裡定義的 filter driver 名稱；讀設定本身不會執行任何程式。
fn filter_drivers(dir: &Path) -> Vec<String> {
    let Ok(out) = base_git(dir)
        .args([
            "config",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process)$",
        ])
        .output()
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|k| {
            let rest = k.strip_prefix("filter.")?;
            Some(rest[..rest.rfind('.')?].to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn nul_split(s: &str) -> Vec<String> {
    s.split('\0').filter(|p| !p.is_empty()).map(norm).collect()
}

/// 在 `dir` 跑 git；失敗回 None。清掉繼承的 GIT_*（collector 可能是從 git hook 裡被叫起來的）。
/// repo 設定的 filter 一律停用：和工作目錄比較時 git 會對檔案跑 clean filter，那是 repo 設定裡的外部程式（#682）。
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut c = base_git(dir);
    for driver in cached_filter_drivers(dir) {
        for key in ["clean", "smudge", "process"] {
            c.arg("-c").arg(format!("filter.{driver}.{key}="));
        }
        c.arg("-c").arg(format!("filter.{driver}.required=false"));
    }
    let out = c.args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `GET /api/sessions/<id>/thinking/<seq>`（#668）：一段 thinking 的全文。本機連線永遠給；遠端依
/// `remote_thinking`：`on_demand`／`show` 給（寫 log：哪把金鑰讀了哪一段）、`hide` 403。不是 thinking 的 seq → 404。
pub async fn thinking(
    State(state): State<Arc<WebState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Extension(Remote(remote)): Extension<Remote>,
    identity: Option<axum::Extension<crate::auth::Identity>>,
    axum::extract::Path((session, seq)): axum::extract::Path<(String, u64)>,
) -> Response {
    if remote && state.remote_thinking() == crate::config::RemoteThinking::Hide {
        return error(
            StatusCode::FORBIDDEN,
            "這個 collector 不對遠端提供 thinking（remote_thinking = hide）",
        );
    }
    let Some(store) = state.store().cloned() else {
        return error(StatusCode::NOT_FOUND, "沒有這段 thinking");
    };
    // 不在 async worker 上拿 store 鎖、讀 JSONL 時不握鎖（L-7）
    let sid = session.clone();
    let found = tokio::task::spawn_blocking(move || {
        let (reads, reader) = {
            let s = store.lock().unwrap_or_else(|e| e.into_inner());
            (s.events_by_seq_reads(&sid, seq, seq)?, s.reader())
        };
        reader.read(&reads)
    })
    .await
    .unwrap_or_else(|e| Err(crate::store::StoreError::Io(std::io::Error::other(e))));
    let events = match found {
        Ok(v) => v,
        Err(e) => return store_error(e),
    };
    let Some(ev) = events.into_iter().next() else {
        return error(StatusCode::NOT_FOUND, "沒有這段 thinking");
    };
    if ev.kind != "assistant.thinking" {
        return error(StatusCode::NOT_FOUND, "這個 seq 不是 thinking");
    }
    let t = &ev.payload;
    if remote {
        let who = match identity.as_deref() {
            Some(crate::auth::Identity::Key { id, .. }) => format!("金鑰 {id}"),
            _ => format!("{peer}"),
        };
        crate::logging::log(&format!("web：{who} 讀取 thinking {session}/{seq}"));
    }
    let mut body = json!({
        "session": session, "seq": seq, "agent": ev.agent, "text": t["text"], "summarized": t["summarized"],
        "messageId": t["messageId"], "model": t["model"],
    });
    if remote {
        // 全文裡的 clk_ 金鑰也要遮（#720）；這個回應不是 Event，以 Show 模式剝除就只會遮金鑰、不會 defer 全文
        body = crate::ws::redact_remote_copy(&body, crate::config::RemoteThinking::Show);
    }
    ok(&body)
}
