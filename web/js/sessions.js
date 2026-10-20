// Sessions tab 與專案分組（#642，不碰 DOM）。
// - WS 的 sessions 列是即時的；GET /api/sessions 補上已結束、不在 live 狀態裡的（contracts §7）。
// - 專案：session 的 cwd 在另一個 session 的 cwd 底下（worktree、子目錄）就歸到那個上層；檔案樹一次看一個專案。

// ▶ 回放按鍵（#690）：只給已結束的 session；進行中的直接看即時串流
export const canReplay = (s) => s?.status === "ended";

import { norm, under } from "./paths.js";

// 同一個 id：WS 的列為準；WS 還沒有模型時用 API 查到的（SQLite 從 usage 補的，#673）。依 startedAt 新到舊
export function mergeSessions(live, fetched) {
  const byId = new Map();
  for (const s of fetched ?? []) byId.set(s.id, s);
  for (const s of live ?? []) {
    const f = byId.get(s.id);
    byId.set(s.id, { ...f, ...s, model: s.model ?? f?.model ?? null });
  }
  return [...byId.values()].sort((a, b) => (b.startedAt ?? 0) - (a.startedAt ?? 0));
}

// [{ cwd, sessions: [id], live }]，依 cwd 排序
export function projectRoots(sessions) {
  // cwd 先正規化：同一個專案的 e:\ 與 E:\ 歸成一個（#748 L-11）
  const cwds = [...new Set((sessions ?? []).map((s) => norm(s.cwd)).filter(Boolean))];
  const rootOf = (c) => cwds.filter((o) => under(c, o)).reduce((a, b) => (b.length < a.length ? b : a), c);
  const out = new Map();
  for (const s of sessions ?? []) {
    if (!s.cwd) continue;
    const r = rootOf(norm(s.cwd));
    let p = out.get(r);
    if (!p) out.set(r, (p = { cwd: r, sessions: [], live: 0 }));
    p.sessions.push(s.id);
    if (s.status !== "ended") p.live++;
  }
  return [...out.values()].sort((a, b) => (a.cwd < b.cwd ? -1 : a.cwd > b.cwd ? 1 : 0));
}

// session tab 列放不下時（#704）：widths 是每個 tab 的寬（含間距），avail 是可用寬，moreW 是「+N」按鈕的寬。
// 從前面放到放不下為止；選中的（selected）被收起來時換上來，從最後一個看得到的開始擠掉。回 { shown, hidden }（索引，遞增）
export function fitTabs(widths, avail, moreW, selected) {
  const all = widths.map((_, i) => i);
  const sum = (ix) => ix.reduce((t, i) => t + widths[i], 0);
  if (sum(all) <= avail) return { shown: all, hidden: [] };
  const room = avail - moreW;
  const shown = [];
  for (const i of all) {
    if (sum(shown) + widths[i] > room) break;
    shown.push(i);
  }
  if (selected >= 0 && selected < widths.length && !shown.includes(selected)) {
    while (shown.length && sum(shown) + widths[selected] > room) shown.pop();
    shown.push(selected);
    shown.sort((a, b) => a - b);
  }
  return { shown, hidden: all.filter((i) => !shown.includes(i)) };
}
