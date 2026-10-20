// 左欄「專案檔案」（#642）與改動基準（#644）的資料（不碰 DOM）。PLAN「左欄改動的檔案以什麼為基準」：
// - 樹的檔案來自 GET /api/files 的 tree（collector 跑 git ls-files，已遵守 .gitignore）；Write 新檔再長節點。
// - 方塊看活動量（Event 累積：讀／改／新建／失敗），檔名顏色看 git 狀態（基準的 changes）。兩件事分開存。
// - 只展開被碰過或有改動的目錄；超過 LAZY_THRESHOLD 個檔時，目錄的子節點等到要顯示才建。

export const LAZY_THRESHOLD = 5000;

import { norm, relUnder as under } from "./paths.js";

const isAbs = (p) => p.startsWith("/") || /^[A-Za-z]:\//.test(p);
const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

function ancestors(path) {
  const out = [];
  for (let i = path.indexOf("/"); i >= 0; i = path.indexOf("/", i + 1)) out.push(path.slice(0, i));
  return out;
}

// 絕對目錄 dir 接上相對路徑 rel（處理 . 與 ..）
function join(dir, rel) {
  const out = dir.split("/");
  for (const seg of rel.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (out.length > 1) out.pop();
    } else out.push(seg);
  }
  return out.join("/");
}

// 包含 p 的 worktree 裡最深的那個（主工作樹也在清單裡，裡面的 worktree 比它深）
function worktreeOf(p, worktrees) {
  let best = null;
  for (const w of worktrees) if (under(p, w) != null && (best == null || w.length > best.length)) best = w;
  return best;
}

// file.* 的 path（相對 session 的 cwd，或根以外的絕對路徑，contracts §4）→ 相對專案根的路徑；不在專案裡回 null。
// collector 給了 worktree 清單（/api/files 的 worktrees）時照它：cwd 接上路徑落在哪個 worktree，就相對那個 worktree
// （session 從主工作樹開、改 worktree 裡的檔時，路徑是 worktrees/… 這種穿進去的相對路徑）；
// cwd 在主工作樹裡不在樹上的目錄（被 ignore 的，例如 e2e 的暫存目錄）不算專案。
// 沒有清單（舊的 collector）時用猜的：樹裡有這個目錄就是子目錄（補前綴），沒有就當成 worktree。
export function projectPath({ root, cwd, path, hasDir, worktrees = null }) {
  const r = norm(root);
  const p = norm(path);
  if (!r || !p) return null;
  const wts = worktrees?.length ? worktrees.map(norm) : null;
  if (isAbs(p)) {
    const w = wts && worktreeOf(p, wts);
    return (w ? under(p, w) : under(p, r)) || null;
  }
  if (cwd == null) return null;
  if (wts) {
    const c = norm(cwd);
    const cw = worktreeOf(c, wts);
    if (cw === r) {
      const rel = under(c, r);
      if (rel && !hasDir(rel)) return null;
    }
    const abs = join(c, p);
    const w = worktreeOf(abs, wts);
    return w ? under(abs, w) || null : null;
  }
  const c = under(norm(cwd), r);
  if (c == null) return null;
  if (c === "" || !hasDir(c)) return p;
  return `${c}/${p}`;
}

// /api/files 的 changes → path → 樹上的標記。state：mod（有未合併差異，琥珀）、new（A 或未追蹤，紫）、del（刪除）
const MODIFY = new Set(["M", "R", "C", "T"]);
export function gitMarks(changes) {
  const out = new Map();
  for (const c of changes ?? []) {
    const by = c.by ?? [];
    const st = by.map((b) => b.status);
    const state = st.length && st.every((s) => s === "D") ? "del" : st.some((s) => MODIFY.has(s) || s === "D") ? "mod" : "new";
    out.set(c.path, {
      state,
      conflict: !!c.conflict,
      by,
      branches: by.map((b) => b.branch ?? b.session ?? null).filter(Boolean),
      from: by.find((b) => b.from)?.from ?? null,
    });
  }
  return out;
}

// 基準 → GET /api/files 的網址。「本次開啟以來」不在 collector（前端累積），仍要 tree，用 base=main 抓
// have：上次回應的 treeHash，相同時伺服器省略 tree（樹只在新增或刪除檔案時才變，#766 F-6）
export function filesUrl(cwd, base, have = null) {
  const q = (c, b) => `/api/files?cwd=${encodeURIComponent(c)}&base=${encodeURIComponent(b)}${have ? `&have=${encodeURIComponent(have)}` : ""}`;
  switch (base?.kind) {
    case "session":
      return q(cwd, `session:${base.id}`);
    case "commit":
      return /^[0-9a-f]{4,64}$/i.test(base.hash ?? "") ? q(base.cwd ?? cwd, `commit:${base.hash.toLowerCase()}`) : null;
    default:
      return q(cwd, "main");
  }
}

// 「本次開啟以來」：這個頁面收到的 file.edit／file.write 累積成跟 /api/files 一樣的 changes（session 基準的寫法：A 或 M）
export function sinceChanges(t) {
  return [...t.editsBySession()]
    .sort(([a], [b]) => cmp(a, b))
    .map(([path, m]) => ({ path, conflict: false, by: [...m].map(([session, status]) => ({ session, status })) }));
}

const PENDING_MAX = 2000;
// 去重（session:seq）與卡片對照（toolUseId）的上限：超過就丟最舊的四分之一（Set／Map 依插入順序）。
// snapshot、重連重送的只有最近的視窗，早就丟掉的鍵不會再出現，計數不會重複（#766 F-5）
const SEEN_MAX = 100_000;
function cap(coll, max = SEEN_MAX) {
  if (coll.size <= max) return;
  let n = coll.size - Math.floor(max * 0.75);
  for (const k of coll.keys()) {
    if (n-- <= 0) break;
    coll.delete(k);
  }
}

export function createFileTree() {
  const t = {
    root: null,
    lazy: false,
    truncated: false,
    cwdOf: () => null, // session id → 第一次 session.started 的 cwd（sessions 列）
  };
  let files = []; // 排序過
  let fileSet = new Set();
  let dirSet = new Set();
  const extras = new Set(); // Event 碰到、但不在 tree 的檔（Write 新檔等）
  let gitExtras = new Set(); // 基準裡有、但不在 tree 的檔（刪除的檔）
  const memo = new Map(); // 目錄 → { dirs, files }（子節點名稱）
  let git = new Map();
  let gitDirs = new Map(); // 目錄 → { conflict }
  const counts = new Map(); // path → { r, e, w, x, isNew }
  const edits = new Map(); // path → Map(session → Map(toolUseId → "A" | "M"))，「本次開啟以來」用
  const activeDirs = new Set();
  const user = new Map(); // 目錄 → 手動展開（true）／收起（false）
  const last = new Map(); // session → 最後碰的檔
  const seen = new Set(); // session:seq
  const toolPath = new Map(); // toolUseId → { path, session, kind }
  const pendingFail = new Map(); // 失敗先到、file.* 還沒到的 toolUseId → 是不是 Esc 中斷
  const revoked = new Set(); // 以 failed 結束、已收回「改／新建」的 toolUseId（#682）
  const countedFail = new Set();
  let early = []; // 樹還沒載入（不知道根）時先收著的 Event
  let worktrees = null; // /api/files 的 worktrees（路徑）

  const hasDir = (d) => dirSet.has(d);
  const addDirs = (path) => { for (const d of ancestors(path)) dirSet.add(d); };

  function addNode(path) {
    if (fileSet.has(path) || extras.has(path)) return;
    extras.add(path);
    addDirs(path);
    memo.clear();
  }

  // 目錄的子節點：lazy 時用二分搜尋找出 tree 裡這個前綴的範圍，只算這一層
  function childrenOf(dir) {
    let c = memo.get(dir);
    if (c) return c;
    if (!t.lazy) {
      buildAll();
      return memo.get(dir) ?? { dirs: [], files: [] };
    }
    const prefix = dir ? `${dir}/` : "";
    const ds = new Set();
    const fs = [];
    const take = (p) => {
      const rest = p.slice(prefix.length);
      const i = rest.indexOf("/");
      if (i < 0) fs.push(rest);
      else ds.add(rest.slice(0, i));
    };
    let lo = 0;
    let hi = files.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (files[mid] < prefix) lo = mid + 1;
      else hi = mid;
    }
    for (let i = lo; i < files.length && files[i].startsWith(prefix); i++) take(files[i]);
    for (const p of extras) if (p.startsWith(prefix)) take(p);
    for (const p of gitExtras) if (p.startsWith(prefix) && !extras.has(p)) take(p);
    c = { dirs: [...ds].sort(cmp), files: [...new Set(fs)].sort(cmp) };
    memo.set(dir, c);
    return c;
  }

  function buildAll() {
    memo.clear();
    const get = (d) => {
      let c = memo.get(d);
      if (!c) memo.set(d, (c = { dirs: new Set(), files: new Set() }));
      return c;
    };
    get("");
    for (const p of [...files, ...extras, ...gitExtras]) {
      const parts = p.split("/");
      let dir = "";
      for (let i = 0; i < parts.length - 1; i++) {
        get(dir).dirs.add(parts[i]);
        dir = dir ? `${dir}/${parts[i]}` : parts[i];
      }
      get(dir).files.add(parts.at(-1));
    }
    for (const [d, c] of memo) memo.set(d, { dirs: [...c.dirs].sort(cmp), files: [...c.files].sort(cmp) });
  }

  function count(path) {
    let c = counts.get(path);
    if (!c) counts.set(path, (c = { r: 0, e: 0, w: 0, x: 0, isNew: false }));
    return c;
  }

  function countFail(id, hits) {
    const tp = toolPath.get(id);
    countedFail.add(id);
    cap(countedFail);
    pendingFail.delete(id);
    count(tp.path).x++;
    hits.push({ path: tp.path, kind: "x", session: tp.session });
  }

  // 被拒或失敗的 Edit／Write 視為沒有發生（contracts §4，#682）：收回改／新建的計數與「本次開啟以來」
  function revoke(id) {
    const tp = toolPath.get(id);
    if (!tp || tp.kind === "r" || revoked.has(id)) return;
    revoked.add(id);
    cap(revoked);
    const c = count(tp.path);
    c[tp.kind] = Math.max(0, c[tp.kind] - 1);
    if (tp.kind === "w" && c.w === 0) c.isNew = false;
    const m = edits.get(tp.path);
    const ids = m?.get(tp.session);
    ids?.delete(id);
    if (ids && ids.size === 0) m.delete(tp.session);
    if (m && m.size === 0) edits.delete(tp.path);
  }

  // 一張卡片以 failed 結束：收回改動；Esc 中斷不算失敗（失敗計數只看真的失敗）
  function failCard(id, interrupted, hits) {
    revoke(id);
    if (!interrupted && !countedFail.has(id)) countFail(id, hits);
    pendingFail.delete(id);
  }

  // tree 換了（換基準、重抓）：根一樣就保留累積計數與展開狀態
  t.setFiles = ({ root, tree = [], treeTruncated = false, worktrees: wts = null }) => {
    const r = norm(root);
    if (t.root != null && r !== t.root) throw new Error(`專案根不同：${t.root} → ${r}`);
    t.root = r;
    if (Array.isArray(wts)) worktrees = wts.map((w) => w.path).filter(Boolean);
    files = [...tree].sort(cmp);
    fileSet = new Set(files);
    t.lazy = files.length > LAZY_THRESHOLD;
    t.truncated = !!treeTruncated;
    dirSet = new Set();
    for (const p of files) addDirs(p);
    for (const p of [...extras]) {
      if (fileSet.has(p)) extras.delete(p);
      else addDirs(p);
    }
    for (const p of gitExtras) addDirs(p);
    memo.clear();
    if (early.length) {
      const ev = early;
      early = [];
      t.apply(ev);
    }
  };

  // 回傳這批碰到的檔（閃爍、點亮父目錄用）：{ path, kind: r|e|w|x, session }
  t.apply = (events) => {
    const hits = [];
    for (const e of events ?? []) {
      const ty = e.type;
      const isFile = ty === "file.read" || ty === "file.edit" || ty === "file.write";
      const isFail = ty === "tool.updated" && e.payload?.status === "failed";
      if (!isFile && !isFail) continue;
      if (t.root == null) {
        if (early.length < 20_000) early.push(e);
        continue;
      }
      const key = `${e.session}:${e.seq}`;
      if (seen.has(key)) continue;
      seen.add(key);
      cap(seen);
      const pl = e.payload ?? {};
      const id = pl.toolUseId;
      if (isFail) {
        if (!id) continue;
        // Esc 中斷與使用者拒絕（denied）都不算失敗，但改動一樣收回（contracts §4「失敗」的計數，#745 D6）
        const interrupted = !!pl.interrupted || !!pl.denied;
        if (toolPath.has(id)) failCard(id, interrupted, hits);
        else {
          pendingFail.set(id, interrupted);
          if (pendingFail.size > PENDING_MAX) pendingFail.delete(pendingFail.keys().next().value);
        }
        continue;
      }
      const path = t.pathOf(t.cwdOf(e.session), pl.path);
      if (path == null) continue;
      const kind = ty === "file.read" ? "r" : ty === "file.write" && pl.isNew ? "w" : "e";
      const c = count(path);
      c[kind]++;
      if (kind === "w") c.isNew = true;
      if (kind !== "r") {
        let m = edits.get(path);
        if (!m) edits.set(path, (m = new Map()));
        let ids = m.get(e.session);
        if (!ids) m.set(e.session, (ids = new Map()));
        ids.set(id ?? key, kind === "w" ? "A" : "M");
      }
      addNode(path);
      for (const d of ancestors(path)) activeDirs.add(d);
      last.set(e.session, path);
      hits.push({ path, kind, session: e.session });
      if (id) {
        toolPath.set(id, { path, session: e.session, kind });
        cap(toolPath);
        if (pendingFail.has(id)) failCard(id, pendingFail.get(id), hits);
      }
    }
    return hits;
  };

  // session 的 cwd 加 file.* 的 path → 相對專案根（不在專案裡回 null）
  t.pathOf = (cwd, path) => projectPath({ root: t.root, cwd, path, hasDir, worktrees });

  t.sizes = () => ({ seen: seen.size, toolPath: toolPath.size, revoked: revoked.size, countedFail: countedFail.size });

  t.counts = (path) => ({ ...(counts.get(path) ?? { r: 0, e: 0, w: 0, x: 0, isNew: false }) });

  // 正在碰這個檔的 session：每個 session 最後碰的檔，只算 running 裡的（還在跑的 session）
  t.touching = (path, running) => [...last].filter(([s, p]) => p === path && running.has(s)).map(([s]) => s);

  // path → Map(session → "A" | "M")：同一個 session 對同一檔有新建就是 A
  t.editsBySession = () =>
    new Map([...edits].map(([path, m]) => [path, new Map([...m].map(([s, ids]) => [s, [...ids.values()].includes("A") ? "A" : "M"]))]));

  // 基準的 changes；mapPath 把 changes 的 path 換成相對專案根（session 基準的 path 相對該 session 的 cwd）
  t.setGit = (changes, mapPath = (p) => p) => {
    const mapped = [];
    for (const c of changes ?? []) {
      const p = mapPath(c.path, c);
      if (p != null) mapped.push({ ...c, path: p });
    }
    git = gitMarks(mapped);
    gitDirs = new Map();
    gitExtras = new Set();
    for (const [p, m] of git) {
      for (const d of ancestors(p)) {
        const g = gitDirs.get(d);
        gitDirs.set(d, { conflict: (g?.conflict ?? false) || m.conflict });
      }
      if (!fileSet.has(p) && !extras.has(p)) {
        gitExtras.add(p);
        addDirs(p);
      }
    }
    memo.clear();
  };

  t.mark = (path) => git.get(path) ?? null;

  const isOpen = (dir) => (user.has(dir) ? user.get(dir) : activeDirs.has(dir) || gitDirs.has(dir));
  t.toggle = (dir) => user.set(dir, !isOpen(dir));

  t.materialized = () => memo.size;

  // 目前看得到的列（前序走訪，目錄在前）：{ path, name, depth, dir, open, pre, git, conflict }
  t.visible = () => {
    const rows = [];
    const walk = (dir, depth, prefix) => {
      const c = childrenOf(dir);
      const n = c.dirs.length + c.files.length;
      let i = 0;
      for (const name of c.dirs) {
        const path = dir ? `${dir}/${name}` : name;
        const lastOne = ++i === n;
        const open = isOpen(path);
        const g = gitDirs.get(path);
        rows.push({ path, name, depth, dir: true, open, pre: prefix + (lastOne ? "└─ " : "├─ "), git: g ? "mod" : null, conflict: !!g?.conflict });
        if (open) walk(path, depth + 1, prefix + (lastOne ? "   " : "│  "));
      }
      for (const name of c.files) {
        const path = dir ? `${dir}/${name}` : name;
        const lastOne = ++i === n;
        const m = git.get(path);
        rows.push({ path, name, depth, dir: false, open: false, pre: prefix + (lastOne ? "└─ " : "├─ "), git: m?.state ?? null, conflict: !!m?.conflict });
      }
    };
    walk("", 0, "");
    return rows;
  };

  return t;
}
