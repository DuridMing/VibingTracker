//! web 入口的 HTTP API（contracts.md §7、#671）：Sessions tab、回放／載入更多、檔案樹與改動基準。
//!
//! 路由與 Host／Origin 檢查在 [`crate::ws::serve_web`]。git 只對 store 裡有 session 的目錄跑，
//! 而且一律關掉 fsmonitor 與 repo 設定的 filter：repo 設定裡的外部程式不能經由瀏覽器開的網頁被叫起來（#682）。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path as UrlPath, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::event::Payload;
use crate::live::{clip, short_model};
use crate::store::ProjectKey;
use crate::store::{SessionRow, Store};
use crate::ws::{MORE_LIMIT, WebState, more_events};

const SESSIONS_DEFAULT: usize = 100;
const SESSIONS_MAX: usize = 1000;
const EVENTS_DEFAULT: usize = 200;
const TREE_MAX: usize = 20_000;

fn reply(code: StatusCode, body: &Value) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
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

/// 查詢字串（`application/x-www-form-urlencoded`）；同名參數取最後一個。
fn query(uri: &Uri) -> HashMap<String, String> {
    uri.query()
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

pub async fn sessions(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
    let q = query(&uri);
    let cwd = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty());
    let Some(store) = state.store().cloned() else {
        return ok(&json!({ "sessions": [] }));
    };
    let n = limit(&q, SESSIONS_DEFAULT, SESSIONS_MAX);
    // SQLite 與 JSONL 都會阻塞：放到 blocking 執行緒，不佔住 async worker（#683）。先讀完 store 再碰 live，不同時拿兩把鎖
    let stored = tokio::task::spawn_blocking(move || {
        stored_rows(
            &store.lock().unwrap_or_else(|e| e.into_inner()),
            cwd.as_deref(),
            n,
        )
    })
    .await;
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
    ok(&json!({ "sessions": rows }))
}

/// store 的 sessions 列，組成與 WS 相同欄位（不在 live 狀態裡的 session 直接用）。
/// 最後一個 prompt 與 `lastPos` 用兩句批次查詢取，不是每列各查一次。
fn stored_rows(
    store: &Store,
    cwd: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, Value)>, crate::store::StoreError> {
    let rows = store.sessions(cwd, limit)?;
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let extra = store.last_prompt_and_pos(&ids)?;
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
        "status": r.status, "statusSince": r.status_since, "doing": Value::Null, "score": 0,
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
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    UrlPath(id): UrlPath<String>,
    uri: Uri,
) -> Response {
    let q = query(&uri);
    let before = q.get("beforePos").and_then(|v| v.parse::<u64>().ok());
    let Some(store) = state.store() else {
        return error(StatusCode::NOT_FOUND, "沒有這個 session");
    };
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    match store.session(&id) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::NOT_FOUND, "沒有這個 session"),
        Err(e) => return store_error(e),
    }
    match more_events(
        &store,
        &id,
        before,
        limit(&q, EVENTS_DEFAULT, MORE_LIMIT as usize),
    ) {
        Ok(events) => {
            let mut body = json!({ "session": id, "beforePos": before, "events": events });
            // 遠端連線剝除敏感輸出（#654）與 thinking（#668），和 WS 一致
            state.redact_for_peer(peer, &mut body);
            ok(&body)
        }
        Err(e) => store_error(e),
    }
}

// ── /api/projects/events（全專案回放，#702）

/// `before=<ts>,<session>,<pos>`：上一頁回應的 `next`。
fn parse_project_key(s: &str) -> Option<ProjectKey> {
    let mut it = s.splitn(3, ',');
    let ts = it.next()?.parse().ok()?;
    let session = it.next()?.to_string();
    let pos = it.next()?.parse().ok()?;
    (!session.is_empty()).then_some(ProjectKey { ts, session, pos })
}

pub async fn project_events(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    uri: Uri,
) -> Response {
    let q = query(&uri);
    let Some(cwd) = q.get("cwd").map(|c| norm(c)).filter(|c| !c.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "缺少 cwd（專案根目錄）");
    };
    let num = |k: &str| q.get(k).map(|v| v.parse::<u64>());
    let (from, to) = match (num("from"), num("to")) {
        (Some(Err(_)), _) | (_, Some(Err(_))) => {
            return error(StatusCode::BAD_REQUEST, "from、to 要是 epoch ms");
        }
        (f, t) => (
            f.and_then(Result::ok).unwrap_or(0),
            t.and_then(Result::ok).unwrap_or(u64::MAX),
        ),
    };
    let before = match q.get("before").map(|b| parse_project_key(b)) {
        Some(None) => {
            return error(StatusCode::BAD_REQUEST, "before 要是 <ts>,<session>,<pos>");
        }
        b => b.flatten(),
    };
    let n = limit(&q, EVENTS_DEFAULT, MORE_LIMIT as usize);
    let Some(store) = state.store().cloned() else {
        return ok(&json!({ "cwd": cwd, "events": [], "next": null }));
    };
    let state2 = state.clone();
    // SQLite 與讀 JSONL 都會阻塞，放到 blocking 執行緒
    tokio::task::spawn_blocking(move || {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        match crate::ws::project_events(&store, &cwd, from, to, before.as_ref(), n) {
            Ok((events, next)) => {
                let next = next.map(|k| json!({ "ts": k.ts, "session": k.session, "pos": k.pos }));
                let mut body =
                    json!({ "cwd": cwd, "from": from, "to": to, "events": events, "next": next });
                // 遠端連線剝除敏感輸出（#654）與 thinking（#668），和 WS 一致
                state2.redact_for_peer(peer, &mut body);
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
    // git 與 SQLite 都會阻塞，整段放到 blocking 執行緒
    tokio::task::spawn_blocking(move || files_blocking(&store, &cwd, base))
        .await
        .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// 只對 store 裡有 session 的目錄（或它的上層）跑 git：不是的話回拒絕的回應。
fn reject_unknown(store: &Store, cwd: &str) -> Option<Response> {
    let known = match store.cwds() {
        Ok(k) => k,
        Err(e) => return Some(store_error(e)),
    };
    let prefix = format!("{cwd}/");
    (!known.iter().any(|k| k == cwd || k.starts_with(&prefix)))
        .then(|| error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄"))
}

fn files_blocking(store: &std::sync::Mutex<Store>, cwd: &str, base: Base) -> Response {
    let session_paths = {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(resp) = reject_unknown(&store, cwd) {
            return resp;
        }
        match &base {
            Base::Session(id) => match session_changes(&store, id) {
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
            let mut t = nul_split(
                &git(
                    Path::new(r),
                    &[
                        "ls-files",
                        "-z",
                        "--cached",
                        "--others",
                        "--exclude-standard",
                    ],
                )
                .unwrap_or_default(),
            );
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
                    head: None,
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
                    head: None,
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

    ok(&json!({
        "root": root_path, "base": base_name, "baseRef": base_ref,
        "tree": tree, "treeTruncated": truncated, "changes": merge_sources(&sources),
        "worktrees": wts.iter().map(|(path, branch)| json!({ "path": path, "branch": branch })).collect::<Vec<_>>(),
    }))
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
    tokio::task::spawn_blocking(move || branches_blocking(&store, &cwd))
        .await
        .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn branches_blocking(store: &std::sync::Mutex<Store>, cwd: &str) -> Response {
    // 還沒結束的 session：(id, cwd)
    let live: Vec<(String, String)> = {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(resp) = reject_unknown(&store, cwd) {
            return resp;
        }
        match store.sessions(None, SESSIONS_MAX) {
            Ok(rows) => rows
                .into_iter()
                .filter(|r| r.ended_at.is_none())
                .map(|r| (r.id, norm(&r.cwd)))
                .collect(),
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
                .and_then(|m| git(dir, &["rev-list", "--left-right", "--count", &format!("{m}...HEAD")]))
                .and_then(|s| {
                    let mut n = s.split_whitespace().map(|v| v.parse::<u64>().ok());
                    Some((n.next()??, n.next()??))
                });
            let dirty = git(dir, &["status", "--porcelain"]).map(|s| s.lines().count());
            let last = git(dir, &["log", "-1", "--format=%H%x00%s%x00%ct"]).and_then(|s| {
                let mut f = s.trim_end().splitn(3, '\0');
                let (hash, msg, ct) = (f.next()?, f.next()?, f.next()?);
                Some(json!({ "hash": hash, "message": msg, "ts": ct.parse::<u64>().ok().map(|t| t * 1000) }))
            });
            json!({
                "branch": branch, "worktree": wt, "head": src.head,
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

pub async fn branch_diff(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
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
    tokio::task::spawn_blocking(move || branch_diff_blocking(&store, &cwd, &worktree, &path))
        .await
        .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn branch_diff_blocking(
    store: &std::sync::Mutex<Store>,
    cwd: &str,
    worktree: &str,
    path: &str,
) -> Response {
    {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(resp) = reject_unknown(&store, cwd) {
            return resp;
        }
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
    let main = integration(dir);
    let empty = main.is_none().then(|| empty_tree_if_unborn(dir)).flatten();
    let Some(base) = fork_point(dir, main, empty.as_deref()) else {
        return error(StatusCode::CONFLICT, NO_INTEGRATION);
    };
    let (diff, truncated) = file_diff(dir, &[&base], path, true);
    ok(&json!({
        "worktree": wt, "branch": branch, "base": base, "integration": main,
        "path": path, "diff": diff, "truncated": truncated,
    }))
}

/// 一個檔的 diff（比較 `range`，只有一個時比到工作目錄）。`untracked`：未追蹤的新檔整檔當新增。
/// 超過 DIFF_MAX 截斷。`--no-ext-diff`、`--no-textconv`：不叫起 repo 設定裡的外部程式（#682）。
fn file_diff(dir: &Path, range: &[&str], path: &str, untracked: bool) -> (String, bool) {
    let literal = format!(":(literal){path}");
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv"];
    args.extend(range);
    args.extend(["--", &literal]);
    let mut diff = git(dir, &args).unwrap_or_default();
    if untracked
        && diff.is_empty()
        && git(dir, &["ls-files", "--error-unmatch", "--", &literal]).is_none()
        && let Ok(text) = std::fs::read_to_string(dir.join(path))
    {
        diff = format!("--- /dev/null\n+++ b/{path}\n")
            + &text.lines().map(|l| format!("+{l}\n")).collect::<String>();
    }
    let truncated = diff.len() > DIFF_MAX;
    if truncated {
        let mut cut = DIFF_MAX;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
    }
    (diff, truncated)
}

/// store 裡有 session 的 cwd 才放行（拿鎖、查完就放）。
fn guard(store: &std::sync::Mutex<Store>, cwd: &str) -> Option<Response> {
    reject_unknown(&store.lock().unwrap_or_else(|e| e.into_inner()), cwd)
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
    tokio::task::spawn_blocking(move || {
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
    tokio::task::spawn_blocking(move || {
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

pub async fn commit_diff(State(state): State<Arc<WebState>>, uri: Uri) -> Response {
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
    tokio::task::spawn_blocking(move || {
        if let Some(resp) = guard(&store, &cwd) {
            return resp;
        }
        let (dir, range, _) = match resolve(&cwd, &target) {
            Ok(x) => x,
            Err((code, msg)) => return error(code, msg),
        };
        let range: Vec<&str> = range.iter().map(String::as_str).collect();
        let untracked = matches!(target, Target::Worktree(_));
        let (diff, truncated) = file_diff(Path::new(&dir), &range, &path, untracked);
        ok(&json!({ "path": path, "diff": diff, "truncated": truncated }))
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
                head: git(dir, &["rev-parse", "HEAD"]).map(|h| h.trim().to_string()),
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
    /// 該 worktree 的 HEAD（判斷衝突時找共同祖先）
    head: Option<String>,
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
        let (Some(dir), Some(ha), Some(hb)) = (&a.dir, &a.head, &b.head) else {
            return true;
        };
        let key = if ha <= hb {
            (ha.clone(), hb.clone())
        } else {
            (hb.clone(), ha.clone())
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
    let others =
        git(dir, &["ls-files", "-z", "--others", "--exclude-standard"]).unwrap_or_default();
    for p in nul_split(&others) {
        out.entry(p).or_insert_with(|| ("?".to_string(), None));
    }
    out
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
fn session_changes(store: &Store, id: &str) -> Result<Option<Changes>, crate::store::StoreError> {
    if store.session(id)?.is_none() {
        return Ok(None);
    }
    let mut out: Changes = BTreeMap::new();
    let events = store.events_of_types(id, &["file.edit", "file.write", "tool.updated"])?;
    // 被拒或失敗的 Edit／Write 不算改動（contracts §4，#682）
    let failed: std::collections::HashSet<&str> = events
        .iter()
        .filter(|e| e.kind == "tool.updated" && e.payload["status"] == "failed")
        .filter_map(|e| e.payload["toolUseId"].as_str())
        .collect();
    for e in &events {
        if e.payload["toolUseId"]
            .as_str()
            .is_some_and(|t| failed.contains(t))
        {
            continue;
        }
        let (path, new) = match e.payload_typed() {
            Ok(Some(Payload::FileEdit(p))) => (p.path, false),
            Ok(Some(Payload::FileWrite(p))) => (p.path, p.is_new),
            _ => continue,
        };
        let entry = out.entry(norm(&path)).or_insert_with(|| ("M".into(), None));
        if new {
            entry.0 = "A".into();
        }
    }
    Ok(Some(out))
}

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
    c.arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotePath=false"]);
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
    for driver in filter_drivers(dir) {
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
    identity: Option<axum::Extension<crate::auth::Identity>>,
    axum::extract::Path((session, seq)): axum::extract::Path<(String, u64)>,
) -> Response {
    let remote = state.remote_peer(peer);
    if remote && state.remote_thinking() == crate::config::RemoteThinking::Hide {
        return error(
            StatusCode::FORBIDDEN,
            "這個 collector 不對遠端提供 thinking（remote_thinking = hide）",
        );
    }
    let Some(store) = state.store() else {
        return error(StatusCode::NOT_FOUND, "沒有這段 thinking");
    };
    let found = {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        store.events_by_seq(&session, seq, seq)
    };
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
    ok(&json!({
        "session": session, "seq": seq, "agent": ev.agent, "text": t["text"], "summarized": t["summarized"],
        "messageId": t["messageId"], "model": t["model"],
    }))
}
