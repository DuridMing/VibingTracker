// 開發用假 collector：網址加 ?mock=<session 數> 時取代 WebSocket（加 &bench 是效能量測模式）。
// 送的是契約（contracts.md §4、§6）格式的 welcome／snapshot／frame／more，走跟真連線同一條 client 路徑。
// 只捏我們自己的 Event／rows 格式，不捏 transcript。

const STATUSES = ["run", "run", "run", "wait", "done", "fail", "danger"];
const PROMPTS = [
  "列表 API 加分頁", "修 auth middleware 的 token 過期", "加 health endpoint", "重構 repo 層",
  "補 users 的整合測試", "手機鍵盤遮住輸入框", "升級 express 到 5", "整理 CI cache",
  "把設定改成 toml", "修 flaky 的 e2e", "加 rate limit", "清掉舊的 migration",
  "寫 README 安裝段落", "拆 monorepo 的 web 套件", "改 log 格式", "加 OpenAPI 文件",
];
const THOUGHTS = [
  "users 列表沒有分頁，先看 repo.list 的簽名。",
  "測試失敗是因為 page 預設成 0，應該從 1 開始。\n另外 size 沒有上限，要夾在 100 以內。",
  "這個 middleware 在 token 過期時直接 500，要改回 401。",
  "先跑一次測試確認現況，再改。",
  "git status 乾淨，可以 commit。",
];
const TEXTS = ["我先讀相關的檔案。", "改好了，接著跑測試。", "測試全過，準備 commit。", "這裡需要你確認要不要同時更新 README。"];
const ROOT = "/home/me/my-api";
// 第二個專案（#669）：mock ≥ 6 個 session 時，每第五個 session 在另一個 repo
const ROOT2 = "/home/me/side-tool";
const FILES2 = ["README.md", "main.py", "lib/util.py", "lib/cli.py", "tests/test_cli.py"];
const rootOf = (c) => (c && [ROOT, ROOT2].find((r) => c === r || c.startsWith(`${r}/`))) || null;
const FILES = ["src/api/users.ts", "src/repo/users.ts", "src/middleware/auth.ts", "tests/users.test.ts", "src/web/useKeyboardInset.ts", "README.md", "src/config.ts"];
const BASH = [
  { command: "npm test", out: "✓ users list (12 ms)\n✓ users paging (8 ms)\n\nTests: 2 passed", ok: true },
  { command: "npm test -- users", out: "✗ users paging\n  expected 20, got 0\n\nTests: 1 failed, 1 passed", ok: false },
  { command: "git status --short", out: " M src/api/users.ts\n?? tests/users.test.ts", ok: true },
  { command: "ls src/api", out: "health.ts\nusers.ts", ok: true },
];
const DANGER = [{ command: "git push --force origin main", why: "強制推送會覆蓋遠端歷史", rule: "git-force-push" }, { command: "rm -rf dist build", why: "遞迴刪除", rule: "rm-rf" }];
const AGENT_TYPES = ["Explore", "general-purpose", "Plan"];
const SCORE_BASE = { danger: 100, fail: 60, wait: 50, done: 20 };
const ATTENTION_REASON = { wait: "要不要同時更新 README？", done: "回合結束，等你確認", fail: "npm test exit 1（連續兩次）", danger: "git push --force" };

function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

function uuid(r) {
  const hex = () => Math.floor(r() * 16).toString(16);
  const part = (n) => Array.from({ length: n }, hex).join("");
  return `${part(8)}-${part(4)}-4${part(3)}-a${part(3)}-${part(12)}`;
}

// collector 端的狀態在所有連線之間共用，斷線重連後還是同一批 session。
function createWorld(n, { bigtree = false } = {}) {
  const r = rng(20261016);
  const pick = (a) => a[Math.floor(r() * a.length)];
  const now = Date.now();
  const sessions = Array.from({ length: n }, (_, i) => ({
    id: uuid(r),
    host: "local",
    // develop 的在 repo 根，其他各開一個 worktree（左欄「相對 main」逐 worktree 合併，#644）
    cwd: n >= 6 && i % 5 === 4 ? ROOT2 : i % 3 === 0 ? ROOT : `${ROOT}/.claude/wt/t${i}`,
    branch: n >= 6 && i % 5 === 4 ? "main" : i % 3 === 0 ? "develop" : `feature/t${i}`,
    model: i % 4 === 0 ? "opus" : "sonnet",
    firstPrompt: PROMPTS[i % PROMPTS.length],
    lastPrompt: PROMPTS[i % PROMPTS.length],
    startedAt: now - (n - i) * 60_000,
    endedAt: null,
    status: "run",
    statusSince: now,
    doing: null,
    score: 0,
    commits: 0,
    dangers: 0,
    failures: 0,
    agentsLive: 0,
    costUsd: 0,
    thinkingTokens: 0,
    lastPos: null,
  }));
  // 每個 session 的 Event 產生狀態
  const gen = new Map(sessions.map((s) => [s.id, { seq: 0, pos: 0, inflight: [], delayNext: false }]));
  const agents = new Map();
  const attention = new Map();
  for (const s of sessions) {
    agents.set(`${s.id}/main`, { id: `${s.id}/main`, session: s.id, agentId: "main", agentType: null, name: "main", task: null, status: "run", current: null, toolCount: 0, parentToolUseId: null, spawnedAt: s.startedAt, finishedAt: null });
  }
  const kpi = { live: n, attention: 0, sessions: n, agents: 0, edits: 0, writes: 0, commands: 0, failures: 0, dangers: 0, commits: 0, thinkingTokens: 0, costUsd: 0, models: {} };
  let frame = 0;
  let toolSeq = 0;
  let agentSeq = 0;

  // 串流紀錄：glog 是進串流的順序（有 pos 的 Event 與狀態 Event）；cardEv 是卡片的補充 Event
  const glog = [];
  const cardEv = new Map(); // toolUseId → { called, updated, result, extra: [] }
  const staged = new Map(); // toolUseId → tool.called
  const changeLog = []; // file.edit／file.write，snapshot 的 changes 用
  const edited = new Map(); // session → Map(path → "A" | "M")，/api/files 用（不受 60 筆限制）
  const GLOG_MAX = 60_000;

  function ev(s, type, payload, { pos = null, agent = "main" } = {}) {
    const g = gen.get(s.id);
    return { v: 1, session: s.id, seq: ++g.seq, type, agent, line: pos == null ? null : { file: agent === "main" ? "main" : agent, no: pos + 10 }, pos, ts_model: null, ts_exec: type === "tool.called" ? Date.now() : null, ts_seen: Date.now(), host: "local", payload };
  }

  function logStream(e) {
    glog.push(e);
    if (glog.length > GLOG_MAX) glog.splice(0, glog.length - GLOG_MAX);
    if (e.pos != null) {
      const s = sessions.find((x) => x.id === e.session);
      s.lastPos = e.pos;
    }
  }

  function runningAgent(s) {
    const mine = [...agents.values()].filter((a) => a.session === s.id && a.status === "run" && a.agentId !== "bg");
    return pick(mine)?.agentId ?? "main";
  }

  // 一個 session 往前走一步，回傳這一步產生的 Event
  function tick(s, out, rowsOut) {
    const g = gen.get(s.id);
    // 1. 進行中的工具：到時間就 placed／完成
    for (const t of [...g.inflight]) {
      t.placeIn--;
      t.doneIn--;
      if (t.placeIn === 0) {
        const fallback = r() < 0.05;
        const e = ev(s, "tool.placed", { toolUseId: t.id, placement: fallback ? "fallback" : "stream" }, { pos: ++g.pos, agent: t.agent });
        if (fallback) g.delayNext = true;
        staged.delete(t.id);
        out.push(e);
        logStream(e);
      }
      if (t.doneIn <= 0 && t.placeIn <= 0) {
        g.inflight.splice(g.inflight.indexOf(t), 1);
        const ok = t.ok;
        const u = ev(s, "tool.updated", { toolUseId: t.id, status: ok ? "done" : "failed", durationMs: Math.floor(80 + r() * 4000), error: ok ? null : "exit 1", interrupted: false }, { agent: t.agent });
        const res = ev(s, "tool.result", { toolUseId: t.id, ok, exitCode: t.tool === "Bash" ? (ok ? 0 : 1) : null, output: t.out ?? "", truncated: false }, { agent: t.agent });
        const c = cardEv.get(t.id);
        c.updated = u;
        c.result = res;
        out.push(u, res);
        for (const x of t.derived) {
          const d = ev(s, x.type, { toolUseId: t.id, ...x.payload }, { agent: t.agent });
          c.extra.push(d);
          out.push(d);
          if (d.type === "file.edit" || d.type === "file.write") {
            changeLog.push(d);
            if (changeLog.length > 60) changeLog.shift();
            const m = edited.get(s.id) ?? new Map();
            if (m.get(d.payload.path) !== "A") m.set(d.payload.path, d.type === "file.write" && d.payload.isNew ? "A" : "M");
            edited.set(s.id, m);
          }
        }
        const a = agents.get(`${s.id}/${t.agent}`);
        if (a) { a.toolCount++; a.current = null; rowsOut.agents.push({ id: a.id, toolCount: a.toolCount, current: null }); }
      }
    }
    // 2. 新動作
    if (s.status !== "run" || g.inflight.length > 2) return;
    const x = r();
    const agent = runningAgent(s);
    if (x < 0.25) {
      const e = ev(s, "assistant.thinking", { text: pick(THOUGHTS), summarized: r() < 0.2, messageId: `msg_${g.seq}`, model: s.model, ...(g.delayNext ? { delayed: true } : {}) }, { pos: ++g.pos, agent });
      g.delayNext = false;
      out.push(e);
      logStream(e);
      return;
    }
    if (x < 0.35) {
      const e = ev(s, "assistant.text", { text: pick(TEXTS), messageId: `msg_${g.seq}`, model: s.model }, { pos: ++g.pos, agent });
      out.push(e);
      logStream(e);
      return;
    }
    // 工具
    const id = `toolu_${(++toolSeq).toString(36).padStart(8, "0")}`;
    const t = { id, agent, placeIn: 1 + Math.floor(r() * 3), doneIn: 1 + Math.floor(r() * 5), ok: true, derived: [], out: "" };
    const y = r();
    let tool;
    let input;
    let danger = null;
    if (y < 0.25) {
      tool = "Read"; input = { file_path: pick(FILES) };
      t.derived.push({ type: "file.read", payload: { path: input.file_path } });
    } else if (y < 0.33) {
      tool = "Grep"; input = { pattern: "listUsers", path: "src" };
    } else if (y < 0.5) {
      t.ok = r() > 0.08;
      t.out = t.ok ? "" : "String to replace not found in file.";
      tool = "Edit"; input = { file_path: pick(FILES), old_string: "const page = 1;", new_string: "const page = Number(req.query.page ?? 1);" };
      t.derived.push({ type: "file.edit", payload: { path: input.file_path, hunk: { start: 41, oldLines: 3, newLines: 4, lines: [" export async function listUsers(req, res) {", "-  const page = 1;", "+  const page = Number(req.query.page ?? 1);", "+  const size = Math.min(100, Number(req.query.size ?? 20));", "   const rows = await repo.list({ page, size });"] }, hunkSource: "pre" } });
      kpi.edits++;
    } else if (y < 0.56) {
      tool = "Write"; input = { file_path: `src/new/${id.slice(-4)}.ts` };
      t.derived.push({ type: "file.write", payload: { path: input.file_path, isNew: true, content: "export const x = 1;\nexport const y = 2;\n", truncated: false } });
      kpi.writes++;
    } else if (y < 0.62 && [...agents.values()].filter((a) => a.session === s.id && a.status === "run").length < 4) {
      tool = "Task"; input = { description: "查 users 相關的呼叫點", subagent_type: pick(AGENT_TYPES) };
      const agentId = `a${(++agentSeq).toString(16).padStart(16, "0")}`;
      const name = agentSeq % 5 === 0 ? `找出所有呼叫 listUsers 的地方並整理成清單，\n順便看分頁參數 ${agentSeq}` : `查呼叫點 ${agentSeq}`;
      const model = r() < 0.4 ? "haiku" : s.model; // §6 agents.model：subagent 可以用不同模型
      const a = { id: `${s.id}/${agentId}`, session: s.id, agentId, agentType: input.subagent_type, name, model, task: input.description, status: "run", current: null, toolCount: 0, parentToolUseId: id, spawnedAt: Date.now(), finishedAt: null };
      agents.set(a.id, a);
      rowsOut.agents.push(a);
      kpi.agents++;
      const sp = ev(s, "agent.spawned", { agentId, agentType: a.agentType, name, task: a.task, parentToolUseId: id });
      out.push(sp);
      t.extraNow = sp;
      t.doneIn = 30; // subagent 跑一陣子
    } else if (y < 0.66) {
      const d = pick(DANGER);
      tool = "Bash"; input = { command: d.command };
      danger = { rule: d.rule, level: "danger", why: d.why };
      t.ok = r() < 0.5;
      t.out = t.ok ? "" : "error: failed to push some refs";
      kpi.dangers++;
    } else if (y < 0.7) {
      tool = "Bash"; input = { command: 'git commit -m "users 列表加分頁"' };
      const hash = Math.floor(r() * 0xfffffff).toString(16).padStart(7, "0");
      t.out = `[${s.branch} ${hash}] users 列表加分頁\n 2 files changed, 14 insertions(+), 3 deletions(-)`;
      t.derived.push({ type: "git.op", payload: { op: "commit", branch: s.branch, hash, message: "users 列表加分頁", ok: true } });
      kpi.commits++;
    } else if (y < 0.75) {
      tool = "WebFetch"; input = { url: "https://expressjs.com/en/guide/migrating-5.html" };
    } else {
      const b = pick(BASH);
      tool = "Bash"; input = { command: b.command };
      t.ok = b.ok;
      t.out = b.out;
      kpi.commands++;
      if (!b.ok) kpi.failures++;
    }
    t.tool = tool;
    const called = ev(s, "tool.called", { toolUseId: id, tool, input, status: "running", placement: "staged", danger }, { agent });
    cardEv.set(id, { called, updated: null, result: null, extra: t.extraNow ? [t.extraNow] : [] });
    staged.set(id, called);
    g.inflight.push(t);
    out.push(called);
    const a = agents.get(`${s.id}/${agent}`);
    if (a) { a.current = `${tool === "Bash" ? "$ " + input.command : tool.toLowerCase() + " " + (input.file_path ?? input.pattern ?? input.url ?? input.description ?? "")}`.slice(0, 80); rowsOut.agents.push({ id: a.id, current: a.current }); }
  }

  // 有 pos 的 Event 連同它卡片的補充 Event 展開（snapshot 與 more 用）
  function expand(e) {
    if (e.type !== "tool.placed") return [e];
    const c = cardEv.get(e.payload.toolUseId);
    if (!c) return [e];
    return [c.called, e, c.updated, c.result, ...c.extra].filter(Boolean);
  }

  function recount() {
    kpi.live = sessions.filter((s) => s.status === "run").length;
    kpi.attention = attention.size;
  }

  // 解除：危險列留到確認、帶 clearedAt（§6）；其他種類直接刪
  function clearSession(s, rows) {
    for (const [id, a] of attention) {
      if (a.session !== s.id) continue;
      if (a.kind === "danger") {
        if (a.clearedAt == null) { a.clearedAt = Date.now(); rows.attention.push({ id, clearedAt: a.clearedAt }); }
      } else {
        attention.delete(id);
        rows.attention.push({ id, deleted: true });
      }
    }
  }

  function statusStep(s, events, rows) {
    clearSession(s, rows);
    const prev = s.status;
    s.status = pick(STATUSES);
    if (s.status === prev) return;
    s.statusSince = Date.now();
    const changed = { id: s.id, status: s.status, statusSince: s.statusSince };
    if (s.status === "run") {
      s.lastPrompt = pick(PROMPTS);
      changed.lastPrompt = s.lastPrompt;
      const p = ev(s, "prompt.submitted", { text: s.lastPrompt, promptId: `p${frame}` });
      events.push(p);
      logStream(p);
    }
    const se = ev(s, "session.status", { status: s.status, since: s.statusSince, score: 0 });
    events.push(se);
    logStream(se);
    if (ATTENTION_REASON[s.status]) {
      const a = { id: `${s.id}/${s.status}/toolu_${frame}`, session: s.id, kind: s.status, ref: `toolu_${frame}`, reason: ATTENTION_REASON[s.status], raisedAt: Date.now(), clearedAt: null };
      attention.set(a.id, a);
      rows.attention.push(a);
    }
    // §6 sessions.score：佇列裡的種類給基礎分，再加一點動態分（真的算法在 collector，#645）
    s.score = (SCORE_BASE[s.status] ?? 0) + Math.floor(r() * 20);
    changed.score = s.score;
    if (s.status === "danger") { changed.dangers = ++s.dangers; s.dangerUntil = frame + 5; }
    if (s.status === "fail") changed.failures = ++s.failures;
    rows.sessions.push(changed);
  }

  function finishAgents(s, rows, events = []) {
    for (const a of agents.values()) {
      if (a.session !== s.id || a.agentId === "main") continue;
      if (a.agentId === "bg") {
        if (r() < 0.3) { agents.delete(a.id); rows.agents.push({ id: a.id, deleted: true }); }
      } else if (a.status === "run" && r() < 0.08) {
        // 偶爾失敗或被 TaskStop 停掉（#677），agents 列照 collector 的規則：killed 也算 done
        const how = r() < 0.15 ? "failed" : r() < 0.1 ? "killed" : undefined;
        const status = how === "failed" ? "fail" : "done";
        Object.assign(a, { status, current: null, finishedAt: Date.now() });
        rows.agents.push({ id: a.id, status, current: null, finishedAt: a.finishedAt });
        const fe = ev(s, "agent.finished", { agentId: a.agentId, lastMessage: how === "failed" ? "npm test 一直失敗，放棄" : `找到 ${1 + Math.floor(r() * 5)} 個呼叫點，都在 src/api`, transcriptPath: null, ...(how ? { status: how } : {}) }, { agent: a.agentId });
        events.push(fe);
        logStream(fe);
      }
    }
    if (!agents.has(`${s.id}/bg`) && r() < 0.03) {
      const b = { id: `${s.id}/bg`, session: s.id, agentId: "bg", agentType: null, name: "背景", task: null, status: "run", current: null, toolCount: 0, parentToolUseId: null, spawnedAt: Date.now(), finishedAt: null };
      agents.set(b.id, b);
      rows.agents.push(b);
    }
    s.agentsLive = [...agents.values()].filter((a) => a.session === s.id && a.agentId !== "main" && a.agentId !== "bg" && a.status === "run").length;
  }

  // GET /api/files 的 tree：FILES 加一些不會被碰到的檔；bigtree 時超過懶載入門檻
  const tree = [...FILES, "package.json", "tsconfig.json", "src/api/health.ts", "src/repo/index.ts", "docs/guide.md", ".gitignore"];
  if (bigtree) for (let d = 0; d < 80; d++) for (let f = 0; f < 100; f++) tree.push(`packages/p${d}/src/m${f}.ts`);
  const ended = Array.from({ length: 3 }, (_, i) => ({ ...sessions[0], id: uuid(r), cwd: ROOT, branch: "develop", status: "ended", firstPrompt: `（昨天）${PROMPTS[(i + 5) % PROMPTS.length]}`, lastPrompt: null, startedAt: now - (i + 1) * 86_400_000, endedAt: now - (i + 1) * 86_400_000 + 3_600_000, model: i === 1 ? "opus" : "sonnet", commits: i, dangers: 0, doing: null, agentsLive: 0, score: 0 }));
  const err = (status, error) => ({ status, body: { error } });

  // contracts §7（#671）的 /api/files：worktree 的改動從各 session 的 file.edit／write 算（同一個 worktree 合併）
  function files(q) {
    const cwd = q.get("cwd");
    const R = rootOf(cwd);
    if (!R) return err(404, "沒有 session 在這個目錄");
    const mine = sessions.filter((s) => rootOf(s.cwd) === R);
    const base = q.get("base") ?? "main";
    const sources = new Map(); // key → { by, paths: Map }
    const add = (key, by, sess) => {
      const src = sources.get(key) ?? { by, paths: new Map() };
      for (const [p, st] of edited.get(sess.id) ?? []) if (src.paths.get(p) !== "A") src.paths.set(p, st === "A" ? "?" : st);
      sources.set(key, src);
    };
    if (base === "main") for (const s of mine) add(s.cwd, { worktree: s.cwd, branch: s.branch }, s);
    else if (base.startsWith("session:")) {
      const s = sessions.find((x) => x.id === base.slice(8));
      if (!s) return err(404, "沒有這個 session");
      add(s.id, { session: s.id }, s);
      for (const [, src] of sources) for (const [p, st] of src.paths) src.paths.set(p, st === "?" ? "A" : st);
    } else if (/^commit:[0-9a-f]{4,64}$/.test(base)) {
      for (const s of sessions.filter((x) => x.cwd === cwd)) add(cwd, { worktree: cwd, branch: s.branch }, s);
    } else return err(400, "base 只能是 main、commit:<hash>（4–64 位十六進位）或 session:<id>");
    const byPath = new Map();
    for (const src of sources.values()) for (const [p, status] of src.paths) {
      if (!byPath.has(p)) byPath.set(p, []);
      byPath.get(p).push({ ...src.by, status });
    }
    const changes = [...byPath].sort(([a], [b]) => (a < b ? -1 : 1)).map(([path, by]) => ({ path, conflict: by.length > 1 && by.some((b) => b.status !== "?"), by }));
    return { status: 200, body: { root: cwd === R || base === "main" ? R : cwd, base, baseRef: base === "main" ? (R === ROOT2 ? "main" : "develop") : base.startsWith("commit:") ? base.slice(7).padEnd(40, "0") : null, tree: [...(R === ROOT2 ? FILES2 : tree)].sort(), treeTruncated: false, changes, worktrees: [...new Map(mine.map((s) => [s.cwd, { path: s.cwd, branch: s.branch }])).values()] } };
  }

  // 主頁時間軸回放（#729）：簡化版——列（sessions、agents、注意佇列、KPI）用現況，串流與改動只取 at 之前的；
  // frame 是 (at, until] 之間進串流的 Event 依 ts_seen 分組，列不變。只用來在前端試時間軸與播放
  function projectReplay(q, snap) {
    const cwd = (q.get("cwd") ?? "").replace(/\/$/, "");
    const at = Number(q.get("at"));
    if (!cwd) return err(400, "缺少 cwd（專案根目錄）");
    if (!Number.isFinite(at)) return err(400, "at 要是 epoch ms");
    const until = Number(q.get("until") ?? at + 3_600_000);
    const limit = Math.min(10_000, Number(q.get("limit") ?? 2000));
    const under = new Set(sessions.filter((s) => s.cwd === cwd || s.cwd.startsWith(`${cwd}/`)).map((s) => s.id));
    const mine = (x) => under.has(x.session ?? x.id);
    const groups = new Map();
    for (const e of glog) {
      if (!under.has(e.session) || e.ts_seen <= at || e.ts_seen > until) continue;
      if (!groups.has(e.ts_seen)) groups.set(e.ts_seen, []);
      groups.get(e.ts_seen).push(...expand(e));
    }
    const frames = [];
    let placed = 0;
    let next = null;
    for (const [ts, events] of [...groups].sort((a, b) => a[0] - b[0])) {
      if (placed >= limit) { next = frames[frames.length - 1].ts; break; }
      placed += events.length;
      frames.push({ t: "frame", ch: "*", frame: frames.length + 1, ts, events, rows: {} });
    }
    const body = { cwd, at, until, frames, next };
    if (q.get("snapshot") !== "0") {
      body.snapshot = {
        ...snap,
        sessions: snap.sessions.filter(mine),
        agents: snap.agents.filter(mine),
        attention: snap.attention.filter(mine),
        changes: snap.changes.filter((e) => mine(e) && e.ts_seen <= at),
        staged: [],
        stream: snap.stream.filter((e) => mine(e) && e.ts_seen <= at),
      };
    }
    return { status: 200, body };
  }

  // contracts §7 的 /api/graph、/api/branches、/api/commit（#696）：假的歷史，develop 主線＋每個 worktree 一條 feature 分支
  const hx = (n) => (n * 2654435761 >>> 0).toString(16).padStart(8, "0").repeat(5);
  const wts = [...new Map(sessions.filter((s) => rootOf(s.cwd) === ROOT).map((s) => [s.cwd, s.branch])).entries()];
  const history = []; // 新到舊（拓撲順序）
  const tips = new Map(); // branch → tip hash
  {
    let id = 1;
    const mk = (subject, parents, ago) => ({ hash: hx(id++), parents, subject, author: "Claude", ts: now - ago * 60_000, refs: [] });
    const dev = [];
    for (let i = 0; i < 8; i++) dev.push(mk(PROMPTS[i % PROMPTS.length], [], (8 - i) * 90 + 300));
    for (let i = 1; i < dev.length; i++) dev[i].parents = [dev[i - 1].hash];
    const feats = wts.filter(([, b]) => b !== "develop").map(([, b], k) => {
      const from = dev[2 + (k % 5)];
      const n = 1 + (k % 3);
      const cs = [];
      for (let i = 0; i < n; i++) cs.push(mk(`${b}：${PROMPTS[(k + i) % PROMPTS.length]}`, [i ? cs[i - 1].hash : from.hash], (n - i) * 20 + k * 7));
      return { b, cs };
    });
    // 一條已經合回 develop 的分支
    const merged = mk("feature/old：舊功能", [dev[3].hash], 500);
    const merge = mk("Merge branch 'feature/old' into develop", [dev[7].hash, merged.hash], 250);
    tips.set("develop", merge.hash);
    for (const f of feats) tips.set(f.b, f.cs[f.cs.length - 1].hash);
    const all = [merge, ...dev, merged, ...feats.flatMap((f) => f.cs)];
    for (const [b, t] of tips) all.find((c) => c.hash === t).refs.push(b);
    // 拓撲順序：時間新到舊，parent 一定在 child 之後
    const byHash = new Map(all.map((c) => [c.hash, c]));
    const seen = new Set();
    const visit = (c) => { if (seen.has(c.hash)) return; seen.add(c.hash); for (const p of c.parents) visit(byHash.get(p)); history.push(c); };
    for (const c of [...all].sort((x, y) => x.ts - y.ts)) visit(c);
    history.reverse();
  }

  function branches(q) {
    const cwd = q.get("cwd");
    const R = rootOf(cwd);
    if (!R) return err(404, "沒有 session 在這個目錄");
    if (R === ROOT2) {
      const tip = history[0];
      return { status: 200, body: { root: ROOT2, integration: "main", branches: [{ branch: "main", worktree: ROOT2, head: tip.hash, integration: true, ahead: 0, behind: 0, dirty: 0, changed: 0, last: { hash: tip.hash, message: tip.subject, ts: tip.ts }, sessions: sessions.filter((s) => s.cwd === ROOT2 && s.endedAt == null).map((s) => s.id), shared: [] }] } };
    }
    const changes = files(new URLSearchParams({ cwd, base: "main" })).body.changes;
    const rows = wts.map(([wt, branch], i) => {
      const mine = changes.filter((c) => c.by.some((b) => b.worktree === wt));
      const tip = history.find((c) => c.hash === tips.get(branch));
      return {
        branch, worktree: wt, head: tip.hash, integration: branch === "develop",
        ahead: branch === "develop" ? 0 : 1 + (i % 3), behind: branch === "develop" ? 0 : i % 4,
        dirty: mine.filter((c) => c.by.some((b) => b.worktree === wt && b.status === "?")).length || mine.length,
        changed: branch === "develop" ? 0 : mine.length,
        last: { hash: tip.hash, message: tip.subject, ts: tip.ts },
        sessions: sessions.filter((s) => s.cwd === wt && s.endedAt == null).map((s) => s.id),
        shared: mine.filter((c) => c.by.length > 1).map((c) => ({ path: c.path, conflict: c.conflict, with: c.by.filter((b) => b.worktree !== wt).map((b) => b.branch) })),
      };
    });
    return { status: 200, body: { root: ROOT, integration: "develop", branches: rows } };
  }

  const mockDiff = (path, who) => `diff --git a/${path} b/${path}\n--- a/${path}\n+++ b/${path}\n@@ -1,3 +1,3 @@\n context\n-old line\n+${who} 改的\n tail\n`;

  function branchDiff(q) {
    const wt = q.get("worktree");
    const path = q.get("path");
    const s = sessions.find((x) => x.cwd === wt);
    if (!s || !path) return err(404, "不是這個 repo 的 worktree");
    return { status: 200, body: { worktree: wt, branch: s.branch, base: "0".repeat(40), integration: "develop", path, diff: mockDiff(path, s.branch), truncated: false } };
  }

  function commit(q, diff) {
    const path = q.get("path");
    if (q.get("worktree")) {
      const wt = q.get("worktree");
      const s = sessions.find((x) => x.cwd === wt);
      if (!s) return err(404, "不是這個 repo 的 worktree");
      if (diff) return { status: 200, body: { path, diff: mockDiff(path, "未 commit"), truncated: false } };
      const fs = [...new Set(sessions.filter((x) => x.cwd === wt).flatMap((x) => [...(edited.get(x.id) ?? [])]).map(([p, st]) => `${st === "A" ? "?" : st}\t${p}`))];
      return { status: 200, body: { worktree: wt, branch: s.branch, files: fs.map((l) => ({ status: l.split("\t")[0], path: l.split("\t")[1] })) } };
    }
    const c = history.find((x) => x.hash.startsWith(q.get("hash") ?? "-"));
    if (!c) return err(404, "找不到這個 commit");
    if (diff) return { status: 200, body: { path, diff: mockDiff(path, c.hash.slice(0, 7)), truncated: false } };
    const n = 1 + (parseInt(c.hash.slice(0, 2), 16) % 4);
    return { status: 200, body: { hash: c.hash, parents: c.parents, author: c.author, ts: c.ts, subject: c.subject, body: c.parents.length > 1 ? "" : "（mock）說明第二段", files: FILES.slice(0, n).map((p, i) => ({ status: ["M", "A", "M", "D"][i], path: p })) } };
  }

  return {
    // GET /api/*：{ status, body }
    api(url) {
      const u = new URL(url, "http://mock");
      if (u.pathname === "/api/files") return files(u.searchParams);
      if (u.pathname === "/api/projects/replay") return projectReplay(u.searchParams, this.snapshot());
      if (u.pathname === "/api/branches") return branches(u.searchParams);
      if (u.pathname === "/api/branch-diff") return branchDiff(u.searchParams);
      if (u.pathname === "/api/graph") return { status: 200, body: { root: ROOT, integration: "develop", commits: history, truncated: false } };
      if (u.pathname === "/api/commit") return commit(u.searchParams, false);
      if (u.pathname === "/api/commit-diff") return commit(u.searchParams, true);
      if (u.pathname === "/api/sessions") return { status: 200, body: { sessions: [...sessions.map((s) => ({ ...s })), ...ended.map((s) => ({ ...s, model: s.model }))] } };
      return err(404, "not found");
    },
    // session:<id> 頻道（§6）：該 session 的一列、agents、attention、staged、最後 3000 個 pos
    sessionSnapshot(id) {
      const mine = glog.filter((e) => e.session === id);
      let n = 0;
      let from = mine.length;
      while (from > 0 && n < 3000) if (mine[--from].pos != null) n++;
      return {
        t: "snapshot", ch: `session:${id}`, frame,
        sessions: sessions.filter((s) => s.id === id).map((s) => ({ ...s })),
        agents: [...agents.values()].filter((a) => a.session === id).map((a) => ({ ...a })),
        attention: [...attention.values()].filter((a) => a.session === id).map((a) => ({ ...a })),
        staged: [...staged.values()].filter((e) => e.session === id),
        stream: mine.slice(from).flatMap(expand),
      };
    },
    snapshot() {
      // 每個 session 最後 200 個 pos（contracts.md §6），依進串流順序，連同卡片內容
      const counted = new Map();
      const stream = [];
      for (let i = glog.length - 1; i >= 0; i--) {
        const e = glog[i];
        const c = counted.get(e.session) ?? 0;
        if (c >= 200) continue; // 這個 session 已經滿 200 個 pos，更早的（含狀態 Event）都不送
        if (e.pos != null) counted.set(e.session, c + 1);
        stream.push(e);
      }
      stream.reverse();
      return {
        t: "snapshot", ch: "*", frame,
        sessions: sessions.map((s) => ({ ...s })),
        kpi: { ...kpi },
        agents: [...agents.values()].map((a) => ({ ...a })),
        attention: [...attention.values()].map((a) => ({ ...a })),
        changes: [...changeLog],
        staged: [...staged.values()],
        stream: stream.flatMap(expand),
      };
    },
    // 「已確認」：collector 收到 ack 立刻 flush，送 deleted
    ack({ session, kind, ref }) {
      const id = `${session}/${kind}/${ref}`;
      if (!attention.delete(id)) return null;
      recount();
      frame++;
      return { t: "frame", frame, ch: "*", events: [], rows: { attention: [{ id, deleted: true }], kpi: { attention: kpi.attention } } };
    },
    more(session, beforePos, limit) {
      const mine = glog.filter((e) => e.session === session && e.pos != null && e.pos < beforePos);
      return { t: "more", session, events: mine.slice(-limit).flatMap(expand) };
    },
    step({ minEvents = 0 } = {}) {
      frame++;
      const events = [];
      const rows = { sessions: [], agents: [], attention: [] };
      const ticks = minEvents ? 10_000 : 1 + Math.floor(r() * 3);
      // 危險指令跑完就解除（§6：危險列留著、帶 clearedAt，等確認才刪），session 回到執行中
      for (const s of sessions) {
        if (s.status !== "danger" || frame < (s.dangerUntil ?? 0)) continue;
        s.status = "run";
        s.statusSince = Date.now();
        rows.sessions.push({ id: s.id, status: "run", statusSince: s.statusSince });
        clearSession(s, rows);
      }
      for (let k = 0; k < ticks && (!minEvents || events.length < minEvents); k++) {
        const s = pick(sessions);
        if (r() < (minEvents ? 0.02 : 0.08)) statusStep(s, events, rows);
        if (s.status !== "run" && r() < 0.15) { // 停下來的 session 過一陣子繼續跑
          s.status = "run";
          s.statusSince = Date.now();
          s.score = Math.floor(r() * 10);
          rows.sessions.push({ id: s.id, status: "run", statusSince: s.statusSince, score: s.score });
          clearSession(s, rows);
        }
        tick(s, events, rows);
        finishAgents(s, rows, events);
        // §6 doing（#648）：collector 從這一輪的文字／思考／工具動作取一句；mock 偶爾換一句
        if (s.status === "run" && r() < 0.2) {
          const d = pick([...TEXTS, "npm test -- users", "edit src/api/users.ts", s.lastPrompt ?? "", "列表 API 加上分頁，page 從 1 開始、size 最多 100，順便補整合測試與 README 的說明段落"]).slice(0, 80);
          s.doing = d || null;
          rows.sessions.push({ id: s.id, doing: s.doing });
        }
        const cost = r() * 0.004;
        const think = Math.floor(r() * 300);
        s.costUsd = +(s.costUsd + cost).toFixed(4);
        s.thinkingTokens += think;
        kpi.costUsd = +(kpi.costUsd + cost).toFixed(4);
        kpi.thinkingTokens += think;
        // §6 kpi.models：subagent 常用較小的模型，這裡讓一部分花費記在 haiku
        const m = r() < 0.25 ? "haiku" : s.model;
        const km = kpi.models[m] ?? { costUsd: 0, thinkingTokens: 0 };
        kpi.models = { ...kpi.models, [m]: { costUsd: +(km.costUsd + cost).toFixed(4), thinkingTokens: km.thinkingTokens + think } };
        rows.sessions.push({ id: s.id, costUsd: s.costUsd, thinkingTokens: s.thinkingTokens, agentsLive: s.agentsLive, lastPos: s.lastPos });
      }
      recount();
      return { t: "frame", frame, ch: "*", events, rows: { ...rows, kpi: { ...kpi } } };
    },
    // 效能量測：先灌到每個 session 都有至少 perSession 個 pos
    prefill(perSession) {
      let guard = 0;
      while (sessions.some((s) => (gen.get(s.id).pos) < perSession) && guard++ < 200_000) {
        for (const s of sessions) {
          if (s.status !== "run") s.status = "run";
          tick(s, [], { sessions: [], agents: [], attention: [] });
        }
      }
      for (const s of sessions) s.status = "run";
      recount();
    },
  };
}

export function installMock(n, { bench = false, prefill = false, bigtree = false } = {}) {
  const world = createWorld(n, { bigtree });
  if (bench || prefill) world.prefill(220);
  const live = new Set();
  let downUntil = 0;

  class MockSocket {
    constructor() {
      this.readyState = 0;
      this.timer = null;
      this.subs = new Set();
      this.all = false;
      live.add(this);
      setTimeout(() => {
        if (Date.now() < downUntil) return this.#end(); // collector「還沒起來」
        this.readyState = 1;
        this.onopen?.({});
      }, 30);
    }
    send(text) {
      const msg = JSON.parse(text);
      if (msg.op === "hello") this.#emit({ t: "welcome", protocol: 1, instance: "mock", host: "local", version: "0.0.0-mock", remote: false, thinking: "full", scope: "operate" });
      if (msg.op === "subscribe" && msg.channel === "*") {
        this.all = true;
        this.#emit(world.snapshot());
      }
      if (msg.op === "subscribe" && msg.channel.startsWith("session:")) {
        const id = msg.channel.slice(8);
        this.subs.add(id);
        setTimeout(() => this.#emit(world.sessionSnapshot(id)), 40);
      }
      // 監視器視窗只訂 session 頻道：訂了任何頻道就開始推進世界
      if (msg.op === "subscribe" && this.timer == null) {
        this.timer = bench
          ? setInterval(() => this.#frame(world.step({ minEvents: 20 })), 50)
          : setInterval(() => this.#frame(world.step()), 400);
      }
      if (msg.op === "unsubscribe" && msg.channel.startsWith("session:")) this.subs.delete(msg.channel.slice(8));
      if (msg.op === "ack") setTimeout(() => { const f = world.ack(msg); if (f) for (const c of live) c.#emit(f); }, 80);
      if (msg.op === "more") setTimeout(() => this.#emit(world.more(msg.session, msg.beforePos, msg.limit)), 60);
    }
    close() { this.#end(); }
    // `*` 的 frame，再為每個訂閱中的 session 頻道各送一份只含它的
    #frame(f) {
      if (this.all) this.#emit(f);
      for (const id of this.subs) {
        const pick = (list) => (list ?? []).filter((x) => (x.session ?? x.id) === id || String(x.id).startsWith(`${id}/`));
        const events = f.events.filter((e) => e.session === id);
        const rows = { sessions: (f.rows.sessions ?? []).filter((x) => x.id === id), agents: pick(f.rows.agents), attention: pick(f.rows.attention) };
        if (events.length || rows.sessions.length || rows.agents.length || rows.attention.length) this.#emit({ t: "frame", frame: f.frame, ch: `session:${id}`, events, rows });
      }
    }
    #emit(obj) { if (this.readyState === 1) this.onmessage?.({ data: JSON.stringify(obj) }); }
    #end() {
      if (this.readyState === 3) return;
      clearInterval(this.timer);
      this.readyState = 3;
      live.delete(this);
      this.onclose?.({});
    }
  }

  return {
    WebSocket: MockSocket,
    // 取代 window.fetch，只認 /api/*
    async fetch(url) {
      await new Promise((ok) => setTimeout(ok, 50));
      const { status, body } = world.api(url);
      return { ok: status < 400, status, json: async () => JSON.parse(JSON.stringify(body)) };
    },
    // 模擬 collector 重啟：所有連線斷掉，之後 ms 毫秒內連不上
    restart(ms = 3000) {
      downUntil = Date.now() + ms;
      for (const s of [...live]) s.close();
    },
  };
}
