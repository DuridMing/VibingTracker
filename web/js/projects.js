// 單機全域「全部專案／單一專案」（#669，PLAN「網路綁定」第 5 點；不碰 DOM）。
// - 專案 = sessions.js 的 projectRoots 算出的根 cwd（worktree 與子目錄歸上層）。
// - 全部專案：串流每行加專案色標、檔案樹與改動紀錄依專案分組；單一專案：其他專案的列只隱藏、不移除。
// - 專案色和 session 色用同一組色票（--s0 … --s11），但同時在畫面上的專案彼此不撞色。

import { PALETTE_SIZE, sessionColorIndex } from "./color.js";

const norm = (p) => String(p ?? "").replace(/\\/g, "/").replace(/\/+$/, "");
const under = (p, base) => p === base || p.startsWith(`${base}/`);

// cwd → 色票索引。每個專案先取雜湊色（和重新整理前、別的視窗一致），撞色時依 cwd 排序後面的往後找空位；
// 同一組專案不論順序都得到同樣的結果，新加一個專案不會動到既有的（除非它撞到的是新的那個）。
export function projectColors(cwds) {
  const out = new Map();
  const taken = new Set();
  const sorted = [...new Set((cwds ?? []).map(norm).filter(Boolean))].sort();
  const want = new Map(sorted.map((c) => [c, sessionColorIndex(c)]));
  // 先放沒有人搶的雜湊色，再處理撞色的（排序靠前的先拿）
  const claim = new Map();
  for (const c of sorted) {
    const i = want.get(c);
    if (!claim.has(i)) claim.set(i, c);
  }
  for (const [i, c] of claim) {
    out.set(c, i);
    taken.add(i);
  }
  for (const c of sorted) {
    if (out.has(c)) continue;
    let i = want.get(c);
    if (taken.size < PALETTE_SIZE) while (taken.has(i)) i = (i + 1) % PALETTE_SIZE;
    out.set(c, i);
    taken.add(i);
  }
  return out;
}

export const projectColor = (colors, cwd) => `var(--s${colors.get(norm(cwd)) ?? sessionColorIndex(norm(cwd))})`;

// cwd → 顯示名稱：最後一段；重複時往前多帶一段，直到不重複或用完
export function projectLabels(cwds) {
  const list = [...new Set((cwds ?? []).map(norm).filter(Boolean))];
  const parts = new Map(list.map((c) => [c, c.split("/").filter(Boolean)]));
  const out = new Map();
  const depth = new Map(list.map((c) => [c, 1]));
  for (;;) {
    const label = (c) => {
      const p = parts.get(c);
      return p.length ? p.slice(-Math.min(depth.get(c), p.length)).join("/") : c;
    };
    const count = new Map();
    for (const c of list) count.set(label(c), (count.get(label(c)) ?? 0) + 1);
    let bumped = false;
    for (const c of list) {
      if (count.get(label(c)) > 1 && depth.get(c) < parts.get(c).length) {
        depth.set(c, depth.get(c) + 1);
        bumped = true;
      }
    }
    if (!bumped) {
      for (const c of list) out.set(c, label(c));
      return out;
    }
  }
}

// session 的 cwd 歸到哪個專案根；不在任何根底下回 null
export function projectOf(cwd, roots) {
  const c = norm(cwd);
  if (!c) return null;
  let best = null;
  for (const r of roots ?? []) {
    const n = norm(r);
    if (under(c, n) && (best == null || n.length > best.length)) best = n;
  }
  return best;
}

// 單一專案時的 session 篩選：null＝全部不篩。還不知道 cwd 的 session（WS 列還沒到）不藏，免得漏看
export function sessionFilterFor(project, roots, cwdOf) {
  if (!project) return null;
  const p = norm(project);
  return (id) => {
    const cwd = cwdOf(id);
    if (!cwd) return true;
    return projectOf(cwd, roots) === p;
  };
}

// 依專案分組，依第一次出現的順序；不屬於任何專案的放最後一組（cwd: null）
export function groupByProject(items, projectOfItem) {
  const groups = new Map();
  let none = null;
  for (const it of items ?? []) {
    const cwd = projectOfItem(it);
    if (cwd == null) {
      (none ??= { cwd: null, items: [] }).items.push(it);
      continue;
    }
    let g = groups.get(cwd);
    if (!g) groups.set(cwd, (g = { cwd, items: [] }));
    g.items.push(it);
  }
  const out = [...groups.values()];
  if (none) out.push(none);
  return out;
}

// 檔案樹（全部專案）：每個專案一個標題列（hdr）接著它的列；collapsed 裡的專案只留標題。只有一個專案時不加標題
export function projectTreeRows(entries, collapsed) {
  const list = entries ?? [];
  if (list.length <= 1) return list.flatMap((e) => e.rows);
  const out = [];
  for (const e of list) {
    const open = !collapsed?.has(e.cwd);
    out.push({ hdr: true, cwd: e.cwd, open, n: e.rows.length });
    if (open) out.push(...e.rows);
  }
  return out;
}
