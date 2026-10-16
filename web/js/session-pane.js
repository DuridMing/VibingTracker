// 單 session 畫面（tab 與之後的監視器視窗，#649／#650）的資料：只吃 `session:<id>` 頻道。
// 終端機用 stream.js、改動用 changes.js（與全部串流同一套），git 列與思考條從 Event 推出來。
// NOW 與 lanes 用 collector 算好的 agents 列（agents.js），與 Agents 面板同一份狀態。

import { createStream } from "./stream.js";
import { createChanges } from "./changes.js";

export function createSessionPane(session = null) {
  const p = {
    session,
    stream: createStream(),
    changes: createChanges({ cap: 1000 }), // 單 session 的改動不多，留多一點給「碰過的檔案」點開看 diff
    git: { commits: [], last: null },
    thinking: null,
  };
  const seen = new Set(); // 已算過的 git.op 與 file.*（同一筆可能在 snapshot 與 frame 都出現）
  const toolStatus = new Map(); // toolUseId → pending／running／done／failed
  const fileMap = new Map(); // path → 碰過的檔案彙總（不受改動紀錄的筆數上限影響）

  function touchFile(e) {
    const key = `${e.session}:${e.seq}`;
    if (seen.has(key)) return;
    seen.add(key);
    const pl = e.payload ?? {};
    const f = fileMap.get(pl.path) ?? { path: pl.path ?? "", edits: 0, writes: 0, added: 0, removed: 0, isNew: false, agents: [], lastTs: 0, lastSeq: 0, toolUseIds: [] };
    if (e.type === "file.edit") {
      f.edits++;
      for (const l of pl.hunk?.lines ?? []) {
        if (l.startsWith("+")) f.added++;
        else if (l.startsWith("-")) f.removed++;
      }
    } else {
      f.writes++;
      if (pl.isNew) f.isNew = true;
    }
    if (!f.agents.includes(e.agent)) f.agents.push(e.agent);
    if (pl.toolUseId) f.toolUseIds.push(pl.toolUseId);
    if (e.ts_seen > f.lastTs || (e.ts_seen === f.lastTs && e.seq > f.lastSeq)) {
      f.lastTs = e.ts_seen;
      f.lastSeq = e.seq;
      f.lastToolUseId = pl.toolUseId ?? null;
    }
    fileMap.set(f.path, f);
  }

  function take(events) {
    for (const e of events ?? []) {
      if (e.session !== p.session) continue;
      if (e.type === "git.op") {
        const g = e.payload ?? {};
        const key = g.toolUseId ?? `${e.seq}`;
        if (g.op !== "commit" || !g.ok || seen.has(key)) continue;
        seen.add(key);
        const c = { hash: g.hash, message: g.message, agent: e.agent, branch: g.branch ?? null };
        p.git.commits.push(c);
        p.git.last = c;
      } else if (e.type === "file.edit" || e.type === "file.write") {
        touchFile(e);
      } else if (e.type === "tool.called" || e.type === "tool.updated") {
        const id = e.payload?.toolUseId;
        if (id && e.payload?.status) toolStatus.set(id, e.payload.status);
      } else if (e.type === "assistant.thinking") {
        p.thinking = { text: e.payload?.text ?? "", agent: e.agent, summarized: !!e.payload?.summarized };
      }
    }
  }

  // snapshot（第一次訂閱或切回這個 tab）：整份取代
  p.load = ({ staged = [], stream = [] } = {}) => {
    p.git = { commits: [], last: null };
    p.thinking = null;
    seen.clear();
    toolStatus.clear();
    fileMap.clear();
    const mine = (list) => list.filter((e) => e.session === p.session);
    const ops = p.stream.load({ staged: mine(staged), stream: mine(stream) });
    p.changes.load(mine(stream).filter((e) => e.type === "file.edit" || e.type === "file.write"));
    take(stream);
    return ops;
  };

  p.apply = (events) => {
    const mine = (events ?? []).filter((e) => e.session === p.session);
    const ops = p.stream.apply(mine);
    p.changes.apply(mine);
    take(mine);
    return ops;
  };

  // 換到另一個 session（同一個 pane 重用，畫面那層只建一次）；等新的 snapshot 再填內容
  // 這個 session 碰過的檔案：正在改（工具還沒結束）的在前，其餘依最近一次改動由新到舊
  p.files = () => {
    const live = (id) => ["pending", "running"].includes(toolStatus.get(id));
    const list = [...fileMap.values()].map((f) => ({ ...f, running: f.toolUseIds.some(live) }));
    return list.sort((a, b) => Number(b.running) - Number(a.running) || b.lastTs - a.lastTs || b.lastSeq - a.lastSeq);
  };
  p.open = (id) => {
    p.session = id;
    return p.load({});
  };

  return p;
}

const STATE_TEXT = { wait: "等你回應", done: "回合結束，等你確認", fail: "連續失敗，等你處理", danger: "危險指令", idle: "已中斷", ended: "已結束" };

// view：agentView() 的結果；row：sessions 列
export function nowOf(view, row) {
  const st = row?.status ?? "idle";
  if (st === "run") {
    if (view.main?.current) return { state: "run", what: view.main.current, agent: "main" };
    const sub = view.active.find((a) => a.status === "run" && a.current);
    if (sub) return { state: "run", what: sub.current, agent: sub.label };
    return { state: "run", what: row?.doing ?? "思考中", agent: "main" };
  }
  return { state: row ? st : "idle", what: row ? STATE_TEXT[st] ?? st : "等待中", agent: "main" };
}

// `*` 一直訂著（全部串流、彙總列都靠它），所以切回「全部」不必補任何事件；session 頻道只在看那個 tab 時訂
export function switchTab(client, from, to) {
  if (from === to) return;
  if (from !== "all") client.unsubscribe(`session:${from}`);
  if (to !== "all") client.subscribe(`session:${to}`);
}
