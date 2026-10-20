// 單 session 畫面（tab 與之後的監視器視窗，#649／#650）的資料：只吃 `session:<id>` 頻道。
// 終端機用 stream.js、改動用 changes.js（與全部串流同一套），git 列與思考條從 Event 推出來。
// NOW 與 lanes 用 collector 算好的 agents 列（agents.js），與 Agents 面板同一份狀態。

import { createStream } from "./stream.js";
import { createChanges, countHunk } from "./changes.js";
import { REPLAY_TAB } from "./replay.js";

// 遠端連線只拿到佔位（#668）：思考條顯示字數，不假裝有內容；hide 模式與 0 字沒東西可載，文案和串流列一致（#724）
export function thinkingText(payload, mode = null) {
  if (!payload?.deferred) return payload?.text ?? "";
  const n = payload.chars ?? null;
  if (n === 0) return "（思考）";
  if (mode === "hide") return `（思考 ${n ?? "?"} 字，這個 collector 不對遠端提供）`;
  return `（思考 ${n ?? "?"} 字，遠端未載入；在串流裡點開那一行可載入）`;
}

// p.thinkingMode(session)：這個 session 那台 collector 對遠端的 thinking 模式，由 main.js 掛上（hub 時每台不同）
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
  // 碰過的檔案（不受改動紀錄的筆數上限影響）：每筆 file.edit／file.write 一個紀錄，files() 時才彙總，
  // 這樣之後以 failed 結束的卡片（被拒或失敗的改動，contracts §4，#682）可以排除
  const touches = [];
  // files() 的結果依版本快取：touches 或 toolStatus 真的變了才重算、重排（單 session 畫面每次重繪都會叫，#757 E-F12）
  let ver = 0;
  let cached = null;

  function touchFile(e) {
    const key = `${e.session}:${e.seq}`;
    if (seen.has(key)) return;
    seen.add(key);
    ver++;
    const pl = e.payload ?? {};
    const { added, removed } = countHunk(pl.hunk?.lines);
    touches.push({ path: pl.path ?? "", edit: e.type === "file.edit", isNew: e.type === "file.write" && !!pl.isNew, added, removed, agent: e.agent, ts: e.ts_seen, seq: e.seq, toolUseId: pl.toolUseId ?? null });
  }

  function fileMap() {
    const out = new Map();
    for (const x of touches) {
      if (x.toolUseId && toolStatus.get(x.toolUseId) === "failed") continue;
      const f = out.get(x.path) ?? { path: x.path, edits: 0, writes: 0, added: 0, removed: 0, isNew: false, agents: [], lastTs: 0, lastSeq: 0, toolUseIds: [] };
      if (x.edit) {
        f.edits++;
        f.added += x.added;
        f.removed += x.removed;
      } else {
        f.writes++;
        if (x.isNew) f.isNew = true;
      }
      if (!f.agents.includes(x.agent)) f.agents.push(x.agent);
      if (x.toolUseId) f.toolUseIds.push(x.toolUseId);
      if (x.ts > f.lastTs || (x.ts === f.lastTs && x.seq > f.lastSeq)) {
        f.lastTs = x.ts;
        f.lastSeq = x.seq;
        f.lastToolUseId = x.toolUseId;
      }
      out.set(x.path, f);
    }
    return out;
  }

  // older：往前翻到的一段（載入更多、回放），比已知的都舊：commit 放前面，
  // 最後一筆 commit、思考條、工具狀態不被舊的蓋掉
  function take(events, older = false) {
    const commits = [];
    const status = new Map();
    let thinking = null;
    for (const e of events ?? []) {
      if (e.session !== p.session) continue;
      if (e.type === "git.op") {
        const g = e.payload ?? {};
        const key = g.toolUseId ?? `${e.seq}`;
        if (g.op !== "commit" || !g.ok || seen.has(key)) continue;
        seen.add(key);
        commits.push({ hash: g.hash, message: g.message, agent: e.agent, branch: g.branch ?? null });
      } else if (e.type === "file.edit" || e.type === "file.write") {
        touchFile(e);
      } else if (e.type === "tool.called" || e.type === "tool.updated") {
        const id = e.payload?.toolUseId;
        if (id && e.payload?.status) status.set(id, e.payload.status);
      } else if (e.type === "assistant.thinking") {
        thinking = { text: thinkingText(e.payload, p.thinkingMode?.(p.session)), agent: e.agent, summarized: !!e.payload?.summarized };
      }
    }
    for (const [id, st] of status) {
      if ((!older || !toolStatus.has(id)) && toolStatus.get(id) !== st) {
        toolStatus.set(id, st);
        ver++;
      }
    }
    if (older) {
      p.git.commits.unshift(...commits);
      p.git.last ??= commits.at(-1) ?? null;
      p.thinking ??= thinking;
    } else {
      p.git.commits.push(...commits);
      if (commits.length) p.git.last = commits.at(-1);
      if (thinking) p.thinking = thinking;
    }
  }

  // snapshot（第一次訂閱或切回這個 tab）：整份取代
  p.load = ({ staged = [], stream = [] } = {}) => {
    p.git = { commits: [], last: null };
    p.thinking = null;
    seen.clear();
    toolStatus.clear();
    touches.length = 0;
    ver++;
    const mine = (list) => list.filter((e) => e.session === p.session);
    const ops = p.stream.load({ staged: mine(staged), stream: mine(stream) });
    p.changes.load(mine(stream).filter((e) => e.type === "file.edit" || e.type === "file.write" || (e.type === "tool.updated" && e.payload?.status === "failed")));
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

  // 載入更多與回放（#653）：collector 依 pos 排好的一段（舊到新），比畫面上的都舊
  p.prepend = (events) => {
    const mine = (events ?? []).filter((e) => e.session === p.session);
    const ops = p.stream.prepend([mine]);
    p.changes.prepend(mine);
    take(mine, true);
    return ops;
  };

  // 換到另一個 session（同一個 pane 重用，畫面那層只建一次）；等新的 snapshot 再填內容
  // 這個 session 碰過的檔案：正在改（工具還沒結束）的在前，其餘依最近一次改動由新到舊
  p.files = () => {
    if (cached?.ver === ver) return cached.list;
    const live = (id) => ["pending", "running"].includes(toolStatus.get(id));
    const list = [...fileMap().values()].map((f) => ({ ...f, running: f.toolUseIds.some(live) }));
    list.sort((a, b) => Number(b.running) - Number(a.running) || b.lastTs - a.lastTs || b.lastSeq - a.lastSeq);
    cached = { ver, list };
    return list;
  };
  p.open = (id) => {
    p.session = id;
    return p.load({});
  };

  return p;
}

// 回放（#653）：session 已從 live 移除（結束 2 小時後，contracts §6）時，session 頻道的 snapshot
// 沒有它的串流，改用「載入更多」從 JSONL 由最新往前讀
export function needsReplay(session, { stream = [] } = {}) {
  return !stream.some((e) => e.session === session && e.pos != null);
}

// session 頻道的 snapshot 怎麼填進 pane：按了「▶ 回放」（#690）就不用 snapshot 的串流（結束不到 2 小時的還在 live），
// 一律清空後從 JSONL 由最新往前讀；沒按時照舊，snapshot 沒有它的串流才自動回放
export function snapshotLoad(session, m, { replay = false } = {}) {
  if (replay) return { load: { staged: [], stream: [] }, replay: true };
  return { load: { staged: m.staged, stream: m.stream }, replay: needsReplay(session, m) };
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
// 「全部串流」與「專案回放」（#702）不是 session 分頁：不訂閱 session 頻道
export const isSessionTab = (t) => t !== "all" && t !== REPLAY_TAB;

export function switchTab(client, from, to) {
  if (from === to) return;
  if (isSessionTab(from)) client.unsubscribe(`session:${from}`);
  if (isSessionTab(to)) client.subscribe(`session:${to}`);
}

// session 的滑過提示（tab、Sessions 列）：完整 id、現在在做什麼（§6 doing，#648）、最近的 prompt
export function sessionTitle(s) {
  const lines = [s.id];
  if (s.doing) lines.push(`▸ ${s.doing}`);
  if (s.lastPrompt) lines.push(`最近的 prompt：${s.lastPrompt}`);
  return lines.join("\n");
}
