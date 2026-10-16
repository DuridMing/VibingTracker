//! web 入口的 HTTP API（contracts.md §7、#671）：Sessions tab、回放／載入更多、檔案樹與改動基準。
//!
//! 路由與 Host／Origin 檢查在 [`crate::ws::serve_web`]。git 只對 store 裡有 session 的目錄跑，
//! 而且一律關掉 fsmonitor：repo 設定裡的外部程式不能經由瀏覽器開的網頁被叫起來。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::event::Payload;
use crate::live::{clip, short_model};
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
    eprintln!("coding-lab web：API 讀取 store 失敗：{e}");
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
    })
}

// ── /api/sessions/<id>/events

pub async fn events(
    State(state): State<Arc<WebState>>,
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
        Ok(events) => ok(&json!({ "session": id, "beforePos": before, "events": events })),
        Err(e) => store_error(e),
    }
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

fn files_blocking(store: &std::sync::Mutex<Store>, cwd: &str, base: Base) -> Response {
    let session_paths = {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        let known = match store.cwds() {
            Ok(c) => c,
            Err(e) => return store_error(e),
        };
        let prefix = format!("{cwd}/");
        if !known.iter().any(|k| k == cwd || k.starts_with(&prefix)) {
            return error(StatusCode::NOT_FOUND, "沒有 session 在這個目錄");
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
            let entries = git_changes(rp, &format!("{full}...HEAD"));
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
            Some(r) => {
                let rp = Path::new(r);
                let Some(main) = ["main", "master"].into_iter().find(|b| {
                    git(
                        rp,
                        &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")],
                    )
                    .is_some()
                }) else {
                    return error(StatusCode::CONFLICT, "repo 沒有 main 也沒有 master 分支");
                };
                let sources = wts
                    .iter()
                    .cloned()
                    .map(|(wt, branch)| Source {
                        entries: git_changes(Path::new(&wt), &format!("{main}...HEAD")),
                        by: json!({ "worktree": wt, "branch": branch }),
                        head: git(Path::new(&wt), &["rev-parse", "HEAD"])
                            .map(|h| h.trim().to_string()),
                        dir: Some(wt),
                    })
                    .collect();
                ("main".into(), json!(main), sources)
            }
        },
    };

    ok(&json!({
        "root": root_path, "base": base_name, "baseRef": base_ref,
        "tree": tree, "treeTruncated": truncated, "changes": merge_sources(&sources),
        "worktrees": wts.iter().map(|(path, branch)| json!({ "path": path, "branch": branch })).collect::<Vec<_>>(),
    }))
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
    /// 來源 `i` 從它與 `j` 的共同祖先之後是否改過 `path`；未追蹤檔一定算，資訊不足時保守算有。
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
        self.names
            .entry((fork.clone(), ha.clone()))
            .or_insert_with(|| {
                let out = git(Path::new(dir), &["diff", "--name-only", "-z", &fork, ha])
                    .unwrap_or_default();
                nul_split(&out).into_iter().collect()
            })
            .contains(path)
    }
}

/// 這些檔在 worktree 裡的內容識別：已追蹤的用 HEAD 的 blob，未追蹤的用 `git hash-object`，刪除的是 `-`。
fn content_ids<'a>(dir: &Path, entries: &Changes, paths: &[&'a str]) -> Vec<(&'a str, String)> {
    let mut out = Vec::new();
    let (untracked, tracked): (Vec<&str>, Vec<&str>) =
        paths.iter().partition(|p| entries[**p].0 == "?");
    let deleted: Vec<&str> = tracked
        .iter()
        .copied()
        .filter(|p| entries[*p].0 == "D")
        .collect();
    out.extend(deleted.iter().map(|p| (*p, "-".to_string())));
    let tracked: Vec<&str> = tracked
        .into_iter()
        .filter(|p| entries[*p].0 != "D")
        .collect();
    if !tracked.is_empty() {
        let mut args = vec!["ls-tree", "-z", "HEAD", "--"];
        args.extend(&tracked);
        let listing = git(dir, &args).unwrap_or_default();
        let blobs: HashMap<&str, &str> = listing
            .split('\0')
            .filter_map(|l| {
                let (meta, path) = l.split_once('\t')?;
                Some((path, meta.split(' ').nth(2)?))
            })
            .collect();
        for p in tracked {
            if let Some(b) = blobs.get(p) {
                out.push((p, b.to_string()));
            }
        }
    }
    if !untracked.is_empty() {
        let mut args = vec!["hash-object", "--"];
        args.extend(&untracked);
        let hashes = git(dir, &args).unwrap_or_default();
        for (p, h) in untracked.into_iter().zip(hashes.lines()) {
            out.push((p, h.to_string()));
        }
    }
    out
}

/// `git diff --name-status <range>` 加上未追蹤檔。
fn git_changes(dir: &Path, range: &str) -> Changes {
    let mut out = BTreeMap::new();
    let diff = git(dir, &["diff", "--name-status", "-z", range]).unwrap_or_default();
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
    let others =
        git(dir, &["ls-files", "-z", "--others", "--exclude-standard"]).unwrap_or_default();
    for p in nul_split(&others) {
        out.entry(p).or_insert_with(|| ("?".to_string(), None));
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
    for e in store.events_of_types(id, &["file.edit", "file.write"])? {
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

fn nul_split(s: &str) -> Vec<String> {
    s.split('\0').filter(|p| !p.is_empty()).map(norm).collect()
}

/// 在 `dir` 跑 git；失敗回 None。清掉繼承的 GIT_*（collector 可能是從 git hook 裡被叫起來的）。
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut c = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(k);
        }
    }
    let out = c
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotePath=false"])
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
