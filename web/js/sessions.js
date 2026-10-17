// Sessions tab 與專案分組（#642，不碰 DOM）。
// - WS 的 sessions 列是即時的；GET /api/sessions 補上已結束、不在 live 狀態裡的（contracts §7）。
// - 專案：session 的 cwd 在另一個 session 的 cwd 底下（worktree、子目錄）就歸到那個上層；檔案樹一次看一個專案。

// ▶ 回放按鍵（#690）：只給已結束的 session；進行中的直接看即時串流
export const canReplay = (s) => s?.status === "ended";

const under = (p, base) => p === base || p.startsWith(base.endsWith("/") ? base : `${base}/`);

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
  const cwds = [...new Set((sessions ?? []).map((s) => s.cwd).filter(Boolean))];
  const rootOf = (c) => cwds.filter((o) => under(c, o)).reduce((a, b) => (b.length < a.length ? b : a), c);
  const out = new Map();
  for (const s of sessions ?? []) {
    if (!s.cwd) continue;
    const r = rootOf(s.cwd);
    let p = out.get(r);
    if (!p) out.set(r, (p = { cwd: r, sessions: [], live: 0 }));
    p.sessions.push(s.id);
    if (s.status !== "ended") p.live++;
  }
  return [...out.values()].sort((a, b) => (a.cwd < b.cwd ? -1 : a.cwd > b.cwd ? 1 : 0));
}

// 使用者選過、而且還在就用它；否則選進行中 session 最多的
export function pickProject(roots, prev) {
  if (!roots.length) return null;
  if (prev && roots.some((r) => r.cwd === prev)) return prev;
  return roots.reduce((a, b) => (b.live > a.live ? b : a)).cwd;
}
