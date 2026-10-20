// 「分支」浮動視窗的純邏輯（#696，PLAN「git 追蹤」）：
// - 資料來自 GET /api/branches（contracts §7）：一個 worktree 一列，session 依 cwd 歸屬，ahead／behind 相對整合分支（#693）。
// - 衝突候選的檔點開，用 GET /api/branch-diff 拿兩條分支各自對 merge-base 的 diff 並排。

const q = (o) => Object.entries(o).map(([k, v]) => `${k}=${encodeURIComponent(v)}`).join("&");

export const branchesUrl = (cwd) => `/api/branches?${q({ cwd })}`;
export const branchDiffUrl = (cwd, worktree, path) => `/api/branch-diff?${q({ cwd, worktree, path })}`;
export const graphUrl = (cwd) => `/api/graph?${q({ cwd })}`;
// target：{ hash } 或 { worktree }（未 commit 的改動）
export const commitUrl = (cwd, target) => `/api/commit?${q({ cwd, ...target })}`;
export const commitDiffUrl = (cwd, target, path) => `/api/commit-diff?${q({ cwd, ...target, path })}`;

// 按鈕上的 N：有改動（已 commit 或還沒 commit）的分支，整合分支本身不算
export function changedBranches(body) {
  return (body?.branches ?? []).filter((r) => !r.integration && ((r.changed ?? 0) > 0 || (r.dirty ?? 0) > 0)).length;
}

// commit 圖底下的提示列（#709）：回陣列，沒截斷就是空的；呼叫端用 ... 展開，不能把 null 交給 replaceChildren（會顯示文字「null」）
export function graphNotices(graph) {
  return graph?.truncated ? [`只列最近 ${graph.commits.length} 個 commit`] : [];
}

// unified diff → 行：meta（檔頭）、hunk、add、del、ctx
export function diffLines(diff) {
  if (!diff) return [];
  const lines = diff.endsWith("\n") ? diff.slice(0, -1).split("\n") : diff.split("\n");
  let inHunk = false;
  return lines.map((l) => {
    if (l.startsWith("@@")) {
      inHunk = true;
      return { kind: "hunk", text: l };
    }
    if (!inHunk || l.startsWith("diff --git")) {
      inHunk = false;
      return { kind: "meta", text: l };
    }
    if (l.startsWith("+")) return { kind: "add", text: l.slice(1) };
    if (l.startsWith("-")) return { kind: "del", text: l.slice(1) };
    return { kind: "ctx", text: l.slice(1) };
  });
}

export function worktreeLabel(root, wt) {
  if (wt === root) return "主工作樹";
  return wt.startsWith(`${root}/`) ? wt.slice(root.length + 1) : wt;
}

// 分支線配置（像 VS Code 的 Git Graph）：commit 依拓撲順序由上往下，lanes[k] 是第 k 欄正在等的 commit。
// 每列回 { col, lane, segments }：col 是圓點的欄；segments 的 x 是欄、y 是 0（上緣）／0.5（圓點）／1（下緣）；
// lane 是線的編號（同一條線一路同色，收掉的欄重用時換新編號）。
export function layoutGraph(commits) {
  const lanes = []; // 欄 → 等待中的 hash（null＝空）
  const ids = []; // 欄 → 線的編號
  let next = 0;
  let width = 0;
  const open = (hash) => {
    let k = lanes.indexOf(null);
    if (k < 0) k = lanes.length;
    lanes[k] = hash;
    ids[k] = next++;
    return k;
  };
  const rows = commits.map((c) => {
    let col = lanes.indexOf(c.hash);
    const tip = col < 0; // 沒有線在等它：分支的頂端，圓點上方不畫線
    if (tip) col = open(c.hash);
    const lane = ids[col];
    const segments = [];
    // 上半：等這個 commit 的線收進圓點，其他的直穿
    lanes.forEach((h, k) => {
      if (h == null || (tip && k === col)) return;
      if (h === c.hash) segments.push({ x1: k, y1: 0, x2: col, y2: 0.5, lane: ids[k] });
      else segments.push({ x1: k, y1: 0, x2: k, y2: 1, lane: ids[k] });
    });
    lanes.forEach((h, k) => { if (h === c.hash) lanes[k] = null; });
    // 下半：第一個 parent 接著這一欄，其他 parent 接到已經在等它的欄或開新欄
    const [first, ...rest] = c.parents ?? [];
    if (first != null) {
      lanes[col] = first;
      ids[col] = lane;
      segments.push({ x1: col, y1: 0.5, x2: col, y2: 1, lane });
    }
    for (const p of rest) {
      let k = lanes.indexOf(p);
      if (k < 0) k = open(p);
      segments.push({ x1: col, y1: 0.5, x2: k, y2: 1, lane: ids[k] });
    }
    while (lanes.length && lanes[lanes.length - 1] == null) lanes.pop();
    width = Math.max(width, col + 1, ...segments.map((s) => Math.max(s.x1, s.x2) + 1));
    return { ...c, col, lane, segments };
  });
  return { rows, width };
}

// 未 commit 的改動：每個有 dirty 的 worktree 在它的 HEAD 正上方多一列，parent 是 HEAD
export function withWip(commits, branches) {
  const wip = new Map();
  for (const b of branches ?? []) if ((b.dirty ?? 0) > 0 && b.head) {
    if (!wip.has(b.head)) wip.set(b.head, []);
    wip.get(b.head).push(b);
  }
  return commits.flatMap((c) => [
    ...(wip.get(c.hash) ?? []).map((b) => ({ hash: `wip:${b.worktree}`, parents: [c.hash], wip: b })),
    c,
  ]);
}
